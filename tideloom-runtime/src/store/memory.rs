use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;

use super::InstanceStore;
use super::StoreError;
use crate::Commit;
use crate::Instance;
use crate::InstanceId;
use crate::Lease;
use crate::Status;
use crate::StepResult;
use crate::WaitReason;
use crate::WorkerId;

#[derive(Debug, Default)]
struct State {
    instances: HashMap<InstanceId, Instance>,
    journal: BTreeMap<(InstanceId, TaskKey, u32), StepResult>,
}

/// Instances and journal rows held in memory for the lifetime of the store.
///
/// One mutex makes claims exclusive and commits atomic across the parent,
/// children, cancellations, and journal. Nothing is persisted across restarts.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<State>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl InstanceStore for MemoryStore {
    async fn create(&self, instance: Instance) -> Result<(), StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        if state.instances.contains_key(&instance.id) {
            return Err(StoreError::AlreadyExists(instance.id));
        }
        state.instances.insert(instance.id.clone(), instance);
        Ok(())
    }

    async fn get(&self, id: &InstanceId) -> Result<Instance, StoreError> {
        self.state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(id.clone()))
    }

    async fn claim(
        &self,
        worker: &WorkerId,
        now: Timestamp,
        until: Timestamp,
        limit: usize,
    ) -> Result<Vec<Instance>, StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut ready: Vec<_> = state
            .instances
            .values_mut()
            .filter(|instance| {
                let due = match instance.status {
                    Status::Runnable => instance.wake_at.is_none_or(|at| at <= now),
                    Status::Waiting(WaitReason::Timer) => {
                        instance.wake_at.is_some_and(|at| at <= now)
                    }
                    _ => false,
                };
                due && instance
                    .lease
                    .as_ref()
                    .is_none_or(|lease| lease.until <= now)
            })
            .collect();
        ready.sort_by(|left, right| (left.wake_at, &left.id).cmp(&(right.wake_at, &right.id)));
        let claimed: Vec<_> = ready
            .into_iter()
            .take(limit)
            .map(|instance| {
                instance.status = Status::Running;
                instance.lease = Some(Lease {
                    worker: worker.clone(),
                    until,
                });
                instance.clone()
            })
            .collect();
        Ok(claimed)
    }

    async fn apply(&self, commit: Commit) -> Result<(), StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let instance = state
            .instances
            .get(&commit.instance)
            .ok_or_else(|| StoreError::NotFound(commit.instance.clone()))?;
        if instance.version != commit.expected_version {
            return Err(StoreError::VersionConflict {
                instance: commit.instance,
                expected: commit.expected_version,
                found: instance.version,
            });
        }

        // Validate the entire commit before changing any row.
        let mut spawned = HashSet::new();
        for child in &commit.spawn_children {
            if state.instances.contains_key(&child.id) || !spawned.insert(&child.id) {
                return Err(StoreError::AlreadyExists(child.id.clone()));
            }
        }
        for id in &commit.cancel {
            if !state.instances.contains_key(id) && !spawned.contains(id) {
                return Err(StoreError::NotFound(id.clone()));
            }
        }

        let mut instance = instance.clone();
        instance.status = commit.status;
        instance.continuation = commit.continuation;
        instance.wake_at = commit.wake_at;
        instance.output = commit.output;
        instance.fault = commit.fault;
        instance.version += 1;
        instance.updated_at = commit.at;
        instance.in_flight = commit.in_flight;
        if commit.release_lease {
            instance.lease = None;
        }
        state.instances.insert(instance.id.clone(), instance);
        for child in commit.spawn_children {
            state.instances.insert(
                child.id.clone(),
                Instance {
                    id: child.id,
                    definition: child.definition,
                    parent: Some(child.parent),
                    status: Status::Runnable,
                    continuation: None,
                    version: 0,
                    lease: None,
                    wake_at: Some(commit.at),
                    root_task: child.root_task,
                    in_flight: None,
                    input: child.input,
                    output: None,
                    fault: None,
                    created_at: commit.at,
                    updated_at: commit.at,
                },
            );
        }
        for id in commit.cancel {
            if let Some(instance) = state.instances.get_mut(&id)
                && !instance.status.is_terminal()
            {
                instance.status = Status::Cancelled;
                instance.continuation = None;
                instance.wake_at = None;
                instance.lease = None;
                // Invalidate any commit a worker built before cancellation.
                instance.version += 1;
                instance.updated_at = commit.at;
            }
        }
        for step in commit.step_results {
            state.journal.insert(
                (step.instance.clone(), step.key.clone(), step.attempt),
                step,
            );
        }
        Ok(())
    }

    async fn sweep(&self, before: Timestamp) -> Result<Vec<InstanceId>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut expired: Vec<_> = state
            .instances
            .values()
            .filter(|instance| {
                instance.status == Status::Running
                    && instance
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.until < before)
            })
            .map(|instance| instance.id.clone())
            .collect();
        expired.sort();
        Ok(expired)
    }

    async fn children(&self, parent: &InstanceId) -> Result<Vec<Instance>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut children: Vec<_> = state
            .instances
            .values()
            .filter(|instance| instance.parent.as_ref().is_some_and(|p| &p.id == parent))
            .cloned()
            .collect();
        children.sort_by(|left, right| {
            let order = |instance: &Instance| {
                (
                    instance.parent.as_ref().and_then(|p| p.branch),
                    instance.created_at,
                )
            };
            order(left)
                .cmp(&order(right))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(children)
    }

    async fn steps(&self, instance: &InstanceId) -> Result<Vec<StepResult>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut steps: Vec<_> = state
            .journal
            .range((instance.clone(), TaskKey::new("", Default::default()), 0)..)
            .take_while(|((id, _, _), _)| id == instance)
            .map(|(_, step)| step.clone())
            .collect();
        steps.sort_by(|left, right| {
            (left.started_at, left.finished_at, &left.key, left.attempt).cmp(&(
                right.started_at,
                right.finished_at,
                &right.key,
                right.attempt,
            ))
        });
        Ok(steps)
    }

    async fn latest_step(
        &self,
        instance: &InstanceId,
        key: &TaskKey,
    ) -> Result<Option<StepResult>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut latest = None;
        for (_, step) in state
            .journal
            .range((instance.clone(), key.clone(), 0)..)
            .take_while(|((id, step_key, _), _)| id == instance && step_key == key)
        {
            latest = Some(step.clone());
        }
        Ok(latest)
    }

    async fn wake(&self, ids: &[InstanceId], now: Timestamp) -> Result<usize, StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut flipped = 0;
        for id in ids {
            let Some(instance) = state.instances.get_mut(id) else {
                continue;
            };
            // A row that already has a wake_at was flipped, or was parked on
            // something else. Flipping it again would move the instant and
            // bump the version under a worker that is about to CAS.
            if instance.status != Status::Waiting(WaitReason::Events) || instance.wake_at.is_some()
            {
                continue;
            }
            instance.status = Status::Runnable;
            instance.wake_at = Some(now);
            instance.version += 1;
            instance.updated_at = now;
            flipped += 1;
        }
        Ok(flipped)
    }

    async fn next_due(&self, now: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut earliest = None;
        for instance in state.instances.values() {
            let claimable = matches!(
                instance.status,
                Status::Runnable | Status::Waiting(WaitReason::Timer)
            );
            if !claimable {
                continue;
            }
            let leased = instance
                .lease
                .as_ref()
                .is_some_and(|lease| lease.until > now);
            if leased {
                continue;
            }
            let Some(at) = instance.wake_at else {
                continue;
            };
            if at <= now {
                continue;
            }
            earliest = Some(earliest.map_or(at, |current: Timestamp| current.min(at)));
        }
        Ok(earliest)
    }

    async fn release_leases(&self, worker: &WorkerId, now: Timestamp) -> Result<usize, StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut released = 0;
        for instance in state.instances.values_mut() {
            let Some(lease) = instance.lease.as_mut() else {
                continue;
            };
            if &lease.worker != worker || lease.until <= now {
                continue;
            }
            lease.until = now;
            released += 1;
        }
        Ok(released)
    }
}

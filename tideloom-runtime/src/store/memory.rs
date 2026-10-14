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
use crate::StepOutcome;
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

    /// Move a held lease into the past so [`InstanceStore::sweep`] lists it.
    ///
    /// Tests use this to stand in for a dead worker. The status, continuation,
    /// and journal are left as the crash committed them. A missing instance or
    /// a missing lease is left alone.
    pub fn lapse_lease(&self, id: &InstanceId, until: Timestamp) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(instance) = state.instances.get_mut(id) else {
            return;
        };
        if let Some(lease) = instance.lease.as_mut() {
            lease.until = until;
        }
        if instance.status == Status::Waiting(WaitReason::Activity) {
            instance.status = Status::Running;
        }
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
        Ok(ready
            .into_iter()
            .take(limit)
            .map(|instance| {
                instance.status = Status::Running;
                instance.lease = Some(Lease {
                    worker: worker.clone(),
                    until,
                    open_attempt: None,
                });
                instance.clone()
            })
            .collect())
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
        if commit.release_lease {
            instance.lease = None;
        } else if let Some(lease) = instance.lease.as_mut() {
            lease.open_attempt = open_attempt_after(&commit.step_results, lease.open_attempt);
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
}

/// The attempt a held lease should remember after this commit's journal rows.
fn open_attempt_after(steps: &[StepResult], current: Option<u32>) -> Option<u32> {
    let mut open = current;
    for step in steps {
        match &step.outcome {
            StepOutcome::Pending => open = Some(step.attempt),
            StepOutcome::Output(_) | StepOutcome::Fault(_) | StepOutcome::Unknown
                if open == Some(step.attempt) =>
            {
                open = None;
            }
            StepOutcome::Output(_) | StepOutcome::Fault(_) | StepOutcome::Unknown => {}
        }
    }
    open
}

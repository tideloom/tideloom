//! Claim instances and run each one to its next non-activity stop.
//!
//! A step reads the clock once. It may call the interpreter several times —
//! a tick, then each activity the run reaches — and applies one commit at
//! the stop. An activity also commits around the executor: `Pending` before
//! it runs, then the outcome upserted on the same `(instance, key, attempt)`
//! row. A crash between those two leaves `Pending`. The next claim reads
//! that row only to choose between re-running an idempotent executor and
//! faulting the instance. The continuation, not the journal, says where the
//! run is.

use std::time::Duration;

use serde_json::Value;
use tideloom_core::Block;
use tideloom_core::CallKind;
use tideloom_core::Continuation;
use tideloom_core::Definition;
use tideloom_core::Fault;
use tideloom_core::NodeKind;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::resume_with;
use tideloom_core::start_at_with;
use tideloom_core::start_with;

use super::child::ChildDecision;
use super::join::JoinDecision;
use super::join::decide;
use super::listen;
use super::listen::ListenStep;
use super::translate::decode;
use super::translate::encode;
use super::translate::prepare_child;
use super::translate::translate;
use crate::ActivityError;
use crate::Clock;
use crate::Commit;
use crate::Engine;
use crate::EngineError;
use crate::Hint;
use crate::Instance;
use crate::InstanceId;
use crate::ManualClock;
use crate::Offset;
use crate::Signal;
use crate::Status;
use crate::StepOutcome;
use crate::StepResult;
use crate::StoreError;
use crate::TaskExecution;
use crate::TaskKind;
use crate::WaitReason;

/// How long a claim lasts.
///
/// Long enough that a step finishes under it, short enough that a dead
/// worker is swept. The sweeper returns a lapsed lease to `Runnable`.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(30);

/// How many instances one `run_once` claims. Children of a fork are claimed
/// on the next pass, so one pass does not have to drain the whole store.
const CLAIM_LIMIT: usize = 64;

/// An activity loop that never reaches a pause. The interpreter has its own
/// inline cap; this one bounds executor calls inside a single step.
const MAX_ACTIVITIES: u32 = 64;

/// Detail of the runtime fault written when a non-idempotent activity's
/// outcome is unknown. The milestone fixes this string.
pub const UNKNOWN_DETAIL: &str = "activity outcome unknown";

/// How many instances a pass processed, and how many commits it dropped
/// because another worker won the CAS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepCount {
    /// Instances whose step finished, including ones whose commit conflicted.
    pub processed: usize,
    /// Commits refused because the stored version moved.
    pub conflicts: usize,
}

impl Engine {
    /// Claim what is due and step each instance, then notify for the next due timer.
    ///
    /// Lapsed leases are returned to `Runnable` first, so a dead worker does
    /// not hide an instance from this pass. A signal is a hint: this function
    /// does not read the bus. A lost signal changes when the sweeper or a due
    /// `wake_at` arrives, not whether the instance runs.
    ///
    /// # Errors
    ///
    /// A store or bus failure. A bad continuation or a missing executor
    /// faults that instance and the loop continues with the next one.
    pub async fn run_once(&self, now: Timestamp) -> Result<StepCount, EngineError> {
        self.sweep(now).await?;
        let claimed =
            self.store()
                .claim(
                    self.worker(),
                    now,
                    Timestamp::from_millis(now.millis().saturating_add(
                        u64::try_from(DEFAULT_LEASE.as_millis()).unwrap_or(u64::MAX),
                    )),
                    CLAIM_LIMIT,
                )
                .await?;
        // Parent versions at claim time. A child that finishes later
        // releases against this snapshot, so a sibling that already
        // released loses the CAS instead of releasing again.
        let mut parent_versions = std::collections::HashMap::new();
        for instance in &claimed {
            if let Some(parent) = &instance.parent {
                parent_versions
                    .entry(parent.id.clone())
                    .or_insert(parent_row_version(self, &parent.id).await?);
            }
        }
        let mut count = StepCount::default();
        for instance in &claimed {
            let observed = instance
                .parent
                .as_ref()
                .and_then(|parent| parent_versions.get(&parent.id).copied());
            if self.step_observed(instance.clone(), now, observed).await? {
                count.conflicts += 1;
            }
            count.processed += 1;
        }
        // A timer that became due during this pass is claimable on the next
        // one. The store, not a signal, is what `claim` reads.
        self.note_spawned(&claimed).await?;
        Ok(count)
    }

    /// Step until nothing is claimable, advancing the clock to the next `wake_at`.
    ///
    /// A future `wait` or retry backoff stops a pass that only claims what is
    /// due. This helper moves `clock` to the earliest such `wake_at` and
    /// continues, so a test drives a workflow to a terminal status without
    /// sleeping. It stops when a pass claims nothing and no instance has a
    /// future `wake_at`. A join does not need the clock: finishing a child
    /// releases the parent back to `Runnable` inside the same drive.
    ///
    /// # Errors
    ///
    /// The same as [`Engine::run_once`].
    pub async fn drive_until_idle(&self, clock: &ManualClock) -> Result<usize, EngineError> {
        let mut stepped = 0;
        loop {
            let count = self.run_once(clock.now()).await?;
            stepped += count.processed;
            if count.processed > 0 {
                continue;
            }
            let Some(next) = next_wake_at(self, clock.now()).await? else {
                return Ok(stepped);
            };
            // Advancing to a due instant and claiming nothing means the row
            // is not actually claimable (a live lease, a non-timer wait).
            // Stop instead of spinning on the same `wake_at`.
            if next <= clock.now() {
                return Ok(stepped);
            }
            clock.set(next);
        }
    }

    /// Run one claimed instance until its `Pending` journal row is committed.
    ///
    /// The executor has not run. The lease stays held, with `open_attempt`
    /// set, so a later [`Engine::step`] of the same instance finishes the
    /// attempt instead of treating it as a crash. Tests use this to stand in
    /// for a worker that died between the two commits: lapse the lease, then
    /// `run_once`.
    ///
    /// # Errors
    ///
    /// The same as [`Engine::step`].
    pub async fn step_until_pending(
        &self,
        instance: Instance,
        now: Timestamp,
    ) -> Result<bool, EngineError> {
        if instance.status == Status::Cancelled || instance.status.is_terminal() {
            return Ok(false);
        }
        let current = self.store().get(&instance.id).await?;
        if current.status == Status::Cancelled || current.status.is_terminal() {
            return Ok(false);
        }
        let definition = self.definitions().get(&current.definition).await?;
        let options = WalkOptions::new().with_now(now);
        let outcome = match &current.continuation {
            None => self.begin(&current, &definition, options)?,
            Some(bytes) => {
                let continuation = decode(&current.id, bytes)?;
                resume_with(&definition, continuation, Resume::Tick, options)
            }
        };
        let Outcome::Blocked {
            block,
            continuation,
        } = outcome
        else {
            return Ok(false);
        };
        if block.pause() != Pause::Activity {
            return Ok(false);
        }
        let journalled = self
            .run_journalled(
                &current,
                &definition,
                &block,
                &continuation,
                current.version,
                now,
                true,
            )
            .await?;
        let Some(commit) = journalled.done else {
            return Ok(false);
        };
        self.apply_step(commit).await
    }

    /// Run one claimed instance to its next stop and commit that.
    ///
    /// Returns whether the commit lost a version race. A cancelled instance
    /// is dropped without calling the interpreter: cancellation already
    /// cleared its continuation, and resuming it would undo the cancel.
    /// The store does not hand a `Cancelled` instance to `claim`, so this
    /// guard is for a cancel that landed between claim and step.
    ///
    /// # Errors
    ///
    /// A store or bus failure. Interpreter and executor failures are written
    /// as a fault commit instead of returned.
    pub async fn step(&self, instance: Instance, now: Timestamp) -> Result<bool, EngineError> {
        self.step_observed(instance, now, None).await
    }

    async fn step_observed(
        &self,
        instance: Instance,
        now: Timestamp,
        parent_version: Option<u64>,
    ) -> Result<bool, EngineError> {
        self.remember(instance.id.clone());
        if instance.status == Status::Cancelled || instance.status.is_terminal() {
            return Ok(false);
        }
        // Re-read: a sibling's join may have cancelled this instance after
        // `claim` copied it. The copy still says `Running`.
        let current = self.store().get(&instance.id).await?;
        if current.status == Status::Cancelled || current.status.is_terminal() {
            return Ok(false);
        }
        let definition = self.definitions().get(&current.definition).await?;
        let result = self.interpret(&current, &definition, now).await;
        let (commit, acked) = match result {
            Ok(commit) => commit,
            Err(error) => (fault_commit(&current, now, &error)?, Vec::new()),
        };
        let terminal = commit.status.is_terminal();
        let parent = current.parent.clone();
        let commit = if terminal {
            self.cancel_losing_siblings(&current, commit).await?
        } else {
            commit
        };
        let conflict = self.apply_step(commit).await?;
        if !conflict {
            // Children created in this commit are not in the claimed set.
            // Remember them before the next pass, including a child whose
            // parent completed in the same step (`await: false`).
            for child in self.store().children(&current.id).await? {
                self.remember(child.id);
            }
        }
        if !conflict && !acked.is_empty() {
            // After the commit, never before. A conflict above leaves the
            // inbox intact so the next claim releases the same events.
            listen::ack_after_commit(self.events_in().as_ref(), &current.id, &acked)
                .await
                .map_err(|error| EngineError::Events(error.to_string()))?;
        }
        if !conflict {
            self.remember(current.id.clone());
        }
        if !conflict
            && terminal
            && let Some(parent) = parent
        {
            // The signal is a hint. `release_parent` is what makes the parent
            // claimable again; losing the signal does not lose the release.
            self.wakeup()
                .notify(parent.id.clone(), Hint::ChildDone(current.id.clone()))
                .await?;
            let release_conflict = self.release_parent(&parent.id, parent_version, now).await?;
            if release_conflict {
                return Ok(true);
            }
        }
        Ok(conflict)
    }

    async fn interpret(
        &self,
        instance: &Instance,
        definition: &Definition,
        now: Timestamp,
    ) -> Result<(Commit, Vec<String>), EngineError> {
        let options = WalkOptions::new().with_now(now);
        let mut outcome = match &instance.continuation {
            None => self.begin(instance, definition, options)?,
            Some(bytes) => {
                let continuation = decode(&instance.id, bytes)?;
                // A released join is `Runnable` with the join continuation
                // still stored. Re-evaluate it. The first time through has
                // no continuation at all, so `begin` walks to the join and
                // `translate` spawns the children.
                if matches!(continuation.pause(), Pause::Join { .. }) {
                    return Ok((
                        self.rejoin(instance, definition, continuation, now).await?,
                        Vec::new(),
                    ));
                }
                if matches!(continuation.pause(), Pause::Child) {
                    return Ok((
                        self.rechild(instance, definition, continuation, now)
                            .await?,
                        Vec::new(),
                    ));
                }
                if matches!(continuation.pause(), Pause::Events) {
                    return self.relisten(instance, definition, continuation, now).await;
                }
                // A due timer or retry is why this instance was claimed.
                // `Tick` only re-checks deadlines and returns the same block,
                // so the step releases it here. Releasing before the deadline
                // would skip the wait; claim does not hand those out.
                if matches!(continuation.pause(), Pause::Timer | Pause::Retry { .. }) {
                    return self
                        .release_timer(instance, definition, continuation, now)
                        .await;
                }
                resume_with(definition, continuation, Resume::Tick, options)
            }
        };
        let mut activities = 0;
        let mut version = instance.version;
        while let Outcome::Blocked {
            block,
            continuation,
        } = &outcome
        {
            if block.pause() != Pause::Activity {
                break;
            }
            // A fresh fork branch stops on its first activity without running
            // the executor. A compete winner claimed in the same pass cancels
            // it in its own commit, and the cancelled branch is never claimed
            // again. A branch that survives runs the executor on the next
            // claim, when its continuation is already stored. A root instance
            // is not a branch, so its activity runs in this step.
            if instance.continuation.is_none() && instance.parent.is_some() {
                break;
            }
            if activities >= MAX_ACTIVITIES {
                return Ok((
                    faulted(
                        instance,
                        now,
                        Fault::runtime(
                            block.position(),
                            "activity loop exceeded 64 executor calls",
                        ),
                    ),
                    Vec::new(),
                ));
            }
            activities += 1;
            let journalled = self
                .run_journalled(
                    instance,
                    definition,
                    block,
                    continuation,
                    version,
                    now,
                    false,
                )
                .await?;
            if let Some(commit) = journalled.done {
                return Ok((commit, Vec::new()));
            }
            version = journalled.version;
            let event = journalled.event.unwrap_or(Resume::Tick);
            outcome = resume_with(definition, continuation.clone(), event, options);
        }
        let events = match &outcome {
            Outcome::Blocked { block, .. } if matches!(block.pause(), Pause::Events) => {
                Some(block.detail().clone())
            }
            _ => None,
        };
        let prepared = prepare_child(
            self.definitions().as_ref(),
            instance,
            definition,
            outcome,
            now,
        )
        .await?;
        let commit = translate(
            &instance.id,
            version,
            &instance.definition,
            prepared.outcome,
            prepared.spawn,
            now,
        )?;
        if let Some(tideloom_core::BlockDetail::Listen(spec)) = events {
            listen::subscribe(self.events_in().as_ref(), &instance.id, spec.clone())
                .await
                .map_err(|error| EngineError::Events(error.to_string()))?;
            // Subscribe happens before the commit, so an emit in this same
            // step is already in the inbox. Release now instead of parking
            // on `Waiting(Events)`, which `claim` would never return.
            let inbox = self
                .events_in()
                .inbox(&instance.id)
                .await
                .map_err(|error| EngineError::Events(error.to_string()))?;
            if crate::LocalEventBus::satisfied(&spec, &inbox) {
                // Due now, so the next `run_once` claims it and releases.
                // An unsatisfied listen stays `Waiting(Events)`, which
                // `claim` does not return, so `drive_until_idle` can stop.
                let mut due = commit;
                due.status = Status::Waiting(WaitReason::Timer);
                due.wake_at = Some(now);
                return Ok((due, Vec::new()));
            }
        }
        Ok((commit, Vec::new()))
    }

    /// A claimed timer or retry has reached `wake_at`. Release it.
    ///
    /// `Tick` runs first, so a workflow or task deadline that has also been
    /// reached faults instead of releasing. The release is what the
    /// interpreter accepts for these pauses; `Output` would be a contract
    /// violation.
    async fn release_timer(
        &self,
        instance: &Instance,
        definition: &Definition,
        continuation: Continuation,
        now: Timestamp,
    ) -> Result<(Commit, Vec<String>), EngineError> {
        let options = WalkOptions::new().with_now(now);
        let ticked = resume_with(definition, continuation.clone(), Resume::Tick, options);
        if let Outcome::Faulted { .. } = &ticked {
            return Ok((
                translate(
                    &instance.id,
                    instance.version,
                    &instance.definition,
                    ticked,
                    Vec::new(),
                    now,
                )?,
                Vec::new(),
            ));
        }
        let Outcome::Blocked {
            continuation: parked,
            ..
        } = ticked
        else {
            return Ok((
                translate(
                    &instance.id,
                    instance.version,
                    &instance.definition,
                    ticked,
                    Vec::new(),
                    now,
                )?,
                Vec::new(),
            ));
        };
        let outcome = resume_with(definition, parked, Resume::Release(Value::Null), options);
        let commit = self
            .finish_inline(instance, definition, outcome, now)
            .await?;
        Ok((commit, Vec::new()))
    }

    /// Resume through any activities the release reached, then translate the stop.
    async fn finish_inline(
        &self,
        instance: &Instance,
        definition: &Definition,
        mut outcome: Outcome,
        now: Timestamp,
    ) -> Result<Commit, EngineError> {
        let options = WalkOptions::new().with_now(now);
        let mut activities = 0;
        let mut version = instance.version;
        while let Outcome::Blocked {
            block,
            continuation,
        } = &outcome
        {
            if block.pause() != Pause::Activity {
                break;
            }
            if activities >= MAX_ACTIVITIES {
                return Ok(faulted(
                    instance,
                    now,
                    Fault::runtime(block.position(), "activity loop exceeded 64 executor calls"),
                ));
            }
            activities += 1;
            let journalled = self
                .run_journalled(
                    instance,
                    definition,
                    block,
                    continuation,
                    version,
                    now,
                    false,
                )
                .await?;
            if let Some(commit) = journalled.done {
                return Ok(commit);
            }
            version = journalled.version;
            let event = journalled.event.unwrap_or(Resume::Tick);
            outcome = resume_with(definition, continuation.clone(), event, options);
        }
        let prepared = prepare_child(
            self.definitions().as_ref(),
            instance,
            definition,
            outcome,
            now,
        )
        .await?;
        translate(
            &instance.id,
            version,
            &instance.definition,
            prepared.outcome,
            prepared.spawn,
            now,
        )
    }

    /// A claimed listen either keeps waiting or resumes with the inbox.
    ///
    /// The inbox is the source of truth. A missing signal does not lose the
    /// event: the next claim reads the same inbox.
    async fn relisten(
        &self,
        instance: &Instance,
        definition: &Definition,
        continuation: Continuation,
        now: Timestamp,
    ) -> Result<(Commit, Vec<String>), EngineError> {
        // Tick rebuilds the listen block, including its spec. The stored
        // continuation does not carry `BlockDetail`.
        let ticked = resume_with(
            definition,
            continuation.clone(),
            Resume::Tick,
            WalkOptions::new().with_now(now),
        );
        let Outcome::Blocked {
            block,
            continuation,
        } = ticked
        else {
            let prepared = prepare_child(
                self.definitions().as_ref(),
                instance,
                definition,
                ticked,
                now,
            )
            .await?;
            let commit = translate(
                &instance.id,
                instance.version,
                &instance.definition,
                prepared.outcome,
                prepared.spawn,
                now,
            )?;
            return Ok((commit, Vec::new()));
        };
        let decision = listen::after_claim(self.events_in().as_ref(), &instance.id, block.detail())
            .await
            .map_err(|error| EngineError::Events(error.to_string()))?;
        let ListenStep::Release(release) = decision else {
            return Ok((still_events(instance, &continuation, now)?, Vec::new()));
        };
        let outcome = resume_with(
            definition,
            continuation,
            Resume::Release(serde_json::Value::Array(release.events)),
            WalkOptions::new().with_now(now),
        );
        let prepared = prepare_child(
            self.definitions().as_ref(),
            instance,
            definition,
            outcome,
            now,
        )
        .await?;
        let commit = translate(
            &instance.id,
            instance.version,
            &instance.definition,
            prepared.outcome,
            prepared.spawn,
            now,
        )?;
        // The caller commits, then acks these ids. Acking here would drop
        // the events if that commit lost the CAS.
        Ok((commit, release.ids))
    }

    /// Journal one activity, then run it unless `cut` stops after `Pending`.
    ///
    /// The pending commit keeps the lease. The outcome upserts the same
    /// `(instance, key, attempt)` row. A `Pending` row whose attempt is not on
    /// this lease is a crash: an idempotent executor runs again at
    /// `attempt + 1`, and any other executor faults the instance.
    #[allow(clippy::too_many_arguments)]
    async fn run_journalled(
        &self,
        instance: &Instance,
        definition: &Definition,
        block: &Block,
        continuation: &Continuation,
        version: u64,
        now: Timestamp,
        after_pending: bool,
    ) -> Result<Journalled, EngineError> {
        let Some(kind) = TaskKind::of(block.kind()) else {
            return Ok(Journalled::resume(Resume::Fault(Fault::runtime(
                block.position(),
                "effectful task has no executor kind",
            ))));
        };
        let held = instance
            .lease
            .as_ref()
            .filter(|lease| lease.worker == *self.worker())
            .and_then(|lease| lease.open_attempt);
        match self
            .open_attempt(&instance.id, block.key(), kind, held)
            .await?
        {
            OpenAttempt::Unknown { pending } => {
                let fault = Fault::runtime(block.position(), UNKNOWN_DETAIL);
                let mut unknown = pending;
                unknown.attempt = unknown.attempt.saturating_add(1);
                unknown.outcome = StepOutcome::Unknown;
                unknown.finished_at = now;
                let mut commit = faulted_at(&instance.id, version, now, fault);
                commit.step_results.push(unknown);
                Ok(Journalled::finished(commit))
            }
            OpenAttempt::Run { attempt } => {
                let mut version = version;
                if held != Some(attempt) {
                    let pending = activity_row(
                        instance,
                        block,
                        kind,
                        attempt,
                        StepOutcome::Pending,
                        now,
                        now,
                    );
                    if after_pending {
                        // This cut is the pending commit. A follow-up commit
                        // would replace Waiting(Activity) and clear the open
                        // attempt the next step of this hold continues.
                        let mut commit = Commit::new(
                            instance.id.clone(),
                            version,
                            Status::Waiting(WaitReason::Activity),
                            now,
                        );
                        commit.continuation = Some(encode(&instance.id, continuation)?);
                        commit.release_lease = false;
                        commit.step_results.push(pending);
                        return Ok(Journalled::finished(commit));
                    }
                    self.commit_row(
                        &instance.id,
                        version,
                        continuation,
                        pending,
                        Status::Waiting(WaitReason::Activity),
                    )
                    .await?;
                    version += 1;
                } else if after_pending {
                    return Ok(Journalled::finished(still_running(
                        instance,
                        version,
                        continuation,
                        now,
                    )?));
                }
                let event = match self
                    .execute(instance, definition, block, continuation, kind, attempt)
                    .await?
                {
                    Ok(output) => Resume::Output(output),
                    Err(fault) => Resume::Fault(fault),
                };
                let finished = match &event {
                    Resume::Output(output) => StepOutcome::Output(output.clone()),
                    Resume::Fault(fault) => StepOutcome::Fault(fault.clone()),
                    Resume::Release(_) | Resume::Tick => StepOutcome::Fault(Fault::runtime(
                        block.position(),
                        "activity resume was neither Output nor Fault",
                    )),
                };
                let started_at = self
                    .started_at_of(&instance.id, block.key(), attempt, now)
                    .await?;
                self.commit_row(
                    &instance.id,
                    version,
                    continuation,
                    activity_row(instance, block, kind, attempt, finished, started_at, now),
                    Status::Running,
                )
                .await?;
                Ok(Journalled {
                    version: version + 1,
                    event: Some(event),
                    done: None,
                })
            }
        }
    }

    /// Start a run that has no continuation yet.
    ///
    /// A `fork` branch carries its task position in the instance input under
    /// [`BRANCH_POSITION`], written when the parent spawned it. The visible
    /// workflow input is everything else. `start_at` makes that task the
    /// root, so a fault inside the branch stops at the branch and does not
    /// walk the parent's `try`.
    fn begin(
        &self,
        instance: &Instance,
        definition: &Definition,
        options: WalkOptions,
    ) -> Result<Outcome, EngineError> {
        match branch_start(&instance.input) {
            Some(position) => Ok(start_at_with(
                definition,
                &position,
                &visible_input(&instance.input),
                options,
            )),
            None => Ok(start_with(definition, &instance.input, options)),
        }
    }

    async fn rejoin(
        &self,
        instance: &Instance,
        definition: &Definition,
        continuation: Continuation,
        now: Timestamp,
    ) -> Result<Commit, EngineError> {
        let compete = matches!(continuation.pause(), Pause::Join { compete: true });
        let children = self.store().children(&instance.id).await?;
        match decide(&children, compete) {
            JoinDecision::Pending => Ok(still_waiting(instance, &continuation, now)?),
            JoinDecision::Release { output, cancel } => {
                let outcome = resume_with(
                    definition,
                    continuation,
                    Resume::Release(output),
                    WalkOptions::new().with_now(now),
                );
                let prepared = prepare_child(
                    self.definitions().as_ref(),
                    instance,
                    definition,
                    outcome,
                    now,
                )
                .await?;
                let mut commit = translate(
                    &instance.id,
                    instance.version,
                    &instance.definition,
                    prepared.outcome,
                    prepared.spawn,
                    now,
                )?;
                commit.cancel = cancel;
                Ok(commit)
            }
            JoinDecision::Fault { fault, cancel } => {
                // The branch fault becomes the parent's status directly.
                // Resuming the join with `Fault` would be a contract
                // violation (a blocking pause resumes with `Release`), and
                // handing the fault to the interpreter would let an outer
                // `try` catch it. A fork is the boundary, so the parent
                // stops here.
                let mut commit = faulted(instance, now, fault);
                commit.cancel = cancel;
                Ok(commit)
            }
        }
    }

    async fn execute(
        &self,
        instance: &Instance,
        definition: &Definition,
        block: &Block,
        continuation: &Continuation,
        kind: TaskKind,
        attempt: u32,
    ) -> Result<Result<Value, Fault>, EngineError> {
        // The core client needs the interpreter block, which an executor trait
        // method does not receive. It runs when no HTTP executor is registered.
        // A registered one — a test double, or a later transport — receives
        // the `TaskExecution` and replaces that client.
        if kind == TaskKind::Http
            && self.executor(kind).is_none()
            && matches!(block.kind(), NodeKind::Call(CallKind::Http))
        {
            return Ok(
                crate::HttpExecutor::call(definition, continuation.workflow_input(), block)
                    .map_err(|error| match error {
                        crate::ActivityError::Fault(fault) => fault,
                        crate::ActivityError::Executor(detail) => {
                            Fault::runtime(block.position(), detail)
                        }
                    }),
            );
        }
        let Some(executor) = self.executor(kind) else {
            return Ok(Err(Fault::runtime(
                block.position(),
                format!("no executor for {kind:?}"),
            )));
        };
        let task = TaskExecution {
            instance: instance.id.clone(),
            definition: instance.definition.clone(),
            key: block.key().clone(),
            kind,
            attempt,
            input: block.input().clone(),
            context: continuation.context(),
            workflow_input: continuation.workflow_input().clone(),
        };
        match executor.execute(task).await {
            Ok(output) => Ok(Ok(output)),
            Err(ActivityError::Fault(fault)) => Ok(Err(fault)),
            Err(ActivityError::Executor(detail)) => {
                Ok(Err(Fault::runtime(block.position(), detail)))
            }
        }
    }

    async fn open_attempt(
        &self,
        instance: &InstanceId,
        key: &TaskKey,
        kind: TaskKind,
        held_attempt: Option<u32>,
    ) -> Result<OpenAttempt, EngineError> {
        let steps = self.store().steps(instance).await?;
        let latest = steps.iter().rev().find(|step| &step.key == key);
        let Some(latest) = latest else {
            return Ok(OpenAttempt::Run { attempt: 0 });
        };
        if !matches!(latest.outcome, StepOutcome::Pending) {
            return Ok(OpenAttempt::Run {
                attempt: latest.attempt.saturating_add(1),
            });
        }
        // This hold already wrote Pending and has not finished. Continue it.
        // A new claim does not carry `open_attempt`, so a Pending row then is
        // a crash, not the second half of this call.
        if held_attempt == Some(latest.attempt) {
            return Ok(OpenAttempt::Run {
                attempt: latest.attempt,
            });
        }
        if activity_idempotent(self, kind) {
            return Ok(OpenAttempt::Run {
                attempt: latest.attempt.saturating_add(1),
            });
        }
        Ok(OpenAttempt::Unknown {
            pending: latest.clone(),
        })
    }

    async fn started_at_of(
        &self,
        instance: &InstanceId,
        key: &TaskKey,
        attempt: u32,
        now: Timestamp,
    ) -> Result<Timestamp, EngineError> {
        let steps = self.store().steps(instance).await?;
        Ok(steps
            .iter()
            .rev()
            .find(|step| &step.key == key && step.attempt == attempt)
            .map(|step| step.started_at)
            .unwrap_or(now))
    }

    async fn commit_row(
        &self,
        instance: &InstanceId,
        version: u64,
        continuation: &Continuation,
        step: StepResult,
        status: Status,
    ) -> Result<(), EngineError> {
        let mut commit = Commit::new(instance.clone(), version, status, step.finished_at);
        commit.continuation = Some(encode(instance, continuation)?);
        commit.release_lease = false;
        commit.step_results.push(step);
        self.apply_step(commit).await?;
        Ok(())
    }

    async fn sweep(&self, now: Timestamp) -> Result<(), EngineError> {
        let expired = self.store().sweep(now).await?;
        for id in expired {
            let instance = self.store().get(&id).await?;
            if instance.status != Status::Running {
                continue;
            }
            let mut commit = Commit::new(id, instance.version, Status::Runnable, now);
            commit.continuation = instance.continuation;
            commit.wake_at = Some(now);
            commit.output = instance.output;
            commit.fault = instance.fault;
            self.apply_step(commit).await?;
        }
        Ok(())
    }

    /// Cancel siblings that a join has already decided against, in this
    /// commit rather than the parent's.
    ///
    /// Waiting for the parent to re-evaluate is too late: the siblings are
    /// claimed in the same pass and finish before the parent runs again.
    /// Compete cancels on the first completion. Cooperative cancels on the
    /// first fault. A completed cooperative branch cancels nothing.
    async fn cancel_losing_siblings(
        &self,
        instance: &Instance,
        mut commit: Commit,
    ) -> Result<Commit, EngineError> {
        // Compete cancels on the first completion. Cooperative cancels on
        // the first fault. Either way the sibling is still `Running` only
        // if this commit is the one that lands first.
        if !matches!(commit.status, Status::Completed | Status::Faulted) {
            return Ok(commit);
        }
        let Some(parent) = &instance.parent else {
            return Ok(commit);
        };
        let Some(branch) = parent.branch else {
            return Ok(commit);
        };
        let parent_row = match self.store().get(&parent.id).await {
            Ok(parent) => parent,
            Err(StoreError::NotFound(_)) => return Ok(commit),
            Err(error) => return Err(error.into()),
        };
        let Some(bytes) = &parent_row.continuation else {
            return Ok(commit);
        };
        let continuation = decode(&parent_row.id, bytes)?;
        let compete = matches!(continuation.pause(), Pause::Join { compete: true });
        let cooperative_fault = matches!(continuation.pause(), Pause::Join { compete: false })
            && commit.status == Status::Faulted;
        if !compete && !cooperative_fault {
            return Ok(commit);
        }
        if compete && commit.status != Status::Completed {
            return Ok(commit);
        }
        let siblings = self.store().children(&parent.id).await?;
        commit.cancel = siblings
            .into_iter()
            .filter(|sibling| {
                sibling
                    .parent
                    .as_ref()
                    .is_some_and(|parent| parent.branch.is_some_and(|index| index != branch))
                    && !sibling.status.is_terminal()
                    && sibling.id != instance.id
            })
            .map(|sibling| sibling.id)
            .collect();
        Ok(commit)
    }

    /// Re-evaluate a `Waiting(Child)` parent from its child workflow.
    ///
    /// The same path a join uses: read `children`, decide, resume. A child
    /// workflow is effectful, so the resume is `Output` or `Fault`, and a
    /// surrounding `try` can catch the fault. A fork join faults the parent
    /// status instead, because a fork is an error boundary.
    async fn rechild(
        &self,
        instance: &Instance,
        definition: &Definition,
        continuation: Continuation,
        now: Timestamp,
    ) -> Result<Commit, EngineError> {
        let children = self.store().children(&instance.id).await?;
        match super::child::decide(&children) {
            ChildDecision::Pending => Ok(still_child(instance, &continuation, now)?),
            ChildDecision::Output { output } => {
                let outcome = resume_with(
                    definition,
                    continuation,
                    Resume::Output(output),
                    WalkOptions::new().with_now(now),
                );
                self.finish_inline(instance, definition, outcome, now).await
            }
            ChildDecision::Fault { fault } => {
                let outcome = resume_with(
                    definition,
                    continuation,
                    Resume::Fault(fault),
                    WalkOptions::new().with_now(now),
                );
                self.finish_inline(instance, definition, outcome, now).await
            }
        }
    }

    /// Flip a `Waiting(Join)` or `Waiting(Child)` parent back to `Runnable`
    /// so the next claim re-evaluates it.
    ///
    /// The child's terminal commit has already landed. A version conflict
    /// means another child released the parent first; the next claim
    /// re-evaluates from the children either way, so the conflict is dropped.
    async fn release_parent(
        &self,
        parent: &InstanceId,
        observed_version: Option<u64>,
        now: Timestamp,
    ) -> Result<bool, EngineError> {
        let parent = match self.store().get(parent).await {
            Ok(parent) => parent,
            Err(StoreError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        // A parent that is no longer waiting was released by a sibling that
        // committed first. The second finisher still attempts the release
        // against the version it observed before either commit, so `apply`
        // rejects it. That conflict is the record that two children finished
        // together and only one release landed. A `run: workflow` parent has
        // one child, so the second release does not arise, but the same CAS
        // is what makes a repeated `ChildDone` a no-op.
        let waiting = matches!(
            parent.status,
            Status::Waiting(WaitReason::Join) | Status::Waiting(WaitReason::Child)
        );
        if !waiting && parent.status != Status::Runnable {
            return Ok(false);
        }
        let Some(bytes) = parent.continuation.clone() else {
            return Ok(false);
        };
        let continuation = decode(&parent.id, &bytes)?;
        if !matches!(continuation.pause(), Pause::Join { .. } | Pause::Child) {
            return Ok(false);
        }
        let version = observed_version.unwrap_or(parent.version);
        let mut commit = Commit::new(parent.id, version, Status::Runnable, now);
        commit.continuation = Some(bytes);
        commit.wake_at = Some(now);
        self.apply_step(commit).await
    }

    /// `apply` with no bus offset: a step is not triggered by a signal.
    /// A version conflict is the other worker winning; drop the work.
    async fn apply_step(&self, commit: Commit) -> Result<bool, EngineError> {
        match self.commit(commit, None).await {
            Ok(()) => Ok(false),
            Err(EngineError::Store(StoreError::VersionConflict { .. })) => Ok(true),
            Err(error) => Err(error),
        }
    }
}

/// Hint that a known instance whose `wake_at` is due, or whose lease has
/// lapsed, may be claimed.
///
/// The store is the source of truth. Dropping every signal this sends still
/// leaves the instance claimable: the next `run_once` sweeps a lapsed lease
/// and claims a due timer without reading the bus. `run_once` does not call
/// this. A worker that watches the bus does, and a test that drops the bus
/// still finishes through the sweeper.
pub async fn notify_due(engine: &Engine, now: Timestamp) -> Result<(), EngineError> {
    for signal in due_signals(engine, now).await? {
        engine.wakeup().notify(signal.instance, signal.hint).await?;
    }
    Ok(())
}

async fn due_signals(engine: &Engine, now: Timestamp) -> Result<Vec<Signal>, EngineError> {
    let mut signals = Vec::new();
    for id in engine.store().sweep(now).await? {
        signals.push(Signal {
            instance: id,
            hint: Hint::Other,
            offset: Offset::default(),
        });
    }
    for instance in known_instances(engine).await? {
        let due = match instance.status {
            Status::Runnable => instance.wake_at.is_none_or(|at| at <= now),
            Status::Waiting(WaitReason::Timer) => instance.wake_at.is_some_and(|at| at <= now),
            _ => false,
        };
        let leased = instance
            .lease
            .as_ref()
            .is_some_and(|lease| lease.until > now);
        if due && !leased && !instance.status.is_terminal() {
            signals.push(Signal {
                instance: instance.id,
                hint: Hint::Timer,
                offset: Offset::default(),
            });
        }
    }
    Ok(signals)
}

async fn next_wake_at(engine: &Engine, now: Timestamp) -> Result<Option<Timestamp>, EngineError> {
    let mut earliest = None;
    for instance in known_instances(engine).await? {
        if instance.status.is_terminal() {
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

async fn known_instances(engine: &Engine) -> Result<Vec<Instance>, EngineError> {
    let mut instances = Vec::new();
    for id in engine.known_ids() {
        match engine.store().get(&id).await {
            Ok(instance) => instances.push(instance),
            Err(StoreError::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(instances)
}

impl Engine {
    async fn note_spawned(&self, claimed: &[Instance]) -> Result<(), EngineError> {
        for instance in claimed {
            self.remember(instance.id.clone());
            if let Some(parent) = &instance.parent {
                self.remember(parent.id.clone());
            }
        }
        Ok(())
    }
}

async fn parent_row_version(engine: &Engine, id: &InstanceId) -> Result<u64, EngineError> {
    match engine.store().get(id).await {
        Ok(parent) => Ok(parent.version),
        Err(error) => Err(error.into()),
    }
}

fn still_events(
    instance: &Instance,
    continuation: &Continuation,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    let mut commit = Commit::new(
        instance.id.clone(),
        instance.version,
        Status::Waiting(WaitReason::Events),
        // wake_at stays unset: an unsatisfied listen is not claimable.
        now,
    );
    commit.continuation = Some(encode(&instance.id, continuation)?);
    Ok(commit)
}

fn still_waiting(
    instance: &Instance,
    continuation: &Continuation,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    parked(instance, continuation, WaitReason::Join, now)
}

fn still_child(
    instance: &Instance,
    continuation: &Continuation,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    parked(instance, continuation, WaitReason::Child, now)
}

fn parked(
    instance: &Instance,
    continuation: &Continuation,
    reason: WaitReason,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    let mut commit = Commit::new(
        instance.id.clone(),
        instance.version,
        Status::Waiting(reason),
        now,
    );
    commit.continuation = Some(encode(&instance.id, continuation)?);
    Ok(commit)
}

/// One activity after its journal commits.
struct Journalled {
    /// Version the next commit must expect.
    version: u64,
    /// What the interpreter resumes with, when the activity ran.
    event: Option<Resume>,
    /// A commit that ends the step: the unknown-outcome fault, or a cut
    /// after `Pending`. The caller applies this instead of translating.
    done: Option<Commit>,
}

impl Journalled {
    fn resume(event: Resume) -> Self {
        Self {
            version: 0,
            event: Some(event),
            done: None,
        }
    }

    fn finished(commit: Commit) -> Self {
        Self {
            version: commit.expected_version,
            event: None,
            done: Some(commit),
        }
    }
}

/// What the latest journal row for this key says the executor should do.
#[allow(clippy::large_enum_variant)]
enum OpenAttempt {
    /// No open `Pending` row, or an idempotent one that must be run again.
    Run { attempt: u32 },
    /// A `Pending` row whose executor must not be run again.
    Unknown { pending: StepResult },
}

fn activity_idempotent(engine: &Engine, kind: TaskKind) -> bool {
    match kind {
        TaskKind::Http => false,
        other => engine
            .executor(other)
            .is_some_and(|executor| executor.idempotent()),
    }
}

fn activity_row(
    instance: &Instance,
    block: &Block,
    kind: TaskKind,
    attempt: u32,
    outcome: StepOutcome,
    started_at: Timestamp,
    finished_at: Timestamp,
) -> StepResult {
    StepResult {
        instance: instance.id.clone(),
        key: block.key().clone(),
        kind,
        attempt,
        input: block.input().clone(),
        outcome,
        started_at,
        finished_at,
    }
}

fn faulted_at(instance: &InstanceId, version: u64, now: Timestamp, fault: Fault) -> Commit {
    let mut commit = Commit::new(instance.clone(), version, Status::Faulted, now);
    commit.fault = Some(fault);
    commit
}

fn still_running(
    instance: &Instance,
    version: u64,
    continuation: &Continuation,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    let mut commit = Commit::new(instance.id.clone(), version, Status::Running, now);
    commit.continuation = Some(encode(&instance.id, continuation)?);
    commit.release_lease = false;
    Ok(commit)
}

fn faulted(instance: &Instance, now: Timestamp, fault: Fault) -> Commit {
    let mut commit = Commit::new(instance.id.clone(), instance.version, Status::Faulted, now);
    commit.fault = Some(fault);
    commit
}

fn fault_commit(
    instance: &Instance,
    now: Timestamp,
    error: &EngineError,
) -> Result<Commit, EngineError> {
    match error {
        EngineError::Store(_) | EngineError::Bus(_) => Err(rewrap(error)),
        other => Ok(faulted(
            instance,
            now,
            Fault::runtime(instance.id.as_str(), other.to_string()),
        )),
    }
}

fn rewrap(error: &EngineError) -> EngineError {
    match error {
        EngineError::Store(error) => EngineError::Store(StoreError::Backend(error.to_string())),
        EngineError::Bus(error) => EngineError::Bus(crate::BusError::Backend(error.to_string())),
        EngineError::Continuation { instance, detail } => EngineError::Continuation {
            instance: instance.clone(),
            detail: detail.clone(),
        },
        EngineError::NoExecutor(kind) => EngineError::NoExecutor(*kind),
        EngineError::Unsupported(detail) => EngineError::Unsupported(detail.clone()),
        EngineError::Events(detail) => EngineError::Events(detail.clone()),
    }
}

/// Input key under which a spawned branch carries its task position.
///
/// `SpawnChild` has no position field, and MR-5 does not change that type.
/// The position has to travel with the child or `start_at` has nothing to
/// start. It is stripped before the branch runs, so a workflow that reads
/// its input does not see it. The key is a JSON Pointer, which a workflow
/// input object cannot collide with: pointers start with `/` or are empty,
/// and this key does not.
pub const BRANCH_POSITION: &str = "\u{0}tideloom.branch";

/// Attach a branch position to the fork input. A non-object input is wrapped
/// so the position has somewhere to live, and unwrapped again on start.
#[must_use]
pub fn branch_input(input: &Value, position: &str) -> Value {
    match input {
        Value::Object(fields) => {
            let mut fields = fields.clone();
            fields.insert(
                BRANCH_POSITION.to_string(),
                Value::String(position.to_string()),
            );
            Value::Object(fields)
        }
        other => serde_json::json!({
            BRANCH_POSITION: position,
            "\u{0}tideloom.input": other,
        }),
    }
}

fn branch_start(input: &Value) -> Option<String> {
    input.get(BRANCH_POSITION)?.as_str().map(str::to_string)
}

fn visible_input(input: &Value) -> Value {
    let Some(fields) = input.as_object() else {
        return input.clone();
    };
    if let Some(wrapped) = fields.get("\u{0}tideloom.input") {
        return wrapped.clone();
    }
    let mut fields = fields.clone();
    fields.remove(BRANCH_POSITION);
    Value::Object(fields)
}

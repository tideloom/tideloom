use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tideloom_core::Fault;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;

use crate::DefinitionKey;
use crate::InstanceId;
use crate::ParentRef;
use crate::Status;
use crate::TaskKind;

/// How far one effectful task got.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StepOutcome {
    /// The executor was about to run it. Written before the side effect.
    Pending,
    /// The executor returned this raw output.
    Output(Value),
    /// The executor failed.
    Fault(Fault),
    /// The worker died between `Pending` and the result, and the executor is
    /// not idempotent. The instance is faulted; a person decides what happened.
    Unknown,
}

/// One row of the `task_execution` journal.
///
/// The journal is written, never read back into a run: the continuation is
/// the only resume state. It is the source for durations, retries, and
/// post-mortems.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// The instance the task belongs to.
    pub instance: InstanceId,
    /// Position plus execution key of the task.
    pub key: TaskKey,
    /// What kind of executor ran it.
    pub kind: TaskKind,
    /// Zero-based execution attempt of this exact key, bumped on crash recovery.
    pub attempt: u32,
    /// Transformed task input.
    pub input: Value,
    /// Where it stands.
    pub outcome: StepOutcome,
    /// When the executor was called.
    pub started_at: Timestamp,
    /// When the outcome was recorded. Equals `started_at` for `Pending`.
    pub finished_at: Timestamp,
}

/// A child instance to create in the same commit as its parent's change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnChild {
    /// Fixed by the engine before the commit so the parent can refer to it.
    pub id: InstanceId,
    /// Which definition the child runs.
    pub definition: DefinitionKey,
    /// Who it belongs to.
    pub parent: ParentRef,
    /// Child input.
    pub input: Value,
}

/// Everything one engine step changes, applied atomically or not at all.
///
/// `expected_version` is the instance version the step read.
/// [`InstanceStore::apply`](crate::InstanceStore::apply) refuses the commit
/// when the stored version differs, and changes nothing. Backends decide how
/// to make the fields atomic; a transaction handle never crosses the trait.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    /// The instance this commit is about.
    pub instance: InstanceId,
    /// CAS guard.
    pub expected_version: u64,
    /// New status.
    pub status: Status,
    /// New serialized continuation, or `None` when the status is terminal.
    pub continuation: Option<Vec<u8>>,
    /// Earliest instant to claim again, for `Waiting(Timer)`.
    pub wake_at: Option<Timestamp>,
    /// Output when `Completed`.
    pub output: Option<Value>,
    /// Fault when `Faulted`.
    pub fault: Option<Fault>,
    /// Journal rows to upsert by `(instance, key, attempt)`.
    pub step_results: Vec<StepResult>,
    /// Children to create: `fork` branches, `run: workflow` targets.
    pub spawn_children: Vec<SpawnChild>,
    /// Instances to mark `Cancelled` if they are not terminal yet: losing
    /// `compete` branches, siblings of a failed cooperative branch.
    pub cancel: Vec<InstanceId>,
    /// Whether to release the lease. `false` keeps it for an immediate next step.
    pub release_lease: bool,
    /// When the commit was built.
    pub at: Timestamp,
}

impl Commit {
    /// A commit that only changes the status and continuation.
    #[must_use]
    pub fn new(instance: InstanceId, expected_version: u64, status: Status, at: Timestamp) -> Self {
        Self {
            instance,
            expected_version,
            status,
            continuation: None,
            wake_at: None,
            output: None,
            fault: None,
            step_results: Vec::new(),
            spawn_children: Vec::new(),
            cancel: Vec::new(),
            release_lease: true,
            at,
        }
    }
}

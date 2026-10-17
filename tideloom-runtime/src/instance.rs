use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tideloom_core::Fault;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use uuid::Uuid;

/// Identity of one workflow instance. A UUID v7, so ids sort by creation time.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstanceId(String);

impl InstanceId {
    /// A fresh id. This is the one place the runtime reads the wall clock
    /// without going through [`Clock`](crate::Clock); ids carry no deadline.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    /// Wrap an id read back from storage.
    #[must_use]
    pub fn from_string(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Identity of a worker process, for leases.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkerId(pub String);

impl fmt::Display for WorkerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which definition an instance runs: the identity triple plus the content
/// hash it was created with. The hash pins the exact tree for the whole run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DefinitionKey {
    /// `document.namespace`.
    pub namespace: String,
    /// `document.name`.
    pub name: String,
    /// `document.version`.
    pub version: String,
    /// Hex of [`ContentHash`](tideloom_core::ContentHash), as `to_hex` prints it.
    pub content_hash: String,
}

/// Why an instance is not runnable right now.
///
/// An effectful task in progress is not a wait. The instance stays `Running`
/// with a live lease and [`Instance::in_flight`] set. A lapsed lease is what
/// the sweeper sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitReason {
    /// `wait` or a retry backoff. `Instance::wake_at` says when.
    Timer,
    /// `fork`: child instances are running. Their terminal commits wake the parent.
    Join,
    /// `run: workflow`: one child instance is running.
    Child,
    /// `listen`: events are being collected in the inbox.
    Events,
}

/// Lifecycle of an instance.
///
/// `Runnable` and `Waiting(Timer)` with a due `wake_at` are what
/// [`InstanceStore::claim`](crate::InstanceStore::claim) hands out. The other
/// `Waiting` reasons are released by the engine when their condition holds.
/// `Completed`, `Faulted`, and `Cancelled` are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Ready to be claimed.
    Runnable,
    /// Claimed by a worker that holds the lease.
    Running,
    /// Parked until the reason is satisfied.
    Waiting(WaitReason),
    /// Finished with an output.
    Completed,
    /// Finished with an uncaught fault.
    Faulted,
    /// Stopped by a parent (compete, sibling failure) or an operator.
    Cancelled,
}

impl Status {
    /// `Completed`, `Faulted`, or `Cancelled`.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Faulted | Self::Cancelled)
    }
}

/// The parent of a child instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentRef {
    /// The instance that created this one.
    pub id: InstanceId,
    /// Branch index when the child is a `fork` branch; `None` for `run: workflow`.
    pub branch: Option<usize>,
}

/// A worker's exclusive hold on an instance.
///
/// A lease is only who holds the instance and until when. The open activity,
/// if any, is [`Instance::in_flight`], not a field of the lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Who holds it.
    pub worker: WorkerId,
    /// When it lapses. A sweeper returns lapsed instances to `Runnable`.
    pub until: Timestamp,
}

/// The activity a worker has opened and not finished.
///
/// The engine writes this when it commits the `Pending` journal row and
/// clears it when that attempt's outcome lands. It names the row a recovery
/// claim should read; it does not replace that row. A crash leaves it set
/// with the lease still held, so the sweeper can find the instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    /// Position plus execution key of the open activity.
    pub key: TaskKey,
    /// Journal attempt the `Pending` row was written at.
    pub attempt: u32,
}

/// One workflow instance as the store keeps it.
///
/// `continuation` is the serialized [`Continuation`](tideloom_core::Continuation)
/// while the instance is alive, and `None` once it is terminal. The store
/// does not parse it; only the engine does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    /// Identity.
    pub id: InstanceId,
    /// Which definition, pinned by content hash.
    pub definition: DefinitionKey,
    /// Parent, for `fork` branches and `run: workflow` children.
    pub parent: Option<ParentRef>,
    /// Lifecycle.
    pub status: Status,
    /// Serialized continuation, `None` when terminal or not yet started.
    pub continuation: Option<Vec<u8>>,
    /// Bumped on every [`Commit`](crate::Commit). `apply` is a CAS on it.
    pub version: u64,
    /// Current lease, if claimed.
    pub lease: Option<Lease>,
    /// Earliest instant a `Waiting(Timer)` instance may be claimed.
    pub wake_at: Option<Timestamp>,
    /// Task position to start at, for a `fork` branch. `None` starts at the root.
    ///
    /// The branch input is [`Instance::input`] unchanged. The position does
    /// not ride along inside that value.
    pub root_task: Option<String>,
    /// The activity this instance has opened and not finished, if any.
    ///
    /// Set while the instance is `Running` and the executor has not returned.
    /// A recovery claim reads `InstanceStore::latest_step` for this key; the
    /// journal row, not this field, decides whether the outcome is unknown.
    pub in_flight: Option<InFlight>,
    /// Workflow input, kept for `run: workflow` children and diagnostics.
    pub input: Value,
    /// Output once `Completed`.
    pub output: Option<Value>,
    /// Fault once `Faulted`.
    pub fault: Option<Fault>,
    /// When the instance was created.
    pub created_at: Timestamp,
    /// When the last commit landed.
    pub updated_at: Timestamp,
}

impl Instance {
    /// A new, runnable instance with no continuation yet.
    #[must_use]
    pub fn new(
        definition: DefinitionKey,
        parent: Option<ParentRef>,
        input: Value,
        created_at: Timestamp,
    ) -> Self {
        Self {
            id: InstanceId::new(),
            definition,
            parent,
            status: Status::Runnable,
            continuation: None,
            version: 0,
            lease: None,
            wake_at: Some(created_at),
            root_task: None,
            in_flight: None,
            input,
            output: None,
            fault: None,
            created_at,
            updated_at: created_at,
        }
    }
}

use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tideloom_core::Fault;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitReason {
    /// `wait` or a retry backoff. `Instance::wake_at` says when.
    Timer,
    /// An effectful task is running in an executor.
    Activity,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Who holds it.
    pub worker: WorkerId,
    /// When it lapses. A sweeper returns lapsed instances to `Runnable`.
    pub until: Timestamp,
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
            input,
            output: None,
            fault: None,
            created_at,
            updated_at: created_at,
        }
    }
}

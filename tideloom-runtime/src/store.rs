use async_trait::async_trait;
use tideloom_core::Timestamp;

use crate::Commit;
use crate::Instance;
use crate::InstanceId;
use crate::StepResult;
use crate::WorkerId;

mod memory;

pub use memory::MemoryStore;

/// Why a store call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `apply` saw a version other than `expected_version`. Nothing changed.
    #[error("version conflict on {instance}: expected {expected}, found {found}")]
    VersionConflict {
        /// The instance.
        instance: InstanceId,
        /// What the commit expected.
        expected: u64,
        /// What the store holds.
        found: u64,
    },
    /// No such instance.
    #[error("instance {0} not found")]
    NotFound(InstanceId),
    /// `create` with an id that already exists.
    #[error("instance {0} already exists")]
    AlreadyExists(InstanceId),
    /// Anything the backend could not do.
    #[error("store backend: {0}")]
    Backend(String),
}

/// Where instances live.
///
/// The four primitives of ADR-0001 §3, plus `children` for `fork` and
/// `run: workflow` and `steps` for the journal. The engine depends on these
/// guarantees and nothing else:
///
/// - `claim` hands an instance to exactly one worker at a time, and only when it is `Runnable`, or
///   `Waiting(Timer)` with `wake_at <= now`, and holds no live lease. It writes the lease and sets
///   `Running` before returning.
/// - `apply` is a compare-and-set on `version`. On success every field of the [`Commit`] lands, the
///   version is bumped, and the lease is released when the commit says so. On conflict nothing
///   changes.
/// - `sweep` lists instances whose lease lapsed before `before`. It does not change them; the
///   engine does.
#[async_trait]
pub trait InstanceStore: Send + Sync {
    /// Insert a new instance. Fails when the id exists.
    async fn create(&self, instance: Instance) -> Result<(), StoreError>;

    /// Read one instance.
    async fn get(&self, id: &InstanceId) -> Result<Instance, StoreError>;

    /// Lease up to `limit` claimable instances, oldest `wake_at` first.
    ///
    /// `until` is when the leases lapse.
    async fn claim(
        &self,
        worker: &WorkerId,
        now: Timestamp,
        until: Timestamp,
        limit: usize,
    ) -> Result<Vec<Instance>, StoreError>;

    /// Apply one commit atomically, guarded by `expected_version`.
    async fn apply(&self, commit: Commit) -> Result<(), StoreError>;

    /// Ids of `Running` instances whose lease lapsed before `before`.
    async fn sweep(&self, before: Timestamp) -> Result<Vec<InstanceId>, StoreError>;

    /// Children of `parent`, ordered by branch index then creation.
    async fn children(&self, parent: &InstanceId) -> Result<Vec<Instance>, StoreError>;

    /// Journal rows of one instance, oldest first.
    async fn steps(&self, instance: &InstanceId) -> Result<Vec<StepResult>, StoreError>;
}

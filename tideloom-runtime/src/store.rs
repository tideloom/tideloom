use async_trait::async_trait;
use tideloom_core::TaskKey;
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
/// - `wake` flips `Waiting(Events)` rows with an empty `wake_at` to `Runnable`. It is a condition,
///   not a compare-and-set, so the engine calls it outside
///   [`Engine::commit`](crate::Engine::commit).
/// - `next_due` is the earliest future `wake_at` a claim could see. The test driver and the
///   production poll use it for their interval. Correctness does not.
/// - `release_leases` only shortens leases this worker still holds. It does not change status. The
///   next sweep decides whether a recovered activity is still `Pending`.
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
    ///
    /// Tests and operators read this. The engine does not: recovery asks
    /// [`InstanceStore::latest_step`] for the one key [`crate::InFlight`] names.
    async fn steps(&self, instance: &InstanceId) -> Result<Vec<StepResult>, StoreError>;

    /// The newest journal row for one key, if any.
    ///
    /// Newest is the highest `attempt`. A missing instance returns an empty
    /// result, the same as [`InstanceStore::steps`]. The engine uses this to
    /// decide whether a recovered activity is still `Pending`.
    async fn latest_step(
        &self,
        instance: &InstanceId,
        key: &TaskKey,
    ) -> Result<Option<StepResult>, StoreError>;

    /// Make event listeners claimable.
    ///
    /// Each id that is `Waiting(Events)` with an empty `wake_at` becomes
    /// `Runnable`, with `wake_at = now` and its version bumped. A live lease,
    /// a terminal status, a timer, a join, and a row that already has a
    /// `wake_at` are left alone. An unknown id is ignored. The count is how
    /// many rows flipped. Calling this twice is a no-op the second time.
    ///
    /// This is not a compare-and-set. The engine calls it after an emit's
    /// activity commit, not from [`Engine::commit`](crate::Engine::commit).
    async fn wake(&self, ids: &[InstanceId], now: Timestamp) -> Result<usize, StoreError>;

    /// The earliest `wake_at` strictly after `now` that a claim could take.
    ///
    /// A candidate is `Runnable` or `Waiting(Timer)` and holds no lease that
    /// is still live at `now`. `Waiting(Events)`, `Waiting(Join)`, and
    /// `Waiting(Child)` are not candidates: they are not claimable until
    /// something else releases them. `None` means nothing is waiting on a
    /// future instant. The test driver advances a manual clock to this
    /// instant; a production loop uses it as a poll interval. Neither uses
    /// it to decide whether an instance is runnable.
    async fn next_due(&self, now: Timestamp) -> Result<Option<Timestamp>, StoreError>;

    /// Shorten every lease this worker still holds so that it lapses at `now`.
    ///
    /// A graceful shutdown stops claiming, finishes the step already in
    /// progress, then calls this. The instances stay `Running`. The next
    /// sweep is what returns them to `Runnable` and keeps `in_flight`, so a
    /// recovered activity is not treated as a fresh entry. A lease already
    /// ending at or before `now` is left as it is. Another worker's lease is
    /// left as it is. The count is how many rows changed.
    async fn release_leases(&self, worker: &WorkerId, now: Timestamp) -> Result<usize, StoreError>;
}

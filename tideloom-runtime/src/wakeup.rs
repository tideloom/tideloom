use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde::Serialize;

use crate::InstanceId;

mod local;

pub use local::LocalWakeup;

/// Position in the signal log. Monotonic per bus.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Offset(pub u64);

/// Why an instance may be runnable. Advisory only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hint {
    /// A `wake_at` came due.
    Timer,
    /// A child instance reached a terminal status.
    ChildDone(InstanceId),
    /// An event landed in the inbox.
    Event(String),
    /// Something else: an operator action, a sweeper.
    Other,
}

/// One entry in the signal log.
///
/// A signal is a hint, never a source of truth. A worker that receives one
/// still goes through [`InstanceStore::claim`](crate::InstanceStore::claim),
/// and a worker that misses one is covered by the sweeper and `wake_at`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Which instance.
    pub instance: InstanceId,
    /// Why, if known.
    pub hint: Hint,
    /// Where this signal sits in the log.
    pub offset: Offset,
}

/// Why a bus call failed.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// `watch` asked for an offset the bus no longer holds.
    #[error("offset {0:?} is older than the retained log")]
    OffsetExpired(Offset),
    /// Anything the backend could not do.
    #[error("bus backend: {0}")]
    Backend(String),
}

/// Signals from `watch`.
pub type SignalStream = BoxStream<'static, Result<Signal, BusError>>;

/// A replayable log of wake-up signals with consumer offsets.
///
/// Not a queue. Kafka, NATS JetStream, Redis Streams, and an in-process ring
/// buffer fit; a classic AMQP queue does not. The engine commits its offset
/// only after the matching [`InstanceStore::apply`](crate::InstanceStore::apply)
/// succeeded, so a crash between the two replays the signal, which is harmless.
#[async_trait]
pub trait Wakeup: Send + Sync {
    /// Append a signal. Best effort: a failure here degrades latency, not correctness.
    async fn notify(&self, instance: InstanceId, hint: Hint) -> Result<Offset, BusError>;

    /// Signals after `from`, replayed first and then live.
    async fn watch(&self, from: Offset) -> Result<SignalStream, BusError>;

    /// Record that everything up to `offset` has been applied. Idempotent.
    async fn commit_offset(&self, offset: Offset) -> Result<(), BusError>;

    /// The last committed offset, to resume `watch` from after a restart.
    async fn committed(&self) -> Result<Offset, BusError>;
}

use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tideloom_core::Timestamp;

/// Where the engine reads the time.
///
/// The core never reads a clock. Every deadline it enforces is computed from
/// instants the engine recorded, and the engine takes those instants from
/// here. Tests inject [`ManualClock`]; production uses [`SystemClock`].
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// Wall-clock time.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        Timestamp::from_millis(u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock that only moves when a test moves it.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<Timestamp>,
}

impl ManualClock {
    /// Start at `now`.
    #[must_use]
    pub fn at(now: Timestamp) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// Jump to `now`. Moving backwards is allowed; the engine does not care.
    pub fn set(&self, now: Timestamp) {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = now;
    }

    /// Move forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let extra = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        *now = Timestamp::from_millis(now.millis().saturating_add(extra));
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

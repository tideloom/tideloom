//! The production loop: one `run_once` at a time, woken by a signal, a sweep
//! interval, or the next due instant.
//!
//! The three waiters share one [`Notify`]. None of them calls `run_once`. The
//! loop does, so a pass cannot re-enter itself. Shutdown stops claiming, lets
//! the pass already in progress finish (that pass is the one that `apply`s),
//! then shortens this worker's leases. It does not flip those instances back
//! to `Runnable`: the next sweep is what keeps `in_flight` for recovery.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::Engine;
use crate::EngineError;
use crate::Offset;

/// How long a claim lasts, and how often the loop looks again.
///
/// A consumer group is a property of a durable bus, not of this loop. The
/// in-memory bus has no groups, so the group stays on that bus's constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// How long a claim lasts. The sweeper returns a lease that lapses before
    /// its `now`.
    pub lease: Duration,
    /// How many instances one pass claims.
    pub claim_limit: usize,
    /// How often a pass runs even when no signal arrives. This is the backstop
    /// for a lost signal; it is not what makes a due timer runnable.
    pub sweep_every: Duration,
    /// The longest the due task parks before it looks at
    /// [`InstanceStore::next_due`](crate::InstanceStore::next_due) again.
    /// A nearer due instant does not make the task sleep until that instant:
    /// the wait is `min(delay, poll_every)`, and a test moves the
    /// [`ManualClock`](crate::ManualClock) instead of waiting out `delay`.
    /// `Duration::ZERO` polls whenever the shared notify wakes.
    pub poll_every: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            lease: super::step::DEFAULT_LEASE,
            claim_limit: 64,
            sweep_every: Duration::from_secs(5),
            poll_every: Duration::from_secs(1),
        }
    }
}

impl Engine {
    /// Run until `shutdown` is cancelled.
    ///
    /// One task watches the bus from the last committed offset and notifies on
    /// each signal. One task notifies every [`EngineConfig::sweep_every`]. One
    /// task asks [`InstanceStore::next_due`](crate::InstanceStore::next_due)
    /// and notifies after the earlier of the delay to that instant and
    /// [`EngineConfig::poll_every`]. The loop is the only caller of
    /// [`Engine::run_once`], so a pass does not start while another is still
    /// applying.
    ///
    /// Waiting is on the shared notify or on `shutdown`. The loop does not
    /// read the wall clock. `run_once` reads [`Engine::clock`]. A test uses a
    /// [`ManualClock`](crate::ManualClock) and moves it; the due task's
    /// `poll_every` bound is what notices the move. The task does not sleep
    /// until `wake_at`.
    ///
    /// On shutdown the loop stops claiming, waits for the pass in progress,
    /// then calls
    /// [`InstanceStore::release_leases`](crate::InstanceStore::release_leases)
    /// with the clock's current instant. Instances stay `Running`. The next
    /// worker's sweep recovers them, including an activity that was
    /// `in_flight`.
    ///
    /// # Errors
    ///
    /// A store failure, or a pass that failed. A signal watch that ends is a
    /// lost hint: the sweep interval and the due poll keep running. The loop
    /// does not claim because the watch ended.
    pub async fn run(
        &self,
        config: EngineConfig,
        shutdown: CancellationToken,
    ) -> Result<(), EngineError> {
        let wake = Arc::new(Notify::new());
        let from = self.wakeup().committed().await?;
        tokio::pin! {
            let watch = watch_signals(self, Arc::clone(&wake), from, shutdown.clone());
            let sweep = sweep_timer(Arc::clone(&wake), config.sweep_every, shutdown.clone());
            let due = due_poll(self, Arc::clone(&wake), config.poll_every, shutdown.clone());
        }

        // One pass up front claims work that was already due, including a
        // signal that landed before the watcher subscribed. A shutdown that
        // arrives during this pass does not abandon it: the pass applies,
        // then the leases are shortened below.
        if !shutdown.is_cancelled() {
            self.run_once_with(config.lease, config.claim_limit).await?;
        }

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let permit = wake.notified();
            tokio::pin!(permit);
            // Register before waiting. A notify that lands in between is kept,
            // and shutdown still wins the select.
            permit.as_mut().enable();
            // Only `Wake::Notify` claims. A waiter that ends, or the shutdown
            // token, stops the loop. A bus or store error is returned.
            let wake_reason = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                result = &mut watch => result?,
                () = &mut sweep => Wake::Stopped,
                result = &mut due => result?,
                // A waiter notified. Drop the permit by taking this branch.
                () = permit => Wake::Notify,
            };
            if shutdown.is_cancelled() || wake_reason != Wake::Notify {
                break;
            }
            self.run_once_with(config.lease, config.claim_limit).await?;
        }

        // The select above is the only claim path, and it has stopped. The
        // pass that was in progress has applied. Shorten what this worker
        // still holds; do not return those rows to `Runnable` here.
        let now = self.clock().now();
        self.store().release_leases(self.worker(), now).await?;
        Ok(())
    }
}

/// Wake the loop for each signal after `from`.
///
/// The stream is dropped when `shutdown` fires, which is what stops the
/// watcher. A stream that ends on its own is a lost hint: the function
/// returns [`Wake::Stopped`] and the sweep interval keeps the loop alive.
async fn watch_signals(
    engine: &Engine,
    wake: Arc<Notify>,
    from: Offset,
    shutdown: CancellationToken,
) -> Result<Wake, EngineError> {
    let mut signals = engine.wakeup().watch(from).await?;
    loop {
        let next = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(Wake::Stopped),
            next = signals.next() => next,
        };
        match next {
            Some(Ok(_signal)) => wake.notify_waiters(),
            // A dropped signal is a hint. The sweep interval and `next_due`
            // are the backstop, so ending the watch does not end the loop
            // and does not claim by itself.
            Some(Err(_)) | None => return Ok(Wake::Stopped),
        }
    }
}

/// Wake the loop on the sweep interval until shutdown.
///
/// Ending is not a claim. Ticks notify the shared [`Notify`], and the loop
/// claims from that permit. `MissedTickBehavior::Delay` keeps a slow pass
/// from scheduling a catch-up burst. The interval is a latency bound for a
/// lost signal, not a sleep the engine uses to decide that work is due. The
/// tick comes from the tokio runtime clock. A test runtime pauses that clock;
/// this function does not call `sleep` for a fixed time of its own. The task
/// stays up for the whole run: one tick notifies, and the next tick is the
/// next backstop.
async fn sweep_timer(wake: Arc<Notify>, every: Duration, shutdown: CancellationToken) {
    if every.is_zero() {
        shutdown.cancelled().await;
        return;
    }
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` is ready immediately. Skip that tick: the loop already runs
    // one pass before waiting.
    ticks.tick().await;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            _ = ticks.tick() => wake.notify_waiters(),
        }
    }
}

/// Wake the loop when a future `wake_at` may have come due.
///
/// The wait is the earlier of the delay to [`InstanceStore::next_due`] and
/// `poll_every`. It is never the full delay to a far `wake_at`: a test moves
/// the [`ManualClock`](crate::ManualClock) and this task notices on the next
/// bound, then notifies the loop. The engine reads
/// [`Engine::clock`](crate::Engine::clock) inside `run_once`; this task reads
/// it only to choose the bound. A zero `poll_every` does not spin: it waits
/// on the shared notify or on shutdown.
async fn due_poll(
    engine: &Engine,
    wake: Arc<Notify>,
    poll_every: Duration,
    shutdown: CancellationToken,
) -> Result<Wake, EngineError> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(Wake::Stopped);
        }
        let now = engine.clock().now();
        let delay = match engine.store().next_due(now).await? {
            Some(at) if at.millis() > now.millis() => {
                Duration::from_millis(at.millis() - now.millis())
            }
            Some(_) => Duration::ZERO,
            None => poll_every,
        };
        let wait = if poll_every.is_zero() {
            Duration::ZERO
        } else {
            delay.min(poll_every)
        };
        if wait.is_zero() {
            wake.notify_waiters();
            notified_or_shutdown(&wake, &shutdown).await;
            continue;
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(Wake::Stopped),
            () = tokio::time::sleep(wait) => wake.notify_waiters(),
        }
    }
}

/// Why [`Engine::run`] woke.
///
/// `Notify` is the only reason that claims. `Stopped` is a waiter ending
/// because shutdown cancelled it. A failure is an [`EngineError`], not a
/// variant: the select returns it before this value is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    /// A signal, a sweep tick, or the due bound notified the shared permit.
    Notify,
    /// A waiter ended without a claim. The loop stops.
    Stopped,
}

/// Park until another waiter notifies, or until shutdown.
async fn notified_or_shutdown(wake: &Notify, shutdown: &CancellationToken) {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => {}
        () = wake.notified() => {}
    }
}

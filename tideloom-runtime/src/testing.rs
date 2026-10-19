//! Helpers that drive an engine from a [`ManualClock`](crate::ManualClock).
//!
//! Production code does not use these. A test puts one `Arc<ManualClock>` in
//! [`EngineParts::clock`](crate::EngineParts::clock) and passes that same
//! `Arc` here. The helper moves that clock to the store's next due instant.
//! It does not build a second clock. [`Clock`](crate::Clock) is not
//! [`Any`](std::any::Any), so the helper cannot downcast the engine's clock;
//! the caller already holds the concrete `Arc`.

use std::sync::Arc;

use crate::Clock;
use crate::Engine;
use crate::EngineError;
use crate::ManualClock;

/// Step until nothing is claimable, advancing `clock` to the next due instant.
///
/// `clock` is the [`ManualClock`] in [`Engine::clock`](crate::Engine::clock).
/// A future `wait` or retry stops a pass that only claims what is due. This
/// helper asks the store for the earliest later `wake_at` and moves `clock`
/// there, so a test reaches a terminal status without sleeping. It stops
/// when a pass claims nothing and the store has no later due instant.
///
/// A join does not need the clock: finishing a child releases the parent
/// inside the same drive. A listen becomes claimable when the emitting
/// engine calls [`InstanceStore::wake`](crate::InstanceStore::wake), not
/// because this helper scans inboxes.
///
/// # Errors
///
/// The same as [`Engine::run_once`](crate::Engine::run_once).
pub async fn drive_until_idle(
    engine: &Engine,
    clock: &Arc<ManualClock>,
) -> Result<usize, EngineError> {
    let mut stepped = 0;
    loop {
        let count = engine.run_once().await?;
        stepped += count.processed;
        if count.processed > 0 {
            continue;
        }
        let now = clock.now();
        let Some(next) = engine.store().next_due(now).await? else {
            return Ok(stepped);
        };
        // A due instant that this pass did not claim is a live lease or a
        // status `claim` does not return. Stop instead of spinning on it.
        if next <= now {
            return Ok(stepped);
        }
        clock.set(next);
    }
}

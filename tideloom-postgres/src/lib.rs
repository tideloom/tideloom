//! PostgreSQL backends for the `tideloom-runtime` traits.
//!
//! This crate owns every SQL implementation. `tideloom-runtime` does not
//! depend on `sqlx`. [`PgStore`] is the [`InstanceStore`](tideloom_runtime::InstanceStore)
//! from milestone M3 MR-S1. [`PgWakeup`] is the [`Wakeup`](tideloom_runtime::Wakeup)
//! from M3 MR-S2. [`PgEventBus`] is the event inbox from MR-S3.
//! Definitions arrive in a later MR.
//!
//! Time never comes from the database clock. Callers pass a
//! [`Timestamp`](tideloom_core::Timestamp); the store converts it to
//! `TIMESTAMPTZ` at the boundary. Signal-log audit columns use the bus clock
//! the same way, and no query compares them.

#![warn(missing_docs)]

mod events;
mod store;
mod wakeup;

pub use events::PgEventBus;
pub use store::PgStore;
pub use wakeup::PgWakeup;

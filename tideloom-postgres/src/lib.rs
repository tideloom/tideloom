//! PostgreSQL backends for the `tideloom-runtime` traits.
//!
//! This crate owns every SQL implementation. `tideloom-runtime` does not
//! depend on `sqlx`. [`PgStore`] is the [`InstanceStore`](tideloom_runtime::InstanceStore)
//! from milestone M3 MR-S1. [`PgDefinitions`] is the
//! [`DefinitionSource`](tideloom_runtime::DefinitionSource) from MR-S4.
//! Wake-up and events arrive in later MRs and are not part of this crate yet.
//!
//! Time never comes from the database clock. Callers pass a
//! [`Timestamp`](tideloom_core::Timestamp); the store converts it to
//! `TIMESTAMPTZ` at the boundary.

#![warn(missing_docs)]

mod definitions;
mod store;

pub use definitions::PgDefinitions;
pub use store::PgStore;

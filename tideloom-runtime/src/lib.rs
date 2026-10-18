//! Run `tideloom-core` continuations against a store, a wake-up bus, and
//! activity executors.
//!
//! This crate holds everything that touches the outside world. The core
//! crate is a pure interpreter: it takes a definition, a continuation, and an
//! event, and returns an [`Outcome`](tideloom_core::Outcome). The runtime
//! decides where continuations live ([`InstanceStore`]), how workers learn
//! that an instance is runnable ([`Wakeup`]), who performs effectful tasks
//! ([`ActivityExecutor`]), where definitions come from
//! ([`DefinitionSource`]), how events travel ([`EventSink`],
//! [`EventSource`]), and what time it is ([`Clock`]). [`Engine`] composes
//! them.
//!
//! The traits are the contract of ADR-0001 §9 and of milestone M2 in
//! `docs/milestone-m2-single-process-runtime.md`. Implementations arrive in
//! their own pull requests; this crate ships the in-memory ones that are small
//! enough to be obviously right ([`ManualClock`], [`SystemClock`],
//! [`MemoryDefinitions`]).
//!
//! [`engine`] is public so milestone MRs can add step helpers beside
//! [`Engine`] without a new crate path.

#![warn(missing_docs)]

mod clock;
mod commit;
mod definitions;
/// The engine, its step loop, and the listen helpers that loop calls.
///
/// [`Engine`] is the composition root. [`engine::listen`] is
/// the Events branch of translate and the post-claim inbox check.
pub mod engine;
mod events;
mod executor;
mod instance;
mod store;
#[cfg(feature = "test-util")]
mod testing;
mod wakeup;

pub use clock::Clock;
pub use clock::ManualClock;
pub use clock::SystemClock;
pub use commit::Commit;
pub use commit::SpawnChild;
pub use commit::StepOutcome;
pub use commit::StepResult;
pub use definitions::DefinitionSource;
pub use definitions::MemoryDefinitions;
pub use engine::Engine;
pub use engine::EngineError;
pub use engine::EngineParts;
pub use engine::StepCount;
pub use engine::UNKNOWN_DETAIL;
pub use engine::listen::ListenRelease;
pub use events::CloudEvent;
pub use events::EventError;
pub use events::EventSink;
pub use events::EventSource;
pub use events::LocalEventBus;
pub use executor::ActivityError;
pub use executor::ActivityExecutor;
pub use executor::EmitExecutor;
pub use executor::HttpExecutor;
pub use executor::LogExecutor;
pub use executor::TaskExecution;
pub use executor::TaskKind;
pub use instance::DefinitionKey;
pub use instance::InFlight;
pub use instance::Instance;
pub use instance::InstanceId;
pub use instance::Lease;
pub use instance::ParentRef;
pub use instance::Status;
pub use instance::WaitReason;
pub use instance::WorkerId;
pub use store::InstanceStore;
pub use store::MemoryStore;
pub use store::StoreError;
#[cfg(feature = "test-util")]
pub use testing::drive_until_idle;
pub use wakeup::BusError;
pub use wakeup::Hint;
pub use wakeup::LocalWakeup;
pub use wakeup::Offset;
pub use wakeup::Signal;
pub use wakeup::SignalStream;
pub use wakeup::Wakeup;

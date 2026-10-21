use std::collections::HashMap;
use std::sync::Arc;

use crate::ActivityExecutor;
use crate::BusError;
use crate::Clock;
use crate::Commit;
use crate::DefinitionSource;
use crate::EventSink;
use crate::EventSource;
use crate::InstanceStore;
use crate::Offset;
use crate::StoreError;
use crate::TaskKind;
use crate::Wakeup;
use crate::WorkerId;

mod child;
mod join;
pub(crate) mod listen;
mod run;
mod step;
mod translate;

pub use run::EngineConfig;
pub use step::StepCount;

/// Why an engine step failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The store refused.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The bus refused.
    #[error(transparent)]
    Bus(#[from] BusError),
    /// A stored continuation could not be decoded.
    #[error("continuation of {instance}: {detail}")]
    Continuation {
        /// Which instance.
        instance: String,
        /// What went wrong.
        detail: String,
    },
    /// No executor is registered for a task kind.
    #[error("no executor for {0:?}")]
    NoExecutor(TaskKind),
    /// A pause this engine does not run yet. Faulting the instance would hide
    /// a missing milestone behind a workflow fault, so the step returns it.
    #[error("{0}")]
    Unsupported(String),
    /// The event bus refused a subscribe, an inbox read, or an ack.
    #[error("events: {0}")]
    Events(String),
}

/// Everything an [`Engine`] is built from.
///
/// All `dyn`: the backends are chosen by configuration at startup, and the
/// engine calls each of them once per block, so dynamic dispatch costs
/// nothing that matters.
pub struct EngineParts {
    /// This worker's identity for leases.
    pub worker: WorkerId,
    /// Where instances live.
    pub store: Arc<dyn InstanceStore>,
    /// Wake-up hints.
    pub wakeup: Arc<dyn Wakeup>,
    /// Definitions by key.
    pub definitions: Arc<dyn DefinitionSource>,
    /// Executors by task kind. A kind with no executor faults the instance.
    pub executors: Vec<Arc<dyn ActivityExecutor>>,
    /// Where `emit` goes.
    pub events_out: Arc<dyn EventSink>,
    /// Where `listen` reads.
    pub events_in: Arc<dyn EventSource>,
    /// The time.
    pub clock: Arc<dyn Clock>,
}

/// Claims instances, runs the interpreter, and commits the result.
///
/// One step is `claim` → decode the continuation → `start` / `start_at` /
/// `resume` → translate the [`Outcome`](tideloom_core::Outcome) into a
/// [`Commit`] → [`Engine::commit`] → `notify`. An activity commits twice
/// around the executor: `Pending`, then the outcome upserted on the same
/// `(instance, key, attempt)` row. [`Engine::commit`] is the only place that
/// calls [`InstanceStore::apply`] and [`Wakeup::commit_offset`], in that
/// order. A `fork` join spawns its branches in that commit and re-evaluates
/// them from `children` when a branch finishes. `run: workflow` spawns one
/// child the same way, with `parent.branch = None`, and releases the parent
/// through that same path.
pub struct Engine {
    worker: WorkerId,
    store: Arc<dyn InstanceStore>,
    wakeup: Arc<dyn Wakeup>,
    definitions: Arc<dyn DefinitionSource>,
    executors: HashMap<TaskKind, Arc<dyn ActivityExecutor>>,
    events_out: Arc<dyn EventSink>,
    events_in: Arc<dyn EventSource>,
    clock: Arc<dyn Clock>,
}

impl Engine {
    /// Assemble an engine. Executors are indexed by [`ActivityExecutor::kind`];
    /// a later executor for the same kind replaces an earlier one.
    #[must_use]
    pub fn new(parts: EngineParts) -> Self {
        let executors = parts
            .executors
            .into_iter()
            .map(|executor| (executor.kind(), executor))
            .collect();
        Self {
            worker: parts.worker,
            store: parts.store,
            wakeup: parts.wakeup,
            definitions: parts.definitions,
            executors,
            events_out: parts.events_out,
            events_in: parts.events_in,
            clock: parts.clock,
        }
    }

    /// This worker's identity.
    #[must_use]
    pub fn worker(&self) -> &WorkerId {
        &self.worker
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn InstanceStore> {
        &self.store
    }

    /// The bus.
    #[must_use]
    pub fn wakeup(&self) -> &Arc<dyn Wakeup> {
        &self.wakeup
    }

    /// Definitions.
    #[must_use]
    pub fn definitions(&self) -> &Arc<dyn DefinitionSource> {
        &self.definitions
    }

    /// The executor for `kind`, if one is registered.
    #[must_use]
    pub fn executor(&self, kind: TaskKind) -> Option<&Arc<dyn ActivityExecutor>> {
        self.executors.get(&kind)
    }

    /// Where `emit` goes.
    #[must_use]
    pub fn events_out(&self) -> &Arc<dyn EventSink> {
        &self.events_out
    }

    /// Where `listen` reads.
    #[must_use]
    pub fn events_in(&self) -> &Arc<dyn EventSource> {
        &self.events_in
    }

    /// The clock.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Apply a commit, then acknowledge the signal that led to it.
    ///
    /// This is the only place `apply` and `commit_offset` are called, and the
    /// order is fixed: the store first, the bus second. A crash between the
    /// two replays a signal whose work is already done, which `claim` turns
    /// into a no-op. The reverse order would lose work.
    ///
    /// `offset` is `None` when the step was not triggered by a signal, for
    /// example by the sweeper.
    pub async fn commit(&self, commit: Commit, offset: Option<Offset>) -> Result<(), EngineError> {
        self.store.apply(commit).await?;
        if let Some(offset) = offset {
            self.wakeup.commit_offset(offset).await?;
        }
        Ok(())
    }
}

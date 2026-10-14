//! `emit` that records the event instead of publishing it.
//!
//! MR-7's [`crate::EmitExecutor`] publishes into an [`crate::EventSink`] and
//! replaces this for a workflow that listens. A workflow that only emits, and
//! a test that wants to see the payload, uses this. Publishing somewhere and
//! recording it are different side effects, so both can be registered; the
//! later one for [`TaskKind::Emit`] wins.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::ActivityError;
use crate::ActivityExecutor;
use crate::TaskExecution;
use crate::TaskKind;

/// Records each `emit` task input and returns it as the task output.
#[derive(Debug, Default)]
pub struct LogExecutor {
    recorded: Mutex<Vec<Value>>,
}

impl LogExecutor {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Task inputs recorded so far, in call order.
    pub fn recorded(&self) -> Vec<Value> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl ActivityExecutor for LogExecutor {
    fn kind(&self) -> TaskKind {
        TaskKind::Emit
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(task.input.clone());
        Ok(task.input)
    }
}

//! `call: http` through the core client.
//!
//! The interpreter builds the request. This executor only runs it and returns
//! the raw output, before `output.as`. It is not idempotent: a retried call
//! is a second request, and a crash between the journal commits faults the
//! instance instead of sending again.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tideloom_core::Block;
use tideloom_core::Definition;
use tideloom_core::Fault;

use crate::ActivityError;
use crate::ActivityExecutor;
use crate::DefinitionSource;
use crate::TaskExecution;
use crate::TaskKind;

/// Performs `call: http` with [`tideloom_core::http_call`].
///
/// The engine calls [`HttpExecutor::call`] when no HTTP executor is registered,
/// because that path still has the interpreter's [`Block`]. Registering one
/// replaces the client: the engine then calls [`ActivityExecutor::execute`].
/// `execute` cannot rebuild the block, so this type's `execute` faults rather
/// than sending a request the block would not have sent.
pub struct HttpExecutor {
    definitions: Arc<dyn DefinitionSource>,
}

impl HttpExecutor {
    /// Resolve the call node from `definitions` when a caller has only the key.
    #[must_use]
    pub fn new(definitions: Arc<dyn DefinitionSource>) -> Self {
        Self { definitions }
    }

    /// Run one blocked HTTP call.
    ///
    /// `workflow_input` is `$workflow.input`. Task input and `$context` come
    /// from `block`.
    pub fn call(
        definition: &Definition,
        workflow_input: &Value,
        block: &Block,
    ) -> Result<Value, ActivityError> {
        tideloom_core::http_call(definition, workflow_input, block).map_err(ActivityError::Fault)
    }
}

#[async_trait]
impl ActivityExecutor for HttpExecutor {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        let _ = self
            .definitions
            .get(&task.definition)
            .await
            .map_err(|error| ActivityError::Executor(error.to_string()))?;
        Err(ActivityError::Fault(Fault::runtime(
            task.key.position(),
            "call: http is sent from the interpreter block; leave the HTTP executor unregistered",
        )))
    }
}

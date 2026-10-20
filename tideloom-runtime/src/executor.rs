use std::any::Any;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tideloom_core::BlockDetail;
use tideloom_core::CallKind;
use tideloom_core::Continuation;
use tideloom_core::Fault;
use tideloom_core::NodeKind;
use tideloom_core::RunKind;
use tideloom_core::TaskKey;

use crate::DefinitionKey;
use crate::InstanceId;

mod emit;
mod http;

pub use emit::EmitExecutor;
pub use http::HttpExecutor;

/// Which executor an effectful task is routed to.
///
/// This is the `task_execution.kind` column. `run: workflow` is not here: it
/// starts a child instance instead of calling an executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskKind {
    /// `call: http`.
    Http,
    /// `call: grpc`.
    Grpc,
    /// `call: openapi`.
    OpenApi,
    /// `call: asyncapi`.
    AsyncApi,
    /// `call: a2a`.
    A2a,
    /// `call: mcp`.
    Mcp,
    /// `emit`.
    Emit,
    /// `run: shell`.
    Shell,
    /// `run: script`.
    Script,
    /// `run: container`.
    Container,
}

impl TaskKind {
    /// The executor kind for an effectful node, or `None` for everything else.
    #[must_use]
    pub fn of(kind: &NodeKind) -> Option<Self> {
        match kind {
            NodeKind::Call(CallKind::Http) => Some(Self::Http),
            NodeKind::Call(CallKind::Grpc) => Some(Self::Grpc),
            NodeKind::Call(CallKind::OpenApi) => Some(Self::OpenApi),
            NodeKind::Call(CallKind::AsyncApi) => Some(Self::AsyncApi),
            NodeKind::Call(CallKind::A2a) => Some(Self::A2a),
            NodeKind::Call(CallKind::Mcp) => Some(Self::Mcp),
            NodeKind::Emit => Some(Self::Emit),
            NodeKind::Run(RunKind::Shell) => Some(Self::Shell),
            NodeKind::Run(RunKind::Script) => Some(Self::Script),
            NodeKind::Run(RunKind::Container) => Some(Self::Container),
            NodeKind::Call(CallKind::Function(_))
            | NodeKind::Run(RunKind::Workflow)
            | NodeKind::Root
            | NodeKind::Do
            | NodeKind::Set
            | NodeKind::Switch
            | NodeKind::SwitchCase { .. }
            | NodeKind::For
            | NodeKind::Fork { .. }
            | NodeKind::Try { .. }
            | NodeKind::Raise
            | NodeKind::Wait
            | NodeKind::Listen => None,
        }
    }
}

/// One effectful task handed to an executor.
///
/// Everything the executor may need is here, so it does not read the store
/// or the definition tree. `context` and `workflow_input` back the
/// `$context` and `$workflow.input` an executor may expand in its own fields.
/// `detail` is the interpreter's block detail, including an `emit` payload
/// already evaluated with enclosing `for` and `try` bindings. `continuation`
/// is that stop, so an executor that still evaluates its own fields sees the
/// same `$item` and `$error`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskExecution {
    /// Which instance.
    pub instance: InstanceId,
    /// Which definition, so the executor can look the node up if it must.
    pub definition: DefinitionKey,
    /// Position plus execution key.
    pub key: TaskKey,
    /// Executor kind.
    pub kind: TaskKind,
    /// Zero-based execution attempt of this key.
    pub attempt: u32,
    /// Transformed task input.
    pub input: Value,
    /// `$context` at the block.
    pub context: Value,
    /// `$workflow.input`.
    pub workflow_input: Value,
    /// Pause-specific data from the block, computed by the interpreter.
    ///
    /// For `emit` this is [`BlockDetail::Emit`]: `emit.event.with` already
    /// evaluated, including `$item` and `$error`. An executor publishes that
    /// value. It does not evaluate the document again.
    pub detail: BlockDetail,
    /// The stop that produced this task, including enclosing frame bindings.
    pub continuation: Continuation,
}

/// Why an executor failed, as the engine sees it.
#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    /// The task failed in a way the workflow can catch. Becomes `Resume::Fault`.
    #[error("{0}")]
    Fault(Fault),
    /// The executor itself broke: configuration, transport, a bug. The engine
    /// faults the instance with a runtime fault at the task position.
    #[error("executor: {0}")]
    Executor(String),
}

/// Performs one kind of effectful task.
///
/// Executors run outside the store transaction. The engine writes a
/// `Pending` journal row before `execute` and the outcome after it. On
/// recovery a still-`Pending` row is executed again only when
/// [`idempotent`](ActivityExecutor::idempotent) is true.
#[async_trait]
pub trait ActivityExecutor: Send + Sync {
    /// Which tasks this executor takes.
    fn kind(&self) -> TaskKind;

    /// Whether running the same task twice is safe. Default: no.
    fn idempotent(&self) -> bool {
        false
    }

    /// Run the task and return its raw output, before `output.as`.
    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError>;

    /// Downcast. The default is not an `EmitExecutor`.
    ///
    /// The engine uses this only to read the ids [`crate::EmitExecutor`]
    /// recorded after `publish`. A custom executor that does not override
    /// this does not wake the store. Hidden because it is not part of the
    /// public contract.
    #[doc(hidden)]
    fn as_any(&self) -> &dyn Any {
        &()
    }
}

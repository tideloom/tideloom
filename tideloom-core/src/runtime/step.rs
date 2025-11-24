use std::time::Instant;

use anyhow::bail;
use tokio_util::sync::CancellationToken;

use serde_json::Value;
use std::time::Duration;

pub type StepResult<T> = anyhow::Result<T>;

/// Basic retry configuration. Extend as the DSL retry semantics are modeled.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 1 }
    }
}

/// Shared runtime context passed to every task execution.
#[derive(Debug, Clone)]
pub struct TaskCtx {
    pub wf_id: String,
    pub task_id: String,
    pub attempt: u32,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    pub http_client: reqwest::Client,
}

impl Default for TaskCtx {
    fn default() -> Self {
        Self {
            wf_id: String::new(),
            task_id: String::new(),
            attempt: 0,
            deadline: None,
            cancel: CancellationToken::new(),
            http_client: reqwest::Client::new(),
        }
    }
}

impl TaskCtx {
    pub fn new(http_client: reqwest::Client) -> Self {
        Self {
            http_client,
            ..Self::default()
        }
    }
}

/// Temporary alias to keep existing code compiling while we pivot to TaskCtx.
pub type WorkflowContext = TaskCtx;

#[async_trait::async_trait]
pub trait Task: Send + Sync {
    async fn execute(&self, ctx: TaskCtx, input: Value) -> StepResult<Value>;
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::default()
    }
    fn timeout(&self) -> Option<Duration> {
        None
    }
    fn name(&self) -> &'static str;
}

/// Lifecycle state of a workflow step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Retrying,
    Succeeded,
    Failed,
}

impl StepStatus {
    /// Checks whether the transition to the next state is allowed.
    pub fn can_transition(self, next: StepStatus) -> bool {
        matches!(
            (self, next),
            (StepStatus::Pending, StepStatus::Running)
                | (StepStatus::Running, StepStatus::Succeeded)
                | (StepStatus::Running, StepStatus::Failed)
                | (StepStatus::Running, StepStatus::Retrying)
                | (StepStatus::Failed, StepStatus::Retrying)
                | (StepStatus::Retrying, StepStatus::Running)
        )
    }
}

/// Runtime instance of a step with lifecycle control.
#[derive(Debug, Clone)]
pub struct StepInstance {
    name: String,
    status: StepStatus,
    attempts: u32,
}

impl StepInstance {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: StepStatus::Pending,
            attempts: 0,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn status(&self) -> StepStatus {
        self.status
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn transition(&mut self, next: StepStatus) -> StepResult<()> {
        if self.status.can_transition(next) {
            if matches!(next, StepStatus::Running) {
                self.attempts += 1;
            }
            self.status = next;
            Ok(())
        } else {
            bail!(
                "invalid transition for step '{}': {:?} -> {:?}",
                self.name,
                self.status,
                next
            )
        }
    }
}

/// Runs a step by enforcing the lifecycle transitions around its execution.
pub async fn run_step(
    step: &mut StepInstance,
    task: &dyn crate::runtime::Task,
    ctx: TaskCtx,
    input: Value,
) -> StepResult<Value> {
    step.transition(StepStatus::Running)?;
    match task.execute(ctx, input).await {
        Ok(output) => {
            step.transition(StepStatus::Succeeded)?;
            Ok(output)
        }
        Err(err) => {
            step.transition(StepStatus::Failed)?;
            Err(anyhow::Error::msg(err.to_string()))
        }
    }
}

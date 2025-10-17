pub type StepResult<T> = std::result::Result<T, String>;

/// Shared runtime context passed to every step execution.
#[derive(Debug, Default, Clone)]
pub struct WorkflowContext {
    pub http_client: reqwest::Client,
}
impl WorkflowContext {
    pub fn new(http_client: reqwest::Client) -> Self {
        Self { http_client }
    }
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


#[async_trait::async_trait]
pub trait Step: Send + Sync {
    type Input: Send;
    type Output: Send;

    async fn execute(&self, ctx: &WorkflowContext, input: Self::Input) -> StepResult<Self::Output>;
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
            Err(format!(
                "invalid transition for step '{}': {:?} -> {:?}",
                self.name, self.status, next
            ))
        }
    }
}

/// Runs a step by enforcing the lifecycle transitions around its execution.
pub async fn run_step<T: Step>(
    step: &mut StepInstance,
    step_type: &T,
    ctx: &WorkflowContext,
    input: T::Input,
) -> StepResult<T::Output> {
    step.transition(StepStatus::Running)?;
    match step_type.execute(ctx, input).await {
        Ok(output) => {
            step.transition(StepStatus::Succeeded)?;
            Ok(output)
        }
        Err(err) => {
            step.transition(StepStatus::Failed)?;
            Err(err)
        }
    }
}

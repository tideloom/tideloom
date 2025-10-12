pub type StepResult<T> = std::result::Result<T, String>;

/// Shared runtime context passed to every step execution.
#[derive(Debug, Default, Clone)]
pub struct WorkflowContext {}

pub trait StepType: Send + Sync {
    type Input: Send;
    type Output: Send;

    async fn execute(&self, ctx: &WorkflowContext, input: Self::Input) -> StepResult<Self::Output>;
}

/// Execution lifecycle for a step. Transitions are validated via `can_transition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Retrying,
}

impl StepStatus {
    fn can_transition(self, next: StepStatus) -> bool {
        matches!(
            (self, next),
            (StepStatus::Pending, StepStatus::Running)
                | (
                    StepStatus::Running,
                    StepStatus::Succeeded | StepStatus::Failed
                )
                | (StepStatus::Failed, StepStatus::Retrying)
                | (StepStatus::Retrying, StepStatus::Running)
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, StepStatus::Succeeded | StepStatus::Failed)
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
            Err(format!(
                "invalid transition for step '{}': {:?} -> {:?}",
                self.name, self.status, next
            ))
        }
    }
}

/// Runs a step by enforcing the lifecycle transitions around its execution.
pub async fn run_step<T: StepType>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::try_join_all;
    use std::future::Future;
    use std::marker::PhantomData;

    pub struct MapStep<F, I, O, Fut, E> {
        function: F,
        _marker: PhantomData<fn() -> (I, O, Fut, E)>,
    }

    impl<F, I, O, Fut, E> MapStep<F, I, O, Fut, E> {
        pub fn new(function: F) -> Self {
            Self {
                function,
                _marker: PhantomData,
            }
        }
    }

    impl<F, I, O, Fut, E> StepType for MapStep<F, I, O, Fut, E>
    where
        F: Fn(I) -> Fut + Send + Sync,
        Fut: Future<Output = std::result::Result<O, E>> + Send,
        E: ToString,
        I: Send,
        O: Send,
    {
        type Input = Vec<I>;
        type Output = Vec<O>;

        async fn execute(
            &self,
            _ctx: &WorkflowContext,
            input: Self::Input,
        ) -> StepResult<Self::Output> {
            try_join_all(input.into_iter().map(&self.function))
                .await
                .map_err(|err| err.to_string())
        }
    }

    #[tokio::test]
    async fn step_lifecycle_success() {
        let map = MapStep::<_, i32, i32, _, String>::new(|value: i32| async move {
            Ok::<_, String>(value * 2)
        });
        let mut step = StepInstance::new("double");
        let ctx = WorkflowContext::default();

        assert_eq!(step.status(), StepStatus::Pending);
        let output = run_step(&mut step, &map, &ctx, vec![1, 2, 3])
            .await
            .unwrap();

        assert_eq!(output, vec![2, 4, 6]);
        assert_eq!(step.status(), StepStatus::Succeeded);
        assert_eq!(step.attempts(), 1);

        let err = step.transition(StepStatus::Running).unwrap_err();
        assert!(err.contains("invalid transition"));
    }

    #[tokio::test]
    async fn step_lifecycle_failure() {
        let map =
            MapStep::<_, i32, i32, _, String>::new(
                |_: i32| async move { Err::<i32, _>("boom".into()) },
            );
        let mut step = StepInstance::new("failing");
        let ctx = WorkflowContext::default();

        let err = run_step(&mut step, &map, &ctx, vec![1]).await.unwrap_err();
        assert_eq!(err, "boom");
        assert_eq!(step.status(), StepStatus::Failed);
        assert_eq!(step.attempts(), 1);

        assert!(step.transition(StepStatus::Retrying).is_ok());
    }
}

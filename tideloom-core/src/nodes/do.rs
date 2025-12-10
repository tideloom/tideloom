use serverless_workflow_core::models::task::{DoTaskDefinition, TaskDefinition};

use crate::runtime::{StepResult, Task, TaskCtx, TaskInput, TaskOutput, executor::TaskExecutor};

#[derive(Debug, Clone)]
pub struct DoNode {
    tasks: Vec<TaskDefinition>,
}

impl DoNode {
    pub fn try_from(def: &DoTaskDefinition) -> StepResult<Self> {
        let mut tasks = Vec::new();
        for entry in &def.do_.entries {
            for (_name, task) in entry {
                tasks.push(task.clone());
            }
        }
        Ok(Self { tasks })
    }
}

#[async_trait::async_trait]
impl Task for DoNode {
    async fn execute(&self, ctx: TaskCtx, input: TaskInput) -> StepResult<TaskOutput> {
        let mut current = input;
        for task in &self.tasks {
            let output = TaskExecutor::execute(task, &ctx, current).await?;
            current = output.into();
        }
        Ok(current.into())
    }

    fn name(&self) -> &'static str {
        "do"
    }
}

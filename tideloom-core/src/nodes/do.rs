use serde_json::Value;
use serverless_workflow_core::models::task::{DoTaskDefinition, TaskDefinition};

use crate::runtime::{StepResult, Task, TaskCtx, executor::TaskExecutor};

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
    async fn execute(&self, ctx: TaskCtx, mut input: Value) -> StepResult<Value> {
        for task in &self.tasks {
            input = TaskExecutor::execute(task, &ctx, input).await?;
        }
        Ok(input)
    }

    fn name(&self) -> &'static str {
        "do"
    }
}

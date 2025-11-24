use anyhow::bail;
use serde_json::Value;
use serverless_workflow_core::models::task::CallTaskDefinition;

use crate::runtime::{StepResult, Task, TaskCtx};

#[derive(Debug, Clone)]
pub struct CallNode {
    def: CallTaskDefinition,
}

impl CallNode {
    pub fn try_from(def: &CallTaskDefinition) -> StepResult<Self> {
        Ok(Self { def: def.clone() })
    }
}

#[async_trait::async_trait]
impl Task for CallNode {
    async fn execute(&self, ctx: TaskCtx, input: Value) -> StepResult<Value> {
        match self.def.call.to_lowercase().as_str() {
            "http" | "asyncapi" => {
                let http = crate::nodes::asyncapi::HTTPNode::try_from(&self.def)?;
                http.execute(ctx, input).await
            }
            other => bail!("unsupported call type: {}", other),
        }
    }

    fn name(&self) -> &'static str {
        "call"
    }
}

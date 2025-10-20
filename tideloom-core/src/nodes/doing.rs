use std::collections::HashMap;
use serde_json::{Map, Value};
use serverless_workflow_core::models::task::{CallTaskDefinition, DoTaskDefinition, TaskDefinition};
use crate::runtime::{StepResult, Task, WorkflowContext};

#[derive(Debug, Clone)]
pub struct DoNode {
    pub tasks: HashMap<String, dyn Task<Input=Value, Output=Value>>,
}

impl DoNode {
    pub fn try_from_definition(call: &DoTaskDefinition) -> StepResult<Self> {

    }

    fn build_request(&self, _input: &Value) -> reqwest::Request {


        reqwest::Request::new(
            self.method.clone(),
            self.endpoint.clone(),
        )
    }
}

impl TryFrom<&TaskDefinition> for crate::nodes::asyncapi::HTTPNode {
    type Error = String;

    fn try_from(task: &TaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_task(task)
    }
}

impl TryFrom<&CallTaskDefinition> for crate::nodes::asyncapi::HTTPNode {
    type Error = String;

    fn try_from(call: &CallTaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_http(call)
    }
}

#[async_trait::async_trait]
impl Task for crate::nodes::asyncapi::HTTPNode {
    type Input = Value;
    type Output = Value;

    async fn execute(&self, ctx: &WorkflowContext, input: Self::Input) -> StepResult<Self::Output> {
        let req = self.build_request(&input);
        ctx.http_client.execute(req).await.unwrap();

        // TODO: fix me
        Ok(Value::Null)
    }
}

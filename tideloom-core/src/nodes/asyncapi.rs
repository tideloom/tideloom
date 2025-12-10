use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use serverless_workflow_core::models::authentication::AuthenticationPolicyDefinition;
use serverless_workflow_core::models::task::{CallTaskDefinition, TaskDefinition};
use std::str::FromStr;

use crate::runtime::{StepResult, Task, TaskCtx, TaskInput, TaskOutput};

#[derive(Debug, Clone, Deserialize)]
pub struct AsyncApiDocument {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub content: Option<Value>,
}

impl AsyncApiDocument {
    #[allow(dead_code)]
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(uri) = &self.uri {
            map.insert("uri".into(), Value::String(uri.clone()));
        }
        if let Some(content) = &self.content {
            map.insert("content".into(), content.clone());
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageConfig {
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AsyncApiConfig {
    pub document: AsyncApiDocument,
    #[serde(rename = "operationRef")]
    pub operation_ref: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub message: Option<MessageConfig>,
    #[serde(default)]
    pub authentication: AuthenticationPolicyDefinition,
}

#[derive(Debug, Clone)]
pub struct HTTPNode {
    endpoint: reqwest::Url,
    method: reqwest::Method,
}

impl HTTPNode {
    pub fn try_from_task(task: &TaskDefinition) -> StepResult<Self> {
        match task {
            TaskDefinition::Call(call) => match call.call.to_lowercase().as_str() {
                "asyncapi" => Self::try_from_http(call),
                "http" => Self::try_from_http(call),
                _ => bail!("expected call 'asyncapi', got '{}'", call.call),
            },
            _ => bail!("AsyncApiNode expects a `call` task definition"),
        }
    }

    pub fn try_from_http(call: &CallTaskDefinition) -> StepResult<Self> {
        let with = call
            .with
            .as_ref()
            .context("asyncapi call requires a `with` block")?;

        let mut with_map = Map::new();
        for (key, value) in with {
            with_map.insert(key.clone(), value.clone());
        }

        let endpoint_url = with_map
            .get("endpoint")
            .and_then(Value::as_str)
            .context("missing or invalid 'endpoint' in asyncapi call")?;
        let method = with_map
            .get("method")
            .and_then(Value::as_str)
            .context("missing or invalid 'method' in asyncapi call")?;

        let config: HTTPNode = HTTPNode {
            endpoint: reqwest::Url::parse(endpoint_url).context("invalid endpoint URL")?,
            method: reqwest::Method::from_str(&method.to_uppercase())
                .context("invalid http method")?,
        };

        Ok(config)
    }

    fn build_request(&self, _input: &Value) -> StepResult<reqwest::Request> {
        // TODO: add body/headers/auth/input templating
        Ok(reqwest::Request::new(
            self.method.clone(),
            self.endpoint.clone(),
        ))
    }
}

impl TryFrom<&TaskDefinition> for HTTPNode {
    type Error = anyhow::Error;

    fn try_from(task: &TaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_task(task)
    }
}

impl TryFrom<&CallTaskDefinition> for HTTPNode {
    type Error = anyhow::Error;

    fn try_from(call: &CallTaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_http(call)
    }
}

#[async_trait::async_trait]
impl Task for HTTPNode {
    async fn execute(&self, ctx: TaskCtx, input: TaskInput) -> StepResult<TaskOutput> {
        let req = self.build_request(&input.data)?;
        ctx.http_client.execute(req).await?;

        // TODO: fix me
        Ok(TaskOutput::new(Value::Null))
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use serverless_workflow_core::models::workflow::WorkflowDefinition;

    use super::*;

    fn load_first_task(yaml: &str) -> TaskDefinition {
        let workflow: WorkflowDefinition = serde_yaml::from_str(yaml).expect("invalid yaml");
        workflow
            .do_
            .entries
            .first()
            .and_then(|entry| entry.iter().next())
            .map(|(_, task)| task.clone())
            .expect("missing task")
    }

    #[test]
    fn http_node_from_task() {
        let yaml = r#"
document:
  dsl: '1.0.1'
  namespace: test
  name: http-example
  version: '0.1.0'
do:
 - test:
     call: http
     with:
        method: get
        endpoint: https://httpbin.org/get

 "#;

        let task = load_first_task(yaml);
        let step = HTTPNode::try_from_task(&task).expect("asyncapi node");
        let input = json!({});

        let request = step.build_request(&input).expect("request should build");

        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().as_str(), "https://httpbin.org/get");
    }
}

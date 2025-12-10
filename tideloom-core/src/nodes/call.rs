use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};
use serverless_workflow_core::models::{resource::EndpointDefinition, task::CallTaskDefinition};
use std::str::FromStr;

use crate::runtime::{StepResult, Task, TaskCtx, TaskInput, TaskOutput};

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
    async fn execute(&self, ctx: TaskCtx, input: TaskInput) -> StepResult<TaskOutput> {
        match self.def.call.to_lowercase().as_str() {
            "http" | "openapi" => {
                let _service = HttpService::try_from(&self.def)?;
                // TODO: 实现 HTTP 调用
                todo!("implement http/openapi call")
            }
            "asyncapi" => {
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

#[derive(Debug, Clone, PartialEq)]
pub struct HttpService {
    pub method: reqwest::Method,
    pub url: reqwest::Url,
    pub headers: HeaderMap,
    pub body: Option<Value>,
}

impl HttpService {
    fn parse_endpoint(value: &Value) -> StepResult<reqwest::Url> {
        if let Some(uri) = value.as_str() {
            return reqwest::Url::parse(uri).context("invalid endpoint url");
        }

        let endpoint: EndpointDefinition = serde_json::from_value(value.clone())
            .context("invalid endpoint object (expected uri or { uri, ... })")?;
        reqwest::Url::parse(&endpoint.uri).context("invalid endpoint url")
    }

    fn try_from_http(def: &CallTaskDefinition) -> StepResult<Self> {
        let with = def
            .with
            .as_ref()
            .context("http call requires a `with` block")?;

        let method = with
            .get("method")
            .and_then(Value::as_str)
            .context("missing or invalid 'method' in http call")?;

        let endpoint_value = with
            .get("endpoint")
            .context("missing 'endpoint' in http call")?;

        let mut url = Self::parse_endpoint(endpoint_value)?;
        if let Some(query) = with.get("query").and_then(Value::as_object) {
            append_query(&mut url, query);
        }

        Ok(Self {
            method: reqwest::Method::from_str(&method.to_uppercase())
                .context("invalid http method")?,
            url,
            headers: headers_from_json(with.get("headers").and_then(Value::as_object))?,
            body: with.get("body").cloned()
        })
    }

    fn try_from_openapi(def: &CallTaskDefinition) -> StepResult<Self> {
        let with = def
            .with
            .as_ref()
            .context("openapi call requires a `with` block")?;

        let _document_value = with
            .get("document")
            .context("missing 'document' in openapi call")?;

        let operation_id = with
            .get("operationId")
            .and_then(Value::as_str)
            .context("missing or invalid 'operationId' in openapi call")?;

        bail!(
            "openapi call is not implemented yet (operationId='{}')",
            operation_id
        )
    }
}

impl TryFrom<&CallTaskDefinition> for HttpService {
    type Error = anyhow::Error;

    fn try_from(def: &CallTaskDefinition) -> StepResult<Self> {
        match def.call.to_lowercase().as_str() {
            "http" => Self::try_from_http(def),
            "openapi" => Self::try_from_openapi(def),
            other => bail!("call '{}' is not an http/openapi function", other),
        }
    }
}


fn headers_from_json(map: Option<&Map<String, Value>>) -> StepResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(map) = map {
        for (name, value) in map {
            let value_str = value
                .as_str()
                .context("header values must be strings")?
                .to_string();
            headers.insert(
                HeaderName::from_str(name)?,
                HeaderValue::from_str(&value_str)?,
            );
        }
    }
    Ok(headers)
}

fn append_query(url: &mut reqwest::Url, params: &Map<String, Value>) {
    let mut pairs = url.query_pairs_mut();
    for (k, v) in params {
        if let Some(s) = v.as_str() {
            pairs.append_pair(k, s);
        } else {
            pairs.append_pair(k, &v.to_string());
        }
    }
}

fn is_status_allowed(status: reqwest::StatusCode, redirect: bool) -> bool {
    if redirect {
        status.is_success() || status.is_redirection()
    } else {
        status.is_success()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serverless_workflow_core::models::workflow::WorkflowDefinition;

    fn load_first_call(yaml: &str) -> CallTaskDefinition {
        let workflow: WorkflowDefinition = serde_yaml::from_str(yaml).expect("invalid yaml");
        workflow
            .do_
            .entries
            .first()
            .and_then(|entry| entry.iter().next())
            .map(|(_, task)| task.clone())
            .and_then(|task| match task {
                serverless_workflow_core::models::task::TaskDefinition::Call(call) => Some(call),
                _ => None,
            })
            .expect("missing call task")
    }

    #[test]
    fn builds_http_from_inline_endpoint() {
        let yaml = r#"
document:
  dsl: '1.0.2'
  namespace: test
  name: http-inline
  version: '0.1.0'
do:
  - getPet:
      call: http
      with:
        method: get
        endpoint: https://petstore.swagger.io/v2/pet/123
        query:
          status: available
        headers:
          x-trace-id: abc123
        body:
          hello: world
        redirect: true
        output: response
"#;

        let call = load_first_call(yaml);
        let func = HttpCallSpec::try_from(&call).expect("should parse http func");

        assert_eq!(func.method, reqwest::Method::GET);
        assert_eq!(
            func.url.as_str(),
            "https://petstore.swagger.io/v2/pet/123?status=available"
        );
        assert!(func.headers.contains_key("x-trace-id"));
        assert!(func.redirect);
        assert_eq!(func.body, Some(json!({"hello": "world"})));
    }

    #[test]
    fn builds_http_from_endpoint_object() {
        let yaml = r#"
document:
  dsl: '1.0.2'
  namespace: test
  name: http-endpoint-object
  version: '0.1.0'
do:
  - createPet:
      call: http
      with:
        method: post
        endpoint:
          uri: https://api.example.com/pet
        body:
          name: fido
"#;

        let call = load_first_call(yaml);
        let func =
            HttpCallSpec::try_from(&call).expect("should parse http func with endpoint object");

        assert_eq!(func.method, reqwest::Method::POST);
        assert_eq!(func.url.as_str(), "https://api.example.com/pet");
        assert!(!func.redirect);
    }

    #[test]
    fn openapi_is_not_implemented() {
        let yaml = r#"
document:
  dsl: '1.0.2'
  namespace: test
  name: openapi-example
  version: '0.1.0'
do:
  - findPet:
      call: openapi
      with:
        document:
          endpoint: https://petstore.swagger.io/v2/swagger.json
        operationId: findPetsByStatus
        parameters:
          status: available
"#;

        let call = load_first_call(yaml);
        let err = HttpCallSpec::try_from(&call).expect_err("openapi not implemented yet");
        assert!(
            err.to_string()
                .contains("openapi call is not implemented yet")
        );
    }

    #[test]
    fn rejects_non_http_call() {
        let call = CallTaskDefinition {
            call: "grpc".into(),
            with: None,
            await_: None,
            common: Default::default(),
        };

        let err = HttpCallSpec::try_from(&call).expect_err("grpc is not http");
        assert!(err.to_string().contains("is not an http/openapi function"));
    }
}

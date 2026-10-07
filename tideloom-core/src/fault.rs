use std::fmt;

use serde_json::Map;
use serde_json::Value;

const COMMUNICATION: &str =
    "https://open-workflow-specification.org/spec/1.0.0/errors/communication";
const EXPRESSION: &str = "https://open-workflow-specification.org/spec/1.0.0/errors/expression";
const RUNTIME: &str = "https://open-workflow-specification.org/spec/1.0.0/errors/runtime";

/// RFC 7807 problem details produced by the walk.
///
/// `instance` is the JSON Pointer of the task that failed. Workflow-level
/// failures use `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    error_type: String,
    status: u16,
    instance: String,
    title: String,
    detail: Option<String>,
}

impl Fault {
    /// Expression failure at `instance`.
    pub fn expression(instance: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(
            EXPRESSION,
            400,
            instance,
            "expression error",
            Some(detail.into()),
        )
    }

    /// Runtime failure at `instance`.
    pub fn runtime(instance: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(RUNTIME, 500, instance, "runtime error", Some(detail.into()))
    }

    /// HTTP or network failure at `instance`.
    ///
    /// `status` is the HTTP status when the server responded, or 500 when the
    /// call did not complete.
    pub fn communication(
        instance: impl Into<String>,
        status: u16,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            COMMUNICATION,
            status,
            instance,
            "communication error",
            Some(detail.into()),
        )
    }

    fn new(
        error_type: &str,
        status: u16,
        instance: impl Into<String>,
        title: &str,
        detail: Option<String>,
    ) -> Self {
        let instance = instance.into();
        Self {
            error_type: error_type.to_string(),
            status,
            instance: if instance.is_empty() {
                "/".to_string()
            } else {
                instance
            },
            title: title.to_string(),
            detail,
        }
    }

    /// Build a fault from a `raise` error object.
    ///
    /// Missing `type`, `status`, and `title` use the runtime defaults. An
    /// explicit `instance` is kept. `detail` and `details` are both accepted.
    pub(crate) fn from_error(position: &str, error: &Value) -> Result<Self, Fault> {
        let Some(object) = error.as_object() else {
            return Err(Self::runtime(
                position,
                "raise.error must be an object or a name",
            ));
        };
        let error_type = match object.get("type") {
            None => RUNTIME.to_string(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => {
                return Err(Self::runtime(position, "error.type must be a string"));
            }
        };
        let status = match object.get("status") {
            None => 500,
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| {
                    Self::runtime(position, "error.status must be an integer status code")
                })?,
            Some(_) => {
                return Err(Self::runtime(position, "error.status must be a number"));
            }
        };
        let title = match object.get("title") {
            None => "workflow error".to_string(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => {
                return Err(Self::runtime(position, "error.title must be a string"));
            }
        };
        let detail = match object.get("detail").or_else(|| object.get("details")) {
            None => None,
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => {
                return Err(Self::runtime(position, "error.detail must be a string"));
            }
        };
        let instance = match object.get("instance") {
            None => position.to_string(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => {
                return Err(Self::runtime(position, "error.instance must be a string"));
            }
        };
        Ok(Self::new(&error_type, status, instance, &title, detail))
    }

    /// Error type URI.
    pub fn error_type(&self) -> &str {
        &self.error_type
    }

    /// HTTP-style status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// JSON Pointer of the failing task, or `/` for the workflow.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Short title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Optional explanation.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// JSON object a `catch` expression sees as `$error` (or the `as` name).
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("type".to_string(), Value::String(self.error_type.clone()));
        object.insert("status".to_string(), Value::from(self.status));
        object.insert("title".to_string(), Value::String(self.title.clone()));
        object.insert("instance".to_string(), Value::String(self.instance.clone()));
        if let Some(detail) = &self.detail {
            object.insert("detail".to_string(), Value::String(detail.clone()));
        }
        Value::Object(object)
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({}) at {}",
            self.title, self.status, self.instance
        )?;
        if let Some(detail) = &self.detail {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

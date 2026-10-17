//! `call: http` as an activity executor.
//!
//! The interpreter stops at the call and leaves the request unevaluated. This
//! executor builds it from the task input, `$context`, and `$workflow.input`,
//! sends it, and returns the raw output, before `output.as`. It is not
//! idempotent: a crash between the journal commits faults the instance instead
//! of sending the request again. A GET that was already sent is still a second
//! request if recovery re-runs it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::ActivityError;
use crate::ActivityExecutor;
use crate::DefinitionSource;
use crate::TaskExecution;
use crate::TaskKind;

mod call;

/// Performs `call: http`.
///
/// The engine routes `TaskKind::Http` here like any other kind. There is no
/// fallback that sends the request when this executor is missing.
pub struct HttpExecutor {
    definitions: Arc<dyn DefinitionSource>,
}

impl HttpExecutor {
    /// Resolve the call node from `definitions`.
    #[must_use]
    pub fn new(definitions: Arc<dyn DefinitionSource>) -> Self {
        Self { definitions }
    }
}

#[async_trait]
impl ActivityExecutor for HttpExecutor {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    fn idempotent(&self) -> bool {
        false
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        let definition = self
            .definitions
            .get(&task.definition)
            .await
            .map_err(|error| ActivityError::Executor(error.to_string()))?;
        let node = definition.get(task.key.position()).ok_or_else(|| {
            ActivityError::Executor(format!(
                "HTTP call {} is not in the definition",
                task.key.position()
            ))
        })?;
        call::call(node, &task.input, &task.workflow_input, &task.context)
            .map_err(ActivityError::Fault)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::call::expand_template;

    #[test]
    fn simple_template_uses_top_level_task_input() {
        let input = json!({"petId": "1", "pet.id": "literal", "pet": {"id": "nested"}});
        let uri = expand_template(
            "http://example.test/pets/{pet.id}?missing={absent}",
            &input,
            "/do/0/call",
        )
        .unwrap();
        assert_eq!(uri, "http://example.test/pets/literal?missing=");
    }

    #[test]
    fn template_encodes_scalars_and_rejects_objects() {
        let uri = expand_template(
            "http://example.test/{n}/{ok}/{q}",
            &json!({"n": 3, "ok": true, "q": "a b&c"}),
            "/p",
        )
        .unwrap();
        assert_eq!(uri, "http://example.test/3/true/a%20b%26c");
        let error = expand_template(
            "http://example.test/{pet}",
            &json!({"pet": {"id": 1}}),
            "/p",
        )
        .unwrap_err();
        assert_eq!(error.status(), 400);
        let error =
            expand_template("http://example.test/{+q}", &json!({"q": "a"}), "/p").unwrap_err();
        assert!(error.detail().unwrap().contains("simple"));
    }
}

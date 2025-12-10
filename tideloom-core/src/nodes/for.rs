use anyhow::{anyhow, bail};
use serde_json::Value;
use serverless_workflow_core::models::task::{ForTaskDefinition, TaskDefinition};

use crate::runtime::{StepResult, Task, TaskCtx, TaskInput, TaskOutput, executor::TaskExecutor};

#[derive(Debug, Clone)]
pub struct ForNode {
    in_expr: String,
    while_expr: Option<String>,
    body: Vec<TaskDefinition>,
}

impl ForNode {
    pub fn try_from(def: &ForTaskDefinition) -> StepResult<Self> {
        let mut body = Vec::new();
        for entry in &def.do_.entries {
            for (_name, task) in entry {
                body.push(task.clone());
            }
        }
        Ok(Self {
            in_expr: def.for_.in_.clone(),
            while_expr: def.while_.clone(),
            body,
        })
    }
}

#[async_trait::async_trait]
impl Task for ForNode {
    async fn execute(&self, ctx: TaskCtx, input: TaskInput) -> StepResult<TaskOutput> {
        if self.while_expr.is_some() {
            bail!("'for.while' is not supported yet");
        }

        let items = resolve_iterable(&self.in_expr, &input.data)?;
        let mut results = Vec::with_capacity(items.len());

        for item in items {
            let mut current = TaskInput::new(item);
            for task in &self.body {
                let output = TaskExecutor::execute(task, &ctx, current).await?;
                current = output.into();
            }
            results.push(current.data);
        }

        Ok(TaskOutput::new(Value::Array(results)))
    }

    fn name(&self) -> &'static str {
        "for"
    }
}

fn resolve_iterable(expr: &str, input: &Value) -> StepResult<Vec<Value>> {
    let target = if expr.trim().is_empty() || expr == "$" || expr == "." {
        input
    } else {
        let mut cursor = input;
        let mut path = expr.trim_start_matches('$');
        path = path.trim_start_matches('.');
        if path.is_empty() {
            input
        } else {
            for segment in path.split('.') {
                if segment.is_empty() {
                    continue;
                }
                cursor = cursor
                    .get(segment)
                    .ok_or_else(|| anyhow!("for.in path '{}' not found in input", expr))?;
            }
            cursor
        }
    };

    match target {
        Value::Array(items) => Ok(items.clone()),
        other => bail!(
            "for.in expression '{}' did not resolve to an array (got {})",
            expr,
            other.to_string()
        ),
    }
}

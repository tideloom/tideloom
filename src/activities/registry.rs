use std::sync::Arc;

use serde_json::Value as JsonValue;

use super::executor::{EffectContext, EffectExecutor};
use crate::errors::WorkflowError;

pub struct EffectRegistry {
    executors: Vec<Arc<dyn EffectExecutor>>, // pluggable executors
}

impl EffectRegistry {
    pub fn new() -> Self { Self { executors: Vec::new() } }

    pub fn with_executor(mut self, exec: Arc<dyn EffectExecutor>) -> Self {
        self.executors.push(exec);
        self
    }

    pub fn register(&mut self, exec: Arc<dyn EffectExecutor>) { self.executors.push(exec); }

    pub async fn execute(&self, ctx: &EffectContext) -> Result<JsonValue, WorkflowError> {
        let kind = &ctx.kind;
        for e in &self.executors {
            if e.can_execute(kind) {
                return e.execute(ctx).await;
            }
        }
        Err(WorkflowError::Task { message: format!("no executor for {:?}", kind) })
    }
}

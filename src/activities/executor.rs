use async_trait::async_trait;
use serde_json::Value as JsonValue;

use crate::errors::WorkflowError;
use crate::nodes::{id::NodeId, kind::EffectKind};

#[derive(Debug, Clone)]
pub struct EffectContext {
    pub id: NodeId,
    pub name: String,
    pub kind: EffectKind,
}

#[async_trait]
pub trait EffectExecutor: Send + Sync {
    fn can_execute(&self, kind: &EffectKind) -> bool;
    async fn execute(&self, ctx: &EffectContext) -> Result<JsonValue, WorkflowError>;
}

pub struct SimpleRunExecutor;

#[async_trait]
impl EffectExecutor for SimpleRunExecutor {
    fn can_execute(&self, kind: &EffectKind) -> bool { matches!(kind, EffectKind::Run) }
    async fn execute(&self, ctx: &EffectContext) -> Result<JsonValue, WorkflowError> {
        Ok(serde_json::json!({ "task": ctx.name, "result": "ok" }))
    }
}


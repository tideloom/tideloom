use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backoff {
    pub attempt: u32,
    pub delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OutboxItemKind {
    Wait { until: OffsetDateTime },
    Retry { backoff: Backoff },
    Schedule { cron: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxItem {
    pub kind: OutboxItemKind,
    pub metadata: JsonValue,
}

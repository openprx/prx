//! Exact, ACL-scoped lookup for transcript events referenced by a context handoff note.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{Memory, MemoryPrincipal};
use async_trait::async_trait;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) const MAX_REFERENCED_EVENTS: usize = 500;
const PAGE_SIZE: usize = 500;
pub(crate) const MAX_ROW_SPAN: i64 = 20_000;
pub const TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME: &str = "transcript_history_lookup";

pub struct TranscriptHistoryLookupTool {
    memory: Arc<dyn Memory>,
}

impl TranscriptHistoryLookupTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }

    fn trusted_principal(args: &serde_json::Value) -> anyhow::Result<MemoryPrincipal> {
        anyhow::ensure!(
            args.get("_zc_scope_trusted").and_then(serde_json::Value::as_bool) == Some(true),
            "transcript lookup requires a runtime-authenticated scope"
        );
        let scope = args
            .get("_zc_scope")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("transcript lookup requires a runtime scope"))?;
        let required = |name: &str| {
            scope
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("runtime scope is missing {name}"))
        };
        Ok(MemoryPrincipal {
            workspace_id: required("workspace_id")?,
            session_key: Some(required("session_key")?),
            channel: scope
                .get("channel")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            sender: scope
                .get("sender")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            owner_id: scope
                .get("owner_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            ..MemoryPrincipal::default()
        })
    }
}

#[async_trait]
impl Tool for TranscriptHistoryLookupTool {
    fn name(&self) -> &str {
        TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Recover the exact transcript events referenced by a context_handoff note. Reads only the current authenticated workspace/session and returns events in the note's event_ids order."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "event_ids": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_REFERENCED_EVENTS,
                    "items": { "type": "string" },
                    "description": "Exact source_event_ids from the context_handoff note."
                },
                "first_row_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "source_event_range.first_row_id from the handoff note."
                },
                "last_row_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "source_event_range.last_row_id from the handoff note."
                }
            },
            "required": ["event_ids", "first_row_id", "last_row_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = Self::trusted_principal(&args)?;
        let session_key = principal.session_key.clone().unwrap_or_default();
        let ids = args
            .get("event_ids")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("missing event_ids"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("event_ids must contain non-empty strings"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!ids.is_empty(), "event_ids cannot be empty");
        anyhow::ensure!(ids.len() <= MAX_REFERENCED_EVENTS, "too many referenced events");
        let unique = ids.iter().collect::<HashSet<_>>();
        anyhow::ensure!(unique.len() == ids.len(), "event_ids must be unique");

        let first_row_id = args
            .get("first_row_id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("missing first_row_id"))?;
        let last_row_id = args
            .get("last_row_id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("missing last_row_id"))?;
        anyhow::ensure!(
            first_row_id > 0 && last_row_id >= first_row_id,
            "invalid transcript row range"
        );
        anyhow::ensure!(
            last_row_id.saturating_sub(first_row_id) <= MAX_ROW_SPAN,
            "transcript row range is too large"
        );

        let wanted = ids.iter().cloned().collect::<HashSet<_>>();
        let mut found = HashMap::new();
        let mut cursor = first_row_id.saturating_sub(1);
        while cursor < last_row_id && found.len() < wanted.len() {
            let page = self
                .memory
                .list_message_events_since(&principal, cursor, PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            let previous = cursor;
            for event in page {
                cursor = cursor.max(event.id);
                if event.id > last_row_id {
                    continue;
                }
                if event.session_key.as_deref() == Some(session_key.as_str()) && wanted.contains(&event.event_id) {
                    found.insert(event.event_id.clone(), event);
                }
            }
            if cursor <= previous {
                break;
            }
        }

        let missing = ids
            .iter()
            .filter(|id| !found.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Referenced transcript is incomplete or outside the authenticated session; missing event IDs: {}",
                    missing.join(", ")
                )),
            });
        }

        let events = ids
            .iter()
            .filter_map(|id| found.remove(id))
            .map(|event| {
                json!({
                    "event_id": event.event_id,
                    "row_id": event.id,
                    "role": event.role,
                    "content": event.content,
                    "created_at": event.created_at
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "kind": "transcript_history",
                "events": events
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryFabric, SqliteMemory};
    use crate::runtime::envelope::RuntimeEnvelope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn lookup_returns_exact_referenced_events_in_requested_order() {
        let temp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-a");
        let envelope = RuntimeEnvelope::agent("workspace-a", "run-a");
        let first = fabric
            .record_inbound_user_message(
                envelope.message_scope(),
                "exact user text",
                Some("source-a".into()),
                None,
            )
            .await
            .unwrap();
        let second = fabric
            .record_assistant_message(envelope.message_scope(), "exact assistant text")
            .await
            .unwrap();
        let tool = TranscriptHistoryLookupTool::new(memory);
        let result = tool
            .execute(json!({
                "event_ids": [second.event_id, first.event_id],
                "first_row_id": first.id,
                "last_row_id": second.id,
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "workspace_id": envelope.workspace_id,
                    "session_key": envelope.session_key,
                    "channel": envelope.channel,
                    "sender": envelope.sender,
                    "owner_id": envelope.resolved_owner_id()
                }
            }))
            .await
            .unwrap();

        assert!(result.success, "lookup failed: {:?}", result.error);
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let events = output.get("events").and_then(serde_json::Value::as_array).unwrap();
        assert_eq!(
            events
                .first()
                .and_then(|event| event.get("content"))
                .and_then(serde_json::Value::as_str),
            Some("exact assistant text")
        );
        assert_eq!(
            events
                .get(1)
                .and_then(|event| event.get("content"))
                .and_then(serde_json::Value::as_str),
            Some("exact user text")
        );
    }

    #[tokio::test]
    async fn lookup_rejects_references_from_another_authenticated_session() {
        let temp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-a");
        let current = RuntimeEnvelope::agent("workspace-a", "run-current");
        let other = RuntimeEnvelope::agent("workspace-a", "run-other");
        let event = fabric
            .record_inbound_user_message(
                other.message_scope(),
                "private other session",
                Some("other-source".into()),
                None,
            )
            .await
            .unwrap();
        let tool = TranscriptHistoryLookupTool::new(memory);
        let result = tool
            .execute(json!({
                "event_ids": [event.event_id],
                "first_row_id": event.id,
                "last_row_id": event.id,
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "workspace_id": current.workspace_id,
                    "session_key": current.session_key,
                    "channel": current.channel,
                    "sender": current.sender,
                    "owner_id": current.resolved_owner_id()
                }
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("outside the authenticated session"));
    }
}

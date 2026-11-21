//! Exact, ACL-scoped lookup of the transcript events a context handoff moved
//! out of the window, by the handoff's own id.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::compaction::{POOL_MAX_ROWS, POOL_PAGE_ROWS, ValidatedContextHandoff, validated_context_handoff};
use crate::memory::{Memory, MemoryPrincipal, SessionContextQuery};
use async_trait::async_trait;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) const MAX_REFERENCED_EVENTS: usize = 500;
const PAGE_SIZE: usize = 500;
pub(crate) const MAX_ROW_SPAN: i64 = 20_000;
pub const TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME: &str = "transcript_history_lookup";
/// Event type of the row a context switch stores its handoff note in.
const CONTEXT_SWITCH_EVENT_TYPE: &str = "context.switch.created";

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
            // MA-01h: the session's rows of the caller's own agent partition only.
            agent_partition: crate::tools::execution::trusted_scope_read_partition(args)?,
            ..MemoryPrincipal::default()
        })
    }
}

/// The refusal of a lookup whose events (or whose handoff) are not rows of
/// the authenticated session.
fn not_found(missing: &[&str]) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(format!(
            "Referenced transcript is incomplete or outside the authenticated session; missing event IDs: {}",
            missing.join(", ")
        )),
    }
}

/// The handoff whose stored note carries `handoff_event_id`, among the
/// session's own `context.switch.created` rows, read newest to oldest within
/// [`POOL_MAX_ROWS`] rows. A branch copy's rows carry new ids and rewritten
/// notes, so a branch resolves the id its own note carries. `None` when no
/// such row lies within the bound or a page cannot be read.
async fn session_handoff(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
    handoff_event_id: &str,
) -> Option<ValidatedContextHandoff> {
    let query = || SessionContextQuery {
        principal: principal.clone(),
        since_event_id: None,
        limit: POOL_PAGE_ROWS,
        include_roles: vec!["assistant".to_string()],
    };
    let mut before_id = i64::MAX;
    let mut rows_read = 0usize;
    while rows_read < POOL_MAX_ROWS {
        let page = match memory.load_session_context_before(query(), before_id).await {
            Ok(page) => page,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(
                    error_len,
                    error_sha256,
                    "transcript lookup could not read the session's handoffs"
                );
                return None;
            }
        };
        let page_len = page.len();
        rows_read = rows_read.saturating_add(page_len);
        let found = page
            .iter()
            .filter(|event| {
                event.event_type == CONTEXT_SWITCH_EVENT_TYPE && event.session_key.as_deref() == Some(session_key)
            })
            .filter_map(|event| validated_context_handoff(&event.content))
            .find(|handoff| handoff.handoff_event_id == handoff_event_id);
        if found.is_some() {
            return found;
        }
        let oldest = page.iter().map(|event| event.id).min();
        match oldest {
            Some(oldest) if page_len >= POOL_PAGE_ROWS => before_id = oldest,
            _ => return None,
        }
    }
    None
}

#[async_trait]
impl Tool for TranscriptHistoryLookupTool {
    fn name(&self) -> &str {
        TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Recover the earlier turns of this session that a context handoff moved out of the window. Pass the handoff_event_id from the context_handoff message; the result lists the moved-out messages exactly, in order. If the result contains another context_handoff, look that one up too. Reads only the current authenticated session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "handoff_event_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The handoff_event_id of the context_handoff message to recover."
                }
            },
            "required": ["handoff_event_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let memory_view = crate::memory::active_view::select(&self.memory);
        let principal = Self::trusted_principal(&args)?;
        let session_key = principal.session_key.clone().unwrap_or_default();
        let handoff_event_id = args
            .get("handoff_event_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("handoff_event_id must be a non-empty string"))?;
        let Some(handoff) = session_handoff(memory_view.as_ref(), &principal, &session_key, handoff_event_id).await
        else {
            return Ok(not_found(&[handoff_event_id]));
        };
        let ValidatedContextHandoff {
            event_ids: ids,
            first_row_id,
            last_row_id,
            ..
        } = handoff;
        anyhow::ensure!(ids.len() <= MAX_REFERENCED_EVENTS, "too many referenced events");
        let unique = ids.iter().collect::<HashSet<_>>();
        anyhow::ensure!(unique.len() == ids.len(), "event_ids must be unique");
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
            let page = memory_view
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
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Ok(not_found(&missing));
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
    use crate::agent::document_ingest::DocumentIngestRuntime;
    use crate::memory::{MemoryFabric, SqliteMemory};
    use crate::providers::ChatMessage;
    use crate::runtime::envelope::RuntimeEnvelope;
    use tempfile::TempDir;

    /// Upper bound for one test, its real SQLite work included.
    const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

    /// The handoff builder needs a provider; switch mode never calls it.
    struct UnusedProvider;

    #[async_trait]
    impl crate::providers::Provider for UnusedProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }
    }

    /// A session of `envelope` whose first three of four stored messages a
    /// real context switch moved out: the switch row's handoff id and the
    /// moved-out texts, in history order.
    async fn switched_session(
        memory: &Arc<dyn Memory>,
        envelope: &RuntimeEnvelope,
        label: &str,
    ) -> (String, Vec<String>) {
        let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone());
        let mut history = vec![ChatMessage::system("sys")];
        for index in 0..4 {
            let text = format!("{label} message {index} {}", "padding ".repeat(40));
            if index % 2 == 0 {
                fabric
                    .record_inbound_user_message(
                        envelope.message_scope(),
                        text.clone(),
                        Some(format!("{label}-source-{index}")),
                        None,
                    )
                    .await
                    .expect("test: record user");
                history.push(ChatMessage::user(text));
            } else {
                fabric
                    .record_assistant_message(envelope.message_scope(), text.clone())
                    .await
                    .expect("test: record assistant");
                history.push(ChatMessage::assistant(text));
            }
        }
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
            os_paging: crate::config::OsPagingConfig::default(),
        };
        let audit = DocumentIngestRuntime::from_envelope(Arc::clone(memory), envelope);
        let patch = crate::agent::compaction::build_configurable_compaction_patch(
            &history,
            &UnusedProvider,
            "model",
            &config,
            Some(&audit),
            "test_switch",
        )
        .await
        .expect("test: build the switch")
        .expect("test: a switch patch");
        let handoff = patch.replacement.first().expect("test: the handoff message");
        let note = validated_context_handoff(&handoff.content).expect("test: the stored note validates");
        let folded = history
            .iter()
            .skip(1)
            .take(3)
            .map(|message| message.content.clone())
            .collect();
        (note.handoff_event_id, folded)
    }

    async fn lookup(memory: &Arc<dyn Memory>, scope: &RuntimeEnvelope, handoff_event_id: &str) -> ToolResult {
        TranscriptHistoryLookupTool::new(Arc::clone(memory))
            .execute(json!({
                "handoff_event_id": handoff_event_id,
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "workspace_id": scope.workspace_id,
                    "session_key": scope.session_key,
                    "channel": scope.channel,
                    "sender": scope.sender,
                    "owner_id": scope.resolved_owner_id()
                }
            }))
            .await
            .expect("test: the lookup runs")
    }

    fn event_contents(result: &ToolResult) -> Vec<String> {
        assert!(result.success, "lookup failed: {:?}", result.error);
        let output: serde_json::Value = serde_json::from_str(&result.output).expect("test: lookup output JSON");
        output
            .get("events")
            .and_then(serde_json::Value::as_array)
            .expect("test: lookup events")
            .iter()
            .filter_map(|event| event.get("content").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn lookup_by_handoff_event_id_returns_the_folded_events() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let temp = TempDir::new().expect("test: tempdir");
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
            let envelope = RuntimeEnvelope::agent("workspace-a", "run-a");
            let (handoff_event_id, folded) = switched_session(&memory, &envelope, "own").await;

            let result = lookup(&memory, &envelope, &handoff_event_id).await;

            assert_eq!(event_contents(&result), folded, "the folded events, in history order");
        })
        .await
        .expect("test: within TEST_DEADLINE");
    }

    #[tokio::test]
    async fn lookup_refuses_a_handoff_of_another_session() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let temp = TempDir::new().expect("test: tempdir");
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
            let current = RuntimeEnvelope::agent("workspace-a", "run-current");
            let other = RuntimeEnvelope::agent("workspace-a", "run-other");
            let (other_handoff, other_folded) = switched_session(&memory, &other, "private").await;
            assert_eq!(
                event_contents(&lookup(&memory, &other, &other_handoff).await),
                other_folded,
                "the owning session resolves its handoff"
            );

            let result = lookup(&memory, &current, &other_handoff).await;

            assert!(!result.success, "another session's handoff is refused");
            let error = result.error.unwrap_or_default();
            assert!(error.contains("outside the authenticated session"), "{error}");
            assert!(
                error.contains(&other_handoff),
                "the refusal names the handoff id: {error}"
            );
            assert!(result.output.is_empty(), "no event of the other session is returned");
        })
        .await
        .expect("test: within TEST_DEADLINE");
    }
}

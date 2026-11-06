use crate::agent::document_ingest::drop_untrusted_external_results;
use crate::agent::response_guard::strip_quoted_fabricated_tool_markers;
use crate::memory;
use crate::memory::principal::{MemoryWriteContext, OwnerPrincipal, Role};
use crate::memory::traits::MemoryEntryOrigin;
use crate::memory::{
    ConversationTurn, DocumentSearchResult, Memory, MemoryPrincipal, MessageEvent, RetrievalTraceInput,
    RetrievedContextItem, SessionContextQuery, SharedContextQuery,
};
use std::borrow::Cow;
use std::collections::BTreeSet;

/// Maximum bytes for the total memory context preamble injected into prompts.
/// 🟡 Behavior-limits Phase 1: raised 4096 -> 24576 (6x).
const MAX_MEMORY_PREAMBLE_BYTES: usize = 24_576;

/// Maximum bytes for a single memory entry within the preamble.
/// 🟡 Behavior-limits Phase 1: raised 512 -> 3072 (6x).
const MAX_MEMORY_ENTRY_BYTES: usize = 3_072;

/// Maximum bytes for recent shared message events injected into prompts.
/// 🟡 Behavior-limits Phase 1: raised 2048 -> 12288 (6x).
const MAX_SHARED_EVENTS_PREAMBLE_BYTES: usize = 12_288;

/// Maximum bytes for document evidence injected into prompts.
/// 🟡 Behavior-limits Phase 1: raised 2048 -> 12288 (6x).
const MAX_DOCUMENT_CONTEXT_PREAMBLE_BYTES: usize = 12_288;

pub(crate) struct RecalledMemoryContext {
    pub(crate) preamble: String,
    pub(crate) ids: Vec<String>,
    pub(crate) semantic_range: std::ops::Range<usize>,
    pub(crate) summaries: Vec<String>,
}

/// Read scope authored by the runtime, independent of decision admission.
pub(crate) struct RecallScope {
    principal: MemoryPrincipal,
    semantic: MemoryWriteContext,
    min_relevance_score: f64,
    record_user_messages: bool,
    current_input_key: Option<String>,
    current_input_event: Option<String>,
}

impl RecallScope {
    pub(crate) fn with_current_input_key(mut self, key: Option<String>) -> Self {
        if key.is_some() {
            self.current_input_key = key;
        }
        self
    }

    pub(crate) fn from_envelope(
        envelope: &crate::runtime::envelope::RuntimeEnvelope,
        config: &crate::config::MemoryConfig,
        chat_type: &str,
    ) -> Self {
        let principal = envelope.memory_principal();
        let mut semantic = envelope.memory_write_context(chat_type);
        // Write projections deliberately omit read isolation. Re-author it
        // from the authenticated principal before issuing a semantic read.
        semantic.owner_id = principal.owner_id.clone();
        semantic.require_owner_match = principal.require_owner_match;
        Self {
            principal,
            semantic,
            min_relevance_score: config.min_relevance_score,
            record_user_messages: config.events.record_user_messages,
            current_input_key: envelope.run_id.as_deref().map(current_input_memory_key),
            current_input_event: envelope.source_message_event_id.clone(),
        }
    }
}

pub(crate) const RECALL_CONTEXT_BUDGET: usize = MAX_MEMORY_PREAMBLE_BYTES;

/// Recall authenticated context without consulting a decision's retrieved set.
/// The budget is in bytes and is clamped to the existing context ceiling.
pub(crate) async fn scoped_recall(
    memory: &dyn Memory,
    scope: &RecallScope,
    query: &str,
    budget: usize,
) -> RecalledMemoryContext {
    // Historical user text must not be reintroduced when recording is off.
    if budget == 0 {
        return RecalledMemoryContext {
            preamble: String::new(),
            ids: Vec::new(),
            semantic_range: 0..0,
            summaries: Vec::new(),
        };
    }
    // Retrieval uses the projected, bounded query; exclusions compare against
    // the untransformed live input.
    let live_input = query;
    let query = recall_query(live_input);
    let bundle = build_agent_context(
        memory,
        scope.principal.clone(),
        &query,
        live_input,
        scope.min_relevance_score,
        Some(&scope.semantic),
        Some(scope.record_user_messages),
        (scope.current_input_key.as_deref(), scope.current_input_event.as_deref()),
    )
    .await;
    let semantic_start = bundle.current_session_context.len() + bundle.recent_shared_context.len();
    let semantic_end = semantic_start + bundle.semantic_memory_context.len();
    let mut preamble = bundle.preamble();
    let budget = budget.min(RECALL_CONTEXT_BUDGET);
    let mut ids = bundle.recalled_memory_ids;
    if preamble.len() > budget {
        preamble.truncate(preamble.floor_char_boundary(budget));
        // A clipped bundle cannot reliably attribute every recalled id.
        ids.clear();
    }
    let semantic_range = semantic_start.min(preamble.len())..semantic_end.min(preamble.len());
    RecalledMemoryContext {
        preamble,
        summaries: if ids.is_empty() {
            Vec::new()
        } else {
            bundle.semantic_summaries
        },
        ids,
        semantic_range,
    }
}

pub(crate) struct AgentContextBundle {
    pub(crate) current_session_context: String,
    pub(crate) recent_shared_context: String,
    pub(crate) semantic_memory_context: String,
    pub(crate) document_context: String,
    pub(crate) recalled_memory_ids: Vec<String>,
    semantic_summaries: Vec<String>,
}

impl AgentContextBundle {
    pub(crate) fn preamble(&self) -> String {
        format!(
            "{}{}{}{}",
            self.current_session_context,
            self.recent_shared_context,
            self.semantic_memory_context,
            self.document_context
        )
    }
}

/// Shared identity for the live input across CLI, chat and embedded turns.
pub(crate) fn current_input_memory_key(run_id: &str) -> String {
    format!("user_msg:{run_id}")
}

/// Build context preamble by searching memory for relevant entries.
/// Entries with a hybrid score below `min_relevance_score` are dropped to
/// prevent unrelated memories from bleeding into the conversation.
pub(crate) async fn build_context_with_scope(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    scope_ctx: Option<&MemoryWriteContext>,
) -> RecalledMemoryContext {
    build_semantic_context(mem, user_msg, min_relevance_score, scope_ctx, false, None, None).await
}

fn fresh_timestamp(timestamp: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .and_then(|created| u64::try_from(chrono::Utc::now().signed_duration_since(created).num_milliseconds()).ok())
        .is_some_and(|age| age <= crate::decision::builtin::weights::STALE_ARTIFACT_MS)
}

/// A remote-service hit without stored event provenance cannot prove owner or
/// workspace scope, so scoped prompt recall never uses it. Stored rows keep the
/// key and source-event exclusions; the content of a row is never compared.
fn unscoped_remote_hit(origin: MemoryEntryOrigin, source_event_id: Option<&str>) -> bool {
    origin == MemoryEntryOrigin::RemoteUnscoped && source_event_id.is_none()
}

pub(crate) fn recall_query(input: &str) -> String {
    let mut query = crate::multimodal::project_image_markers_for_routing(input).into_owned();
    query.truncate(query.floor_char_boundary(1024));
    query
}

async fn build_semantic_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    scope_ctx: Option<&MemoryWriteContext>,
    fresh_only: bool,
    excluded_key: Option<&str>,
    excluded_event: Option<&str>,
) -> RecalledMemoryContext {
    let mut summaries = Vec::new();
    let mut context = String::new();
    let mut ids = Vec::new();

    // The current turn is already in the live input; its semantic promotion
    // must not count as historical recall or trigger a presentation callback.
    let scored_entries = mem
        .recall_with_context(user_msg, 5, None, scope_ctx)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.score.map_or(true, |score| score >= min_relevance_score))
        .filter(|entry| !fresh_only || fresh_timestamp(&entry.timestamp))
        .filter(|entry| excluded_key != Some(entry.key.as_str()))
        .filter(|entry| excluded_event.is_none() || entry.source_event_id.as_deref() != excluded_event)
        .filter(|entry| !unscoped_remote_hit(entry.origin, entry.source_event_id.as_deref()))
        .map(|entry| (entry.id, entry.key, entry.content))
        .collect::<Vec<_>>();
    let entries = if fresh_only && scored_entries.is_empty() {
        // When scored recall has no matches, scoped prompt reads also use the
        // repository's bounded keyword contract, independent of decision selection.
        // Its hits have no semantic score: raw BM25 ranks are not comparable
        // with the configured cosine/hybrid relevance threshold.
        let Some(scope) = scope_ctx else {
            return RecalledMemoryContext {
                preamble: context,
                ids,
                semantic_range: 0..0,
                summaries,
            };
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // An empty source-scoped keyword store needs no content retrieval.
        // Unsupported count ports retain the bounded fallback below.
        let empty = matches!(
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                mem.count_rehearsal_hits(user_msg, 5, scope, deadline),
            )
            .await,
            Ok(Ok(0))
        );
        if empty {
            return RecalledMemoryContext {
                preamble: context,
                ids,
                semantic_range: 0..0,
                summaries,
            };
        }
        let hits = match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            mem.retrieve_decision_memory(user_msg, 5, scope, None, deadline),
        )
        .await
        {
            Ok(Ok(hits)) => Some(hits),
            outcome => {
                tracing::warn!(timed_out = outcome.is_err(), "scoped memory keyword fallback failed");
                None
            }
        };
        hits.filter(|hits| {
            hits.len() <= 5
                && hits.iter().all(|hit| {
                    hit.id.len() <= 128
                        && hit.key.len() <= 1024
                        && hit.content.len() <= 4096
                        && hit.created_at.len() <= 64
                })
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|hit| fresh_timestamp(&hit.created_at))
        .filter(|hit| excluded_key != Some(hit.key.as_str()))
        .filter(|hit| excluded_event.is_none() || hit.source_event_id.as_deref() != excluded_event)
        .map(|hit| {
            let content = crate::causal_tree::projection::bounded_artifact_summary(&hit.content).0;
            (hit.id, hit.key, content)
        })
        .collect::<Vec<_>>()
    } else {
        scored_entries
    };
    if !entries.is_empty() {
        context.push_str("[Memory context]\n");
        for (id, key, content) in entries {
            if memory::is_assistant_autosave_key(&key) {
                continue;
            }

            // Neutralize quoted media before truncating or composing a prompt.
            let quoted = crate::multimodal::neutralize_media_markers(&content);
            let content = if quoted.len() > MAX_MEMORY_ENTRY_BYTES {
                let end = quoted.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
                format!("{}...", quoted.get(..end).unwrap_or_default())
            } else {
                quoted.into_owned()
            };
            let line = format!("- {key}: {content}\n");
            if context.len() + line.len() > MAX_MEMORY_PREAMBLE_BYTES {
                break;
            }
            ids.push(id);
            summaries.push(crate::causal_tree::projection::bounded_artifact_summary(&content).0);
            context.push_str(&line);
        }
        if context != "[Memory context]\n" {
            context.push('\n');
        } else {
            context.clear();
        }
    }

    let semantic_range = 0..context.len();
    RecalledMemoryContext {
        preamble: context,
        ids,
        semantic_range,
        summaries,
    }
}

/// Flatten a historical message into a single prompt line.
///
/// Every preamble builder below quotes content that was authored in some other
/// turn (or by some other role) and splices it into the live user message.
/// Media markers must lose their trigger on the way in: an `[IMAGE:...]` the
/// assistant once wrote about, or a path whose artifact has since been cleaned
/// up, would otherwise be resolved as an attachment of the current turn and
/// fail the provider call. Neutralising happens *before* truncation so a cut
/// can never land inside a marker either.
fn neutralize_quoted_content(content: &str) -> String {
    crate::multimodal::neutralize_media_markers(content).replace('\n', " ")
}

/// Trim a quoted turn and, when it was authored by the assistant, drop the
/// fabricated `[Used tools: ...]` markers the persisted rows still carry.
///
/// The strip must happen *before* `neutralize_quoted_content`, which flattens
/// the content to one line: after flattening, a marker that used to own a whole
/// line is in-line and the line-level detector can no longer see it. User turns
/// are never rewritten — an operator quoting the marker back is real content.
fn quoted_event_content<'a>(role: &str, content: &'a str) -> Cow<'a, str> {
    let trimmed = content.trim();
    if role == "assistant" {
        strip_quoted_fabricated_tool_markers(trimmed)
    } else {
        Cow::Borrowed(trimmed)
    }
}

fn format_shared_event_line(event: &MessageEvent) -> String {
    let channel = event.channel.as_deref().unwrap_or("unknown");
    let actor = event
        .sender
        .as_deref()
        .or(event.agent_id.as_deref())
        .or(event.persona_id.as_deref())
        .unwrap_or("unknown");
    let mut content = neutralize_quoted_content(&quoted_event_content(&event.role, &event.content));
    if content.len() > MAX_MEMORY_ENTRY_BYTES {
        let end = content.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
        content = format!("{}...", content.get(..end).unwrap_or_default());
    }
    format!("- [{}:{}:{}] {}\n", event.source, channel, actor, content)
}

fn format_session_event_line(event: &MessageEvent) -> String {
    let actor = event
        .sender
        .as_deref()
        .or(event.agent_id.as_deref())
        .or(event.persona_id.as_deref())
        .unwrap_or("unknown");
    let mut content = neutralize_quoted_content(&quoted_event_content(&event.role, &event.content));
    if content.len() > MAX_MEMORY_ENTRY_BYTES {
        let end = content.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
        content = format!("{}...", content.get(..end).unwrap_or_default());
    }
    format!("- [{}:{}] {}\n", event.role, actor, content)
}

fn build_shared_events_preamble(principal: &MemoryPrincipal, events: &[MessageEvent]) -> String {
    // D4 review: classify "current session" by the full candidate key set
    // (canonical + legacy), not the single canonical `session_key`. Otherwise a
    // legacy-keyed current-session event is not recognized as belonging to the
    // current session and leaks into `[Recent shared workspace events]`.
    let session_candidates = principal.session_key_candidates();
    let mut context = String::new();
    for event in events {
        if event
            .session_key
            .as_deref()
            .is_some_and(|session_key| session_candidates.iter().any(|candidate| candidate == session_key))
        {
            continue;
        }
        if event.role == "tool" || event.role == "system" {
            continue;
        }
        if context.is_empty() {
            context.push_str("[Recent shared workspace events]\n");
        }
        let line = format_shared_event_line(event);
        if context.len() + line.len() > MAX_SHARED_EVENTS_PREAMBLE_BYTES {
            break;
        }
        context.push_str(&line);
    }
    if !context.is_empty() {
        context.push('\n');
    }
    context
}

fn build_current_session_events_preamble(
    principal: &MemoryPrincipal,
    events: &[MessageEvent],
    user_msg: &str,
) -> String {
    // D4 review: an event belongs to the current session when its `session_key`
    // is any candidate key (canonical OR legacy), so legacy-keyed current-session
    // history is preserved in `[Current session context]` after a canonical
    // cutover rather than being dropped.
    let session_candidates = principal.session_key_candidates();
    if session_candidates.is_empty() {
        return String::new();
    }
    let mut context = String::new();
    for event in events {
        let belongs_to_current_session = event
            .session_key
            .as_deref()
            .is_some_and(|session_key| session_candidates.iter().any(|candidate| candidate == session_key));
        if !belongs_to_current_session {
            continue;
        }
        if event.role == "tool" || event.role == "system" {
            continue;
        }
        if event.role == "user" && event.content.trim() == user_msg.trim() {
            continue;
        }
        if context.is_empty() {
            context.push_str("[Current session context]\n");
        }
        let line = format_session_event_line(event);
        if context.len() + line.len() > MAX_SHARED_EVENTS_PREAMBLE_BYTES {
            break;
        }
        context.push_str(&line);
    }
    if !context.is_empty() {
        context.push('\n');
    }
    context
}

fn format_session_turn_line(turn: &ConversationTurn) -> String {
    let mut content = neutralize_quoted_content(&quoted_event_content(&turn.role, &turn.content));
    if content.len() > MAX_MEMORY_ENTRY_BYTES {
        let end = content.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
        content = format!("{}...", content.get(..end).unwrap_or_default());
    }
    format!("- [{}] {}\n", turn.role, content)
}

fn build_current_session_preamble(turns: &[ConversationTurn], user_msg: &str) -> String {
    let mut context = String::new();
    for turn in turns {
        if turn.role == "tool" || turn.role == "system" {
            continue;
        }
        if turn.role == "user" && turn.content.trim() == user_msg.trim() {
            continue;
        }
        if context.is_empty() {
            context.push_str("[Current session context]\n");
        }
        let line = format_session_turn_line(turn);
        if context.len() + line.len() > MAX_SHARED_EVENTS_PREAMBLE_BYTES {
            break;
        }
        context.push_str(&line);
    }
    if !context.is_empty() {
        context.push('\n');
    }
    context
}

fn format_document_result_line(result: &DocumentSearchResult) -> String {
    let chunk = &result.chunk;
    // A document chunk can be an evicted context page, i.e. a verbatim
    // transcript of earlier assistant turns; the same fabricated markers must
    // not ride back in through document evidence either.
    let mut content = neutralize_quoted_content(&strip_quoted_fabricated_tool_markers(chunk.content.trim()));
    if content.len() > MAX_MEMORY_ENTRY_BYTES {
        let end = content.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
        content = format!("{}...", content.get(..end).unwrap_or_default());
    }
    format!(
        "- [document_id={} chunk_id={} source_anchor={}] {}\n",
        chunk.document_id, chunk.chunk_id, chunk.source_anchor, content
    )
}

fn document_result_to_context_item(result: &DocumentSearchResult) -> RetrievedContextItem {
    let chunk = &result.chunk;
    RetrievedContextItem {
        source: "document_chunk".to_string(),
        document_id: Some(chunk.document_id.clone()),
        chunk_id: Some(chunk.chunk_id.clone()),
        source_anchor: Some(chunk.source_anchor.clone()),
        score: Some(result.score),
        token_estimate: Some(chunk.token_estimate),
        payload_json: Some(
            serde_json::json!({
                "workspace_id": chunk.workspace_id,
                "owner_id": chunk.owner_id,
                "topic_id": chunk.topic_id,
                "task_id": chunk.task_id,
                "chunk_index": chunk.chunk_index
            })
            .to_string(),
        ),
    }
}

fn build_document_context_trace_payload(
    results: &[DocumentSearchResult],
) -> (String, Vec<RetrievedContextItem>, Vec<RetrievedContextItem>) {
    let mut context = String::new();
    let mut selected = Vec::new();
    let mut dropped = Vec::new();

    for result in results {
        let line = format_document_result_line(result);
        let header_len = if context.is_empty() {
            "[Document evidence]\n".len()
        } else {
            0
        };
        if context.len() + header_len + line.len() > MAX_DOCUMENT_CONTEXT_PREAMBLE_BYTES {
            dropped.push(document_result_to_context_item(result));
            continue;
        }
        if context.is_empty() {
            context.push_str("[Document evidence]\n");
        }
        context.push_str(&line);
        selected.push(document_result_to_context_item(result));
    }

    if !context.is_empty() {
        context.push('\n');
    }

    (context, selected, dropped)
}

fn owner_id_for_memory_principal(principal: &MemoryPrincipal) -> Option<String> {
    let channel = principal.channel.as_deref()?.trim();
    let sender = principal.sender.as_deref()?.trim();
    if channel.is_empty() || sender.is_empty() {
        return None;
    }
    Some(
        OwnerPrincipal::new(
            principal.workspace_id.clone(),
            channel,
            sender,
            principal.session_key.clone().unwrap_or_default(),
            vec![Role::Anonymous],
        )
        .owner_id,
    )
}

async fn append_document_retrieval_trace(
    mem: &dyn Memory,
    principal: &MemoryPrincipal,
    user_msg: &str,
    candidate_count: usize,
    selected: &[RetrievedContextItem],
    dropped: &[RetrievedContextItem],
) {
    if candidate_count == 0 {
        return;
    }

    let selected_json = match serde_json::to_string(selected) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(error = %error, "failed to serialize selected document retrieval trace items");
            None
        }
    };
    let dropped_json = match serde_json::to_string(dropped) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(error = %error, "failed to serialize dropped document retrieval trace items");
            None
        }
    };

    if let Err(error) = mem
        .append_retrieval_trace(RetrievalTraceInput {
            trace_id: None,
            workspace_id: principal.workspace_id.clone(),
            owner_id: owner_id_for_memory_principal(principal),
            session_key: principal.session_key.clone(),
            agent_id: principal.agent_id.clone(),
            persona_id: principal.persona_id.clone(),
            source: "agent_context.document_evidence".to_string(),
            query: user_msg.to_string(),
            candidate_count,
            selected_count: selected.len(),
            dropped_count: dropped.len(),
            budget_tokens: Some(MAX_DOCUMENT_CONTEXT_PREAMBLE_BYTES / 4),
            selected_json,
            dropped_json,
            payload_json: Some(
                serde_json::json!({
                    "channel": principal.channel,
                    "sender": principal.sender,
                    "limit": 5
                })
                .to_string(),
            ),
        })
        .await
    {
        tracing::debug!(error = %error, "failed to append document retrieval trace");
    }
}

pub(crate) async fn build_agent_context_with_semantic_scope(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    user_msg: &str,
    min_relevance_score: f64,
    semantic_scope: Option<&MemoryWriteContext>,
) -> AgentContextBundle {
    build_agent_context(
        mem,
        principal,
        user_msg,
        user_msg,
        min_relevance_score,
        semantic_scope,
        None,
        (None, None),
    )
    .await
}

async fn build_agent_context(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    user_msg: &str,
    live_input: &str,
    min_relevance_score: f64,
    semantic_scope: Option<&MemoryWriteContext>,
    record_user_messages: Option<bool>,
    exclusion: (Option<&str>, Option<&str>),
) -> AgentContextBundle {
    let (excluded_key, excluded_event) = exclusion;
    let semantic = if record_user_messages.is_some() {
        build_semantic_context(
            mem,
            user_msg,
            min_relevance_score,
            semantic_scope,
            true,
            excluded_key,
            excluded_event,
        )
        .await
    } else {
        build_context_with_scope(mem, user_msg, min_relevance_score, semantic_scope).await
    };
    // D4 C3: the agent recall path read-merges canonical + legacy session keys
    // through `MemoryPrincipal::session_key_candidates` (wired into
    // `list_conversation_turns` and `load_recent_session_context`). The agent
    // write side deliberately keeps its per-turn `agent:{turn_run_id}` boundary
    // (no canonical collapse): since the CLI agent has no stable cross-turn
    // session id, the principal carries no `legacy_session_key`, so the candidate
    // set degrades to the single canonical key and recall behaviour is unchanged
    // while unrelated runs stay isolated.
    let mut current_session_turns = match principal.session_key.as_deref() {
        Some(session_key) => mem
            .list_conversation_turns(&principal, session_key, 12, 0)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut current_session_events = mem
        .load_recent_session_context(SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 24,
            include_roles: vec!["user".to_string(), "assistant".to_string(), "event".to_string()],
        })
        .await
        .unwrap_or_default();
    let mut shared_events = mem
        .load_recent_shared_context(SharedContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 24,
            include_roles: vec!["user".to_string(), "assistant".to_string(), "event".to_string()],
        })
        .await
        .unwrap_or_default();
    let mut document_results = mem
        .search_document_chunks(&principal, user_msg, 5)
        .await
        .unwrap_or_default();
    if let Some(event_id) = excluded_event {
        current_session_events.retain(|event| event.event_id != event_id);
        shared_events.retain(|event| event.event_id != event_id);
    }
    if let Some(record) = record_user_messages {
        current_session_turns.retain(|turn| fresh_timestamp(&turn.timestamp) && (record || turn.role != "user"));
        current_session_events.retain(|event| fresh_timestamp(&event.created_at) && (record || event.role != "user"));
        shared_events.retain(|event| fresh_timestamp(&event.created_at) && (record || event.role != "user"));
        document_results.retain(|result| fresh_timestamp(&result.chunk.created_at));
    }
    // FIX-P2-02: hard-filter untrusted external content copies (web_fetch /
    // http_request / file_read ingests) out of the trusted recall context before
    // it is assembled into the prompt. These copies are retained in the store for
    // provenance/audit only and must never be re-injected as trusted memory.
    drop_untrusted_external_results(&mut document_results);
    let (document_context, selected_document_items, dropped_document_items) =
        build_document_context_trace_payload(&document_results);
    append_document_retrieval_trace(
        mem,
        &principal,
        user_msg,
        document_results.len(),
        &selected_document_items,
        &dropped_document_items,
    )
    .await;
    let current_session_context = {
        let from_events = build_current_session_events_preamble(&principal, &current_session_events, live_input);
        if from_events.is_empty() {
            build_current_session_preamble(&current_session_turns, live_input)
        } else {
            from_events
        }
    };

    AgentContextBundle {
        current_session_context,
        recent_shared_context: build_shared_events_preamble(&principal, &shared_events),
        semantic_memory_context: semantic.preamble,
        document_context,
        recalled_memory_ids: semantic.ids,
        semantic_summaries: semantic.summaries,
    }
}

/// Build context with recent shared message events plus semantic memory recall,
/// flattened from the structured [`AgentContextBundle`].
pub(crate) async fn build_context_with_shared_events_and_scope(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    user_msg: &str,
    min_relevance_score: f64,
    semantic_scope: Option<&MemoryWriteContext>,
) -> RecalledMemoryContext {
    let bundle =
        build_agent_context_with_semantic_scope(mem, principal, user_msg, min_relevance_score, semantic_scope).await;
    let semantic_start = bundle.current_session_context.len() + bundle.recent_shared_context.len();
    let semantic_range = semantic_start..semantic_start + bundle.semantic_memory_context.len();
    RecalledMemoryContext {
        preamble: bundle.preamble(),
        ids: bundle.recalled_memory_ids,
        semantic_range,
        summaries: bundle.semantic_summaries,
    }
}

pub(crate) async fn increment_recalled_useful_counts(mem: &dyn Memory, recalled_ids: &[String]) {
    let unique_ids: BTreeSet<&str> = recalled_ids.iter().map(String::as_str).collect();
    for id in unique_ids {
        if let Err(error) = mem.increment_useful_count(id).await {
            tracing::debug!(memory_id = id, error = %error, "failed to increment useful_count");
        }
    }
}

#[path = "turn_tests/context_recall.rs"]
mod tests;

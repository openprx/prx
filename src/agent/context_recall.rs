use crate::agent::document_ingest::drop_untrusted_external_results;
use crate::agent::response_guard::strip_quoted_fabricated_tool_markers;
use crate::memory;
use crate::memory::principal::{MemoryWriteContext, OwnerPrincipal, Role};
use crate::memory::{
    ConversationTurn, DocumentSearchResult, Memory, MemoryPrincipal, MessageEvent, RetrievalTraceInput,
    RetrievedContextItem, SessionContextQuery, SharedContextQuery,
};
use std::borrow::Cow;
use std::collections::BTreeSet;
use uuid::Uuid;

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
}

pub(crate) struct AgentContextBundle {
    pub(crate) current_session_context: String,
    pub(crate) recent_shared_context: String,
    pub(crate) semantic_memory_context: String,
    pub(crate) document_context: String,
    pub(crate) recalled_memory_ids: Vec<String>,
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

pub(super) fn autosave_memory_key(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
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
    let mut context = String::new();
    let mut ids = Vec::new();

    // Pull relevant memories for this message
    if let Ok(entries) = mem.recall_with_context(user_msg, 5, None, scope_ctx).await {
        let relevant: Vec<_> = entries
            .iter()
            .filter(|e| e.score.map_or(true, |score| score >= min_relevance_score))
            .collect();

        if !relevant.is_empty() {
            context.push_str("[Memory context]\n");
            for entry in &relevant {
                if memory::is_assistant_autosave_key(&entry.key) {
                    continue;
                }
                ids.push(entry.id.clone());

                // Quoted material: strip the marker syntax before it is spliced
                // into the live user turn, then cap the entry so a single huge
                // memory cannot dominate the preamble.
                let quoted = crate::multimodal::neutralize_media_markers(&entry.content);
                let content = if quoted.len() > MAX_MEMORY_ENTRY_BYTES {
                    let end = quoted.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
                    format!("{}...", quoted.get(..end).unwrap_or_default())
                } else {
                    quoted.into_owned()
                };

                let line = format!("- {}: {}\n", entry.key, content);

                // Cap total preamble size
                if context.len() + line.len() > MAX_MEMORY_PREAMBLE_BYTES {
                    break;
                }
                context.push_str(&line);
            }
            if context != "[Memory context]\n" {
                context.push('\n');
            } else {
                context.clear();
            }
        }
    }

    RecalledMemoryContext { preamble: context, ids }
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
    let semantic = build_context_with_scope(mem, user_msg, min_relevance_score, semantic_scope).await;
    // D4 C3: the agent recall path read-merges canonical + legacy session keys
    // through `MemoryPrincipal::session_key_candidates` (wired into
    // `list_conversation_turns` and `load_recent_session_context`). The agent
    // write side deliberately keeps its per-turn `agent:{turn_run_id}` boundary
    // (no canonical collapse): since the CLI agent has no stable cross-turn
    // session id, the principal carries no `legacy_session_key`, so the candidate
    // set degrades to the single canonical key and recall behaviour is unchanged
    // while unrelated runs stay isolated.
    let current_session_turns = match principal.session_key.as_deref() {
        Some(session_key) => mem
            .list_conversation_turns(&principal, session_key, 12, 0)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let current_session_events = mem
        .load_recent_session_context(SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 24,
            include_roles: vec!["user".to_string(), "assistant".to_string(), "event".to_string()],
        })
        .await
        .unwrap_or_default();
    let shared_events = mem
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
        let from_events = build_current_session_events_preamble(&principal, &current_session_events, user_msg);
        if from_events.is_empty() {
            build_current_session_preamble(&current_session_turns, user_msg)
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
    RecalledMemoryContext {
        preamble: bundle.preamble(),
        ids: bundle.recalled_memory_ids,
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

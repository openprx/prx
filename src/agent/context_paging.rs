use crate::agent::compaction::COMPACTION_MAX_SUMMARY_CHARS;
use crate::agent::context_budget::{estimate_history_tokens, plan_context_budget};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::history::drop_orphan_tool_results_at;
use crate::agent::response_guard::strip_quoted_fabricated_tool_markers;
use crate::agent::turn_contracts::OS_PAGING_RECALL_MARKER;
use crate::memory::{DocumentIngestInput, MemoryPrincipal};
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use sha2::Digest;
use std::fmt::Write;

/// Marker prefix used for the short page-summary breadcrumb left in the hot
/// window after an OS-paging eviction. Distinct from `[Context compacted ...]`
/// so callers can tell paging eviction apart from lossy compaction.
const OS_PAGING_EVICT_MARKER: &str = "[Context paged out at";

/// Historical framing written immediately under `OS_PAGING_RECALL_MARKER`.
///
/// A recalled page is a verbatim transcript of turns that already happened, so
/// without an explicit disclaimer a model reads the version numbers, commit
/// hashes, and command output inside it as observations of the current turn and
/// repeats them as freshly measured facts. The marker alone does not say that;
/// this notice does, and it must state the staleness, the re-measure duty, and
/// the ban on replaying page content as this turn's work.
const OS_PAGING_RECALL_HISTORY_NOTICE: &str = "HISTORY NOTICE: the excerpts below are a verbatim transcript of EARLIER turns in this session, replayed for reference only. Every number, version, commit hash, status, path, and command output in them describes the state at the time it was recorded, not the state now. Do not present anything from them as something you did, ran, or verified in this turn; when a current value matters, call the appropriate tool again and report that fresh result instead.";

/// Prefix of the per-excerpt source line that precedes every recalled page.
const OS_PAGING_RECALL_PAGE_PREFIX: &str = "[page";

/// `source_kind` used when persisting evicted context pages into the document
/// store. Distinguishes paged context from tool-output documents.
const OS_PAGING_SOURCE_KIND: &str = "context_page";

pub(super) const fn context_mode_uses_os_paging(config: &crate::config::AgentCompactionConfig) -> bool {
    !matches!(config.mode, crate::config::AgentCompactionMode::Switch)
}

/// Render evicted messages into a durable, searchable transcript. Unlike
/// compaction this is non-destructive: the full content of every paged message
/// is preserved so it can be recalled verbatim later.
fn build_page_transcript(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let _ = writeln!(transcript, "{role}: {}", msg.content.trim());
    }
    transcript
}

/// OS-paging eviction (Letta/MemGPT style), FIX-P3-05.
///
/// When the hot window exceeds `eviction_threshold * available_input_tokens`, the
/// oldest non-system messages (beyond `keep_recent_messages`) are evicted to the
/// durable document store via `Memory::ingest_document`, preserving full content
/// for later semantic recall. A short breadcrumb replaces them in the hot window.
///
/// Returns `Ok(true)` when at least one page was evicted. All failures are soft:
/// the function returns `Ok(false)` (or propagates only on genuinely fatal
/// errors) so the caller can fall back to legacy compaction without regression.
///
pub(super) async fn apply_os_paging(
    history: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
    runtime: Option<&DocumentIngestRuntime>,
) -> Result<bool> {
    let paging = &config.os_paging;
    if config.max_context_tokens == 0 {
        return Ok(false);
    }
    let budget = plan_context_budget(history, config, paging.eviction_threshold);
    if !budget.over_warning {
        return Ok(false);
    }

    // OS-paging requires a durable, searchable document backend. Without one we
    // cannot non-destructively evict, so defer to the legacy compaction path.
    let Some(runtime) = runtime else {
        return Ok(false);
    };
    if !runtime.memory.supports_document_ingest() {
        tracing::debug!(
            backend = runtime.memory.name(),
            "os-paging: backend does not support document ingest; deferring to legacy compaction"
        );
        return Ok(false);
    }

    let has_system = history.first().is_some_and(|m| m.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = history.len().saturating_sub(start);
    let keep_recent = paging.keep_recent_messages.max(1).min(non_system_count);
    let evict_count = non_system_count.saturating_sub(keep_recent);
    if evict_count == 0 {
        return Ok(false);
    }
    // Page out the stranded tail of a tool round-trip together with the
    // assistant message that requested it. Cutting between them leaves a
    // `role="tool"` orphan at the head of the hot window, which every native
    // provider rejects, and dropping it instead would lose output that was
    // never written to the page store.
    let mut evict_end = start + evict_count;
    while history.get(evict_end).is_some_and(|message| message.role == "tool") {
        evict_end = evict_end.saturating_add(1);
    }
    if evict_end >= history.len() {
        tracing::debug!("os-paging: eviction would consume the whole hot window; deferring to legacy compaction");
        return Ok(false);
    }
    let Some(to_evict) = history.get(start..evict_end) else {
        return Ok(false);
    };
    if to_evict.is_empty() {
        return Ok(false);
    }

    let transcript = build_page_transcript(to_evict);
    let token_count = estimate_history_tokens(to_evict);
    let evicted_message_count = to_evict.len();
    let timestamp = chrono::Utc::now().to_rfc3339();
    // V02-R6: clone the turn's key once. The paging identifiers below fall back
    // to a fixed label when the turn carries none, but the event row must carry
    // the real key or nothing — a synthetic label would be matchable.
    let runtime_session_key = runtime.session_key.clone();
    let session_key = runtime_session_key.as_deref().unwrap_or("session");
    // Stable, content-addressed page id so identical evictions are idempotent.
    let mut hasher = sha2::Sha256::new();
    hasher.update(session_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(transcript.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let short = digest.get(..16).unwrap_or(&digest);
    let document_id = format!("context-page:{session_key}:{short}");

    let metadata_json = serde_json::json!({
        "kind": OS_PAGING_SOURCE_KIND,
        "session_key": session_key,
        "evicted_at": timestamp,
        "message_count": to_evict.len(),
        "token_count": token_count,
    })
    .to_string();
    let input = DocumentIngestInput {
        document_id: Some(document_id.clone()),
        workspace_id: runtime.workspace_id.clone(),
        owner_id: runtime.owner_id.clone(),
        topic_id: runtime.topic_id.clone(),
        task_id: runtime.task_id.clone(),
        source_message_event_id: runtime.source_message_event_id.clone(),
        source_kind: OS_PAGING_SOURCE_KIND.to_string(),
        source_uri: Some(format!("context-page:{session_key}")),
        title: Some(format!("Paged context ({evicted_message_count} messages)")),
        content: transcript,
        mime_type: Some("text/plain".to_string()),
        visibility: runtime.visibility.clone(),
        metadata_json: Some(metadata_json),
        // V02-R6: scope the `document.ingested` event to this turn.
        session_key: runtime_session_key,
        agent_id: runtime.agent_id.clone(),
        persona_id: runtime.persona_id.clone(),
    };

    match runtime.memory.ingest_document(input).await {
        Ok(document) => {
            tracing::info!(
                document_id = %document.document_id,
                messages = evicted_message_count,
                token_count,
                "os-paging: evicted cold context to document store"
            );
        }
        Err(error) => {
            // Soft failure: do NOT mutate history. Returning false lets the
            // caller fall through to legacy compaction (no data lost, no
            // regression on the hot path).
            tracing::warn!(
                document_id = %document_id,
                error = %error,
                "os-paging: eviction failed; deferring to legacy compaction"
            );
            return Ok(false);
        }
    }

    // Replace the evicted span with a single short breadcrumb so the model knows
    // older context exists and can be recalled. Unlike compaction this is a
    // pointer, not a lossy summary — the full content lives in the page store.
    history.splice(
        start..evict_end,
        std::iter::once(ChatMessage::assistant(format!(
            "{OS_PAGING_EVICT_MARKER} {timestamp}. {} older messages moved to durable page store \
             (id: {document_id}); relevant excerpts are recalled automatically when needed.]",
            evicted_message_count
        ))),
    );
    // The breadcrumb carries no `tool_calls`, so anything left directly behind
    // it that still claims to answer one is an orphan. The forward walk above
    // normally leaves none; this is the invariant, asserted at the mutation
    // point rather than assumed.
    drop_orphan_tool_results_at(history, start.saturating_add(1));

    Ok(true)
}

/// Source line stamped in front of every recalled page excerpt so the model can
/// judge how old the excerpt is instead of reading it as a current observation.
///
/// The timestamp is the page chunk's creation time, i.e. the moment the
/// eviction wrote the page to the document store. Search results carry no
/// document metadata, so when a backend cannot supply a creation time the
/// field is dropped and the bare `[page]` line is still emitted.
fn page_source_line(evicted_at: &str) -> String {
    let evicted_at = evicted_at.trim();
    if evicted_at.is_empty() {
        format!("{OS_PAGING_RECALL_PAGE_PREFIX}]")
    } else {
        format!("{OS_PAGING_RECALL_PAGE_PREFIX} evicted_at={evicted_at}]")
    }
}

/// Semantic recall of evicted context pages (FIX-P3-05, stage 2).
///
/// Searches the durable document store for paged context relevant to `query`
/// and returns a single formatted message to be injected into history. Returns
/// `None` when paging/recall is disabled, no runtime is available, the query is
/// empty, or nothing relevant is found. All backend failures are soft (logged,
/// `None` returned) so recall never blocks a turn.
pub(super) async fn recall_context_pages(
    query: &str,
    config: &crate::config::AgentCompactionConfig,
    runtime: Option<&DocumentIngestRuntime>,
) -> Option<ChatMessage> {
    let paging = &config.os_paging;
    if !paging.proactive_retrieval {
        return None;
    }
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let runtime = runtime?;
    if !runtime.memory.supports_document_ingest() {
        return None;
    }
    let limit = paging.max_recalled_pages.max(1);
    let principal = MemoryPrincipal {
        workspace_id: runtime.workspace_id.clone(),
        agent_id: runtime.agent_id.clone(),
        persona_id: runtime.persona_id.clone(),
        session_key: runtime.session_key.clone(),
        channel: runtime.channel.clone(),
        sender: runtime.sender.clone(),
        owner_id: runtime.owner_id.clone(),
        // In-process agent turn: the historical sender/session semantics apply.
        require_owner_match: false,
        legacy_session_key: None,
    };
    // Over-fetch then filter to context pages, since search spans all docs in
    // the workspace/owner scope (tool outputs, ingested files, etc.).
    let fetch = limit.saturating_mul(4).max(limit);
    let results = match runtime.memory.search_document_chunks(&principal, query, fetch).await {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(error = %error, "os-paging: page recall failed; skipping injection");
            return None;
        }
    };
    let mut recalled = format!("{OS_PAGING_RECALL_MARKER}\n{OS_PAGING_RECALL_HISTORY_NOTICE}");
    let mut injected = 0usize;
    for result in results {
        if injected >= limit {
            break;
        }
        // Only inject chunks that originate from evicted context pages.
        if !result.chunk.document_id.starts_with("context-page:") {
            continue;
        }
        // A page is a verbatim transcript of older turns: its media markers
        // belong to those turns, not to the one being assembled now, and any
        // fabricated `[Used tools: ...]` those turns carried must not be handed
        // back to the model as something it once did.
        let unfabricated = strip_quoted_fabricated_tool_markers(result.chunk.content.trim());
        let quoted = crate::multimodal::neutralize_media_markers(&unfabricated);
        let snippet = truncate_with_ellipsis(&quoted, COMPACTION_MAX_SUMMARY_CHARS);
        let source_line = page_source_line(&result.chunk.created_at);
        let _ = write!(recalled, "\n\n---\n{source_line}\n{snippet}");
        injected += 1;
    }
    if injected == 0 {
        return None;
    }
    tracing::info!(pages = injected, "os-paging: recalled cold context pages");
    match paging.retrieval_injection_role {
        crate::config::RetrievalInjectionRole::System => Some(ChatMessage::system(recalled)),
        crate::config::RetrievalInjectionRole::User => Some(ChatMessage::user(recalled)),
    }
}

/// Insert a recalled-context message into history without polluting the most
/// recent user turn. The recall message is placed just after any leading
/// `system` message (or at the front when there is none).
pub(super) fn inject_recalled_context(history: &mut Vec<ChatMessage>, recalled: ChatMessage) {
    let has_system = history.first().is_some_and(|m| m.role == "system");
    let insert_at = if has_system { 1 } else { 0 };
    history.insert(insert_at, recalled);
}

#[path = "turn_tests/context_paging.rs"]
mod tests;

use crate::agent::document_ingest::drop_untrusted_external_results;
use crate::agent::response_guard::strip_quoted_fabricated_tool_markers;
use crate::memory;
use crate::memory::branch_copy::{CONTEXT_SWITCH_EVENT_TYPE, is_branch_copy};
use crate::memory::principal::{MemoryWriteContext, OwnerPrincipal, Role};
use crate::memory::MemoryEntry;
use crate::memory::traits::MemoryEntryOrigin;
use crate::memory::{
    ConversationTurn, DocumentSearchResult, Memory, MemoryPrincipal, MessageEvent, MessageEventSource,
    MessageEventSubject, RetrievalTraceInput, RetrievedContextItem, SessionContextQuery, SharedContextQuery,
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
    /// The stored rows the preamble was built from (empty when the caller's
    /// path does not track them).
    pub(crate) sources: crate::context::transcript::AttachmentSources,
}

/// Read scope authored by the runtime, independent of decision admission.
pub(crate) struct RecallScope {
    principal: MemoryPrincipal,
    semantic: MemoryWriteContext,
    min_relevance_score: f64,
    record_user_messages: bool,
    current_input_key: Option<String>,
    current_input_event: Option<String>,
    /// Logical task identity of a child run (`envelope.task_id`, falling back
    /// to the run id). `Some` restricts recall to this child's own task events;
    /// `None` leaves main, operator and shared-context turns unchanged.
    child_task: Option<String>,
    /// The session a woken turn's shared-events block leaves out: the sender
    /// of the message it adopted, whose rows are the run the message reports
    /// on. `None` for every other turn.
    excluded_session: Option<String>,
}

impl RecallScope {
    pub(crate) fn with_current_input_key(mut self, key: Option<String>) -> Self {
        if key.is_some() {
            self.current_input_key = key;
        }
        self
    }

    /// Leave `session`'s rows out of the shared-events block. `None` keeps it
    /// unchanged.
    pub(crate) fn with_excluded_session(mut self, session: Option<&str>) -> Self {
        self.excluded_session = session.map(str::to_owned);
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
            child_task: child_runtime_source(envelope.source).then(|| {
                envelope
                    .task_id
                    .clone()
                    .or_else(|| envelope.run_id.clone())
                    .unwrap_or_default()
            }),
            excluded_session: None,
        }
    }
}

/// Child runs are the sessions_spawn, session-worker and delegate sources; a
/// child's input and history events are recorded under those sources.
const fn child_runtime_source(source: crate::runtime::envelope::RuntimeSource) -> bool {
    use crate::runtime::envelope::RuntimeSource;
    matches!(
        source,
        RuntimeSource::SessionWorker | RuntimeSource::SessionsSpawn | RuntimeSource::Delegate
    )
}

const fn child_origin_event(source: &MessageEventSource) -> bool {
    matches!(
        source,
        MessageEventSource::SessionWorker | MessageEventSource::SessionsSpawn | MessageEventSource::Delegate
    )
}

/// Child-scoped admission of a stored event. An event whose subject names a
/// task is visible only to that task, so a sibling's task never reaches this
/// child. Child-origin events without a task subject cannot establish their
/// eligibility and are dropped. Other (main, operator, shared-fact) events
/// stay visible. An empty identity admits no child-origin event.
fn child_event_visible(event: &MessageEvent, task_id: &str) -> bool {
    match event.subject.as_ref() {
        Some(MessageEventSubject::Task(task)) => !task_id.is_empty() && task == task_id,
        _ => !child_origin_event(&event.source),
    }
}

/// Identity under which a child turn admits recall hits.
struct ChildRecallGate<'a> {
    workspace_id: &'a str,
    owner_id: Option<&'a str>,
    task_id: &'a str,
}

/// Semantic copies are admitted through their stored source-event provenance.
/// A hit without provenance is an explicit or shared fact and is kept. A hit
/// whose source event cannot be resolved (missing owner, unsupported lookup,
/// lookup error, or no row) is dropped.
async fn child_hit_admitted(mem: &dyn Memory, gate: &ChildRecallGate<'_>, source_event_id: Option<&str>) -> bool {
    let Some(event_id) = source_event_id else {
        return true;
    };
    let Some(owner_id) = gate.owner_id else {
        return false;
    };
    match mem
        .find_owned_message_event(gate.workspace_id, owner_id, event_id)
        .await
    {
        Ok(Some(event)) => child_event_visible(&event, gate.task_id),
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(error = %crate::agent::diagnostics::error_class(&error), "child recall dropped a hit whose source event did not resolve");
            false
        }
    }
}

/// Child admission of a document copy. A stored task admits only this child's
/// task. A task-less copy is admitted through its document's source event
/// (owned lookup, fail-closed); without one, only an explicitly ingested user
/// document is shared. A null task alone never admits a copy.
async fn child_chunk_admitted(mem: &dyn Memory, gate: &ChildRecallGate<'_>, result: &DocumentSearchResult) -> bool {
    if let Some(task) = result.chunk.task_id.as_deref() {
        return !gate.task_id.is_empty() && task == gate.task_id;
    }
    match result.source_message_event_id.as_deref() {
        Some(event_id) => child_hit_admitted(mem, gate, Some(event_id)).await,
        None => result.source_kind.as_deref() == Some(crate::tools::document_ingest::USER_DOCUMENT_SOURCE_KIND),
    }
}

pub(crate) const RECALL_CONTEXT_BUDGET: usize = MAX_MEMORY_PREAMBLE_BYTES;

/// A recall budget at or below this comes from a window of about 16k tokens
/// (callers pass half the window); there the semantic memory is ordered before
/// the shared events, so the clip cuts the shared block first.
const SMALL_WINDOW_RECALL_BUDGET: usize = 8_192;

/// Recall authenticated context without consulting a decision's retrieved set.
/// The budget is in bytes and is clamped to the existing context ceiling.
pub(crate) async fn scoped_recall(
    memory: &dyn Memory,
    scope: &RecallScope,
    query: &str,
    budget: usize,
) -> RecalledMemoryContext {
    recall_with_session(memory, scope, query, budget, true).await
}

/// Recall for a surface that replays the session's own rows as history. The
/// current-session block is left out because those rows are already in the
/// replayed history; the shared events, semantic memory and document evidence
/// are recalled as in [`scoped_recall`]. The budget rules are the same.
pub(crate) async fn replayed_session_recall(
    memory: &dyn Memory,
    scope: &RecallScope,
    query: &str,
    budget: usize,
) -> RecalledMemoryContext {
    recall_with_session(memory, scope, query, budget, false).await
}

async fn recall_with_session(
    memory: &dyn Memory,
    scope: &RecallScope,
    query: &str,
    budget: usize,
    include_current_session: bool,
) -> RecalledMemoryContext {
    // Historical user text must not be reintroduced when recording is off.
    if budget == 0 {
        return RecalledMemoryContext {
            preamble: String::new(),
            ids: Vec::new(),
            semantic_range: 0..0,
            summaries: Vec::new(),
            sources: Default::default(),
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
        scope.child_task.as_deref(),
        include_current_session,
        scope.excluded_session.as_deref(),
    )
    .await;
    let semantic_first = budget <= SMALL_WINDOW_RECALL_BUDGET;
    let (mut preamble, semantic_start) = if semantic_first {
        (bundle.preamble_semantic_first(), bundle.current_session_context.len())
    } else {
        (
            bundle.preamble(),
            bundle.current_session_context.len() + bundle.recent_shared_context.len(),
        )
    };
    let semantic_end = semantic_start + bundle.semantic_memory_context.len();
    let budget = budget.min(RECALL_CONTEXT_BUDGET);
    // Every id the preamble was built from, kept even when the clip below
    // drops the ones it cannot attribute: a delete may then blank more, never less.
    let sources = crate::context::transcript::AttachmentSources {
        memories: bundle.recalled_memory_ids.clone(),
        events: bundle.source_events.clone(),
        documents: bundle.source_documents.clone(),
        results: Vec::new(),
    };
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
        sources,
    }
}

pub(crate) struct AgentContextBundle {
    pub(crate) current_session_context: String,
    pub(crate) recent_shared_context: String,
    pub(crate) semantic_memory_context: String,
    pub(crate) document_context: String,
    pub(crate) recalled_memory_ids: Vec<String>,
    /// Ids of the stored events the session and shared blocks quote.
    source_events: Vec<String>,
    /// Ids of the documents the evidence block quotes.
    source_documents: Vec<String>,
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

    /// The preamble with the semantic memory block ahead of the shared events.
    fn preamble_semantic_first(&self) -> String {
        format!(
            "{}{}{}{}",
            self.current_session_context,
            self.semantic_memory_context,
            self.recent_shared_context,
            self.document_context
        )
    }
}

/// Separator between the several stored keys one current input can have (its
/// autosaved memory, its conversation-turn row).
const INPUT_KEY_SEPARATOR: char = '\u{1f}';

/// The `current_input_key` of a turn whose input is stored under several keys.
pub(crate) fn join_input_keys(keys: &[&str]) -> String {
    keys.join(&INPUT_KEY_SEPARATOR.to_string())
}

fn input_key_excluded(excluded: Option<&str>, key: &str) -> bool {
    excluded.is_some_and(|excluded| excluded.split(INPUT_KEY_SEPARATOR).any(|own| own == key))
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
    build_semantic_context(
        mem,
        user_msg,
        min_relevance_score,
        scope_ctx,
        false,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Rows recalled into the prompt per turn.
const RECALL_LIMIT: usize = 5;

/// Candidates read from the store before recency re-ranking: three times the
/// limit, the same depth the store already scans per requested row.
const RECALL_CANDIDATES: usize = RECALL_LIMIT * 3;

/// Largest relative boost recency gives a row's relevance score.
const RECENCY_MAX_BOOST: f64 = 0.1;

/// Age in days over which the recency boost halves.
const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;

/// Orders candidates by `score * (1 + RECENCY_MAX_BOOST * 0.5^(age_days /
/// RECENCY_HALF_LIFE_DAYS))`: a row stored now gets up to a 10% boost, one
/// 30 days old 5%, a year old about 0%. The boost is relative, so it works on
/// both the hybrid score (0..1) and the raw keyword score. Nothing is excluded
/// by age, a row with an unreadable timestamp gets no boost, and equal
/// adjusted scores keep the store's order (the sort is stable).
fn rank_with_recency(entries: &mut [MemoryEntry]) {
    let now = chrono::Utc::now();
    let adjusted = |entry: &MemoryEntry| -> f64 {
        let age_days = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
            .map(|created| now.signed_duration_since(created).num_seconds().max(0) as f64 / 86_400.0)
            .ok();
        let boost = age_days.map_or(0.0, |age| RECENCY_MAX_BOOST * 0.5_f64.powf(age / RECENCY_HALF_LIFE_DAYS));
        entry.score.unwrap_or(0.0) * (1.0 + boost)
    };
    entries.sort_by(|a, b| adjusted(b).total_cmp(&adjusted(a)));
}

/// Whether `timestamp` is within the recent-events window. Applies only to the
/// session and shared-event blocks; stored memories and documents have no age
/// cut-off.
fn fresh_timestamp(timestamp: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .and_then(|created| u64::try_from(chrono::Utc::now().signed_duration_since(created).num_milliseconds()).ok())
        .is_some_and(|age| age <= crate::tools::capability::turn_state::STALE_ARTIFACT_MS)
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

/// Drop recalled items whose source event is a message event of the turn's own
/// session: they are the conversation's earlier messages (autosaved on
/// channels), which the replayed history already carries. Checks only the
/// candidates' source ids (one indexed query); items without a source event
/// are kept, and a failed check keeps everything.
async fn drop_own_session_sources<T>(
    mem: &dyn Memory,
    own_session: Option<&MemoryPrincipal>,
    items: &mut Vec<T>,
    source_of: impl Fn(&T) -> Option<&str>,
) {
    let Some(principal) = own_session else {
        return;
    };
    let ids = items
        .iter()
        .filter_map(|item| source_of(item).map(str::to_owned))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }
    let Ok(own) = mem.message_event_ids_in_session(principal, &ids).await else {
        return;
    };
    if !own.is_empty() {
        items.retain(|item| source_of(item).is_none_or(|id| !own.contains(id)));
    }
}

async fn build_semantic_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    scope_ctx: Option<&MemoryWriteContext>,
    scoped_prompt: bool,
    excluded_key: Option<&str>,
    excluded_event: Option<&str>,
    child: Option<&ChildRecallGate<'_>>,
    own_session: Option<&MemoryPrincipal>,
) -> RecalledMemoryContext {
    let mut summaries = Vec::new();
    let mut context = String::new();
    let mut ids = Vec::new();

    // The current turn is already in the live input; its semantic promotion
    // must not count as historical recall or trigger a presentation callback.
    let mut candidates = mem
        .recall_with_context(user_msg, RECALL_CANDIDATES, None, scope_ctx)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.score.map_or(true, |score| score >= min_relevance_score))
        .filter(|entry| !input_key_excluded(excluded_key, &entry.key))
        .filter(|entry| excluded_event.is_none() || entry.source_event_id.as_deref() != excluded_event)
        .filter(|entry| !unscoped_remote_hit(entry.origin, entry.source_event_id.as_deref()))
        .collect::<Vec<_>>();
    drop_own_session_sources(mem, own_session, &mut candidates, |entry| entry.source_event_id.as_deref()).await;
    rank_with_recency(&mut candidates);
    candidates.truncate(RECALL_LIMIT);
    let scored_entries = candidates
        .into_iter()
        .map(|entry| (entry.id, entry.key, entry.content, entry.source_event_id))
        .collect::<Vec<_>>();
    let entries = if scoped_prompt && scored_entries.is_empty() {
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
                sources: Default::default(),
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
                sources: Default::default(),
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
        let mut hits = hits;
        if let Some(hits) = hits.as_mut() {
            drop_own_session_sources(mem, own_session, hits, |hit| hit.source_event_id.as_deref()).await;
        }
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
        .filter(|hit| !input_key_excluded(excluded_key, &hit.key))
        .filter(|hit| excluded_event.is_none() || hit.source_event_id.as_deref() != excluded_event)
        .map(|hit| {
            let content = crate::memory::artifact_summary::bounded_artifact_summary(&hit.content).0;
            let source_event_id = hit.source_event_id;
            (hit.id, hit.key, content, source_event_id)
        })
        .collect::<Vec<_>>()
    } else {
        scored_entries
    };
    let entries = match child {
        Some(gate) => {
            let mut admitted = Vec::with_capacity(entries.len());
            for (id, key, content, source_event_id) in entries {
                if child_hit_admitted(mem, gate, source_event_id.as_deref()).await {
                    admitted.push((id, key, content, source_event_id));
                }
            }
            admitted
        }
        None => entries,
    };
    if !entries.is_empty() {
        context.push_str("[Memory context]\n");
        for (id, key, content, _) in entries {
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
            summaries.push(crate::memory::artifact_summary::bounded_artifact_summary(&content).0);
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
        sources: Default::default(),
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

#[cfg(test)]
fn build_shared_events_preamble(principal: &MemoryPrincipal, events: &[MessageEvent]) -> String {
    build_shared_events_preamble_sourced(principal, events, None).0
}

/// The shared-events block and the ids of the events it quotes. Rows of
/// `excluded_session` are left out, as are the current session's rows.
fn build_shared_events_preamble_sourced(
    principal: &MemoryPrincipal,
    events: &[MessageEvent],
    excluded_session: Option<&str>,
) -> (String, Vec<String>) {
    let mut quoted = Vec::new();
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
        if excluded_session.is_some_and(|excluded| event.session_key.as_deref() == Some(excluded)) {
            continue;
        }
        if event.role == "tool" || event.role == "system" {
            continue;
        }
        // Another session's handoff note points at rows only that session may
        // look up (a branch's source session, for one): quoted here it would
        // only send the model to a refused lookup.
        if event.event_type == CONTEXT_SWITCH_EVENT_TYPE {
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
        quoted.push(event.event_id.clone());
    }
    if !context.is_empty() {
        context.push('\n');
    }
    (context, quoted)
}

/// The rows of the current session that `[Current session context]` may still
/// quote, oldest first. A context handoff replaced everything stored before
/// the session's newest handoff row (the replaced cold prefix and the kept
/// tail, which the live history carries verbatim), and the live history
/// carries the handoff itself, so only rows after it qualify. Rows a branch
/// copied are its live history or that replaced prefix, so none qualifies.
/// Older facts stay reachable through the transcript lookup tool only.
fn quotable_current_session_events<'a>(
    session_candidates: &[String],
    events: &'a [MessageEvent],
) -> impl Iterator<Item = &'a MessageEvent> {
    let belongs = |event: &MessageEvent| {
        event
            .session_key
            .as_deref()
            .is_some_and(|session_key| session_candidates.iter().any(|candidate| candidate == session_key))
    };
    let after_handoff = events
        .iter()
        .rposition(|event| belongs(event) && event.event_type == CONTEXT_SWITCH_EVENT_TYPE)
        .map_or(0, |index| index.saturating_add(1));
    events
        .get(after_handoff..)
        .unwrap_or_default()
        .iter()
        .filter(move |event| belongs(event) && !is_branch_copy(event))
}

/// `[Current session context]` from the session's stored events; `None` when
/// none of `events` belongs to the current session (the caller then falls
/// back to conversation turns). A session whose rows a handoff replaced
/// yields `Some` even when nothing is left to quote, so the fallback never
/// re-quotes what the handoff replaced.
#[cfg(test)]
fn build_current_session_events_preamble(
    principal: &MemoryPrincipal,
    events: &[MessageEvent],
    user_msg: &str,
) -> Option<String> {
    build_current_session_events_preamble_sourced(principal, events, user_msg).map(|(context, _)| context)
}

/// [`build_current_session_events_preamble`] with the ids of the events quoted.
fn build_current_session_events_preamble_sourced(
    principal: &MemoryPrincipal,
    events: &[MessageEvent],
    user_msg: &str,
) -> Option<(String, Vec<String>)> {
    let mut quoted = Vec::new();
    // D4 review: an event belongs to the current session when its `session_key`
    // is any candidate key (canonical OR legacy), so legacy-keyed current-session
    // history is preserved in `[Current session context]` after a canonical
    // cutover rather than being dropped.
    let session_candidates = principal.session_key_candidates();
    if session_candidates.is_empty() {
        return None;
    }
    let has_session_events = events.iter().any(|event| {
        event
            .session_key
            .as_deref()
            .is_some_and(|session_key| session_candidates.iter().any(|candidate| candidate == session_key))
    });
    if !has_session_events {
        return None;
    }
    let mut context = String::new();
    for event in quotable_current_session_events(&session_candidates, events) {
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
        quoted.push(event.event_id.clone());
    }
    if !context.is_empty() {
        context.push('\n');
    }
    Some((context, quoted))
}

fn format_session_turn_line(turn: &ConversationTurn) -> String {
    let mut content = neutralize_quoted_content(&quoted_event_content(&turn.role, &turn.content));
    if content.len() > MAX_MEMORY_ENTRY_BYTES {
        let end = content.floor_char_boundary(MAX_MEMORY_ENTRY_BYTES);
        content = format!("{}...", content.get(..end).unwrap_or_default());
    }
    format!("- [{}] {}\n", turn.role, content)
}

#[cfg(test)]
fn build_current_session_preamble(turns: &[ConversationTurn], user_msg: &str) -> String {
    build_current_session_preamble_sourced(turns, user_msg).0
}

/// The conversation-turn fallback block and the ids of the events its turns
/// were projected from.
fn build_current_session_preamble_sourced(turns: &[ConversationTurn], user_msg: &str) -> (String, Vec<String>) {
    let mut quoted = Vec::new();
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
        quoted.extend(turn.message_event_id.clone());
    }
    if !context.is_empty() {
        context.push('\n');
    }
    (context, quoted)
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
            tracing::debug!(error = %crate::agent::diagnostics::error_class(&error), "failed to serialize selected document retrieval trace items");
            None
        }
    };
    let dropped_json = match serde_json::to_string(dropped) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(error = %crate::agent::diagnostics::error_class(&error), "failed to serialize dropped document retrieval trace items");
            None
        }
    };

    if let Err(error) = mem
        .append_retrieval_trace(RetrievalTraceInput {
            agent_partition: principal.agent_partition.clone(),
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
        tracing::debug!(error = %crate::agent::diagnostics::error_class(&error), "failed to append document retrieval trace");
    }
}

#[cfg(test)]
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
        None,
        true,
        None,
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
    child_task: Option<&str>,
    include_current_session: bool,
    excluded_session: Option<&str>,
) -> AgentContextBundle {
    let (excluded_key, excluded_event) = exclusion;
    let gate = child_task.map(|task_id| ChildRecallGate {
        workspace_id: principal.workspace_id.as_str(),
        owner_id: principal.owner_id.as_deref(),
        task_id,
    });
    let semantic = if record_user_messages.is_some() {
        build_semantic_context(
            mem,
            user_msg,
            min_relevance_score,
            semantic_scope,
            true,
            excluded_key,
            excluded_event,
            gate.as_ref(),
            (!include_current_session).then_some(&principal),
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
    // Conversation turns carry no source provenance, so a child never reads them.
    let mut current_session_turns = match principal.session_key.as_deref() {
        Some(session_key) if child_task.is_none() && include_current_session => mem
            .list_conversation_turns(&principal, session_key, 12, 0)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut current_session_events = if include_current_session {
        mem.load_recent_session_context(SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 24,
            include_roles: vec!["user".to_string(), "assistant".to_string(), "event".to_string()],
        })
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };
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
    if let Some(gate) = gate.as_ref() {
        current_session_events.retain(|event| child_event_visible(event, gate.task_id));
        shared_events.retain(|event| child_event_visible(event, gate.task_id));
        let mut admitted = Vec::with_capacity(document_results.len());
        for result in document_results {
            if child_chunk_admitted(mem, gate, &result).await {
                admitted.push(result);
            }
        }
        document_results = admitted;
    }
    if let Some(record) = record_user_messages {
        current_session_turns.retain(|turn| fresh_timestamp(&turn.timestamp) && (record || turn.role != "user"));
        current_session_events.retain(|event| fresh_timestamp(&event.created_at) && (record || event.role != "user"));
        shared_events.retain(|event| fresh_timestamp(&event.created_at) && (record || event.role != "user"));
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
    // A replayed session's rows are already in the request history, so the
    // block is not built and its ids are not recorded.
    let (current_session_context, mut source_events) = if include_current_session {
        build_current_session_events_preamble_sourced(&principal, &current_session_events, live_input)
            .unwrap_or_else(|| build_current_session_preamble_sourced(&current_session_turns, live_input))
    } else {
        (String::new(), Vec::new())
    };
    let (recent_shared_context, shared_event_ids) =
        build_shared_events_preamble_sourced(&principal, &shared_events, excluded_session);
    source_events.extend(shared_event_ids);
    let source_documents = selected_document_items
        .iter()
        .filter_map(|item| item.document_id.clone())
        .collect();

    AgentContextBundle {
        current_session_context,
        recent_shared_context,
        source_events,
        source_documents,
        semantic_memory_context: semantic.preamble,
        document_context,
        recalled_memory_ids: semantic.ids,
        semantic_summaries: semantic.summaries,
    }
}

pub(crate) async fn increment_recalled_useful_counts(mem: &dyn Memory, recalled_ids: &[String]) {
    let unique_ids: BTreeSet<&str> = recalled_ids.iter().map(String::as_str).collect();
    for id in unique_ids {
        if let Err(error) = mem.increment_useful_count(id).await {
            tracing::debug!(memory_id = %crate::agent::diagnostics::text_class(id), error = %crate::agent::diagnostics::error_class(&error), "failed to increment useful_count");
        }
    }
}

#[path = "turn_tests/context_recall.rs"]
mod tests;

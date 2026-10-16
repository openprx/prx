//! Saved chat session persistence, message-event projection and branch/rewind helpers.

use super::chat_events::chat_runtime_envelope;
use super::{sanitize, session};
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::config::Config;
use crate::llm::route_decision::ProviderExecutionOutcome;
use crate::memory::Memory;
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;

// ── Session persistence helpers ──────────────────────────────────────────

/// Save a session to the Memory backend.
pub(super) async fn save_session(mem: &dyn Memory, session: &session::ChatSession) -> Result<()> {
    let sanitized = sanitize::sanitize_session_content(session);
    let json = sanitized.to_json().map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    mem.store(&session.memory_key(), &json, MemoryCategory::Conversation, None)
        .await
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    Ok(())
}

/// Load a session by ID (exact key lookup, not similarity search).
///
/// Returns `Ok(None)` only when there is genuinely no entry under the key.
/// A storage error (D10/C1) or a corrupt stored blob (D10/C2) is propagated as
/// `Err` rather than collapsed into "not found", so callers can fail fast
/// instead of silently starting a fresh session that buries the real context.
pub(super) async fn load_session_by_id(mem: &dyn Memory, id: &str) -> Result<Option<session::ChatSession>> {
    let key = format!("{}:{}", session::SESSION_MEMORY_PREFIX, id);
    // C1: propagate storage Err; Ok(None) is the only genuine "no such session".
    let Some(entry) = mem
        .get(&key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load session '{id}': {e}"))?
    else {
        return Ok(None);
    };
    // C2: a corrupt stored blob is data corruption, not absence — surface it.
    let session = session::ChatSession::from_json(&entry.content)
        .map_err(|e| anyhow::anyhow!("session '{id}' stored entry is corrupt: {e}"))?;
    // C3 (id consistency): the embedded id must match the requested id.  If
    // they differ the stored blob was written under the wrong key (or the key
    // was tampered with).  Resuming it would silently continue with the wrong
    // session and subsequent saves would land under the embedded id, burying
    // the entry that was stored under `id`.
    if session.id != id {
        return Err(anyhow::anyhow!(
            "session '{}' stored entry is corrupt: embedded id '{}' disagrees with requested id; \
             refusing to start a fresh session that would bury it",
            id,
            session.id
        ));
    }
    Ok(Some(session))
}

const CHAT_MESSAGE_EVENT_REPLAY_WINDOW: usize = 500;

#[derive(Debug)]
pub(super) struct ChatMessageEventProjection {
    pub(super) session: session::ChatSession,
    pub(super) turns_from_events: bool,
    pub(super) usage_from_events: bool,
}

/// Read the backend's session-key-filtered replay window. The query hard-filters
/// canonical + legacy keys before applying its bound, so unrelated workspace
/// traffic cannot evict this chat's events. A longer/incomplete replay simply
/// fails the exact parity test and retains the blob compatibility snapshot.
async fn load_chat_session_message_event_window(
    mem: &dyn Memory,
    workspace_id: &str,
    session_id: &str,
) -> Result<Vec<crate::memory::MessageEvent>> {
    let legacy_session_key = format!("chat:{session_id}");
    let principal = chat_runtime_envelope(workspace_id, &legacy_session_key).memory_principal();
    let mut events = mem
        .load_recent_session_context(crate::memory::SessionContextQuery {
            principal,
            since_event_id: None,
            limit: CHAT_MESSAGE_EVENT_REPLAY_WINDOW,
            include_roles: Vec::new(),
        })
        .await
        .map_err(|error| anyhow::anyhow!("failed to read MessageEvent replay window: {error}"))?;
    events.sort_by_key(|event| event.id);
    Ok(events)
}

fn apply_chat_message_event_projection(
    mut snapshot: session::ChatSession,
    events: &[crate::memory::MessageEvent],
    cost_config: &crate::config::schema::CostConfig,
) -> ChatMessageEventProjection {
    let message_events = events
        .iter()
        .filter(|event| {
            event.source == "chat"
                && event.event_type == "message.created"
                && matches!(event.role.as_str(), "user" | "assistant")
        })
        .collect::<Vec<_>>();
    let snapshot_turn_indices = snapshot
        .turns
        .iter()
        .enumerate()
        .filter_map(|(index, turn)| matches!(turn.role.as_str(), "user" | "assistant").then_some(index))
        .collect::<Vec<_>>();
    let turns_from_events = !message_events.is_empty()
        && message_events.len() == snapshot_turn_indices.len()
        && message_events.iter().zip(&snapshot_turn_indices).all(|(event, index)| {
            snapshot
                .turns
                .get(*index)
                .is_some_and(|turn| event.role == turn.role && event.content == turn.content)
        });
    if turns_from_events {
        for (event, index) in message_events.iter().zip(snapshot_turn_indices) {
            // MessageEvent owns replay role/content. Blob-only timestamp and
            // tool-call summaries remain compatibility metadata until the event
            // contract can reproduce them as well.
            if let Some(turn) = snapshot.turns.get_mut(index) {
                turn.role.clone_from(&event.role);
                turn.content.clone_from(&event.content);
            }
        }
    }

    let final_outcome_events = events
        .iter()
        .filter(|event| event.source == "chat" && event.event_type == "provider.final_outcome")
        .collect::<Vec<_>>();
    let mut malformed_outcome = false;
    let mut projected_usage = Vec::new();
    for event in &final_outcome_events {
        let Some(payload) = event.raw_payload_json.as_deref() else {
            malformed_outcome = true;
            break;
        };
        let outcome = match serde_json::from_str::<ProviderExecutionOutcome>(payload) {
            Ok(outcome) => outcome,
            Err(_) => {
                malformed_outcome = true;
                break;
            }
        };
        if let Some(record) =
            crate::llm::route_decision::MeteredTokenUsageRecord::from_provider_outcome(&outcome, cost_config)
        {
            projected_usage.push(record);
        }
    }
    let usage_from_events = !final_outcome_events.is_empty()
        && !malformed_outcome
        && projected_usage.len() == snapshot.token_usage_records.len()
        && projected_usage
            .iter()
            .zip(&snapshot.token_usage_records)
            .all(|(projected, persisted)| {
                projected.provider == persisted.provider
                    && projected.model == persisted.model
                    && projected.prompt_tokens == persisted.prompt_tokens
                    && projected.completion_tokens == persisted.completion_tokens
                    && projected.total_tokens == persisted.total_tokens
                    && projected.cache_creation_input_tokens == persisted.cache_creation_input_tokens
                    && projected.cache_read_input_tokens == persisted.cache_read_input_tokens
                    && projected.source == persisted.source
                    && projected.cost_usd == persisted.cost_usd
            });
    if usage_from_events {
        for (projected, persisted) in projected_usage.iter_mut().zip(&snapshot.token_usage_records) {
            projected.settlement_id.clone_from(&persisted.settlement_id);
        }
        snapshot.token_usage_records = projected_usage;
    }

    ChatMessageEventProjection {
        session: snapshot,
        turns_from_events,
        usage_from_events,
    }
}

/// Project resume/export/cost state from MessageEvent only after exact parity
/// with the persisted blob. Event read failures and partial/mismatched replay
/// retain the blob as the compatibility snapshot.
pub(super) async fn project_chat_session_from_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    snapshot: session::ChatSession,
    cost_config: &crate::config::schema::CostConfig,
) -> ChatMessageEventProjection {
    let session_id = snapshot.id.clone();
    let events = match load_chat_session_message_event_window(mem, workspace_id, &session_id).await {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(
                session_id,
                error = %error,
                "MessageEvent projection unavailable; retaining chat blob compatibility snapshot"
            );
            return ChatMessageEventProjection {
                session: snapshot,
                turns_from_events: false,
                usage_from_events: false,
            };
        }
    };
    let projection = apply_chat_message_event_projection(snapshot, &events, cost_config);
    tracing::debug!(
        session_id,
        turns_from_events = projection.turns_from_events,
        usage_from_events = projection.usage_from_events,
        "evaluated chat MessageEvent projection parity"
    );
    projection
}

pub(super) async fn load_session_by_id_with_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    id: &str,
    cost_config: &crate::config::schema::CostConfig,
) -> Result<Option<session::ChatSession>> {
    let Some(snapshot) = load_session_by_id(mem, id).await? else {
        return Ok(None);
    };
    Ok(Some(
        project_chat_session_from_message_events(mem, workspace_id, snapshot, cost_config)
            .await
            .session,
    ))
}

/// Load the most recent session.
///
/// Returns `Ok(None)` when no saved session exists. A storage error (D10/C3) or
/// a corrupt entry under an exact session key (D10) is propagated as `Err`,
/// never silently degraded to "no session".
pub(super) async fn load_latest_session(mem: &dyn Memory) -> Result<Option<session::ChatSession>> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved sessions: {e}"))?;
    // Find entries with the session prefix, parse (corrupt exact entry -> Err) and sort by updated_at.
    let mut sessions: Vec<session::ChatSession> = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);
    Ok(sessions.into_iter().next())
}

pub(super) async fn load_latest_session_with_message_events(
    mem: &dyn Memory,
    workspace_id: &str,
    cost_config: &crate::config::schema::CostConfig,
) -> Result<Option<session::ChatSession>> {
    let Some(snapshot) = load_latest_session(mem).await? else {
        return Ok(None);
    };
    Ok(Some(
        project_chat_session_from_message_events(mem, workspace_id, snapshot, cost_config)
            .await
            .session,
    ))
}

/// Parse session entries, distinguishing corruption from non-session entries.
///
/// A corrupt blob under an exact `chat_session:{id}` key is treated as data
/// corruption and returned as `Err` (rather than silently dropped, which would
/// misreport a damaged session as "no session"). Entries that are not session
/// entries at all (wrong prefix / blank id) are skipped.
fn collect_valid_sessions(entries: &[crate::memory::MemoryEntry]) -> Result<Vec<session::ChatSession>> {
    let mut sessions = Vec::new();
    for entry in entries {
        if let Some(session) = valid_session_entry(entry)? {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

pub(super) fn bind_session_to_runtime_provider_model(
    session: &mut session::ChatSession,
    provider_name: &str,
    model_name: &str,
) {
    session.provider = provider_name.to_string();
    session.model = model_name.to_string();
}

/// Validate and parse a candidate session entry.
///
/// * `Ok(None)` — the entry is not a chat-session entry (wrong prefix or blank
///   id); skip it.
/// * `Ok(Some(session))` — a valid session keyed by its own id.
/// * `Err(..)` — the entry *is* keyed as a chat session (exact `chat_session:{id}`)
///   but its blob is corrupt or its embedded id disagrees with the key. This is
///   data corruption and must not be silently filtered out (D10): a damaged
///   session must surface as an error, not be misreported as "no session".
pub(super) fn valid_session_entry(entry: &crate::memory::MemoryEntry) -> Result<Option<session::ChatSession>> {
    let Some(rest) = entry.key.strip_prefix(session::SESSION_MEMORY_PREFIX) else {
        return Ok(None);
    };
    let Some(id_from_key) = rest.strip_prefix(':') else {
        return Ok(None);
    };
    if id_from_key.trim().is_empty() {
        return Ok(None);
    }
    let session = session::ChatSession::from_json(&entry.content)
        .map_err(|e| anyhow::anyhow!("saved session '{id_from_key}' stored entry is corrupt: {e}"))?;
    if session.id == id_from_key {
        Ok(Some(session))
    } else {
        Err(anyhow::anyhow!(
            "saved session entry key '{}' disagrees with stored id '{}'",
            entry.key,
            session.id
        ))
    }
}

pub(super) fn session_turns_to_history(session: &session::ChatSession) -> Vec<ChatMessage> {
    session
        .turns
        .iter()
        .filter(|turn| turn.role == "user" || turn.role == "assistant")
        // A stored user turn re-enters a history here: one saved before
        // inbound text was neutralised could still open with a reserved marker.
        .map(|turn| {
            crate::agent::turn_contracts::stored_turn_message(
                turn.role.clone(),
                turn.content.clone(),
            )
        })
        .collect()
}

pub(super) fn persisted_history_for_current_turn(
    session: &session::ChatSession,
    system_prompt: &str,
    user_input: &str,
) -> Vec<ChatMessage> {
    let mut history = Vec::with_capacity(session.turns.len().saturating_add(2));
    history.push(ChatMessage::system(system_prompt.to_string()));
    history.extend(session_turns_to_history(session));
    history.push(crate::agent::turn_contracts::inbound_user_message(user_input));
    history
}

pub(super) fn upsert_leading_system_prompt(history: &mut Vec<ChatMessage>, system_prompt: String) {
    let system_message = ChatMessage::system(system_prompt);
    match history.first_mut() {
        None => history.push(system_message),
        Some(first) if first.role == "system" => *first = system_message,
        // Skill-RAG sessions are rebuilt from durable user/assistant turns
        // without a leading system message. Insert in that case; replacing
        // index 0 would silently discard the oldest durable user turn and
        // shift context-switch patch indices away from their audit source.
        Some(_) => history.insert(0, system_message),
    }
}

pub(super) fn history_for_session_with_system(
    session: &session::ChatSession,
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> Vec<ChatMessage> {
    let resumed_history = session_turns_to_history(session);
    if config.skill_rag.available() {
        return resumed_history;
    }
    let mut history = vec![ChatMessage::system(build_runtime_system_prompt(
        config,
        model_name,
        skills,
        native_tools,
    ))];
    history.extend(resumed_history);
    history
}

pub(super) fn format_saved_chat_sessions(sessions: &[session::ChatSession]) -> String {
    if sessions.is_empty() {
        return "No saved chat sessions.".to_string();
    }

    let mut out = String::from("Saved chat sessions:\n");
    for session in sessions {
        let title = if session.title.is_empty() {
            "(untitled)"
        } else {
            session.title.as_str()
        };
        out.push_str(&format!(
            "  {} | {} | {} turns | {}\n",
            session.id,
            title,
            session.turn_count(),
            session.updated_at.format("%Y-%m-%d %H:%M")
        ));
    }
    out.push_str("\nResume with: /resume <ID> or /resume last");
    out
}

pub(super) async fn saved_chat_sessions(mem: &dyn Memory) -> Result<Vec<session::ChatSession>> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved chat sessions: {e}"))?;
    let mut sessions = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);
    Ok(sessions)
}

fn sort_saved_sessions_newest_first(sessions: &mut [session::ChatSession]) {
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.id.cmp(&a.id))
    });
}

pub(super) fn parse_turn_boundary(raw: &str, turn_count: usize, command: &str) -> std::result::Result<usize, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("Usage: /{command} <N> where N is between 0 and {turn_count}."));
    }
    if trimmed.split_whitespace().count() != 1 {
        return Err(format!("Usage: /{command} <N> where N is between 0 and {turn_count}."));
    }
    let boundary = trimmed
        .parse::<usize>()
        .map_err(|_| format!("Invalid turn boundary '{trimmed}'. Use a number between 0 and {turn_count}."))?;
    if boundary > turn_count {
        return Err(format!(
            "Turn boundary {boundary} is out of range. This session has {turn_count} turns."
        ));
    }
    Ok(boundary)
}

fn background_summaries_for_turn_boundary(
    session: &session::ChatSession,
    keep_turns: usize,
) -> Vec<crate::chat::sessions::PersistedSessionSummary> {
    if keep_turns == session.turn_count() {
        session.background_sessions.clone()
    } else {
        Vec::new()
    }
}

pub(super) fn branched_chat_session_from(
    current: &session::ChatSession,
    keep_turns: usize,
    provider_name: &str,
    model_name: &str,
) -> session::ChatSession {
    let mut branch = session::ChatSession::new(provider_name, model_name);
    branch.id = safe_branch_session_id();
    branch.title = if current.title.is_empty() {
        format!("Branch at {keep_turns} turns")
    } else {
        format!("{} (branch at {keep_turns})", current.title)
    };
    branch.turns = current.turns.iter().take(keep_turns).cloned().collect();
    branch.background_sessions = background_summaries_for_turn_boundary(current, keep_turns);
    branch.updated_at = chrono::Utc::now();
    branch
}

fn safe_branch_session_id() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut id = String::with_capacity("branch-".len() + 32);
    id.push_str("branch-");
    for byte in uuid.as_bytes() {
        id.push(char::from(b'a' + (byte >> 4)));
        id.push(char::from(b'a' + (byte & 0x0f)));
    }
    id
}

#[cfg(feature = "terminal-tui")]
pub(super) fn rewound_chat_session_from(current: &session::ChatSession, keep_turns: usize) -> session::ChatSession {
    let mut rewound = current.clone();
    rewound.turns.truncate(keep_turns);
    rewound.background_sessions = background_summaries_for_turn_boundary(current, keep_turns);
    rewound.updated_at = chrono::Utc::now();
    rewound
}

pub(super) fn format_turn_boundaries(session: &session::ChatSession) -> String {
    let mut out = format!(
        "Turn boundaries for session {} ({} turns):\n",
        session.id,
        session.turn_count()
    );
    out.push_str("  0 | empty branch\n");
    for (idx, turn) in session.turns.iter().enumerate() {
        let boundary = idx.saturating_add(1);
        let preview = truncate_with_ellipsis(&turn.content.replace('\n', " "), 72);
        out.push_str(&format!("  {boundary} | {} | {}\n", turn.role, preview.trim()));
    }
    out.push_str("\nUse /branch <N> to fork, or /rewind <N> to trim this session.");
    out
}

/// List all saved sessions.
pub(super) async fn list_saved_sessions(mem: &dyn Memory) -> Result<()> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list saved sessions: {e}"))?;
    // C4: surface storage Err instead of unwrap_or_default printing a misleading
    // "No saved sessions"; corrupt exact entries propagate as Err (D10).
    let mut sessions: Vec<session::ChatSession> = collect_valid_sessions(&entries)?;
    sort_saved_sessions_newest_first(&mut sessions);

    if sessions.is_empty() {
        println!("No saved sessions.");
        return Ok(());
    }

    println!("Saved sessions:\n");
    for s in &sessions {
        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
        // B2 (v4 review): print the FULL session id. The previous 8-char
        // truncation could not be passed back to `--session`, which requires
        // the complete UUID — copy/pasting a listed id now resumes correctly.
        println!(
            "  {} | {} | {} turns | {}",
            s.id,
            title,
            s.turn_count(),
            s.updated_at.format("%Y-%m-%d %H:%M")
        );
    }
    println!("\nResume with: prx chat --session <ID>");
    Ok(())
}

//! Per-sender conversation history, persistence and hydration.

use super::inbound_policy::ConversationKey;
use super::{
    Arc, ChannelRuntimeContext, ChatMessage, HashMap, MAX_CHANNEL_HISTORY, MAX_HYDRATED_SESSIONS, Memory, MemoryFabric,
    MemoryPrincipal, MemoryVisibility, RuntimeEnvelope, strip_fabricated_tool_markers_from_assistant_turns,
};

/// Reshape cached turns into the strict user/assistant alternation the
/// providers expect, and drop fabricated `[Used tools: ...]` lines from the
/// assistant turns on the way out.
///
/// The strip happens on this replay copy only: the persisted
/// `conversation_turns` rows keep exactly what the model produced. It is what
/// stops ~150 historical fabrications from teaching the model, turn after turn,
/// that announcing tool use without calling a tool is the house style.
pub(super) fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut normalized = Vec::with_capacity(turns.len());
    let mut expecting_user = true;

    for turn in turns {
        match (expecting_user, turn.role.as_str()) {
            (true, "user") => {
                normalized.push(turn);
                expecting_user = false;
            }
            (false, "assistant") => {
                normalized.push(turn);
                expecting_user = true;
            }
            // Interrupted channel turns can produce consecutive user messages
            // (no assistant persisted yet). Merge instead of dropping.
            (false, "user") | (true, "assistant") => {
                if let Some(last_turn) = normalized.last_mut() {
                    if !turn.content.is_empty() {
                        if !last_turn.content.is_empty() {
                            last_turn.content.push_str("\n\n");
                        }
                        last_turn.content.push_str(&turn.content);
                    }
                }
            }
            _ => {}
        }
    }

    strip_fabricated_tool_markers_from_assistant_turns(&mut normalized);
    normalized
}

/// Read-only union of the canonical and legacy histories for `key`.
///
/// FIX-P1-25b (read-merge, not move): the legacy `{channel}_{sender}` history
/// predates the canonical recipient dimension, so it is surfaced as shared
/// pre-history for any canonical conversation of the same sender. Legacy turns
/// are ordered first (they are older — written before the canonical cutover),
/// then canonical turns. The result is truncated to the same
/// `MAX_CHANNEL_HISTORY` window used for single-key reads.
///
/// ## No deduplication — pure concatenation
///
/// The legacy and canonical keys partition the timeline: legacy turns were
/// written *before* the canonical cutover, canonical turns *after* it. Each
/// physical conversation turn is persisted under exactly one of the two keys, so
/// reading the union never surfaces the same physical turn twice. A
/// content/role-based dedup would therefore only ever remove *genuinely
/// repeated* messages within a single conversation (e.g. a user who deliberately
/// sends the same text twice in a row), corrupting the history the model sees.
/// We must not do that — the merge is a plain ordered concatenation
/// (legacy ++ canonical), preserving each store's own insertion order.
///
/// The legacy map entry is never moved, mutated, or removed here; it stays a
/// read-only fallback.
pub(super) fn merged_history(
    map: &std::collections::HashMap<String, Vec<ChatMessage>>,
    key: &ConversationKey,
) -> Vec<ChatMessage> {
    let legacy_turns = if key.legacy == key.canonical {
        None
    } else {
        map.get(&key.legacy)
    };
    let canonical_turns = map.get(&key.canonical);

    let capacity = legacy_turns.map_or(0, Vec::len) + canonical_turns.map_or(0, Vec::len);
    if capacity == 0 {
        return Vec::new();
    }

    // Pure ordered concatenation: legacy pre-history first, then canonical turns,
    // each in its own insertion order. No dedup (see doc comment) — repeated
    // messages within a conversation are real history and must be preserved.
    let mut merged: Vec<ChatMessage> = Vec::with_capacity(capacity);
    merged.extend(legacy_turns.into_iter().flatten().cloned());
    merged.extend(canonical_turns.into_iter().flatten().cloned());

    // Apply the same retention window single-key reads enforce, keeping the most
    // recent turns when the union exceeds the cap.
    if merged.len() > MAX_CHANNEL_HISTORY {
        let drop = merged.len() - MAX_CHANNEL_HISTORY;
        merged.drain(0..drop);
    }

    // Fabricated `[Used tools: ...]` lines are stripped here too, not only in
    // `normalize_cached_channel_turns`: this function is the read path for every
    // caller that consumes the union directly (transcript lookups, compaction
    // accounting), several of which never run the normalizer. The cached map and
    // the sqlite rows behind it are untouched — `merged` is a fresh clone.
    strip_fabricated_tool_markers_from_assistant_turns(&mut merged);
    merged
}

pub(super) fn clear_sender_history(ctx: &ChannelRuntimeContext, key: &ConversationKey) {
    // Clear ONLY the current (canonical) session. The legacy
    // `{channel}_{sender}` entry is the *immutable, cross-recipient shared*
    // pre-cutover history: removing it here would silently wipe the pre-history
    // of every *other* recipient of the same sender (legacy has no recipient
    // dimension), which violates the "legacy is read-only, never moved/deleted"
    // invariant. So clear (e.g. triggered by /model or /models) drops the
    // canonical conversation only; the legacy pre-history is left intact and will
    // naturally age out of the merged read window as new canonical turns fill it.
    ctx.conversation_histories.lock().remove(&key.canonical);
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn append_sender_turn(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    key: &ConversationKey,
    channel: &str,
    sender: &str,
    recipient: Option<&str>,
    turn: ChatMessage,
    visibility: MemoryVisibility,
    timestamp: Option<&str>,
    message_id: Option<&str>,
    // D8-1: the per-turn run_id, generated at the turn entry (before the inbound
    // append). Threaded onto the channel_with_session envelope so the user and
    // assistant message events recorded for this turn carry the same run_id
    // (latent provenance, EU AI Act Art.12).
    run_id: &str,
    record_message_event: bool,
) -> Option<crate::memory::MessageEvent> {
    let role = turn.role.clone();
    // The reply is persisted (conversation turn and message event) redacted like
    // the terminal commit (D-053); the live history keeps the delivered text.
    let content = if role == "assistant" {
        crate::agent::terminal::redact_persisted_text(&turn.content)
    } else {
        turn.content.clone()
    };
    {
        // Writes always go to the canonical entry only; the legacy entry is left
        // untouched as a read-only fallback that the read paths (`merged_history`)
        // union in. The per-key cap stays at MAX_CHANNEL_HISTORY here; the merged
        // read view re-applies the same cap across the union.
        let mut histories = ctx.conversation_histories.lock();
        let turns = histories.entry(key.canonical.clone()).or_default();
        turns.push(turn);
        while turns.len() > MAX_CHANNEL_HISTORY {
            turns.remove(0);
        }
    }

    // The message-event envelope keeps the legacy session_key: message_events are
    // a separate persistence namespace whose shared-event recall is keyed on the
    // existing identity, so it is left untouched here. The canonical key governs
    // the conversation-turn cache (above) and its persistence (below); legacy
    // conversation-turn rows stay readable via the merged-read fallback, so no
    // turn is lost.
    let envelope = RuntimeEnvelope::channel_with_session(
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
        key.legacy.clone(),
        channel.to_string(),
        sender.to_string(),
        recipient.unwrap_or(sender).to_string(),
        visibility,
    )
    .with_run_id(run_id)
    .with_config_generation(config_generation);
    let owner_id = envelope.resolved_owner_id();

    if let Err(error) = ctx
        .memory
        .append_conversation_turn(
            &key.canonical,
            channel,
            sender,
            &role,
            &content,
            timestamp,
            message_id,
            Some(owner_id.as_str()),
        )
        .await
    {
        tracing::warn!(
            session_key = key.canonical,
            channel,
            sender,
            "Failed to persist channel conversation turn: {error}"
        );
    }

    let fabric = MemoryFabric::new(
        ctx.memory.clone(),
        ctx.workspace_dir.as_path().to_string_lossy().to_string(),
    )
    .with_event_recording(ctx.memory_event_recording);
    let scope = if role == "assistant" {
        envelope.message_scope().with_sender("prx")
    } else {
        envelope.message_scope()
    };
    if !record_message_event {
        return None;
    }
    let result = if role == "assistant" {
        fabric.record_assistant_message(scope, content).await
    } else {
        fabric
            .record_inbound_user_message(
                scope,
                content,
                message_id.map(|id| format!("channel:{channel}:{id}")),
                None,
            )
            .await
    };
    match result {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(
                session_key = key.canonical,
                channel,
                sender,
                role,
                "Failed to persist channel message event: {error}"
            );
            None
        }
    }
}

pub(super) async fn load_persisted_histories(
    workspace_dir: &std::path::Path,
    memory: &dyn Memory,
) -> HashMap<String, Vec<ChatMessage>> {
    let principal = MemoryPrincipal {
        workspace_id: workspace_dir.to_string_lossy().to_string(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: Some("system:*".to_string()),
        // Local history hydration under the system owner; not an external origin.
        require_owner_match: false,
        legacy_session_key: None,
    };
    match memory
        .load_recent_conversation_histories(&principal, MAX_CHANNEL_HISTORY, MAX_HYDRATED_SESSIONS)
        .await
    {
        Ok(histories) => {
            // Hydrate every persisted session under its own stored key — legacy
            // `{channel}_{sender}` rows land under the legacy key, post-cutover
            // canonical rows under the canonical key. The read paths
            // (`merged_history`) union the two on access (FIX-P1-25b read-merge),
            // so a session that was migrated and then ran once (turns split across
            // both keys) reads back the full ordered union after a restart. We do
            // not merge here, because the in-memory map is keyed per stored key and
            // `ConversationKey` derivation (which pairs a canonical key with its
            // legacy key) happens per inbound message, not at hydration time.
            let mut hydrated: HashMap<String, Vec<ChatMessage>> = HashMap::new();
            for (session_key, turns) in histories {
                // stored user turns are neutralised like fresh input
                let turns = turns
                    .into_iter()
                    .map(|turn| crate::agent::turn_contracts::stored_turn_message(turn.role, turn.content))
                    .collect::<Vec<_>>();
                if !turns.is_empty() {
                    hydrated.insert(session_key, normalize_cached_channel_turns(turns));
                }
            }
            hydrated
        }
        Err(error) => {
            // Startup hydration is best-effort: a failure must not block channel
            // startup. The durable conversation_turns rows are untouched and will
            // re-hydrate on the next start; emit a structured warn so operators can
            // locate the cause instead of silently running on an empty cache.
            tracing::warn!(
                error = %error,
                "channel history hydration from persistent store failed; starting with empty in-memory cache (durable rows intact, will re-hydrate next start)"
            );
            HashMap::new()
        }
    }
}

/// Whether a history message carries the result of an executed tool call.
///
/// Native-tool mode emits a dedicated `tool` role message per call; prompt mode
/// folds every result of the iteration into one `user` message behind
/// [`crate::agent::turn_contracts::TOOL_RESULTS_PREFIX`].
#[cfg(test)]
pub(super) fn is_tool_result_message(message: &ChatMessage) -> bool {
    message.role == "tool"
        || (message.role == "user"
            && message
                .content
                .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX))
}

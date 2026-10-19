//! Chat message-event recording and turn finalization.

#[cfg(test)]
use super::session;
use super::{decision_source, sanitize};
use crate::llm::route_decision::ProviderExecutionOutcome;
use crate::llm::route_decision::RouteDecision;
use crate::llm::route_decision::record_route_decision_event as record_raw_route_decision_event;
use crate::memory::MemoryFabric;
use crate::memory::MemoryVisibility;
use crate::memory::MessageEventScope;
#[cfg(feature = "terminal-tui")]
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use anyhow::Context;

/// Run mode of the chat input path.
///
/// After the v0.4.1 cleanup the terminal TUI only supports the single reducer/driver route. The old
/// Off/Both/Redux staged-rollout modes were retired once v0.4.0 was accepted; `PRX_CHAT_REDUX` is
/// still read once to warn, but it no longer changes the run path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
pub(crate) enum ReduxMode {
    Pure,
}

#[cfg(feature = "terminal-tui")]
impl ReduxMode {
    /// Read the retired `PRX_CHAT_REDUX` env only to warn operators when a
    /// stale deployment still tries to select a legacy mode.
    pub(super) fn from_env() -> Self {
        if let Ok(raw) = std::env::var("PRX_CHAT_REDUX") {
            let value = raw.trim();
            if !value.is_empty() && !value.eq_ignore_ascii_case("pure") && value != "2" {
                tracing::warn!(
                    requested = value,
                    "PRX_CHAT_REDUX legacy modes are retired; using Pure reducer path"
                );
            }
        }
        Self::Pure
    }
}

pub(super) fn autosave_memory_key(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}:{ts}")
}

/// Extract the raw chat session id from the legacy `chat:{id}` durable key.
///
/// D4 C6: the durable `session_key` is migrated to a stable canonical derived
/// from the immutable session id, so the helpers need the bare id. Defensive:
/// if the key is not in `chat:{id}` form it is returned unchanged (the canonical
/// derivation then keys on the whole string, still stable).
pub(super) fn chat_session_id_from_key(chat_session_key: &str) -> &str {
    chat_session_key.strip_prefix("chat:").unwrap_or(chat_session_key)
}

/// Build a chat message-event write scope on the stable durable-canonical
/// `session_key` (D4 C6).
///
/// The durable `session_key` is the recipient-aware canonical derived from the
/// session id (`chat:terminal:local-user:{id}`), NOT the legacy `chat:{id}`, and
/// NOT `{provider}/{model}` (which would split one conversation across model
/// switches). provider/model is recorded on the event's `recipient` field only —
/// it does not feed the durable `session_key`. The legacy `chat:{id}` key is
/// carried for read-merge so pre-cutover history stays visible.
pub(super) fn chat_message_event_scope(
    workspace_id: &str,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
) -> MessageEventScope {
    let chat_session_id = chat_session_id_from_key(chat_session_key);
    RuntimeEnvelope::chat_canonical(workspace_id, chat_session_id, MemoryVisibility::Workspace)
        .with_recipient(format!("{provider_name}/{model_name}"))
        .with_run_id(chat_run_id.to_string())
        .message_scope()
}

/// Build the chat read envelope on the same stable durable-canonical
/// `session_key` as the write scope (D4 C6), carrying the legacy `chat:{id}` key
/// for read-merge so the read principal unions canonical + legacy history.
pub(super) fn chat_runtime_envelope(workspace_id: &str, chat_session_key: &str) -> RuntimeEnvelope {
    let chat_session_id = chat_session_id_from_key(chat_session_key);
    RuntimeEnvelope::chat_canonical(workspace_id.to_string(), chat_session_id, MemoryVisibility::Workspace)
        .with_recipient("terminal:user")
}

#[cfg(test)]
pub(super) async fn record_chat_user_message_event(
    memory_fabric: &MemoryFabric,
    chat_session: &session::ChatSession,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    turn_seq: u64,
    user_input: &str,
) -> anyhow::Result<crate::memory::MessageEvent> {
    record_chat_user_message_event_in_generation(
        memory_fabric,
        chat_session,
        chat_session_key,
        chat_run_id,
        provider_name,
        model_name,
        turn_seq,
        user_input,
        None,
    )
    .await
}

#[cfg(test)]
pub(super) async fn record_chat_user_message_event_in_generation(
    memory_fabric: &MemoryFabric,
    chat_session: &session::ChatSession,
    chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    turn_seq: u64,
    user_input: &str,
    generation: Option<&crate::config::ConfigGeneration>,
) -> anyhow::Result<crate::memory::MessageEvent> {
    let mut scope = chat_message_event_scope(
        memory_fabric.workspace_id(),
        chat_session_key,
        chat_run_id,
        provider_name,
        model_name,
    );
    if let Some(generation) = generation {
        scope.config_generation_id = Some(generation.id.0);
        scope.config_source_revision = Some(generation.source_revision.fingerprint_sha256.clone());
    }
    memory_fabric
        .record_inbound_user_message(
            scope,
            sanitize::sanitize_for_persistence(user_input),
            Some(format!("chat:{}:{chat_run_id}:{turn_seq}:user", chat_session.id)),
            None,
        )
        .await
}

pub(super) async fn record_route_decision_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    decision: &RouteDecision,
) -> anyhow::Result<crate::memory::MessageEvent> {
    let sanitized = sanitize::sanitize_json_structure(decision)?;
    record_raw_route_decision_event(fabric, scope, &sanitized).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_chat_success_turn(
    turn_binding: Option<&decision_source::ChatTurnBinding>,
    fabric: &MemoryFabric,
    _chat_session_key: &str,
    chat_run_id: &str,
    provider_name: &str,
    model_name: &str,
    _route_scope: MessageEventScope,
    provider_outcome: ProviderExecutionOutcome,
    recorded_response: Option<String>,
    history_commit_len: usize,
    summary: &str,
    cost_config: &crate::config::schema::CostConfig,
    workspace_dir: &std::path::Path,
) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
    let binding = turn_binding.context("chat terminal binding is missing")?;
    let decision = binding
        .settlement_ref(chat_run_id)?
        .context("chat terminal decision has not been issued")?;
    let route_scope = binding.envelope().message_scope();

    // `finalize_turn` redacts the reply and the provider outcome before it
    // persists them, for chat as for every other surface.
    let history = recorded_response.map(|assistant_content| crate::agent::terminal::TurnHistoryProjection {
        assistant_content,
        history_commit_len,
    });
    let history_scope = history.as_ref().map(|_| {
        route_scope
            .clone()
            .with_sender(format!("{provider_name}/{model_name}"))
            .with_recipient("local-user")
    });
    let receipt = binding
        .commit_terminal(
            fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(decision),
                terminal_id: chat_run_id.to_string(),
                scope: route_scope,
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history,
                history_scope,
                provider_outcome: Some(provider_outcome.clone()),
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: summary.to_string(),
                    started_at: provider_outcome.started_at,
                    finished_at: provider_outcome.finished_at,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                    target: "terminal:local-user".to_string(),
                },
            },
            cost_config,
            workspace_dir,
        )
        .await?;
    binding.rejoin_terminal(chat_run_id).await?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_chat_non_success_turn(
    turn_binding: Option<&decision_source::ChatTurnBinding>,
    fabric: &MemoryFabric,
    chat_run_id: &str,
    _route_scope: MessageEventScope,
    status: crate::agent::terminal::TurnTerminalStatus,
    provider_outcome: Option<ProviderExecutionOutcome>,
    started_at: chrono::DateTime<chrono::Utc>,
    summary: &str,
    cost_config: &crate::config::schema::CostConfig,
    workspace_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let binding = turn_binding.context("chat terminal binding is missing")?;
    let Some(decision) = binding.settlement_ref(chat_run_id)? else {
        anyhow::ensure!(
            provider_outcome.is_none(),
            "pre-decision failure cannot settle a provider outcome"
        );
        return binding
            .record_predecision_failure(fabric, chat_run_id, status, summary)
            .await;
    };
    let route_scope = binding.envelope().message_scope();

    // `finalize_turn` redacts the provider outcome (with its error text)
    // before it persists it.
    let finished_at = provider_outcome
        .as_ref()
        .map_or_else(chrono::Utc::now, |outcome| outcome.finished_at);
    binding
        .commit_terminal(
            fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(decision),
                terminal_id: chat_run_id.to_string(),
                scope: route_scope,
                status,
                history: None,
                history_scope: None,
                provider_outcome,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: summary.to_string(),
                    started_at,
                    finished_at,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                    reason: summary.to_string(),
                },
            },
            cost_config,
            workspace_dir,
        )
        .await?;
    binding.rejoin_terminal(chat_run_id).await?;
    Ok(())
}

pub(super) fn sanitize_chat_semantic_memory_content(content: &str) -> String {
    sanitize::sanitize_for_persistence(content)
}

/// Aggregate the model's reasoning/thinking content from the turn's history
/// slice. The agent loop encodes assistant turns that carried reasoning as a
/// JSON object containing `{"reasoning_content": "..."}` (see
/// `build_native_assistant_history` in `agent/history.rs`). We pull those out
/// and join them with blank-line separators so the TUI can render a single
/// foldable card per turn.
///
/// Returns an empty string when no reasoning is present, signalling the
/// caller to skip pushing a card.
#[cfg(feature = "terminal-tui")]
pub(super) fn collect_reasoning_from_history_slice(slice: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in slice {
        if msg.role != "assistant" {
            continue;
        }
        // Fast pre-filter to skip plain-text assistant turns without paying
        // the JSON parse cost.
        if !msg.content.contains("reasoning_content") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg.content) else {
            continue;
        };
        if let Some(rc) = parsed.get("reasoning_content").and_then(serde_json::Value::as_str) {
            let trimmed = rc.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    parts.join("\n\n")
}

//! Shared terminal commit for provider/tool turns.
//!
//! Provider execution has one owner in `agent::loop_`; this module owns the
//! matching durable terminal boundary. Entry points supply their projection and
//! delivery intent, while this finalizer writes idempotent history/telemetry,
//! computes one usage settlement, and appends one `turn.finalized` commit marker.

use crate::config::schema::CostConfig;
use crate::llm::route_decision::{MeteredTokenUsageRecord, ProviderExecutionOutcome};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Completed,
    Silent,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnDeliveryIntent {
    Reply { target: String },
    ReturnToCaller,
    Suppress { reason: String },
    Deferred { route: String },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnHistoryProjection {
    pub assistant_content: String,
    pub history_commit_len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnTerminalTelemetry {
    pub summary: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct TurnTerminalCommit {
    pub terminal_id: String,
    pub scope: MessageEventScope,
    pub status: TurnTerminalStatus,
    pub history: Option<TurnHistoryProjection>,
    /// Optional message scope for the assistant history projection. Runtime
    /// telemetry may use a different channel/recipient scope.
    pub history_scope: Option<MessageEventScope>,
    pub provider_outcome: Option<ProviderExecutionOutcome>,
    pub telemetry: TurnTerminalTelemetry,
    pub delivery_intent: TurnDeliveryIntent,
}

#[derive(Debug, Clone)]
pub struct TurnTerminalReceipt {
    pub terminal_id: String,
    pub terminal_event: MessageEvent,
    pub assistant_event: Option<MessageEvent>,
    pub usage_settlement: Option<MeteredTokenUsageRecord>,
    pub delivery_intent: TurnDeliveryIntent,
}

#[must_use]
pub(crate) fn provider_outcome_from_trace(
    decision: &crate::llm::route_decision::RouteDecision,
    started_at: chrono::DateTime<chrono::Utc>,
    trace: crate::agent::loop_::ToolLoopTrace,
) -> ProviderExecutionOutcome {
    if trace.final_model.is_some() && !trace.attempts.is_empty() {
        let final_provider = trace
            .final_provider
            .unwrap_or_else(|| decision.selected.provider.clone());
        let final_model = trace.final_model.unwrap_or_else(|| decision.selected.model.clone());
        ProviderExecutionOutcome::from_trace_with_usage(
            decision,
            trace.attempts,
            final_provider,
            final_model,
            started_at,
            chrono::Utc::now(),
            trace.any_turn_had_fallback,
            trace.tokens_used,
        )
    } else {
        ProviderExecutionOutcome::success_for_decision_with_usage(decision, started_at, trace.tokens_used)
    }
}

#[must_use]
pub fn usage_settlement(
    terminal_id: &str,
    outcome: &ProviderExecutionOutcome,
    cost_config: &CostConfig,
) -> Option<MeteredTokenUsageRecord> {
    MeteredTokenUsageRecord::from_provider_outcome(outcome, cost_config).map(|mut record| {
        record.settlement_id = Some(terminal_id.to_string());
        record
    })
}

#[derive(Debug, Serialize)]
struct TurnTerminalPayload<'a> {
    terminal_id: &'a str,
    status: TurnTerminalStatus,
    history: &'a Option<TurnHistoryProjection>,
    assistant_event_id: Option<&'a str>,
    provider_outcome: &'a Option<ProviderExecutionOutcome>,
    usage_settlement: &'a Option<MeteredTokenUsageRecord>,
    telemetry: &'a TurnTerminalTelemetry,
    delivery_intent: &'a TurnDeliveryIntent,
    attempt_id: Option<&'a str>,
    lease_epoch: Option<i64>,
}

/// Finalize one semantic turn.
///
/// The `turn.finalized` marker is deliberately written last. A retry after a
/// partial failure replays assistant/provider writes through stable idempotency
/// keys, then closes the transaction with exactly one marker. The marker embeds
/// the sole metered usage settlement and the requested delivery disposition.
pub async fn finalize_turn(
    fabric: &MemoryFabric,
    commit: TurnTerminalCommit,
    cost_config: &CostConfig,
) -> anyhow::Result<TurnTerminalReceipt> {
    anyhow::ensure!(!commit.terminal_id.trim().is_empty(), "terminal_id must not be empty");

    let assistant_event = match commit.history.as_ref() {
        Some(history) if commit.status == TurnTerminalStatus::Completed => Some(
            fabric
                .record_assistant_message_idempotent(
                    commit.history_scope.clone().unwrap_or_else(|| commit.scope.clone()),
                    history.assistant_content.clone(),
                    format!("turn:{}:assistant", commit.terminal_id),
                )
                .await?,
        ),
        _ => None,
    };

    if let Some(outcome) = commit.provider_outcome.as_ref() {
        crate::llm::route_decision::record_provider_outcome_events(fabric, commit.scope.clone(), outcome).await?;
    }

    let usage_settlement = commit
        .provider_outcome
        .as_ref()
        .and_then(|outcome| usage_settlement(&commit.terminal_id, outcome, cost_config));
    let payload = TurnTerminalPayload {
        terminal_id: &commit.terminal_id,
        status: commit.status,
        history: &commit.history,
        assistant_event_id: assistant_event.as_ref().map(|event| event.event_id.as_str()),
        provider_outcome: &commit.provider_outcome,
        usage_settlement: &usage_settlement,
        telemetry: &commit.telemetry,
        delivery_intent: &commit.delivery_intent,
        attempt_id: commit.scope.attempt_id.as_deref(),
        lease_epoch: commit.scope.lease_epoch,
    };
    let payload_json = serde_json::to_string(&payload)?;
    let terminal_event = fabric
        .record_runtime_event_idempotent(
            commit.scope,
            "turn.finalized",
            format!("terminal_id={} status={:?}", commit.terminal_id, commit.status),
            Some(payload_json),
            format!("turn:{}:final", commit.terminal_id),
        )
        .await?;

    Ok(TurnTerminalReceipt {
        terminal_id: commit.terminal_id,
        terminal_event,
        assistant_event,
        usage_settlement,
        delivery_intent: commit.delivery_intent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::route_decision::RouteDecision;
    use crate::memory::{Memory, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use std::sync::Arc;

    #[tokio::test]
    async fn repeated_finalize_writes_one_terminal_one_assistant_and_one_usage_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");
        let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
            .with_channel("test")
            .with_session_key("session-a")
            .with_run_id("run-a");
        let started_at = chrono::Utc::now();
        let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate("provider-a", "model-a"),
            started_at,
            crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
        );
        let commit = TurnTerminalCommit {
            terminal_id: "run-a".to_string(),
            scope,
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "done".to_string(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: Some(outcome),
            telemetry: TurnTerminalTelemetry {
                summary: "test completed".to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        };

        let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default())
            .await
            .unwrap();
        let second = finalize_turn(&fabric, commit, &CostConfig::default()).await.unwrap();

        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(
            first
                .usage_settlement
                .as_ref()
                .and_then(|usage| usage.settlement_id.as_deref()),
            Some("run-a")
        );
        let principal = MemoryPrincipal {
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("session-a".to_string()),
            channel: Some("test".to_string()),
            sender: None,
            owner_id: None,
            legacy_session_key: None,
        };
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "provider.final_outcome")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn non_reply_terminal_states_replay_once_without_assistant_projection() {
        for (index, status) in [
            TurnTerminalStatus::Silent,
            TurnTerminalStatus::Failed,
            TurnTerminalStatus::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            let temp = tempfile::tempdir().unwrap();
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
            let fabric = MemoryFabric::new(memory.clone(), "workspace-a");
            let session_key = format!("non-reply-{index}");
            let terminal_id = format!("terminal-{index}");
            let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
                .with_channel("test")
                .with_session_key(session_key.clone())
                .with_run_id(terminal_id.clone());
            let started_at = chrono::Utc::now();
            let provider_outcome = (status == TurnTerminalStatus::Silent).then(|| {
                ProviderExecutionOutcome::success_for_decision_with_usage(
                    &RouteDecision::single_candidate("provider-a", "model-a"),
                    started_at,
                    crate::llm::route_decision::TokenUsage::reported(Some(4), Some(2), Some(6)),
                )
            });
            let commit = TurnTerminalCommit {
                terminal_id: terminal_id.clone(),
                scope,
                status,
                // A defensive history projection on a non-completed status must
                // never create an assistant message.
                history: Some(TurnHistoryProjection {
                    assistant_content: "must not be projected".to_string(),
                    history_commit_len: 2,
                }),
                history_scope: None,
                provider_outcome,
                telemetry: TurnTerminalTelemetry {
                    summary: format!("{status:?}"),
                    started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: TurnDeliveryIntent::None,
            };

            let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default())
                .await
                .unwrap();
            let second = finalize_turn(&fabric, commit, &CostConfig::default()).await.unwrap();
            assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
            assert!(first.assistant_event.is_none());
            assert_eq!(first.usage_settlement.is_some(), status == TurnTerminalStatus::Silent);

            let principal = MemoryPrincipal {
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some(session_key),
                channel: Some("test".to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            };
            let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.event_type == "turn.finalized" && event.content.contains(terminal_id.as_str())
                    })
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                    .count(),
                0
            );
        }
    }

    #[tokio::test]
    async fn all_runtime_entrypoint_kinds_share_one_terminal_and_usage_contract() {
        const ENTRYPOINTS: [&str; 8] = [
            "chat",
            "agent",
            "gateway_webhook",
            "channel",
            "gateway_console",
            "session_worker",
            "sessions_spawn",
            "delegate",
        ];
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");

        for entrypoint in ENTRYPOINTS {
            let terminal_id = format!("{entrypoint}-run");
            let scope = MessageEventScope::new(entrypoint, MemoryVisibility::Workspace)
                .with_channel(entrypoint)
                .with_session_key(terminal_id.clone())
                .with_run_id(terminal_id.clone());
            let started_at = chrono::Utc::now();
            let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
                &RouteDecision::single_candidate("provider-a", "model-a"),
                started_at,
                crate::llm::route_decision::TokenUsage::reported(Some(3), Some(2), Some(5)),
            );
            let receipt = finalize_turn(
                &fabric,
                TurnTerminalCommit {
                    terminal_id: terminal_id.clone(),
                    scope,
                    status: TurnTerminalStatus::Completed,
                    history: Some(TurnHistoryProjection {
                        assistant_content: format!("{entrypoint} done"),
                        history_commit_len: 2,
                    }),
                    history_scope: None,
                    provider_outcome: Some(outcome),
                    telemetry: TurnTerminalTelemetry {
                        summary: format!("{entrypoint} completed"),
                        started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: TurnDeliveryIntent::ReturnToCaller,
                },
                &CostConfig::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                receipt
                    .usage_settlement
                    .as_ref()
                    .and_then(|usage| usage.settlement_id.as_deref()),
                Some(terminal_id.as_str())
            );

            let principal = MemoryPrincipal {
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some(terminal_id.clone()),
                channel: Some(entrypoint.to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            };
            let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.event_type == "turn.finalized" && event.content.contains(terminal_id.as_str())
                    })
                    .count(),
                1,
                "{entrypoint} must have one terminal marker"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.event_type == "message.created"
                            && event.role == "assistant"
                            && event.content == format!("{entrypoint} done")
                    })
                    .count(),
                1,
                "{entrypoint} must have one assistant projection"
            );
        }
    }
}

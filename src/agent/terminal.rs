//! Shared terminal commit for provider/tool turns.
//!
//! Provider execution has one owner in `agent::loop_`; this module owns the
//! matching durable terminal boundary. Entry points supply their projection and
//! delivery intent, while this finalizer writes idempotent history/telemetry,
//! computes one usage settlement, and appends one `turn.finalized` commit marker.

use crate::config::schema::CostConfig;
use crate::cost::types::CostSettlement;
use crate::llm::route_decision::{MeteredTokenUsageRecord, ProviderExecutionOutcome};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    pub cost_settlement: Option<CostSettlement>,
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
    cost_settlement: &'a Option<CostSettlement>,
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
async fn ensure_runtime_authority(scope: &MessageEventScope, phase: &'static str) -> anyhow::Result<()> {
    let Some(authority_guard) = scope.authority_guard.clone() else {
        return Ok(());
    };
    anyhow::ensure!(
        tokio::task::spawn_blocking(move || authority_guard.validate())
            .await
            .context("runtime authority validation task failed")??,
        "runtime execution authority was lost during terminal finalization phase {phase}"
    );
    Ok(())
}

pub async fn finalize_turn(
    fabric: &MemoryFabric,
    commit: TurnTerminalCommit,
    cost_config: &CostConfig,
    workspace_dir: &Path,
) -> anyhow::Result<TurnTerminalReceipt> {
    anyhow::ensure!(!commit.terminal_id.trim().is_empty(), "terminal_id must not be empty");
    ensure_runtime_authority(&commit.scope, "start").await?;

    let usage_settlement = commit
        .provider_outcome
        .as_ref()
        .and_then(|outcome| usage_settlement(&commit.terminal_id, outcome, cost_config));

    // The canonical usage event is written before the JSONL projection. A
    // crash can therefore leave usage awaiting projection, but can no longer
    // leave an untraceable cost-only settlement.
    if let Some(usage) = usage_settlement.as_ref() {
        let usage_json = serde_json::to_string(usage)?;
        fabric
            .record_runtime_event_idempotent(
                commit.scope.clone(),
                "usage.settled",
                format!("settlement_id={}", commit.terminal_id),
                Some(usage_json),
                format!("turn:{}:usage", commit.terminal_id),
            )
            .await?;
    }

    let cost_settlement = match usage_settlement.as_ref() {
        Some(usage) if cost_config.enabled => {
            let cost_config = cost_config.clone();
            let workspace_dir = workspace_dir.to_path_buf();
            let usage = usage.clone();
            Some(
                match tokio::task::spawn_blocking(move || {
                    let tracker = crate::cost::tracker::CostTracker::for_workspace(cost_config, &workspace_dir)?;
                    tracker.settle_metered(&usage)
                })
                .await
                {
                    Ok(Ok(settlement)) => settlement,
                    Ok(Err(error)) => CostSettlement::Failed {
                        error: error.to_string(),
                    },
                    Err(error) => CostSettlement::Failed {
                        error: format!("cost settlement task failed: {error}"),
                    },
                },
            )
        }
        Some(_) => Some(CostSettlement::Disabled),
        None => None,
    };
    ensure_runtime_authority(&commit.scope, "projection").await?;

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

    let payload = TurnTerminalPayload {
        terminal_id: &commit.terminal_id,
        status: commit.status,
        history: &commit.history,
        assistant_event_id: assistant_event.as_ref().map(|event| event.event_id.as_str()),
        provider_outcome: &commit.provider_outcome,
        usage_settlement: &usage_settlement,
        cost_settlement: &cost_settlement,
        telemetry: &commit.telemetry,
        delivery_intent: &commit.delivery_intent,
        attempt_id: commit.scope.attempt_id.as_deref(),
        lease_epoch: commit.scope.lease_epoch,
    };
    let payload_json = serde_json::to_string(&payload)?;
    ensure_runtime_authority(&commit.scope, "terminal-marker").await?;
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
        cost_settlement,
        delivery_intent: commit.delivery_intent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::route_decision::RouteDecision;
    use crate::memory::{Memory, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use std::sync::Arc;

    fn completed_commit(terminal_id: &str) -> TurnTerminalCommit {
        let started_at = chrono::Utc::now();
        TurnTerminalCommit {
            terminal_id: terminal_id.to_string(),
            scope: MessageEventScope::new("agent", MemoryVisibility::Workspace)
                .with_channel("test")
                .with_session_key(terminal_id)
                .with_run_id(terminal_id),
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "done".to_string(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: Some(ProviderExecutionOutcome::success_for_decision_with_usage(
                &RouteDecision::single_candidate("provider-a", "model-a"),
                started_at,
                crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
            )),
            telemetry: TurnTerminalTelemetry {
                summary: "test completed".to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        }
    }

    fn workspace_principal(workspace_id: &str, session_key: &str) -> MemoryPrincipal {
        MemoryPrincipal {
            workspace_id: workspace_id.to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some(session_key.to_string()),
            channel: Some("test".to_string()),
            sender: None,
            owner_id: None,
            legacy_session_key: None,
        }
    }

    #[tokio::test]
    async fn logical_workspace_id_uses_explicit_cost_ledger_directory() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let cost_config = CostConfig {
            enabled: true,
            ..CostConfig::default()
        };

        let receipt = finalize_turn(
            &fabric,
            completed_commit("logical-workspace-run"),
            &cost_config,
            temp.path(),
        )
        .await
        .unwrap();

        assert_eq!(receipt.cost_settlement, Some(CostSettlement::UnknownPricing));
        assert!(temp.path().join("state").join("costs.jsonl").parent().unwrap().exists());
        let events = memory
            .list_message_events_since(
                &workspace_principal("logical-workspace", "logical-workspace-run"),
                0,
                32,
            )
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn invalid_cost_ledger_directory_is_recorded_in_terminal_projection() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let invalid_workspace_dir = temp.path().join("not-a-directory");
        std::fs::write(&invalid_workspace_dir, "occupied").unwrap();
        let cost_config = CostConfig {
            enabled: true,
            ..CostConfig::default()
        };

        let receipt = finalize_turn(
            &fabric,
            completed_commit("preflight-failure-run"),
            &cost_config,
            &invalid_workspace_dir,
        )
        .await
        .unwrap();

        assert!(matches!(
            receipt.cost_settlement,
            Some(CostSettlement::Failed { ref error })
                if error.contains("Failed to open cost storage")
        ));
        let events = memory
            .list_message_events_since(
                &workspace_principal("logical-workspace", "preflight-failure-run"),
                0,
                32,
            )
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        let terminal_payload: serde_json::Value = serde_json::from_str(
            events
                .iter()
                .find(|event| event.event_type == "turn.finalized")
                .and_then(|event| event.raw_payload_json.as_deref())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            terminal_payload
                .pointer("/cost_settlement/status")
                .and_then(serde_json::Value::as_str),
            Some("failed")
        );
    }

    #[tokio::test]
    async fn lost_runtime_authority_fails_before_terminal_or_cost_projection() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let mut commit = completed_commit("lost-authority-run");
        commit.scope.authority_guard = Some(crate::memory::RuntimeAuthorityGuard::new("lost-authority", || {
            Ok(false)
        }));
        let cost_config = CostConfig {
            enabled: true,
            ..CostConfig::default()
        };

        let error = finalize_turn(&fabric, commit, &cost_config, temp.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runtime execution authority was lost"));
        let events = memory
            .list_message_events_since(&workspace_principal("logical-workspace", "lost-authority-run"), 0, 32)
            .await
            .unwrap();
        assert!(events.is_empty());
        assert!(!temp.path().join("state").join("costs.jsonl").exists());
    }

    #[tokio::test]
    async fn authority_lost_during_projection_never_writes_stale_terminal_or_assistant() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let validations = Arc::new(AtomicUsize::new(0));
        let mut commit = completed_commit("mid-finalize-authority-loss");
        commit.scope.authority_guard = Some(crate::memory::RuntimeAuthorityGuard::new(
            "authority-lost-after-usage",
            {
                let validations = Arc::clone(&validations);
                move || Ok(validations.fetch_add(1, Ordering::SeqCst) == 0)
            },
        ));

        let error = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("runtime execution authority was lost during terminal finalization phase projection")
        );
        let events = memory
            .list_message_events_since(
                &workspace_principal("logical-workspace", "mid-finalize-authority-loss"),
                0,
                32,
            )
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            0
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwritable_cost_ledger_is_recorded_in_terminal_projection() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let cost_path = state_dir.join("costs.jsonl");
        std::fs::write(&cost_path, "").unwrap();
        std::fs::set_permissions(&cost_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let mut cost_config = CostConfig {
            enabled: true,
            ..CostConfig::default()
        };
        cost_config.prices.insert(
            "provider-a/model-a".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );

        let receipt = finalize_turn(
            &fabric,
            completed_commit("cost-write-failure-run"),
            &cost_config,
            temp.path(),
        )
        .await
        .unwrap();

        assert!(matches!(
            receipt.cost_settlement,
            Some(CostSettlement::Failed { ref error })
                if error.contains("Failed to open cost storage")
        ));
        let events = memory
            .list_message_events_since(
                &workspace_principal("logical-workspace", "cost-write-failure-run"),
                0,
                32,
            )
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cost_lock_wait_does_not_block_runtime_and_usage_precedes_projection() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let cost_path = state_dir.join("costs.jsonl");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(cost_path.with_extension("jsonl.lock"))
            .unwrap();
        lock_file.lock().unwrap();

        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "logical-workspace");
        let mut cost_config = CostConfig {
            enabled: true,
            ..CostConfig::default()
        };
        cost_config.prices.insert(
            "provider-a/model-a".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let workspace_dir = temp.path().to_path_buf();
        let task = tokio::spawn({
            let fabric = fabric.clone();
            async move {
                finalize_turn(
                    &fabric,
                    completed_commit("cost-lock-liveness"),
                    &cost_config,
                    &workspace_dir,
                )
                .await
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let events = memory
            .list_message_events_since(&workspace_principal("logical-workspace", "cost-lock-liveness"), 0, 32)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert!(!cost_path.exists());
        assert!(!task.is_finished());

        drop(lock_file);
        let receipt = task.await.unwrap().unwrap();
        assert!(matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })));
        assert_eq!(
            std::fs::read_to_string(&cost_path)
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            1
        );
    }

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

        let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default(), temp.path())
            .await
            .unwrap();
        let second = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
            .await
            .unwrap();

        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(
            first
                .usage_settlement
                .as_ref()
                .and_then(|usage| usage.settlement_id.as_deref()),
            Some("run-a")
        );
        assert_eq!(first.cost_settlement, Some(CostSettlement::Disabled));
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
                .filter(|event| event.event_type == "usage.settled")
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

            let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default(), temp.path())
                .await
                .unwrap();
            let second = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
                .await
                .unwrap();
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
            let events = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
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
                temp.path(),
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
            let events = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
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

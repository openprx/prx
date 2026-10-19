#![cfg(test)]

use super::admission_resources;
use crate::agent::test_admission::AdmittedTurn;
use crate::decision::telemetry::CausalEventKind;
use crate::memory::{Memory, SqliteMemory};
use crate::runtime::turn_cleanup::CleanupTracker;
use anyhow::Context;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test]
async fn admission_releases_phase_writes_while_its_physical_accounting_is_retained() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let memory = Arc::new(SqliteMemory::new(workspace.path())?);
    let owner = Arc::new(CleanupTracker::default());
    owner
        .scope(Box::pin(async {
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.config_path = workspace.path().join("config.toml");
            assert!(config.causal_tree.enabled);
            let admitted = AdmittedTurn::with_enabled_builtin_model(
                config,
                "Explain two integers.",
                Arc::new(Vec::new()),
                memory.clone(),
                ("ollama", "prx-local-model"),
            )
            .await;
            let decision = admitted.decision();
            let service = decision.service;
            assert!(service.drain_writes(Instant::now() + Duration::from_secs(2)).await);
            let principal = admitted.principal();
            let before = memory.list_message_events_since(&principal, 0, 100).await?.len();
            let budgets = service.agent_budget_snapshot(decision.proof)?.budgets;
            assert_eq!(budgets.latency_ms, 500);
            let deadline = Instant::now() + Duration::from_millis(budgets.latency_ms);
            let (accounting, guard) = admission_resources(service, decision.proof, budgets, deadline);
            anyhow::ensure!(guard.is_some(), "enabled admission owns phase deferral");
            let retained = accounting.ensure_scope()?;
            service.emit_agent_phase(
                decision.proof,
                decision.context.decision_id(),
                CausalEventKind::CandidateGated,
                Some(1),
            );
            let sink = service.durable_events.as_ref().context("durable phase sink")?;
            let (first, concurrent) = tokio::join!(
                sink.drain(Instant::now() + Duration::from_secs(2)),
                sink.drain(Instant::now() + Duration::from_secs(2)),
            );
            assert!(
                first && concurrent,
                "drain must not wait for a live admission guard or its retained accounting"
            );
            drop(accounting);
            drop(guard);
            assert!(sink.drain(Instant::now() + Duration::from_secs(2)).await);
            assert_eq!(retained.remaining_budgets().tokens, budgets.tokens);
            assert_eq!(
                memory.list_message_events_since(&principal, 0, 100).await?.len(),
                before + 1
            );
            Ok(())
        }))
        .await
}

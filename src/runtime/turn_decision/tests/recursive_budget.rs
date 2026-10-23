//! Exercise allowance fanout through a real admitted parent and sealed grant.
#![cfg(test)]

use super::*;
use crate::agent::test_admission::{AdmittedTurn, registry_with_search};
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::tools::sessions_spawn::SessionsSpawnTool;

pub(super) struct NoChannel;
#[async_trait::async_trait]
impl Channel for NoChannel {
    fn name(&self) -> &str {
        "prx_grant_fixture"
    }
    async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
        anyhow::bail!("grant fixture must not announce")
    }
    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        anyhow::bail!("grant fixture has no listener")
    }
}

#[tokio::test]
async fn recursive_publication_cannot_multiply_the_parent_allowance_by_reissue_or_clone() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.sessions_spawn.allow_recursive_delegation = true;
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
    let spawn = SessionsSpawnTool::new(
        Arc::new(NoChannel),
        "ollama",
        "prx-local-model",
        0.0,
        security,
        workspace.path().to_owned(),
        config.multimodal.clone(),
        Default::default(),
        None,
        Default::default(),
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let registry = registry_with_search(vec![Box::new(spawn)]);
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let result = cleanup
        .scope(Box::pin(async {
            let admitted = AdmittedTurn::with_enabled_builtin_model(
                config,
                "sessions_spawn: Explain two integers.",
                Arc::clone(&registry),
                Arc::clone(&memory),
                ("ollama", "prx-local-model"),
            )
            .await;
            let batch = admitted
                .execute_tool(
                    "sessions_spawn",
                    serde_json::json!({
                        "action": "spawn_batch", "allow_delegation": true,
                        "tasks": ["Explain the first integer.", "Explain the second integer."]
                    }),
                )
                .await;
            let refusal = match batch {
                Ok(result) => {
                    assert!(!result.success);
                    result.error.unwrap_or_default()
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(refusal.contains("at most one child"), "{refusal}");
            let rows = memory.list_message_events_since(&admitted.principal(), 0, 100).await?;
            assert!(
                !rows.iter().any(|row| row.event_type == "task.spawned"),
                "refused fanout must not launch a member"
            );
            let decision = admitted.decision();
            let issue = || decision.service.issue_child_grant(decision.proof, decision.context);
            let root = issue()?;
            let ordinary = root.for_spawn_request(false, "ollama", "prx-local-model")?;
            assert!(ordinary.scope().gate_inputs().0.allowed.is_empty());
            assert!(ordinary.scope().gate_inputs().0.authority.delegation.is_none());
            let recursive = root.for_spawn_request(true, "ollama", "prx-local-model")?;
            let (manifest, parent) = recursive.scope().gate_inputs();
            manifest.verify(parent)?;
            assert!(parent.authority.max_cost_cents > 0, "exercise a real nonzero allowance");
            assert_eq!(manifest.authority.max_cost_cents, parent.authority.max_cost_cents / 2);
            let root_remaining = root.recursive_child_allowance().context("root budget owner")?.money();
            assert_eq!(
                root_remaining.load(Ordering::Acquire) + u64::from(manifest.authority.max_cost_cents) * 10_000,
                u64::from(parent.authority.max_cost_cents) * 10_000
            );
            assert_eq!(manifest.authority.remaining_depth, 1);
            assert_eq!(manifest.allowed.len(), 1);
            assert!(
                issue()?.for_spawn_request(true, "ollama", "prx-local-model").is_err(),
                "reissuing from the same decision must not reset its allowance"
            );
            // Component boundary: the genuine recursive grant reaches the
            // production batch parser with two non-opted-in leaves. No model
            // call or launched nested worker is claimed by this assertion.
            let leaves = crate::runtime::child_source::scope_outgoing_grant(
                Some(Arc::clone(&recursive)),
                registry
                    .first()
                    .expect("one real spawn tool")
                    .execute(serde_json::json!({
                        "action": "spawn_batch", "tasks": ["First leaf.", "Second leaf."]
                    })),
            )
            .await;
            let error = leaves.expect_err("recursive leaf fanout must be refused before launch");
            assert!(error.to_string().contains("at most one child"));
            // A cloned/process-narrowed incoming grant retains its one leaf slot.
            let worker = Arc::new(recursive.for_process_worker()?);
            let leaf = worker.for_spawn_request(false, "ollama", "prx-local-model")?;
            assert_eq!(
                leaf.scope().gate_inputs().0.authority.max_cost_cents,
                manifest.authority.max_cost_cents / 2
            );
            let child_remaining = recursive
                .recursive_child_allowance()
                .context("child budget owner")?
                .money();
            let leaf_money = u64::from(leaf.scope().gate_inputs().0.authority.max_cost_cents) * 10_000;
            assert!(leaf_money < u64::from(manifest.authority.max_cost_cents) * 10_000);
            assert_eq!(
                root_remaining.load(Ordering::Acquire) + child_remaining.load(Ordering::Acquire) + leaf_money,
                u64::from(parent.authority.max_cost_cents) * 10_000
            );
            assert!(
                recursive.for_spawn_request(false, "ollama", "prx-local-model").is_err(),
                "a second leaf would duplicate the whole subtree ceiling"
            );
            assert!(worker.for_spawn_request(false, "ollama", "prx-local-model").is_err());
            assert!(
                decision
                    .service
                    .drain_writes(Instant::now() + Duration::from_secs(5))
                    .await
            );
            Ok(())
        }))
        .await;
    cleanup.drain((Instant::now() + Duration::from_secs(5)).into()).await?;
    result
}

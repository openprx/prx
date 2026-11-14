//! Source-issued filesystem confinement for authenticated Console turns.
//!
//! Paths in model arguments remain logical relative paths. The retained scope
//! selects the physical namespace independently of arguments and approvals.
use super::OpenedTurn;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::approval_ledger::ApprovalReviewAdapter;
use crate::tools::owner_filesystem::OwnerFilesystem;
use anyhow::Context;
use std::sync::Arc;

impl TurnRuntimeService {
    pub(super) async fn console_filesystem(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<Option<Arc<OwnerFilesystem>>> {
        if proof.approval_review_adapter() != Some(ApprovalReviewAdapter::ConsoleOwnerReview) {
            return Ok(None);
        }
        // The sealed Gateway proof grants this adapter only for an actual
        // isolated Console envelope with durable authenticated input lineage.
        self.resolve_agent_model(proof)?;
        let mut supported = false;
        for tool in ["file_read", "file_write", "file_edit"] {
            if self.execution.supports_public_name(tool) {
                if !self.execution.supports_owner_filesystem(tool) {
                    return Ok(None);
                }
                supported = true;
            }
        }
        if !supported {
            return Ok(None);
        }
        self.open_console_filesystem(proof.workspace_id(), proof.owner_id())
            .await
            .map_or_else(
                |_| {
                    tracing::warn!("Console private filesystem is unavailable; file capabilities remain denied");
                    Ok(None)
                },
                |scope| Ok(Some(scope)),
            )
    }

    async fn open_console_filesystem(&self, workspace: &str, owner: &str) -> anyhow::Result<Arc<OwnerFilesystem>> {
        let config = &self.generation.effective;
        anyhow::ensure!(
            !owner.is_empty() && workspace == config.workspace_dir.to_string_lossy(),
            "Console filesystem identity differs from the pinned workspace"
        );
        let root = config
            .config_path
            .parent()
            .context("Console private filesystem root missing")?
            .to_owned();
        let shared_workspace = config.workspace_dir.clone();
        let workspace = workspace.to_owned();
        let owner = owner.to_owned();
        owned_filesystem_read(move || OwnerFilesystem::open(&root, &workspace, &owner, &shared_workspace).map(Arc::new))
            .await
    }

    pub(super) fn verify_console_filesystem_context(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            context
                .runtime()
                .trusted
                .constraints
                .iter()
                .any(|code| code.as_str() == "owner_private_filesystem_v1")
                == context.owner_filesystem.is_some()
                && context.runtime().trusted.owner_isolated() == proof.owner_isolated()
                && context.owner_filesystem.as_ref().is_none_or(|scope| {
                    proof.approval_review_adapter() == Some(ApprovalReviewAdapter::ConsoleOwnerReview)
                        && proof.owner_isolated()
                        && scope.matches(proof.workspace_id(), proof.owner_id())
                }),
            "Console filesystem scope differs from the sealed turn source"
        );
        Ok(())
    }
}

/// Register before spawning and finish from the actual blocking closure. A
/// dropped waiter cannot make the gateway owner report an unfinished read done.
async fn owned_filesystem_read<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let owner = crate::runtime::turn_cleanup::current_tracker().context("Console filesystem cleanup owner missing")?;
    crate::runtime::ingress_recovery::owned_store_read(&owner, work).await
}

#[cfg(test)]
mod working_root_tests {

    struct FixedShellApproval(bool);

    #[async_trait::async_trait]
    impl crate::tools::ApprovalStrategy for FixedShellApproval {
        async fn resolve(&self, _: crate::tools::ToolApprovalRequest) -> crate::tools::ToolApprovalDecision {
            if self.0 {
                crate::tools::ToolApprovalDecision::Approved {
                    runtime_approval_granted: true,
                    runtime_grant: None,
                }
            } else {
                crate::tools::ToolApprovalDecision::Denied {
                    reason: "test: operator denied".into(),
                }
            }
        }
    }

    /// R58: in Supervised, a shell call whose path resolves inside a working
    /// root outside the workspace is approved once and executes once; a
    /// denied one never executes. The approval path check uses the root.
    #[tokio::test]
    async fn supervised_working_root_approval_executes_once_and_denial_never_executes() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let workspace = tempfile::tempdir().expect("test: workspace");
            let root = tempfile::tempdir().expect("test: working root");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().into();
            config.runtime.workdir = Some(root.path().into());
            config.autonomy.level = crate::security::AutonomyLevel::Supervised;
            config.autonomy.workspace_only = false;
            config.autonomy.forbidden_paths = vec![root.path().join("blocked").to_string_lossy().into_owned()];
            config.causal_tree.enabled = false;
            let generation = crate::config::new_shared(config.clone()).pin();
            let catalog = crate::providers::catalog::CatalogView::from_config(&config);
            let service =
                crate::runtime::turn_publication::service::test_support::disabled_agent_service(&generation, &catalog)
                    .await;
            std::fs::create_dir(root.path().join("state")).expect("test: state directory");
            let command = "printf x >> state/count";
            for (approve, expected) in [(false, ""), (true, "x")] {
                let tool = crate::tools::shell::ShellTool::new(
                    std::sync::Arc::new(crate::runtime::NativeRuntime::new()),
                    root.path().into(),
                )
                .with_path_policy(service.security.clone());
                let execution = crate::tools::ToolExecutionService::from_shared_boxed_registry(
                    std::sync::Arc::new(vec![Box::new(tool) as Box<dyn crate::tools::Tool>]),
                    std::sync::Arc::new(crate::tools::SecurityEffectPolicy::new(service.security.clone())),
                    std::sync::Arc::new(FixedShellApproval(approve)),
                    std::sync::Arc::new(crate::tools::AdapterOwnedPreparation),
                    std::sync::Arc::new(crate::tools::TracingToolExecutionAudit),
                );
                let context = crate::tools::ToolExecutionContext::new(
                    crate::runtime::envelope::RuntimeEnvelope::agent("workspace-a", "run-a")
                        .with_channel("cli")
                        .with_sender("local-user")
                        .with_owner_id("owner-a"),
                    "dm",
                );
                let outcome = execution
                    .execute(
                        crate::tools::ToolExecutionCommand::new("shell", serde_json::json!({"command": command})),
                        context,
                        None,
                    )
                    .await;
                let written = std::fs::read_to_string(root.path().join("state/count")).unwrap_or_default();
                assert_eq!(written, expected, "test: approve={approve}: {}", outcome.model_content);
                if !approve {
                    assert_eq!(outcome.status, crate::tools::ToolExecutionStatus::ApprovalDenied);
                }
            }
        })
        .await
        .expect("test: working-root approval deadline");
    }
}

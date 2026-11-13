//! Source-issued filesystem confinement for authenticated Console turns.
//!
//! Paths in model arguments remain logical relative paths. The retained scope
//! selects the physical namespace independently of arguments and approvals.
use super::{TurnDecisionContext, TurnDecisionService};
use crate::acl::approval_grant::RiskLevel;
use crate::causal_tree::approval_collection::ApprovalReviewAdapter;
use crate::memory::{MessageEvent, MessageEventSource};
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::owner_filesystem::OwnerFilesystem;
use anyhow::Context;
use std::sync::Arc;

impl TurnDecisionService {
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

    /// Called only with the MAC-authenticated durable source of a verified
    /// pending approval. A display owner or model-authored source is insufficient.
    pub(super) async fn pending_console_filesystem(
        &self,
        source: &MessageEvent,
    ) -> anyhow::Result<Option<Arc<OwnerFilesystem>>> {
        if source.source != MessageEventSource::Console {
            return Ok(None);
        }
        anyhow::ensure!(
            source.config_generation_id == Some(self.generation.id.0)
                && source.config_source_revision.as_deref()
                    == Some(self.generation.source_revision.fingerprint_sha256.as_str())
                && source.id > 0
                && !source.event_id.is_empty(),
            "pending Console filesystem source generation changed"
        );
        let owner = source.owner_id.as_deref().context("pending Console owner missing")?;
        self.open_console_filesystem(&source.workspace_id, owner)
            .await
            .map(Some)
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

    /// Reject impossible shell paths before recording or reviewing approval.
    pub(super) fn check_shell_path_arguments(&self, command: &str) -> anyhow::Result<()> {
        if self.security.autonomy != crate::security::AutonomyLevel::Full {
            let root = crate::security::path_policy::effective_root(
                self.generation.effective.runtime.workdir.as_deref(),
                &self.security,
            );
            self.security
                .validate_command_paths_at(root, command)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    /// The collection, runtime recorder, review and resume all resolve the same
    /// logical path through this function. Resolution creates no directories.
    pub(super) async fn scoped_file_operation(
        &self,
        filesystem: Option<&Arc<OwnerFilesystem>>,
        tool: &str,
        path: &str,
    ) -> anyhow::Result<(String, RiskLevel)> {
        anyhow::ensure!(matches!(tool, "file_write" | "file_edit"), "unsupported file mutation");
        let root = crate::security::path_policy::effective_root(
            if filesystem.is_some() {
                None
            } else {
                self.generation.effective.runtime.workdir.as_deref()
            },
            &self.security,
        );
        anyhow::ensure!(
            self.security.is_path_allowed_at(root, path),
            "file approval path is outside allowed workspace"
        );
        let (operation, risk) = if let Some(filesystem) = filesystem {
            anyhow::ensure!(
                self.execution.supports_owner_filesystem(tool),
                "file backend does not enforce Console owner isolation"
            );
            let filesystem = Arc::clone(filesystem);
            let path = path.to_owned();
            let tool = tool.to_owned();
            owned_filesystem_read(move || {
                let target = if tool == "file_write" {
                    filesystem.resolve(&path)?
                } else {
                    filesystem.resolve_existing(&path)?
                };
                let (operation, risk, _, _) = crate::security::approval_operation::file_operation(
                    &tool,
                    target.namespace_root(),
                    target.operation_target(),
                );
                Ok::<_, anyhow::Error>((operation, risk))
            })
            .await?
        } else {
            let target = crate::security::approval_operation::resolve_file_target(
                crate::security::path_policy::effective_root(
                    self.generation.effective.runtime.workdir.as_deref(),
                    &self.security,
                ),
                path,
                tool == "file_write",
            )
            .await?;
            anyhow::ensure!(
                self.security
                    .is_resolved_path_allowed(target.parent().context("file parent missing")?),
                "file approval target is outside allowed workspace"
            );
            let (operation, risk, _, _) =
                crate::security::approval_operation::file_operation(tool, &self.security.workspace_dir, &target);
            (operation, risk)
        };
        Ok((
            operation,
            match risk {
                crate::security::policy::ResourceRiskLevel::Low => RiskLevel::Low,
                crate::security::policy::ResourceRiskLevel::Medium => RiskLevel::Medium,
                crate::security::policy::ResourceRiskLevel::High => RiskLevel::High,
            },
        ))
    }

    pub(super) fn verify_console_filesystem_context(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            context
                .engine
                .state
                .trusted
                .parts()
                .constraints
                .iter()
                .any(|code| code.as_str() == "owner_private_filesystem_v1")
                == context.owner_filesystem.is_some()
                && context.engine.state.trusted.parts().owner_isolated() == proof.owner_isolated()
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

    #[tokio::test]
    async fn shell_approval_paths_use_the_execution_root() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let workspace = tempfile::tempdir().expect("test: workspace");
            let root = tempfile::tempdir().expect("test: execution root");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().into();
            config.runtime.workdir = Some(root.path().into());
            config.autonomy.level = crate::security::AutonomyLevel::Supervised;
            config.autonomy.workspace_only = false;
            config.autonomy.forbidden_paths = vec![
                workspace.path().join("state").to_string_lossy().into_owned(),
                root.path().join("blocked").to_string_lossy().into_owned(),
            ];
            config.causal_tree.enabled = false;
            let generation = crate::config::new_shared(config.clone()).pin();
            let catalog = crate::providers::catalog::CatalogView::from_config(&config);
            let service = super::super::tests::disabled_agent_service(&generation, &catalog).await;
            assert!(
                service.check_shell_path_arguments("cat state/x").is_ok(),
                "test: workspace deny entry must not reject a different execution target"
            );
            assert_eq!(
                service
                    .check_shell_path_arguments("cat blocked/x")
                    .expect_err("test: forbidden shell approval")
                    .to_string(),
                "forbidden path argument: blocked/x",
                "test: forbidden execution target is rejected before approval"
            );
            assert!(
                crate::security::SideEffectGate::new(&service.security)
                    .authorize_command_execution_at(Some(root.path()), "shell", "cat state/x", None)
                    .is_ok(),
                "test: command orchestration uses the supplied root"
            );
            assert!(
                service
                    .security
                    .validate_command_execution("cat state/x", false)
                    .is_err(),
                "test: workspace-only callers retain their original target"
            );
            std::fs::create_dir(root.path().join("state")).expect("test: allowed directory");
            std::fs::write(root.path().join("state/x"), "allowed").expect("test: allowed file");
            use crate::tools::Tool as _;
            let tool = crate::tools::shell::ShellTool::new(
                std::sync::Arc::new(crate::runtime::NativeRuntime::new()),
                root.path().into(),
            )
            .with_path_policy(service.security.clone());
            let result = crate::tools::effective_autonomy::scope(
                crate::security::AutonomyLevel::Supervised,
                tool.execute(serde_json::json!({"command":"cat state/x"})),
            )
            .await
            .expect("test: shell result");
            assert!(
                result.success && result.output == "allowed",
                "test: allowed proposal executes the same target: {result:?}"
            );
        })
        .await
        .expect("test: shell approval deadline");
    }

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
            let service = super::super::tests::disabled_agent_service(&generation, &catalog).await;
            std::fs::create_dir(root.path().join("state")).expect("test: state directory");
            let command = "printf x >> state/count";
            service
                .check_shell_path_arguments(command)
                .expect("test: the working-root path is approvable");
            assert!(
                service.check_shell_path_arguments("cat blocked/x").is_err(),
                "test: the check resolves against the working root, not the workspace"
            );
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

    #[tokio::test]
    async fn approval_path_checks_the_execution_root() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let workspace = tempfile::tempdir().expect("test: workspace");
            let root = tempfile::tempdir().expect("test: execution root");
            for level in [
                crate::security::AutonomyLevel::Supervised,
                crate::security::AutonomyLevel::ReadOnly,
            ] {
                let mut config = crate::config::Config::default();
                config.workspace_dir = workspace.path().into();
                config.runtime.workdir = Some(root.path().into());
                config.autonomy.level = level;
                config.autonomy.workspace_only = false;
                config.autonomy.forbidden_paths = vec![root.path().join("blocked").to_string_lossy().into_owned()];
                config.causal_tree.enabled = false;
                let generation = crate::config::new_shared(config.clone()).pin();
                let catalog = crate::providers::catalog::CatalogView::from_config(&config);
                let service = super::super::tests::disabled_agent_service(&generation, &catalog).await;
                let denied = service
                    .scoped_file_operation(None, "file_write", "blocked/target")
                    .await;
                assert_eq!(
                    denied.expect_err("test: forbidden approval must fail").to_string(),
                    "file approval path is outside allowed workspace",
                    "test: approval must use selected root"
                );
                let allowed = service.scoped_file_operation(None, "file_write", "allowed").await;
                assert!(
                    allowed.is_ok(),
                    "test: path resolution permits allowed target; autonomy is checked separately: {allowed:?}"
                );
            }
        })
        .await
        .expect("test: approval path deadline");
    }
}

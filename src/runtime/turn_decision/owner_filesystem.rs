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

    /// The collection, runtime recorder, review and resume all resolve the same
    /// logical path through this function. Resolution creates no directories.
    pub(super) async fn scoped_file_operation(
        &self,
        filesystem: Option<&Arc<OwnerFilesystem>>,
        tool: &str,
        path: &str,
    ) -> anyhow::Result<(String, RiskLevel)> {
        anyhow::ensure!(matches!(tool, "file_write" | "file_edit"), "unsupported file mutation");
        anyhow::ensure!(
            self.security.is_path_allowed(path),
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
                &self.security.workspace_dir,
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

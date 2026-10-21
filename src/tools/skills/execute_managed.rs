//! Execution of lifecycle-managed executable skills (SL11), a part of
//! [`SkillExecuteTool`] kept in its own file.

use super::{
    Arc, CancellationToken, Path, ResourceRiskLevel, SecurityPolicy, SideEffectGate, SkillExecuteTool, Tool,
    ToolResult, trusted_scope_field,
};

/// What the operator's policy grants a managed skill call: the tools the run
/// excludes are not granted, and a tool the security policy denies this
/// caller is not granted. A manifest contributes nothing to it, so a skill
/// cannot grant its own access.
struct PolicyGrant {
    security: Arc<SecurityPolicy>,
    excluded: Vec<String>,
    sender: String,
    channel: String,
    chat_type: String,
}

impl crate::skills::lifecycle::ToolGrant for PolicyGrant {
    fn permits(&self, tool: &str) -> Result<(), String> {
        if self.excluded.iter().any(|excluded| excluded == tool) {
            return Err("it is excluded from this run's tool surface".to_string());
        }
        match self.security.decide(tool, &self.sender, &self.channel, &self.chat_type) {
            crate::security::policy::ToolDecision::Deny => {
                Err("the security policy denies it for this caller".to_string())
            }
            crate::security::policy::ToolDecision::Allow | crate::security::policy::ToolDecision::Ask => Ok(()),
        }
    }
}

impl SkillExecuteTool {
    /// Refuses a call that would take the legacy path for a skill the
    /// lifecycle manages in the configured owner's store: only a skill that is
    /// not managed at all may run unpinned. A store that cannot be read is a
    /// refusal too, never a fallback.
    fn refuse_if_managed(&self, skill_name: &str, tool_name: &str) -> anyhow::Result<()> {
        let Some(configured) = self.config.skills_lifecycle.owner_scope.as_deref() else {
            return Ok(());
        };
        let Ok(owner) = crate::skills::lifecycle::OwnerScope::new(configured) else {
            return Ok(());
        };
        match crate::skills::lifecycle::is_managed_skill(&self.workspace_dir, &owner, skill_name) {
            Ok(false) => Ok(()),
            Ok(true) => Err(anyhow::anyhow!(
                "Managed skill '{skill_name}' tool '{tool_name}' was refused: a lifecycle-managed skill runs only for a caller with a trusted owner scope, pinned to its revision"
            )),
            Err(error) => Err(anyhow::anyhow!(
                "Managed skill '{skill_name}' tool '{tool_name}' was refused: its lifecycle store cannot be read ({error})"
            )),
        }
    }

    /// Runs a declared tool of a lifecycle-managed skill from the revision the
    /// task is pinned to. `Ok(None)` means the skill is not managed for the
    /// caller and the legacy path applies.
    ///
    /// The whole pinned bundle is verified and staged in a private run
    /// directory before anything runs, the policy must grant the tool's kind and
    /// every capability the revision declares, and the call goes through the
    /// same shell/HTTP adapters, under a deadline. The process tree is killed and
    /// reaped by the adapter on completion, timeout and cancellation; the run
    /// directory is removed afterwards (or when the call is dropped).
    pub(super) async fn execute_managed(
        &self,
        skill_name: &str,
        tool_name: &str,
        args: &serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<Option<ToolResult>> {
        use crate::skills::lifecycle;
        let Some(owner) = crate::tools::sessions_read_model::effective_run_owner(args)
            .and_then(|owner_id| lifecycle::OwnerScope::new(&owner_id).ok())
        else {
            // No trusted owner this call could be pinned for. A skill the
            // lifecycle manages never falls back to the unpinned legacy path.
            return self.refuse_if_managed(skill_name, tool_name).map(|()| None);
        };
        let task_id = lifecycle::task_key(
            trusted_scope_field(args, &["task_id"]).as_deref(),
            trusted_scope_field(args, &["run_id"]).as_deref(),
            trusted_scope_field(args, &["parent_run_id"]).as_deref(),
        );
        let caller = |key: &str| trusted_scope_field(args, &[key]).unwrap_or_else(|| "unknown".to_string());
        let grant = PolicyGrant {
            security: Arc::clone(&self.security),
            excluded: self.config.tool_tiering.always_exclude.clone(),
            sender: caller("sender"),
            channel: caller("channel"),
            chat_type: caller("chat_type"),
        };
        let workspace = self.workspace_dir.clone();
        let (skill, tool) = (skill_name.to_string(), tool_name.to_string());
        let call_arguments = args.get("arguments").cloned();
        let (prepared, grant) = tokio::task::spawn_blocking(move || {
            let prepared = lifecycle::prepare_execution(&lifecycle::PrepareRequest {
                workspace_dir: &workspace,
                owner: &owner,
                skill_name: &skill,
                tool_name: &tool,
                task_id: task_id.as_deref(),
                arguments: call_arguments.as_ref(),
                grant: &grant,
            });
            (prepared, grant)
        })
        .await
        .map_err(|error| anyhow::anyhow!("managed skill preparation failed: {error}"))?;
        let prepared = prepared
            .map_err(|error| anyhow::anyhow!("Managed skill '{skill_name}' tool '{tool_name}' was refused: {error}"))?;
        let Some(prepared) = prepared else {
            return self.refuse_if_managed(skill_name, tool_name).map(|()| None);
        };

        // The identity of this run covers the complete bundle, the tool and
        // its argument schema, and the settings, environment and policy it runs
        // under. It names the run in the side-effect audit record.
        let policy = format!(
            "autonomy={:?};caller={}/{}/{};direct_tool={:?};excluded={}",
            self.security.autonomy,
            grant.sender,
            grant.channel,
            grant.chat_type,
            self.security.decide(
                lifecycle::equivalent_direct_tool(&prepared.tool.kind),
                &grant.sender,
                &grant.channel,
                &grant.chat_type
            ),
            grant.excluded.join(",")
        );
        let identity = lifecycle::ExecutionIdentity {
            revision_id: &prepared.pin.revision_id,
            tool: &prepared.tool,
            files: &prepared.files,
            settings: &format!("timeout_ms={}", self.managed_timeout.as_millis()),
            environment: &format!(
                "runtime={};workspace={}",
                self.runtime_name,
                self.workspace_dir.display()
            ),
            policy: &policy,
        }
        .digest();
        let operation = format!(
            "skill_execute:run:{}:{}:{}",
            prepared.pin.skill_id,
            prepared.tool.name,
            identity.get(..16).unwrap_or(&identity)
        );
        // Approval was taken once, by the orchestration boundary, for the
        // `skill_execute` call itself. The gate here refuses a read-only
        // installation and writes the audit record bound to the run identity.
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation(self.name(), &operation, ResourceRiskLevel::Low, None)
            .map_err(|error| anyhow::anyhow!("Managed skill '{skill_name}' tool '{tool_name}' was refused: {error}"))?;

        prepared
            .verify_before_spawn()
            .map_err(|error| anyhow::anyhow!("Managed skill '{skill_name}' tool '{tool_name}' was refused: {error}"))?;
        let root = prepared
            .root()
            .map_or_else(|| self.workspace_dir.clone(), Path::to_path_buf);
        let outcome = self
            .execute_resolved(
                &root,
                &prepared.tool,
                prepared.arguments.clone(),
                cancellation,
                Some(self.managed_timeout),
            )
            .await;
        // The adapter has killed and reaped the process tree and joined its
        // output readers by now, whichever way the call ended.
        let cleanup = prepared.finish();
        match (outcome, cleanup) {
            (Ok(result), Ok(())) => Ok(Some(result)),
            (Ok(_), Err(error)) => Err(anyhow::anyhow!(
                "Managed skill '{skill_name}' tool '{tool_name}' ran but left its run directory: {error}"
            )),
            (Err(error), cleanup) => {
                if let Err(cleanup_error) = cleanup {
                    tracing::warn!(%cleanup_error, "failed to remove a skill run directory after a failed call");
                }
                Err(error)
            }
        }
    }
}

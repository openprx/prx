mod h2;
use super::traits::{Tool, ToolCategory, ToolResult, ToolSpec, ToolTier};
use crate::config::Config;
use crate::runtime::RuntimeAdapter;
use crate::runtime::shell_process::{ShellProcessAdapter, ShellProcessError, ShellProcessRequest};
use crate::security::SecurityPolicy;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_SKILL_RESOURCE_BYTES: u64 = 64 * 1024;
const MAX_SKILL_ALIAS_SPECS: usize = 32;
/// Deadline of a lifecycle-managed executable skill call. A direct shell call
/// has none; a managed one never runs unbounded.
const MANAGED_EXECUTION_TIMEOUT: Duration = Duration::from_mins(5);

fn executable_skill_tool_kind(kind: &str) -> bool {
    matches!(kind.to_ascii_lowercase().as_str(), "shell" | "script" | "http")
}

pub struct SkillsListTool {
    workspace_dir: PathBuf,
    config: Config,
}

impl SkillsListTool {
    pub const fn new(workspace_dir: PathBuf, config: Config) -> Self {
        Self { workspace_dir, config }
    }
}

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List configured PRX skills, their origin, loading mode, and declared tool readiness."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let workspace_skills = normalized_path(&self.workspace_dir.join("skills"));
        let skills = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config);
        let mut records = skills
            .into_iter()
            .map(|skill| {
                let location = skill
                    .location
                    .unwrap_or_else(|| self.workspace_dir.join("skills").join(&skill.name));
                let location_normalized = normalized_path(&location);
                let trusted = location_normalized.starts_with(&workspace_skills);
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "version": skill.version,
                    "origin": if trusted { "workspace" } else { "community" },
                    "instruction_mode": "lazy",
                    "location": location,
                    "declared_tools": skill.tools.iter().map(|tool| json!({
                        "name": tool.name,
                        "kind": tool.kind,
                        "availability": if executable_skill_tool_kind(&tool.kind) {
                            "executable_via_skill_execute"
                        } else {
                            "unsupported_kind"
                        },
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let installed = crate::skills::load_installed_workspace_skills(&self.workspace_dir);
        for state in crate::skills::installed_skill_states(&self.workspace_dir)?
            .into_iter()
            .filter(|state| !state.enabled)
        {
            let skill = installed
                .iter()
                .find(|skill| skill.name.eq_ignore_ascii_case(&state.name));
            records.push(json!({
                "name": state.name,
                "description": skill.map(|skill| skill.description.as_str()).unwrap_or_default(),
                "version": skill.map(|skill| skill.version.as_str()).unwrap_or_default(),
                "origin": "workspace",
                "instruction_mode": "disabled",
                "location": state.path,
                "declared_tools": skill.map(|skill| {
                    skill.tools.iter().map(|tool| json!({
                        "name": tool.name,
                        "kind": tool.kind,
                        "availability": "disabled",
                    })).collect::<Vec<_>>()
                }).unwrap_or_default(),
            }));
        }
        records.sort_by(|left, right| {
            left.get("name")
                .and_then(serde_json::Value::as_str)
                .cmp(&right.get("name").and_then(serde_json::Value::as_str))
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "skills": records,
                "management": {
                    "available": true,
                    "tool": "skills_manage",
                    "actions": ["create", "install", "update", "enable", "disable", "validate", "sync", "remove"]
                }
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System]
    }
}

/// First non-empty string among `keys` of a trusted, runtime-authored scope.
/// Absent for untrusted or direct calls.
fn trusted_scope_field(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
    if !args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let scope = args.get("_zc_scope")?.as_object()?;
    keys.iter().find_map(|key| {
        scope
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ChildSkillReadError {
    #[error("skill is outside the child's authenticated metadata set")]
    Unbound,
    #[error("skill metadata differs from the child's authenticated binding")]
    MetadataChanged,
}

pub struct SkillReadTool {
    workspace_dir: PathBuf,
    config: Config,
    /// Pin lifecycle-managed skills to the revision a task first read. On by
    /// default: `agent::terminal::finalize_turn` releases the task's pins at its
    /// terminal.
    pin_tasks: bool,
    child_metadata: Option<BTreeMap<String, crate::skills::SkillMetadata>>,
}

/// Stable provider-compatible identity. Display names and bound read arguments
/// retain their original spelling; punctuation must not collapse two targets.
pub(crate) fn catalog_skill_read_name(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return name.to_owned();
    }
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"prx-skill-read-public-name-v1\0");
    hash.update(name.as_bytes());
    format!("skill_read_{}", hex::encode(hash.finalize()))
}
pub(crate) fn catalog_skill_read_spec(skill: &crate::skills::SkillMetadata) -> ToolSpec {
    let name = catalog_skill_read_name(&skill.name);
    ToolSpec {
        description: if name == skill.name {
            skill.description.clone()
        } else {
            format!("Read skill {}: {}", skill.name, skill.description)
        },
        name,
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"const": skill.name},
                "resource_path": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

impl SkillReadTool {
    pub const fn new(workspace_dir: PathBuf, config: Config) -> Self {
        Self {
            workspace_dir,
            config,
            pin_tasks: true,
            child_metadata: None,
        }
    }

    /// Turns per-task revision pinning on or off. The task terminal releases
    /// pins (`agent::terminal`); turning pinning on elsewhere requires the same
    /// release.
    #[must_use]
    pub const fn with_task_pinning(mut self, enabled: bool) -> Self {
        self.pin_tasks = enabled;
        self
    }

    /// Restrict a worker reader to its authenticated metadata; terminals never
    /// need to release pins in the parent's lifecycle store.
    pub(crate) fn with_child_metadata(mut self, metadata: Vec<crate::skills::SkillMetadata>) -> Self {
        self.child_metadata = Some(metadata.into_iter().map(|skill| (skill.name.clone(), skill)).collect());
        self.pin_tasks = false;
        self
    }

    fn check_child_binding(&self, current: &crate::skills::SkillMetadata) -> Result<(), ChildSkillReadError> {
        let Some(allowed) = &self.child_metadata else {
            return Ok(());
        };
        let signed = allowed.get(&current.name).ok_or(ChildSkillReadError::Unbound)?;
        if crate::skills::catalog_skill_binding_digest(current) != crate::skills::catalog_skill_binding_digest(signed) {
            return Err(ChildSkillReadError::MetadataChanged);
        }
        Ok(())
    }

    fn check_child_metadata(&self, name: &str) -> Result<(), ChildSkillReadError> {
        let Some(allowed) = &self.child_metadata else {
            return Ok(());
        };
        if !allowed.contains_key(name) {
            return Err(ChildSkillReadError::Unbound);
        }
        let current = crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config)
            .into_iter()
            .find(|skill| skill.name == name)
            .ok_or(ChildSkillReadError::MetadataChanged)?;
        self.check_child_binding(&current)
    }

    fn metadata_for_name(&self, name: &str) -> Option<crate::skills::SkillMetadata> {
        let skills = crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config);
        skills
            .iter()
            .filter(|skill| self.check_child_binding(skill).is_ok())
            .find(|skill| skill.name == name || catalog_skill_read_name(&skill.name) == name)
            .cloned()
    }

    /// Serves a lifecycle-managed skill from its immutable revision instead of
    /// the legacy projection. `Ok(None)` keeps the legacy path: the caller has
    /// no trusted owner, the skill is not in the catalog, or it has no binding
    /// for that owner. Once a binding exists nothing falls back to another
    /// revision or to the projection: a disabled, missing or tampered revision
    /// fails the read.
    async fn read_lifecycle_managed(
        &self,
        name: &str,
        resource_path: Option<&str>,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<ToolResult>> {
        let Some(owner_id) = execute_managed::checked_run_owner("skill_read", args) else {
            return Ok(None);
        };
        let owner = match crate::skills::lifecycle::OwnerScope::new(&owner_id) {
            Ok(owner) => owner,
            Err(error) => {
                tracing::warn!(%error, "skill_read owner id is not a valid lifecycle owner scope; using the legacy path");
                return Ok(None);
            }
        };
        let in_catalog = crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config)
            .iter()
            .any(|skill| skill.name.eq_ignore_ascii_case(name));
        if !in_catalog {
            return Ok(None);
        }
        // The unit the trusted scope belongs to (`run_id`, else `task_id`, the
        // same key the terminal releases);
        // hydration is captured for it, and pinned when pinning is enabled.
        let task_id = crate::skills::lifecycle::task_key(
            trusted_scope_field(args, &["task_id"]).as_deref(),
            trusted_scope_field(args, &["run_id"]).as_deref(),
            trusted_scope_field(args, &["parent_run_id"]).as_deref(),
        );
        // Each identity is recorded as the runtime authored it. The scope
        // carries no segment or child id today; they are read if it ever does.
        let correlation = crate::skills::lifecycle::Correlation {
            run_id: trusted_scope_field(args, &["run_id"]),
            task_id: trusted_scope_field(args, &["task_id"]),
            parent_run_id: trusted_scope_field(args, &["parent_run_id"]),
            segment_id: trusted_scope_field(args, &["segment_id"]),
            child_id: trusted_scope_field(args, &["child_id"]),
        };
        let pin_task = self.pin_tasks;
        let workspace = self.workspace_dir.clone();
        let skill_name = name.to_string();
        let resource = resource_path.map(str::to_string);
        let read = tokio::task::spawn_blocking(move || {
            crate::skills::lifecycle::read_managed_resource(&crate::skills::lifecycle::ManagedReadRequest {
                workspace_dir: &workspace,
                owner: &owner,
                skill_name: &skill_name,
                resource_path: resource.as_deref(),
                task_id: task_id.as_deref(),
                correlation,
                pin_task,
                max_bytes: MAX_SKILL_RESOURCE_BYTES,
            })
        })
        .await
        .map_err(|error| anyhow::anyhow!("lifecycle skill read task failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("Managed skill '{name}' cannot be read: {error}"))?;
        let Some(read) = read else {
            return Ok(None);
        };
        Ok(Some(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "skill": name,
                "resource": read.bundle_dir.join(&read.resource_path),
                "skill_root": read.bundle_dir,
                "trust": if read.untrusted_origin { "community_untrusted" } else { "workspace_trusted" },
                "content": read.content,
                "managed": true,
                "revision_id": read.pin.revision_id.as_str(),
                "generation": read.pin.generation,
                "pinned": read.pinned,
            }))?,
            error: None,
        }))
    }

    fn resolve_target(&self, name: &str, resource_path: Option<&str>) -> anyhow::Result<(PathBuf, PathBuf, bool)> {
        // Resolve through the same metadata-only selection used by capability
        // publication; the legacy process cache can retain an old community
        // target after a workspace shadow is installed.
        let skills = crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config);
        let skill = skills
            .iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("Unknown skill '{name}'. Use skills_list to inspect the catalog."))?;
        let location = &skill.location;
        let skill_root = if location.is_dir() {
            location.clone()
        } else {
            location
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has an invalid location"))?
                .to_path_buf()
        };
        let target = match resource_path.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => {
                let relative = Path::new(path);
                if relative.is_absolute() {
                    anyhow::bail!("resource_path must be relative to the selected skill");
                }
                skill_root.join(relative)
            }
            None => location.clone(),
        };

        let canonical_root = std::fs::canonicalize(&skill_root)
            .map_err(|error| anyhow::anyhow!("Failed to resolve skill root {}: {error}", skill_root.display()))?;
        let canonical_target = std::fs::canonicalize(&target)
            .map_err(|error| anyhow::anyhow!("Failed to resolve skill resource {}: {error}", target.display()))?;
        if !canonical_target.starts_with(&canonical_root) {
            anyhow::bail!("Skill resource escapes its catalog root");
        }
        if !canonical_target.is_file() {
            anyhow::bail!("Skill resource is not a regular file: {}", canonical_target.display());
        }

        let trusted_root = normalized_path(&self.workspace_dir.join("skills"));
        let trusted = canonical_target.starts_with(trusted_root);
        Ok((canonical_target, canonical_root, trusted))
    }
}

#[async_trait]
impl Tool for SkillReadTool {
    fn name(&self) -> &str {
        "skill_read"
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config)
            .into_iter()
            .filter(|skill| self.check_child_binding(skill).is_ok())
            .flat_map(|skill| {
                let digest = crate::skills::catalog_skill_binding_digest(&skill);
                // Synthetic dispatch binds the original `name` argument;
                // native descriptor lookup binds the public alias. Both must
                // resolve to the same immutable metadata, under the same root.
                [
                    (skill.name.clone(), digest),
                    (catalog_skill_read_name(&skill.name), digest),
                ]
            })
            .collect()
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name() || self.metadata_for_name(name).is_some()
    }

    fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
        if name == self.name() {
            return Some(self.spec());
        }
        self.metadata_for_name(name).as_ref().map(catalog_skill_read_spec)
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if name == self.name() {
            return self.execute(args).await;
        }
        let skill = self
            .metadata_for_name(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown or disabled skill '{name}'"))?;
        let mut arguments = args
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Skill read arguments must be an object"))?;
        // The alias schema is closed; enforce the same on the direct path.
        if let Some(unknown) = arguments.keys().find(|key| {
            !matches!(key.as_str(), "name" | "resource_path")
                && !super::execution::RUNTIME_ONLY_ARG_PREFIXES
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
        }) {
            anyhow::bail!("Unexpected skill read argument '{unknown}'");
        }
        if let Some(requested) = arguments.get("name") {
            anyhow::ensure!(
                requested.as_str() == Some(skill.name.as_str()),
                "Skill read alias and requested name differ"
            );
        }
        arguments.insert("name".to_string(), serde_json::Value::String(skill.name));
        self.execute(serde_json::Value::Object(arguments)).await
    }

    fn description(&self) -> &str {
        "Read a catalog-selected skill instruction file or a relative resource without workspace path restrictions."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name from skills_list"},
                "resource_path": {
                    "type": "string",
                    "description": "Optional path relative to the selected skill directory; defaults to its SKILL.md or SKILL.toml"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'name' parameter"))?;
        self.check_child_metadata(name)?;
        let resource_path = args.get("resource_path").and_then(serde_json::Value::as_str);
        if let Some(result) = self.read_lifecycle_managed(name, resource_path, &args).await? {
            return Ok(result);
        }
        let (target, root, trusted) = self.resolve_target(name, resource_path)?;
        let metadata = tokio::fs::metadata(&target).await?;
        if metadata.len() > MAX_SKILL_RESOURCE_BYTES {
            anyhow::bail!(
                "Skill resource {} exceeds the {}-byte limit",
                target.display(),
                MAX_SKILL_RESOURCE_BYTES
            );
        }
        let content = tokio::fs::read_to_string(&target).await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "skill": name,
                "resource": target,
                "skill_root": root,
                "trust": if trusted { "workspace_trusted" } else { "community_untrusted" },
                "content": content,
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        // Every skill advertised in the system context is lazy-loaded through
        // this tool. It is therefore part of the core prompt/tool contract, not
        // an optional intent category.
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System]
    }
}

/// Lifecycle actions an agent may be offered that only read.
pub(crate) const LIFECYCLE_READ_ACTIONS: [&str; 3] = ["lifecycle_status", "lifecycle_list", "lifecycle_inspect"];
/// Lifecycle actions an agent may be offered that mutate. Each one goes
/// through the lifecycle service under the operator's settings; none of them
/// takes a suite, a policy, a mode or a revision body from the caller.
pub(crate) const LIFECYCLE_MUTATING_ACTIONS: [&str; 4] = [
    "lifecycle_publish",
    "lifecycle_cancel",
    "lifecycle_rollback",
    "lifecycle_import_executable",
];
/// Operations that are never an agent action: they choose what judges a
/// proposal, or spend on evaluation, and belong to the operator and the
/// scheduler only.
#[cfg(test)]
pub(crate) const LIFECYCLE_OPERATOR_ONLY: [&str; 8] = [
    "force_close",
    "write_suite",
    "install_suite",
    "begin_evaluation",
    "evaluate",
    "review_source",
    "gate",
    "advance",
];

pub struct SkillsManageTool {
    controlled: Option<Arc<crate::runtime::h2_entry::ControlledH2Profile>>,
    execution_schema: std::sync::OnceLock<Arc<serde_json::Value>>,
    workspace_dir: PathBuf,
    config: Config,
    security: Arc<SecurityPolicy>,
    /// The lifecycle policy owner, when the installation has one. Without it
    /// no lifecycle action is offered or accepted.
    lifecycle: Option<Arc<crate::skills::lifecycle::SkillsLifecycleService>>,
}

impl SkillsManageTool {
    pub const fn new(workspace_dir: PathBuf, config: Config, security: Arc<SecurityPolicy>) -> Self {
        Self {
            workspace_dir,
            config,
            security,
            lifecycle: None,
            controlled: None,
            execution_schema: std::sync::OnceLock::new(),
        }
    }

    /// Builds the tool for an installation: the lifecycle actions are attached
    /// only when `[skills_lifecycle]` is enabled and its store opens. A
    /// disabled lifecycle opens nothing and offers nothing; a lifecycle that
    /// cannot start is logged and not offered.
    pub fn from_config(workspace_dir: PathBuf, config: Config, security: Arc<SecurityPolicy>) -> Self {
        let service = crate::skills::lifecycle_runtime::open_service(&workspace_dir, &config.skills_lifecycle);
        let tool = Self::new(workspace_dir, config, security);
        match service {
            Ok(Some(service)) => tool.with_lifecycle(service),
            Ok(None) => tool,
            Err(error) => {
                tracing::error!(%error, "skill lifecycle is enabled but could not start; its actions are not offered");
                tool
            }
        }
    }

    /// Offers the lifecycle actions, served by `service` (the only way a
    /// lifecycle mutation reaches the store from this tool).
    #[must_use]
    pub fn with_lifecycle(mut self, service: Arc<crate::skills::lifecycle::SkillsLifecycleService>) -> Self {
        self.lifecycle = Some(service);
        self.execution_schema.take();
        self
    }

    pub(crate) fn with_controlled_profile(
        mut self,
        profile: Arc<crate::runtime::h2_entry::ControlledH2Profile>,
    ) -> Self {
        self.controlled = Some(profile);
        self.execution_schema.take();
        self
    }
    fn lifecycle_actions(&self) -> Vec<&'static str> {
        if self.controlled.is_some() {
            return h2::ACTIONS.to_vec();
        }
        if self.lifecycle.is_some() {
            LIFECYCLE_READ_ACTIONS
                .iter()
                .chain(LIFECYCLE_MUTATING_ACTIONS.iter())
                .copied()
                .collect()
        } else {
            Vec::new()
        }
    }

    fn lifecycle_refusal(code: &str, transaction_id: Option<&str>, error: impl Into<String>) -> ToolResult {
        let output = transaction_id.map_or_else(
            || json!({"lifecycle_code": code}),
            |id| json!({"lifecycle_code": code, "transaction_id": id}),
        );
        ToolResult {
            success: false,
            output: output.to_string(),
            error: Some(error.into()),
        }
    }

    fn lifecycle_error_code(error: &crate::skills::lifecycle::ServiceError) -> (&'static str, Option<String>) {
        use crate::skills::lifecycle::{LifecycleError, ServiceError};
        match error {
            ServiceError::Lifecycle(LifecycleError::CommittedRepairRequired { transaction_id, .. }) => {
                ("committed_repair_required", Some(transaction_id.clone()))
            }
            ServiceError::Lifecycle(
                LifecycleError::StaleGeneration { .. } | LifecycleError::RevisionAlreadyActive(_),
            ) => ("stale_generation", None),
            ServiceError::Lifecycle(
                LifecycleError::TransactionConflict { .. } | LifecycleError::ProposalConflict { .. },
            ) => ("conflict", None),
            ServiceError::NoSuchProposal(_)
            | ServiceError::NoGateDecision(_)
            | ServiceError::Lifecycle(LifecycleError::RevisionNotFound(_) | LifecycleError::RollbackTargetUnknown(_)) => {
                ("not_found", None)
            }
            ServiceError::Lifecycle(LifecycleError::OwnerMismatch { .. }) => ("wrong_owner", None),
            ServiceError::Lifecycle(LifecycleError::InvalidId { .. } | LifecycleError::InvalidOwnerScope(_)) => {
                ("invalid", None)
            }
            ServiceError::Disabled | ServiceError::ModeForbids { .. } => ("unavailable", None),
            ServiceError::Busy { .. } => ("busy", None),
            _ => ("lifecycle_failed", None),
        }
    }

    async fn execute_lifecycle(&self, action: &str, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        use crate::skills::lifecycle::{RevisionId, ServiceError, SkillId};
        let Some(service) = self.lifecycle.clone() else {
            anyhow::bail!("Unsupported skills_manage action: {action}");
        };
        let optional = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let action_name = action.to_string();
        let work: anyhow::Result<Result<serde_json::Value, ServiceError>> = match action {
            "lifecycle_status" => {
                tokio::task::spawn_blocking(move || service.status().map(|status| json!(status))).await
            }
            "lifecycle_list" => {
                let skill = optional("skill_id").map(|id| SkillId::parse(&id)).transpose()?;
                tokio::task::spawn_blocking(move || {
                    service
                        .list_proposals(skill.as_ref())
                        .map(|proposals| json!({"proposals": proposals}))
                })
                .await
            }
            "lifecycle_inspect" => {
                let id = Self::required_string(args, "proposal_id")?.to_string();
                tokio::task::spawn_blocking(move || service.inspect_proposal(&id).map(|detail| json!(detail))).await
            }
            "lifecycle_publish" => {
                let id = Self::required_string(args, "proposal_id")?.to_string();
                tokio::task::spawn_blocking(move || {
                    service.publish(&id).map(|outcome| {
                        json!({
                            "proposal_id": outcome.proposal.proposal_id,
                            "status": outcome.proposal.status.as_str(),
                            "revision_id": outcome.transaction.entry.candidate_revision_id.as_str(),
                            "generation": outcome.transaction.entry.new_generation,
                        })
                    })
                })
                .await
            }
            "lifecycle_cancel" => {
                let id = Self::required_string(args, "proposal_id")?.to_string();
                let reason = optional("reason").unwrap_or_else(|| "cancelled through skills_manage".to_string());
                tokio::task::spawn_blocking(move || {
                    service.cancel(&id, &reason).map(
                        |proposal| json!({"proposal_id": proposal.proposal_id, "status": proposal.status.as_str()}),
                    )
                })
                .await
            }
            "lifecycle_rollback" => {
                let skill = SkillId::parse(Self::required_string(args, "skill_id")?)?;
                let revision = RevisionId::parse(Self::required_string(args, "revision_id")?)?;
                let generation = args
                    .get("expected_generation")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("Missing non-negative integer 'expected_generation' parameter"))?;
                tokio::task::spawn_blocking(move || {
                    service.rollback(&skill, &revision, generation).map(|outcome| {
                        json!({
                            "skill_id": outcome.entry.skill_id.as_str(),
                            "revision_id": outcome.entry.candidate_revision_id.as_str(),
                            "generation": outcome.entry.new_generation,
                            "replayed": outcome.replayed,
                        })
                    })
                })
                .await
            }
            "lifecycle_import_executable" => {
                // The operator reviewed this directory as exactly this bundle
                // hash; the service refuses a directory that hashes otherwise.
                let source = PathBuf::from(Self::required_string(args, "source")?);
                let source = if source.is_absolute() {
                    source
                } else {
                    self.workspace_dir.join(source)
                };
                let revision = RevisionId::parse(Self::required_string(args, "revision_id")?)?;
                tokio::task::spawn_blocking(move || {
                    service.import_executable(&source, &revision).map(|import| {
                        json!({
                            "skill_id": import.skill_id.as_str(),
                            "revision_id": import.revision_id.as_str(),
                            "generation": import.generation,
                            "replaced": import.replaced.as_ref().map(RevisionId::as_str),
                        })
                    })
                })
                .await
            }
            _ => anyhow::bail!("Unsupported skills_manage action: {action}"),
        }
        .map_err(|error| anyhow::anyhow!("lifecycle worker failed: {error}"));
        Ok(match work? {
            Ok(mut output) => {
                if let Some(object) = output.as_object_mut() {
                    object.insert("action".to_string(), json!(action_name));
                }
                ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                    error: None,
                }
            }
            Err(error) => {
                let (code, transaction_id) = Self::lifecycle_error_code(&error);
                let text = if error.is_retryable() {
                    format!("{error} (retryable)")
                } else {
                    error.to_string()
                };
                Self::lifecycle_refusal(code, transaction_id.as_deref(), text)
            }
        })
    }

    /// The grant the runtime attached, with the trusted caller principal bound
    /// from the trusted scope (a signed v2 grant is honoured only for it).
    fn approval_grant(args: &serde_json::Value) -> Option<ApprovalGrant> {
        ApprovalGrant::from_runtime_args("skills_manage", args)
    }

    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), ToolResult> {
        if LIFECYCLE_READ_ACTIONS.contains(&action) {
            // Status and listing change nothing.
            return Ok(());
        }
        let risk = match action {
            "enable" | "disable" | "validate" => ResourceRiskLevel::Low,
            "lifecycle_publish" | "lifecycle_rollback" | "lifecycle_import_executable" => ResourceRiskLevel::High,
            _ => ResourceRiskLevel::Medium,
        };
        let arg = |key: &str| args.get(key).and_then(serde_json::Value::as_str).map(str::trim);
        // A lifecycle mutation binds its target into the operation a grant
        // authorizes, so a grant for one proposal (or one rollback) cannot be
        // replayed against another.
        let target = match action {
            "lifecycle_publish" | "lifecycle_cancel" => arg("proposal_id").map(str::to_string),
            // The expected generation is part of the bound operation, so a
            // grant for one observed state cannot roll back another.
            "lifecycle_rollback" => match (
                arg("skill_id"),
                arg("revision_id"),
                args.get("expected_generation").and_then(serde_json::Value::as_u64),
            ) {
                (Some(skill), Some(revision), Some(generation)) => Some(format!("{skill}:{revision}:{generation}")),
                _ => None,
            },
            // The complete bundle hash the operator reviewed is part of the
            // operation: a grant for one reviewed artifact cannot publish
            // another, and an edited script is a different hash.
            "lifecycle_import_executable" => match (arg("source"), arg("revision_id")) {
                (Some(source), Some(revision)) => Some(format!("{source}:{revision}")),
                _ => None,
            },
            _ => None,
        };
        let operation = target.as_ref().map_or_else(
            || format!("skills_manage:{action}"),
            |target| format!("skills_manage:{action}:{target}"),
        );
        let grant = Self::approval_grant(args);
        if (crate::tools::effective_autonomy::call_policy(&self.security).autonomy
            != crate::security::AutonomyLevel::Full
            || args.get("_zc_approval_grant").is_some())
            && matches!(
                action,
                "lifecycle_publish" | "lifecycle_rollback" | "lifecycle_import_executable"
            )
        {
            // Supervised publication needs an exact one-shot grant. Full
            // follows the operator's execution policy without a forced Ask.
            let now = chrono::Utc::now().timestamp();
            // Only a signed one-shot (v2) grant qualifies: it expires and can be
            // spent once, which a v1 hash grant cannot.
            let granted = target.is_some()
                && grant.as_ref().is_some_and(|grant| {
                    grant.v2.is_some() && grant.permits_resource_operation(self.name(), &operation, risk, now)
                });
            if !granted {
                return Err(Self::lifecycle_refusal(
                    "approval_required",
                    None,
                    format!("{action} requires an explicit approval grant bound to its target ({operation})"),
                ));
            }
        }
        SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
            .authorize_resource_operation(self.name(), &operation, risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| {
                let code = if error.to_ascii_lowercase().contains("read-only") {
                    "read_only"
                } else {
                    "policy_denied"
                };
                Self::lifecycle_refusal(code, None, error)
            })?;
        // Supervised spends its grant in the gate. Full needs no grant, but
        // an explicitly supplied one-shot must still reject replay. Consume
        // it after the gate allows the call. The ledger is in memory:
        // a restart forgets it, which is acceptable because runtime grants
        // live about a minute, a repeated publish is idempotent by proposal
        // state, and a repeated rollback fails its expected_generation CAS.
        if matches!(
            action,
            "lifecycle_publish" | "lifecycle_rollback" | "lifecycle_import_executable"
        ) && crate::tools::effective_autonomy::call_policy(&self.security).autonomy
            == crate::security::AutonomyLevel::Full
            && grant.is_some()
            && !grant.as_ref().is_some_and(ApprovalGrant::consume_single_use)
        {
            return Err(Self::lifecycle_refusal(
                "approval_required",
                None,
                format!("{action} approval grant was already used (replay refused)"),
            ));
        }
        Ok(())
    }

    /// The owner the runtime vouches for, `None` when it does not: no trusted
    /// scope, an incomplete one, or an owner-isolated caller without its own
    /// owner id (such a caller never gets an owner composed for it).
    fn lifecycle_caller_owner(args: &serde_json::Value) -> Option<String> {
        let isolated_without_owner = args.get("_zc_scope_trusted").and_then(serde_json::Value::as_bool) == Some(true)
            && args
                .get("_zc_scope")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|scope| {
                    scope.get("owner_isolated").and_then(serde_json::Value::as_bool) == Some(true)
                        && scope
                            .get("owner_id")
                            .and_then(serde_json::Value::as_str)
                            .is_none_or(|owner| owner.trim().is_empty())
                });
        if isolated_without_owner {
            return None;
        }
        execute_managed::checked_run_owner("skills_manage", args)
    }

    fn required_string<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty '{key}' parameter"))
    }
}

impl SkillsManageTool {
    fn execution_schema(&self) -> &std::sync::Arc<serde_json::Value> {
        self.execution_schema.get_or_init(|| {
            if self.controlled.is_some() {return std::sync::Arc::new(h2::parameters_schema());}
            std::sync::Arc::new({
                let mut actions = vec![
                    "create", "install", "update", "enable", "disable", "validate", "sync", "remove",
                ];
                if self.controlled.is_some() {actions.clear();}
                let lifecycle = self.lifecycle_actions();
                actions.extend(lifecycle.iter().copied());
                let mut action_description = "Skill action: 'create' writes a new instruction skill, 'install' adds one from a source, 'update' refreshes an installed one, 'enable' and 'disable' toggle one, 'validate' checks one, 'sync' synchronizes the community skill repositories, 'remove' deletes one".to_string();
                if self.controlled.is_some() {action_description="Controlled instruction experiment intents. Exact source, decision approval, immutable admission and resource accounting are enforced by the runtime and lifecycle service.".into();}
                else if !lifecycle.is_empty() {
                    action_description.push_str(", 'lifecycle_status' reports the lifecycle mode and proposal counts, 'lifecycle_list' lists proposals, 'lifecycle_inspect' shows one proposal, 'lifecycle_publish' publishes an eligible proposal, 'lifecycle_cancel' closes a proposal, 'lifecycle_rollback' re-activates an earlier revision, 'lifecycle_import_executable' publishes a reviewed executable bundle");
                }
                let mut schema = json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": actions,
                            "description": action_description
                        },
                        "name": {"type": "string", "description": "Workspace skill name; required for 'create', 'update', 'enable', 'disable', 'validate' and 'remove'"},
                        "source": {"type": "string", "description": "Git URL or explicit local directory; required for 'install'. For 'lifecycle_import_executable', the directory of the reviewed executable bundle"},
                        "description": {"type": "string", "description": "Short description of the skill; required for 'create'"},
                        "instructions": {"type": "string", "description": "SKILL.md instructions; required for 'create'"}
                    },
                    "required": ["action"],
                    "additionalProperties": false
                });
                if !lifecycle.is_empty()
                    && let Some(properties) = schema.get_mut("properties").and_then(serde_json::Value::as_object_mut)
                {
                    if self.controlled.is_some() {properties.insert("request".into(),json!({"type":"object","description":"Exact typed experiment intent; conveys no entry authority"}));}
                    properties.insert("proposal_id".to_string(), json!({"type": "string", "description": "Lifecycle proposal id; required for 'lifecycle_inspect', 'lifecycle_publish' and 'lifecycle_cancel'"}));
                    properties.insert("skill_id".to_string(), json!({"type": "string", "description": "Lifecycle skill id; optional filter for 'lifecycle_list', required for 'lifecycle_rollback'"}));
                    properties.insert("revision_id".to_string(), json!({"type": "string", "description": "An earlier published revision id, required for 'lifecycle_rollback'; for 'lifecycle_import_executable' the complete bundle hash the operator reviewed"}));
                    properties.insert("expected_generation".to_string(), json!({"type": "integer", "minimum": 0, "description": "Active generation the caller observed; required for 'lifecycle_rollback'"}));
                    properties.insert(
                "reason".to_string(),
                json!({"type": "string", "description": "Why the proposal is closed; optional for 'lifecycle_cancel'"}),
            );
                }
                let mut requirements = vec![
                    crate::tools::schema::ActionRequirement {
                        action: "create",
                        required: &["name", "description", "instructions"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "install",
                        required: &["source"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "update",
                        required: &["name"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "enable",
                        required: &["name"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "disable",
                        required: &["name"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "validate",
                        required: &["name"],
                    },
                    crate::tools::schema::ActionRequirement {
                        action: "remove",
                        required: &["name"],
                    },
                ];
                if !lifecycle.is_empty() {
                    requirements.extend([
                        crate::tools::schema::ActionRequirement {
                            action: "lifecycle_inspect",
                            required: &["proposal_id"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "lifecycle_publish",
                            required: &["proposal_id"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "lifecycle_cancel",
                            required: &["proposal_id"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "lifecycle_rollback",
                            required: &["skill_id", "revision_id", "expected_generation"],
                        },
                        crate::tools::schema::ActionRequirement {
                            action: "lifecycle_import_executable",
                            required: &["source", "revision_id"],
                        },
                    ]);
                }
                if self.controlled.is_some() {
                    if let Some(properties)=schema.get_mut("properties").and_then(serde_json::Value::as_object_mut) {
                        properties.retain(|key,_|matches!(key.as_str(),"action"|"request"));
                    }
                    requirements=h2::ACTIONS.iter().map(|action|crate::tools::schema::ActionRequirement {
                        action,required:&["request"],
                    }).collect();
                }
                crate::tools::schema::with_action_requirements(schema, "action", &requirements)
            })
        })
    }
}

#[async_trait]
impl Tool for SkillsManageTool {
    fn name(&self) -> &str {
        "skills_manage"
    }

    fn description(&self) -> &str {
        if self.controlled.is_some() {
            return h2::DESCRIPTION;
        }
        if self.lifecycle.is_some() {
            "Manage skill lifecycle: import reviewed executable bundles, publish proposals, and roll back revisions. Supervised lifecycle writes require exact operator approval; Full executes under the configured policy. Also 'create', 'install', 'update', 'enable', 'disable', 'validate', 'sync' and 'remove' instruction skills. Lifecycle actions: 'lifecycle_status', 'lifecycle_list' and 'lifecycle_inspect' only read and must use their same-named read-only capabilities instead of this mixed-effect root; 'lifecycle_publish' publishes an eligible proposal, 'lifecycle_cancel' closes one and 'lifecycle_rollback' re-activates an earlier revision and 'lifecycle_import_executable' publishes an operator-reviewed executable bundle from a directory, bound to the complete bundle hash the operator reviewed, all under the operator's lifecycle mode and policy. Mutating actions follow the configured execution and approval policy."
        } else {
            "Manage PRX skills: 'create' writes a new instruction skill, 'install' adds one from a source, 'update' refreshes an installed one, 'enable' and 'disable' toggle one, 'validate' checks one, 'sync' synchronizes the community skill repositories, and 'remove' deletes one. Mutating actions follow the configured execution and approval policy."
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    fn execution_parameters_schema(&self) -> Option<std::sync::Arc<serde_json::Value>> {
        Some(std::sync::Arc::clone(self.execution_schema()))
    }

    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![self.spec()];
        if self.lifecycle.is_some() && self.controlled.is_none() {
            specs.extend(LIFECYCLE_READ_ACTIONS.into_iter().map(|action| {
                let mut properties = serde_json::Map::from_iter([("action".into(), json!({"type":"string","enum":[action]}))]);
                let mut required = vec!["action"];
                if action == "lifecycle_inspect" {
                    properties.insert("proposal_id".into(), json!({"type":"string"}));
                    required.push("proposal_id");
                } else if action == "lifecycle_list" {
                    properties.insert("skill_id".into(), json!({"type":"string"}));
                }
                ToolSpec {
                    name: action.into(),
                    description: format!("Read-only execution of skills_manage with action {action} for the current owner. This alias completes that skills_manage operation; do not rediscover or call the mixed-effect root to repeat the read. No publication or workspace mutation."),
                    parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
                }
            }));
        }
        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name() || (self.lifecycle.is_some() && LIFECYCLE_READ_ACTIONS.contains(&name))
    }

    async fn execute_named(&self, name: &str, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if name != self.name() {
            let spec = self
                .spec_for_name(name)
                .ok_or_else(|| anyhow::anyhow!("unknown lifecycle read capability"))?;
            crate::tools::schema::validate_execution_contract(name, &spec.parameters, &mut args)?;
            anyhow::ensure!(
                args.get("action").and_then(serde_json::Value::as_str) == Some(name),
                "lifecycle read action changed"
            );
        }
        self.execute_request(args).await
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_request(args).await
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::Automation]
    }
}

impl SkillsManageTool {
    async fn execute_request(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), self.execution_schema(), &mut args)?;
        let action = Self::required_string(&args, "action")?;
        if let Some(profile) = &self.controlled {
            profile.verify(&self.config)?;
            crate::runtime::h2_entry::EntryAction::parse(action)?;
        }
        if action.starts_with("lifecycle_experiment_") {
            return self.execute_h2(action, &args).await;
        }
        if crate::tools::execution::scope_requires_owner_match(&args)
            && !matches!(
                action,
                "lifecycle_status" | "lifecycle_list" | "lifecycle_inspect" | "lifecycle_cancel"
            )
        {
            return Ok(Self::lifecycle_refusal(
                "owner_isolated_write",
                None,
                "owner-isolated callers may inspect or cancel their own lifecycle proposals; shared-workspace actions require the local operator",
            ));
        }
        if action.starts_with("lifecycle_") {
            if !self.lifecycle_actions().contains(&action) {
                anyhow::bail!("Unsupported skills_manage action: {action}");
            }
            // The store belongs to one owner scope. Every lifecycle action,
            // reads included (they expose the owner's proposals), needs a
            // caller the runtime knows to be that owner: another owner, and a
            // caller whose owner is unknown, never reach it and do not spend
            // a grant trying.
            if let Some(service) = self.lifecycle.as_ref() {
                let configured = service.owner_scope().as_str();
                let refusal = match Self::lifecycle_caller_owner(&args) {
                    Some(caller) if caller == configured => None,
                    // This install's own scheduler acts as the install owner.
                    _ if super::sessions_read_model::is_install_scheduler_caller(&args) => None,
                    Some(caller) => Some((
                        "wrong_owner",
                        format!(
                            "the skill lifecycle store belongs to another owner scope (configured owner_scope: {configured}; this caller's owner id: {caller})"
                        ),
                    )),
                    None => Some((
                        "unknown_owner",
                        format!(
                            "the skill lifecycle needs a caller with a known owner scope (configured owner_scope: {configured}; the runtime could not derive one for this caller)"
                        ),
                    )),
                };
                if let Some((code, error)) = refusal {
                    return Ok(Self::lifecycle_refusal(code, None, error));
                }
            }
            if let Err(result) = self.authorize(action, &args) {
                return Ok(result);
            }
            return self.execute_lifecycle(action, &args).await;
        }
        if let Err(result) = self.authorize(action, &args) {
            return Ok(result);
        }

        let result = match action {
            "create" => {
                let name = Self::required_string(&args, "name")?;
                let description = Self::required_string(&args, "description")?;
                let instructions = Self::required_string(&args, "instructions")?;
                crate::skills::create_instruction_skill(&self.workspace_dir, name, description, instructions)
                    .map(|path| json!({"action": action, "name": name, "path": path}))
            }
            "install" => {
                let source = Self::required_string(&args, "source")?;
                crate::skills::install_skill_from_source(&self.workspace_dir, source)
                    .await
                    .map(|path| json!({"action": action, "path": path}))
            }
            "update" => {
                let name = Self::required_string(&args, "name")?;
                crate::skills::update_installed_skill(&self.workspace_dir, name)
                    .await
                    .map(|path| json!({"action": action, "name": name, "path": path}))
            }
            "enable" | "disable" => {
                let name = Self::required_string(&args, "name")?;
                let enabled = action == "enable";
                crate::skills::set_installed_skill_enabled(&self.workspace_dir, name, enabled)
                    .map(|path| json!({"action": action, "name": name, "enabled": enabled, "path": path}))
            }
            "validate" => {
                let name = Self::required_string(&args, "name")?;
                crate::skills::validate_installed_skill(&self.workspace_dir, name)
                    .map(|path| json!({"action": action, "name": name, "valid": true, "path": path}))
            }
            "sync" => {
                let mut contacted: Vec<String> = Vec::new();
                crate::skills::sync_community_skill_repositories(&self.config, &mut |line| {
                    contacted.push(line.to_string());
                })
                .await
                .map(|synced| json!({"action": action, "synchronized": synced > 0, "repositories": synced, "log": contacted}))
            }
            "remove" => {
                let name = Self::required_string(&args, "name")?;
                crate::skills::remove_installed_skill(&self.workspace_dir, name)
                    .map(|()| json!({"action": action, "name": name, "removed": true}))
            }
            _ => anyhow::bail!("Unsupported skills_manage action: {action}"),
        };

        Ok(match result {
            Ok(output) => ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&output)?,
                error: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
            },
        })
    }
}

mod execute_managed;

pub struct SkillExecuteTool {
    workspace_dir: PathBuf,
    config: Config,
    shell: ShellProcessAdapter,
    http: super::HttpRequestTool,
    security: Arc<SecurityPolicy>,
    runtime_name: String,
    managed_timeout: Duration,
}

impl SkillExecuteTool {
    pub fn new(
        workspace_dir: PathBuf,
        config: Config,
        runtime: Arc<dyn RuntimeAdapter>,
        security: Arc<SecurityPolicy>,
        browser_config: &crate::config::BrowserConfig,
        http_config: &crate::config::HttpRequestConfig,
    ) -> Self {
        let runtime_name = runtime.name().to_string();
        let shell = Self::process_adapter(runtime, &config.runtime);
        Self {
            workspace_dir,
            config,
            shell,
            http: super::HttpRequestTool::new(
                Arc::clone(&security),
                browser_config.allowed_domains.clone(),
                http_config.max_response_size,
                http_config.timeout_secs,
            ),
            security,
            runtime_name,
            managed_timeout: MANAGED_EXECUTION_TIMEOUT,
        }
    }

    /// Overrides the deadline of a lifecycle-managed executable call.
    #[must_use]
    pub const fn with_managed_timeout(mut self, timeout: Duration) -> Self {
        self.managed_timeout = timeout;
        self
    }

    fn alias_component(value: &str) -> String {
        value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn alias_name(skill: &str, tool: &str) -> String {
        format!(
            "skill__{}__{}",
            Self::alias_component(skill),
            Self::alias_component(tool)
        )
    }

    fn declared_tools(&self) -> Vec<(crate::skills::Skill, crate::skills::SkillTool)> {
        let mut tools = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config)
            .into_iter()
            .flat_map(|skill| {
                let cloned = skill.clone();
                skill.tools.into_iter().map(move |tool| (cloned.clone(), tool))
            })
            .filter(|(_, tool)| executable_skill_tool_kind(&tool.kind))
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| {
            Self::alias_name(&left.0.name, &left.1.name).cmp(&Self::alias_name(&right.0.name, &right.1.name))
        });
        tools.dedup_by(|left, right| {
            Self::alias_name(&left.0.name, &left.1.name) == Self::alias_name(&right.0.name, &right.1.name)
        });
        tools
    }

    fn resolve(&self, skill_name: &str, tool_name: &str) -> anyhow::Result<(PathBuf, crate::skills::SkillTool)> {
        let skill = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config)
            .into_iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(skill_name))
            .ok_or_else(|| anyhow::anyhow!("Unknown or disabled skill '{skill_name}'"))?;
        let tool = skill
            .tools
            .iter()
            .find(|tool| tool.name.eq_ignore_ascii_case(tool_name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Skill '{skill_name}' has no declared tool '{tool_name}'"))?;
        let location = skill
            .location
            .ok_or_else(|| anyhow::anyhow!("Skill '{skill_name}' has no local location"))?;
        let root = if location.is_dir() {
            location
        } else {
            location
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Skill '{skill_name}' has an invalid location"))?
                .to_path_buf()
        };
        Ok((std::fs::canonicalize(root)?, tool))
    }

    fn scalar_arguments(
        tool: &crate::skills::SkillTool,
        arguments: Option<&serde_json::Value>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut values = tool
            .args
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        if let Some(arguments) = arguments {
            let object = arguments
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Skill tool arguments must be an object"))?;
            for (key, value) in object {
                let value = match value {
                    serde_json::Value::String(value) => value.clone(),
                    serde_json::Value::Number(value) => value.to_string(),
                    serde_json::Value::Bool(value) => value.to_string(),
                    serde_json::Value::Null => String::new(),
                    _ => anyhow::bail!("Skill tool argument '{key}' must be a scalar"),
                };
                values.insert(key.clone(), value);
            }
        }
        Ok(values)
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn environment_prefix(arguments: &BTreeMap<String, String>) -> String {
        let assignments = arguments
            .iter()
            .map(|(key, value)| {
                let key = key
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() {
                            ch.to_ascii_uppercase()
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>();
                format!("PRX_SKILL_ARG_{key}={}", Self::shell_quote(value))
            })
            .collect::<Vec<_>>()
            .join(" ");
        if assignments.is_empty() {
            String::new()
        } else {
            format!("export {assignments};")
        }
    }

    async fn execute_resolved(
        &self,
        root: &Path,
        tool: &crate::skills::SkillTool,
        arguments: BTreeMap<String, String>,
        cancellation: Option<CancellationToken>,
        // `Some` for a lifecycle-managed run: it has a deadline and is joined.
        timeout: Option<Duration>,
    ) -> anyhow::Result<ToolResult> {
        if tool.command.trim().is_empty() {
            anyhow::bail!("Skill tool '{}' has an empty command", tool.name);
        }
        if tool.kind.eq_ignore_ascii_case("http") {
            let mut request = serde_json::Map::new();
            request.insert("url".to_string(), json!(tool.command));
            request.insert(
                "method".to_string(),
                json!(arguments.get("method").map_or("GET", String::as_str)),
            );
            if let Some(body) = arguments.get("body") {
                request.insert("body".to_string(), json!(body));
            }
            let headers = arguments
                .iter()
                .filter_map(|(key, value)| key.strip_prefix("header.").map(|name| (name.to_string(), json!(value))))
                .collect::<serde_json::Map<_, _>>();
            request.insert("headers".to_string(), serde_json::Value::Object(headers));
            return self
                .http
                .execute_with_cancellation(serde_json::Value::Object(request), cancellation)
                .await;
        }

        let prefix = self.with_call_location(Self::environment_prefix(&arguments));
        let command = if tool.kind.eq_ignore_ascii_case("shell") {
            if prefix.is_empty() {
                tool.command.clone()
            } else {
                format!("{prefix} {}", tool.command)
            }
        } else if tool.kind.eq_ignore_ascii_case("script") {
            let relative = Path::new(tool.command.trim());
            if relative.is_absolute() {
                anyhow::bail!("Skill script command must be relative to the skill root");
            }
            let script = std::fs::canonicalize(root.join(relative))?;
            if !script.starts_with(root) || !script.is_file() {
                anyhow::bail!("Skill script escapes its skill root or is not a file");
            }
            let mut command = String::new();
            if !prefix.is_empty() {
                command.push_str(&prefix);
                command.push(' ');
            }
            command.push_str(&Self::shell_quote(&script.display().to_string()));
            for (key, value) in &arguments {
                command.push(' ');
                command.push_str(&Self::shell_quote(&format!("--{key}")));
                command.push(' ');
                command.push_str(&Self::shell_quote(value));
            }
            command
        } else {
            anyhow::bail!(
                "Unsupported skill tool kind '{}'; supported: shell, script, http",
                tool.kind
            );
        };

        let request = ShellProcessRequest {
            command: &command,
            workspace_dir: root,
            timeout,
            cancellation,
        };
        // A deadline marks a managed run, which is also joined: the process
        // group ends with its leader.
        let outcome = if timeout.is_some() {
            let full = !self.security.evaluation_sandbox
                && crate::tools::effective_autonomy::call_policy(&self.security).autonomy
                    == crate::security::AutonomyLevel::Full;
            if full {
                self.shell.execute_joined_full(request).await
            } else {
                self.shell.execute_joined(request).await
            }
        } else {
            self.shell.execute(request).await
        };
        match outcome {
            Ok(outcome) => Ok(ToolResult {
                success: outcome.status.success(),
                output: outcome.stdout,
                error: (!outcome.status.success()).then_some(outcome.stderr),
            }),
            Err(ShellProcessError::Cancelled) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(super::traits::TOOL_EXECUTION_CANCELLED.to_string()),
            }),
            Err(ShellProcessError::Timeout(limit)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Skill tool timed out after {limit:?} and was killed")),
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn parse_alias(&self, name: &str) -> Option<(String, String)> {
        self.declared_tools().into_iter().find_map(|(skill, tool)| {
            (Self::alias_name(&skill.name, &tool.name) == name).then_some((skill.name, tool.name))
        })
    }
}

#[async_trait]
impl Tool for SkillExecuteTool {
    fn name(&self) -> &str {
        "skill_execute"
    }

    fn description(&self) -> &str {
        "Execute a declared shell, script, or HTTP tool from an enabled PRX skill. Scalar arguments are exposed as PRX_SKILL_ARG_* variables."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let tools = self.declared_tools();
        let skills = tools.iter().map(|(skill, _)| skill.name.clone()).collect::<Vec<_>>();
        let names = tools.iter().map(|(_, tool)| tool.name.clone()).collect::<Vec<_>>();
        json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string", "enum": skills},
                "tool": {"type": "string", "enum": names},
                "arguments": {"type": "object", "description": "Scalar tool arguments", "default": {}}
            },
            "required": ["skill", "tool"],
            "additionalProperties": false
        })
    }

    fn specs(&self) -> Vec<super::traits::ToolSpec> {
        let mut specs = vec![self.spec()];
        specs.extend(
            self.declared_tools()
                .into_iter()
                .take(MAX_SKILL_ALIAS_SPECS)
                .map(|(skill, tool)| super::traits::ToolSpec {
                    name: Self::alias_name(&skill.name, &tool.name),
                    description: format!("{} (skill: {})", tool.description, skill.name),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "arguments": {"type": "object", "description": "Scalar tool arguments", "default": {}}
                        },
                        "additionalProperties": false
                    }),
                }),
        );
        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name() || self.parse_alias(name).is_some()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named_with_cancellation(self.name(), args, None).await
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named_with_cancellation(name, args, None).await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let (skill_name, tool_name) = if name == self.name() {
            (
                args.get("skill")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'skill' parameter"))?
                    .to_string(),
                args.get("tool")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'tool' parameter"))?
                    .to_string(),
            )
        } else {
            self.parse_alias(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown skill tool alias '{name}'"))?
        };
        if let Some(result) = self
            .execute_managed(&skill_name, &tool_name, &args, cancellation.clone())
            .await?
        {
            return Ok(result);
        }
        let (root, tool) = self.resolve(&skill_name, &tool_name)?;
        self.refuse_projection(&root, &skill_name, &tool_name)?;
        let arguments = Self::scalar_arguments(&tool, args.get("arguments"))?;
        self.execute_resolved(&root, &tool, arguments, cancellation, None).await
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        // Enabled skill tools must remain callable when the user asks for the
        // skill's domain task without literally saying "skill" or "automation".
        &[]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        let count = self
            .declared_tools()
            .iter()
            .filter(|(_, tool)| executable_skill_tool_kind(&tool.kind))
            .count();
        if count == 0 {
            crate::capability::CapabilityAvailability::declared("no enabled skill tools are installed")
        } else {
            crate::capability::CapabilityAvailability::ready(format!(
                "{count} executable skill tool backend(s) are registered"
            ))
        }
    }
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod executable_tests;

#[cfg(test)]
#[path = "skills/child_reader_tests.rs"]
mod child_reader_tests;

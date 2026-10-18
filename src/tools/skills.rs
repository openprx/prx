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
use tokio_util::sync::CancellationToken;

const MAX_SKILL_RESOURCE_BYTES: u64 = 64 * 1024;
const MAX_SKILL_ALIAS_SPECS: usize = 32;

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

pub struct SkillReadTool {
    workspace_dir: PathBuf,
    config: Config,
    /// Pin lifecycle-managed skills to the revision a task first read. On by
    /// default: `agent::terminal::finalize_turn` releases the task's pins at its
    /// terminal.
    pin_tasks: bool,
}

pub(crate) fn catalog_skill_read_spec(skill: &crate::skills::SkillMetadata) -> ToolSpec {
    ToolSpec {
        name: skill.name.clone(),
        description: skill.description.clone(),
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

    fn metadata_for_name(&self, name: &str) -> Option<crate::skills::SkillMetadata> {
        crate::skills::catalog_skill_metadata(&self.workspace_dir, &self.config)
            .into_iter()
            .find(|skill| skill.name == name)
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
        let Some(owner_id) = super::sessions_read_model::effective_run_owner(args) else {
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
            .map(|skill| (skill.name.clone(), crate::skills::catalog_skill_binding_digest(&skill)))
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
pub(crate) const LIFECYCLE_MUTATING_ACTIONS: [&str; 3] =
    ["lifecycle_publish", "lifecycle_cancel", "lifecycle_rollback"];
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
        self
    }

    fn lifecycle_actions(&self) -> Vec<&'static str> {
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
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(if error.is_retryable() {
                    format!("{error} (retryable)")
                } else {
                    error.to_string()
                }),
            },
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
            "lifecycle_publish" | "lifecycle_rollback" => ResourceRiskLevel::High,
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
            _ => None,
        };
        let operation = target.as_ref().map_or_else(
            || format!("skills_manage:{action}"),
            |target| format!("skills_manage:{action}:{target}"),
        );
        let grant = Self::approval_grant(args);
        if matches!(action, "lifecycle_publish" | "lifecycle_rollback") {
            // Publication changes what every future task reads, so it needs an
            // explicit grant bound to its target in every autonomy level, also
            // under full autonomy where the gate would otherwise let it pass.
            let now = chrono::Utc::now().timestamp();
            // Only a signed one-shot (v2) grant qualifies: it expires and can be
            // spent once, which a v1 hash grant cannot.
            let granted = target.is_some()
                && grant.as_ref().is_some_and(|grant| {
                    grant.v2.is_some() && grant.permits_resource_operation(self.name(), &operation, risk, now)
                });
            if !granted {
                return Err(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "{action} requires an explicit approval grant bound to its target ({operation})"
                    )),
                });
            }
        }
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation(self.name(), &operation, risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })?;
        // Replay ledger. Under `supervised` the gate above has just spent the
        // grant; under `full` it lets the call through without looking at the
        // grant, so spend it here, after the gate allowed the call (a call the
        // gate refuses must not burn the approval). The ledger is in memory:
        // a restart forgets it, which is acceptable because runtime grants
        // live about a minute, a repeated publish is idempotent by proposal
        // state, and a repeated rollback fails its expected_generation CAS.
        if matches!(action, "lifecycle_publish" | "lifecycle_rollback")
            && self.security.autonomy == crate::security::AutonomyLevel::Full
            && !grant.as_ref().is_some_and(ApprovalGrant::consume_single_use)
        {
            return Err(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("{action} approval grant was already used (replay refused)")),
            });
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
        super::sessions_read_model::effective_run_owner(args)
    }

    fn required_string<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty '{key}' parameter"))
    }
}

#[async_trait]
impl Tool for SkillsManageTool {
    fn name(&self) -> &str {
        "skills_manage"
    }

    fn description(&self) -> &str {
        if self.lifecycle.is_some() {
            "Manage PRX skills: 'create' writes a new instruction skill, 'install' adds one from a source, 'update' refreshes an installed one, 'enable' and 'disable' toggle one, 'validate' checks one, 'sync' synchronizes the community skill repositories, and 'remove' deletes one. Lifecycle proposals of learned revisions: 'lifecycle_status', 'lifecycle_list' and 'lifecycle_inspect' only read; 'lifecycle_publish' publishes an eligible proposal, 'lifecycle_cancel' closes one and 'lifecycle_rollback' re-activates an earlier revision, all under the operator's lifecycle mode and policy. Mutating actions are approval-gated."
        } else {
            "Manage PRX skills: 'create' writes a new instruction skill, 'install' adds one from a source, 'update' refreshes an installed one, 'enable' and 'disable' toggle one, 'validate' checks one, 'sync' synchronizes the community skill repositories, and 'remove' deletes one. Mutating actions are approval-gated."
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut actions = vec![
            "create", "install", "update", "enable", "disable", "validate", "sync", "remove",
        ];
        let lifecycle = self.lifecycle_actions();
        actions.extend(lifecycle.iter().copied());
        let mut action_description = "Skill action: 'create' writes a new instruction skill, 'install' adds one from a source, 'update' refreshes an installed one, 'enable' and 'disable' toggle one, 'validate' checks one, 'sync' synchronizes the community skill repositories, 'remove' deletes one".to_string();
        if !lifecycle.is_empty() {
            action_description.push_str(", 'lifecycle_status' reports the lifecycle mode and proposal counts, 'lifecycle_list' lists proposals, 'lifecycle_inspect' shows one proposal, 'lifecycle_publish' publishes an eligible proposal, 'lifecycle_cancel' closes a proposal, 'lifecycle_rollback' re-activates an earlier revision");
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
                "source": {"type": "string", "description": "Git URL or explicit local directory; required for 'install'"},
                "description": {"type": "string", "description": "Short description of the skill; required for 'create'"},
                "instructions": {"type": "string", "description": "SKILL.md instructions; required for 'create'"}
            },
            "required": ["action"],
            "additionalProperties": false
        });
        if !lifecycle.is_empty()
            && let Some(properties) = schema.get_mut("properties").and_then(serde_json::Value::as_object_mut)
        {
            properties.insert("proposal_id".to_string(), json!({"type": "string", "description": "Lifecycle proposal id; required for 'lifecycle_inspect', 'lifecycle_publish' and 'lifecycle_cancel'"}));
            properties.insert("skill_id".to_string(), json!({"type": "string", "description": "Lifecycle skill id; optional filter for 'lifecycle_list', required for 'lifecycle_rollback'"}));
            properties.insert("revision_id".to_string(), json!({"type": "string", "description": "An earlier published revision id; required for 'lifecycle_rollback'"}));
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
            ]);
        }
        crate::tools::schema::with_action_requirements(schema, "action", &requirements)
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = Self::required_string(&args, "action")?;
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
                let refusal = match Self::lifecycle_caller_owner(&args) {
                    Some(caller) if caller == service.owner_scope().as_str() => None,
                    Some(_) => Some("the skill lifecycle store belongs to another owner scope"),
                    None => Some("the skill lifecycle needs a caller with a known owner scope"),
                };
                if let Some(error) = refusal {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error.to_string()),
                    });
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
            "sync" => crate::skills::sync_community_skill_repositories(&self.config)
                .await
                .map(|()| json!({"action": action, "synchronized": true})),
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

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::Automation]
    }
}

pub struct SkillExecuteTool {
    workspace_dir: PathBuf,
    config: Config,
    shell: ShellProcessAdapter,
    http: super::HttpRequestTool,
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
        Self {
            workspace_dir,
            config,
            shell: ShellProcessAdapter::new(runtime),
            http: super::HttpRequestTool::new(
                security,
                browser_config.allowed_domains.clone(),
                http_config.max_response_size,
                http_config.timeout_secs,
            ),
        }
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
        arguments: Option<&serde_json::Value>,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let arguments = Self::scalar_arguments(tool, arguments)?;
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

        let prefix = Self::environment_prefix(&arguments);
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

        let outcome = self
            .shell
            .execute(ShellProcessRequest {
                command: &command,
                workspace_dir: root,
                timeout: None,
                cancellation,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;
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
        let (root, tool) = self.resolve(&skill_name, &tool_name)?;
        self.execute_resolved(&root, &tool, args.get("arguments"), cancellation)
            .await
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(workspace: &Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.skills.open_skills_dir = Some(workspace.join("missing-open-skills").to_string_lossy().to_string());
        config.skills.openclaw_skills_dir = Some(workspace.join("missing-openclaw").to_string_lossy().to_string());
        config
    }

    /// The shared action contract every action-dispatched tool is held to:
    /// the tool-list description and the `action` parameter's own description
    /// must each name every enumerated action, and no parameter may ship
    /// undescribed.
    #[test]
    fn skills_manage_action_tool_contract() {
        let workspace = TempDir::new().unwrap();
        let tool = SkillsManageTool::new(
            workspace.path().to_path_buf(),
            test_config(workspace.path()),
            Arc::new(SecurityPolicy::default()),
        );
        crate::tools::schema::action_contract::assert_action_tool_contract(
            tool.name(),
            tool.description(),
            &tool.parameters_schema(),
        );
    }

    #[test]
    fn skill_read_is_a_core_prompt_dependency() {
        let workspace = TempDir::new().unwrap();
        let tool = SkillReadTool::new(workspace.path().to_path_buf(), test_config(workspace.path()));

        assert_eq!(tool.tier(), ToolTier::Core);
    }

    #[tokio::test]
    async fn skill_read_reads_catalog_selected_resource_and_blocks_escape() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Demo\n\nDo the demo.").unwrap();
        std::fs::write(skill_dir.join("reference.txt"), "reference data").unwrap();
        std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
        let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        let result = tool
            .execute(json!({"name": "demo", "resource_path": "reference.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("reference data"));

        let error = tool
            .execute(json!({"name": "demo", "resource_path": "../../secret.txt"}))
            .await
            .expect_err("resource escape must fail");
        assert!(error.to_string().contains("escapes"));
    }

    #[test]
    fn skill_read_target_tracks_workspace_shadow_even_with_legacy_cache_primed() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let community = temp.path().join("community");
        let openclaw = temp.path().join("openclaw");
        std::fs::create_dir_all(&community).unwrap();
        std::fs::write(community.join("prx_skill.md"), "# Community\nCommunity body\n").unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.skills.open_skills_dir = Some(community.to_string_lossy().to_string());
        config.skills.openclaw_skills_dir = Some(openclaw.to_string_lossy().to_string());
        let _legacy = crate::skills::load_skills_with_config(&workspace, &config);
        let workspace_skill = workspace.join("skills/prx_skill/SKILL.md");
        std::fs::create_dir_all(workspace_skill.parent().unwrap()).unwrap();
        std::fs::write(&workspace_skill, "# Workspace\nWorkspace body\n").unwrap();
        let reader = SkillReadTool::new(workspace.clone(), config.clone());
        let (target, _, _) = reader.resolve_target("prx_skill", None).unwrap();
        assert_eq!(target, std::fs::canonicalize(&workspace_skill).unwrap());
        let selected = crate::skills::catalog_skill_metadata(&workspace, &config);
        assert_eq!(selected.first().unwrap().location, workspace_skill);
    }

    #[tokio::test]
    async fn skills_list_marks_supported_manifest_tools_as_executable() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='run'\ndescription='run it'\nkind='shell'\ncommand='echo ok'\n",
        )
        .unwrap();
        let tool = SkillsListTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("executable_via_skill_execute"));
        assert!(result.output.contains("\"instruction_mode\": \"lazy\""));
    }

    #[tokio::test]
    async fn skills_manage_create_disable_enable_and_remove_are_operational() {
        let temp = TempDir::new().unwrap();
        let config = test_config(temp.path());
        let tool = SkillsManageTool::new(
            temp.path().to_path_buf(),
            config.clone(),
            Arc::new(SecurityPolicy::default()),
        );

        let created = tool
            .execute(json!({
                "action": "create",
                "name": "managed",
                "description": "managed skill",
                "instructions": "Follow the managed workflow."
            }))
            .await
            .unwrap();
        assert!(created.success, "{:?}", created.error);
        assert_eq!(crate::skills::load_skills_with_config(temp.path(), &config).len(), 1);

        let disabled = tool
            .execute(json!({"action": "disable", "name": "managed"}))
            .await
            .unwrap();
        assert!(disabled.success, "{:?}", disabled.error);
        assert!(crate::skills::load_skills_with_config(temp.path(), &config).is_empty());
        let states = crate::skills::installed_skill_states(temp.path()).unwrap();
        assert_eq!(states.len(), 1);
        assert!(!states.first().unwrap().enabled);

        let enabled = tool
            .execute(json!({"action": "enable", "name": "managed"}))
            .await
            .unwrap();
        assert!(enabled.success, "{:?}", enabled.error);
        assert_eq!(crate::skills::load_skills_with_config(temp.path(), &config).len(), 1);

        let removed = tool
            .execute(json!({"action": "remove", "name": "managed"}))
            .await
            .unwrap();
        assert!(removed.success, "{:?}", removed.error);
        assert!(crate::skills::installed_skill_states(temp.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn skills_manage_resolves_manifest_name_when_install_directory_differs() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        let source = temp.path().join("repository-name");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.toml"),
            "[skill]\nname='logical-name'\ndescription='logical skill'\n",
        )
        .unwrap();
        let config = test_config(&workspace);
        let tool = SkillsManageTool::new(workspace.clone(), config, Arc::new(SecurityPolicy::default()));

        let installed = tool
            .execute(json!({"action": "install", "source": source}))
            .await
            .unwrap();
        assert!(installed.success, "{:?}", installed.error);
        let disabled = tool
            .execute(json!({"action": "disable", "name": "logical-name"}))
            .await
            .unwrap();
        assert!(disabled.success, "{:?}", disabled.error);
        let removed = tool
            .execute(json!({"action": "remove", "name": "logical-name"}))
            .await
            .unwrap();
        assert!(removed.success, "{:?}", removed.error);
        assert!(!workspace.join("skills/repository-name").exists());
    }

    #[tokio::test]
    async fn skill_execute_exports_alias_and_runs_declared_shell_tool() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='run'\ndescription='run it'\nkind='shell'\ncommand='printf %s \"$PRX_SKILL_ARG_MESSAGE\"'\n",
        )
        .unwrap();
        let config = test_config(temp.path());
        let runtime: Arc<dyn RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
        let tool = SkillExecuteTool::new(
            temp.path().to_path_buf(),
            config,
            runtime,
            Arc::new(SecurityPolicy::default()),
            &crate::config::BrowserConfig::default(),
            &crate::config::HttpRequestConfig::default(),
        );

        assert!(tool.supports_name("skill__demo__run"));
        assert!(tool.specs().iter().any(|spec| spec.name == "skill__demo__run"));
        let catalog = crate::tools::ToolCatalog::from_tools([&tool as &dyn Tool]);
        assert_eq!(
            catalog.descriptor("skill__demo__run").unwrap().adapter,
            crate::tools::ToolAdapterKind::Skill
        );
        let result = tool
            .execute_named("skill__demo__run", json!({"arguments": {"message": "operational"}}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, "operational");
    }

    fn scoped_args(owner: &str, task: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut args = extra;
        if let Some(object) = args.as_object_mut() {
            object.insert("_zc_scope_trusted".to_string(), json!(true));
            object.insert("_zc_scope".to_string(), json!({"owner_id": owner, "task_id": task}));
        }
        args
    }

    fn skill_md(body: &str) -> String {
        format!("---\nname: demo\ndescription: Demo skill\n---\n\n{body}\n")
    }

    #[tokio::test]
    async fn skill_read_serves_lifecycle_revisions_pinned_per_task() {
        use crate::skills::lifecycle::{
            Bundle, BundleEntry, OwnerScope, SkillId, SkillLifecycleStore, SourceKind, apply_migration, plan_migration,
        };
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md("legacy body")).unwrap();
        let owner = "owner:ws:cli:alice";
        let store = SkillLifecycleStore::open(temp.path(), OwnerScope::new(owner).unwrap()).unwrap();
        let plan = plan_migration(temp.path(), Some(&store)).unwrap();
        let report = apply_migration(&store, temp.path(), &plan);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path())).with_task_pinning(true);
        let unpinned_tool =
            SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path())).with_task_pinning(false);

        let read = |args: serde_json::Value| {
            let tool = &tool;
            async move { serde_json::from_str::<serde_json::Value>(&tool.execute(args).await.unwrap().output).unwrap() }
        };
        let first = read(scoped_args(owner, "task-tool-1", json!({"name": "demo"}))).await;
        assert!(first.get("content").unwrap().as_str().unwrap().contains("legacy body"));
        assert_eq!(first.get("managed").unwrap(), &json!(true));
        assert_eq!(first.get("pinned").unwrap(), &json!(true));
        let revision_a = first.get("revision_id").unwrap().as_str().unwrap().to_string();

        // Publish revision B for the same skill id and move the binding.
        let id = SkillId::from_legacy_dir("demo").unwrap();
        let bundle = Bundle::from_entries(vec![BundleEntry {
            path: "SKILL.md".into(),
            bytes: skill_md("revised body").into_bytes(),
            executable: false,
        }])
        .unwrap();
        crate::skills::lifecycle::test_support::publish_revision(&store, &id, SourceKind::Learned, &bundle, 1);

        let same_task = read(scoped_args(owner, "task-tool-1", json!({"name": "demo"}))).await;
        assert!(
            same_task
                .get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("legacy body")
        );
        assert_eq!(same_task.get("revision_id").unwrap().as_str().unwrap(), revision_a);
        let new_task = read(scoped_args(owner, "task-tool-2", json!({"name": "demo"}))).await;
        assert!(
            new_task
                .get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("revised body")
        );
        assert_eq!(new_task.get("generation").unwrap(), &json!(2));

        // Production default: no task pinning, so every read follows the binding.
        let unpinned: serde_json::Value = serde_json::from_str(
            &unpinned_tool
                .execute(scoped_args(owner, "task-tool-1", json!({"name": "demo"})))
                .await
                .unwrap()
                .output,
        )
        .unwrap();
        assert!(
            unpinned
                .get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("revised body")
        );
        assert_eq!(unpinned.get("pinned").unwrap(), &json!(false));

        // Callers without a trusted owner, and other owners, keep the legacy
        // projection exactly as before.
        let legacy = read(json!({"name": "demo"})).await;
        assert!(legacy.get("content").unwrap().as_str().unwrap().contains("legacy body"));
        assert!(legacy.get("managed").is_none());
        let other = read(scoped_args("owner:ws:cli:bob", "task-tool-3", json!({"name": "demo"}))).await;
        assert!(other.get("managed").is_none());

        // Escapes are still refused for managed skills.
        let escape = tool
            .execute(scoped_args(
                owner,
                "task-tool-1",
                json!({"name": "demo", "resource_path": "../../x"}),
            ))
            .await
            .expect_err("managed resource escape must fail");
        assert!(escape.to_string().contains("cannot be read"));
        let owner_scope = OwnerScope::new(owner).unwrap();
        for task in ["task-tool-1", "task-tool-2", "task-tool-3"] {
            crate::skills::lifecycle::release_task_pins(&owner_scope, task);
        }
    }

    #[tokio::test]
    async fn skill_read_alias_schema_is_closed_and_paths_stay_confined() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md("body")).unwrap();
        std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("secret.txt"), skill_dir.join("link.txt")).unwrap();
        let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        let metadata = crate::skills::catalog_skill_metadata(temp.path(), &test_config(temp.path()));
        let spec = catalog_skill_read_spec(metadata.first().unwrap());
        assert_eq!(spec.parameters.get("additionalProperties").unwrap(), &json!(false));
        assert!(spec.parameters.pointer("/properties/resource_path").is_some());

        let junk = tool
            .execute_named("demo", json!({"name": "demo", "extra": 1}))
            .await
            .expect_err("unknown alias argument must fail");
        assert!(junk.to_string().contains("Unexpected skill read argument"));
        let read = tool
            .execute_named("demo", json!({"name": "demo", "resource_path": "SKILL.md"}))
            .await
            .unwrap();
        assert!(read.success);

        for bad in ["../../secret.txt", "/etc/passwd", "link.txt"] {
            let error = tool
                .execute_named("demo", json!({"name": "demo", "resource_path": bad}))
                .await
                .expect_err("resource must stay inside the skill directory");
            let text = error.to_string();
            assert!(text.contains("escapes") || text.contains("relative"), "{bad}: {text}");
        }
    }

    #[tokio::test]
    async fn skill_read_records_every_trusted_scope_identity_on_its_receipt() {
        use crate::skills::lifecycle::{Correlation, OwnerScope, SkillLifecycleStore, apply_migration, plan_migration};
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md("body")).unwrap();
        let owner = "owner:ws:cli:alice";
        let store = SkillLifecycleStore::open(temp.path(), OwnerScope::new(owner).unwrap()).unwrap();
        let plan = plan_migration(temp.path(), Some(&store)).unwrap();
        let report = apply_migration(&store, temp.path(), &plan);
        assert_eq!(report.failed, Vec::new());
        let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        // The scope exactly as the execution service authors it (task, run and parent).
        let args = json!({
            "name": "demo",
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "owner_id": owner,
                "task_id": "task-corr",
                "run_id": "run-corr",
                "parent_run_id": "run-parent"
            }
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.success);

        // The experience is keyed by the run (a child is never keyed by the task).
        let record = store.read_experience("run-corr").unwrap().unwrap();
        let expected = Correlation {
            run_id: Some("run-corr".into()),
            task_id: Some("task-corr".into()),
            parent_run_id: Some("run-parent".into()),
            segment_id: None,
            child_id: None,
        };
        assert_eq!(record.correlation, expected);
        assert_eq!(
            record.hydration_receipts.first().map(|receipt| &receipt.correlation),
            Some(&expected)
        );
        crate::skills::lifecycle::release_task_pins(&OwnerScope::new(owner).unwrap(), "run-corr");
    }

    fn lifecycle_tool(workspace: &Path, mode: crate::skills::lifecycle::LifecycleMode) -> SkillsManageTool {
        lifecycle_tool_with(workspace, mode, Arc::new(SecurityPolicy::default()))
    }

    fn lifecycle_tool_with(
        workspace: &Path,
        mode: crate::skills::lifecycle::LifecycleMode,
        security: Arc<SecurityPolicy>,
    ) -> SkillsManageTool {
        use crate::skills::lifecycle::{
            OwnerScope, SkillLifecycleStore, SkillsLifecycleService, SkillsLifecycleSettings,
        };
        let store = SkillLifecycleStore::open(workspace, OwnerScope::new("owner:ws:cli:alice").unwrap()).unwrap();
        let settings = SkillsLifecycleSettings {
            enabled: true,
            mode,
            ..SkillsLifecycleSettings::default()
        };
        let service = SkillsLifecycleService::new(Arc::new(store), settings).unwrap();
        SkillsManageTool::new(workspace.to_path_buf(), test_config(workspace), security)
            .with_lifecycle(Arc::new(service))
    }

    fn offered_actions(tool: &SkillsManageTool) -> Vec<String> {
        tool.parameters_schema()
            .pointer("/properties/action/enum")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect()
    }

    /// The actions the schema offers are exactly the legacy ones, plus the
    /// lifecycle ones when a service is attached, and never an operator-only
    /// operation or a parameter that lets the caller choose the suite, the
    /// policy or the mode.
    #[test]
    fn lifecycle_actions_offered_match_the_policy_owner_split() {
        let workspace = TempDir::new().unwrap();
        let legacy = [
            "create", "install", "update", "enable", "disable", "validate", "sync", "remove",
        ];
        let plain = SkillsManageTool::new(
            workspace.path().to_path_buf(),
            test_config(workspace.path()),
            Arc::new(SecurityPolicy::default()),
        );
        assert_eq!(offered_actions(&plain), legacy);
        crate::tools::schema::action_contract::assert_action_tool_contract(
            plain.name(),
            plain.description(),
            &plain.parameters_schema(),
        );

        let tool = lifecycle_tool(workspace.path(), crate::skills::lifecycle::LifecycleMode::Shadow);
        let mut expected: Vec<String> = legacy.iter().map(|a| (*a).to_string()).collect();
        expected.extend(LIFECYCLE_READ_ACTIONS.iter().map(|a| (*a).to_string()));
        expected.extend(LIFECYCLE_MUTATING_ACTIONS.iter().map(|a| (*a).to_string()));
        let offered = offered_actions(&tool);
        assert_eq!(offered, expected);
        for forbidden in LIFECYCLE_OPERATOR_ONLY {
            assert!(
                !offered.iter().any(|action| action.contains(forbidden)),
                "{forbidden} must never be offered"
            );
        }
        let schema = tool.parameters_schema();
        let properties = schema.get("properties").and_then(serde_json::Value::as_object).unwrap();
        for forbidden in [
            "suite",
            "suite_digest",
            "policy",
            "mode",
            "settings",
            "candidate",
            "bundle",
            "path",
        ] {
            assert!(
                !properties.contains_key(forbidden),
                "{forbidden} must not be a parameter"
            );
        }
        crate::tools::schema::action_contract::assert_action_tool_contract(tool.name(), tool.description(), &schema);
    }

    #[tokio::test]
    async fn lifecycle_actions_are_read_through_the_service_and_unknown_ones_are_refused() {
        let workspace = TempDir::new().unwrap();
        let tool = lifecycle_tool(workspace.path(), crate::skills::lifecycle::LifecycleMode::Shadow);

        let status = tool
            .execute(owned(json!({"action": "lifecycle_status"})))
            .await
            .unwrap();
        assert!(status.success, "{:?}", status.error);
        let status: serde_json::Value = serde_json::from_str(&status.output).unwrap();
        assert_eq!(status.get("mode").and_then(serde_json::Value::as_str), Some("shadow"));
        assert!(
            status
                .get("publication_block")
                .and_then(serde_json::Value::as_str)
                .is_some()
        );

        let listed = tool.execute(owned(json!({"action": "lifecycle_list"}))).await.unwrap();
        assert!(listed.success, "{:?}", listed.error);
        assert!(listed.output.contains("\"proposals\": []"));

        // Operator-only operations are not actions at all.
        for forbidden in [
            "write_suite",
            "begin_evaluation",
            "lifecycle_begin_evaluation",
            "lifecycle_write_suite",
        ] {
            assert!(tool.execute(json!({"action": forbidden})).await.is_err(), "{forbidden}");
        }

        // A mutation reaches the service and is refused by the mode.
        let publish = tool
            .execute(with_grant(
                json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
                &format!("skills_manage:lifecycle_publish:{PROPOSAL}"),
            ))
            .await
            .unwrap();
        assert!(!publish.success);
        assert!(
            publish.error.unwrap().contains("does not allow"),
            "mode must refuse publication"
        );
        let rollback = tool
            .execute(owned(json!({
                "action": "lifecycle_rollback",
                "skill_id": "release-notes",
                "revision_id": format!("r1-{}", "0".repeat(64)),
                "expected_generation": 1
            })))
            .await
            .unwrap();
        assert!(!rollback.success);
    }

    #[tokio::test]
    async fn lifecycle_actions_are_unavailable_without_a_service() {
        let workspace = TempDir::new().unwrap();
        let tool = SkillsManageTool::new(
            workspace.path().to_path_buf(),
            test_config(workspace.path()),
            Arc::new(SecurityPolicy::default()),
        );
        for action in LIFECYCLE_READ_ACTIONS.iter().chain(LIFECYCLE_MUTATING_ACTIONS.iter()) {
            assert!(
                tool.execute(json!({"action": action, "proposal_id": "x"}))
                    .await
                    .is_err(),
                "{action}"
            );
        }
    }

    /// A signed one-shot grant for `operation`, as the agent loop issues it.
    fn grant_for(operation: &str) -> serde_json::Value {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
        let risk = if operation.contains(":lifecycle_cancel:") {
            RiskLevel::Medium
        } else {
            RiskLevel::High
        };
        let grant = ApprovalGrantV2::issue_one_shot(
            WitnessKeyring::global_for_tests(),
            Subject {
                agent_id: "prx:test:agent".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:ws:cli:alice".to_string(),
                workspace_id: "ws".to_string(),
                session_key: Some("session".to_string()),
            },
            IssuerAuthority::HumanReview,
            operation,
            risk,
        )
        .unwrap();
        serde_json::to_value(crate::security::policy::ApprovalGrant::from_verified_v2(
            "skills_manage",
            "test",
            grant,
        ))
        .unwrap()
    }

    /// Attaches `grant` and the trusted caller principal it is bound to.
    fn attach_grant(mut args: serde_json::Value, grant: serde_json::Value) -> serde_json::Value {
        if let Some(object) = args.as_object_mut() {
            object.insert(crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(), grant);
            object.insert("_zc_scope_trusted".to_string(), json!(true));
            object.insert(
                "_zc_scope".to_string(),
                json!({"principal_id": "telegram:alice", "owner_id": "owner:ws:cli:alice"}),
            );
        }
        args
    }

    /// A call from the configured owner, with no grant.
    fn owned(mut args: serde_json::Value) -> serde_json::Value {
        if let Some(object) = args.as_object_mut() {
            object.insert("_zc_scope_trusted".to_string(), json!(true));
            object.insert("_zc_scope".to_string(), json!({"owner_id": "owner:ws:cli:alice"}));
        }
        args
    }

    fn with_grant(args: serde_json::Value, operation: &str) -> serde_json::Value {
        attach_grant(args, grant_for(operation))
    }

    const PROPOSAL: &str = "prop-00000000000000000000000000000000";
    const OTHER_PROPOSAL: &str = "prop-11111111111111111111111111111111";

    /// Publication and rollback need a grant bound to their target in every
    /// autonomy level; cancel needs the bound grant when supervised.
    #[tokio::test]
    async fn lifecycle_mutations_need_grants_bound_to_their_target() {
        use crate::security::AutonomyLevel;
        let revision = format!("r1-{}", "0".repeat(64));
        let rollback_args = json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": 1});
        let rollback_op = format!("skills_manage:lifecycle_rollback:release-notes:{revision}:1");
        let publish_args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let cancel_args = json!({"action": "lifecycle_cancel", "proposal_id": PROPOSAL});
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let workspace = TempDir::new().unwrap();
            let security = Arc::new(SecurityPolicy {
                autonomy,
                workspace_dir: workspace.path().to_path_buf(),
                ..SecurityPolicy::default()
            });
            let tool = lifecycle_tool_with(
                workspace.path(),
                crate::skills::lifecycle::LifecycleMode::Shadow,
                security,
            );

            // No grant: refused before the service is reached.
            for args in [&publish_args, &rollback_args] {
                let result = tool.execute(owned(args.clone())).await.unwrap();
                assert!(!result.success);
                assert!(
                    result
                        .error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("explicit approval grant"),
                    "{autonomy:?}: {:?}",
                    result.error
                );
            }
            // A grant bound to another proposal or target is refused too.
            for (args, operation) in [
                (
                    &publish_args,
                    format!("skills_manage:lifecycle_publish:{OTHER_PROPOSAL}"),
                ),
                (&publish_args, "skills_manage:lifecycle_publish".to_string()),
                (
                    &rollback_args,
                    "skills_manage:lifecycle_rollback:release-notes:r1-other:1".to_string(),
                ),
                // The grant binds the generation the caller observed.
                (
                    &rollback_args,
                    format!("skills_manage:lifecycle_rollback:release-notes:{revision}:2"),
                ),
                (
                    &rollback_args,
                    format!("skills_manage:lifecycle_rollback:release-notes:{revision}"),
                ),
            ] {
                let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
                assert!(!result.success);
                assert!(
                    result
                        .error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("explicit approval grant"),
                    "{autonomy:?} {operation}: {:?}",
                    result.error
                );
            }
            // The bound grant reaches the service, which the mode then refuses.
            for (args, operation) in [
                (&publish_args, format!("skills_manage:lifecycle_publish:{PROPOSAL}")),
                (&rollback_args, rollback_op.clone()),
            ] {
                let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
                assert!(!result.success);
                assert!(
                    result.error.as_deref().unwrap_or_default().contains("does not allow"),
                    "{autonomy:?} {operation}: {:?}",
                    result.error
                );
            }
        }

        // Supervised cancel: refused without a grant, accepted with the bound one.
        let workspace = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = lifecycle_tool_with(
            workspace.path(),
            crate::skills::lifecycle::LifecycleMode::Shadow,
            security,
        );
        let refused = tool.execute(owned(cancel_args.clone())).await.unwrap();
        assert!(!refused.success);
        assert!(
            refused.error.as_deref().unwrap_or_default().contains("approval grant"),
            "{:?}",
            refused.error
        );
        let wrong = tool
            .execute(with_grant(
                cancel_args.clone(),
                &format!("skills_manage:lifecycle_cancel:{OTHER_PROPOSAL}"),
            ))
            .await
            .unwrap();
        assert!(
            wrong.error.as_deref().unwrap_or_default().contains("approval grant"),
            "{:?}",
            wrong.error
        );
        let reached = tool
            .execute(with_grant(
                cancel_args,
                &format!("skills_manage:lifecycle_cancel:{PROPOSAL}"),
            ))
            .await
            .unwrap();
        assert!(!reached.success);
        assert!(
            reached
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no such proposal"),
            "the bound grant must reach the service: {:?}",
            reached.error
        );
    }

    /// What `grant_op_for_call` mints for `args`: the exact operation and
    /// whether the grant is issued at high risk.
    fn minted(args: &serde_json::Value) -> Option<(String, bool)> {
        use crate::acl::approval_grant::RiskLevel;
        use crate::security::approval_operation::{GrantOp, grant_op_for_call};
        match grant_op_for_call("skills_manage", args)? {
            GrantOp::ResourceExact(operation, risk) => Some((operation, risk == RiskLevel::High)),
            GrantOp::Command(_) | GrantOp::ResourceGlob(..) => None,
        }
    }

    fn supervised_tool(workspace: &Path, autonomy: crate::security::AutonomyLevel) -> SkillsManageTool {
        lifecycle_tool_with(
            workspace,
            crate::skills::lifecycle::LifecycleMode::Shadow,
            Arc::new(SecurityPolicy {
                autonomy,
                workspace_dir: workspace.to_path_buf(),
                ..SecurityPolicy::default()
            }),
        )
    }

    /// The operation the runtime mints for a call is exactly the one the tool
    /// checks: a grant for the minted operation gets the call to the service,
    /// in both gated autonomy levels.
    #[tokio::test]
    async fn the_runtime_minted_operations_are_the_ones_the_tool_checks() {
        use crate::security::AutonomyLevel;
        let revision = format!("r1-{}", "0".repeat(64));
        let cases = [
            (
                json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
                true,
                "does not allow",
            ),
            (
                json!({"action": "lifecycle_cancel", "proposal_id": PROPOSAL}),
                false,
                "no such proposal",
            ),
            (
                json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": 7}),
                true,
                "does not allow",
            ),
        ];
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let workspace = TempDir::new().unwrap();
            let tool = supervised_tool(workspace.path(), autonomy);
            for (args, high, reaches) in &cases {
                let (operation, minted_high) = minted(args).expect("a lifecycle mutation gets a grant");
                assert_eq!(minted_high, *high, "{operation}");
                let result = tool.execute(with_grant(args.clone(), &operation)).await.unwrap();
                assert!(!result.success);
                assert!(
                    result.error.as_deref().unwrap_or_default().contains(reaches),
                    "{autonomy:?} {operation}: {:?}",
                    result.error
                );
            }
        }
        let (rollback, _) = minted(&cases[2].0).unwrap();
        assert_eq!(
            rollback,
            format!("skills_manage:lifecycle_rollback:release-notes:{revision}:7")
        );
        // Reads, incomplete mutations and the other actions get no grant.
        for args in [
            json!({"action": "lifecycle_status"}),
            json!({"action": "lifecycle_list"}),
            json!({"action": "lifecycle_inspect", "proposal_id": PROPOSAL}),
            json!({"action": "lifecycle_publish"}),
            json!({"action": "lifecycle_rollback", "skill_id": "a", "revision_id": "b"}),
            json!({"action": "create", "name": "x"}),
            json!({"action": "lifecycle_force_close", "proposal_id": PROPOSAL}),
        ] {
            assert!(minted(&args).is_none(), "{args}");
        }
    }

    /// A rollback grant minted for one observed generation does not permit
    /// another.
    #[tokio::test]
    async fn a_rollback_grant_is_bound_to_the_generation() {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
        let revision = format!("r1-{}", "0".repeat(64));
        let args_at = |generation: u64| json!({"action": "lifecycle_rollback", "skill_id": "release-notes", "revision_id": revision, "expected_generation": generation});
        let (operation, _) = minted(&args_at(3)).unwrap();
        let wrong = tool.execute(with_grant(args_at(4), &operation)).await.unwrap();
        assert!(
            wrong
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("explicit approval grant"),
            "{:?}",
            wrong.error
        );
        let right = tool.execute(with_grant(args_at(3), &operation)).await.unwrap();
        assert!(
            right.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{:?}",
            right.error
        );
    }

    /// A grant authorizes one publication or rollback: replaying it is refused
    /// under supervised (the gate spends it) and under full (the tool does).
    #[tokio::test]
    async fn a_publication_grant_cannot_be_replayed_in_any_autonomy_level() {
        use crate::security::AutonomyLevel;
        for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let workspace = TempDir::new().unwrap();
            let tool = supervised_tool(workspace.path(), autonomy);
            let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
            let (operation, _) = minted(&args).unwrap();
            let grant = grant_for(&operation);

            let first = tool.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
            assert!(
                first.error.as_deref().unwrap_or_default().contains("does not allow"),
                "{autonomy:?}: the first use reaches the service: {:?}",
                first.error
            );
            let replay = tool.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
            assert!(!replay.success);
            assert!(
                replay.error.as_deref().unwrap_or_default().contains("replay"),
                "{autonomy:?}: {:?}",
                replay.error
            );
            assert!(
                !replay.error.as_deref().unwrap_or_default().contains("does not allow"),
                "{autonomy:?}: a replay must not reach the service"
            );
            // A fresh grant works again.
            let fresh = tool.execute(with_grant(args, &operation)).await.unwrap();
            assert!(fresh.error.as_deref().unwrap_or_default().contains("does not allow"));
        }
    }

    /// Only a signed one-shot grant authorizes a publication or rollback: a v1
    /// operation hash grant has no use budget and never expires.
    #[tokio::test]
    async fn a_hash_grant_does_not_authorize_a_publication() {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
        let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let (operation, _) = minted(&args).unwrap();
        let v1 = serde_json::to_value(crate::security::policy::ApprovalGrant::for_resource_operation(
            "skills_manage",
            &operation,
            "test",
            None,
        ))
        .unwrap();
        let result = tool.execute(attach_grant(args, v1)).await.unwrap();
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("explicit approval grant"),
            "{:?}",
            result.error
        );
    }

    /// A caller the runtime knows to be another owner never reaches the store,
    /// and does not spend its grant trying.
    #[tokio::test]
    async fn another_owner_does_not_reach_the_lifecycle_store() {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
        let foreign = |owner: &str| {
            let mut args = with_grant(
                json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL}),
                &format!("skills_manage:lifecycle_publish:{PROPOSAL}"),
            );
            if let Some(object) = args.as_object_mut() {
                object.insert(
                    "_zc_scope".to_string(),
                    json!({"principal_id": "telegram:alice", "owner_id": owner}),
                );
            }
            args
        };
        let denied = tool.execute(foreign("owner:ws:cli:bob")).await.unwrap();
        assert!(
            denied
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("another owner scope"),
            "{:?}",
            denied.error
        );
        let status = tool.execute(json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_id": "owner:ws:cli:bob"}})).await.unwrap();
        assert!(!status.success);
        // The configured owner is served.
        let served = tool.execute(foreign("owner:ws:cli:alice")).await.unwrap();
        assert!(
            served.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{:?}",
            served.error
        );
    }

    /// A disabled lifecycle builds the tool without any lifecycle action and
    /// opens no store.
    #[test]
    fn a_disabled_lifecycle_offers_nothing_and_opens_no_store() {
        let workspace = TempDir::new().unwrap();
        let tool = SkillsManageTool::from_config(
            workspace.path().to_path_buf(),
            test_config(workspace.path()),
            Arc::new(SecurityPolicy::default()),
        );
        assert!(tool.lifecycle.is_none());
        assert!(
            offered_actions(&tool)
                .iter()
                .all(|action| !action.starts_with("lifecycle_"))
        );
        assert!(!workspace.path().join("self").exists(), "no store was opened");

        let mut config = test_config(workspace.path());
        config.skills_lifecycle.enabled = true;
        config.skills_lifecycle.owner_scope = Some("owner:ws:cli:alice".to_string());
        let enabled = SkillsManageTool::from_config(
            workspace.path().to_path_buf(),
            config,
            Arc::new(SecurityPolicy::default()),
        );
        assert!(enabled.lifecycle.is_some());
        assert!(
            offered_actions(&enabled)
                .iter()
                .any(|action| action == "lifecycle_status")
        );
    }

    /// A caller whose owner the runtime cannot vouch for reaches nothing, reads
    /// included: no trusted scope, an incomplete scope, an owner-isolated
    /// caller without an owner id of its own.
    #[tokio::test]
    async fn a_caller_with_an_unknown_owner_reaches_no_lifecycle_action() {
        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Full);
        let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let (operation, _) = minted(&args).unwrap();
        let grant = grant_for(&operation);
        let calls = [
            // No scope at all.
            json!({"action": "lifecycle_status"}),
            json!({"action": "lifecycle_list"}),
            // A scope the runtime did not mark trusted.
            json!({"action": "lifecycle_status", "_zc_scope": {"owner_id": "owner:ws:cli:alice"}}),
            // Trusted but incomplete.
            json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"workspace_id": "ws"}}),
            // Owner-isolated with workspace, channel and sender but no owner id.
            json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "workspace_id": "ws", "channel": "cli", "sender": "alice"}}),
            json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "owner_id": "  ", "workspace_id": "ws", "channel": "cli", "sender": "alice"}}),
        ];
        for call in calls {
            let result = tool.execute(call.clone()).await.unwrap();
            assert!(!result.success, "{call}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("known owner scope"),
                "{call}: {:?}",
                result.error
            );
        }
        // A grant is not spent by an unknown caller: it still works for the owner.
        let mut unknown = args.clone();
        if let Some(object) = unknown.as_object_mut() {
            object.insert(
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                grant.clone(),
            );
        }
        let refused = tool.execute(unknown).await.unwrap();
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("known owner scope")
        );
        let served = tool.execute(attach_grant(args, grant)).await.unwrap();
        assert!(
            served.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{:?}",
            served.error
        );
        // An owner-isolated caller with its own owner id is served.
        let isolated = tool
            .execute(json!({"action": "lifecycle_status", "_zc_scope_trusted": true, "_zc_scope": {"owner_isolated": true, "owner_id": "owner:ws:cli:alice"}}))
            .await
            .unwrap();
        assert!(isolated.success, "{:?}", isolated.error);
    }

    /// A call the gate refuses does not burn the approval.
    #[tokio::test]
    async fn a_refused_call_does_not_spend_its_grant() {
        use crate::security::AutonomyLevel;
        let workspace = TempDir::new().unwrap();
        // Read-only autonomy: the gate refuses before any grant is looked at.
        let denied = supervised_tool(workspace.path(), AutonomyLevel::ReadOnly);
        let allowed = supervised_tool(workspace.path(), AutonomyLevel::Full);
        let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let (operation, _) = minted(&args).unwrap();
        let grant = grant_for(&operation);
        let first = denied.execute(attach_grant(args.clone(), grant.clone())).await.unwrap();
        assert!(!first.success);
        assert!(!first.error.as_deref().unwrap_or_default().contains("does not allow"));
        let then = allowed.execute(attach_grant(args, grant)).await.unwrap();
        assert!(
            then.error.as_deref().unwrap_or_default().contains("does not allow"),
            "the grant was not burned by the refused call: {:?}",
            then.error
        );
    }

    /// The decision-resume grant: a decision-bound grant is honoured only
    /// inside the claimed-decision scope the resume path installs, and only for
    /// the operation the runtime mints.
    #[tokio::test]
    async fn a_decision_bound_grant_works_only_inside_the_claimed_scope() {
        use crate::acl::approval_grant::{
            ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring,
        };
        use crate::security::decision_approval::{DecisionGrantAuthority, with_claimed_decision};
        struct Claimed {
            grant: ApprovalGrantV2,
            operation: String,
        }
        impl DecisionGrantAuthority for Claimed {
            fn verify(
                &self,
                grant: &ApprovalGrantV2,
                tool: &str,
                operation: &str,
                _risk: RiskLevel,
                caller_principal: Option<&str>,
            ) -> Result<(), String> {
                if *grant == self.grant
                    && tool == "skills_manage"
                    && operation == self.operation
                    && caller_principal == Some(grant.subject.principal_id.as_str())
                {
                    Ok(())
                } else {
                    Err("decision grant differs from active claimed call".to_string())
                }
            }
        }

        let workspace = TempDir::new().unwrap();
        let tool = supervised_tool(workspace.path(), crate::security::AutonomyLevel::Supervised);
        let args = json!({"action": "lifecycle_publish", "proposal_id": PROPOSAL});
        let (operation, _) = minted(&args).unwrap();
        let grant = ApprovalGrantV2::issue_decision_one_shot(
            WitnessKeyring::global_for_tests(),
            Subject {
                agent_id: "prx:test:agent".to_string(),
                principal_id: "telegram:alice".to_string(),
                owner_id: "owner:ws:cli:alice".to_string(),
                workspace_id: "ws".to_string(),
                session_key: Some("session".to_string()),
            },
            operation.clone(),
            RiskLevel::High,
            DecisionApprovalBinding {
                decision_id: uuid::Uuid::now_v7().to_string(),
                catalog_generation: [1; 32],
                call_digest: [2; 32],
            },
            chrono::Utc::now() + chrono::Duration::minutes(2),
        )
        .unwrap();
        let wire = serde_json::to_value(crate::security::policy::ApprovalGrant::from_verified_v2(
            "skills_manage",
            "approval_manager",
            grant.clone(),
        ))
        .unwrap();

        // Outside a claimed execution the grant is refused.
        let outside = tool.execute(attach_grant(args.clone(), wire.clone())).await.unwrap();
        assert!(
            outside
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("explicit approval grant"),
            "{:?}",
            outside.error
        );
        // Inside it, the call reaches the service (the mode then refuses).
        let claimed = Claimed {
            grant: grant.clone(),
            operation: operation.clone(),
        };
        let inside = with_claimed_decision(
            Box::new(claimed),
            tool.execute(attach_grant(args.clone(), wire.clone())),
        )
        .await
        .unwrap();
        assert!(
            inside.error.as_deref().unwrap_or_default().contains("does not allow"),
            "{:?}",
            inside.error
        );
        // A claim for another operation does not cover this call.
        let other = Claimed {
            grant,
            operation: format!("{operation}x"),
        };
        let mismatched = with_claimed_decision(Box::new(other), tool.execute(attach_grant(args, wire)))
            .await
            .unwrap();
        assert!(
            mismatched
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("explicit approval grant"),
            "{:?}",
            mismatched.error
        );
    }
}

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::Config;
use crate::runtime::RuntimeAdapter;
use crate::runtime::shell_process::{ShellProcessAdapter, ShellProcessError, ShellProcessRequest};
use crate::security::SecurityPolicy;
use crate::security::policy::{ApprovalGrant, RUNTIME_APPROVAL_GRANT_ARG, ResourceRiskLevel, SideEffectGate};
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
                    "instruction_mode": if skill.prompts.is_empty() { "lazy" } else { "preloaded" },
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

pub struct SkillReadTool {
    workspace_dir: PathBuf,
    config: Config,
}

impl SkillReadTool {
    pub const fn new(workspace_dir: PathBuf, config: Config) -> Self {
        Self { workspace_dir, config }
    }

    fn resolve_target(&self, name: &str, resource_path: Option<&str>) -> anyhow::Result<(PathBuf, PathBuf, bool)> {
        let skills = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config);
        let skill = skills
            .iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("Unknown skill '{name}'. Use skills_list to inspect the catalog."))?;
        let location = skill
            .location
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has no readable local location"))?;
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
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System]
    }
}

pub struct SkillsManageTool {
    workspace_dir: PathBuf,
    config: Config,
    security: Arc<SecurityPolicy>,
}

impl SkillsManageTool {
    pub const fn new(workspace_dir: PathBuf, config: Config, security: Arc<SecurityPolicy>) -> Self {
        Self {
            workspace_dir,
            config,
            security,
        }
    }

    fn approval_grant(args: &serde_json::Value) -> Option<ApprovalGrant> {
        args.get(RUNTIME_APPROVAL_GRANT_ARG)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
    }

    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), ToolResult> {
        let risk = match action {
            "enable" | "disable" | "validate" => ResourceRiskLevel::Low,
            _ => ResourceRiskLevel::Medium,
        };
        let operation = format!("skills_manage:{action}");
        let grant = Self::approval_grant(args);
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation(self.name(), &operation, risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
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
        "Create, install, update, enable, disable, validate, synchronize, or remove PRX skills. Mutating actions are approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "install", "update", "enable", "disable", "validate", "sync", "remove"]
                },
                "name": {"type": "string", "description": "Workspace skill name"},
                "source": {"type": "string", "description": "Git URL or explicit local directory for install"},
                "description": {"type": "string", "description": "Short description for a created skill"},
                "instructions": {"type": "string", "description": "SKILL.md instructions for a created skill"}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = Self::required_string(&args, "action")?;
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
}

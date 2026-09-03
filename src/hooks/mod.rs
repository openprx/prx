use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const HOOKS_JSON_FILE: &str = "hooks.json";
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_HOOKS_FILE_BYTES: u64 = 256 * 1024;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 16 * 1024;
const MAX_STDERR_CHARS: usize = 400;
const MAX_ACTIONS: usize = 256;
const MAX_ARGS_PER_ACTION: usize = 128;
const MAX_ENV_PER_ACTION: usize = 128;

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    AgentStart,
    AgentEnd,
    LlmRequest,
    LlmResponse,
    ToolCallStart,
    ToolCall,
    TurnComplete,
    Error,
}

impl HookEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::LlmRequest => "llm_request",
            Self::LlmResponse => "llm_response",
            Self::ToolCallStart => "tool_call_start",
            Self::ToolCall => "tool_call",
            Self::TurnComplete => "turn_complete",
            Self::Error => "error",
        }
    }

    #[cfg(feature = "wasm-plugins")]
    const fn wasm_lifecycle_name(self) -> &'static str {
        match self {
            // The native hook name predates the public WASM lifecycle schema.
            Self::AgentEnd => "agent_stop",
            _ => self.as_str(),
        }
    }

    const ALL: [Self; 8] = [
        Self::AgentStart,
        Self::AgentEnd,
        Self::LlmRequest,
        Self::LlmResponse,
        Self::ToolCallStart,
        Self::ToolCall,
        Self::TurnComplete,
        Self::Error,
    ];

    fn from_name(name: &str) -> Option<Self> {
        let normalized = normalize_event_name(name);
        Self::ALL.into_iter().find(|event| event.as_str() == normalized)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HookAction {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default = "default_stdin_json")]
    stdin_json: bool,
}

const fn default_stdin_json() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    hooks: HashMap<String, Vec<HookAction>>,
}

#[derive(Debug, Clone)]
struct HookConfig {
    timeout_ms: u64,
    hooks: HashMap<String, Vec<HookAction>>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            hooks: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RuntimeState {
    config: HookConfig,
    hooks_json_digest: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookDiagnostics {
    pub configured: bool,
    pub valid: bool,
    pub config_path: String,
    pub timeout_ms: u64,
    pub action_count: usize,
    pub events: BTreeMap<String, usize>,
    pub error: Option<String>,
}

pub struct HookManager {
    workspace_dir: PathBuf,
    hooks_json_path: PathBuf,
    media_artifacts: std::sync::Arc<crate::media::MediaArtifactOwner>,
    state: RwLock<RuntimeState>,
    /// Process-level WASM plugin runtime; it is the sole generation owner.
    #[cfg(feature = "wasm-plugins")]
    plugin_runtime: tokio::sync::RwLock<Option<std::sync::Arc<crate::plugins::PluginRuntime>>>,
}

impl HookManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&workspace_dir);
        Self {
            hooks_json_path: workspace_dir.join(HOOKS_JSON_FILE),
            workspace_dir,
            media_artifacts,
            state: RwLock::new(RuntimeState::default()),
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: tokio::sync::RwLock::new(None),
        }
    }

    pub fn media_artifacts(&self) -> std::sync::Arc<crate::media::MediaArtifactOwner> {
        self.media_artifacts.clone()
    }

    /// Attach the sole process-level plugin runtime for lifecycle observation.
    #[cfg(feature = "wasm-plugins")]
    pub async fn set_plugin_runtime(&self, runtime: std::sync::Arc<crate::plugins::PluginRuntime>) {
        *self.plugin_runtime.write().await = Some(runtime);
    }

    /// Transform one shared agent-pipeline envelope through the current WASM
    /// middleware generation. Invalid JSON output is isolated and the original
    /// envelope is preserved so a plugin cannot corrupt the turn protocol.
    #[cfg(feature = "wasm-plugins")]
    pub async fn process_wasm_middleware(
        &self,
        stage: crate::plugins::capabilities::middleware::MiddlewareStage,
        envelope: serde_json::Value,
    ) -> serde_json::Value {
        let runtime = self.plugin_runtime.read().await.clone();
        let Some(runtime) = runtime else {
            return envelope;
        };
        let chain = runtime.middleware();
        if chain.is_empty() {
            return envelope;
        }
        let original = envelope.to_string();
        let transformed = chain.process(stage, &original).await;
        match serde_json::from_str(&transformed) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    stage = stage.as_str(),
                    error = %error,
                    "WASM middleware returned invalid JSON; preserving the original envelope"
                );
                envelope
            }
        }
    }

    pub fn diagnostics(&self) -> HookDiagnostics {
        let error = self.refresh_if_changed().err().map(|error| error.to_string());
        let state = self.state.read();
        let events = state
            .config
            .hooks
            .iter()
            .map(|(event, actions)| (event.clone(), actions.len()))
            .collect::<BTreeMap<_, _>>();
        HookDiagnostics {
            configured: self.hooks_json_path.is_file(),
            valid: error.is_none(),
            config_path: self.hooks_json_path.display().to_string(),
            timeout_ms: state.config.timeout_ms,
            action_count: events.values().sum(),
            events,
            error,
        }
    }

    pub fn validate_config(&self, content: &str) -> anyhow::Result<HookDiagnostics> {
        if content.len() as u64 > MAX_HOOKS_FILE_BYTES {
            anyhow::bail!("hooks config exceeds {MAX_HOOKS_FILE_BYTES} byte limit");
        }
        let config = parse_hook_config(content.as_bytes())?;
        let events = config
            .hooks
            .iter()
            .map(|(event, actions)| (event.clone(), actions.len()))
            .collect::<BTreeMap<_, _>>();
        Ok(HookDiagnostics {
            configured: false,
            valid: true,
            config_path: self.hooks_json_path.display().to_string(),
            timeout_ms: config.timeout_ms,
            action_count: events.values().sum(),
            events,
            error: None,
        })
    }

    pub fn replace_config(&self, content: &str) -> anyhow::Result<HookDiagnostics> {
        let _ = self.validate_config(content)?;
        std::fs::create_dir_all(&self.workspace_dir)?;
        let mut candidate = tempfile::Builder::new()
            .prefix(".hooks-json-")
            .suffix(".tmp")
            .tempfile_in(&self.workspace_dir)?;
        candidate.write_all(content.as_bytes())?;
        candidate.flush()?;
        candidate.persist(&self.hooks_json_path).map_err(|error| error.error)?;
        self.refresh_if_changed()?;
        Ok(self.diagnostics())
    }

    pub fn remove_config(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.hooks_json_path.exists() {
            *self.state.write() = RuntimeState::default();
            return Ok(None);
        }
        let trash_dir = self.workspace_dir.join(".hooks-trash");
        std::fs::create_dir_all(&trash_dir)?;
        let backup = trash_dir.join(format!("hooks-{}.json", uuid::Uuid::new_v4()));
        std::fs::rename(&self.hooks_json_path, &backup)?;
        *self.state.write() = RuntimeState::default();
        Ok(Some(backup))
    }

    pub async fn test_event(&self, event: HookEvent, payload: serde_json::Value) {
        self.emit(event, payload).await;
    }

    pub fn manage_tool(
        self: &std::sync::Arc<Self>,
        security: std::sync::Arc<crate::security::SecurityPolicy>,
    ) -> Box<dyn crate::tools::Tool> {
        Box::new(HooksManageTool {
            manager: self.clone(),
            security,
        })
    }

    pub fn status_tool(self: &std::sync::Arc<Self>) -> Box<dyn crate::tools::Tool> {
        Box::new(HooksStatusTool { manager: self.clone() })
    }

    pub async fn emit(&self, event: HookEvent, payload: serde_json::Value) {
        if let Err(err) = self.refresh_if_changed() {
            tracing::warn!(error = %err, "hooks refresh failed");
        } else {
            let (timeout_ms, actions) = {
                let state = self.state.read();
                (
                    state.config.timeout_ms,
                    state.config.hooks.get(event.as_str()).cloned().unwrap_or_default(),
                )
            };

            if !actions.is_empty() {
                for action in actions {
                    if let Err(err) = self.run_action(event, &payload, timeout_ms, &action).await {
                        tracing::warn!(
                            event = event.as_str(),
                            command = action.command,
                            error = %err,
                            "hook execution failed"
                        );
                    }
                }
            }
        }

        // Fire WASM hooks and bridge the lifecycle topic through the same process runtime.
        #[cfg(feature = "wasm-plugins")]
        {
            let runtime = self.plugin_runtime.read().await.clone();
            if let Some(runtime) = runtime {
                let payload_str = payload.to_string();
                let topic = format!("prx.lifecycle.{}", event.wasm_lifecycle_name());
                runtime.emit_hook(&topic, &payload_str).await;
                if let Err(error) = runtime.event_bus().publish(&topic, &payload_str).await {
                    tracing::debug!(topic = %topic, error = %error, "event bus lifecycle bridge error");
                }
            }
        }
    }

    fn refresh_if_changed(&self) -> anyhow::Result<()> {
        let Some(raw) = read_bounded_file(&self.hooks_json_path, MAX_HOOKS_FILE_BYTES)? else {
            let mut state = self.state.write();
            if state.hooks_json_digest.is_some() {
                *state = RuntimeState::default();
            }
            return Ok(());
        };

        let digest: [u8; 32] = Sha256::digest(&raw).into();
        if self.state.read().hooks_json_digest == Some(digest) {
            return Ok(());
        }

        let config = parse_hook_config(&raw)?;

        // Candidate parsing and validation finish before this single generation swap.
        *self.state.write() = RuntimeState {
            config,
            hooks_json_digest: Some(digest),
        };
        Ok(())
    }

    async fn run_action(
        &self,
        event: HookEvent,
        payload: &serde_json::Value,
        default_timeout_ms: u64,
        action: &HookAction,
    ) -> anyhow::Result<()> {
        let command = action.command.trim();
        if command.is_empty() {
            anyhow::bail!("hook command is empty");
        }
        let timeout_ms = bounded_timeout(action.timeout_ms.unwrap_or(default_timeout_ms))?;

        let payload_json = payload.to_string();
        if payload_json.len() > MAX_PAYLOAD_BYTES {
            anyhow::bail!(
                "hook payload is {} bytes; limit is {MAX_PAYLOAD_BYTES}",
                payload_json.len()
            );
        }
        let mut cmd = Command::new(command);
        cmd.args(&action.args);
        cmd.env("ZERO_HOOK_EVENT", event.as_str());

        // Avoid execve argv+env size overflow for large payloads.
        const MAX_ENV_PAYLOAD_BYTES: usize = 8 * 1024;
        if payload_json.len() <= MAX_ENV_PAYLOAD_BYTES {
            cmd.env("ZERO_HOOK_PAYLOAD", &payload_json);
        } else {
            cmd.env("ZERO_HOOK_PAYLOAD", "");
            cmd.env("ZERO_HOOK_PAYLOAD_TRUNCATED", "1");
        }

        // NamedTempFile is created with restrictive permissions and removes itself on every exit.
        let mut payload_file = tempfile::Builder::new()
            .prefix("openprx-hook-payload-")
            .suffix(".json")
            .tempfile()?;
        payload_file.write_all(payload_json.as_bytes())?;
        payload_file.flush()?;
        cmd.env("ZERO_HOOK_PAYLOAD_FILE", payload_file.path());

        if !action.env.is_empty() {
            // Block env vars that could hijack the process (e.g., LD_PRELOAD, PATH).
            const BLOCKED_VARS: &[&str] = &[
                "LD_PRELOAD",
                "LD_LIBRARY_PATH",
                "DYLD_INSERT_LIBRARIES",
                "DYLD_LIBRARY_PATH",
                "PATH",
                "HOME",
            ];
            for (key, value) in &action.env {
                if value.contains('\0') || key.contains('\0') {
                    tracing::warn!(env_var = %key, "hook env var contains null byte, skipping");
                    continue;
                }
                if BLOCKED_VARS.iter().any(|b| key.eq_ignore_ascii_case(b)) {
                    tracing::warn!(env_var = %key, "hook attempted to set blocked env var, skipping");
                    continue;
                }
                cmd.env(key, value);
            }
        }

        if let Some(cwd) = &action.cwd {
            let cwd_path = Path::new(cwd);
            let resolved = if cwd_path.is_absolute() {
                cwd_path.to_path_buf()
            } else {
                self.workspace_dir.join(cwd_path)
            };
            cmd.current_dir(resolved);
        } else {
            cmd.current_dir(&self.workspace_dir);
        }

        if action.stdin_json {
            cmd.stdin(std::process::Stdio::piped());
        }
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = crate::runtime::shell_process::spawn_managed_shell_child(cmd)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        let stderr = child.take_stderr();
        let stderr_reader = tokio::spawn(async move {
            let Some(stderr) = stderr else {
                return std::io::Result::Ok(Vec::new());
            };
            let mut bytes = Vec::new();
            stderr.take(MAX_STDERR_BYTES + 1).read_to_end(&mut bytes).await?;
            Ok(bytes)
        });

        if action.stdin_json {
            if let Some(mut stdin) = child.take_stdin() {
                let write_result = tokio::time::timeout_at(deadline, stdin.write_all(payload_json.as_bytes())).await;
                match write_result {
                    Err(_) => {
                        let _ = child.terminate_and_reap().await;
                        let _ = stderr_reader.await;
                        anyhow::bail!("hook timed out while writing stdin");
                    }
                    Ok(Err(error)) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                        let _ = child.terminate_and_reap().await;
                        let _ = stderr_reader.await;
                        return Err(error.into());
                    }
                    Ok(_) => {}
                }
            }
        }

        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = child.terminate_and_reap().await;
                let _ = stderr_reader.await;
                return Err(error.into());
            }
            Err(_) => {
                let _ = child.terminate_and_reap().await;
                let _ = stderr_reader.await;
                anyhow::bail!("hook timed out after {timeout_ms} ms");
            }
        };
        let mut stderr = stderr_reader.await??;
        child.mark_complete();
        let stderr_truncated = stderr.len() > MAX_STDERR_BYTES as usize;
        stderr.truncate(MAX_STDERR_BYTES as usize);

        if status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&stderr);
        let mut stderr = truncate_for_log(stderr.as_ref(), MAX_STDERR_CHARS);
        if stderr_truncated {
            stderr.push_str(" [truncated]");
        }
        anyhow::bail!(
            "hook exited with status {}{}",
            status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(", stderr: {stderr}")
            }
        )
    }
}

pub struct HooksManageTool {
    manager: std::sync::Arc<HookManager>,
    security: std::sync::Arc<crate::security::SecurityPolicy>,
}

pub struct HooksStatusTool {
    manager: std::sync::Arc<HookManager>,
}

#[async_trait]
impl crate::tools::Tool for HooksStatusTool {
    fn name(&self) -> &str {
        "hooks_status"
    }

    fn description(&self) -> &str {
        "Inspect the active workspace lifecycle-hooks configuration, event coverage, action count, and validation error."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&self.manager.diagnostics())?,
            error: None,
        })
    }

    fn tier(&self) -> crate::tools::ToolTier {
        crate::tools::ToolTier::Standard
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        &[
            crate::tools::ToolCategory::System,
            crate::tools::ToolCategory::Automation,
        ]
    }
}

impl HooksManageTool {
    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), crate::tools::ToolResult> {
        use crate::security::policy::{ApprovalGrant, ResourceRiskLevel, SideEffectGate};
        let risk = match action {
            "test" => ResourceRiskLevel::High,
            "replace" | "remove" => ResourceRiskLevel::Medium,
            _ => return Ok(()),
        };
        let grant = ApprovalGrant::from_runtime_args("hooks_manage", args);
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation("hooks_manage", &format!("hooks_manage:{action}"), risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| crate::tools::ToolResult {
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
impl crate::tools::Tool for HooksManageTool {
    fn name(&self) -> &str {
        "hooks_manage"
    }

    fn description(&self) -> &str {
        "Inspect, validate, atomically replace, remove, refresh, or test workspace lifecycle hooks. Mutating and test actions are approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "refresh", "validate", "replace", "remove", "test"]},
                "content": {"type": "string", "description": "Complete hooks.json for validate or replace"},
                "event": {"type": "string", "enum": HookEvent::ALL.map(HookEvent::as_str)},
                "payload": {"type": "object", "description": "Synthetic payload for test"}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        use crate::tools::ToolResult;
        let action = Self::required_string(&args, "action")?;
        if let Err(result) = self.authorize(action, &args) {
            return Ok(result);
        }
        let result: anyhow::Result<serde_json::Value> = match action {
            "status" | "refresh" => serde_json::to_value(self.manager.diagnostics()).map_err(Into::into),
            "validate" => {
                let content = Self::required_string(&args, "content")?;
                self.manager
                    .validate_config(content)
                    .and_then(|status| serde_json::to_value(status).map_err(Into::into))
            }
            "replace" => {
                let content = Self::required_string(&args, "content")?;
                self.manager
                    .replace_config(content)
                    .and_then(|status| serde_json::to_value(status).map_err(Into::into))
            }
            "remove" => self
                .manager
                .remove_config()
                .map(|backup| json!({"removed": backup.is_some(), "recoverable_backup": backup})),
            "test" => {
                let event_name = Self::required_string(&args, "event")?;
                let event = HookEvent::from_name(event_name)
                    .ok_or_else(|| anyhow::anyhow!("Unknown hook event '{event_name}'"))?;
                let payload = args
                    .get("payload")
                    .cloned()
                    .unwrap_or_else(|| json!({"source": "hooks_manage", "test": true}));
                self.manager.test_event(event, payload).await;
                Ok(json!({"tested": true, "event": event.as_str()}))
            }
            _ => anyhow::bail!("Unsupported hooks_manage action: {action}"),
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

    fn tier(&self) -> crate::tools::ToolTier {
        crate::tools::ToolTier::Extended
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        &[
            crate::tools::ToolCategory::System,
            crate::tools::ToolCategory::Automation,
        ]
    }
}

fn bounded_timeout(timeout_ms: u64) -> anyhow::Result<u64> {
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        anyhow::bail!("hook timeout_ms must be between 1 and {MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

fn validate_hooks_file(file: &HooksFile) -> anyhow::Result<()> {
    if let Some(timeout_ms) = file.timeout_ms {
        bounded_timeout(timeout_ms)?;
    }
    let action_count = file.hooks.values().map(Vec::len).sum::<usize>();
    if action_count > MAX_ACTIONS {
        anyhow::bail!("hooks.json has {action_count} actions; limit is {MAX_ACTIONS}");
    }
    let mut normalized_events = std::collections::HashSet::new();
    for event in file.hooks.keys() {
        let normalized = normalize_event_name(event);
        if HookEvent::from_name(&normalized).is_none() {
            anyhow::bail!("unknown hook event '{event}'");
        }
        if !normalized_events.insert(normalized) {
            anyhow::bail!("duplicate hook event after normalization: '{event}'");
        }
    }
    for actions in file.hooks.values() {
        for action in actions {
            if action.args.len() > MAX_ARGS_PER_ACTION {
                anyhow::bail!("hook action has too many arguments");
            }
            if action.env.len() > MAX_ENV_PER_ACTION {
                anyhow::bail!("hook action has too many environment entries");
            }
            if let Some(timeout_ms) = action.timeout_ms {
                bounded_timeout(timeout_ms)?;
            }
        }
    }
    Ok(())
}

fn parse_hook_config(raw: &[u8]) -> anyhow::Result<HookConfig> {
    let parsed: HooksFile = serde_json::from_slice(raw)?;
    validate_hooks_file(&parsed)?;
    Ok(HookConfig {
        timeout_ms: bounded_timeout(parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))?,
        hooks: parsed
            .hooks
            .into_iter()
            .map(|(name, actions)| (normalize_event_name(&name), actions))
            .collect(),
    })
}

fn normalize_event_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

fn read_bounded_file(path: &Path, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        anyhow::bail!("{} exceeds {max_bytes} byte limit", path.display());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("{} exceeds {max_bytes} byte limit", path.display());
    }
    Ok(Some(bytes))
}

fn truncate_for_log(input: &str, max_chars: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }
    let mut out = String::new();
    for c in input.chars().take(max_chars) {
        out.push(c);
    }
    out.push_str("...");
    out
}

pub fn payload_error(component: &str, message: &str) -> serde_json::Value {
    json!({
        "component": component,
        "message": message,
    })
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normalize_event_name_handles_case_and_dash() {
        assert_eq!(normalize_event_name("Tool-Call-Start"), "tool_call_start");
    }

    #[test]
    fn refresh_loads_hooks_json() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(HOOKS_JSON_FILE);
        std::fs::write(
            &path,
            r#"{
  "timeout_ms": 1200,
  "hooks": {
    "tool-call-start": [
      { "command": "echo", "args": ["ok"] }
    ]
  }
}"#,
        )
        .unwrap();

        let manager = HookManager::new(temp.path().to_path_buf());
        manager.refresh_if_changed().unwrap();

        let state = manager.state.read();
        assert_eq!(state.config.timeout_ms, 1200);
        assert!(state.config.hooks.contains_key("tool_call_start"));
    }

    // ── HookEvent::as_str ─────────────────────────────────────

    #[test]
    fn hook_event_all_variants_have_names() {
        let events = [
            HookEvent::AgentStart,
            HookEvent::AgentEnd,
            HookEvent::LlmRequest,
            HookEvent::LlmResponse,
            HookEvent::ToolCallStart,
            HookEvent::ToolCall,
            HookEvent::TurnComplete,
            HookEvent::Error,
        ];
        for e in &events {
            assert!(!e.as_str().is_empty());
        }
    }

    #[test]
    fn hook_event_as_str_values() {
        assert_eq!(HookEvent::AgentStart.as_str(), "agent_start");
        assert_eq!(HookEvent::ToolCall.as_str(), "tool_call");
        assert_eq!(HookEvent::Error.as_str(), "error");
    }

    // ── normalize_event_name ────────────────────────────────────

    #[test]
    fn normalize_event_name_identity() {
        assert_eq!(normalize_event_name("agent_start"), "agent_start");
    }

    #[test]
    fn normalize_event_name_trims() {
        assert_eq!(normalize_event_name("  tool_call  "), "tool_call");
    }

    #[test]
    fn normalize_event_name_uppercased() {
        assert_eq!(normalize_event_name("AGENT_END"), "agent_end");
    }

    // ── truncate_for_log ────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let long = "a".repeat(500);
        let out = truncate_for_log(&long, 10);
        assert_eq!(out.len(), 13); // 10 chars + "..."
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_for_log("", 10), "");
    }

    #[test]
    fn truncate_unicode_safe() {
        let s = "你好世界！测试数据会被截断吗";
        let out = truncate_for_log(s, 4);
        // Should take exactly 4 chars + "..."
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), 7); // 4 + 3 dots
    }

    // ── payload_error ───────────────────────────────────────────

    #[test]
    fn payload_error_format() {
        let p = payload_error("gateway", "connection refused");
        assert_eq!(p["component"], "gateway");
        assert_eq!(p["message"], "connection refused");
    }

    // ── HookManager::new ────────────────────────────────────────

    #[test]
    fn hook_manager_new_sets_path() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        assert_eq!(manager.hooks_json_path, temp.path().join(HOOKS_JSON_FILE));
    }

    // ── refresh_if_changed edge cases ───────────────────────────

    #[test]
    fn refresh_missing_file_resets_to_default() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        // No hooks.json → should succeed with default config
        manager.refresh_if_changed().unwrap();
        let state = manager.state.read();
        assert_eq!(state.config.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(state.config.hooks.is_empty());
    }

    #[test]
    fn refresh_invalid_json_fails() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join(HOOKS_JSON_FILE), "not json!!!").unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        assert!(manager.refresh_if_changed().is_err());
    }

    #[test]
    fn refresh_invalid_candidate_preserves_active_generation() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(HOOKS_JSON_FILE);
        std::fs::write(&path, r#"{"timeout_ms": 1200, "hooks": {}}"#).unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        manager.refresh_if_changed().unwrap();
        let active_digest = manager.state.read().hooks_json_digest;

        std::fs::write(&path, "not json").unwrap();
        assert!(manager.refresh_if_changed().is_err());
        let state = manager.state.read();
        assert_eq!(state.config.timeout_ms, 1200);
        assert_eq!(state.hooks_json_digest, active_digest);
    }

    #[test]
    fn diagnostics_and_recoverable_lifecycle_reflect_active_config() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let candidate = r#"{
  "timeout_ms": 1500,
  "hooks": {
    "tool-call": [{"command": "true"}],
    "turn_complete": [{"command": "true"}]
  }
}"#;

        let validated = manager.validate_config(candidate).unwrap();
        assert!(!validated.configured);
        assert!(validated.valid);
        assert_eq!(validated.action_count, 2);

        let active = manager.replace_config(candidate).unwrap();
        assert!(active.configured);
        assert!(active.valid);
        assert_eq!(active.timeout_ms, 1500);
        assert_eq!(active.events.get("tool_call"), Some(&1));

        let backup = manager.remove_config().unwrap().unwrap();
        assert!(!temp.path().join(HOOKS_JSON_FILE).exists());
        assert!(backup.is_file());
        assert!(!manager.diagnostics().configured);
    }

    #[test]
    fn validation_rejects_unknown_and_duplicate_normalized_events() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        assert!(
            manager
                .validate_config(r#"{"hooks":{"not-real":[]}}"#)
                .unwrap_err()
                .to_string()
                .contains("unknown hook event")
        );
        assert!(
            manager
                .validate_config(r#"{"hooks":{"tool-call":[],"tool_call":[]}}"#)
                .unwrap_err()
                .to_string()
                .contains("duplicate hook event")
        );
    }

    #[test]
    fn refresh_uses_content_generation_not_mtime_only() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(HOOKS_JSON_FILE);
        std::fs::write(&path, r#"{"timeout_ms": 1000, "hooks": {}}"#).unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        manager.refresh_if_changed().unwrap();
        let first_digest = manager.state.read().hooks_json_digest;

        std::fs::write(&path, r#"{"timeout_ms": 1200, "hooks": {}}"#).unwrap();
        manager.refresh_if_changed().unwrap();
        let state = manager.state.read();
        assert_ne!(state.hooks_json_digest, first_digest);
    }

    #[test]
    fn refresh_rejects_oversized_file_without_replacing_generation() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(HOOKS_JSON_FILE);
        std::fs::write(&path, r#"{"timeout_ms": 1200, "hooks": {}}"#).unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        manager.refresh_if_changed().unwrap();

        std::fs::write(&path, vec![b' '; MAX_HOOKS_FILE_BYTES as usize + 1]).unwrap();
        assert!(manager.refresh_if_changed().is_err());
    }

    // ── run_action edge cases ───────────────────────────────────

    #[tokio::test]
    async fn run_action_empty_command_fails() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let action = HookAction {
            command: "".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: Some(1000),
            stdin_json: false,
        };
        let result = manager.run_action(HookEvent::ToolCall, &json!({}), 1000, &action).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn run_action_success() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let action = HookAction {
            command: "true".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: Some(2000),
            stdin_json: false,
        };
        let result = manager
            .run_action(HookEvent::AgentStart, &json!({"msg": "hi"}), 2000, &action)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_action_failure_exit_code() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let action = HookAction {
            command: "false".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: Some(2000),
            stdin_json: false,
        };
        let result = manager.run_action(HookEvent::Error, &json!({}), 2000, &action).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exited with status"));
    }

    #[tokio::test]
    async fn run_action_timeout() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let action = HookAction {
            command: "sleep".into(),
            args: vec!["10".into()],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: Some(100),
            stdin_json: false,
        };
        let result = manager.run_action(HookEvent::ToolCall, &json!({}), 100, &action).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_action_timeout_kills_reaps_and_removes_payload_file() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        let action = HookAction {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "echo $$ > hook.pid; sleep 10 & echo $! > hook-descendant.pid; \
                 echo \"$ZERO_HOOK_PAYLOAD_FILE\" > payload.path; wait"
                    .into(),
            ],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: Some(250),
            stdin_json: false,
        };

        let result = manager.run_action(HookEvent::ToolCall, &json!({}), 250, &action).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let pid = std::fs::read_to_string(temp.path().join("hook.pid"))
            .unwrap()
            .trim()
            .to_string();
        for _ in 0..200 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "hook child was not reaped"
        );
        let descendant_pid = std::fs::read_to_string(temp.path().join("hook-descendant.pid"))
            .unwrap()
            .trim()
            .to_string();
        for _ in 0..200 {
            if !Path::new(&format!("/proc/{descendant_pid}")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !Path::new(&format!("/proc/{descendant_pid}")).exists(),
            "hook descendant process was not terminated"
        );
        let payload_path = std::fs::read_to_string(temp.path().join("payload.path")).unwrap();
        assert!(!Path::new(payload_path.trim()).exists(), "payload temp file leaked");
    }

    // ── emit with no hooks ──────────────────────────────────────

    #[tokio::test]
    async fn emit_no_hooks_does_not_panic() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        manager.emit(HookEvent::AgentStart, json!({"test": true})).await;
        // Should not panic even with no hooks.json
    }

    #[tokio::test]
    async fn run_action_large_payload_uses_file_and_stdin_without_env_overflow() {
        let temp = TempDir::new().unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());

        let action = HookAction {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "test -n \"$ZERO_HOOK_PAYLOAD_FILE\" && test -f \"$ZERO_HOOK_PAYLOAD_FILE\"".to_string(),
            ],
            env: std::collections::HashMap::new(),
            cwd: None,
            timeout_ms: Some(5_000),
            stdin_json: true,
        };

        let payload = json!({ "blob": "x".repeat(3_000_000) });
        let result = manager
            .run_action(HookEvent::ToolCallStart, &payload, 5_000, &action)
            .await;
        assert!(
            result.is_ok(),
            "large payload hook should succeed via file/stdin: {result:?}"
        );
    }
}

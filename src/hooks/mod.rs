use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const HOOKS_JSON_FILE: &str = "hooks.json";
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_STDERR_CHARS: usize = 400;

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
    pub fn as_str(self) -> &'static str {
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

fn default_stdin_json() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    hooks: HashMap<String, Vec<HookAction>>,
}

#[derive(Debug, Clone)]
struct HookConfig {
    enabled: bool,
    timeout_ms: u64,
    hooks: HashMap<String, Vec<HookAction>>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            hooks: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RuntimeState {
    config: HookConfig,
    hooks_json_mtime: Option<SystemTime>,
}

pub struct HookManager {
    workspace_dir: PathBuf,
    hooks_json_path: PathBuf,
    state: RwLock<RuntimeState>,
    /// Optional WASM hook executor for plugins with hook capability.
    #[cfg(feature = "wasm-plugins")]
    wasm_executor: tokio::sync::RwLock<
        Option<std::sync::Arc<crate::plugins::capabilities::hook::WasmHookExecutor>>,
    >,
    /// Optional event bus for bridging lifecycle events to inter-plugin messaging.
    #[cfg(feature = "wasm-plugins")]
    event_bus: tokio::sync::RwLock<Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>>,
}

impl HookManager {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self {
            hooks_json_path: workspace_dir.join(HOOKS_JSON_FILE),
            workspace_dir,
            state: RwLock::new(RuntimeState::default()),
            #[cfg(feature = "wasm-plugins")]
            wasm_executor: tokio::sync::RwLock::new(None),
            #[cfg(feature = "wasm-plugins")]
            event_bus: tokio::sync::RwLock::new(None),
        }
    }

    /// Set the WASM hook executor for lifecycle event observation.
    #[cfg(feature = "wasm-plugins")]
    pub async fn set_wasm_executor(
        &self,
        executor: std::sync::Arc<crate::plugins::capabilities::hook::WasmHookExecutor>,
    ) {
        *self.wasm_executor.write().await = Some(executor);
    }

    /// Set the event bus to bridge lifecycle events into inter-plugin topics.
    #[cfg(feature = "wasm-plugins")]
    pub async fn set_event_bus(&self, bus: std::sync::Arc<crate::plugins::event_bus::EventBus>) {
        *self.event_bus.write().await = Some(bus);
    }

    pub async fn emit(&self, event: HookEvent, payload: serde_json::Value) {
        if let Err(err) = self.refresh_if_changed() {
            tracing::warn!(error = %err, "hooks refresh failed");
        } else {
            let (enabled, timeout_ms, actions) = {
                let state = self.state.read();
                (
                    state.config.enabled,
                    state.config.timeout_ms,
                    state
                        .config
                        .hooks
                        .get(event.as_str())
                        .cloned()
                        .unwrap_or_default(),
                )
            };

            if enabled && !actions.is_empty() {
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

        // Also fire WASM hook plugins (if feature enabled and executor configured).
        #[cfg(feature = "wasm-plugins")]
        {
            let executor = self.wasm_executor.read().await;
            if let Some(ref exec) = *executor {
                let payload_str = payload.to_string();
                exec.emit(event.as_str(), &payload_str).await;
            }
        }

        // Bridge lifecycle event to the event bus under `prx.lifecycle.<event>`.
        #[cfg(feature = "wasm-plugins")]
        {
            let bus_guard = self.event_bus.read().await;
            if let Some(ref bus) = *bus_guard {
                let topic = format!("prx.lifecycle.{}", event.as_str());
                let payload_str = payload.to_string();
                let bus = bus.clone();
                tokio::spawn(async move {
                    if let Err(e) = bus.publish(&topic, &payload_str).await {
                        tracing::debug!(topic = %topic, error = %e, "event bus lifecycle bridge error");
                    }
                });
            }
        }
    }

    fn refresh_if_changed(&self) -> anyhow::Result<()> {
        let latest_mtime = file_mtime(&self.hooks_json_path)?;
        let current_mtime = self.state.read().hooks_json_mtime;
        if latest_mtime == current_mtime {
            return Ok(());
        }

        if latest_mtime.is_none() {
            let mut state = self.state.write();
            state.config = HookConfig::default();
            state.hooks_json_mtime = None;
            return Ok(());
        }

        let raw = std::fs::read_to_string(&self.hooks_json_path)?;
        let parsed: HooksFile = serde_json::from_str(&raw)?;
        let config = HookConfig {
            enabled: parsed.enabled.unwrap_or(true),
            timeout_ms: parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            hooks: parsed
                .hooks
                .into_iter()
                .map(|(name, actions)| (normalize_event_name(&name), actions))
                .collect(),
        };

        let mut state = self.state.write();
        state.config = config;
        state.hooks_json_mtime = latest_mtime;
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

        let mut cmd = Command::new(command);
        cmd.args(&action.args);
        cmd.env("ZERO_HOOK_EVENT", event.as_str());
        cmd.env("ZERO_HOOK_PAYLOAD", payload.to_string());
        if !action.env.is_empty() {
            cmd.envs(action.env.clone());
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

        let mut child = cmd.spawn()?;

        if action.stdin_json {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(payload.to_string().as_bytes()).await?;
            }
        }

        let timeout_ms = action.timeout_ms.unwrap_or(default_timeout_ms);
        let output =
            tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
                .await
                .map_err(|_| anyhow::anyhow!("hook timed out after {timeout_ms} ms"))??;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = truncate_for_log(stderr.as_ref(), MAX_STDERR_CHARS);
        anyhow::bail!(
            "hook exited with status {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(", stderr: {stderr}")
            }
        )
    }
}

fn normalize_event_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

fn file_mtime(path: &Path) -> anyhow::Result<Option<SystemTime>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(path)?;
    Ok(metadata.modified().ok())
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
  "enabled": true,
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
        assert!(state.config.enabled);
        assert_eq!(state.config.timeout_ms, 1200);
        assert!(state.config.hooks.contains_key("tool_call_start"));
    }
}

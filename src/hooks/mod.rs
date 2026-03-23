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
    wasm_executor: tokio::sync::RwLock<Option<std::sync::Arc<crate::plugins::capabilities::hook::WasmHookExecutor>>>,
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
                    state.config.hooks.get(event.as_str()).cloned().unwrap_or_default(),
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

        let payload_json = payload.to_string();
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

        // Always provide a temp payload file path as a stable fallback channel.
        let payload_file_path = std::env::temp_dir().join(format!(
            "openprx-hook-payload-{}-{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        if let Err(e) = std::fs::write(&payload_file_path, payload_json.as_bytes()) {
            tracing::warn!("Failed to write hook payload temp file {:?}: {e}", payload_file_path);
        } else {
            cmd.env("ZERO_HOOK_PAYLOAD_FILE", &payload_file_path);
        }

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

        let mut child = cmd.spawn()?;

        if action.stdin_json {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(e) = stdin.write_all(payload_json.as_bytes()).await {
                    // Hook command may not consume stdin; fallback file path is still available.
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(e.into());
                    }
                }
            }
        }

        let timeout_ms = action.timeout_ms.unwrap_or(default_timeout_ms);
        let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("hook timed out after {timeout_ms} ms"))??;

        let _ = std::fs::remove_file(&payload_file_path);

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
        assert!(state.config.enabled);
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
    fn refresh_disabled_flag() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join(HOOKS_JSON_FILE), r#"{"enabled": false, "hooks": {}}"#).unwrap();
        let manager = HookManager::new(temp.path().to_path_buf());
        manager.refresh_if_changed().unwrap();
        let state = manager.state.read();
        assert!(!state.config.enabled);
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

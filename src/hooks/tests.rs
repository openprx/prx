#![allow(clippy::indexing_slicing)]

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use super::config::HookAction;
use super::hook_core::HookPolicy;
use super::event::{HookEvent, normalize_event_name, payload_error, redacted_hook_payload};
use super::manager::HookManager;
use super::tools::HooksManageTool;
use super::{DEFAULT_TIMEOUT_MS, HOOKS_JSON_FILE, MAX_HOOKS_FILE_BYTES, WORKSPACE_HOOKS_OPT_IN, truncate_for_log};
use crate::tools::Tool;
use tempfile::TempDir;

/// A manager whose workspace hooks run (the operator opted in), with its
/// workspace and launch root inside `temp`.
/// Generous bound for a delivery the tests wait on; reached only on failure.
const TEST_DRAIN: Duration = Duration::from_secs(30);

fn allowed_manager(temp: &TempDir) -> HookManager {
    HookManager::new(temp.path().join("workspace")).allowing_workspace_hooks(temp.path().join("hooks"))
}

/// The shared action contract every action-dispatched tool is held to:
/// the tool-list description and the `action` parameter's own description
/// must each name every enumerated action, and no parameter may ship
/// undescribed.
#[tokio::test]
async fn hooks_manage_action_tool_contract() {
    let temp = TempDir::new().unwrap();
    let manager = std::sync::Arc::new(HookManager::new(temp.path().to_path_buf()));
    let tool = HooksManageTool {
        manager,
        security: std::sync::Arc::new(crate::security::SecurityPolicy::default()),
    };
    crate::tools::schema::assert_flat_execution_contract(&tool, tool.execution_schema()).await;
    crate::tools::schema::action_contract::assert_action_tool_contract(
        tool.name(),
        tool.description(),
        tool.execution_schema(),
    );
}

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
        assert!(!e.as_str().is_empty(), "expected a non-empty value");
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
    let s = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}, \u{44d}\u{442}\u{43e} \u{434}\u{43b}\u{438}\u{43d}\u{43d}\u{430}\u{44f} \u{441}\u{442}\u{440}\u{43e}\u{43a}\u{430}";
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

#[test]
fn append_control_tools_publishes_complete_live_surface() {
    let temp = TempDir::new().unwrap();
    let manager = std::sync::Arc::new(HookManager::new(temp.path().to_path_buf()));
    let mut tools = Vec::new();
    manager.append_control_tools(
        &mut tools,
        std::sync::Arc::new(crate::security::SecurityPolicy::default()),
    );

    let names = tools.iter().map(|tool| tool.name()).collect::<Vec<_>>();
    assert_eq!(names, vec!["hooks_status", "hooks_manage"]);
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
    let manager = allowed_manager(&temp);
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
    let manager = allowed_manager(&temp);
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
    let manager = allowed_manager(&temp);
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
    let manager = allowed_manager(&temp);
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
    let manager = allowed_manager(&temp);
    let out = temp.path().display().to_string();
    let action = HookAction {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "echo $$ > '{out}/hook.pid'; sleep 10 & echo $! > '{out}/hook-descendant.pid'; \
                 echo \"$ZERO_HOOK_PAYLOAD_FILE\" > '{out}/payload.path'; wait"
            ),
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

/// Write `hooks.json` into `workspace` with one `turn_complete` hook that
/// runs `sh -c <script>`.
fn write_turn_complete_hook(workspace: &Path, script: &str) {
    std::fs::create_dir_all(workspace).unwrap();
    let hooks = json!({
        "hooks": {"turn_complete": [{"command": "sh", "args": ["-c", script], "stdin_json": false}]}
    });
    std::fs::write(workspace.join("hooks.json"), hooks.to_string()).unwrap();
}

/// RED before HOOKS-ENV (D-085 O-1): a workspace `hooks.json` (which the
/// agent's file tools can write) ran its commands with no operator
/// consent. Without `security.workspace_hooks = "on"` nothing runs now,
/// and the diagnostics say how to allow it.
#[tokio::test]
async fn workspace_hooks_do_not_run_without_the_operator_opt_in() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let sentinel = temp.path().join("hook-ran");
    write_turn_complete_hook(&workspace, &format!("touch '{}'", sentinel.display()));
    let manager = HookManager::new(workspace.clone());

    manager.emit(HookEvent::TurnComplete, json!({"turn": "t"})).await;
    assert!(manager.drain(TEST_DRAIN).await, "the event was delivered");

    assert!(!sentinel.exists(), "a workspace hook ran without the operator's opt-in");
    let diagnostics = manager.diagnostics();
    assert!(!diagnostics.runs);
    let blocked = diagnostics.blocked.expect("test: a blocked note");
    assert!(blocked.contains(WORKSPACE_HOOKS_OPT_IN), "{blocked}");

    let allowed = HookManager::new(workspace).allowing_workspace_hooks(temp.path().join("hooks"));
    allowed.emit(HookEvent::TurnComplete, json!({"turn": "t"})).await;
    assert!(allowed.drain(TEST_DRAIN).await, "the event was delivered");
    assert!(sentinel.exists(), "with the opt-in the hook runs");
    assert!(allowed.diagnostics().blocked.is_none());
}

/// RED before HOOKS-ENV: a hook inherited PRX's whole environment (every
/// provider key exported in its shell) and ran in the workspace. It now
/// starts from the cleared launch environment plus its own `env`, in its
/// dedicated directory.
#[tokio::test]
async fn a_hook_starts_in_a_cleared_environment_and_its_dedicated_directory() {
    let temp = TempDir::new().unwrap();
    let allowlist = ["PATH", "HOME", "LANG", "TMPDIR"];
    let parent_only = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .find(|name| !allowlist.contains(&name.as_str()) && !name.starts_with("LC_"))
        .expect("test: the test process has a variable outside the allowlist");
    let out = temp.path().join("seen");
    let manager = allowed_manager(&temp);
    let action = HookAction {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            format!("env > '{0}.env'; pwd -P > '{0}.cwd'", out.display()),
        ],
        env: HashMap::from([
            ("HOOK_OWN".to_string(), "own".to_string()),
            ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
            ("npm_config_registry".to_string(), "https://evil.example".to_string()),
        ]),
        cwd: None,
        timeout_ms: Some(5_000),
        stdin_json: false,
    };
    manager
        .run_action(HookEvent::TurnComplete, &json!({}), 5_000, &action)
        .await
        .expect("test: the hook runs");

    let environment = std::fs::read_to_string(format!("{}.env", out.display())).unwrap();
    let names = environment
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_string()))
        .collect::<Vec<_>>();
    assert!(names.contains(&"HOOK_OWN".to_string()), "{names:?}");
    assert!(names.contains(&"ZERO_HOOK_EVENT".to_string()), "{names:?}");
    assert!(
        !names.iter().any(|name| *name == parent_only),
        "`{parent_only}` reached the hook: {names:?}"
    );
    for passed in ["LD_PRELOAD", "npm_config_registry"] {
        assert!(names.iter().any(|name| name == passed), "`{passed}` was dropped: {names:?}");
    }
    let cwd = std::fs::read_to_string(format!("{}.cwd", out.display())).unwrap();
    let expected =
        crate::config::mcp_launch::launch_directory(&temp.path().join("hooks"), "workspace-hooks", None).unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd.trim()).unwrap(),
        std::fs::canonicalize(expected).unwrap(),
        "the hook must start in its dedicated directory, not the workspace"
    );
}

/// HOOKS-ENV: without a configuration directory there is no launch
/// directory, and a hook is refused rather than started in the workspace.
#[tokio::test]
async fn a_hook_without_a_launch_root_is_refused() {
    let temp = TempDir::new().unwrap();
    let manager = HookManager::new(temp.path().to_path_buf());
    *manager.policy.write() = HookPolicy::Fixed {
        allowed: true,
        launch_root: None,
    };
    let action = HookAction {
        command: "true".into(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        timeout_ms: Some(1_000),
        stdin_json: false,
    };
    let refused = manager
        .run_action(HookEvent::TurnComplete, &json!({}), 1_000, &action)
        .await
        .expect_err("test: refused");
    assert!(refused.to_string().contains("no launch directory"), "{refused}");
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
    let manager = allowed_manager(&temp);

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

/// USERINFO-2: hook scripts receive the provider label and error text of
/// every LLM and agent event. A `custom:` URL may carry `user:password@`;
/// the payload a hook sees names the endpoint without it.
#[tokio::test]
async fn a_hook_payload_names_the_provider_without_url_userinfo() {
    let temp = TempDir::new().unwrap();
    let manager = allowed_manager(&temp);
    let seen_path = temp.path().join("payload.json");
    let action = HookAction {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            format!("cat \"$ZERO_HOOK_PAYLOAD_FILE\" > '{}'", seen_path.display()),
        ],
        env: HashMap::new(),
        cwd: None,
        timeout_ms: Some(5_000),
        stdin_json: false,
    };
    let label = "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1";
    let payloads = [
        json!({"provider": label, "model": "m"}),
        json!({"provider": label, "model": "m", "success": false, "error_message": format!("provider={label} failed")}),
        payload_error("llm", &format!("Primary provider \"{label}\" unavailable")),
    ];
    for payload in payloads {
        manager
            .run_action(HookEvent::LlmResponse, &payload, 5_000, &action)
            .await
            .expect("test: the hook runs");
        let seen = std::fs::read_to_string(&seen_path).expect("test: payload written");
        assert!(!seen.contains("SECRET-fixture-pw"), "the hook saw the password: {seen}");
        assert!(seen.contains("custom:http://[redacted]@127.0.0.1:1/v1"), "{seen}");
    }
}

/// R-C2BU F-2: the same payload fans out to WASM lifecycle hooks and the
/// plugin event bus. Third-party plugin code and every bus subscriber must
/// see the provider label without its userinfo, like a command hook does.
#[cfg(feature = "wasm-plugins")]
#[tokio::test]
async fn the_plugin_event_bus_receives_the_provider_without_url_userinfo() {
    let temp = TempDir::new().unwrap();
    let manager = HookManager::new(temp.path().to_path_buf());
    let runtime = crate::plugins::init_test_plugin_runtime(temp.path(), None)
        .await
        .expect("test: plugin runtime");
    let (_id, mut events) = runtime
        .event_bus()
        .subscribe("fixture", "prx.lifecycle.*")
        .await
        .expect("test: subscribe");
    manager.set_plugin_runtime(runtime).await;

    manager
        .emit(
            HookEvent::LlmResponse,
            json!({
                "provider": "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1",
                "error_message": "provider=custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1 failed",
            }),
        )
        .await;
    let delivered = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("test: the bus delivers within 5 s")
        .expect("test: a lifecycle event");
    assert!(
        delivered.payload.contains("[redacted]@127.0.0.1:1"),
        "{}",
        delivered.payload
    );
    assert!(
        !delivered.payload.contains("SECRET-fixture-pw"),
        "{}",
        delivered.payload
    );
}

/// USERINFO-3: the `llm_request` and `llm_response` middleware envelopes
/// name the serving provider, and a `custom:` label may carry
/// `user:password@`. The middleware guest is third-party code like a
/// lifecycle hook, so it must see the label redacted as well. The
/// committed fixture echoes the envelope it received, so its answer is
/// what the guest saw.
#[cfg(feature = "wasm-plugins")]
#[tokio::test]
async fn a_middleware_guest_receives_the_provider_without_url_userinfo() {
    use crate::plugins::capabilities::middleware::MiddlewareStage;

    let temp = TempDir::new().expect("test: temporary workspace");
    let plugin_dir = temp.path().join("plugins/pipeline-middleware");
    std::fs::create_dir_all(&plugin_dir).expect("test: fixture plugin directory");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
    std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).expect("test: fixture manifest");
    std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).expect("test: fixture component");
    let runtime = crate::plugins::init_test_plugin_runtime(temp.path(), None)
        .await
        .expect("test: plugin runtime");
    let manager = HookManager::new(temp.path().to_path_buf());
    manager.set_plugin_runtime(runtime).await;

    for stage in [MiddlewareStage::LlmRequest, MiddlewareStage::LlmResponse] {
        let seen = manager
            .process_wasm_middleware(
                stage,
                json!({
                    "provider": "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1",
                    "model": "m",
                    "messages": [],
                    "text": "t",
                }),
            )
            .await;
        assert!(
            seen.get("middleware_probe").is_some(),
            "{}: premise: the guest ran and echoed its input: {seen}",
            stage.as_str()
        );
        assert_eq!(
            seen.get("provider").and_then(serde_json::Value::as_str),
            Some("custom:http://[redacted]@127.0.0.1:1/v1"),
            "{}: the guest must not see the provider's URL userinfo",
            stage.as_str()
        );
        assert!(!seen.to_string().contains("SECRET-fixture-pw"), "{seen}");
    }
}

/// R-C2BUg G-1: hook error texts that start with a URL keep none of their
/// credentials.
#[test]
fn hook_texts_redact_every_url_in_url_first_text() {
    crate::providers::label_redaction_tests::assert_every_url_redacted("hook payload", &|text| {
        redacted_hook_payload(&json!({"message": text, "error": text, "error_message": text})).to_string()
    });
}

// ── background dispatch (finding 96 a) ──────────────────────

/// Write a `turn_complete` hook that waits for `gate` to exist, then
/// appends the payload file's content and a newline to `log`.
fn write_gated_logging_hook(workspace: &Path, gate: &Path, log: &Path) {
    std::fs::create_dir_all(workspace).unwrap();
    let script = format!(
        "while [ ! -e '{gate}' ]; do sleep 0.02; done; cat \"$ZERO_HOOK_PAYLOAD_FILE\" >> '{log}'; echo >> '{log}'",
        gate = gate.display(),
        log = log.display()
    );
    let hooks = json!({
        "timeout_ms": 60_000,
        "hooks": {"turn_complete": [{"command": "sh", "args": ["-c", script], "stdin_json": false}]}
    });
    std::fs::write(workspace.join("hooks.json"), hooks.to_string()).unwrap();
}

fn logged_seqs(log: &Path) -> Vec<u64> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value["seq"].as_u64())
        .collect()
}

#[tokio::test]
async fn a_blocked_hook_does_not_delay_emit_and_is_delivered_later() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (gate, log) = (temp.path().join("gate"), temp.path().join("log"));
    write_gated_logging_hook(&workspace, &gate, &log);
    let manager = HookManager::new(workspace).allowing_workspace_hooks(temp.path().join("hooks"));

    // The hook cannot finish before the gate opens; emit must return anyway.
    manager.emit(HookEvent::TurnComplete, json!({"seq": 1})).await;
    manager.emit(HookEvent::TurnComplete, json!({"seq": 2})).await;
    let stats = manager.dispatch_stats();
    assert_eq!((stats.queued, stats.delivered, stats.dropped), (2, 0, 0));
    assert!(logged_seqs(&log).is_empty(), "the gated hook cannot have run yet");

    std::fs::write(&gate, b"open").unwrap();
    assert!(manager.drain(TEST_DRAIN).await);
    assert_eq!(logged_seqs(&log), vec![1, 2]);
    assert_eq!(manager.dispatch_stats().delivered, 2);
}

#[tokio::test]
async fn events_reach_the_hook_in_emit_order() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (gate, log) = (temp.path().join("gate"), temp.path().join("log"));
    std::fs::write(&gate, b"open").unwrap();
    write_gated_logging_hook(&workspace, &gate, &log);
    let manager = HookManager::new(workspace).allowing_workspace_hooks(temp.path().join("hooks"));

    for seq in 1..=12u64 {
        manager.emit(HookEvent::TurnComplete, json!({"seq": seq})).await;
    }
    assert!(manager.drain(TEST_DRAIN).await);
    assert_eq!(logged_seqs(&log), (1..=12).collect::<Vec<_>>());
    assert_eq!(manager.dispatch_stats().dropped, 0);
}

#[tokio::test]
async fn a_full_queue_drops_and_counts_instead_of_blocking() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (gate, log) = (temp.path().join("gate"), temp.path().join("log"));
    write_gated_logging_hook(&workspace, &gate, &log);
    let manager =
        HookManager::with_queue_capacity(workspace, 2).allowing_workspace_hooks(temp.path().join("hooks"));

    for seq in 1..=10u64 {
        manager.emit(HookEvent::TurnComplete, json!({"seq": seq})).await;
    }
    let stats = manager.dispatch_stats();
    assert_eq!(stats.queued + stats.dropped, 10);
    // Two queue slots plus the one event the worker may already hold.
    assert!(stats.queued <= 3, "{stats:?}");
    assert!(stats.dropped >= 7, "{stats:?}");

    std::fs::write(&gate, b"open").unwrap();
    assert!(manager.drain(TEST_DRAIN).await);
    let delivered = logged_seqs(&log);
    assert_eq!(delivered.len() as u64, stats.queued);
    assert!(delivered.windows(2).all(|pair| pair[0] < pair[1]), "{delivered:?}");
    assert_eq!(manager.dispatch_stats().delivered, stats.queued);
}

#[tokio::test]
async fn drain_is_bounded_and_delivers_what_was_queued() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (gate, log) = (temp.path().join("gate"), temp.path().join("log"));
    write_gated_logging_hook(&workspace, &gate, &log);
    let manager = HookManager::new(workspace).allowing_workspace_hooks(temp.path().join("hooks"));
    // Nothing queued: nothing to wait for.
    assert!(manager.drain(Duration::from_millis(10)).await);

    for seq in 1..=3u64 {
        manager.emit(HookEvent::TurnComplete, json!({"seq": seq})).await;
    }
    // The hook is blocked, so a short bound expires without a result.
    assert!(!manager.drain(Duration::from_millis(100)).await);
    assert_eq!(logged_seqs(&log), Vec::<u64>::new());

    // Once the hook can finish, a drain covers everything queued.
    std::fs::write(&gate, b"open").unwrap();
    assert!(manager.drain(TEST_DRAIN).await);
    assert_eq!(logged_seqs(&log), vec![1, 2, 3]);
}

#[tokio::test]
async fn shutdown_drain_delivers_an_event_emitted_just_before_exit() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (gate, log) = (temp.path().join("gate"), temp.path().join("log"));
    std::fs::write(&gate, b"open").unwrap();
    write_gated_logging_hook(&workspace, &gate, &log);
    let manager = HookManager::new(workspace).allowing_workspace_hooks(temp.path().join("hooks"));

    // The helper every owner (worker, CLI, chat, channels, gateway) calls
    // on its exit path: nothing emitted before it may be lost.
    manager.emit(HookEvent::TurnComplete, json!({"seq": 1})).await;
    manager.drain_for_shutdown().await;
    assert_eq!(logged_seqs(&log), vec![1]);
}

#[tokio::test]
async fn a_delivery_that_never_completes_does_not_stall_the_next_event() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let marker = temp.path().join("second-ran");
    std::fs::create_dir_all(&workspace).unwrap();
    let hooks = json!({
        "timeout_ms": 60_000,
        "hooks": {
            "turn_complete": [{"command": "sh", "args": ["-c", "sleep 30"], "stdin_json": false}],
            "error": [{"command": "sh", "args": ["-c", format!("touch '{}'", marker.display())], "stdin_json": false}]
        }
    });
    std::fs::write(workspace.join("hooks.json"), hooks.to_string()).unwrap();
    let manager = HookManager::new(workspace)
        .allowing_workspace_hooks(temp.path().join("hooks"))
        .with_delivery_bound(Duration::from_millis(300));

    manager.emit(HookEvent::TurnComplete, json!({})).await;
    manager.emit(HookEvent::Error, json!({})).await;
    assert!(
        manager.drain(TEST_DRAIN).await,
        "the stuck event must not hold the queue"
    );
    assert!(
        marker.exists(),
        "the next event was delivered after the first timed out"
    );
    assert_eq!(manager.dispatch_stats().delivered, 2);
}

#[cfg(unix)]
#[tokio::test]
async fn an_abandoned_delivery_kills_and_reaps_the_hook_process() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let pid_file = temp.path().join("hook.pid");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
    let hooks = json!({
        "timeout_ms": 60_000,
        "hooks": {"turn_complete": [{"command": "sh", "args": ["-c", script], "stdin_json": false}]}
    });
    std::fs::write(workspace.join("hooks.json"), hooks.to_string()).unwrap();
    let manager = HookManager::new(workspace)
        .allowing_workspace_hooks(temp.path().join("hooks"))
        .with_delivery_bound(Duration::from_secs(3));

    manager.emit(HookEvent::TurnComplete, json!({})).await;
    assert!(manager.drain(TEST_DRAIN).await);
    let pid = std::fs::read_to_string(&pid_file)
        .expect("test: hook started")
        .trim()
        .to_string();
    let mut gone = false;
    for _ in 0..100 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !alive {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the abandoned hook process {pid} is still running");
}

#[tokio::test]
async fn replacing_hooks_json_applies_to_later_events() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let (first, second) = (temp.path().join("first"), temp.path().join("second"));
    write_turn_complete_hook(&workspace, &format!("touch '{}'", first.display()));
    let manager = HookManager::new(workspace.clone()).allowing_workspace_hooks(temp.path().join("hooks"));

    manager.emit(HookEvent::TurnComplete, json!({})).await;
    assert!(manager.drain(TEST_DRAIN).await);
    assert!(first.exists() && !second.exists());

    let replacement = json!({
        "hooks": {"turn_complete": [{
            "command": "sh", "args": ["-c", format!("touch '{}'", second.display())], "stdin_json": false
        }]}
    });
    std::fs::remove_file(&first).unwrap();
    manager.replace_config(&replacement.to_string()).unwrap();
    manager.emit(HookEvent::TurnComplete, json!({})).await;
    assert!(manager.drain(TEST_DRAIN).await);
    assert!(second.exists() && !first.exists());
}

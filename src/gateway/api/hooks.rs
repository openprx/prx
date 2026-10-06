use super::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::hooks::HookChange;

const HOOKS_JSON_FILE: &str = "hooks.json";
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 5_000;
const MIN_HOOK_TIMEOUT_MS: u64 = 1_000;
const VALID_HOOK_EVENTS: &[&str] = &[
    "agent_start",
    "agent_end",
    "llm_request",
    "llm_response",
    "tool_call_start",
    "tool_call",
    "turn_complete",
    "error",
];

// ── Wire types ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HookActionEntry {
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    hooks: HashMap<String, Vec<HookActionEntry>>,
}

// ── API response types ────────────────────────────────────

#[derive(Serialize)]
struct HookItem {
    id: String,
    event: String,
    command: String,
    timeout_ms: u64,
}

#[derive(Serialize)]
pub(super) struct HooksResponse {
    hooks: Vec<HookItem>,
}

// ── API request types ─────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateHookRequest {
    event: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
pub struct UpdateHookRequest {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

// ── Helpers ───────────────────────────────────────────────

fn hooks_json_path(state: &AppState) -> std::path::PathBuf {
    let config = state.config.load_full();
    config.workspace_dir.join(HOOKS_JSON_FILE)
}

fn read_hooks_file(state: &AppState) -> Result<HooksFile, (StatusCode, Json<serde_json::Value>)> {
    let path = hooks_json_path(state);
    if !path.exists() {
        return Ok(HooksFile::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        tracing::warn!("Failed to read hooks.json: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to read hooks configuration"})),
        )
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        tracing::warn!("Failed to parse hooks.json: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Invalid hooks configuration format"})),
        )
    })
}

fn write_hooks_file(state: &AppState, file: &HooksFile) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let path = hooks_json_path(state);
    let content = serde_json::to_string_pretty(file).map_err(|e| {
        tracing::warn!("Failed to serialize hooks: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to save hooks configuration"})),
        )
    })?;
    std::fs::write(&path, content).map_err(|e| {
        tracing::warn!("Failed to write hooks.json: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to save hooks configuration"})),
        )
    })
}

fn normalize_event_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

fn validate_hook_event(raw_event: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let normalized = normalize_event_name(raw_event);
    if VALID_HOOK_EVENTS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Unsupported hook event '{raw_event}'")
            })),
        ))
    }
}

fn validate_hook_command(raw_command: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let command = raw_command.trim().to_string();
    if command.is_empty() {
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Hook command must not be empty"})),
        ))
    } else {
        Ok(command)
    }
}

fn validate_hook_timeout(timeout_ms: Option<u64>) -> Result<Option<u64>, (StatusCode, Json<serde_json::Value>)> {
    match timeout_ms {
        Some(value) if value < MIN_HOOK_TIMEOUT_MS => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Hook timeout must be at least {MIN_HOOK_TIMEOUT_MS} ms")
            })),
        )),
        other => Ok(other),
    }
}

fn hooks_file_to_items(file: &HooksFile) -> Vec<HookItem> {
    let global_timeout = file.timeout_ms.unwrap_or(DEFAULT_HOOK_TIMEOUT_MS);
    let mut items = Vec::new();

    for (event, actions) in &file.hooks {
        for (idx, action) in actions.iter().enumerate() {
            let id = if actions.len() == 1 {
                event.clone()
            } else {
                format!("{event}:{idx}")
            };
            items.push(HookItem {
                id,
                event: event.clone(),
                command: action.command.clone(),
                timeout_ms: action.timeout_ms.unwrap_or(global_timeout),
            });
        }
    }
    items.sort_by(|a, b| a.event.cmp(&b.event).then(a.id.cmp(&b.id)));
    items
}

/// What an update changes, in the one classification of hooks.json changes
/// (D-096): a new program, arguments or event is a command change; a
/// timeout alone only changes a limit.
const fn update_change(req: &UpdateHookRequest) -> HookChange {
    if req.command.is_some() || req.args.is_some() || req.event.is_some() {
        HookChange::Command
    } else {
        HookChange::Restriction
    }
}

// ── Handlers ──────────────────────────────────────────────

pub async fn get_hooks(
    State(state): State<AppState>,
) -> Result<Json<HooksResponse>, (StatusCode, Json<serde_json::Value>)> {
    let file = read_hooks_file(&state)?;
    Ok(Json(HooksResponse {
        hooks: hooks_file_to_items(&file),
    }))
}

pub async fn create_hook(
    State(state): State<AppState>,
    Json(req): Json<CreateHookRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let mut file = read_hooks_file(&state)?;
    let event = validate_hook_event(&req.event)?;
    let command = validate_hook_command(&req.command)?;
    let timeout_ms = validate_hook_timeout(req.timeout_ms)?;
    let action = HookActionEntry {
        command,
        args: req.args,
        env: HashMap::new(),
        cwd: None,
        timeout_ms,
        stdin_json: true,
    };
    super::authorize_resource_mutation(&state, "gateway_api:hooks:create", HookChange::Command.risk())?;
    file.hooks.entry(event.clone()).or_default().push(action);
    write_hooks_file(&state, &file)?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"status": "created", "event": event})),
    ))
}

pub async fn update_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateHookRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Classified before the request is taken apart below.
    let change = update_change(&req);
    let mut file = read_hooks_file(&state)?;

    // Parse id: either "event" or "event:idx"
    let (event_key, action_idx) = parse_hook_id(&id);

    let actions = file.hooks.get_mut(&event_key).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Hook '{id}' not found")})),
        )
    })?;

    let action = actions.get_mut(action_idx).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Hook '{id}' not found")})),
        )
    })?;

    if let Some(command) = req.command {
        action.command = validate_hook_command(&command)?;
    }
    if let Some(args) = req.args {
        action.args = args;
    }
    if let Some(timeout_ms) = req.timeout_ms {
        action.timeout_ms = validate_hook_timeout(Some(timeout_ms))?;
    }

    // If event changed, move the action
    if let Some(new_event) = req.event {
        let new_event = validate_hook_event(&new_event)?;
        if new_event != event_key {
            let action = actions.remove(action_idx);
            if actions.is_empty() {
                file.hooks.remove(&event_key);
            }
            file.hooks.entry(new_event).or_default().push(action);
        }
    }

    super::authorize_resource_mutation(&state, "gateway_api:hooks:update", change.risk())?;
    write_hooks_file(&state, &file)?;
    Ok(Json(serde_json::json!({"status": "updated", "id": id})))
}

pub async fn delete_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut file = read_hooks_file(&state)?;
    let (event_key, action_idx) = parse_hook_id(&id);

    let actions = file.hooks.get_mut(&event_key).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Hook '{id}' not found")})),
        )
    })?;

    if action_idx >= actions.len() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Hook '{id}' not found")})),
        ));
    }

    actions.remove(action_idx);
    if actions.is_empty() {
        file.hooks.remove(&event_key);
    }

    super::authorize_resource_mutation(&state, "gateway_api:hooks:delete", HookChange::Restriction.risk())?;
    write_hooks_file(&state, &file)?;
    Ok(Json(serde_json::json!({"status": "deleted", "id": id})))
}

fn parse_hook_id(id: &str) -> (String, usize) {
    if let Some((event, idx_str)) = id.rsplit_once(':') {
        if let Ok(idx) = idx_str.parse::<usize>() {
            return (event.to_string(), idx);
        }
    }
    (id.to_string(), 0)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{json, serve_config};
    use super::*;
    use crate::security::policy::{AutonomyLevel, ResourceRiskLevel};

    const EXISTING: &str = r#"{"hooks":{"turn_complete":[{"command":"/bin/true"}]}}"#;

    fn update(
        event: Option<&str>,
        command: Option<&str>,
        args: Option<Vec<String>>,
        timeout_ms: Option<u64>,
    ) -> UpdateHookRequest {
        UpdateHookRequest {
            event: event.map(str::to_string),
            command: command.map(str::to_string),
            args,
            timeout_ms,
        }
    }

    /// The Gateway classifies a hooks.json change as `hooks_manage` does
    /// (D-096): a command change is High, a removal or a limit Medium.
    #[test]
    fn gateway_hook_changes_share_the_hooks_manage_classification() {
        assert_eq!(HookChange::of_manage_action("replace"), Some(HookChange::Command));
        assert_eq!(HookChange::of_manage_action("test"), Some(HookChange::Command));
        assert_eq!(HookChange::of_manage_action("remove"), Some(HookChange::Restriction));
        assert_eq!(HookChange::Command.risk(), ResourceRiskLevel::High);
        assert_eq!(HookChange::Restriction.risk(), ResourceRiskLevel::Medium);
        for request in [
            update(None, Some("/bin/echo"), None, None),
            update(None, None, Some(vec!["x".to_string()]), None),
            update(Some("agent_start"), None, None, None),
            update(None, Some("/bin/echo"), None, Some(2_000)),
        ] {
            assert_eq!(update_change(&request), HookChange::Command);
        }
        assert_eq!(
            update_change(&update(None, None, None, Some(2_000))),
            HookChange::Restriction
        );
    }

    /// At `supervised` autonomy a Gateway request carries no approval grant,
    /// so every hooks.json change (now Medium or High) is refused and the file
    /// is left as it was; at `full` it goes through.
    #[tokio::test]
    async fn gateway_hook_writes_need_the_hooks_manage_risk() {
        let served = serve_config(|config| config.autonomy.level = AutonomyLevel::Supervised).await;
        let workspace = served.state.config.load_full().workspace_dir.clone();
        let hooks_json = workspace.join(HOOKS_JSON_FILE);
        std::fs::write(&hooks_json, EXISTING).expect("test: write hooks.json");
        for (method, path, body) in [
            (
                reqwest::Method::POST,
                "/api/hooks",
                Some(serde_json::json!({"event": "turn_complete", "command": "/bin/echo"})),
            ),
            (
                reqwest::Method::PUT,
                "/api/hooks/turn_complete:0",
                Some(serde_json::json!({"command": "/bin/echo"})),
            ),
            (
                reqwest::Method::PUT,
                "/api/hooks/turn_complete:0",
                Some(serde_json::json!({"timeout_ms": 2_000})),
            ),
            (reqwest::Method::DELETE, "/api/hooks/turn_complete:0", None),
        ] {
            let (status, body) = served.call(method.clone(), path, body).await;
            assert_eq!(status, 403, "{method} {path} at supervised: {body}");
            assert!(
                json(&body)
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .contains("approval"),
                "{method} {path}: {body}"
            );
            assert_eq!(
                std::fs::read_to_string(&hooks_json).expect("test: read hooks.json"),
                EXISTING,
                "{method} {path} changed hooks.json"
            );
        }

        let full = serve_config(|config| config.autonomy.level = AutonomyLevel::Full).await;
        let (status, body) = full
            .call(
                reqwest::Method::POST,
                "/api/hooks",
                Some(serde_json::json!({"event": "turn_complete", "command": "/bin/echo"})),
            )
            .await;
        assert_eq!(status, 201, "create at full: {body}");
    }
}

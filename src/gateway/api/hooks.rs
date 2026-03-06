use super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const HOOKS_JSON_FILE: &str = "hooks.json";

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

fn default_stdin_json() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    enabled: Option<bool>,
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
    enabled: bool,
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
    let config = state.config.lock();
    config.workspace_dir.join(HOOKS_JSON_FILE)
}

fn read_hooks_file(state: &AppState) -> Result<HooksFile, (StatusCode, Json<serde_json::Value>)> {
    let path = hooks_json_path(state);
    if !path.exists() {
        return Ok(HooksFile::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to read hooks.json: {e}")})),
        )
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to parse hooks.json: {e}")})),
        )
    })
}

fn write_hooks_file(
    state: &AppState,
    file: &HooksFile,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let path = hooks_json_path(state);
    let content = serde_json::to_string_pretty(file).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to serialize hooks: {e}")})),
        )
    })?;
    std::fs::write(&path, content).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write hooks.json: {e}")})),
        )
    })
}

fn hooks_file_to_items(file: &HooksFile) -> Vec<HookItem> {
    let global_enabled = file.enabled.unwrap_or(true);
    let global_timeout = file.timeout_ms.unwrap_or(5000);
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
                enabled: global_enabled,
            });
        }
    }
    items.sort_by(|a, b| a.event.cmp(&b.event).then(a.id.cmp(&b.id)));
    items
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
    let action = HookActionEntry {
        command: req.command.clone(),
        args: req.args,
        env: HashMap::new(),
        cwd: None,
        timeout_ms: req.timeout_ms,
        stdin_json: true,
    };
    file.hooks
        .entry(req.event.clone())
        .or_default()
        .push(action);
    write_hooks_file(&state, &file)?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"status": "created", "event": req.event})),
    ))
}

pub async fn update_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateHookRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
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
        action.command = command;
    }
    if let Some(args) = req.args {
        action.args = args;
    }
    if let Some(timeout_ms) = req.timeout_ms {
        action.timeout_ms = Some(timeout_ms);
    }

    // If event changed, move the action
    if let Some(new_event) = req.event {
        if new_event != event_key {
            let action = actions.remove(action_idx);
            if actions.is_empty() {
                file.hooks.remove(&event_key);
            }
            file.hooks.entry(new_event).or_default().push(action);
        }
    }

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

    write_hooks_file(&state, &file)?;
    Ok(Json(serde_json::json!({"status": "deleted", "id": id})))
}

pub async fn toggle_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut file = read_hooks_file(&state)?;

    // For toggle, we toggle the global enabled flag since hooks.json uses a global enabled
    // If the id matches a specific hook, we still toggle globally (hooks.json design limitation)
    let (event_key, _action_idx) = parse_hook_id(&id);

    if !file.hooks.contains_key(&event_key) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Hook '{id}' not found")})),
        ));
    }

    let current = file.enabled.unwrap_or(true);
    file.enabled = Some(!current);
    write_hooks_file(&state, &file)?;

    Ok(Json(
        serde_json::json!({"status": "toggled", "id": id, "enabled": !current}),
    ))
}

fn parse_hook_id(id: &str) -> (String, usize) {
    if let Some((event, idx_str)) = id.rsplit_once(':') {
        if let Ok(idx) = idx_str.parse::<usize>() {
            return (event.to_string(), idx);
        }
    }
    (id.to_string(), 0)
}

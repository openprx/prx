use super::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tracing::warn;

/// POST /api/config/reload — hot-reload configuration from config.toml (authenticated).
pub async fn post_config_reload(State(state): State<AppState>) -> Response {
    use crate::tools::Tool as _;
    let tool = crate::tools::ConfigReloadTool::new(Arc::clone(&state.shared_config));
    match tool.execute(serde_json::json!({})).await {
        Ok(result) if result.success => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "message": result.output,
            })),
        )
            .into_response(),
        Ok(result) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": result.error.unwrap_or_else(|| "Unknown error".into()),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

const REDACTION_MASK: &str = "***";

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ConfigFileSource {
    Main,
    Fragment,
}

#[derive(Serialize)]
struct ConfigFilePayload {
    path: String,
    filename: String,
    content: String,
    source: ConfigFileSource,
}

#[derive(Deserialize)]
pub struct UpdateConfigFileRequest {
    content: String,
}

/// POST /api/config — merge partial JSON into current config, save to disk.
pub async fn post_config(State(state): State<AppState>, Json(incoming): Json<Value>) -> Response {
    if !incoming.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Request body must be a JSON object"})),
        )
            .into_response();
    }

    // 1. Serialize current config to JSON, merge incoming on top
    let config = state.config.lock().clone();
    let mut current_value = match serde_json::to_value(&config) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to serialize current config: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error processing config"})),
            )
                .into_response();
        }
    };

    merge_json(&mut current_value, &incoming);

    // 2. Deserialize merged JSON back into Config
    let mut merged_config: crate::config::Config = match serde_json::from_value(current_value) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid config after merge: {e}")})),
            )
                .into_response();
        }
    };

    // 3. Preserve non-serialized fields
    merged_config.config_path = config.config_path.clone();
    merged_config.workspace_dir = config.workspace_dir.clone();

    // 4. Validate
    if let Err(e) = merged_config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Config validation failed: {e}")})),
        )
            .into_response();
    }

    // 5. Save to disk (Config::save handles backup + atomic write)
    if let Err(e) = merged_config.save().await {
        warn!("Failed to save config: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to save configuration"})),
        )
            .into_response();
    }

    // 6. Update in-memory config (hold Mutex while updating both stores atomically)
    {
        let mut guard = state.config.lock();
        *guard = merged_config.clone();
        state.shared_config.store(Arc::new(merged_config));
    }

    Json(serde_json::json!({
        "status": "ok",
        "restart_required": true
    }))
    .into_response()
}

/// Deep-merge `source` into `target`. Arrays are replaced, not merged.
fn merge_json(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, source_val) in source_map {
                // Skip redacted values — don't overwrite real secrets with "***"
                if source_val.as_str() == Some(REDACTION_MASK) {
                    continue;
                }
                let entry = target_map.entry(key.clone()).or_insert(Value::Null);
                merge_json(entry, source_val);
            }
        }
        (target, source) => {
            *target = source.clone();
        }
    }
}

pub async fn get_config(State(state): State<AppState>) -> Response {
    let config = state.config.lock().clone();
    let mut value = match serde_json::to_value(config) {
        Ok(value) => value,
        Err(error) => {
            warn!("Failed to serialize config: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error processing config"})),
            )
                .into_response();
        }
    };

    redact_config_value(None, &mut value);
    Json(value).into_response()
}

pub async fn get_config_files(State(state): State<AppState>) -> Response {
    let config = state.config.lock().clone();
    match collect_config_files(&config.config_path) {
        Ok(files) => Json(files).into_response(),
        Err(error) => {
            warn!("Failed to read config files: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to read configuration files"})),
            )
                .into_response()
        }
    }
}

pub async fn put_config_file(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    Json(payload): Json<UpdateConfigFileRequest>,
) -> Response {
    let current = state.config.lock().clone();
    let target_path = match resolve_config_file_path(&current.config_path, &filename) {
        Ok(path) => path,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": error}))).into_response();
        }
    };

    let parsed: toml::Value = match payload.content.parse() {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid TOML content: {error}")})),
            )
                .into_response();
        }
    };
    if !parsed.is_table() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Config file root must be a TOML table"})),
        )
            .into_response();
    }

    if let Some(parent) = target_path.parent() {
        if target_path != current.config_path {
            if let Ok(metadata) = std::fs::symlink_metadata(parent) {
                if metadata.file_type().is_symlink() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": "config.d path must not be a symlink"})),
                    )
                        .into_response();
                }
                if !metadata.is_dir() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": "config.d path is not a directory"})),
                    )
                        .into_response();
                }
            } else if let Err(error) = tokio::fs::create_dir_all(parent).await {
                warn!("Failed to create config.d directory: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "Failed to create configuration directory"})),
                )
                    .into_response();
            }
        }
    }

    if let Err(error) = crate::config::schema::write_toml_string_atomic(&target_path, &payload.content).await {
        warn!("Failed to save config file: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to save configuration file"})),
        )
            .into_response();
    }

    let refreshed = match crate::config::Config::load_from_path(&current.config_path, current.workspace_dir) {
        Ok(config) => config,
        Err(error) => {
            warn!("Saved file but merged config is invalid: {error}");
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "Saved file but merged configuration is invalid — check TOML syntax"}),
                ),
            )
                .into_response();
        }
    };

    // Atomic dual-store update: hold Mutex while swapping ArcSwap
    {
        let mut guard = state.config.lock();
        *guard = refreshed.clone();
        state.shared_config.store(Arc::new(refreshed));
    }

    Json(serde_json::json!({
        "status": "ok",
        "restart_required": true
    }))
    .into_response()
}

pub async fn get_config_schema() -> Response {
    Json(schema_for!(crate::config::Config)).into_response()
}

fn collect_config_files(config_path: &std::path::Path) -> anyhow::Result<Vec<ConfigFilePayload>> {
    let mut files = vec![read_config_file(config_path.to_path_buf(), ConfigFileSource::Main)?];
    for path in crate::config::files::list_config_fragment_paths(config_path)? {
        files.push(read_config_file(path, ConfigFileSource::Fragment)?);
    }
    Ok(files)
}

fn read_config_file(path: PathBuf, source: ConfigFileSource) -> anyhow::Result<ConfigFilePayload> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid config filename: {}", path.display()))?
        .to_string();
    let content = std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("Failed to read {}: {error}", path.display()))?;
    let relative_path = match source {
        ConfigFileSource::Main => filename.clone(),
        ConfigFileSource::Fragment => format!("config.d/{filename}"),
    };

    Ok(ConfigFilePayload {
        path: relative_path,
        filename,
        content,
        source,
    })
}

fn resolve_config_file_path(config_path: &std::path::Path, filename: &str) -> Result<PathBuf, String> {
    if filename == "config.toml" {
        return Ok(config_path.to_path_buf());
    }

    if filename.trim().is_empty() {
        return Err("Filename must not be empty".to_string());
    }
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Filename must not contain path separators".to_string());
    }
    if !filename.ends_with(".toml") {
        return Err("Filename must end with .toml".to_string());
    }

    Ok(crate::config::files::config_dir_path(config_path).join(filename))
}

fn redact_config_value(key: Option<&str>, value: &mut Value) {
    if key.is_some_and(is_sensitive_key) {
        redact_sensitive_value(value);
        return;
    }

    match value {
        Value::Object(map) => {
            for (child_key, child_value) in map {
                redact_config_value(Some(child_key.as_str()), child_value);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_config_value(key, item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn redact_sensitive_value(value: &mut Value) {
    match value {
        Value::Null => {}
        Value::Bool(_) | Value::Number(_) | Value::String(_) => *value = Value::String(REDACTION_MASK.to_string()),
        Value::Array(items) => {
            for item in items {
                redact_sensitive_value(item);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                redact_sensitive_value(item);
            }
        }
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "api_key"
        || key == "api_keys"
        || key == "auth_token"
        || key == "token"
        || key == "secret"
        || key == "password"
        || key == "paired_tokens"
        || key == "db_url"
        || key == "private_key"
        || key == "access_key"
        || key == "credential"
        || key == "credentials"
        || key == "connection_string"
        || key == "signing_secret"
        || key == "webhook_secret"
        || key == "app_secret"
        || key.ends_with("_api_key")
        || key.ends_with("_api_keys")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.ends_with("_key")
        || key.ends_with("_credential")
        || key.ends_with("_credentials")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("private_key")
}

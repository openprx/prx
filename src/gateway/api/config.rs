use super::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

const REDACTION_MASK: &str = "***";

/// POST /api/config — merge partial JSON into current config, save to disk.
pub async fn post_config(
    State(state): State<AppState>,
    Json(incoming): Json<Value>,
) -> Response {
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize current config: {e}")})),
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
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to save config: {e}")})),
        )
            .into_response();
    }

    // 6. Update in-memory config
    *state.config.lock() = merged_config;

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
                let entry = target_map
                    .entry(key.clone())
                    .or_insert(Value::Null);
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize config: {error}")})),
            )
                .into_response();
        }
    };

    redact_config_value(None, &mut value);
    Json(value).into_response()
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
        Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            *value = Value::String(REDACTION_MASK.to_string())
        }
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
        || key.ends_with("_api_key")
        || key.ends_with("_api_keys")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.contains("password")
}

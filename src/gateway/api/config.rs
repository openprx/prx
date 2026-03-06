use super::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

const REDACTION_MASK: &str = "***";

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

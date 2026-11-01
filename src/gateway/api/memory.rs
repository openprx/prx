//! Memory HTTP routes.
//!
//! Each route calls the existing memory tool with a server-built scope whose
//! `owner_id` is only [`ApiPrincipal::owner_id`] and whose `owner_isolated`
//! flag is set. The body cannot name an owner, a channel, or a sender.
//! Forget uses the tool's own approval path. A backend named `none` is
//! unavailable. A missing key and another owner's key share one 404 body.

use super::{ApiPrincipal, AppState};
use crate::tools::traits::Tool;
use crate::tools::{MemoryForgetTool, MemoryRecallTool, MemorySearchTool, MemoryStoreTool};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

const NOT_FOUND_ERROR: &str = "memory not found";

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchBody {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
    #[serde(default, rename = "maxResults")]
    max_results_camel: Option<u32>,
    #[serde(default, rename = "minScore")]
    min_score: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallBody {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreBody {
    key: String,
    content: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetBody {
    key: String,
}

fn error_response(status: StatusCode, error: impl Into<String>) -> ApiResponse {
    (status, Json(json!({ "error": error.into() })))
}

fn bad_request(error: impl Into<String>) -> ApiResponse {
    error_response(StatusCode::BAD_REQUEST, error)
}

fn not_found() -> ApiResponse {
    error_response(StatusCode::NOT_FOUND, NOT_FOUND_ERROR)
}

fn unavailable(error: impl Into<String>) -> ApiResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "success": false, "error": error.into() })),
    )
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &Bytes) -> Result<T, ApiResponse> {
    if bytes.is_empty() {
        return Err(bad_request("request body is required"));
    }
    serde_json::from_slice(bytes).map_err(|error| bad_request(format!("malformed request: {error}")))
}

fn require_text(value: &str, field: &str) -> Result<String, ApiResponse> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(bad_request(format!("{field} is required")))
    } else {
        Ok(trimmed.to_string())
    }
}

fn backend_name(state: &AppState) -> &str {
    crate::memory::Memory::name(state.mem.as_ref())
}

fn ensure_backend(state: &AppState) -> Result<(), ApiResponse> {
    if backend_name(state) == "none" {
        Err(unavailable("memory is unavailable"))
    } else {
        Ok(())
    }
}

fn scoped(owner_id: &str, fields: Map<String, Value>) -> Value {
    let mut fields = fields;
    fields.insert("_zc_scope_trusted".to_string(), json!(true));
    fields.insert(
        "_zc_scope".to_string(),
        json!({
            "owner_id": owner_id,
            "owner_isolated": true
        }),
    );
    Value::Object(fields)
}

fn security(state: &AppState) -> Arc<crate::security::SecurityPolicy> {
    let config = state.config.load_full();
    crate::runtime::bootstrap::build_security_policy(config.as_ref())
}

fn source_name(state: &AppState) -> String {
    backend_name(state).to_string()
}

fn viewed(source: &str, mut extra: Map<String, Value>) -> Value {
    extra.insert("success".to_string(), json!(true));
    extra.insert("scope".to_string(), json!("owner"));
    extra.insert("source".to_string(), json!(source));
    Value::Object(extra)
}

fn map_tool_failure(error: &str) -> ApiResponse {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not available to this caller") || lower.contains("no memory found") {
        return not_found();
    }
    if lower.contains("approval grant") || lower.contains("read-only") || lower.contains("resource operation") {
        return error_response(StatusCode::FORBIDDEN, error);
    }
    if lower.contains("is required") || lower.contains("not available to an owner-isolated") {
        return bad_request(error);
    }
    tracing::warn!("memory tool refused the call");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory call failed")
}

fn tool_output(result: crate::tools::traits::ToolResult) -> Result<String, ApiResponse> {
    if result.success {
        Ok(result.output)
    } else {
        Err(map_tool_failure(
            result.error.as_deref().unwrap_or("memory call failed"),
        ))
    }
}

fn output_is_empty(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("no matches found") || lower.contains("no memories found")
}

fn hits_from_output(output: &str, source: &str) -> Vec<Value> {
    let mut hits = Vec::new();
    let mut key = String::new();
    let mut content = String::new();
    let flush = |hits: &mut Vec<Value>, key: &mut String, content: &mut String| {
        if key.is_empty() {
            return;
        }
        hits.push(json!({
            "key": key,
            "content": content,
            "scope": "owner",
            "source": source,
        }));
        key.clear();
        content.clear();
    };
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("- key:") {
            flush(&mut hits, &mut key, &mut content);
            key = value.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("- [") {
            flush(&mut hits, &mut key, &mut content);
            if let Some((parsed_key, parsed_content)) =
                rest.split_once("] ").and_then(|(_, tail)| tail.split_once(": "))
            {
                key = parsed_key.trim().to_string();
                content = parsed_content.trim().to_string();
            }
        } else if let Some(value) = trimmed.strip_prefix("content:") {
            content = value.trim().to_string();
        }
    }
    flush(&mut hits, &mut key, &mut content);
    hits
}

fn read_body(source: &str, output: String) -> Value {
    let empty = output_is_empty(&output);
    let hits = if empty {
        Vec::new()
    } else {
        hits_from_output(&output, source)
    };
    let mut extra = Map::new();
    extra.insert("empty".to_string(), json!(empty));
    extra.insert("output".to_string(), json!(output));
    extra.insert("hits".to_string(), json!(hits));
    viewed(source, extra)
}

pub(super) async fn search(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    if let Err(response) = ensure_backend(&state) {
        return response;
    }
    let parsed: SearchBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let query = match require_text(&parsed.query, "query") {
        Ok(query) => query,
        Err(response) => return response,
    };
    let mut fields = Map::new();
    fields.insert("query".to_string(), json!(query));
    if let Some(limit) = parsed.max_results.or(parsed.max_results_camel) {
        fields.insert("max_results".to_string(), json!(limit));
    }
    if let Some(score) = parsed.min_score {
        fields.insert("minScore".to_string(), json!(score));
    }
    let config = state.config.load_full();
    let tool = MemorySearchTool::new(
        config.workspace_dir.clone(),
        Arc::clone(&state.mem),
        config.memory.acl_enabled,
    );
    let executed = match tool.execute(scoped(&principal.owner_id, fields)).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "memory search failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory call failed");
        }
    };
    match tool_output(executed) {
        Ok(output) => (StatusCode::OK, Json(read_body(&source_name(&state), output))),
        Err(response) => response,
    }
}

pub(super) async fn recall(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    if let Err(response) = ensure_backend(&state) {
        return response;
    }
    let parsed: RecallBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let query = match require_text(&parsed.query, "query") {
        Ok(query) => query,
        Err(response) => return response,
    };
    let mut fields = Map::new();
    fields.insert("query".to_string(), json!(query));
    if let Some(limit) = parsed.limit {
        fields.insert("limit".to_string(), json!(limit));
    }
    if let Some(session_id) = parsed
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        fields.insert("session_id".to_string(), json!(session_id));
    }
    let config = state.config.load_full();
    let tool = MemoryRecallTool::new(Arc::clone(&state.mem), config.memory.acl_enabled);
    let executed = match tool.execute(scoped(&principal.owner_id, fields)).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "memory recall failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory call failed");
        }
    };
    match tool_output(executed) {
        Ok(output) => (StatusCode::OK, Json(read_body(&source_name(&state), output))),
        Err(response) => response,
    }
}

pub(super) async fn store(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    if let Err(response) = ensure_backend(&state) {
        return response;
    }
    let parsed: StoreBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let key = match require_text(&parsed.key, "key") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let content = match require_text(&parsed.content, "content") {
        Ok(content) => content,
        Err(response) => return response,
    };
    let mut fields = Map::new();
    fields.insert("key".to_string(), json!(key.clone()));
    fields.insert("content".to_string(), json!(content));
    if let Some(category) = parsed
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        fields.insert("category".to_string(), json!(category));
    }
    let tool = MemoryStoreTool::new(Arc::clone(&state.mem), security(&state));
    let executed = match tool.execute(scoped(&principal.owner_id, fields)).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "memory store failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory call failed");
        }
    };
    match tool_output(executed) {
        Ok(_) => {
            let mut extra = Map::new();
            extra.insert("key".to_string(), json!(key));
            (StatusCode::OK, Json(viewed(&source_name(&state), extra)))
        }
        Err(response) => response,
    }
}

pub(super) async fn forget(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    if let Err(response) = ensure_backend(&state) {
        return response;
    }
    let parsed: ForgetBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let key = match require_text(&parsed.key, "key") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let mut fields = Map::new();
    fields.insert("key".to_string(), json!(key));
    let tool = MemoryForgetTool::new(Arc::clone(&state.mem), security(&state));
    let executed = match tool.execute(scoped(&principal.owner_id, fields)).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "memory forget failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory call failed");
        }
    };
    match tool_output(executed) {
        Ok(output) if output.starts_with("Forgot memory:") => {
            let mut extra = Map::new();
            extra.insert("forgotten".to_string(), json!(true));
            (StatusCode::OK, Json(viewed(&source_name(&state), extra)))
        }
        Ok(_) => not_found(),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TOKEN_OTHER, at, json, serve_config, serve_with_memory};
    use super::NOT_FOUND_ERROR;
    use crate::memory::NoneMemory;
    use std::sync::Arc;

    fn note() -> serde_json::Value {
        serde_json::json!({ "key": "c4-note", "content": "alpha-only-note" })
    }

    /// Pins the prose the memory tools emit today. A format change that the
    /// HTTP parser no longer understands must fail here, not return an empty
    /// `hits` list beside a still-successful `output`.
    #[test]
    fn search_and_recall_prose_parse_into_owner_hits() {
        let search = "Found 1 matches:\n- key: c4-note\n  content: alpha-only-note\n  snippet: alpha-only-note\n";
        assert_eq!(
            super::hits_from_output(search, "sqlite"),
            vec![serde_json::json!({
                "key": "c4-note",
                "content": "alpha-only-note",
                "scope": "owner",
                "source": "sqlite"
            })]
        );
        let recall = "Found 1 memories:\n- [core] c4-note: alpha-only-note [100%]\n";
        assert_eq!(
            super::hits_from_output(recall, "sqlite"),
            vec![serde_json::json!({
                "key": "c4-note",
                "content": "alpha-only-note [100%]",
                "scope": "owner",
                "source": "sqlite"
            })]
        );
        assert_eq!(
            super::hits_from_output("No matches found for query: 'alpha-only-note'", "sqlite"),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(
            super::hits_from_output("No memories found matching that query.", "sqlite"),
            Vec::<serde_json::Value>::new()
        );
    }

    #[tokio::test]
    async fn memory_routes_isolate_owners_and_do_not_leak_a_foreign_key() {
        let served = serve_config(|_| {}).await;
        let (status, _) = served
            .call_anonymous(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes" })),
            )
            .await;
        assert_eq!(status, 401);

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes", "owner_id": "api:other" })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, _) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/recall",
                Some(serde_json::json!({ "query": "" })),
            )
            .await;
        assert_eq!(status, 400);

        let (status, body) = served
            .call(reqwest::Method::POST, "/api/memory/store", Some(note()))
            .await;
        assert_eq!(status, 200, "{body}");
        let stored = json(&body);
        assert_eq!(at(&stored, "/success"), true);
        assert_eq!(at(&stored, "/scope"), "owner");
        assert_eq!(at(&stored, "/source"), "sqlite");
        assert_eq!(at(&stored, "/key"), "c4-note");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let found = json(&body);
        assert_eq!(at(&found, "/empty"), false);
        assert_eq!(at(&found, "/scope"), "owner");
        assert_eq!(at(&found, "/source"), "sqlite");
        let output = at(&found, "/output").as_str().unwrap_or("").to_string();
        assert!(output.contains("- key: c4-note"), "{output}");
        assert!(output.contains("content: alpha-only-note"), "{output}");
        assert_eq!(
            at(&found, "/hits"),
            serde_json::json!([{
                "key": "c4-note",
                "content": "alpha-only-note",
                "scope": "owner",
                "source": "sqlite"
            }])
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/recall",
                Some(serde_json::json!({ "query": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let recalled = json(&body);
        assert_eq!(at(&recalled, "/empty"), false);
        let recall_output = at(&recalled, "/output").as_str().unwrap_or("").to_string();
        assert!(
            recall_output.contains("- [core] c4-note: alpha-only-note"),
            "{recall_output}"
        );
        let recall_hits = at(&recalled, "/hits");
        assert_eq!(at(&recall_hits, "/0/key"), "c4-note");
        assert_eq!(at(&recall_hits, "/0/scope"), "owner");
        assert_eq!(at(&recall_hits, "/0/source"), "sqlite");
        assert!(
            at(&recall_hits, "/0/content")
                .as_str()
                .unwrap_or("")
                .starts_with("alpha-only-note"),
            "{recall_hits}"
        );

        let (status, body) = served
            .call_as(
                TOKEN_OTHER,
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let hidden = json(&body);
        assert_eq!(at(&hidden, "/empty"), true);
        assert_eq!(at(&hidden, "/hits"), serde_json::json!([]));
        let hidden_output = at(&hidden, "/output").as_str().unwrap_or("").to_string();
        // The empty sentence repeats the caller's own query. The stored key must not appear.
        assert!(
            hidden_output.starts_with("No matches found for query:"),
            "{hidden_output}"
        );
        assert!(!hidden_output.contains("c4-note"), "{hidden_output}");

        let (status, body) = served
            .call_as(
                TOKEN_OTHER,
                reqwest::Method::POST,
                "/api/memory/recall",
                Some(serde_json::json!({ "query": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let recalled = at(&json(&body), "/output").as_str().unwrap_or("").to_string();
        assert!(!recalled.contains("alpha-only-note"), "{recalled}");

        let (status, body) = served
            .call_as(
                TOKEN_OTHER,
                reqwest::Method::POST,
                "/api/memory/forget",
                Some(serde_json::json!({ "key": "c4-note" })),
            )
            .await;
        assert_eq!(status, 404, "{body}");
        let foreign = json(&body);
        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/forget",
                Some(serde_json::json!({ "key": "missing-c4-key" })),
            )
            .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(at(&foreign, "/error"), NOT_FOUND_ERROR);
        assert_eq!(at(&json(&body), "/error"), at(&foreign, "/error"));
        assert!(!at(&foreign, "/error").as_str().unwrap_or("").contains("c4-note"));

        let (status, body) = served
            .call_as(TOKEN_OTHER, reqwest::Method::POST, "/api/memory/store", Some(note()))
            .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(at(&json(&body), "/error"), at(&foreign, "/error"));

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            at(&json(&body), "/output")
                .as_str()
                .unwrap_or("")
                .contains("alpha-only-note"),
            "{body}"
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/forget",
                Some(serde_json::json!({ "key": "c4-note" })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/forgotten"), true);
    }

    #[tokio::test]
    async fn a_memory_backend_without_the_feature_is_unavailable() {
        let served = serve_with_memory(|_| {}, Arc::new(NoneMemory::new())).await;
        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes" })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);
        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/store",
                Some(serde_json::json!({ "key": "c4-note", "content": "alpha-only-note" })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);
    }
}

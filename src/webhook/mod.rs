use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::MemoryCategory;
use crate::memory::{Memory, SqliteMemory};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
    routing::post,
};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookEvent {
    pub source: String,
    pub event_type: String,
    pub project: Option<String>,
    pub external_id: String,
    pub external_url: Option<String>,
    pub title: String,
    pub content: String,
    pub actor: Option<String>,
    pub timestamp: String,
}

#[derive(Clone)]
struct WebhookState {
    token: Arc<str>,
    /// Optional HMAC signing secret for `X-Webhook-Signature` verification.
    signing_secret: Option<Arc<str>>,
    db_path: Arc<PathBuf>,
    acl_enabled: bool,
    rate_limiter: Arc<WebhookRateLimiter>,
    idempotency_store: Arc<IdempotencyStore>,
}

#[derive(Debug, Serialize)]
struct WebhookAck {
    topic_id: String,
}

const WEBHOOK_RATE_LIMIT_WINDOW_SECS: u64 = 60;
const WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE: u32 = 60;
const WEBHOOK_IDEMPOTENCY_TTL_SECS: u64 = 300;
const WEBHOOK_IDEMPOTENCY_MAX_KEYS: usize = 10_000;

#[derive(Debug)]
struct WebhookRateLimiter {
    limit_per_window: u32,
    window: Duration,
    requests: Mutex<Vec<Instant>>,
}

impl WebhookRateLimiter {
    fn new(limit_per_window: u32, window: Duration) -> Self {
        Self {
            limit_per_window,
            window,
            requests: Mutex::new(Vec::new()),
        }
    }

    async fn allow(&self) -> bool {
        if self.limit_per_window == 0 {
            return true;
        }

        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or_else(Instant::now);
        let mut requests = self.requests.lock().await;
        requests.retain(|instant| *instant > cutoff);
        if requests.len() >= self.limit_per_window as usize {
            return false;
        }
        requests.push(now);
        true
    }
}

#[derive(Debug)]
struct IdempotencyStore {
    ttl: Duration,
    max_keys: usize,
    keys: Mutex<HashMap<String, Instant>>,
}

impl IdempotencyStore {
    fn new(ttl: Duration, max_keys: usize) -> Self {
        Self {
            ttl,
            max_keys: max_keys.max(1),
            keys: Mutex::new(HashMap::new()),
        }
    }

    async fn record_if_new(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut keys = self.keys.lock().await;
        keys.retain(|_, seen_at| now.duration_since(*seen_at) < self.ttl);
        if keys.contains_key(key) {
            return false;
        }
        if keys.len() >= self.max_keys {
            let evict_key = keys
                .iter()
                .min_by_key(|(_, seen_at)| *seen_at)
                .map(|(k, _)| k.clone());
            if let Some(evict_key) = evict_key {
                keys.remove(&evict_key);
            }
        }
        keys.insert(key.to_string(), now);
        true
    }
}

pub async fn run(bind: &str, token: &str, workspace_dir: &Path, acl_enabled: bool) -> Result<()> {
    run_with_signing_secret(bind, token, None, workspace_dir, acl_enabled).await
}

/// Run the standalone webhook server with optional HMAC signing secret.
///
/// When `signing_secret` is `Some`, every request must include a valid
/// `X-Webhook-Signature` header (HMAC-SHA256 of the request body).
pub async fn run_with_signing_secret(
    bind: &str,
    token: &str,
    signing_secret: Option<&str>,
    workspace_dir: &Path,
    acl_enabled: bool,
) -> Result<()> {
    let trimmed_token = token.trim();
    if trimmed_token.is_empty() {
        anyhow::bail!("webhook token must not be empty when webhook is enabled");
    }

    let db_path = workspace_dir.join("memory").join("brain.db");
    ensure_memory_schema(&db_path)?;

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind webhook server at {bind}"))?;
    let addr = listener.local_addr()?;

    tracing::info!("Webhook server listening on {}", addr);

    let state = WebhookState {
        token: Arc::<str>::from(trimmed_token.to_string()),
        signing_secret: signing_secret
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Arc::<str>::from(s.to_string())),
        db_path: Arc::new(db_path),
        acl_enabled,
        rate_limiter: Arc::new(WebhookRateLimiter::new(
            WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE,
            Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
        )),
        idempotency_store: Arc::new(IdempotencyStore::new(
            Duration::from_secs(WEBHOOK_IDEMPOTENCY_TTL_SECS),
            WEBHOOK_IDEMPOTENCY_MAX_KEYS,
        )),
    };

    run_with_listener(listener, state).await
}

async fn run_with_listener(listener: TcpListener, state: WebhookState) -> Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .await
        .context("webhook server stopped unexpectedly")?;
    Ok(())
}

fn router(state: WebhookState) -> Router {
    Router::new()
        .route("/webhook", post(handle_webhook_event))
        .route("/webhook/events", post(handle_webhook_event))
        .with_state(state)
}

async fn handle_webhook_event(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if !state.rate_limiter.allow().await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "Too many webhook requests. Please retry later.",
                "retry_after": WEBHOOK_RATE_LIMIT_WINDOW_SECS
            })),
        )
            .into_response();
    }

    if !is_authorized(&headers, &state.token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }

    // HMAC signature verification (when signing secret is configured)
    if let Some(ref signing_secret) = state.signing_secret {
        let signature = headers
            .get("X-Webhook-Signature")
            .and_then(|v| v.to_str().ok());
        match signature {
            Some(sig) if verify_webhook_hmac_signature(signing_secret, &body, sig) => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "Invalid HMAC signature" })),
                )
                    .into_response();
            }
        }
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("Invalid JSON: {e}") })),
            )
                .into_response();
        }
    };

    let event = match parse_webhook_event(payload) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!("invalid webhook event payload: {error}");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid webhook event" })),
            )
                .into_response();
        }
    };

    let replay_key = headers
        .get("X-Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("header:{value}"))
        .unwrap_or_else(|| format!("event:{}", webhook_replay_fingerprint(&event)));
    if !state.idempotency_store.record_if_new(&replay_key).await {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "duplicate",
                "idempotent": true,
                "message": "Request already processed"
            })),
        )
            .into_response();
    }

    let db_path = (*state.db_path).clone();
    let acl_enabled = state.acl_enabled;
    let saved =
        tokio::task::spawn_blocking(move || persist_event(&db_path, &event, acl_enabled)).await;

    match saved {
        Ok(Ok(topic_id)) => (
            StatusCode::OK,
            Json(serde_json::json!(WebhookAck { topic_id })),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::error!("failed to persist webhook event: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to persist event" })),
            )
                .into_response()
        }
        Err(join_error) => {
            tracing::error!("webhook worker panicked: {join_error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Webhook worker failure" })),
            )
                .into_response()
        }
    }
}

/// Verify HMAC-SHA256 signature of the request body.
/// Accepts signatures in the format `sha256=<hex>` or raw hex.
fn verify_webhook_hmac_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};

    let signature_hex = signature_header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature_header.trim());
    let Ok(provided) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

fn is_authorized(headers: &HeaderMap, expected_token: &str) -> bool {
    // Try X-Webhook-Token first (preferred, matches gateway behavior)
    if let Some(raw) = headers.get("X-Webhook-Token") {
        if let Ok(token) = raw.to_str() {
            return expected_token
                .as_bytes()
                .ct_eq(token.trim().as_bytes())
                .into();
        }
    }

    // Fallback to Authorization: Bearer <token>
    let Some(raw) = headers.get(AUTHORIZATION) else {
        return false;
    };

    let Ok(value) = raw.to_str() else {
        return false;
    };

    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };

    expected_token
        .as_bytes()
        .ct_eq(token.trim().as_bytes())
        .into()
}

fn parse_webhook_event(payload: Value) -> Result<WebhookEvent> {
    if is_openpr_payload(&payload) {
        return map_openpr_event(&payload);
    }

    let mut event: WebhookEvent = serde_json::from_value(payload)
        .context("payload does not match generic webhook event format")?;

    normalize_event(&mut event);
    validate_event(&event)?;
    Ok(event)
}

fn normalize_event(event: &mut WebhookEvent) {
    event.source = event.source.trim().to_lowercase();
    event.event_type = event.event_type.trim().to_lowercase();
    event.external_id = event.external_id.trim().to_lowercase();
    event.title = event.title.trim().to_string();
    event.content = event.content.trim().to_string();
    if event.timestamp.trim().is_empty() {
        event.timestamp = Utc::now().to_rfc3339();
    } else {
        event.timestamp = event.timestamp.trim().to_string();
    }
    event.project = event
        .project
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    event.external_url = event
        .external_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    event.actor = event
        .actor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
}

fn validate_event(event: &WebhookEvent) -> Result<()> {
    if event.source.is_empty()
        || event.event_type.is_empty()
        || event.external_id.is_empty()
        || event.title.is_empty()
        || event.content.is_empty()
    {
        anyhow::bail!("event contains required empty fields");
    }
    Ok(())
}

fn is_openpr_payload(payload: &Value) -> bool {
    payload.get("workspace_id").is_some()
        || payload.get("issue_id").is_some()
        || payload.get("issue_identifier").is_some()
        || payload.get("comment_id").is_some()
}

fn map_openpr_event(payload: &Value) -> Result<WebhookEvent> {
    let source = "openpr".to_string();

    let event_type = first_string(payload, &["event_type", "event", "type", "action"])
        .map(|value| normalize_openpr_event_type(&value))
        .unwrap_or_else(|| "issue.updated".to_string());

    let workspace_id = first_string(payload, &["workspace_id", "project", "project_id"]);

    let issue_identifier = first_string(payload, &["issue_identifier", "issue_id"]);
    let comment_id = first_string(payload, &["comment_id"]);

    let external_id = if let Some(identifier) = issue_identifier {
        format!("issue#{}", identifier.trim())
    } else if let Some(identifier) = comment_id {
        format!("comment#{}", identifier.trim())
    } else {
        anyhow::bail!("openpr payload missing issue/comment identifier");
    };

    let external_url = first_string(
        payload,
        &[
            "external_url",
            "issue_url",
            "comment_url",
            "url",
            "html_url",
        ],
    );

    let title = first_string(
        payload,
        &["title", "issue_title", "comment_title", "subject", "name"],
    )
    .unwrap_or_else(|| format!("OpenPR {}", external_id));

    let content = first_string(
        payload,
        &[
            "content",
            "body",
            "description",
            "text",
            "comment",
            "message",
        ],
    )
    .unwrap_or_else(|| title.clone());

    let actor = first_string(payload, &["actor", "operator", "author", "user"]);

    let timestamp = first_string(
        payload,
        &["timestamp", "occurred_at", "created_at", "updated_at"],
    )
    .unwrap_or_else(|| Utc::now().to_rfc3339());

    let mut event = WebhookEvent {
        source,
        event_type,
        project: workspace_id,
        external_id,
        external_url,
        title,
        content,
        actor,
        timestamp,
    };

    normalize_event(&mut event);
    validate_event(&event)?;
    Ok(event)
}

fn first_string(payload: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = payload.get(*key) else {
            continue;
        };
        if let Some(as_str) = value.as_str() {
            let trimmed = as_str.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
            continue;
        }
        if value.is_number() || value.is_boolean() {
            return Some(value.to_string());
        }
    }
    None
}

fn normalize_openpr_event_type(raw: &str) -> String {
    let normalized = raw.trim().to_lowercase().replace('_', ".");
    if normalized.contains('.') {
        return normalized;
    }

    if normalized == "closed" {
        return "issue.closed".to_string();
    }
    if normalized == "reopened" {
        return "issue.reopened".to_string();
    }
    if normalized == "created" {
        return "issue.created".to_string();
    }

    format!("issue.{normalized}")
}

fn persist_event(db_path: &Path, event: &WebhookEvent, acl_enabled: bool) -> Result<String> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open webhook db {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .context("failed to configure webhook sqlite busy_timeout")?;

    let topic_id = match crate::memory::topic::find_topic_by_project_and_external(
        &conn,
        event.project.as_deref(),
        &event.external_id,
    )? {
        Some(topic) => topic.id,
        None => {
            let fingerprint = webhook_topic_fingerprint(
                event.project.as_deref(),
                &event.external_id,
                &event.title,
            );
            crate::memory::topic::create_topic(
                &conn,
                &event.title,
                event.project.as_deref(),
                Some(&event.external_id),
                &fingerprint,
            )?
        }
    };

    if let Some(url) = event.external_url.as_deref() {
        conn.execute(
            "UPDATE topics
             SET external_url = COALESCE(external_url, ?1),
                 updated_at = ?2
             WHERE id = ?3",
            params![url, Utc::now().to_rfc3339(), &topic_id],
        )?;
    }

    let system_sender = format!("system:{}", event.source);
    crate::memory::topic::add_participant(&conn, &topic_id, &system_sender, "observer")?;

    let memory_key = format!(
        "webhook:{}:{}:{}",
        event.source,
        event.external_id,
        Uuid::new_v4()
    );
    let content = format_event_memory(event);
    let memory_ctx = MemoryWriteContext {
        channel: Some("webhook".to_string()),
        chat_type: Some("webhook".to_string()),
        chat_id: Some(format!("{}:{}", event.source, event.external_id)),
        sender_id: None,
        raw_sender: Some(system_sender.clone()),
    };

    // Persist via memory classification path to keep ACL policy consistent.
    let is_group_event = memory_ctx
        .chat_id
        .as_deref()
        .is_some_and(|chat_id| chat_id.contains("group:") || chat_id.contains("@g.us"));
    if !is_group_event && crate::memory::should_autosave_content(&content) {
        let memory = SqliteMemory::new_with_path_and_acl(db_path.to_path_buf(), acl_enabled)?;
        futures::executor::block_on(memory.store_with_context(
            &memory_key,
            &content,
            MemoryCategory::Conversation,
            None,
            Some(&memory_ctx),
        ))?;
    }

    conn.execute(
        "UPDATE memories
         SET topic_id = ?1,
             created_at = ?2,
             updated_at = ?2
         WHERE key = ?3",
        params![&topic_id, &event.timestamp, &memory_key],
    )?;

    match event.event_type.as_str() {
        "issue.closed" => crate::memory::topic::update_topic_status(&conn, &topic_id, "resolved")?,
        "issue.reopened" => crate::memory::topic::update_topic_status(&conn, &topic_id, "open")?,
        _ => crate::memory::topic::touch_topic(&conn, &topic_id)?,
    }
    Ok(topic_id)
}

fn format_event_memory(event: &WebhookEvent) -> String {
    let mut lines = vec![
        format!("source: {}", event.source),
        format!("event_type: {}", event.event_type),
        format!("external_id: {}", event.external_id),
        format!("title: {}", event.title),
        format!("content: {}", event.content),
        format!("timestamp: {}", event.timestamp),
    ];

    if let Some(project) = &event.project {
        lines.push(format!("project: {project}"));
    }
    if let Some(url) = &event.external_url {
        lines.push(format!("external_url: {url}"));
    }
    if let Some(actor) = &event.actor {
        lines.push(format!("actor: {actor}"));
    }

    lines.join("\n")
}

fn webhook_topic_fingerprint(project: Option<&str>, external_id: &str, title: &str) -> String {
    let payload = format!(
        "{}:{}:{}",
        project.unwrap_or_default().trim().to_lowercase(),
        external_id.trim().to_lowercase(),
        title.trim().to_lowercase(),
    );
    let digest = Sha256::digest(payload.as_bytes());
    format!("{digest:x}")
}

fn webhook_replay_fingerprint(event: &WebhookEvent) -> String {
    let payload = format!(
        "{}:{}:{}:{}:{}:{}",
        event.source.trim().to_lowercase(),
        event.event_type.trim().to_lowercase(),
        event
            .project
            .as_deref()
            .unwrap_or("_global")
            .trim()
            .to_lowercase(),
        event.external_id.trim().to_lowercase(),
        event
            .actor
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase(),
        event.timestamp.trim()
    );
    let digest = Sha256::digest(payload.as_bytes());
    format!("{digest:x}")
}

fn ensure_memory_schema(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = crate::memory::SqliteMemory::new_with_path(db_path.to_path_buf())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn setup_state(tmp: &TempDir, token: &str) -> WebhookState {
        let db_path = tmp.path().join("memory").join("brain.db");
        ensure_memory_schema(&db_path).unwrap();
        WebhookState {
            token: Arc::<str>::from(token.to_string()),
            signing_secret: None,
            db_path: Arc::new(db_path),
            acl_enabled: false,
            rate_limiter: Arc::new(WebhookRateLimiter::new(
                WEBHOOK_DEFAULT_RATE_LIMIT_PER_MINUTE,
                Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
            )),
            idempotency_store: Arc::new(IdempotencyStore::new(
                Duration::from_secs(WEBHOOK_IDEMPOTENCY_TTL_SECS),
                WEBHOOK_IDEMPOTENCY_MAX_KEYS,
            )),
        }
    }

    fn setup_state_with_limits(
        tmp: &TempDir,
        token: &str,
        rate_limit_per_minute: u32,
        idempotency_max_keys: usize,
    ) -> WebhookState {
        let db_path = tmp.path().join("memory").join("brain.db");
        ensure_memory_schema(&db_path).unwrap();
        WebhookState {
            token: Arc::<str>::from(token.to_string()),
            signing_secret: None,
            db_path: Arc::new(db_path),
            acl_enabled: false,
            rate_limiter: Arc::new(WebhookRateLimiter::new(
                rate_limit_per_minute,
                Duration::from_secs(WEBHOOK_RATE_LIMIT_WINDOW_SECS),
            )),
            idempotency_store: Arc::new(IdempotencyStore::new(
                Duration::from_secs(WEBHOOK_IDEMPOTENCY_TTL_SECS),
                idempotency_max_keys,
            )),
        }
    }

    #[tokio::test]
    async fn token_auth_missing_rejected() {
        let tmp = TempDir::new().unwrap();
        let app = router(setup_state(&tmp, "secret"));

        let req = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "external_id": "issue#1",
                    "title": "test",
                    "content": "test",
                    "timestamp": Utc::now().to_rfc3339()
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_auth_invalid_rejected() {
        let tmp = TempDir::new().unwrap();
        let app = router(setup_state(&tmp, "secret"));

        let req = Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "external_id": "issue#1",
                    "title": "test",
                    "content": "test",
                    "timestamp": Utc::now().to_rfc3339()
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_auth_valid_accepts_and_persists_topic() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("memory").join("brain.db");
        let app = router(setup_state(&tmp, "secret"));

        let req = Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                json!({
                    "source": "custom",
                    "event_type": "issue.created",
                    "project": "openpr",
                    "external_id": "issue#42",
                    "external_url": "https://example.com/issues/42",
                    "title": "Issue 42",
                    "content": "details",
                    "actor": "openprx_user",
                    "timestamp": Utc::now().to_rfc3339()
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let topic_id = parsed
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(!topic_id.is_empty());

        let conn = Connection::open(db_path).unwrap();
        let topic_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(topic_count, 1);

        let visibility: String = conn
            .query_row(
                "SELECT visibility FROM memories WHERE topic_id = ?1 ORDER BY created_at DESC LIMIT 1",
                params![topic_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(visibility, "owner");
    }

    #[tokio::test]
    async fn same_external_id_in_different_projects_keeps_separate_topics() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("memory").join("brain.db");
        let app = router(setup_state(&tmp, "secret"));

        for project in ["openpr", "lc"] {
            let req = Request::builder()
                .method("POST")
                .uri("/webhook/events")
                .header("content-type", "application/json")
                .header("authorization", "Bearer secret")
                .body(Body::from(
                    json!({
                        "source": "custom",
                        "event_type": "issue.created",
                        "project": project,
                        "external_id": "issue#42",
                        "title": format!("{project} issue"),
                        "content": "details",
                        "actor": "openprx_user",
                        "timestamp": Utc::now().to_rfc3339()
                    })
                    .to_string(),
                ))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let conn = Connection::open(db_path).unwrap();
        let topic_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM topics WHERE external_id = 'issue#42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(topic_count, 2);
    }

    #[test]
    fn openpr_payload_maps_to_generic_event() {
        let payload = json!({
            "workspace_id": "openpr",
            "issue_identifier": 88,
            "event_type": "issue.closed",
            "title": "Close issue",
            "content": "merged",
            "actor": "project_bot",
            "timestamp": "2026-02-23T00:00:00Z"
        });

        let mapped = parse_webhook_event(payload).unwrap();
        assert_eq!(mapped.source, "openpr");
        assert_eq!(mapped.external_id, "issue#88");
        assert_eq!(mapped.event_type, "issue.closed");
        assert_eq!(mapped.project.as_deref(), Some("openpr"));
    }

    #[tokio::test]
    async fn webhook_server_starts_with_listener() {
        let tmp = TempDir::new().unwrap();
        let state = setup_state(&tmp, "secret");
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            let _ = run_with_listener(listener, state).await;
        });

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{addr}/webhook"))
            .bearer_auth("secret")
            .json(&json!({
                "source": "custom",
                "event_type": "issue.created",
                "external_id": "issue#100",
                "title": "Boot",
                "content": "ok",
                "timestamp": Utc::now().to_rfc3339()
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn webhook_idempotency_key_rejects_duplicate_replay() {
        let tmp = TempDir::new().unwrap();
        let app = router(setup_state_with_limits(&tmp, "secret", 60, 1024));
        let body = json!({
            "source": "custom",
            "event_type": "issue.created",
            "external_id": "issue#200",
            "title": "Idempotent",
            "content": "same",
            "timestamp": Utc::now().to_rfc3339()
        })
        .to_string();

        let req1 = Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .header("X-Idempotency-Key", "dup-key")
            .body(Body::from(body.clone()))
            .unwrap();
        let resp1 = app.clone().oneshot(req1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);

        let req2 = Request::builder()
            .method("POST")
            .uri("/webhook/events")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .header("X-Idempotency-Key", "dup-key")
            .body(Body::from(body))
            .unwrap();
        let resp2 = app.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        let body2 = to_bytes(resp2.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body2).unwrap();
        assert_eq!(parsed["status"], "duplicate");
    }

    #[tokio::test]
    async fn webhook_rate_limit_rejects_excess_requests() {
        let tmp = TempDir::new().unwrap();
        let app = router(setup_state_with_limits(&tmp, "secret", 1, 1024));
        let make_req = || {
            Request::builder()
                .method("POST")
                .uri("/webhook/events")
                .header("content-type", "application/json")
                .header("authorization", "Bearer secret")
                .body(Body::from(
                    json!({
                        "source": "custom",
                        "event_type": "issue.created",
                        "external_id": format!("issue#{}", Uuid::new_v4()),
                        "title": "rl",
                        "content": "rl",
                        "timestamp": Utc::now().to_rfc3339()
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let first = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(make_req()).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}

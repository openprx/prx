//! HTTP surface for the chat-session registry and its assignment mailbox.
//!
//! Closes the one leg of the cross-entry-point loop that was missing: a message
//! arriving on a channel can now make a *specific* `prx chat` session do
//! something, and the answer finds its way back. See
//! [`crate::runtime::chat_sessions`] for the model, the absence of any wall
//! clock, and the delivery guarantee.
//!
//! Everything here is mounted under the gateway's `auth_middleware`, so a
//! request that reaches a handler has already satisfied the bearer check when
//! pairing is enabled. Two endpoints — the mailbox pull and the result report —
//! require a *second* credential on top: the per-session token minted at
//! registration. That is not a redundant lock. The bearer token is the operator
//! plane, held by anything on this machine that talks to the daemon; the session
//! token is what stops one such thing from draining a *different* chat session's
//! mailbox and answering on its behalf.

use super::{AppState, authorize_resource_mutation};
use crate::runtime::chat_sessions::{
    self, AssignPrincipal, AssignmentResult, ChatSessionError, Disposition, ResultStatus,
};
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};

/// Header carrying the per-session mailbox token.
pub(crate) const SESSION_TOKEN_HEADER: &str = "x-prx-session-token";

type Refusal = (StatusCode, Json<serde_json::Value>);

fn refusal(status: StatusCode, detail: impl Into<String>) -> Refusal {
    (status, Json(serde_json::json!({ "error": detail.into() })))
}

/// Map a mailbox refusal onto a status code without rewording it.
///
/// The wording always comes from [`ChatSessionError`], which is also what the
/// in-process assignment path reports, so an operator reading an HTTP body and a
/// model reading a tool error see the same sentence — and the same redaction.
fn into_refusal(error: &ChatSessionError) -> Refusal {
    let status = match error {
        ChatSessionError::UnknownSession | ChatSessionError::UnknownAssignment => StatusCode::NOT_FOUND,
        ChatSessionError::BadToken => StatusCode::UNAUTHORIZED,
        ChatSessionError::NotAuthorized { .. } => StatusCode::FORBIDDEN,
        ChatSessionError::Invalid(_) => StatusCode::BAD_REQUEST,
    };
    refusal(status, error.to_string())
}

fn session_token(headers: &HeaderMap) -> String {
    headers
        .get(SESSION_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ── Register ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct RegisterRequest {
    /// Human name for this chat session; also what an `assign_allow` rule can
    /// match on. Self-declared, and documented as such.
    label: String,
    #[serde(default)]
    pid: Option<u32>,
}

#[derive(Serialize)]
pub(super) struct RegisterResponse {
    session_id: String,
    session_ref: String,
    /// Returned once and never again: only its hash is kept.
    token: String,
    label: String,
    registered_at_unix_ms: u64,
}

/// Register a `prx chat` session so the daemon can hand it work.
pub async fn post_register(
    State(state): State<AppState>,
    Json(request): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<RegisterResponse>), Refusal> {
    authorize_resource_mutation(&state, "chat_session_register", ResourceRiskLevel::Low)?;
    let registration = chat_sessions::register(&request.label, request.pid).map_err(|error| into_refusal(&error))?;
    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            session_id: registration.session_id,
            session_ref: registration.session_ref,
            token: registration.token,
            label: registration.label,
            registered_at_unix_ms: registration.registered_at_unix_ms,
        }),
    ))
}

// ── List ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(super) struct SessionResponse {
    session_id: String,
    session_ref: String,
    label: String,
    pid: Option<u32>,
    registered_at_unix_ms: u64,
    /// `never_polled` / `polling` / `silent`. A *report*: a silent session is
    /// still listed, still holds its queue, and is never evicted.
    liveness: String,
    last_poll_age_secs: Option<u64>,
    polls: u64,
    queued: usize,
    delivered: usize,
    accepted: usize,
}

#[derive(Serialize)]
pub(super) struct SessionsResponse {
    sessions: Vec<SessionResponse>,
}

/// Every registered chat session, so a caller knows what it can assign to.
pub async fn get_sessions() -> Json<SessionsResponse> {
    Json(SessionsResponse {
        sessions: chat_sessions::list()
            .into_iter()
            .map(|session| SessionResponse {
                session_id: session.session_id,
                session_ref: session.session_ref,
                label: session.label,
                pid: session.pid,
                registered_at_unix_ms: session.registered_at_unix_ms,
                liveness: session.liveness.as_str().to_string(),
                last_poll_age_secs: session.last_poll_age_secs,
                polls: session.polls,
                queued: session.queued,
                delivered: session.delivered,
                accepted: session.accepted,
            })
            .collect(),
    })
}

// ── Deregister ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(super) struct DeregisterResponse {
    session_id: String,
    /// Assignments that were still outstanding; each is recorded in the result
    /// feed as `cancelled` rather than silently dropped.
    discarded: usize,
}

/// Remove a chat session. Reachable from the operator plane without the session
/// token, so a session whose process died can be reaped by hand — which is the
/// deliberate alternative to evicting it on a clock.
pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeregisterResponse>, Refusal> {
    authorize_resource_mutation(&state, "chat_session_deregister", ResourceRiskLevel::Medium)?;
    let discarded = chat_sessions::deregister(&id).ok_or_else(|| into_refusal(&ChatSessionError::UnknownSession))?;
    Ok(Json(DeregisterResponse {
        session_id: id,
        discarded,
    }))
}

// ── Assign ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct OnBehalfOf {
    channel: String,
    sender: String,
    #[serde(default)]
    chat_type: String,
}

#[derive(Deserialize)]
pub(super) struct AssignRequest {
    task: String,
    /// `queue` (default), `steer`, or `interrupt`.
    #[serde(default)]
    disposition: Option<String>,
    /// Optional correspondent to be judged as instead of the operator plane.
    ///
    /// **This narrows and can never widen.** The only thing authenticated on
    /// this endpoint is the gateway bearer token, and holding it already means
    /// operator authority — so naming a correspondent here is not an identity
    /// assertion, it is a request to *also* satisfy that correspondent's
    /// default-deny assignment rules. A caller that wants more reach simply
    /// omits the field, which is why this can never be a privilege escalation.
    ///
    /// It exists so the daemon's own agent-facing assignment path, which does
    /// hold a trusted `_zc_scope`, can be exercised over HTTP end to end.
    #[serde(default)]
    on_behalf_of: Option<OnBehalfOf>,
}

#[derive(Serialize)]
pub(super) struct AssignResponse {
    assignment_id: String,
    session_id: String,
    session_ref: String,
    disposition: String,
    /// `prx tasks list` address of this assignment, so it can be killed.
    work_id: String,
    queued_ahead: usize,
}

/// Queue one task for a chat session.
///
/// Authorization is not performed here: it happens inside
/// [`chat_sessions::assign`], which is the only way into the mailbox, so no
/// entry point can reach it having "already checked".
///
/// An immediate disposition is classified `High` rather than `Medium` for the
/// same reason a kill is: `interrupt` ends a turn that is running right now and
/// `steer` injects text into one, and both are strictly more forceful than
/// queueing behind it.
pub async fn post_assign(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AssignRequest>,
) -> Result<(StatusCode, Json<AssignResponse>), Refusal> {
    let disposition = match request.disposition.as_deref() {
        None => Disposition::Queue,
        Some(value) => Disposition::parse(value).ok_or_else(|| {
            refusal(
                StatusCode::BAD_REQUEST,
                format!("disposition {value:?} must be one of: queue, steer, interrupt"),
            )
        })?,
    };
    let risk = if matches!(disposition, Disposition::Queue) {
        ResourceRiskLevel::Medium
    } else {
        ResourceRiskLevel::High
    };
    authorize_resource_mutation(&state, "chat_session_assign", risk)?;

    let principal = request
        .on_behalf_of
        .map_or(AssignPrincipal::operator_plane(), |origin| {
            AssignPrincipal::Correspondent {
                channel: origin.channel,
                sender: origin.sender,
                chat_type: origin.chat_type,
            }
        });

    let config = state.config.load_full();
    let policy = crate::runtime::bootstrap::build_security_policy(&config);
    let receipt = chat_sessions::assign(&policy, &principal, &id, &request.task, disposition)
        .map_err(|error| into_refusal(&error))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(AssignResponse {
            assignment_id: receipt.assignment_id,
            session_id: receipt.session_id,
            session_ref: receipt.session_ref,
            disposition: receipt.disposition.as_str().to_string(),
            work_id: receipt.work_id.to_string(),
            queued_ahead: receipt.queued_ahead,
        }),
    ))
}

// ── Pull ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub(super) struct PullRequest {
    /// Assignment ids from the previous response that the caller now holds.
    #[serde(default)]
    ack: Vec<String>,
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct PulledAssignment {
    assignment_id: String,
    session_id: String,
    task: String,
    disposition: String,
    /// Greater than 1 means this is a redelivery: treat `assignment_id` as an
    /// idempotency key.
    deliveries: u32,
    created_at_unix_ms: u64,
    work_id: String,
    origin_channel: String,
    /// Redacted origin; absent when the assignment came from the operator plane.
    origin_ref: Option<String>,
}

#[derive(Serialize)]
pub(super) struct PullResponse {
    assignments: Vec<PulledAssignment>,
    acked: usize,
    requeued: usize,
    queued_remaining: usize,
}

/// Hand a chat session its queued work and acknowledge the previous batch.
///
/// Requires the per-session token; see the module note on why the operator
/// bearer token alone is not enough here.
pub async fn post_pull(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<PullRequest>>,
) -> Result<Json<PullResponse>, Refusal> {
    authorize_resource_mutation(&state, "chat_session_pull", ResourceRiskLevel::Low)?;
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let pulled = chat_sessions::pull(&id, &session_token(&headers), &request.ack, request.max)
        .map_err(|error| into_refusal(&error))?;
    Ok(Json(PullResponse {
        assignments: pulled
            .assignments
            .into_iter()
            .map(|assignment| PulledAssignment {
                assignment_id: assignment.assignment_id,
                session_id: assignment.session_id,
                task: assignment.task,
                disposition: assignment.disposition.as_str().to_string(),
                deliveries: assignment.deliveries,
                created_at_unix_ms: assignment.created_at_unix_ms,
                work_id: assignment.work_id.to_string(),
                origin_channel: assignment.origin_channel,
                origin_ref: assignment.origin_ref,
            })
            .collect(),
        acked: pulled.acked,
        requeued: pulled.requeued,
        queued_remaining: pulled.queued_remaining,
    }))
}

// ── Report ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct ResultRequest {
    assignment_id: String,
    /// `completed`, `failed` or `rejected`.
    status: String,
    summary: String,
}

#[derive(Serialize)]
pub(super) struct ResultResponse {
    assignment_id: String,
    status: String,
    /// Sequence number in the result feed, for a consumer that wants to fetch
    /// exactly this entry.
    seq: u64,
}

/// Record what a chat session made of an assignment.
pub async fn post_result(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResultRequest>,
) -> Result<Json<ResultResponse>, Refusal> {
    authorize_resource_mutation(&state, "chat_session_result", ResourceRiskLevel::Low)?;
    // `cancelled` is this runtime's own verdict about work it ended and is not
    // accepted from a session, so an operator kill can never be mistaken for a
    // client giving up.
    let status = ResultStatus::parse(&request.status).ok_or_else(|| {
        refusal(
            StatusCode::BAD_REQUEST,
            format!(
                "status {:?} must be one of: completed, failed, rejected",
                request.status
            ),
        )
    })?;
    let seq = chat_sessions::report(
        &id,
        &session_token(&headers),
        &request.assignment_id,
        status,
        &request.summary,
    )
    .map_err(|error| into_refusal(&error))?;
    Ok(Json(ResultResponse {
        assignment_id: request.assignment_id,
        status: status.as_str().to_string(),
        seq,
    }))
}

// ── Result feed ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct ResultsQuery {
    #[serde(default)]
    after_seq: u64,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct ResultEntry {
    seq: u64,
    assignment_id: String,
    session_id: String,
    session_label: String,
    disposition: String,
    status: String,
    summary: String,
    completed_at_unix_ms: u64,
    origin_channel: String,
    /// Redacted origin. The plaintext origin never leaves the process: an
    /// in-process consumer that has to answer the correspondent reads it from
    /// `AssignmentResult::origin` and still goes through the outbound gates.
    origin_ref: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ResultsResponse {
    results: Vec<ResultEntry>,
    next_seq: u64,
    /// Lowest sequence still retained. A consumer whose cursor is below it
    /// missed entries and can say so.
    oldest_seq: u64,
}

fn entry(result: &AssignmentResult) -> ResultEntry {
    ResultEntry {
        seq: result.seq,
        assignment_id: result.assignment_id.clone(),
        session_id: result.session_id.clone(),
        session_label: result.session_label.clone(),
        disposition: result.disposition.as_str().to_string(),
        status: result.status.as_str().to_string(),
        summary: result.summary.clone(),
        completed_at_unix_ms: result.completed_at_unix_ms,
        origin_channel: result.origin.channel().to_string(),
        origin_ref: result.origin.reference(),
    }
}

/// Finished assignments across every session, oldest first.
pub async fn get_results(Query(query): Query<ResultsQuery>) -> Json<ResultsResponse> {
    let page = chat_sessions::results_after(query.after_seq, query.limit.unwrap_or(64));
    Json(ResultsResponse {
        results: page.results.iter().map(entry).collect(),
        next_seq: page.next_seq,
        oldest_seq: page.oldest_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ScopeRule};
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::SqliteMemory;
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::security::op_id::ref_for_channel_recipient;
    use crate::security::pairing::PairingGuard;
    use crate::security::policy::AutonomyLevel;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    const TOKEN: &str = "zc_chat_assign_test_token";
    const SENDER: &str = "+15550001111";
    const CHANNEL: &str = "wacli";
    const LABEL: &str = "workstation";

    /// Provider the gateway state needs but these tests never reach.
    struct UnusedProvider;

    #[async_trait]
    impl Provider for UnusedProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Err(anyhow::anyhow!("test: the provider is never called"))
        }
    }

    fn config_with(workspace: &std::path::Path, rules: Vec<ScopeRule>, owners: &[&str]) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.autonomy.level = AutonomyLevel::Full;
        config.autonomy.scopes.rules = rules;
        config.autonomy.scopes.assign_owners = owners.iter().map(|entry| (*entry).to_string()).collect();
        config
    }

    fn test_app_state(config: Config) -> AppState {
        test_app_state_with_api_limit(config, 10_000)
    }

    fn test_app_state_with_api_limit(config: Config, api_per_minute: u32) -> AppState {
        let workspace = config.workspace_dir.clone();
        let memory = SqliteMemory::new(&workspace).expect("test: sqlite memory");
        AppState {
            config: crate::config::new_shared(config),
            provider: Arc::new(UnusedProvider),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(memory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            turn_runtime: None,
            hooks: Arc::new(HookManager::new(workspace)),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(true, &[TOKEN.to_string()])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(10_000, 10_000, api_per_minute, 10_000)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: None,
        }
    }

    /// Serve the real API router — same routes, same auth and rate-limit layers
    /// the daemon mounts — on an ephemeral port.
    async fn serve(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .nest("/api", super::super::router(state.clone()))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind ephemeral port");
        let port = listener.local_addr().expect("test: local addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("test: http client")
    }

    async fn post(
        base_url: &str,
        path: &str,
        bearer: Option<&str>,
        session: Option<&str>,
        body: serde_json::Value,
    ) -> (u16, String) {
        let mut request = client().post(format!("{base_url}/api{path}")).json(&body);
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(session) = session {
            request = request.header(SESSION_TOKEN_HEADER, session);
        }
        let response = request.send().await.expect("test: request");
        (response.status().as_u16(), response.text().await.expect("test: body"))
    }

    async fn get(base_url: &str, path: &str) -> (u16, String) {
        let response = client()
            .get(format!("{base_url}/api{path}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("test: request");
        (response.status().as_u16(), response.text().await.expect("test: body"))
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap_or_else(|error| panic!("test: body was not json ({error}): {body}"))
    }

    fn value_at(body: &str, pointer: &str) -> serde_json::Value {
        json(body)
            .pointer(pointer)
            .cloned()
            .unwrap_or_else(|| panic!("test: {pointer} missing from {body}"))
    }

    fn string_at(body: &str, pointer: &str) -> String {
        json(body)
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("test: {pointer} missing from {body}"))
            .to_string()
    }

    async fn register_session(base_url: &str) -> (String, String) {
        let (status, body) = post(
            base_url,
            "/chat-sessions/register",
            Some(TOKEN),
            None,
            serde_json::json!({"label": LABEL, "pid": 4242}),
        )
        .await;
        assert_eq!(status, 201, "test: body was {body}");
        (string_at(&body, "/session_id"), string_at(&body, "/token"))
    }

    fn owner_rule() -> Vec<ScopeRule> {
        vec![ScopeRule {
            channel: Some(CHANNEL.to_string()),
            assign_allow: vec![format!("{LABEL}:*")],
            ..ScopeRule::default()
        }]
    }

    /// The whole loop over the real routes: a session registers, work is
    /// assigned to it, it pulls the work, reports an answer, and the answer
    /// shows up on the feed a channel-side consumer reads.
    #[tokio::test]
    async fn a_session_registers_pulls_its_work_and_reports_back() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;

        let (session_id, session_token) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({"task": "summarize the repo", "disposition": "queue"}),
        )
        .await;
        assert_eq!(status, 202, "test: body was {body}");
        let assignment_id = string_at(&body, "/assignment_id");
        let work_id = string_at(&body, "/work_id");
        assert!(work_id.starts_with('w'), "test: {work_id}");

        // Listed, and reported as not having pulled yet.
        let (status, body) = get(&base_url, "/chat-sessions").await;
        assert_eq!(status, 200, "test: body was {body}");
        let listed = value_at(&body, "/sessions")
            .as_array()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.get("session_id") == Some(&serde_json::json!(session_id)))
                    .cloned()
            })
            .unwrap_or_else(|| panic!("test: the session must be listed: {body}"));
        assert_eq!(listed.get("liveness"), Some(&serde_json::json!("never_polled")));
        assert_eq!(listed.get("queued"), Some(&serde_json::json!(1)));

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");
        assert_eq!(string_at(&body, "/assignments/0/task"), "summarize the repo");
        assert_eq!(string_at(&body, "/assignments/0/assignment_id"), assignment_id);

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/result"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({"assignment_id": assignment_id, "status": "completed", "summary": "42 crates"}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");
        let seq = value_at(&body, "/seq").as_u64().unwrap_or_default();

        let (status, body) = get(&base_url, &format!("/chat-sessions/results?after_seq={}", seq - 1)).await;
        assert_eq!(status, 200, "test: body was {body}");
        assert!(body.contains("42 crates"), "test: {body}");
        assert!(body.contains(&assignment_id), "test: {body}");

        server.abort();
    }

    /// The refusal over HTTP is the same sentence, with the same redaction, that
    /// the in-process path produces — one wording, one leak surface.
    #[tokio::test]
    async fn an_unauthorized_origin_is_refused_and_the_body_names_nobody() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, session_token) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({
                "task": "do a thing",
                "on_behalf_of": {"channel": CHANNEL, "sender": SENDER, "chat_type": "direct"},
            }),
        )
        .await;

        assert_eq!(status, 403, "test: body was {body}");
        assert!(!body.contains(SENDER), "plaintext sender in the refusal: {body}");
        assert!(
            !body.contains(&session_id),
            "plaintext session id in the refusal: {body}"
        );
        assert!(
            body.contains(&ref_for_channel_recipient(CHANNEL, SENDER)),
            "test: {body}"
        );

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");
        assert_eq!(value_at(&body, "/assignments"), serde_json::json!([]));
        server.abort();
    }

    /// `on_behalf_of` narrows and never widens: the operator token alone would
    /// have been enough, and naming an origin only adds that origin's
    /// default-deny rules. The control is the same request with a rule the
    /// origin matches.
    #[tokio::test]
    async fn on_behalf_of_only_narrows_what_the_operator_token_already_permits() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), owner_rule(), &[]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, _) = register_session(&base_url).await;

        // Named origin, and a rule that names it → accepted.
        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({
                "task": "permitted",
                "on_behalf_of": {"channel": CHANNEL, "sender": SENDER, "chat_type": "direct"},
            }),
        )
        .await;
        assert_eq!(status, 202, "test: body was {body}");

        // A different origin, which no rule names → refused, even though the
        // very same bearer token is presented.
        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({
                "task": "refused",
                "on_behalf_of": {"channel": "telegram", "sender": "12345", "chat_type": "direct"},
            }),
        )
        .await;
        assert_eq!(status, 403, "test: body was {body}");
        server.abort();
    }

    /// An owner named in `autonomy.scopes.assign_owners` gets through with no
    /// rule at all.
    #[tokio::test]
    async fn a_configured_owner_may_assign() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[&format!("{CHANNEL}:{SENDER}")]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, session_token) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({
                "task": "owner task",
                "disposition": "interrupt",
                "on_behalf_of": {"channel": CHANNEL, "sender": SENDER, "chat_type": "direct"},
            }),
        )
        .await;
        assert_eq!(status, 202, "test: body was {body}");
        assert_eq!(string_at(&body, "/disposition"), "interrupt");

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");
        assert_eq!(string_at(&body, "/assignments/0/disposition"), "interrupt");
        // The pulled work carries the redacted origin, never the plaintext one.
        assert!(!body.contains(SENDER), "test: {body}");
        assert_eq!(string_at(&body, "/assignments/0/origin_channel"), CHANNEL);
        server.abort();
    }

    #[tokio::test]
    async fn the_mailbox_needs_the_session_token_not_only_the_operator_token() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, session_token) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            None,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 401, "test: body was {body}");

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some("not-the-token"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 401, "test: body was {body}");

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");
        server.abort();
    }

    #[tokio::test]
    async fn an_unauthenticated_caller_reaches_nothing() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;

        let (status, _) = post(
            &base_url,
            "/chat-sessions/register",
            None,
            None,
            serde_json::json!({"label": LABEL}),
        )
        .await;
        assert_eq!(status, 401);

        let response = client()
            .get(format!("{base_url}/api/chat-sessions"))
            .send()
            .await
            .expect("test: request");
        assert_eq!(response.status().as_u16(), 401);
        server.abort();
    }

    #[tokio::test]
    async fn a_bad_disposition_or_status_is_refused_at_the_boundary() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, session_token) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({"task": "x", "disposition": "obliterate"}),
        )
        .await;
        assert_eq!(status, 400, "test: body was {body}");
        assert!(body.contains("queue, steer, interrupt"), "test: {body}");

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/result"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({"assignment_id": "x", "status": "cancelled", "summary": "y"}),
        )
        .await;
        assert_eq!(status, 400, "test: body was {body}");
        assert!(body.contains("completed, failed, rejected"), "test: {body}");
        server.abort();
    }

    #[tokio::test]
    async fn an_unknown_session_is_reported_as_missing() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;

        let (status, body) = post(
            &base_url,
            "/chat-sessions/no-such-session/assign",
            Some(TOKEN),
            None,
            serde_json::json!({"task": "x"}),
        )
        .await;
        assert_eq!(status, 404, "test: body was {body}");

        let response = client()
            .delete(format!("{base_url}/api/chat-sessions/no-such-session"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("test: request");
        assert_eq!(response.status().as_u16(), 404);
        server.abort();
    }

    /// An operator can reap a session whose process died — the deliberate
    /// alternative to evicting it on a clock — and the work it was holding is
    /// reported as cancelled rather than vanishing.
    #[tokio::test]
    async fn an_operator_can_reap_a_session_and_its_outstanding_work() {
        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state(config)).await;
        let (session_id, _) = register_session(&base_url).await;

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({"task": "will be reaped"}),
        )
        .await;
        assert_eq!(status, 202, "test: body was {body}");
        let assignment_id = string_at(&body, "/assignment_id");

        let response = client()
            .delete(format!("{base_url}/api/chat-sessions/{session_id}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("test: request");
        assert_eq!(response.status().as_u16(), 200);
        let body = response.text().await.expect("test: body");
        assert_eq!(value_at(&body, "/discarded"), serde_json::json!(1));

        let (status, body) = get(&base_url, "/chat-sessions/results?after_seq=0&limit=256").await;
        assert_eq!(status, 200, "test: body was {body}");
        let cancelled = value_at(&body, "/results").as_array().is_some_and(|results| {
            results.iter().any(|result| {
                result.get("assignment_id") == Some(&serde_json::json!(assignment_id))
                    && result.get("status") == Some(&serde_json::json!("cancelled"))
            })
        });
        assert!(cancelled, "test: {body}");
        server.abort();
    }

    /// Several chats polling their mailboxes at once must not throttle each
    /// other, or the operator, out of the daemon.
    ///
    /// Every `prx chat` on a host polls the same daemon from the same address,
    /// and with pairing on it presents the same operator bearer token, so both
    /// of the API limiter's keys put every chat on the host into one bucket:
    /// the quota was divided by however many chats were open, and at the
    /// shipped default it broke at *two*. The mailbox is an internal poll
    /// between local processes and is not metered at all — the point of this
    /// test is that four sessions each spend a full minute of polls, far past
    /// the quota, and are all served, and that an operator call afterwards is
    /// served too.
    #[tokio::test]
    async fn mailbox_polling_is_never_throttled_by_the_api_quota() {
        const SESSIONS: u32 = 4;

        let api_limit = Config::default().gateway.api_rate_limit_per_minute;
        let window_millis = u128::from(crate::gateway::RATE_LIMIT_WINDOW_SECS) * 1_000;
        let poll_millis = crate::chat::sessions::assignment::POLL_INTERVAL.as_millis().max(1);
        let polls_per_minute = u32::try_from(window_millis / poll_millis).unwrap_or(u32::MAX);

        // Without this the test would pass on a quota that still applied.
        assert!(
            u64::from(polls_per_minute) * u64::from(SESSIONS) > u64::from(api_limit),
            "{SESSIONS} sessions at {polls_per_minute} polls/min must exceed the {api_limit}/min quota"
        );

        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state_with_api_limit(config, api_limit)).await;

        let mut sessions = Vec::new();
        for _ in 0..SESSIONS {
            let (session_id, session_token) = register_session(&base_url).await;
            let (status, body) = post(
                &base_url,
                &format!("/chat-sessions/{session_id}/assign"),
                Some(TOKEN),
                None,
                serde_json::json!({"task": "one task each"}),
            )
            .await;
            assert_eq!(status, 202, "test: body was {body}");
            sessions.push((session_id, session_token, string_at(&body, "/assignment_id")));
        }

        for poll in 1..=polls_per_minute {
            for (session_id, session_token, _) in &sessions {
                let (status, body) = post(
                    &base_url,
                    &format!("/chat-sessions/{session_id}/inbox/pull"),
                    Some(TOKEN),
                    Some(session_token),
                    serde_json::json!({}),
                )
                .await;
                assert_eq!(
                    status, 200,
                    "test: poll {poll} of {session_id} answered {status}: {body}"
                );
            }
        }

        // Reporting an answer is the other half of the same poll and is exempt
        // for the same reason.
        for (session_id, session_token, assignment_id) in &sessions {
            let (status, body) = post(
                &base_url,
                &format!("/chat-sessions/{session_id}/result"),
                Some(TOKEN),
                Some(session_token),
                serde_json::json!({"assignment_id": assignment_id, "status": "completed", "summary": "done"}),
            )
            .await;
            assert_eq!(status, 200, "test: body was {body}");
        }

        // Exempt from the quota, never from authentication: the mailbox is off
        // the rate-limit layer, not off the bearer check.
        let unauthenticated = sessions
            .first()
            .map(|(session_id, session_token, _)| (session_id.clone(), session_token.clone()))
            .unwrap_or_default();
        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{}/inbox/pull", unauthenticated.0),
            None,
            Some(&unauthenticated.1),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 401, "test: body was {body}");

        // And the operator, whose budget the polling never touched, is served.
        let (status, body) = get(&base_url, "/chat-sessions").await;
        assert_eq!(status, 200, "test: body was {body}");

        server.abort();
    }

    /// The exemption covers the mailbox and stops there.
    ///
    /// `assign` is the reachable end of this feature — an agent handling a
    /// message on a channel calls it — so it stays metered with the rest of the
    /// API. Sharing a file with two exempt routes must not be enough to make it
    /// one of them.
    #[tokio::test]
    async fn assigning_work_is_still_rate_limited() {
        const API_LIMIT: u32 = 4;

        let workspace = TempDir::new().expect("test: workspace");
        let config = config_with(workspace.path(), vec![], &[]);
        let (base_url, server) = serve(test_app_state_with_api_limit(config, API_LIMIT)).await;

        // Registering spends the first of the four.
        let (session_id, session_token) = register_session(&base_url).await;

        for attempt in 2..=API_LIMIT {
            let (status, body) = post(
                &base_url,
                &format!("/chat-sessions/{session_id}/assign"),
                Some(TOKEN),
                None,
                serde_json::json!({"task": "within the quota"}),
            )
            .await;
            assert_eq!(status, 202, "test: attempt {attempt} answered {status}: {body}");
        }

        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/assign"),
            Some(TOKEN),
            None,
            serde_json::json!({"task": "over the quota"}),
        )
        .await;
        assert_eq!(status, 429, "test: body was {body}");
        assert!(body.contains("Too many API requests"), "test: {body}");

        // The other half of the same shared quota: the mailbox of the very
        // session that just exhausted it still answers.
        let (status, body) = post(
            &base_url,
            &format!("/chat-sessions/{session_id}/inbox/pull"),
            Some(TOKEN),
            Some(&session_token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "test: body was {body}");

        server.abort();
    }
}

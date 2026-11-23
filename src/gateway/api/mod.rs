use super::AppState;
use crate::config::Config;
use crate::security::install_owner::InstallOwnerId;
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, delete, get, patch, post, put},
};

mod agent_erasure;
mod agents;
pub(in crate::gateway) mod capabilities;
mod causal;
mod channels;
mod chat_sessions;
mod config;
mod cost;
mod cron_jobs;
mod decision_engine;
mod hooks;
mod identity;
mod xin_jobs;
pub(in crate::gateway) use identity::verify_startup_identity;
mod jobs;
mod logs;
mod manage;
mod mcp;
mod memory;
mod models;
mod operations_tools;
mod owner;
mod plugins;
mod providers;
mod runtime;
mod sessions;
#[cfg(test)]
pub(in crate::gateway) use sessions::post_session_message;
#[cfg(test)]
pub(crate) use sessions::{drive_disabled_engine_console_entry, drive_search_console_entry};
mod skills;
mod skills_h2;
mod skills_lifecycle;
mod status;
mod sync_snapshot;
pub(super) mod sync_startup;
mod sync_ws;
#[cfg(test)]
mod test_support;

pub(super) fn authorize_resource_mutation(
    state: &AppState,
    operation_name: &str,
    risk: ResourceRiskLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    // Authorization reads the sole active ConfigGeneration, so config reloads
    // take effect consistently at every API authorization point.
    let config = state.config.load_full();
    authorize_resource_mutation_for_config(&config, operation_name, risk)
}

pub(super) fn authorize_resource_mutation_for_config(
    config: &Config,
    operation_name: &str,
    risk: ResourceRiskLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    // D2 / FIX-P1-31: build via the shared `build_security_policy` helper so this
    // gateway authz site cannot drift from (or forget) the audit-config wiring — the
    // construction is byte-for-byte identical to the former local
    // `from_config(&autonomy, &workspace_dir)` + audit-config of `security.audit`, and
    // it stays on the same audit-wiring baseline as bootstrap (BUG-D1-01 class).
    let policy = crate::runtime::bootstrap::build_security_policy(config);
    crate::security::SideEffectGate::new(&policy)
        .authorize_resource_operation("gateway_api", operation_name, risk, None)
        .map(|_| ())
        .map_err(|error| (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": error}))))
}

pub fn router(state: AppState) -> Router<AppState> {
    // The provider/model management plane (SP-PMM §2.8). Every endpoint
    // calls the one management service; a URL that carries a secret is
    // refused before a handler runs (a credential goes only in the body of
    // the credential `PUT`). Model ids carry `/`, so they travel in the query
    // or the body, never in the path.
    let management_routes = Router::new()
        .route(
            "/providers",
            get(providers::get_providers).post(providers::post_provider),
        )
        .route(
            "/providers/{id}",
            get(providers::get_provider)
                .patch(providers::patch_provider)
                .delete(providers::delete_provider),
        )
        .route(
            "/providers/{id}/credential",
            put(providers::put_credential).delete(providers::delete_credential),
        )
        .route("/providers/{id}/test", post(providers::post_test))
        .route(
            "/models",
            get(models::get_models)
                .post(models::post_model)
                .patch(models::patch_model)
                .delete(models::delete_model),
        )
        .route(
            "/models/settings",
            get(models::get_model_settings).put(models::put_model_settings),
        )
        .route("/defaults", get(models::get_defaults).put(models::put_defaults))
        .route("/routes", get(models::get_routes).post(models::post_route))
        .route(
            "/routes/{hint}",
            patch(models::patch_route).delete(models::delete_route),
        )
        .route("/config/plans", post(manage::post_plan))
        .route("/config/apply", post(manage::post_apply))
        .route("/config/history", get(manage::get_history))
        .route("/config/rollback", post(manage::post_rollback))
        .route_layer(middleware::from_fn(manage::refuse_secrets_in_url));

    let causal_routes = causal::RETIRED_ROUTES
        .into_iter()
        .fold(Router::new(), |router, path| router.route(path, any(causal::gone)));

    let decision_engine_routes = Router::new()
        .route("/decision-engine/status", get(decision_engine::get_status))
        .route("/decision-engine/backends", get(decision_engine::get_backends))
        .route("/decision-engine/analyses", get(decision_engine::get_analyses))
        .route("/decision-engine/outcomes", get(decision_engine::get_outcomes))
        .route("/decision-engine/config/plan", post(decision_engine::post_plan))
        .route("/decision-engine/config/apply", post(decision_engine::post_apply))
        .route("/decision-engine/credential", put(decision_engine::put_credential))
        .route_layer(middleware::from_fn(decision_engine::refuse_secrets_in_url));

    let protected_routes = Router::new().merge(xin_jobs::router(state.clone()))
        .route("/status", get(status::get_status))
        .route("/identity",get(identity::get_identity))
        .route("/sessions/approvals", get(sessions::get_session_approvals))
        .route(
            "/sessions/{id}/messages",
            get(sessions::get_session_messages),
        )
        .route(
            "/sessions/{id}/approvals/{decision_id}",
            get(sessions::get_decision_approval)
                .post(sessions::post_decision_approval)
                .delete(sessions::delete_decision_approval),
        )
        .route("/channels", get(channels::get_channels_status))
        // Phase 1: alias for frontend compatibility
        .route("/channels/status", get(channels::get_channels_status))
        // Outbound on behalf of an entry point that holds no channel of its own
        // (`prx chat`). Authenticated operator plane, and still subject to the
        // same outbound scope rules as an in-process send.
        .route("/channels/{name}/send", post(channels::post_channel_send))
        // daemon → `prx chat` assignment. Chat has no listener of its own, so
        // the daemon holds a mailbox per session and chat pulls from it; the
        // authorization for filling that mailbox is default-deny and lives in
        // `runtime::chat_sessions::assign`.
        .route("/chat-sessions", get(chat_sessions::get_sessions))
        .route("/chat-sessions/register", post(chat_sessions::post_register))
        .route("/chat-sessions/results", get(chat_sessions::get_results))
        .route("/chat-sessions/{id}", delete(chat_sessions::delete_session))
        .route("/chat-sessions/{id}/assign", post(chat_sessions::post_assign))
        .route("/config", get(config::get_config))
        .route("/config/files", get(config::get_config_files))
        .route("/config/files/{filename}", put(config::put_config_file))
        // Write-only: a credential goes in, and no response carries it back.
        .route("/config/secrets", put(config::put_config_secret))
        .route("/config/schema", get(config::get_config_schema))
        .route("/config", post(config::post_config))
        .route("/config", put(config::post_config))
        // Phase 2: read-only endpoints
        .route("/hooks", get(hooks::get_hooks))
        .route("/mcp/servers", get(mcp::get_mcp_servers))
        .route("/skills", get(skills::get_skills))
        .route("/skills/discover", get(skills::discover_skills))
        .route("/skills/install", post(skills::install_skill))
        .route("/skills/{name}", delete(skills::uninstall_skill))
        // Phase 3: CRUD endpoints
        .route("/hooks", post(hooks::create_hook))
        .route("/hooks/{id}", put(hooks::update_hook))
        .route("/hooks/{id}", delete(hooks::delete_hook))
        .route("/skills/{id}/toggle", patch(skills::toggle_skill))
        // Long work submitted by `?mode=async`: status, result, cancellation.
        .route("/jobs", get(jobs::get_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/cancel", post(jobs::post_job_cancel))
        // Runtime observability: what is running, and the manual kill path that
        // replaces the timeouts PRX deliberately does not have.
        .route("/runtime/tasks", get(runtime::get_tasks))
        .route("/runtime/tasks/{id}/kill", post(runtime::post_task_kill))
        // Cross-entry-point steering: address a live run by its run id and hand
        // it an operator instruction. See `runtime::post_task_message`.
        .route("/runtime/tasks/{id}/message", post(runtime::post_task_message))
        .route("/runtime/pools", get(runtime::get_pools))
        // Phase 4: WASM plugins
        .route("/plugins", get(plugins::list_plugins))
        .route("/plugins/refresh", post(plugins::refresh_plugins))
        .route("/plugins/install", post(plugins::install_plugin))
        .route("/plugins/update", post(plugins::update_plugin))
        .route("/plugins/{name}/reload", post(plugins::reload_plugin))
        .route("/plugins/{name}/enable", post(plugins::enable_plugin))
        .route("/plugins/{name}/disable", post(plugins::disable_plugin))
        .route("/plugins/{name}/remove", post(plugins::remove_plugin_post))
        .route("/plugins/{name}", delete(plugins::remove_plugin))
        .route("/cron/jobs", get(cron_jobs::list_jobs).post(cron_jobs::create_job))
        .route(
            "/cron/jobs/{id}",
            get(cron_jobs::get_job).patch(cron_jobs::patch_job).delete(cron_jobs::delete_job),
        )
        .route("/cron/jobs/{id}/runs", get(cron_jobs::list_runs))
        .route("/cron/jobs/{id}/pause", post(cron_jobs::pause_job))
        .route("/cron/jobs/{id}/resume", post(cron_jobs::resume_job))
        .route("/cron/jobs/{id}/run", post(cron_jobs::run_job))
        .route("/cron/status", get(cron_jobs::status))
        .route("/memory/search", post(memory::search))
        .route("/memory/recall", post(memory::recall))
        .route("/memory/store", post(memory::store))
        .route("/memory/forget", post(memory::forget))
        .route("/cost", get(cost::get_cost))
        .route("/skills/lifecycle/status", get(skills_lifecycle::status))
        .route("/skills/lifecycle/proposals", get(skills_lifecycle::list_proposals))
        .route("/skills/lifecycle/proposals/{id}", get(skills_lifecycle::get_proposal))
        .route(
            "/skills/lifecycle/proposals/{id}/publish",
            post(skills_lifecycle::publish),
        )
        .route("/skills/lifecycle/proposals/{id}/cancel", post(skills_lifecycle::cancel))
        .route("/skills/lifecycle/rollback", post(skills_lifecycle::rollback))
        .route("/skills/lifecycle/h2", post(skills_h2::post_h2))
        // The decision plane (CDP-15, SP-CDP-A 5.14).
        .merge(causal_routes)
        .merge(decision_engine_routes)
        .merge(management_routes)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // The two session routes that also take rf-shaped requests (AC-1b1). Their
    // bearer check is the shared one; a refused request whose body is
    // rf-shaped gets the coded `401 auth_failed`, any other the shared body.
    let session_create_route = Router::new()
        .route("/sessions", get(sessions::get_sessions).post(sessions::post_session))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            sessions::create_route_auth,
        ));
    let session_message_route = Router::new()
        .route("/sessions/{id}/message", post(sessions::post_session_message))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            sessions::message_route_auth,
        ));

    // The rf contract routes (AC-2, AC-1a1): their own guard gives every
    // error the contract's coded shape; the bearer check is the shared one,
    // and it alone authorises them (no autonomy level is consulted).
    let rf_contract_routes = Router::new()
        .route(
            "/capabilities",
            get(capabilities::get_capabilities).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/agents/catalog",
            get(capabilities::get_catalog)
                .put(agents::reserved_name)
                .delete(agents::reserved_name)
                .fallback(capabilities::method_not_allowed),
        )
        .route(
            "/agents",
            get(agents::get_agents).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/owner",
            get(owner::get_owner).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/agents/{name}",
            get(agents::get_agent)
                .put(agents::put_agent)
                .delete(agents::delete_agent)
                .fallback(agents::agent_method_not_allowed),
        )
        .route(
            "/agents/{name}/purge",
            post(agent_erasure::post_purge).fallback(|| async { capabilities::method_not_allowed_allowing("POST") }),
        )
        .route(
            "/gateway/targeted-agents",
            get(agents::get_targeted_agents)
                .put(agents::put_targeted_agents)
                .fallback(agents::switch_method_not_allowed),
        )
        .route(
            "/config/reload",
            post(config::post_config_reload).fallback(config::reload_method_not_allowed),
        )
        .route(
            "/sessions/{id}/agents/{name}/history",
            delete(agent_erasure::delete_history)
                .fallback(|| async { capabilities::method_not_allowed_allowing("DELETE") }),
        )
        .route(
            "/terminal/sessions/{id}/agents/{name}/history",
            get(agent_erasure::get_terminal_history)
                .delete(agent_erasure::delete_terminal_history)
                .fallback(|| async { capabilities::method_not_allowed_allowing("GET, DELETE") }),
        )
        .route(
            "/sessions/{id}/runs/{run_id}/cancel",
            post(sessions::post_run_cancel).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/sessions/{id}/runs/{run_id}/reply/delete",
            post(sessions::post_reply_delete).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/sessions/{id}/messages/delete",
            post(sessions::post_batch_delete).fallback(capabilities::method_not_allowed),
        )
        .route(
            "/sessions/{id}/messages/{client_message_id}/delete",
            post(sessions::post_message_delete).fallback(capabilities::method_not_allowed),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), capabilities::guard));

    // The two mailbox endpoints, and only those two. They are authenticated
    // exactly like everything above — the same bearer check, plus the
    // per-session token — but they are deliberately **not** rate limited, so
    // they are merged in after that layer is applied rather than before.
    //
    // Why they are the exception. This is a `prx chat` on this machine asking
    // its own mailbox whether the daemon has given it work, and answering when
    // it has: an internal poll between two local processes, not a public API
    // surface. The per-minute quota exists to bound what a *remote* caller can
    // spend; applying it here was an accident of mounting the mailbox under the
    // same `/api` router, and it bites immediately — one chat polling costs two
    // thirds of the default 60/min budget, so a second chat, or the operator's
    // own `curl`, gets 429 and assignment stops working. What protects a
    // mailbox is the per-session token every one of these requests must carry —
    // a secret that guessing does not get cheaper at; a quota only decides how
    // fast a chat may ask about work already addressed to it.
    //
    // The exemption stops here. `assign` — the endpoint that *puts* work in a
    // mailbox, and the one an agent on a channel reaches — stays above, rate
    // limited with the rest, as does every destructive route on this router.
    let mailbox_routes = Router::new()
        .route("/chat-sessions/{id}/inbox/pull", post(chat_sessions::post_pull))
        .route("/chat-sessions/{id}/result", post(chat_sessions::post_result))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    let sync_routes = Router::new()
        .route(
            "/sync/snapshot",
            get(sync_snapshot::snapshot).fallback(capabilities::method_not_allowed),
        )
        .route("/sync/ws", get(sync_ws::ws).fallback(capabilities::method_not_allowed))
        .route_layer(middleware::from_fn_with_state(state.clone(), capabilities::guard));
    Router::new()
        .route("/logs", get(logs::ws_handler))
        // Phase 1: alias for frontend compatibility
        .route("/logs/stream", get(logs::ws_handler))
        .route("/sessions/media", get(sessions::get_session_media))
        .merge(protected_routes)
        .merge(session_create_route)
        .merge(session_message_route)
        .merge(rf_contract_routes)
        .route_layer(middleware::from_fn_with_state(
            state,
            api_rate_limit_middleware,
        ))
        .merge(mailbox_routes)
        .merge(sync_routes)
}

pub(super) fn extract_bearer_auth_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn extract_cookie_auth_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie_header| {
            cookie_header.split(';').find_map(|entry| {
                let (name, value) = entry.trim().split_once('=')?;
                if name == "prx_console_token" {
                    Some(value.trim())
                } else {
                    None
                }
            })
        })
        .and_then(|value| {
            urlencoding::decode(value)
                .ok()
                .map(|decoded| decoded.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

fn header_matches_host_origin(headers: &HeaderMap, header_name: axum::http::HeaderName) -> bool {
    let forwarded_host = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let candidate = headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let (Some(host), Some(candidate)) = (host, candidate) else {
        return false;
    };

    let authority = candidate
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(candidate)
        .split('/')
        .next()
        .unwrap_or("");

    authority.eq_ignore_ascii_case(host) || forwarded_host.is_some_and(|value| authority.eq_ignore_ascii_case(value))
}

fn cookie_auth_allowed(headers: &HeaderMap) -> bool {
    if header_matches_host_origin(headers, header::ORIGIN) || header_matches_host_origin(headers, header::REFERER) {
        return true;
    }

    matches!(
        headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .map(str::trim),
        Some("same-origin")
    )
}

pub(super) fn extract_resource_auth_token(headers: &HeaderMap) -> String {
    extract_bearer_auth_token(headers)
        .or_else(|| {
            if cookie_auth_allowed(headers) {
                extract_cookie_auth_token(headers)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Who an authenticated API request acts for (SP-CDP-A 5.14, D-040 OQ-A10):
/// the owner of the rows it may read. It is resolved when the request
/// passes [`auth_middleware`] and inserted into the request's extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApiPrincipal {
    /// The install owner id `api:install:<32 hex>` for every paired bearer
    /// (HOST-1a: one owner per install, the same across bearer rotation,
    /// re-pairing and restarts); `api:local` for every request while pairing
    /// is off, whatever bearer it sends.
    pub(super) owner_id: String,
}

/// The owner of every request while pairing is off.
const LOCAL_OWNER_ID: &str = "api:local";

impl ApiPrincipal {
    /// The principal of every request while pairing is off.
    fn local() -> Self {
        Self {
            owner_id: LOCAL_OWNER_ID.to_owned(),
        }
    }

    /// The principal of a paired bearer.
    fn install(owner: &InstallOwnerId) -> Self {
        Self {
            owner_id: owner.as_str().to_owned(),
        }
    }

    /// The principal a paired bearer gets from the gateway test fixtures.
    #[cfg(test)]
    pub(super) fn for_tests() -> Self {
        Self::install(&InstallOwnerId::for_tests())
    }

    /// The principal of a request with pairing off. It is never
    /// owner-authenticated, so its Console turns stay owner-isolated.
    pub(super) fn is_local(&self) -> bool {
        self.owner_id == LOCAL_OWNER_ID
    }
}

/// The principal of a request that passes the pairing check; `None` when it
/// does not. With pairing off every request is `api:local` and any bearer it
/// carries is ignored; with pairing on a paired bearer (or its same-origin
/// cookie) acts for the install owner.
fn authenticated_principal(state: &AppState, headers: &HeaderMap) -> Option<ApiPrincipal> {
    if !state.pairing.require_pairing() {
        return Some(ApiPrincipal::local());
    }
    if !state.pairing.is_authenticated(&extract_resource_auth_token(headers)) {
        return None;
    }
    // The gateway always sets the install owner at start-up; a guard
    // without one authenticates nobody rather than inventing an owner.
    state.pairing.install_owner().map(ApiPrincipal::install)
}

/// Whether this request was authenticated by a paired bearer: pairing is
/// required and the bearer it carries is a paired token. When pairing is off
/// every request passes [`authenticated_principal`] without a bearer, so no
/// route grants operator rights or skips Main's level gate for it.
pub(super) fn owner_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    state.pairing.require_pairing() && state.pairing.is_authenticated(&extract_resource_auth_token(headers))
}

/// The shared body of a request the bearer check refused.
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Unauthorized"})),
    )
        .into_response()
}

async fn auth_middleware(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let Some(principal) = authenticated_principal(&state, request.headers()) else {
        return unauthorized_response();
    };
    request.extensions_mut().insert(principal);
    next.run(request).await
}

async fn api_rate_limit_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let token = extract_resource_auth_token(request.headers());
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|value| value.0);
    let rate_key = if token.is_empty() {
        super::client_key_from_request(peer_addr, request.headers(), state.trust_forwarded_headers)
    } else {
        format!("token:{}", super::hash_webhook_secret(&token))
    };

    if !state.rate_limiter.allow_api(&rate_key) {
        return rate_limited_response();
    }

    next.run(request).await
}

/// The coded `429 rate_limited` of the shared API limit. The sliding window
/// does not compute the exact wait, so both the `Retry-After` header (seconds)
/// and `retry_after_ms` carry the window length: an upper bound.
fn rate_limited_response() -> Response {
    let retry_after_ms = super::RATE_LIMIT_WINDOW_SECS.saturating_mul(1000);
    let mut response = capabilities::coded_error_value(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many API requests. Please retry later.",
        Some(("retry_after_ms", serde_json::Value::from(retry_after_ms))),
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        header::HeaderValue::from(super::RATE_LIMIT_WINDOW_SECS),
    );
    response
}

/// The coded `503 provider_not_ready` an `/api` turn route answers while the
/// turn runtime is not ready.
pub(super) fn provider_not_ready(refusal: super::TurnNotReady) -> Response {
    capabilities::coded_error_with(
        StatusCode::SERVICE_UNAVAILABLE,
        "provider_not_ready",
        "The default provider is not ready; store its credential and reload the configuration.",
        Some(("reason", refusal.0.code())),
    )
}

pub(super) fn configured_channel_names(config: &Config) -> Vec<String> {
    let mut channels = Vec::new();
    if config.channels_config.cli {
        channels.push("cli".to_string());
    }
    if config.channels_config.telegram.is_some() {
        channels.push("telegram".to_string());
    }
    if config.channels_config.discord.is_some() {
        channels.push("discord".to_string());
    }
    if config.channels_config.slack.is_some() {
        channels.push("slack".to_string());
    }
    if config.channels_config.mattermost.is_some() {
        channels.push("mattermost".to_string());
    }
    if config.channels_config.webhook.is_some() {
        channels.push("webhook".to_string());
    }
    if config.channels_config.imessage.is_some() {
        channels.push("imessage".to_string());
    }
    if config.channels_config.signal.is_some() {
        channels.push("signal".to_string());
    }
    if config.channels_config.whatsapp.is_some() {
        channels.push("whatsapp".to_string());
    }
    if config.channels_config.wacli.is_some() {
        channels.push("wacli".to_string());
    }
    if config.channels_config.linq.is_some() {
        channels.push("linq".to_string());
    }
    if config.channels_config.nextcloud_talk.is_some() {
        channels.push("nextcloud_talk".to_string());
    }
    if config.channels_config.email.is_some() {
        channels.push("email".to_string());
    }
    if config.channels_config.irc.is_some() {
        channels.push("irc".to_string());
    }
    if config.channels_config.lark.is_some() {
        channels.push("lark".to_string());
    }
    if config.channels_config.dingtalk.is_some() {
        channels.push("dingtalk".to_string());
    }
    if config.channels_config.qq.is_some() {
        channels.push("qq".to_string());
    }
    channels
}

pub(super) fn resolve_memory_backend(config: &Config) -> String {
    let backend = config.memory.backend.trim().to_ascii_lowercase();
    if backend == "sqlite" || backend == "postgres" {
        return backend;
    }

    let storage_provider = config.storage.provider.config.provider.trim().to_ascii_lowercase();
    if storage_provider == "sqlite" || storage_provider == "postgres" {
        return storage_provider;
    }

    backend
}

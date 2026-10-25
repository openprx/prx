use super::AppState;
use crate::config::Config;
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};

mod causal;
mod channels;
mod chat_sessions;
mod config;
mod hooks;
mod jobs;
mod logs;
mod manage;
mod mcp;
mod models;
mod plugins;
mod providers;
mod runtime;
mod sessions;
mod skills;
mod status;
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
        .route("/agents", get(models::get_agents))
        .route("/agents/{name}", put(models::put_agent))
        .route("/config/plans", post(manage::post_plan))
        .route("/config/apply", post(manage::post_apply))
        .route("/config/history", get(manage::get_history))
        .route("/config/rollback", post(manage::post_rollback))
        .route_layer(middleware::from_fn(manage::refuse_secrets_in_url));

    let causal_routes = Router::new()
        .route("/causal/status", get(causal::get_status))
        .route("/causal/backends", get(causal::get_backends))
        .route("/causal/test", post(causal::post_test))
        .route("/causal/decisions", get(causal::get_decisions))
        .route("/causal/decisions/{id}", get(causal::get_decision))
        .route("/causal/reports", get(causal::get_report))
        .route("/causal/reports/saved", get(causal::get_saved_reports))
        .route("/causal/config/plans", post(causal::post_plan))
        .route("/causal/config/apply", post(causal::post_apply))
        .route("/causal/credential", put(causal::put_credential))
        .route_layer(middleware::from_fn(causal::refuse_secrets_in_url));

    let protected_routes = Router::new()
        .route("/status", get(status::get_status))
        .route("/sessions", get(sessions::get_sessions).post(sessions::post_session))
        .route(
            "/sessions/{id}/messages",
            get(sessions::get_session_messages),
        )
        .route(
            "/sessions/{id}/message",
            post(sessions::post_session_message),
        )
        .route(
            "/sessions/{id}/approvals/{decision_id}",
            get(sessions::get_decision_approval)
                .post(sessions::post_decision_approval)
                .delete(sessions::delete_decision_approval),
        )
        .route(
            "/sessions/{id}/approvals/{decision_id}/resume",
            post(sessions::post_decision_resume),
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
        .route("/config/reload", post(config::post_config_reload))
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
        .route("/plugins/{name}/reload", post(plugins::reload_plugin))
        // The decision plane (CDP-15, SP-CDP-A 5.14).
        .merge(causal_routes)
        .merge(management_routes)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

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

    Router::new()
        .route("/logs", get(logs::ws_handler))
        // Phase 1: alias for frontend compatibility
        .route("/logs/stream", get(logs::ws_handler))
        .route("/sessions/media", get(sessions::get_session_media))
        .merge(protected_routes)
        .route_layer(middleware::from_fn_with_state(
            state,
            api_rate_limit_middleware,
        ))
        .merge(mailbox_routes)
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
/// the owner of the rows it may read. It is derived from the bearer that
/// passed [`auth_middleware`] and inserted into the request's extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApiPrincipal {
    /// `api:<digest>`: SHA-256 over `prx-api-owner-v1 \0 token`, truncated
    /// to 128 bits, so it is stable per token, distinct across tokens, and
    /// never the at-rest verifier of `gateway.paired_tokens` (the plain
    /// digest). `api:local` when pairing is off and no token was sent.
    pub(super) owner_id: String,
}

/// Domain separator of the API owner digest (D-040 OQ-A10).
const API_OWNER_DIGEST_DOMAIN: &str = "prx-api-owner-v1";

impl ApiPrincipal {
    /// The principal of a request that presented `token`.
    fn of_token(token: &str) -> Self {
        use sha2::{Digest, Sha256};
        if token.is_empty() {
            return Self {
                owner_id: "api:local".to_string(),
            };
        }
        let mut hasher = Sha256::new();
        hasher.update(API_OWNER_DIGEST_DOMAIN.as_bytes());
        hasher.update([0u8]);
        hasher.update(token.as_bytes());
        let digest = hex::encode(hasher.finalize());
        Self {
            owner_id: format!("api:{}", digest.get(..32).unwrap_or(digest.as_str())),
        }
    }
}

async fn auth_middleware(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let provided_token = extract_resource_auth_token(request.headers());

    if state.pairing.require_pairing() && !state.pairing.is_authenticated(&provided_token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }

    request.extensions_mut().insert(ApiPrincipal::of_token(&provided_token));
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
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "Too many API requests. Please retry later.",
                "retry_after": super::RATE_LIMIT_WINDOW_SECS,
            })),
        )
            .into_response();
    }

    next.run(request).await
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
    if config.channels_config.matrix.is_some() {
        channels.push("matrix".to_string());
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

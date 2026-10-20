//! Provider and credential endpoints (SP-PMM §2.8, PMM-F-1).
//!
//! | Endpoint | Service call | Resource risk |
//! | --- | --- | --- |
//! | `GET /api/providers` | `configured_providers` | read |
//! | `GET /api/providers/{id}` | `provider_entry` + `provider_credentials` | read |
//! | `POST /api/providers` | `ManagedChange::AddProvider` | Medium |
//! | `PATCH /api/providers/{id}` | `ManagedChange::SetProviderEndpoint` | Medium |
//! | `DELETE /api/providers/{id}` | `ManagedChange::RemoveProvider` | Medium |
//! | `PUT /api/providers/{id}/credential` | `ManagementService::set_credential` | High |
//! | `DELETE /api/providers/{id}/credential` | `ManagementService::remove_credential` | High |
//! | `POST /api/providers/{id}/test` | `connection_test::test_connection` | read; `live` Low |
//!
//! `{id}` is the provider as configured (`openai`, or a percent-encoded
//! `custom:<url>`). A credential goes in only through the body of the `PUT`,
//! which answers `204` with no body: the value is write-only, and no
//! response, log line or later read carries it. A URL that carries a secret
//! is refused before any handler runs (`manage::refuse_secrets_in_url`).

use super::AppState;
use super::manage::{self, Write, WriteQuery, manage_error_response, verbatim};
use crate::config::manage::credentials::{CredentialReferenceError, CredentialReferences};
use crate::config::manage::service::{
    configured_providers, is_configured_provider, provider_credentials, provider_entry, redact_provider_text,
};
use crate::config::manage::{ConfigActor, ManageError, ManagedChange};
use crate::providers::ProviderId;
use crate::providers::connection_test::{ProbeMode, test_connection};
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// `GET /api/providers`: every provider the configuration names, with its
/// roles and the selections that use it. No credential.
pub async fn get_providers(State(state): State<AppState>) -> Response {
    let service = manage::service(&state);
    let config = service.config();
    Json(json!({
        "providers": configured_providers(&config),
        "revision": service.revision(),
    }))
    .into_response()
}

/// A provider argument the service can construct; its text is never echoed.
fn constructible(provider: &str) -> Result<(), Box<Response>> {
    if ProviderId::of(provider.trim()).is_resolvable() {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "not a provider PRX can construct (a name from GET /api/providers, `custom:<url>` or \
                              `anthropic-custom:<url>`)"
                })),
            )
                .into_response(),
        ))
    }
}

/// `GET /api/providers/{id}`: how the configuration uses the provider, and
/// where its credentials are (ids and paths only, never a value).
pub async fn get_provider(State(state): State<AppState>, Path(provider): Path<String>) -> Response {
    if let Err(refused) = constructible(&provider) {
        return *refused;
    }
    let config = manage::service(&state).config();
    let entry = provider_entry(&config, &provider);
    let credentials = {
        let config = std::sync::Arc::clone(&config);
        crate::runtime::blocking::spawn_blocking(move || provider_credentials(&config, &provider))
            .await
            .unwrap_or_else(|error| {
                Err(ManageError::Invalid(format!(
                    "the credential store reader failed ({error})"
                )))
            })
    };
    match credentials {
        Ok(credentials) => Json(json!({"provider": entry, "credentials": credentials})).into_response(),
        Err(error) => manage_error_response(error, None, &verbatim),
    }
}

/// The body of `POST /api/providers`.
#[derive(Deserialize)]
pub struct AddProviderBody {
    provider: String,
}

/// `POST /api/providers`: add a provider to the failover chain.
pub async fn post_provider(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<AddProviderBody>,
) -> Response {
    let write = match Write::of(&query, &headers) {
        Ok(write) => write,
        Err(refused) => return *refused,
    };
    manage::apply_change(
        &state,
        ManagedChange::AddProvider {
            provider: body.provider,
        },
        write,
    )
    .await
}

/// `PATCH /api/providers/{id}`: `{"api_url": "<url>"}` sets the default
/// provider's endpoint, `{"api_url": null}` clears it. High risk (a stored
/// key goes to another host): `428` until confirmed against the revision.
pub async fn patch_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let write = match Write::of(&query, &headers) {
        Ok(write) => write,
        Err(refused) => return *refused,
    };
    let api_url = match body.get("api_url") {
        Some(Value::String(url)) => Some(url.clone()),
        Some(Value::Null) => None,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "send `api_url`: the endpoint URL, or null to clear it; nothing was written"})),
            )
                .into_response();
        }
    };
    manage::apply_change(&state, ManagedChange::SetProviderEndpoint { provider, api_url }, write).await
}

/// `DELETE /api/providers/{id}`: remove the provider's failover membership
/// and declarations. A provider a selection still uses is refused with
/// `409` listing the `referrers`.
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
) -> Response {
    let write = match Write::of(&query, &headers) {
        Ok(write) => write,
        Err(refused) => return *refused,
    };
    manage::apply_change(&state, ManagedChange::RemoveProvider { provider }, write).await
}

/// The credential name a request means: `name`, or the provider's catalog
/// name.
fn credential_name(provider: &str, name: Option<&str>) -> Result<String, Box<Response>> {
    name.map(str::trim).filter(|name| !name.is_empty()).map_or_else(
        || {
            ProviderId::of(provider.trim())
                .catalog_name()
                .map(str::to_string)
                .ok_or_else(|| {
                    Box::new(
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": "name the credential (`name`): a provider without a catalog name has no \
                                          default name; nothing was stored"
                            })),
                        )
                            .into_response(),
                    )
                })
        },
        |name| Ok(name.to_string()),
    )
}

/// `PUT /api/providers/{id}/credential`: store a credential for the
/// provider, write-only.
///
/// Body: `{"value": "<credential>", "name": "<optional name>"}`. The value
/// is stored in the encrypted auth-profile store as the reference
/// `cred-<name>` owned by the provider (`name` defaults to the provider's
/// catalog name), and the answer is `204` with no body. Neither the
/// response, a log line nor a later `GET` carries the value; an error names
/// the reference, never the value.
pub async fn put_credential(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(refused) = manage::authorize(&state, "gateway_api:providers:credential_set", ResourceRiskLevel::High) {
        return *refused;
    }
    // An empty or blank value is refused here, in this endpoint's words
    // (removal is `DELETE`), before anything reaches the store (D-089 5).
    let Some(value) = body
        .get("value")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "send the credential as a non-empty string `value` in the request body (DELETE removes \
                          it); nothing was stored"
            })),
        )
            .into_response();
    };
    let name = match credential_name(&provider, body.get("name").and_then(Value::as_str)) {
        Ok(name) => name,
        Err(refused) => return *refused,
    };
    // The service writes under the transaction locks, records a replaced
    // reference in the history (D-089 11), and keeps this request's log
    // subscriber on its worker.
    let stored = manage::service(&state)
        .set_credential(provider, name, value, ConfigActor::GatewayManagement)
        .await;
    // No error variant quotes the value (`CredentialReferenceError`), so
    // nothing is scrubbed by substring: a short value would garble the text.
    match stored {
        Ok(status) => {
            tracing::info!(
                provider = %status.owner,
                reference = %status.id,
                "Credential reference stored through the gateway"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => manage_error_response(error, None, &verbatim),
    }
}

/// The query of `DELETE /api/providers/{id}/credential`.
#[derive(Deserialize)]
pub struct CredentialQuery {
    /// The credential name; the provider's catalog name when omitted.
    #[serde(default)]
    name: Option<String>,
}

/// `DELETE /api/providers/{id}/credential?name=<name>`: remove the
/// provider's credential reference `cred-<name>`. A reference the stored
/// configuration still names is refused with `409` listing the `referrers`;
/// one issued for another provider with `409`; `204` when removed.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<CredentialQuery>,
) -> Response {
    if let Err(refused) = manage::authorize(
        &state,
        "gateway_api:providers:credential_remove",
        ResourceRiskLevel::High,
    ) {
        return *refused;
    }
    let name = match credential_name(&provider, query.name.as_deref()) {
        Ok(name) => name,
        Err(refused) => return *refused,
    };
    let id = match CredentialReferences::id_for(&name) {
        Ok(id) => id,
        Err(error) => return manage_error_response(ManageError::Credential(error), None, &verbatim),
    };
    // The owner check, the referrer check and the removal run under the
    // transaction locks (D-089 11, D-101 F-6).
    let removed = manage::service(&state)
        .remove_owned_credential(Some(&provider), &id)
        .await;
    let removed = removed.map(|removed| (id, removed));
    match removed {
        Ok((id, true)) => {
            tracing::info!(reference = %id, "Credential reference removed through the gateway");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok((id, false)) => manage_error_response(
            ManageError::Credential(CredentialReferenceError::Missing(id)),
            None,
            &verbatim,
        ),
        Err(error) => manage_error_response(error, None, &verbatim),
    }
}

/// The body of `POST /api/providers/{id}/test`.
#[derive(Deserialize)]
pub struct TestBody {
    /// The model to test; the default model when omitted.
    #[serde(default)]
    model: Option<String>,
    /// Send one live model-list request after the local check. Explicit:
    /// without it nothing leaves the process.
    #[serde(default)]
    live: bool,
}

/// `POST /api/providers/{id}/test`: resolve the provider and model locally,
/// and, only with `"live": true`, send one model-list `GET` to it. Only a
/// configured provider (the default, a fallback, or one a selection uses)
/// is tested, so the endpoint cannot be pointed at an arbitrary host.
pub async fn post_test(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<TestBody>,
) -> Response {
    let config = manage::service(&state).config();
    // Compared by provider identity, never by the redacted display text
    // (D-089 8).
    if !is_configured_provider(&config, &provider) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "only a configured provider is tested (the default, a fallback, or one a route, agent or \
                          default uses; see GET /api/providers); nothing was sent"
            })),
        )
            .into_response();
    }
    let mode = if body.live {
        if let Err(refused) = manage::authorize(&state, "gateway_api:providers:test_live", ResourceRiskLevel::Low) {
            return *refused;
        }
        ProbeMode::Live
    } else {
        ProbeMode::Local
    };
    let mut report = test_connection(&config, Some(&provider), body.model.as_deref(), mode).await;
    // The report names the provider and quotes refusals and upstream
    // errors; each is shown the way every read model shows provider text
    // (D-089 1).
    report.provider = redact_provider_text(&report.provider);
    report.local.detail = redact_provider_text(&report.local.detail);
    if let Some(live) = report.live.as_mut() {
        live.error = live.error.as_deref().map(redact_provider_text);
    }
    Json(report).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{CountingModelList, LogSink, at, json, serve_config};
    use super::CredentialReferences;
    use crate::config::secret_paths::fixture;
    use crate::security::policy::AutonomyLevel;
    use reqwest::Method;
    use serde_json::json;

    /// A credential-shaped sentinel. Not a real secret.
    const SENTINEL: &str = "pmmf-provider-sentinel-value";

    /// Every management endpoint, with a body it accepts.
    fn endpoints() -> Vec<(Method, &'static str, Option<serde_json::Value>)> {
        vec![
            (Method::GET, "/api/providers", None),
            (Method::GET, "/api/providers/openai", None),
            (Method::POST, "/api/providers", Some(json!({"provider": "openrouter"}))),
            (Method::PATCH, "/api/providers/openai", Some(json!({"api_url": null}))),
            (Method::DELETE, "/api/providers/openrouter", None),
            (
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL})),
            ),
            (Method::DELETE, "/api/providers/openai/credential", None),
            (Method::POST, "/api/providers/openai/test", Some(json!({}))),
            (Method::GET, "/api/models?provider=openai", None),
            (
                Method::POST,
                "/api/models",
                Some(json!({"provider": "openai", "model": "gpt-4o", "operations": ["chat"]})),
            ),
            (
                Method::PATCH,
                "/api/models",
                Some(json!({"provider": "openai", "model": "gpt-4o", "native_tools": true})),
            ),
            (
                Method::DELETE,
                "/api/models",
                Some(json!({"provider": "openai", "model": "gpt-4o"})),
            ),
            (Method::GET, "/api/defaults", None),
            (
                Method::PUT,
                "/api/defaults",
                Some(json!({"provider": "openai", "model": "gpt-4o-mini"})),
            ),
            (Method::GET, "/api/routes", None),
            (
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "fast", "provider": "openai", "model": "gpt-4o-mini"})),
            ),
            (Method::PATCH, "/api/routes/fast", Some(json!({"model": "gpt-4o"}))),
            (Method::DELETE, "/api/routes/fast", None),
            (Method::GET, "/api/agents", None),
            (
                Method::PUT,
                "/api/agents/coder",
                Some(json!({"provider": "openai", "model": "gpt-4o"})),
            ),
            (
                Method::POST,
                "/api/config/plans",
                Some(json!({"change": {"kind": "add_provider", "provider": "openrouter"}})),
            ),
            (
                Method::POST,
                "/api/config/apply",
                Some(json!({"change": {"kind": "add_provider", "provider": "openrouter"}})),
            ),
            (Method::GET, "/api/config/history", None),
            (Method::POST, "/api/config/rollback", Some(json!({"record_id": "none"}))),
        ]
    }

    /// F-1: every management endpoint is behind the auth layer.
    #[tokio::test]
    async fn every_management_endpoint_refuses_an_unauthenticated_request() {
        let served = serve_config(|_| {}).await;
        let before = served.disk();
        for (method, path, body) in endpoints() {
            let (status, reply) = served.call_anonymous(method.clone(), path, body).await;
            assert_eq!(status, 401, "{method} {path}: {reply}");
        }
        assert_eq!(served.disk(), before);
        assert!(!served.dir.path().join("auth-profiles.json").exists());
    }

    /// F-1: read-only autonomy refuses every management write with 403 and
    /// writes nothing; reads and dry runs still answer.
    #[tokio::test]
    async fn read_only_autonomy_refuses_every_management_write() {
        let served = serve_config(|config| config.autonomy.level = AutonomyLevel::ReadOnly).await;
        let before = served.disk();
        for (method, path, body) in endpoints() {
            let read = method == Method::GET
                || path == "/api/config/plans"
                || (path == "/api/providers/openai/test" && method == Method::POST);
            let (status, reply) = served.call(method.clone(), path, body).await;
            if read {
                assert_ne!(status, 403, "{method} {path} is a read: {reply}");
            } else {
                assert_eq!(status, 403, "{method} {path}: {reply}");
                assert!(reply.contains("read-only"), "{method} {path}: {reply}");
                // D-089 4: the supervised explanation only where it is the
                // cause.
                assert!(!reply.contains("runtime approval grant"), "{method} {path}: {reply}");
            }
            assert!(!reply.contains(SENTINEL), "{method} {path}: {reply}");
        }
        assert_eq!(served.disk(), before);
        assert!(!served.dir.path().join("auth-profiles.json").exists());
    }

    /// Supervised autonomy: a provider change (Medium) and a credential
    /// write (High) need a runtime grant the Gateway never holds.
    #[tokio::test]
    async fn supervised_autonomy_refuses_provider_and_credential_writes() {
        let served = serve_config(|config| config.autonomy.level = AutonomyLevel::Supervised).await;
        let (status, reply) = served
            .call(Method::POST, "/api/providers", Some(json!({"provider": "openrouter"})))
            .await;
        assert_eq!(status, 403, "{reply}");
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL})),
            )
            .await;
        assert_eq!(status, 403, "{reply}");
        assert!(
            reply.contains("runtime approval grant"),
            "the refusal says why: {reply}"
        );
        assert!(!reply.contains(SENTINEL), "{reply}");
        assert!(!served.dir.path().join("auth-profiles.json").exists());
        let (status, reply) = served
            .call(Method::DELETE, "/api/providers/openai/credential?name=spare", None)
            .await;
        assert_eq!(status, 403, "{reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        // A route change is Low, as `POST /api/config` is.
        let (status, reply) = served
            .call(
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "fast", "provider": "openai", "model": "gpt-4o-mini"})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
    }

    /// F-1: `PUT …/credential` takes the value from the body only, answers
    /// 204 with no body, stores it encrypted, and neither the response, the
    /// logs nor any later read carries it.
    #[tokio::test]
    async fn a_credential_put_answers_204_and_never_echoes_the_value() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        let served = serve_config(|_| {}).await;
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL})),
            )
            .await;
        assert_eq!(status, 204, "{reply}");
        assert!(reply.is_empty(), "a 204 has no body: {reply}");

        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL, "name": "spare"})),
            )
            .await;
        assert_eq!(status, 204, "{reply}");

        let store = std::fs::read_to_string(served.dir.path().join("auth-profiles.json")).expect("test: store");
        assert!(!store.contains(SENTINEL), "stored in plaintext");

        let (status, reply) = served.call(Method::GET, "/api/providers/openai", None).await;
        assert_eq!(status, 200, "{reply}");
        let ids = at(&json(&reply), "/credentials/references")
            .as_array()
            .expect("test: references")
            .iter()
            .filter_map(|reference| at(&reference, "/id").as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(ids.contains(&"cred-openai".to_string()), "{reply}");
        assert!(ids.contains(&"cred-spare".to_string()), "{reply}");
        for path in [
            "/api/providers",
            "/api/providers/openai",
            "/api/config",
            "/api/config/files",
            "/api/config/history",
            "/api/defaults",
        ] {
            let (status, reply) = served.call(Method::GET, path, None).await;
            assert_eq!(status, 200, "{path}: {reply}");
            assert!(!reply.contains(SENTINEL), "{path} leaked the credential: {reply}");
        }

        // A refused value is not echoed either.
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": format!("{SENTINEL}\nX-Injected: 1")})),
            )
            .await;
        assert_eq!(status, 400, "{reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");

        let (status, reply) = served
            .call(Method::DELETE, "/api/providers/openai/credential?name=spare", None)
            .await;
        assert_eq!(status, 204, "{reply}");
        let (status, reply) = served
            .call(Method::DELETE, "/api/providers/openai/credential?name=spare", None)
            .await;
        assert_eq!(status, 404, "{reply}");

        let logs = sink.text();
        assert!(logs.contains("cred-openai"), "the write is logged by reference: {logs}");
        assert!(!logs.contains(SENTINEL), "the logs carry the credential: {logs}");
    }

    /// F-1: a secret in the URL or the query is refused with 400, nothing is
    /// stored and nothing of it comes back.
    #[tokio::test]
    async fn a_secret_in_the_url_or_query_is_refused() {
        let served = serve_config(|_| {}).await;
        let userinfo = urlencoding::encode(&format!("custom:http://alice:{SENTINEL}@127.0.0.1:1/v1")).into_owned();
        for (method, path) in [
            (
                Method::PUT,
                format!("/api/providers/openai/credential?value={SENTINEL}"),
            ),
            (
                Method::PUT,
                format!("/api/providers/openai/credential?api_key={SENTINEL}"),
            ),
            (Method::GET, format!("/api/providers?token={SENTINEL}")),
            (Method::GET, format!("/api/providers/{userinfo}")),
            (Method::PUT, format!("/api/providers/{userinfo}/credential")),
            (
                Method::GET,
                "/api/providers?provider=openai&key=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123".to_string(),
            ),
            (
                Method::DELETE,
                format!("/api/providers/openai/credential?name=sk-proj-{SENTINEL}"),
            ),
        ] {
            let (status, reply) = served
                .call(method.clone(), &path, Some(json!({"value": "body-value-1"})))
                .await;
            assert_eq!(status, 400, "{method} {path}: {reply}");
            assert!(!reply.contains(SENTINEL), "{reply}");
            assert!(!reply.contains("sk-ant-api03"), "{reply}");
        }
        assert!(
            !served.dir.path().join("auth-profiles.json").exists(),
            "nothing may be stored"
        );
    }

    /// D-049 B: a high-risk change answers 428 with its plan and revision;
    /// `confirm_high_risk` without `If-Match` asks for the revision, a wrong
    /// revision is a conflict, and only the reported revision applies it.
    #[tokio::test]
    async fn a_high_risk_confirmation_is_bound_to_the_reported_revision() {
        let served = serve_config(|_| {}).await;
        let before = served.disk();
        let body = json!({"api_url": "https://proxy.example/v1"});

        let (status, reply) = served
            .call(Method::PATCH, "/api/providers/openai", Some(body.clone()))
            .await;
        assert_eq!(status, 428, "{reply}");
        let refused = json(&reply);
        assert_eq!(at(&refused, "/required"), "confirm_high_risk");
        assert!(
            at(&refused, "/plan")
                .as_str()
                .is_some_and(|plan| plan.contains("  - changes the provider endpoint at api_url\n")),
            "{reply}"
        );
        let revision = at(&refused, "/revision").as_str().expect("test: revision").to_string();

        let (status, reply) = served
            .call(
                Method::PATCH,
                "/api/providers/openai?confirm_high_risk=true",
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, 428, "{reply}");
        assert_eq!(at(&json(&reply), "/required"), "if_match", "{reply}");

        let other = "0".repeat(revision.len());
        let (status, reply) = served
            .call_if_match(
                Method::PATCH,
                "/api/providers/openai?confirm_high_risk=true",
                &other,
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, 409, "{reply}");
        assert_eq!(served.disk(), before);

        let (status, reply) = served
            .call_if_match(
                Method::PATCH,
                "/api/providers/openai?confirm_high_risk=true",
                &revision,
                Some(body),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(served.disk().contains("https://proxy.example/v1"));
    }

    /// D-078 carried: removing a provider a selection still uses answers 409
    /// listing every referrer; nothing is written.
    #[tokio::test]
    async fn removing_a_referenced_provider_answers_409_with_the_referrers() {
        let served = serve_config(|config| {
            config.reliability.fallback_providers = vec!["openrouter".into()];
            config.model_routes = vec![crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "fast".into(),
                provider: "openrouter".into(),
                model: "openai/gpt-4o-mini".into(),
                api_key: Some(SENTINEL.into()),
                credential_ref: None,
            }];
        })
        .await;
        let before = served.disk();
        let (status, reply) = served.call(Method::DELETE, "/api/providers/openrouter", None).await;
        assert_eq!(status, 409, "{reply}");
        let reply = json(&reply);
        assert_eq!(at(&reply, "/object"), "provider 'openrouter'");
        assert_eq!(at(&reply, "/referrers"), json!(["model route `hint:fast`"]));
        assert!(
            at(&reply, "/error")
                .as_str()
                .is_some_and(|error| error.ends_with("nothing was written")),
            "{reply}"
        );
        assert!(!reply.to_string().contains(SENTINEL));
        assert_eq!(served.disk(), before);
    }

    /// D-078 carried: a credential reference the stored configuration still
    /// names is not removed: 409 with the referrers, never the value.
    #[tokio::test]
    async fn removing_a_referenced_credential_answers_409_with_the_referrers() {
        let served = serve_config(|_| {}).await;
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL, "name": "route-key"})),
            )
            .await;
        assert_eq!(status, 204, "{reply}");
        let config_path = served.dir.path().join("config.toml");
        let mut stored = std::fs::read_to_string(&config_path).expect("test: read");
        stored.push_str("\n[decision.backends.hosted]\ncredential_ref = \"cred-route-key\"\n");
        std::fs::write(&config_path, stored).expect("test: write");

        let (status, reply) = served
            .call(Method::DELETE, "/api/providers/openai/credential?name=route-key", None)
            .await;
        assert_eq!(status, 409, "{reply}");
        let body = json(&reply);
        assert_eq!(at(&body, "/object"), "credential reference 'cred-route-key'");
        assert_eq!(
            at(&body, "/referrers"),
            json!(["decision.backends.hosted.credential_ref"])
        );
        assert!(!reply.contains(SENTINEL), "{reply}");

        // A reference issued for another provider is not removed through it.
        let before_disk = served.disk();
        let config = super::super::manage::service(&served.state).config();
        let before_reference = CredentialReferences::for_config(&config)
            .expect("test: credential store")
            .status("cred-route-key")
            .expect("test: credential status");
        let (status, reply) = served
            .call(
                Method::DELETE,
                "/api/providers/anthropic/credential?name=route-key",
                None,
            )
            .await;
        assert_eq!(status, 409, "{reply}");
        assert!(reply.contains("belongs to provider 'catalog:openai'"), "{reply}");
        assert_eq!(served.disk(), before_disk);
        assert_eq!(
            CredentialReferences::for_config(&config)
                .expect("test: credential store")
                .status("cred-route-key")
                .expect("test: credential status"),
            before_reference,
            "wrong-owner DELETE removed or changed the credential reference"
        );
    }

    /// D-078 carried: only a configured provider is tested; a local test
    /// sends nothing, and `live` sends exactly one model-list request.
    #[tokio::test]
    async fn the_test_endpoint_is_limited_to_configured_providers_and_live_is_explicit() {
        let configured = CountingModelList::start("local-model").await;
        let stranger = CountingModelList::start("local-model").await;
        let provider = format!("custom:http://127.0.0.1:{}/v1", configured.port);
        let served = {
            let provider = provider.clone();
            serve_config(move |config| {
                config.default_provider = Some(provider);
                config.default_model = Some("local-model".into());
            })
            .await
        };
        let encoded = urlencoding::encode(&provider).into_owned();

        let (status, reply) = served
            .call(Method::POST, &format!("/api/providers/{encoded}/test"), Some(json!({})))
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(at(&json(&reply), "/local/resolved"), true, "{reply}");
        assert!(at(&json(&reply), "/live").is_null(), "{reply}");
        assert_eq!(configured.count(), 0, "a local test sends nothing");

        let (status, reply) = served
            .call(
                Method::POST,
                &format!("/api/providers/{encoded}/test"),
                Some(json!({"live": true})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(at(&json(&reply), "/live/model_listed"), true, "{reply}");
        assert_eq!(configured.count(), 1, "exactly one live request");

        let other = urlencoding::encode(&format!("custom:http://127.0.0.1:{}/v1", stranger.port)).into_owned();
        let (status, reply) = served
            .call(
                Method::POST,
                &format!("/api/providers/{other}/test"),
                Some(json!({"live": true})),
            )
            .await;
        assert_eq!(status, 404, "{reply}");
        assert_eq!(stranger.count(), 0, "an unconfigured provider is never contacted");
    }

    /// D-089 5: an empty or blank value is refused up front, in this
    /// endpoint's words, and nothing is stored.
    #[tokio::test]
    async fn a_blank_credential_value_is_refused_in_the_endpoints_words() {
        let served = serve_config(|_| {}).await;
        for value in ["", "   ", "\t"] {
            let (status, reply) = served
                .call(
                    Method::PUT,
                    "/api/providers/openai/credential",
                    Some(json!({"value": value})),
                )
                .await;
            assert_eq!(status, 400, "{value:?}: {reply}");
            assert!(reply.contains("non-empty string `value`"), "{reply}");
            assert!(reply.contains("DELETE removes it"), "{reply}");
            assert!(!reply.contains("null"), "no slot wording: {reply}");
        }
        assert!(
            !served.dir.path().join("auth-profiles.json").exists(),
            "a refused value stores nothing"
        );
    }

    /// D-089 10: the store write runs on the blocking pool; its log lines
    /// reach the request's subscriber, and a failing store answers 500
    /// without the value in the answer or the logs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_credential_store_never_logs_the_value() {
        use std::os::unix::fs::PermissionsExt as _;
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        let served = serve_config(|_| {}).await;
        let dir = served.dir.path().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("test: read-only dir");
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/providers/openai/credential",
                Some(json!({"value": SENTINEL})),
            )
            .await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("test: restore dir");
        assert_eq!(status, 500, "{reply}");
        assert!(reply.contains("credential store"), "{reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");
        let logs = sink.text();
        assert!(
            logs.contains("Credential reference operation failed"),
            "the failure is logged: {logs}"
        );
        assert!(!logs.contains(SENTINEL), "the logs carry the credential: {logs}");
    }
}

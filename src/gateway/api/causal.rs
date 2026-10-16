//! The Gateway surface of the decision plane (SP-CDP-A 5.14; CDP-15).
//!
//! Every endpoint sits behind `auth_middleware` and the API rate limit, and
//! calls the services the CLI calls: the admin service
//! ([`crate::causal_tree::admin`]) for reads and tests, and the management
//! service for changes, so an equal change gives the CLI and the Gateway the
//! same plan and the same refusal text (V8.08).
//!
//! - Reads are scoped to the caller's owner ([`super::ApiPrincipal`], D-019
//!   Q-5): a decision of another owner answers `404` with the same body as
//!   an unknown or malformed id, so there is no existence oracle (V6.04).
//!   Workspace-wide inspection stays with the local CLI.
//! - Store access runs on a blocking thread (D-091 F-16): the store is
//!   SQLite. A disabled plane's decisions and reports are not read at all
//!   (D-092: long-lived readers only while enabled).
//! - `POST /api/causal/test` is local; `live: true` asks a remote backend
//!   once and needs `?confirm_high_risk=true`, the confirmation revision in
//!   `If-Match`, and the High gate (PMM-D-7).
//! - `POST /api/causal/config/plans` is a dry run of typed mutations;
//!   `POST /api/causal/config/apply` commits the same body, with the
//!   management endpoints' `If-Match` and `?confirm_high_risk=true` rules.
//!   A plan that sends decision state off the machine or changes the
//!   credential is authorized at High.
//! - `PUT /api/causal/credential` stores a new decision credential,
//!   write-only: the value is never echoed. It is High; the request is the
//!   confirmation, bound to the revision in `If-Match` (D-094 C-9).

use super::ApiPrincipal;
use super::AppState;
use super::manage::{self, Write, WriteQuery};
use crate::causal_tree::admin::{DecisionAdmin, TestMode, TestRequest, test_target};
use crate::causal_tree::config::{BackendSelection, CredentialRefSetting};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::TimeWindow;
use crate::config::manage::credentials::SecretString;
use crate::config::manage::mutation::causal::{CausalMutation, PolicyPatch, paired};
use crate::config::manage::{ConfigActor, RiskReason};
use crate::decision::BackendKind;
use crate::decision::telemetry::{CausalEventSink, ObserverCausalSink};
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Arc;

/// The actor decision-plane changes record.
const ACTOR: ConfigActor = ConfigActor::GatewayManagement;

/// The most decisions one list returns.
const MAX_LIST: u32 = 100;

/// The default length of a list.
const DEFAULT_LIST: u32 = 50;

pub(super) async fn refuse_secrets_in_url(request: axum::extract::Request, next: Next) -> Response {
    // Each handler accepts only its own query fields. A known key from a
    // different causal endpoint must not become an ignored URL parameter.
    let query_keys: &[&str] = match request.uri().path() {
        "/causal/decisions" => &["from", "to", "limit"],
        "/causal/reports" => &["from", "to"],
        "/causal/test" | "/causal/config/apply" => &["confirm_high_risk"],
        _ => &[],
    };
    if manage::url_carries_secret(request.uri(), query_keys) {
        return bad_request("a credential or unknown parameter is not accepted in a causal URL; nothing was stored");
    }
    next.run(request).await
}

/// The one answer for a decision the caller cannot see: unknown, malformed,
/// another owner's, or on a disabled plane.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no such decision"}))).into_response()
}

fn internal(message: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": message}))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()
}

/// The admin service over the current generation, emitting to the
/// Gateway's observer.
fn admin(state: &AppState) -> DecisionAdmin {
    let config = (*state.config.load_full()).clone();
    let events: Arc<dyn CausalEventSink> = Arc::new(ObserverCausalSink::new(Arc::clone(&state.observer)));
    crate::causal_cli::http_admin(config, events)
}

/// Run `work` over a fresh admin service on a blocking thread (D-091
/// F-16), then drain its store.
#[allow(clippy::result_large_err)] // the axum `Response` is the error contract of these helpers
async fn on_admin<T, F, Fut>(state: &AppState, work: F) -> Result<T, Response>
where
    F: FnOnce(Arc<DecisionAdmin>) -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    on_pinned_admin(Arc::new(admin(state)), work).await
}

#[allow(clippy::result_large_err)]
async fn on_pinned_admin<T, F, Fut>(admin: Arc<DecisionAdmin>, work: F) -> Result<T, Response>
where
    F: FnOnce(Arc<DecisionAdmin>) -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    crate::runtime::blocking::spawn_blocking(move || {
        handle.block_on(async move {
            let result = work(Arc::clone(&admin)).await;
            admin.close().await;
            result
        })
    })
    .await
    .map_err(|_| internal("the decision plane read did not finish"))
}

/// A `from`/`to` window: a day (`YYYY-MM-DD`, UTC; `to` inclusive) or an
/// RFC 3339 time.
fn window(from: &str, to: &str) -> Result<TimeWindow, Box<Response>> {
    let parse = |text: &str, end: bool| {
        crate::causal_cli::parse_time(text, end)
            .map_err(|_| Box::new(bad_request("`from` and `to` are days or RFC 3339 times")))
    };
    let window = TimeWindow {
        from_ms: parse(from, false)?,
        to_ms: parse(to, true)?,
    };
    if window.from_ms < window.to_ms {
        Ok(window)
    } else {
        Err(Box::new(bad_request("`from` must be before `to`")))
    }
}

/// `GET /api/causal/status`: the plane, the order with health and circuit,
/// exclusions and the store (redacted DTOs).
pub(super) async fn get_status(State(state): State<AppState>) -> Response {
    match on_admin(&state, |admin| async move { admin.status().await }).await {
        Ok(status) => Json(status).into_response(),
        Err(response) => response,
    }
}

/// `GET /api/causal/backends`: the computed order with exclusion reasons.
pub(super) async fn get_backends(State(state): State<AppState>) -> Response {
    Json(admin(&state).backends()).into_response()
}

/// The body of `POST /api/causal/test`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TestBody {
    /// `configured` (the default) or `builtin`.
    #[serde(default)]
    backend: Option<String>,
    /// Ask a remote backend once, live.
    #[serde(default)]
    live: bool,
}

/// `POST /api/causal/test`: the neutral fixture. Local unless `live`; a live
/// call to a remote backend needs `?confirm_high_risk=true`, the active
/// generation confirmation in `If-Match`, and the High gate. A `428` names
/// the redacted target and its confirmation revision; stale confirmations
/// return `409` without sending.
pub(super) async fn post_test(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Option<Json<TestBody>>,
) -> Response {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let kind = match body.backend.as_deref().map(str::trim) {
        None | Some("" | "configured") => None,
        Some("builtin") => Some(BackendKind::Builtin),
        Some(_) => return bad_request("`backend` is `configured` or `builtin`"),
    };
    let risk = if body.live {
        ResourceRiskLevel::High
    } else {
        ResourceRiskLevel::Low
    };
    if let Err(refused) = manage::authorize(&state, "gateway_api:causal:test", risk) {
        return *refused;
    }
    // Confirmation identifies the exact active generation and requested
    // backend. The same pinned configuration supplies both target display
    // and execution, so a reload cannot substitute a different destination.
    let runtime = state.pin_turn_runtime();
    let generation = &runtime.config_generation;
    let events: Arc<dyn CausalEventSink> = Arc::new(ObserverCausalSink::new(Arc::clone(&state.observer)));
    let pinned = Arc::new(crate::causal_cli::http_admin((*generation.effective).clone(), events));
    if body.live {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"prx.live-decision-test.v1");
        digest.update(generation.id.0.to_be_bytes());
        digest.update(generation.source_revision.fingerprint_sha256.as_bytes());
        digest.update(if kind.is_some() {
            b"builtin".as_slice()
        } else {
            b"configured".as_slice()
        });
        let revision = hex::encode(digest.finalize());
        let write = match Write::of(&query, &headers) {
            Ok(write) => write,
            Err(response) => return *response,
        };
        if !query.confirms() || write.if_match().is_none() {
            return (StatusCode::PRECONDITION_REQUIRED, Json(json!({
                "error": "a live test may be billed; confirm this target with If-Match and confirm_high_risk. Nothing was sent.",
                "required": "confirm_high_risk",
                "revision": revision,
                "target": if kind.is_some() { None } else { pinned.live_target() },
            }))).into_response();
        }
        if write.if_match() != Some(revision.as_str()) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "the live-test configuration changed; review the target again. Nothing was sent.",
                })),
            )
                .into_response();
        }
    }
    let mode = if body.live { TestMode::Live } else { TestMode::Local };
    let tested = on_pinned_admin(pinned, move |admin| async move {
        admin
            .test(TestRequest {
                target: test_target(kind),
                mode,
            })
            .await
            .map_err(|error| error.to_string())
    })
    .await;
    match tested {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(message)) => internal(&message),
        Err(response) => response,
    }
}

/// The query of `GET /api/causal/decisions`.
#[derive(Debug, Deserialize)]
pub(super) struct ListQuery {
    from: String,
    to: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /api/causal/decisions?from&to&limit`: the caller's decisions in the
/// window, newest first, at most 100.
pub(super) async fn get_decisions(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Query(query): Query<ListQuery>,
) -> Response {
    let window = match window(&query.from, &query.to) {
        Ok(window) => window,
        Err(response) => return *response,
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIST).clamp(1, MAX_LIST);
    if !state.config.load_full().causal_tree.enabled {
        return Json(json!({"decisions": []})).into_response();
    }
    let listed = on_admin(&state, move |admin| async move {
        admin
            .list(Some(&principal.owner_id), window, limit)
            .await
            .map_err(|error| error.to_string())
    })
    .await;
    match listed {
        Ok(Ok(decisions)) => Json(json!({"decisions": decisions})).into_response(),
        Ok(Err(message)) => internal(&message),
        Err(response) => response,
    }
}

/// `GET /api/causal/decisions/{id}`: one of the caller's decisions and its
/// outcome join. Anything else is the same `404`.
pub(super) async fn get_decision(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = DecisionId::parse(&id) else {
        return not_found();
    };
    if !state.config.load_full().causal_tree.enabled {
        return not_found();
    }
    let found = on_admin(&state, move |admin| async move {
        admin
            .inspect_for(Some(&principal.owner_id), &id)
            .await
            .map_err(|error| error.to_string())
    })
    .await;
    match found {
        Ok(Ok(Some(view))) => Json(view).into_response(),
        Ok(Ok(None)) => not_found(),
        Ok(Err(message)) => internal(&message),
        Err(response) => response,
    }
}

/// Read-only catalog of reports authenticated for the current principal.
pub(super) async fn get_saved_reports(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
) -> Response {
    match on_admin(&state, move |admin| async move {
        admin
            .saved_reports(&principal.owner_id)
            .map_err(|error| error.to_string())
    })
    .await
    {
        Ok(Ok(reports)) => Json(reports).into_response(),
        Ok(Err(message)) => internal(&message),
        Err(response) => response,
    }
}

/// The query of `GET /api/causal/reports`.
#[derive(Debug, Deserialize)]
pub(super) struct ReportQuery {
    from: String,
    to: String,
}

/// `GET /api/causal/reports?from&to`: the evaluation of the caller's
/// decisions in the window. Nothing is written: the report file is the
/// CLI's (`prx causal evaluate`).
pub(super) async fn get_report(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Query(query): Query<ReportQuery>,
) -> Response {
    let window = match window(&query.from, &query.to) {
        Ok(window) => window,
        Err(response) => return *response,
    };
    if !state.config.load_full().causal_tree.enabled {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "the decision plane is disabled: nothing is recorded to evaluate"})),
        )
            .into_response();
    }
    let evaluated = on_admin(&state, move |admin| async move {
        admin
            .evaluate_with(Some(&principal.owner_id), window, false)
            .await
            .map_err(|error| error.to_string())
    })
    .await;
    match evaluated {
        Ok(Ok(view)) => Json(view.report).into_response(),
        Ok(Err(message)) => internal(&message),
        Err(response) => response,
    }
}

/// One mutation of a decision-plane change, as the Gateway takes it.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum MutationBody {
    SetEnabled {
        enabled: bool,
    },
    SetBackend {
        kind: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        pinned_snapshot: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u32>,
    },
    /// Back to `kind = "auto"`: no endpoint, model, snapshot or credential
    /// reference, and remote processing off.
    ResetBackendToAuto,
    SetRemote {
        remote: bool,
    },
    /// `""`, `none` or a `cred-` reference; never a secret.
    SetCredentialRef {
        reference: String,
    },
    SetPolicy {
        #[serde(default)]
        max_candidates: Option<u8>,
        #[serde(default)]
        max_decision_hops: Option<u8>,
        #[serde(default)]
        extra_latency_budget_ms: Option<u64>,
        #[serde(default)]
        commit_threshold: Option<f32>,
        #[serde(default)]
        minimum_choice_confidence: Option<f64>,
        #[serde(default)]
        minimum_top_margin: Option<f64>,
        #[serde(default)]
        extra_token_ratio_limit: Option<f32>,
    },
    ApplyCalibration {
        report_id: String,
    },
    ResetPolicy,
}

impl MutationBody {
    fn into_mutation(self) -> Result<CausalMutation, String> {
        Ok(match self {
            Self::SetEnabled { enabled } => CausalMutation::SetEnabled(enabled),
            Self::SetBackend {
                kind,
                endpoint,
                model,
                pinned_snapshot,
                timeout_ms,
            } => {
                let BackendSelection::Fixed(kind) = BackendSelection::try_from(kind)? else {
                    return Err("`kind` names one backend kind, not `auto`".to_string());
                };
                CausalMutation::SetBackend {
                    kind,
                    endpoint,
                    model,
                    pinned_snapshot,
                    timeout_ms,
                }
            }
            Self::ResetBackendToAuto => CausalMutation::ResetBackendToAuto,
            Self::SetRemote { remote } => CausalMutation::SetRemote(remote),
            Self::SetCredentialRef { reference } => {
                CausalMutation::SetCredentialRef(CredentialRefSetting::try_from(reference)?)
            }
            Self::SetPolicy {
                max_candidates,
                max_decision_hops,
                extra_latency_budget_ms,
                commit_threshold,
                minimum_choice_confidence,
                minimum_top_margin,
                extra_token_ratio_limit,
            } => CausalMutation::SetPolicy(PolicyPatch {
                max_candidates,
                max_decision_hops,
                extra_latency_budget_ms,
                commit_threshold,
                minimum_choice_confidence,
                minimum_top_margin,
                extra_token_ratio_limit,
            }),
            Self::ApplyCalibration { report_id } => CausalMutation::ApplyCalibration { report_id },
            Self::ResetPolicy => CausalMutation::ResetPolicyToDefaults,
        })
    }
}

/// The body of a decision-plane plan or apply.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanBody {
    mutations: Vec<MutationBody>,
}

/// The mutations of `body`, with the C-6 pairing every surface applies.
fn mutations(state: &AppState, body: PlanBody) -> Result<Vec<CausalMutation>, Box<Response>> {
    if body.mutations.is_empty() {
        return Err(Box::new(bad_request("`mutations` is empty")));
    }
    let mutations = body
        .mutations
        .into_iter()
        .map(MutationBody::into_mutation)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|message| Box::new(bad_request(&message)))?;
    Ok(paired(&state.config.load_full(), mutations))
}

/// `POST /api/causal/config/plans`: the plan of the mutations; nothing is
/// written.
pub(super) async fn post_plan(State(state): State<AppState>, Json(body): Json<PlanBody>) -> Response {
    let mutations = match mutations(&state, body) {
        Ok(mutations) => mutations,
        Err(response) => return *response,
    };
    match manage::service(&state).plan_causal(&mutations, ACTOR).await {
        Ok(plan) => Json(manage::plan_body(&plan)).into_response(),
        Err(error) => manage::manage_error_response(error, None, &manage::verbatim),
    }
}

/// Whether a decision-plane plan is authorized at High: it sends decision
/// state off the machine or to another backend, or changes the credential.
fn needs_high(reasons: &[RiskReason]) -> bool {
    reasons.iter().any(|reason| {
        matches!(
            reason,
            RiskReason::RemoteDecisionEnabled { .. } | RiskReason::CredentialChanged { .. }
        )
    })
}

/// `POST /api/causal/config/apply`: plan and commit the mutations, with the
/// management endpoints' `If-Match` and confirmation rules.
pub(super) async fn post_apply(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<PlanBody>,
) -> Response {
    const OPERATION: &str = "gateway_api:causal:config";
    let write = match Write::of(&query, &headers) {
        Ok(write) => write,
        Err(response) => return *response,
    };
    if let Err(refused) = manage::authorize(&state, OPERATION, ResourceRiskLevel::Low) {
        return *refused;
    }
    let mutations = match mutations(&state, body) {
        Ok(mutations) => mutations,
        Err(response) => return *response,
    };
    let service = manage::service(&state);
    let plan = match service.plan_causal(&mutations, ACTOR).await {
        Ok(plan) => plan,
        Err(error) => return manage::manage_error_response(error, None, &manage::verbatim),
    };
    if needs_high(&plan.summary().risk_reasons)
        && let Err(refused) = manage::authorize(&state, OPERATION, ResourceRiskLevel::High)
    {
        return *refused;
    }
    manage::commit_plan(&state, &service, plan, &write, OPERATION).await
}

/// The body of `PUT /api/causal/credential`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialBody {
    value: String,
}

/// `PUT /api/causal/credential`: store a new credential for the configured
/// decision backend and reference it. Write-only; High; the request is the
/// confirmation, bound to `If-Match` (`428` names the revision without it).
pub(super) async fn put_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CredentialBody>,
) -> Response {
    const OPERATION: &str = "gateway_api:causal:credential";
    let secret = SecretString::new(body.value);
    if secret.expose().trim().is_empty() {
        return bad_request("the credential is empty; nothing was stored");
    }
    if let Err(refused) = manage::authorize(&state, OPERATION, ResourceRiskLevel::High) {
        return *refused;
    }
    let write = match Write::of(&WriteQuery::confirming(true), &headers) {
        Ok(write) => write,
        Err(response) => return *response,
    };
    let service = manage::service(&state);
    let Some(revision) = write.if_match().map(str::to_string) else {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({
                "error": "storing a decision credential is high risk: resend it with If-Match naming this revision",
                "required": "if_match",
                "revision": service.revision(),
            })),
        )
            .into_response();
    };
    let scrub = {
        let value = secret.expose().to_string();
        move |message: &str| -> String {
            if value.is_empty() {
                message.to_string()
            } else {
                message.replace(value.as_str(), "[REDACTED]")
            }
        }
    };
    match service
        .store_decision_credential(secret, ConfigActor::GatewayCredential, Some(revision.as_str()))
        .await
    {
        Ok(stored) => {
            let body: Value = json!({
                "id": stored.id,
                "record_id": stored.outcome.record_id,
                "removed_previous": stored.removed_previous,
                "previous_left": stored.previous_left.as_ref().map(|(id, _)| id),
            });
            Json(body).into_response()
        }
        Err(error) => manage::manage_error_response(error, None, &scrub),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::test_support::{TOKEN, at, json, serve_config};
    use super::*;
    use reqwest::Method;

    const LOOPBACK: &str = "http://127.0.0.1:8900/v1";

    fn loopback_plane(config: &mut crate::config::Config) {
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = LOOPBACK.into();
        config.causal_tree.backend.model = "decider-1".into();
        config.providers.models.push(
            serde_json::from_value(json_value(&format!(
                r#"{{"provider": "custom:{LOOPBACK}", "model": "decider-1", "operations": ["decision_endpoint"]}}"#
            )))
            .expect("test: declaration"),
        );
    }

    fn json_value(text: &str) -> Value {
        serde_json::from_str(text).expect("test: json")
    }

    /// Two decisions in the served workspace: one of the test token's owner,
    /// one of another owner.
    async fn seed(workspace: &std::path::Path) -> (DecisionId, DecisionId) {
        use crate::causal_tree::store::testing::record;
        let mine = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").expect("test: id");
        let theirs = DecisionId::parse("0192f0c4-0000-7000-8000-000000000002").expect("test: id");
        let owner = ApiPrincipal::of_token(TOKEN).owner_id;
        let store = crate::causal_tree::store::CausalDecisionStore::open_shared(
            workspace,
            crate::causal_tree::store::Retention::days(30, 30),
            Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
            crate::causal_tree::store::StoreMode::ReadWrite,
        )
        .expect("test: store");
        let writer = store.writer().expect("test: writer");
        let now = chrono::Utc::now().timestamp_millis();
        writer.record_decision(record(mine.clone(), "ws", Some(&owner), now));
        writer.record_decision(record(theirs.clone(), "ws", Some("api:someone-else"), now));
        writer
            .drain(std::time::Instant::now() + std::time::Duration::from_secs(10))
            .await;
        (mine, theirs)
    }

    /// V6.04 (Gateway half): the caller reads its own decision; another
    /// owner's, an unknown and a malformed id all answer the same `404`
    /// body; the list holds only the caller's decisions.
    #[tokio::test]
    async fn decisions_are_owner_scoped_with_one_not_found() {
        let served = serve_config(loopback_plane).await;
        let (mine, theirs) = seed(&served.dir.path().join("workspace")).await;
        let (status, body) = served
            .call(Method::GET, &format!("/api/causal/decisions/{mine}"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/decision_id"), Value::String(mine.to_string()));
        let mut answers = Vec::new();
        for id in [
            theirs.to_string(),
            "0192f0c4-0000-7000-8000-00000000ffff".to_string(),
            "not-an-id".to_string(),
        ] {
            answers.push(
                served
                    .call(Method::GET, &format!("/api/causal/decisions/{id}"), None)
                    .await,
            );
        }
        for (status, body) in &answers {
            assert_eq!(*status, 404, "{body}");
            assert_eq!(body, &answers[0].1, "one body for every id the caller cannot see");
        }
        let (status, body) = served
            .call(Method::GET, "/api/causal/decisions?from=2020-01-01&to=2099-12-31", None)
            .await;
        assert_eq!(status, 200, "{body}");
        let listed = at(&json(&body), "/decisions");
        let ids: Vec<&str> = listed
            .as_array()
            .expect("test: list")
            .iter()
            .filter_map(|decision| decision["decision_id"].as_str())
            .collect();
        assert_eq!(ids, [mine.to_string().as_str()]);
    }

    /// Every causal route needs the bearer.
    #[tokio::test]
    async fn causal_routes_require_auth() {
        let served = serve_config(loopback_plane).await;
        for (method, path) in [
            (Method::GET, "/api/causal/status"),
            (Method::GET, "/api/causal/backends"),
            (Method::POST, "/api/causal/test"),
            (Method::GET, "/api/causal/decisions?from=2026-01-01&to=2026-12-31"),
            (Method::POST, "/api/causal/config/plans"),
            (Method::PUT, "/api/causal/credential"),
        ] {
            let (status, _) = served.call_anonymous(method.clone(), path, None).await;
            assert_eq!(status, 401, "{method} {path}");
        }
        let (status, body) = served.call(Method::GET, "/api/causal/status", None).await;
        assert_eq!(status, 200, "{body}");
    }

    /// V8.08: the Gateway plan of a change is the CLI's plan text, from the
    /// same service; the builtin switch is paired with `SetRemote(false)`.
    #[tokio::test]
    async fn the_gateway_plan_is_the_cli_plan() {
        let served = serve_config(|config| {
            loopback_plane(config);
            config.causal_tree.backend.remote_enabled = true;
        })
        .await;
        let (status, body) = served
            .call(
                Method::POST,
                "/api/causal/config/plans",
                Some(json!({"mutations": [{"op": "set_backend", "kind": "builtin"}]})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let plan = at(&json(&body), "/plan");
        let config = served.loaded();
        let cli = crate::config::manage::ManagementService::new(crate::config::new_shared(config.clone()))
            .plan_causal(
                &paired(
                    &config,
                    vec![CausalMutation::SetBackend {
                        kind: BackendKind::Builtin,
                        endpoint: None,
                        model: None,
                        pinned_snapshot: None,
                        timeout_ms: None,
                    }],
                ),
                ConfigActor::Cli,
            )
            .await
            .expect("test: plan");
        assert_eq!(plan, Value::String(cli.text()));
        assert!(cli.text().contains("remote_enabled"), "{}", cli.text());
    }

    /// PMM-D-7: a live test is refused without the confirmation, naming the
    /// target, and nothing is sent; a local test of a remote backend sends
    /// nothing either.
    #[tokio::test]
    async fn a_live_test_needs_the_confirmation() {
        let served = serve_config(|config| {
            loopback_plane(config);
            config.causal_tree.backend.endpoint = "https://decisions.invalid/v1".into();
            config.causal_tree.backend.remote_enabled = true;
            config.providers.models.clear();
            config.providers.models.push(
                serde_json::from_value(json_value(
                    r#"{"provider": "custom:https://decisions.invalid/v1", "model": "decider-1", "operations": ["decision_endpoint"]}"#,
                ))
                .expect("test: declaration"),
            );
        })
        .await;
        let (status, body) = served
            .call(Method::POST, "/api/causal/test", Some(json!({"live": true})))
            .await;
        assert_eq!(status, 428, "{body}");
        assert_eq!(at(&json(&body), "/required"), Value::String("confirm_high_risk".into()));
        let (status, body) = served.call(Method::POST, "/api/causal/test", Some(json!({}))).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/outcome"), Value::String("requires_live".into()));
        assert_eq!(at(&json(&body), "/sent"), Value::Bool(false));
    }

    /// The credential PUT is write-only and bound to the revision: without
    /// `If-Match` it answers `428` with the revision and stores nothing; with
    /// it, the credential is stored and referenced, and no answer carries
    /// the value.
    #[tokio::test]
    async fn the_credential_put_is_write_only_and_revision_bound() {
        const SECRET: &str = "sk-GATEWAY-DECISION-SENTINEL-01";
        let served = serve_config(loopback_plane).await;
        let (status, body) = served
            .call(Method::PUT, "/api/causal/credential", Some(json!({"value": SECRET})))
            .await;
        assert_eq!(status, 428, "{body}");
        assert!(!body.contains(SECRET));
        assert!(!served.dir.path().join("auth-profiles.json").exists(), "nothing stored");
        let revision = at(&json(&body), "/revision");
        let revision = revision.as_str().expect("test: revision");
        let (status, body) = served
            .call_if_match(
                Method::PUT,
                "/api/causal/credential",
                revision,
                Some(json!({"value": SECRET})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains(SECRET), "{body}");
        let id = at(&json(&body), "/id");
        assert_eq!(
            served.loaded().causal_tree.backend.credential_ref,
            CredentialRefSetting::Profile(id.as_str().expect("test: id").to_string())
        );
    }

    /// A credential or an unknown query never reaches a causal handler, even
    /// if that handler would otherwise ignore the query string.
    #[tokio::test]
    async fn causal_urls_refuse_query_secrets_without_storing_or_echoing() {
        const SECRET: &str = "sk-GATEWAY-DECISION-URL-SENTINEL-01";
        let served = serve_config(loopback_plane).await;
        for (method, path, body) in [
            (
                Method::PUT,
                format!("/api/causal/credential?api_key={SECRET}"),
                Some(json!({"value": SECRET})),
            ),
            (
                Method::PUT,
                "/api/causal/credential?unexpected=value".to_string(),
                Some(json!({"value": SECRET})),
            ),
            (
                Method::POST,
                "/api/causal/config/plans?unexpected=value".to_string(),
                Some(json!({"mutations": []})),
            ),
            (
                Method::PUT,
                "/api/causal/credential?from=2026-01-01".to_string(),
                Some(json!({"value": SECRET})),
            ),
            (Method::GET, "/api/causal/status?limit=1".to_string(), None),
        ] {
            let (status, response) = served.call(method, &path, body).await;
            assert_eq!(status, 400, "{path}: {response}");
            assert!(!response.contains(SECRET), "{response}");
            assert!(!served.dir.path().join("auth-profiles.json").exists());
        }
        let (status, response) = served
            .call(
                Method::GET,
                "/api/causal/decisions?from=2020-01-01&to=2099-12-31&limit=1",
                None,
            )
            .await;
        assert_eq!(status, 200, "{response}");
        let (status, response) = served
            .call(Method::POST, "/api/causal/test?confirm_high_risk=true", Some(json!({})))
            .await;
        assert_eq!(status, 200, "{response}");
    }
}

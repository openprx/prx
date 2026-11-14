//! `/api/decision-engine/*` (P2-R9; strip R44): the advisory engine's
//! status, backends, bounded telemetry reads, allowlisted configuration and
//! write-only credential. Every route sits behind the API auth layer and
//! [`refuse_secrets_in_url`]. No response carries user input text, backend
//! free text or a secret, and every string field is bounded.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::manage::{self, Write, WriteQuery};
use crate::config::manage::ConfigActor;
use crate::config::manage::RiskReason;
use crate::config::manage::credentials::SecretString;
use crate::config::manage::mutation::decision_engine::{CausalMutation, EngineKeyError, engine_setting};
use crate::decision_engine::observe::{SqliteTelemetryWriter, TELEMETRY_FILE};
use crate::security::policy::ResourceRiskLevel;

/// The actor engine configuration changes record.
const ACTOR: ConfigActor = ConfigActor::GatewayManagement;

/// The default and largest `limit` of a telemetry read.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// The largest number of changes one plan takes (the allowlist size).
const MAX_CHANGES: usize = 17;

pub(super) async fn refuse_secrets_in_url(request: axum::extract::Request, next: Next) -> Response {
    let query_keys: &[&str] = match request.uri().path() {
        "/decision-engine/analyses" | "/decision-engine/outcomes" => &["limit"],
        "/decision-engine/config/apply" => &["confirm_high_risk"],
        _ => &[],
    };
    if manage::url_carries_secret(request.uri(), query_keys) {
        return invalid("query");
    }
    next.run(request).await
}

/// The typed `400` every refused request answers.
fn invalid(field: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "invalid_request", "field": field})),
    )
        .into_response()
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "decision engine telemetry is unavailable"})),
    )
        .into_response()
}

/// `limit` of a telemetry read: absent is 20; otherwise an integer in
/// `1..=100`. Zero, above 100, negative or non-integer is refused.
fn parse_limit(query: Option<&str>) -> Result<usize, Box<Response>> {
    let mut limit = None;
    for pair in query.unwrap_or_default().split('&').filter(|pair| !pair.is_empty()) {
        // A bare `limit` has an empty value, which the parse below refuses.
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "limit" || limit.is_some() {
            return Err(Box::new(invalid("limit")));
        }
        limit = Some(value);
    }
    let Some(text) = limit else {
        return Ok(DEFAULT_LIMIT);
    };
    match text.parse::<usize>() {
        Ok(value) if (1..=MAX_LIMIT).contains(&value) => Ok(value),
        _ => Err(Box::new(invalid("limit"))),
    }
}

/// `GET /api/decision-engine/status`.
pub(super) async fn get_status(State(state): State<AppState>) -> Response {
    let config = state.config.load_full();
    Json(crate::decision_engine::status::snapshot(&config)).into_response()
}

/// `GET /api/decision-engine/backends`: the effective backend, then the
/// builtin.
pub(super) async fn get_backends(State(state): State<AppState>) -> Response {
    let config = state.config.load_full();
    Json(crate::decision_engine_cli::backend_rows(&config)).into_response()
}

/// One analysis as the API shows it.
#[derive(Debug, Serialize)]
struct AnalysisView {
    decision_id: String,
    created_at_ms: i64,
    brief_redacted: String,
    brief_sha256: String,
    global: Value,
    candidates: Value,
}

/// One outcome as the API shows it.
#[derive(Debug, Serialize)]
struct OutcomeView {
    decision_id: Option<String>,
    result: &'static str,
    cost_micro_usd: Option<u64>,
    duration_ms: u64,
    tools: Vec<String>,
}

/// Read the workspace telemetry store on a blocking thread; a missing store
/// reads as empty and nothing is created.
async fn read_store<T, F>(state: &AppState, read: F) -> Result<Vec<T>, Box<Response>>
where
    T: Send + 'static,
    F: FnOnce(&SqliteTelemetryWriter) -> anyhow::Result<Vec<T>> + Send + 'static,
{
    let path = state.config.load_full().workspace_dir.join(TELEMETRY_FILE);
    let read = tokio::task::spawn_blocking(move || {
        SqliteTelemetryWriter::open_existing(&path)?.map_or_else(|| Ok(Vec::new()), |store| read(&store))
    })
    .await;
    match read {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(error)) => {
            tracing::warn!("decision engine telemetry read failed: {error}");
            Err(Box::new(internal()))
        }
        Err(error) => {
            tracing::warn!("decision engine telemetry read task failed: {error}");
            Err(Box::new(internal()))
        }
    }
}

/// `GET /api/decision-engine/analyses?limit=N`: the last `N` analyses,
/// newest first.
pub(super) async fn get_analyses(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let limit = match parse_limit(query.as_deref()) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let rows = match read_store(&state, move |store| store.analyses(limit)).await {
        Ok(rows) => rows,
        Err(response) => return *response,
    };
    let views: Vec<AnalysisView> = rows
        .into_iter()
        .map(|row| {
            // The store wrote `values_json` itself under its CHECK bound; a
            // row that does not parse shows empty values rather than failing
            // the whole read.
            let values: Value = serde_json::from_str(row.values_json()).unwrap_or(Value::Null);
            AnalysisView {
                decision_id: row.decision_id().to_string(),
                created_at_ms: row.created_at_ms(),
                brief_redacted: row.brief_redacted().to_string(),
                brief_sha256: row.brief_sha256().to_string(),
                global: values.get("global").cloned().unwrap_or_else(|| json!({})),
                candidates: values.get("candidates").cloned().unwrap_or_else(|| json!([])),
            }
        })
        .collect();
    Json(views).into_response()
}

/// `GET /api/decision-engine/outcomes?limit=N`: the last `N` outcomes,
/// newest first.
pub(super) async fn get_outcomes(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let limit = match parse_limit(query.as_deref()) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let rows = match read_store(&state, move |store| store.outcomes(limit)).await {
        Ok(rows) => rows,
        Err(response) => return *response,
    };
    let views: Vec<OutcomeView> = rows
        .into_iter()
        .map(|row| OutcomeView {
            decision_id: row.decision_id().map(str::to_string),
            result: row.result().as_str(),
            cost_micro_usd: row.cost_micro_usd(),
            duration_ms: row.duration_ms(),
            tools: row.tools().to_vec(),
        })
        .collect();
    Json(views).into_response()
}

/// The body of a configuration plan or apply.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangesBody {
    changes: BTreeMap<String, Value>,
}

/// The JSON value of a change as the allowlist parser reads it.
fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// The mutations of `body`: every key must be in the allowlist.
fn mutations(
    state: &AppState,
    body: Result<Json<ChangesBody>, JsonRejection>,
) -> Result<Vec<CausalMutation>, Box<Response>> {
    let Ok(Json(body)) = body else {
        return Err(Box::new(invalid("body")));
    };
    if body.changes.is_empty() || body.changes.len() > MAX_CHANGES {
        return Err(Box::new(invalid("changes")));
    }
    let mut mutations = Vec::with_capacity(body.changes.len());
    for (key, value) in &body.changes {
        let Some(text) = value_text(value) else {
            return Err(Box::new(invalid("changes")));
        };
        match engine_setting(key, &text) {
            Ok(setting) => mutations.push(CausalMutation::SetEngine(setting)),
            Err(EngineKeyError::Refused { .. } | EngineKeyError::InvalidValue { .. }) => {
                return Err(Box::new(invalid("changes")));
            }
        }
    }
    let config = state.config.load_full();
    Ok(crate::config::manage::mutation::decision_engine::paired(
        &config, mutations,
    ))
}

/// `POST /api/decision-engine/config/plan`: the plan; nothing is written.
pub(super) async fn post_plan(
    State(state): State<AppState>,
    body: Result<Json<ChangesBody>, JsonRejection>,
) -> Response {
    let mutations = match mutations(&state, body) {
        Ok(mutations) => mutations,
        Err(response) => return *response,
    };
    match manage::service(&state).plan_causal(&mutations, ACTOR).await {
        Ok(plan) => Json(manage::plan_body(&plan)).into_response(),
        Err(error) => manage::manage_error_response(error, None, &manage::verbatim),
    }
}

/// Whether a plan is authorized at High: it sends engine state off the
/// machine or changes the credential.
fn needs_high(reasons: &[RiskReason]) -> bool {
    reasons.iter().any(|reason| {
        matches!(
            reason,
            RiskReason::RemoteDecisionEnabled { .. } | RiskReason::CredentialChanged { .. }
        )
    })
}

/// `POST /api/decision-engine/config/apply`: plan and commit, with the
/// management endpoints' `If-Match` and confirmation rules.
pub(super) async fn post_apply(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Result<Json<ChangesBody>, JsonRejection>,
) -> Response {
    const OPERATION: &str = "gateway_api:decision_engine:config";
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

/// The body of `PUT /api/decision-engine/credential`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialBody {
    value: String,
}

/// `PUT /api/decision-engine/credential`: store a new credential for the
/// effective engine backend and reference it. Write-only; High; bound to
/// `If-Match`. Success is `204`; the value is never echoed or logged.
pub(super) async fn put_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CredentialBody>, JsonRejection>,
) -> Response {
    const OPERATION: &str = "gateway_api:decision_engine:credential";
    let Ok(Json(body)) = body else {
        return invalid("body");
    };
    let secret = SecretString::new(body.value);
    if secret.expose().trim().is_empty() {
        return invalid("value");
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
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => manage::manage_error_response(error, None, &scrub),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::test_support::{json, serve_config};
    use super::*;
    use crate::decision_engine::observe::TelemetryWriter as _;
    use crate::decision_engine::{AnalysisRecord, IntentAnalysis, IntentValue, TelemetryRecord};
    use reqwest::Method;

    /// OV-7: every retired `/api/causal/*` route answers 410 with the
    /// pointer body.
    #[tokio::test]
    async fn legacy_causal_routes_return_gone() {
        let served = serve_config(|_| {}).await;
        for (method, path) in [
            (Method::GET, "/api/causal/status"),
            (Method::GET, "/api/causal/backends"),
            (Method::POST, "/api/causal/test"),
            (Method::GET, "/api/causal/decisions"),
            (
                Method::GET,
                "/api/causal/decisions/0192f0c4-0000-7000-8000-000000000001",
            ),
            (Method::GET, "/api/causal/reports"),
            (Method::GET, "/api/causal/reports/saved"),
            (Method::POST, "/api/causal/config/plans"),
            (Method::POST, "/api/causal/config/apply"),
            (Method::PUT, "/api/causal/credential"),
        ] {
            let (status, body) = served.call(method, path, None).await;
            assert_eq!(status, 410, "{path}: {body}");
            assert_eq!(
                json(&body),
                json!({"error": "gone", "see": "/api/decision-engine"}),
                "{path}"
            );
        }
    }

    /// Analyses are bounded by `limit` and hold only the redacted brief and
    /// typed values; `limit` 0, 101 and `abc` answer 400, absent is 20 and
    /// 100 is 100.
    #[tokio::test]
    async fn analyses_are_bounded_and_redacted() {
        let served = serve_config(|_| {}).await;
        let workspace = served.loaded().workspace_dir;
        let mut store = SqliteTelemetryWriter::open(&workspace.join(TELEMETRY_FILE)).expect("test: store");
        for index in 0..120 {
            let analysis = IntentAnalysis {
                decision_id: format!("decision-{index}"),
                global: BTreeMap::from([("risk".into(), IntentValue::Probability(0.5))]),
                candidates: BTreeMap::new(),
            };
            let brief = crate::agent::terminal::redact_persisted_text(&format!("brief {index} sk-live-secret-value"));
            let record = AnalysisRecord::new(&analysis, brief, index, 5).expect("test: record");
            store
                .append_fifo(TelemetryRecord::Analysis(record), 1_000)
                .expect("test: append");
        }
        drop(store);
        for (query, expected) in [
            ("", Some(20)),
            ("?limit=100", Some(100)),
            ("?limit=0", None),
            ("?limit=101", None),
            ("?limit=abc", None),
            ("?limit=-1", None),
        ] {
            let (status, body) = served
                .call(Method::GET, &format!("/api/decision-engine/analyses{query}"), None)
                .await;
            match expected {
                Some(count) => {
                    assert_eq!(status, 200, "{query}: {body}");
                    let rows = json(&body);
                    let rows = rows.as_array().expect("test: array");
                    assert_eq!(rows.len(), count, "{query}");
                    assert_eq!(rows[0]["decision_id"], "decision-119", "newest first");
                    for row in rows {
                        assert_eq!(row["brief_sha256"].as_str().map(str::len), Some(64));
                        assert!(
                            row["brief_redacted"]
                                .as_str()
                                .is_some_and(|brief| brief.len() <= 16 * 1024)
                        );
                        assert!(!body.contains("sk-live-secret-value"), "{query}: brief is redacted");
                    }
                }
                None => {
                    assert_eq!(status, 400, "{query}: {body}");
                    assert_eq!(json(&body)["error"], "invalid_request", "{query}");
                }
            }
        }
    }

    /// Only allowlisted keys plan or apply; a secret or unknown key and a
    /// malformed body answer the typed 400.
    #[tokio::test]
    async fn config_apply_rejects_secret_and_unknown_keys() {
        let served = serve_config(|_| {}).await;
        for body in [
            json!({"changes": {"backend.credential_ref": "cred-x"}}),
            json!({"changes": {"backend.api_key": "sk-1"}}),
            json!({"changes": {"token": "t"}}),
            json!({"changes": {"apply-calibration": "r"}}),
            json!({"changes": {"policy.max_candidates": 3}}),
            json!({"changes": {}}),
            json!({"mutations": []}),
        ] {
            for path in ["/api/decision-engine/config/plan", "/api/decision-engine/config/apply"] {
                let (status, response) = served.call(Method::POST, path, Some(body.clone())).await;
                assert_eq!(status, 400, "{path} {body}: {response}");
                assert_eq!(json(&response)["error"], "invalid_request", "{path} {body}");
                assert!(!response.contains("sk-1"), "a refused value is never echoed");
            }
        }
        let (status, response) = served
            .call(
                Method::POST,
                "/api/decision-engine/config/plan",
                Some(json!({"changes": {"enabled": true, "telemetry_cap": 10}})),
            )
            .await;
        assert_eq!(status, 200, "{response}");
    }

    /// P2-R8/R9: the allowlisted keys whose names contain a credential word
    /// plan and apply through the Gateway, and the applied values load back.
    #[tokio::test]
    async fn config_apply_accepts_allowlisted_token_keys() {
        let served = serve_config(|_| {}).await;
        let body = json!({"changes": {"prompt_budget_tokens": 300, "token_ratio_limit": 0.5}});
        let (status, response) = served
            .call(Method::POST, "/api/decision-engine/config/plan", Some(body.clone()))
            .await;
        assert_eq!(status, 200, "{response}");
        let revision = json(&response)["revision"]
            .as_str()
            .expect("test: plan revision")
            .to_string();
        let (status, response) = served
            .call_if_match(Method::POST, "/api/decision-engine/config/apply", &revision, Some(body))
            .await;
        assert_eq!(status, 200, "{response}");
        let loaded = served.loaded();
        assert_eq!(loaded.decision_engine.prompt_budget_tokens, 300);
        assert!((loaded.decision_engine.token_ratio_limit - 0.5).abs() < f32::EPSILON);
    }

    /// Audit r2 F1: a backend selected through the allowlisted Gateway keys
    /// (`SetEngine`) whose catalog recommendation exceeds the configured
    /// `latency_budget_ms` is refused on plan and apply, with the engine on
    /// or off, naming both values; the stored configuration is unchanged.
    /// Raising the budget explicitly in the change admits the selection.
    #[tokio::test]
    async fn config_apply_refuses_a_backend_below_its_latency_recommendation() {
        let seed = |enabled: bool| {
            move |config: &mut crate::config::Config| {
                config.decision_engine.enabled = enabled;
                config
                    .decision_engine
                    .backend
                    .get_or_insert_with(Default::default)
                    .credential_ref = crate::config::schema::decision_engine::CredentialRefSetting::Profile(
                    "cred-openrouter-decision".to_string(),
                );
                config.providers.models.push(
                    serde_json::from_value(json!({
                        "provider": "openrouter",
                        "model": "typesafe/jev-1.13",
                        "operations": ["decision_endpoint"],
                    }))
                    .expect("test: declaration"),
                );
            }
        };
        let selection = json!({"changes": {
            "backend.kind": "openrouter",
            "backend.model": "typesafe/jev-1.13",
            "backend.pinned_snapshot": "typesafe/jev-1.13-20260917",
        }});
        for enabled in [false, true] {
            let served = serve_config(seed(enabled)).await;
            let before = served.loaded();
            for path in ["/api/decision-engine/config/plan", "/api/decision-engine/config/apply"] {
                let (status, response) = served.call(Method::POST, path, Some(selection.clone())).await;
                assert_eq!(status, 400, "enabled = {enabled}, {path}: {response}");
                let error = json(&response)["error"].as_str().unwrap_or_default().to_string();
                assert!(
                    error.contains("typesafe/jev-1.13")
                        && error.contains("3000")
                        && error.contains(&before.decision_engine.latency_budget_ms.to_string()),
                    "enabled = {enabled}, {path}: {error}"
                );
            }
            let after = served.loaded();
            assert_eq!(after.decision_engine, before.decision_engine, "enabled = {enabled}");
            assert_eq!(after.providers.models.len(), before.providers.models.len());
        }
        let served = serve_config(seed(false)).await;
        let mut raised = selection;
        raised["changes"]["latency_budget_ms"] = json!(3000);
        let (status, response) = served
            .call(Method::POST, "/api/decision-engine/config/plan", Some(raised.clone()))
            .await;
        assert_eq!(status, 200, "{response}");
        let revision = json(&response)["revision"]
            .as_str()
            .expect("test: plan revision")
            .to_string();
        let (status, response) = served
            .call_if_match(
                Method::POST,
                "/api/decision-engine/config/apply",
                &revision,
                Some(raised),
            )
            .await;
        assert_eq!(status, 200, "{response}");
        let loaded = served.loaded();
        assert_eq!(loaded.decision_engine.latency_budget_ms, 3_000);
        let backend = crate::config::schema::decision_engine::effective_engine_backend(&loaded);
        assert_eq!(backend.model, "typesafe/jev-1.13");
        assert_eq!(
            backend.kind,
            crate::config::schema::decision_engine::BackendSelection::Fixed(
                crate::config::schema::decision_engine::BackendKind::OpenRouter
            )
        );
    }
}

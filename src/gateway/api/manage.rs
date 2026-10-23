//! The Gateway surface of the provider/model management service (SP-PMM
//! §2.8, PMM-F).
//!
//! Every management endpoint calls [`ManagementService`] in-process, as the
//! CLI does: one candidate builder, one validation, one plan renderer
//! ([`ManagedPlan::text`]). An equal change therefore gives the CLI and the
//! Gateway byte-identical plan and refusal text.
//!
//! This module holds what the three endpoint modules share:
//!
//! - [`apply_change`]: plan, authorize at the level [`gate_level`] derives
//!   from what the plan changes, check that the caller's `If-Match` names
//!   the revision the plan was read from, and commit through the
//!   transaction. A high-risk plan answers `428` with the plan and its
//!   revision; the retry confirms with `?confirm_high_risk=true` and
//!   `If-Match` naming that revision (D-049 B).
//! - [`gate_level`] / [`authorize_plan`]: the one derivation of the
//!   Gateway resource level from a plan (D-089 3). Every Gateway write that
//!   has a plan uses it (`POST /api/config`, the raw editor, the typed
//!   endpoints, `/api/config/apply` and rollback), so the same change meets
//!   the same gate on every endpoint.
//! - [`manage_error_response`]: the one mapping of [`ManageError`] to a
//!   response. The `error` text is the service's own sentence, so it is the
//!   text the CLI prints, passed through `redact_provider_text` (URL
//!   userinfo, query values and secrets); a still-referenced object answers
//!   `409` with the `referrers`.
//! - [`refuse_secrets_in_url`]: a layer that answers `400` when a request
//!   line carries a secret (URL userinfo, a credential-shaped token, or a
//!   query parameter the endpoints do not take). A credential is accepted
//!   only in the body of `PUT /api/providers/{id}/credential`.
//!
//! And the endpoints of the transaction itself:
//!
//! - `POST /api/config/plans`: dry run of a typed change or a raw file;
//!   nothing is written.
//! - `POST /api/config/apply`: the same body, committed.
//! - `GET /api/config/history`: the redacted history records.
//! - `POST /api/config/rollback`: restore the before-image of a record.

use super::AppState;
use crate::config::files::ConfigRevisionConflict;
use crate::config::manage::credentials::CredentialReferenceError;
use crate::config::manage::service::redact_provider_text;
use crate::config::manage::{
    ConfigActor, ConfigChangeSummary, ConfigTransactionError, ManageError, ManagedChange, ManagedPlan,
    ManagementService, NotAppliedOutcome, RiskReason, RollbackRequest, RootCredential, SelectionOperation, render_plan,
};
use crate::config::schema::ProviderModelConfig;
use crate::security::policy::{AutonomyLevel, ResourceRiskLevel};
use axum::{
    Json,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use tracing::warn;

/// The actor every management endpoint records.
const ACTOR: ConfigActor = ConfigActor::GatewayManagement;

/// The query parameters the management endpoints take. Anything else in a
/// query is refused: a credential never travels in a URL.
const QUERY_KEYS: &[&str] = &[
    "provider",
    "model",
    "operation",
    "executable",
    "confirm_high_risk",
    "name",
];

/// The management service of the gateway's configuration.
pub(super) fn service(state: &AppState) -> ManagementService {
    ManagementService::new(Arc::clone(&state.config))
}

/// Refuse a request whose URL carries a secret: URL userinfo or a
/// credential-shaped token in a path segment or query value, or a query
/// parameter no management endpoint takes. Nothing of the URL is echoed or
/// logged.
pub(super) async fn refuse_secrets_in_url(request: Request, next: Next) -> Response {
    if url_carries_secret(request.uri(), QUERY_KEYS) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "A credential is never accepted in a URL or query string, and this query parameter is \
                          not one the endpoint takes. Send a credential in the body of PUT \
                          /api/providers/{id}/credential; nothing was stored."
            })),
        )
            .into_response();
    }
    next.run(request).await
}

pub(super) fn url_carries_secret(uri: &Uri, query_keys: &[&str]) -> bool {
    let decode = |text: &str| -> String {
        urlencoding::decode(&text.replace('+', " ")).map_or_else(|_| text.to_string(), Cow::into_owned)
    };
    let in_path = uri.path().split('/').any(|segment| looks_secret(&decode(segment)));
    let in_query = uri.query().is_some_and(|query| {
        query.split('&').filter(|pair| !pair.is_empty()).any(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode(key);
            !query_keys.contains(&key.as_str()) || looks_secret(&decode(value))
        })
    });
    in_path || in_query
}

/// URL userinfo, or anything the log redactor recognises as a secret.
fn looks_secret(text: &str) -> bool {
    matches!(crate::providers::redact_endpoint_userinfo(text), Cow::Owned(_))
        || crate::security::redaction::redact_secrets(text) != text
}

/// `?confirm_high_risk=true` of a management write.
#[derive(Debug, Default, Deserialize)]
pub(super) struct WriteQuery {
    #[serde(default)]
    confirm_high_risk: bool,
}

impl WriteQuery {
    /// Whether the request confirms a high-risk operation.
    pub(super) const fn confirms(&self) -> bool {
        self.confirm_high_risk
    }

    /// The write query of an endpoint whose own query also carries the
    /// confirmation.
    pub(super) const fn confirming(confirm_high_risk: bool) -> Self {
        Self { confirm_high_risk }
    }
}

/// How a caller commits a change: its confirmation and the revision it
/// names in `If-Match`.
pub(super) struct Write {
    confirm_high_risk: bool,
    if_match: Option<String>,
}

impl Write {
    /// The write options of a request.
    ///
    /// # Errors
    ///
    /// `400` for an `If-Match` that is not a configuration revision.
    pub(super) fn of(query: &WriteQuery, headers: &HeaderMap) -> Result<Self, Box<Response>> {
        let if_match = super::config::if_match_revision(headers)
            .map_err(|message| Box::new((StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()))?;
        Ok(Self {
            confirm_high_risk: query.confirm_high_risk,
            if_match,
        })
    }

    /// The revision the caller names in `If-Match`.
    pub(super) fn if_match(&self) -> Option<&str> {
        self.if_match.as_deref()
    }

    /// The revision a confirmation names: only a confirmed request with an
    /// `If-Match` confirms anything (D-049 B).
    fn confirmed_revision(&self) -> Option<&str> {
        if self.confirm_high_risk {
            self.if_match.as_deref()
        } else {
            None
        }
    }

    /// A confirmation sent without `If-Match` asks for the revision, not for
    /// the flag the caller already sent.
    fn explain(&self, error: ManageError) -> ManageError {
        match error {
            ManageError::Transaction(ConfigTransactionError::ConfirmationRequired { reasons, revision, .. })
                if self.confirm_high_risk && self.if_match.is_none() =>
            {
                ManageError::Transaction(ConfigTransactionError::ConfirmationRequired {
                    required: "if_match",
                    reasons,
                    revision,
                })
            }
            other => other,
        }
    }
}

/// The operation name a change is authorized under.
const fn operation_name(change: &ManagedChange) -> &'static str {
    match change {
        ManagedChange::AddProvider { .. } => "gateway_api:providers:add",
        ManagedChange::SetProviderEndpoint { .. } => "gateway_api:providers:update",
        ManagedChange::RemoveProvider { .. } => "gateway_api:providers:remove",
        ManagedChange::AddModel(_) | ManagedChange::UpdateModel(_) | ManagedChange::RemoveModel { .. } => {
            "gateway_api:models:update"
        }
        ManagedChange::MigrateRetired => "gateway_api:models:migrate-retired",
        ManagedChange::SetModelSettings { .. } => "gateway_api:models:settings",
        ManagedChange::SetDefault { .. } => "gateway_api:defaults:update",
        ManagedChange::AddRoute { .. } | ManagedChange::UpdateRoute { .. } | ManagedChange::RemoveRoute { .. } => {
            "gateway_api:routes:update"
        }
        ManagedChange::BindAgent { .. } => "gateway_api:agents:update",
    }
}

/// Whether a changed path is part of the provider set: which providers
/// receive prompts (the default, the failover chain) or where the default
/// provider's key is sent (`api_url`).
fn changes_provider_set(path: &str) -> bool {
    path == "default_provider"
        || path == "api_url"
        || path
            .strip_prefix("reliability.fallback_providers")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('['))
}

/// The Gateway resource level of a write, derived from what its plan
/// changes (D-089 3), never from the endpoint it arrived at:
///
/// - Medium when the plan changes the provider set (`default_provider`,
///   `reliability.fallback_providers`, `api_url`), re-points a provider
///   endpoint, or hands a stored key to another provider
///   (`endpoint_changed`, `credential_owner_changed`);
/// - High when the plan creates or changes a secret, or attaches a stored
///   credential reference (a credential slot, a channel, webhook or paired
///   token, an MCP environment value): the level
///   of the credential reference PUT, so a key gets one gate whichever
///   endpoint writes it (D-099 7a, D-101 F-3);
/// - Medium when it only clears secrets (compared by identity, so a
///   removed neighbour is no secret change);
/// - Low otherwise, as every other configuration write.
///
/// The shared plan classifier is unchanged (Low/High, which decides the
/// `428` confirmation); this decides only the autonomy gate, which at
/// `supervised` needs a runtime grant for Medium.
pub(super) fn gate_level(summary: &ConfigChangeSummary) -> ResourceRiskLevel {
    if !summary.secret_writes.is_empty() {
        return ResourceRiskLevel::High;
    }
    if !summary.secret_clears.is_empty() {
        return ResourceRiskLevel::Medium;
    }
    let provider_set = summary.changes.iter().any(|change| changes_provider_set(&change.path))
        || summary.risk_reasons.iter().any(|reason| {
            matches!(
                reason,
                RiskReason::EndpointChanged { .. } | RiskReason::CredentialOwnerChanged { .. }
            )
        });
    if provider_set {
        ResourceRiskLevel::Medium
    } else {
        ResourceRiskLevel::Low
    }
}

/// Authorize a planned write at the level its plan derives
/// ([`gate_level`]). A Low plan was already authorized by the entry check
/// every write makes before planning.
///
/// # Errors
///
/// The gate's `403`.
pub(super) fn authorize_plan(
    state: &AppState,
    operation: &str,
    summary: &ConfigChangeSummary,
) -> Result<(), Box<Response>> {
    match gate_level(summary) {
        ResourceRiskLevel::Low => Ok(()),
        level => authorize(state, operation, level),
    }
}

/// Authorize a management write through the gateway's resource gate, with
/// the approval semantics every gateway write has (D-087): at `supervised`
/// autonomy a Medium or High operation needs a runtime approval grant, and
/// the Gateway carries none, so it answers `403`, and only then says why
/// (D-089 4).
///
/// # Errors
///
/// The gate's `403`.
pub(super) fn authorize(state: &AppState, operation: &str, risk: ResourceRiskLevel) -> Result<(), Box<Response>> {
    super::authorize_resource_mutation(state, operation, risk).map_err(|(status, Json(mut body))| {
        let supervised = state.config.load_full().autonomy.level == AutonomyLevel::Supervised;
        if supervised
            && matches!(risk, ResourceRiskLevel::Medium | ResourceRiskLevel::High)
            && let Some(object) = body.as_object_mut()
        {
            object.insert(
                "reason".to_string(),
                Value::String(
                    "this operation is Medium or High risk; at `supervised` autonomy such an operation needs a \
                     runtime approval grant, which a Gateway request cannot carry. Use `full` autonomy or the \
                     `prx` CLI; nothing was changed."
                        .to_string(),
                ),
            );
        }
        Box::new((status, Json(body)).into_response())
    })
}

/// `409` when the caller's `If-Match` names another revision than the one
/// the change would be derived from.
fn stale(service: &ManagementService, if_match: Option<&str>) -> Option<Response> {
    let expected = if_match?;
    let actual = service.revision().unwrap_or_default();
    (expected != actual).then(|| {
        manage_error_response(
            ManageError::Transaction(ConfigTransactionError::Conflict(ConfigRevisionConflict {
                expected: expected.to_string(),
                actual,
            })),
            None,
            &verbatim,
        )
    })
}

/// Plan, authorize and commit a typed change.
pub(super) async fn apply_change(state: &AppState, change: ManagedChange, write: Write) -> Response {
    let operation = operation_name(&change);
    // Read-only autonomy refuses every write before anything is planned.
    if let Err(refused) = authorize(state, operation, ResourceRiskLevel::Low) {
        return *refused;
    }
    let service = service(state);
    match service.plan(&change, ACTOR).await {
        Ok(plan) => commit(state, &service, plan, &write, &Commit::typed(operation)).await,
        Err(error) => manage_error_response(error, None, &verbatim),
    }
}

/// What a commit adds around the plan: the operation it is authorized
/// under, how its messages are scrubbed, and a warning for the response.
struct Commit<'a> {
    operation: &'a str,
    scrub: &'a (dyn Fn(&str) -> String + Sync),
    warning: Option<String>,
}

impl<'a> Commit<'a> {
    fn typed(operation: &'a str) -> Self {
        Self {
            operation,
            scrub: &verbatim,
            warning: None,
        }
    }
}

/// Authorize a plan at its derived level, bind the caller's `If-Match` to
/// the revision the plan was read from (D-089 2: a writer that landed
/// between the caller's read and this plan answers `409`, never a change on
/// a revision nobody named), and commit it. The response carries the plan
/// text either way.
async fn commit(
    state: &AppState,
    service: &ManagementService,
    plan: ManagedPlan,
    write: &Write,
    around: &Commit<'_>,
) -> Response {
    if let Err(refused) = authorize_plan(state, around.operation, plan.summary()) {
        return *refused;
    }
    if let Some(expected) = write.if_match.as_deref()
        && plan.revision() != Some(expected)
    {
        return manage_error_response(
            ManageError::Transaction(ConfigTransactionError::Conflict(ConfigRevisionConflict {
                expected: expected.to_string(),
                actual: plan.revision().unwrap_or_default().to_string(),
            })),
            None,
            &verbatim,
        );
    }
    let text = plan.text();
    match service.apply(plan, ACTOR, write.confirmed_revision()).await {
        Ok(outcome) => {
            let mut body = super::config::applied_body(&outcome);
            if let Some(object) = body.as_object_mut() {
                object.insert("plan".to_string(), Value::String(text));
                if let Some(warning) = &around.warning {
                    object.insert("warning".to_string(), Value::String(warning.clone()));
                }
            }
            Json(body).into_response()
        }
        Err(error) => manage_error_response(write.explain(error), Some(&text), around.scrub),
    }
}

/// Authorize and commit a typed plan under `operation` (the decision
/// plane's changes, CDP-15), exactly as a management change.
pub(super) async fn commit_plan(
    state: &AppState,
    service: &ManagementService,
    plan: ManagedPlan,
    write: &Write,
    operation: &str,
) -> Response {
    commit(state, service, plan, write, &Commit::typed(operation)).await
}

/// The dry-run answer: the plan as text and as data, and the revision a
/// confirmation of it must name.
pub(super) fn plan_body(plan: &ManagedPlan) -> Value {
    let summary = plan.summary();
    json!({
        "plan": plan.text(),
        "revision": plan.revision(),
        "risk": summary.risk,
        "risk_reasons": summary.risk_reasons,
        "changes": summary.changes,
        "unverified": plan.warnings(),
    })
}

const fn credential_status(error: &CredentialReferenceError) -> StatusCode {
    match error {
        CredentialReferenceError::Missing(_) => StatusCode::NOT_FOUND,
        CredentialReferenceError::OwnedByOther { .. } => StatusCode::CONFLICT,
        CredentialReferenceError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CredentialReferenceError::InvalidName
        | CredentialReferenceError::UnknownOwner
        | CredentialReferenceError::OwnerCarriesUserinfo
        | CredentialReferenceError::InvalidValue(_)
        | CredentialReferenceError::NotAReference(_) => StatusCode::BAD_REQUEST,
    }
}

/// A message that quotes nothing the request carried as a secret.
pub(super) fn verbatim(message: &str) -> String {
    message.to_string()
}

/// The one response of a refused management operation. `error` is the
/// service's sentence (what the CLI prints), passed through
/// `redact_provider_text` (URL userinfo, query values, fragments, secret
/// shapes; D-089 item 1) and through `secrets` (what the request carried);
/// `plan`, when given, is the refused plan's text.
pub(super) fn manage_error_response(
    error: ManageError,
    plan: Option<&str>,
    secrets: &(dyn Fn(&str) -> String + Sync),
) -> Response {
    let (status, body) = manage_error_parts(error, plan, secrets);
    (status, Json(body)).into_response()
}

/// The status and body of [`manage_error_response`].
fn manage_error_parts(
    error: ManageError,
    plan: Option<&str>,
    secrets: &(dyn Fn(&str) -> String + Sync),
) -> (StatusCode, Value) {
    let scrub = |message: &str| -> String { secrets(redact_provider_text(message).as_str()) };
    let message = scrub(&error.to_string());
    let (status, mut body) = match &error {
        ManageError::Invalid(_) => (StatusCode::BAD_REQUEST, json!({"error": message})),
        ManageError::NotFound(_) => (StatusCode::NOT_FOUND, json!({"error": message})),
        ManageError::AlreadyExists(_) => (StatusCode::CONFLICT, json!({"error": message})),
        ManageError::Referenced { object, referrers } => (
            StatusCode::CONFLICT,
            json!({"error": message, "object": scrub(object), "referrers": referrers.iter().map(|referrer| scrub(referrer)).collect::<Vec<_>>()}),
        ),
        ManageError::Credential(credential) => {
            let status = credential_status(credential);
            if status == StatusCode::INTERNAL_SERVER_ERROR {
                warn!("Credential reference operation failed: {message}");
            }
            (status, json!({"error": message}))
        }
        ManageError::Transaction(transaction) => match transaction {
            ConfigTransactionError::Invalid(_) => (StatusCode::BAD_REQUEST, json!({"error": message})),
            ConfigTransactionError::Conflict(conflict) => (
                StatusCode::CONFLICT,
                json!({
                    "error": "The configuration changed since it was read (another writer, a manual edit, or a \
                              reload that has not happened yet). Read it again and retry; nothing was written.",
                    "current_revision": conflict.actual,
                }),
            ),
            ConfigTransactionError::ConfirmationRequired {
                required,
                reasons,
                revision,
            } => (
                StatusCode::PRECONDITION_REQUIRED,
                json!({
                    "error": message,
                    "required": required,
                    "risk_reasons": reasons,
                    "revision": revision,
                }),
            ),
            ConfigTransactionError::NotApplied { record_id, outcome, .. } => {
                warn!("Management change not applied: {message}");
                let status = match outcome {
                    NotAppliedOutcome::Restored | NotAppliedOutcome::NothingWritten => StatusCode::UNPROCESSABLE_ENTITY,
                    NotAppliedOutcome::RestoredNotReactivated | NotAppliedOutcome::NotRestored => {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                };
                (
                    status,
                    json!({"error": message, "record_id": record_id, "restored": outcome.disk_restored()}),
                )
            }
            ConfigTransactionError::Failed(_) => {
                warn!("Failed to save a management change: {message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "Failed to save configuration"}),
                )
            }
        },
    };
    if let (Some(plan), Some(object)) = (plan, body.as_object_mut()) {
        object.insert("plan".to_string(), Value::String(plan.to_string()));
    }
    (status, body)
}

/// Which default or route table a request means; `chat` when omitted.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OperationParam {
    #[default]
    Chat,
    Embedding,
}

impl From<OperationParam> for SelectionOperation {
    fn from(operation: OperationParam) -> Self {
        match operation {
            OperationParam::Chat => Self::Chat,
            OperationParam::Embedding => Self::Embedding,
        }
    }
}

/// A change as `POST /api/config/plans` and `POST /api/config/apply` take
/// it: `{"kind": "add_route", ...}`, the same fields the object endpoints
/// take, or a raw file.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ChangeRequest {
    SetModelSettings {
        model: String,
        settings: crate::config::model_settings::ModelSettings,
    },
    AddProvider {
        provider: String,
    },
    UpdateProvider {
        provider: String,
        /// `null` clears the endpoint.
        api_url: Option<String>,
    },
    RemoveProvider {
        provider: String,
    },
    AddModel(ProviderModelConfig),
    UpdateModel(ProviderModelConfig),
    RemoveModel {
        provider: String,
        #[serde(default)]
        model: Option<String>,
    },
    SetDefault {
        #[serde(default)]
        settings: Option<crate::config::model_settings::ModelSettings>,
        #[serde(default)]
        operation: OperationParam,
        provider: String,
        model: String,
        #[serde(default)]
        root_credential: RootCredential,
    },
    AddRoute {
        #[serde(default)]
        operation: OperationParam,
        hint: String,
        provider: String,
        model: String,
    },
    UpdateRoute {
        #[serde(default)]
        operation: OperationParam,
        hint: String,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    RemoveRoute {
        #[serde(default)]
        operation: OperationParam,
        hint: String,
    },
    BindAgent {
        agent: String,
        provider: String,
        model: String,
    },
    /// `config.toml` or a `config.d` fragment, written as given (the raw
    /// editor's change).
    RawFile {
        file: String,
        content: String,
    },
}

/// A parsed change: typed, or a raw file with the scan that scrubs its
/// secrets from messages.
enum Change {
    Managed(ManagedChange),
    RawFile {
        file: String,
        content: String,
        secrets: crate::config::secret_paths::SecretScan,
        /// Removed keys the posted file names (dropped at load).
        warning: Option<String>,
    },
}

impl ChangeRequest {
    fn into_change(self) -> Result<Change, Box<Response>> {
        let managed = match self {
            Self::SetModelSettings { model, settings } => ManagedChange::SetModelSettings { model, settings },
            Self::AddProvider { provider } => ManagedChange::AddProvider { provider },
            Self::UpdateProvider { provider, api_url } => ManagedChange::SetProviderEndpoint { provider, api_url },
            Self::RemoveProvider { provider } => ManagedChange::RemoveProvider { provider },
            Self::AddModel(declaration) => ManagedChange::AddModel(declaration),
            Self::UpdateModel(declaration) => ManagedChange::UpdateModel(declaration),
            Self::RemoveModel { provider, model } => ManagedChange::RemoveModel { provider, model },
            Self::SetDefault {
                settings,
                operation,
                provider,
                model,
                root_credential,
            } => ManagedChange::SetDefault {
                settings,
                operation: operation.into(),
                provider,
                model,
                root_credential,
            },
            Self::AddRoute {
                operation,
                hint,
                provider,
                model,
            } => ManagedChange::AddRoute {
                operation: operation.into(),
                hint,
                provider,
                model,
            },
            Self::UpdateRoute {
                operation,
                hint,
                provider,
                model,
            } => ManagedChange::UpdateRoute {
                operation: operation.into(),
                hint,
                provider,
                model,
            },
            Self::RemoveRoute { operation, hint } => ManagedChange::RemoveRoute {
                operation: operation.into(),
                hint,
            },
            Self::BindAgent { agent, provider, model } => ManagedChange::BindAgent { agent, provider, model },
            Self::RawFile { file, content } => {
                // The parser's own message quotes the offending line, which
                // may hold a secret: only its position is reported.
                let parsed = toml::from_str::<toml::Table>(&content).map_err(|error| {
                    Box::new(
                        (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({"error": crate::config::manage::render::toml_error_location(&content, &error)}),
                            ),
                        )
                            .into_response(),
                    )
                })?;
                let parsed = toml::Value::Table(parsed);
                let secrets = crate::config::secret_paths::scan(&parsed);
                let warning = super::config::removed_keys_warning_of(&parsed);
                return Ok(Change::RawFile {
                    file,
                    content,
                    secrets,
                    warning,
                });
            }
        };
        Ok(Change::Managed(managed))
    }
}

/// The body of `POST /api/config/plans` and `POST /api/config/apply`.
#[derive(Deserialize)]
pub(super) struct ChangeBody {
    change: ChangeRequest,
}

/// `POST /api/config/plans`: validate a change and describe it; nothing is
/// written. The validation is the one a write runs first (staging, the
/// write boundary, every affected selection), so a dry run refuses what the
/// write would. The answer's `revision` is what a high-risk confirmation
/// names.
pub async fn post_plan(State(state): State<AppState>, Json(body): Json<ChangeBody>) -> Response {
    let service = service(&state);
    let change = match body.change.into_change() {
        Ok(change) => change,
        Err(refused) => return *refused,
    };
    match change {
        Change::Managed(change) => match service.plan(&change, ACTOR).await {
            Ok(plan) => Json(plan_body(&plan)).into_response(),
            Err(error) => manage_error_response(error, None, &verbatim),
        },
        Change::RawFile {
            file, content, secrets, ..
        } => {
            if let Err(refused) = raw_file_target(&service, &file) {
                return *refused;
            }
            match service.plan_file(file, content, false, ACTOR).await {
                Ok(plan) => Json(plan_body(&plan)).into_response(),
                Err(error) => manage_error_response(error, None, &|message: &str| secrets.scrub(message)),
            }
        }
    }
}

/// `POST /api/config/apply`: plan and commit a change. `If-Match` names the
/// revision the change is based on (`409` on another); a high-risk plan
/// needs `?confirm_high_risk=true` with `If-Match` naming the revision the
/// `428` reported.
pub async fn post_apply(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    Json(body): Json<ChangeBody>,
) -> Response {
    let write = match Write::of(&query, &headers) {
        Ok(write) => write,
        Err(refused) => return *refused,
    };
    let change = match body.change.into_change() {
        Ok(change) => change,
        Err(refused) => return *refused,
    };
    match change {
        Change::Managed(change) => apply_change(&state, change, write).await,
        Change::RawFile {
            file,
            content,
            secrets,
            warning,
        } => {
            const OPERATION: &str = "gateway_api:config:file_update";
            if let Err(refused) = authorize(&state, OPERATION, ResourceRiskLevel::Low) {
                return *refused;
            }
            let service = service(&state);
            if let Err(refused) = raw_file_target(&service, &file) {
                return *refused;
            }
            let scrub = |message: &str| secrets.scrub(message);
            match service.plan_file(file, content, false, ACTOR).await {
                Ok(plan) => {
                    let around = Commit {
                        operation: OPERATION,
                        scrub: &scrub,
                        warning,
                    };
                    commit(&state, &service, plan, &write, &around).await
                }
                Err(error) => manage_error_response(error, None, &scrub),
            }
        }
    }
}

/// The raw editor's file checks, shared with `PUT /api/config/files` (D-089
/// 9): the name rule, and a `config.d` that is a symlink or not a
/// directory is refused.
fn raw_file_target(service: &ManagementService, file: &str) -> Result<(), Box<Response>> {
    super::config::raw_file_target(&service.config().config_path, file)
        .map(|_| ())
        .map_err(|message| Box::new((StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()))
}

/// `GET /api/config/history`: every recorded transaction, newest first,
/// redacted, each with its plan text.
pub async fn get_history(State(state): State<AppState>) -> Response {
    let service = service(&state);
    let revision = service.revision();
    let read = crate::runtime::blocking::spawn_blocking(move || {
        service.transactions().history().and_then(|history| history.list())
    })
    .await
    .unwrap_or_else(|error| Err(anyhow::anyhow!("history reader failed: {error}")));
    let records = match read {
        Ok(records) => records,
        Err(error) => {
            warn!("Failed to read the configuration history: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to read the configuration history"})),
            )
                .into_response();
        }
    };
    let mut listed = Vec::with_capacity(records.len());
    for record in records {
        let plan = render_plan(&record.summary, &[]);
        let mut value = match serde_json::to_value(&record) {
            Ok(value) => value,
            Err(error) => {
                warn!("Failed to render a history record: {error}");
                continue;
            }
        };
        if let Some(object) = value.as_object_mut() {
            object.insert("plan".to_string(), Value::String(plan));
        }
        listed.push(value);
    }
    Json(json!({"revision": revision, "records": listed})).into_response()
}

/// The body of `POST /api/config/rollback`. Every flag is a confirmation,
/// and each counts only with `If-Match` naming the current revision.
#[derive(Deserialize)]
pub struct RollbackBody {
    record_id: String,
    /// Confirms the rollback itself.
    #[serde(default)]
    confirm: bool,
    /// Allows restoring a credential the current configuration no longer
    /// holds.
    #[serde(default)]
    restore_credentials: bool,
    /// Allows a rollback that turns `secrets.encrypt` off.
    #[serde(default)]
    disable_encryption: bool,
}

/// `POST /api/config/rollback`: restore the configuration from before a
/// history record, as a new validated transaction with its own record.
///
/// `plan` is what the restore writes: restoring to before a record also
/// undoes every later change (D-089 6); `undoes` is the recorded change
/// itself. The gate level is derived from the restore plan, like every
/// other write. Without confirmation it answers `428` with both and the
/// revision to name in `If-Match`.
pub async fn post_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RollbackBody>,
) -> Response {
    if let Err(refused) = authorize(&state, "gateway_api:config:rollback", ResourceRiskLevel::Low) {
        return *refused;
    }
    let if_match = match super::config::if_match_revision(&headers) {
        Ok(if_match) => if_match,
        Err(message) => return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response(),
    };
    let service = service(&state);
    if let Some(conflict) = stale(&service, if_match.as_deref()) {
        return conflict;
    }
    let record_id = body.record_id.trim().to_string();
    let lookup = {
        let service = service.clone();
        let record_id = record_id.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            service
                .transactions()
                .history()
                .and_then(|history| history.get(&record_id))
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("history reader failed: {error}")))
    };
    let Ok(record) = lookup else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no history record has this id; nothing was written"})),
        )
            .into_response();
    };
    let undoes = render_plan(&record.summary, &[]);
    let restore = match service.transactions().plan_rollback(&record.id).await {
        Ok(restore) => restore,
        Err(error) => return manage_error_response(ManageError::Transaction(error), None, &verbatim),
    };
    let plan = render_plan(restore.summary(), &[]);
    if let Err(refused) = authorize_plan(&state, "gateway_api:config:rollback", restore.summary()) {
        return *refused;
    }
    let Some(current) = service.revision() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "the configuration was not loaded from disk, so there is nothing to roll back"})),
        )
            .into_response();
    };
    let expected = if_match.clone().unwrap_or(current);
    let named = if_match.is_some();
    let asked = body.confirm || body.restore_credentials || body.disable_encryption;
    let write = Write {
        confirm_high_risk: asked,
        if_match,
    };
    let result = service
        .transactions()
        .rollback(RollbackRequest {
            record_id: &record.id,
            expected_revision: &expected,
            confirm: body.confirm && named,
            restore_credentials: body.restore_credentials && named,
            disable_encryption: body.disable_encryption && named,
        })
        .await;
    match result {
        Ok(outcome) => {
            let mut response = super::config::applied_body(&outcome);
            if let Some(object) = response.as_object_mut() {
                object.insert("restores".to_string(), Value::String(record.id));
                object.insert("plan".to_string(), Value::String(plan));
                object.insert("undoes".to_string(), Value::String(undoes));
            }
            Json(response).into_response()
        }
        Err(error) => {
            let (status, mut body) =
                manage_error_parts(write.explain(ManageError::Transaction(error)), Some(&plan), &verbatim);
            if let Some(object) = body.as_object_mut() {
                object.insert("undoes".to_string(), Value::String(undoes));
            }
            (status, Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{Served, at, json, serve_config};

    /// D-101 F-3: creating or changing a secret takes the High gate,
    /// clearing one Medium, and anything else keeps its own level.
    #[test]
    fn the_gate_is_high_for_a_secret_write_and_medium_for_a_clear() {
        use crate::config::Config;
        use crate::config::manage::ConfigChangeSummary;
        use crate::security::policy::ResourceRiskLevel;
        let plain = Config::default();
        let keyed = Config {
            api_key: Some("pmmh-gate-sentinel".into()),
            ..Config::default()
        };
        let rotated = Config {
            api_key: Some("pmmh-gate-rotated".into()),
            ..Config::default()
        };
        let level = |before: &Config, after: &Config| {
            super::gate_level(&ConfigChangeSummary::between(Some(before), after).expect("test: summary"))
        };
        assert_eq!(level(&plain, &keyed), ResourceRiskLevel::High);
        assert_eq!(level(&keyed, &rotated), ResourceRiskLevel::High);
        assert_eq!(level(&keyed, &plain), ResourceRiskLevel::Medium);
        let warmer = Config {
            default_temperature: 0.2,
            ..Config::default()
        };
        assert_eq!(level(&plain, &warmer), ResourceRiskLevel::Low);

        let mut referenced = plain.clone();
        referenced.model_routes.push(
            serde_json::from_value(serde_json::json!({
                "hint": "fast", "provider": "openai", "model": "gpt-4o-mini",
                "credential_ref": "cred-route"
            }))
            .expect("test: route"),
        );
        assert_eq!(level(&plain, &referenced), ResourceRiskLevel::High);
        let mut detached = referenced.clone();
        detached.model_routes.get_mut(0).expect("test: route").credential_ref = None;
        assert_eq!(level(&referenced, &detached), ResourceRiskLevel::Low);

        let mut decision = plain.clone();
        decision.causal_tree.backend.credential_ref =
            crate::causal_tree::config::CredentialRefSetting::Profile("cred-decision".into());
        assert_eq!(level(&plain, &decision), ResourceRiskLevel::High);
        decision.causal_tree.backend.credential_ref = crate::causal_tree::config::CredentialRefSetting::None;
        assert_eq!(level(&plain, &decision), ResourceRiskLevel::Low);

        let mut credentialed_proxy = plain;
        credentialed_proxy.proxy.http_proxy = Some("http://prx_user:example@proxy.example.com".into());
        let mut cleared_proxy = credentialed_proxy.clone();
        cleared_proxy.proxy.http_proxy = None;
        let cleared =
            ConfigChangeSummary::between(Some(&credentialed_proxy), &cleared_proxy).expect("test: proxy clear plan");
        assert!(cleared.secret_writes.is_empty(), "{:?}", cleared.secret_writes);
        assert_ne!(level(&credentialed_proxy, &cleared_proxy), ResourceRiskLevel::High);
    }
    use crate::config::manage::{
        ApplyOptions, ConfigActor, ConfigChange, ManagedChange, ManagementService, RootCredential, SelectionOperation,
    };
    use reqwest::Method;
    use serde_json::json;

    /// A credential-shaped sentinel. Not a real secret.
    const SENTINEL: &str = "pmmf-manage-sentinel-value";

    #[tokio::test]
    async fn supervised_raw_config_cannot_attach_a_stored_credential_reference() {
        let served = serve_config(|config| {
            config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
            // This case exercises the autonomy gate after raw-file validation:
            // plaintext is valid only when encryption is explicitly off.
            config.secrets.encrypt = false;
        })
        .await;
        crate::config::manage::credentials::CredentialReferences::for_config(&served.loaded())
            .expect("test: references")
            .set("openai", "route", SENTINEL)
            .expect("test: store reference");
        let before = served.disk();
        let mut edited: toml::Value = toml::from_str(&before).expect("test: parse config");
        let route: toml::Value = toml::from_str(
            "hint = \"fast\"\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\ncredential_ref = \"cred-route\"\n",
        )
        .expect("test: route");
        edited
            .as_table_mut()
            .expect("test: config table")
            .insert("model_routes".into(), toml::Value::Array(vec![route]));
        let content = toml::to_string(&edited).expect("test: config text");
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/config/files/config.toml",
                Some(json!({"content": content})),
            )
            .await;
        assert_eq!(status, 403, "{reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        assert_eq!(served.disk(), before, "a refused attachment wrote configuration");
        assert!(!reply.contains(SENTINEL), "{reply}");

        let mut inline: toml::Value = toml::from_str(&before).expect("test: parse config");
        let keyed_route: toml::Value = toml::from_str(&format!(
            "hint = \"fast\"\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\napi_key = \"{SENTINEL}\"\n"
        ))
        .expect("test: keyed route");
        inline
            .as_table_mut()
            .expect("test: config table")
            .insert("model_routes".into(), toml::Value::Array(vec![keyed_route]));
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/config/files/config.toml",
                Some(json!({"content": toml::to_string(&inline).expect("test: config text")})),
            )
            .await;
        assert_eq!(status, 403, "{reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");
        assert_eq!(served.disk(), before, "a refused inline key wrote configuration");
    }

    /// Identity-preserving changes to keyed lists stay Low at supervised.
    #[tokio::test]
    async fn supervised_route_index_shift_and_key_reorder_remain_low() {
        let served = serve_config(|config| {
            config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
            config.secrets.encrypt = false;
            config.model_routes = vec![
                serde_json::from_value(json!({"hint": "first", "provider": "openai", "model": "gpt-4o-mini"}))
                    .expect("test: first route"),
                serde_json::from_value(
                    json!({"hint": "keyed", "provider": "openai", "model": "gpt-4o-mini", "api_key": SENTINEL}),
                )
                .expect("test: keyed route"),
            ];
            config.reliability.api_keys = vec!["prx_reorder_one".into(), "prx_reorder_two".into()];
        })
        .await;
        let mut shifted: toml::Value = toml::from_str(&served.disk()).expect("test: parse config");
        shifted
            .get_mut("model_routes")
            .expect("test: model routes")
            .as_array_mut()
            .expect("test: route list")
            .remove(0);
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/config/files/config.toml",
                Some(json!({"content": toml::to_string(&shifted).expect("test: config text")})),
            )
            .await;
        assert_eq!(status, 200, "removing the unkeyed route should stay Low: {reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");
        assert_eq!(served.state.config.desired().config.model_routes.len(), 1);
        assert_eq!(
            served
                .state
                .config
                .desired()
                .config
                .model_routes
                .first()
                .expect("test: keyed route")
                .hint,
            "keyed"
        );
        assert_eq!(
            served
                .state
                .config
                .desired()
                .config
                .model_routes
                .first()
                .expect("test: keyed route")
                .api_key
                .as_deref(),
            Some(SENTINEL)
        );

        let mut reordered: toml::Value = toml::from_str(&served.disk()).expect("test: parse config");
        reordered
            .get_mut("reliability")
            .and_then(|reliability| reliability.get_mut("api_keys"))
            .expect("test: reliability keys")
            .as_array_mut()
            .expect("test: reliability keys")
            .reverse();
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/config/files/config.toml",
                Some(json!({"content": toml::to_string(&reordered).expect("test: config text")})),
            )
            .await;
        assert_eq!(status, 200, "reordering the same keys should stay Low: {reply}");
        assert!(
            !reply.contains("prx_reorder_one") && !reply.contains("prx_reorder_two"),
            "{reply}"
        );
        assert_eq!(
            served.state.config.desired().config.reliability.api_keys,
            vec!["prx_reorder_two", "prx_reorder_one"]
        );
        assert_eq!(
            served
                .state
                .config
                .desired()
                .config
                .model_routes
                .first()
                .expect("test: keyed route")
                .api_key
                .as_deref(),
            Some(SENTINEL)
        );
    }

    /// A CLI key rotation can be rolled back only through a High gate at the
    /// Gateway; restoring its before-image is still a credential write.
    #[tokio::test]
    async fn supervised_apply_and_rollback_cannot_restore_a_root_key() {
        const OLD_KEY: &str = "prx_old_gateway_key";
        const NEW_KEY: &str = "prx_new_gateway_key";
        let served = serve_config(|config| {
            config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
            config.secrets.encrypt = false;
            config.api_key = Some(OLD_KEY.into());
        })
        .await;
        let before = served.disk();
        assert!(before.contains(OLD_KEY), "test: root key must be stored");
        let rotated = before.replace(OLD_KEY, NEW_KEY);
        let cli = cli_service(&served);
        let plan = cli
            .transactions()
            .plan(ConfigChange::RawFile {
                allow_secret_clear: false,
                filename: "config.toml".into(),
                contents: rotated,
            })
            .await
            .expect("test: CLI plan");
        let revision = plan.planned_revision().expect("test: revision").to_string();
        let record = cli
            .transactions()
            .apply(
                plan,
                ConfigActor::Cli,
                ApplyOptions {
                    expected_revision: None,
                    confirm_high_risk: true,
                    confirmed_revision: Some(&revision),
                },
            )
            .await
            .expect("test: CLI rotates key")
            .record_id
            .expect("test: history record");
        let current = served.disk();
        assert!(current.contains(NEW_KEY) && !current.contains(OLD_KEY));

        for (path, body) in [
            (
                "/api/config/apply",
                json!({"change": {"kind": "raw_file", "file": "config.toml", "content": before}}),
            ),
            ("/api/config/rollback", json!({"record_id": record, "confirm": true})),
        ] {
            let (status, reply) = served.call(Method::POST, path, Some(body)).await;
            assert_eq!(status, 403, "{path}: {reply}");
            assert!(reply.contains("runtime approval grant"), "{path}: {reply}");
            assert!(!reply.contains(OLD_KEY) && !reply.contains(NEW_KEY), "{path}: {reply}");
            assert_eq!(served.disk(), current, "{path}: refused request wrote configuration");
        }
    }

    /// D-104 refuses an embedding-route edit that leaves duplicate hints
    /// before the autonomy gate, even when the edit changes a credential.
    #[tokio::test]
    async fn duplicate_embedding_hint_key_edit_is_refused_without_a_write() {
        let served = serve_config(|config| {
            config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
            config.secrets.encrypt = false;
            config.embedding_routes = vec![
                crate::config::EmbeddingRouteConfig {
                    hint: "semantic".into(),
                    provider: "openai".into(),
                    model: "text-embedding-3-small".into(),
                    dimensions: None,
                    api_key: Some(SENTINEL.into()),
                    credential_ref: None,
                },
                crate::config::EmbeddingRouteConfig {
                    hint: "semantic".into(),
                    provider: "openai".into(),
                    model: "text-embedding-3-large".into(),
                    dimensions: None,
                    api_key: Some("prx_second_embedding_key".into()),
                    credential_ref: None,
                },
            ];
        })
        .await;
        let before = served.disk();
        let mut edited: toml::Value = toml::from_str(&before).expect("test: parse config");
        *edited
            .get_mut("embedding_routes")
            .and_then(toml::Value::as_array_mut)
            .and_then(|routes| routes.first_mut())
            .and_then(|route| route.get_mut("api_key"))
            .expect("test: first embedding route key") = toml::Value::String("prx_rotated_embedding_key".into());
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/config/files/config.toml",
                Some(json!({"content": toml::to_string(&edited).expect("test: config text")})),
            )
            .await;
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("both use hint 'semantic'"), "{reply}");
        assert!(
            !reply.contains(SENTINEL)
                && !reply.contains("prx_second_embedding_key")
                && !reply.contains("prx_rotated_embedding_key"),
            "{reply}"
        );
        assert_eq!(served.disk(), before);
    }

    async fn with_agent() -> Served {
        serve_config(|config| {
            config.agents.insert(
                "coder".into(),
                serde_json::from_value(json!({"provider": "openai", "model": "gpt-4o"})).expect("test: agent"),
            );
        })
        .await
    }

    /// The object endpoint that makes the same change as a `change` body.
    fn object_endpoint(change: &serde_json::Value) -> (Method, String, serde_json::Value) {
        let mut body = change.clone();
        let kind = at(change, "/kind").as_str().map(str::to_string).unwrap_or_default();
        if let Some(object) = body.as_object_mut() {
            object.remove("kind");
        }
        match kind.as_str() {
            "add_route" => (Method::POST, "/api/routes".to_string(), body),
            "set_default" => (Method::PUT, "/api/defaults".to_string(), body),
            "add_provider" => (Method::POST, "/api/providers".to_string(), body),
            "bind_agent" => {
                let agent = at(change, "/agent").as_str().map(str::to_string).unwrap_or_default();
                if let Some(object) = body.as_object_mut() {
                    object.remove("agent");
                }
                (Method::PUT, format!("/api/agents/{agent}"), body)
            }
            "add_model" => (Method::POST, "/api/models".to_string(), body),
            other => panic!("test: no object endpoint for {other}"),
        }
    }

    /// The service the CLI calls, over the same configuration.
    fn cli_service(served: &Served) -> ManagementService {
        ManagementService::new(std::sync::Arc::clone(&served.state.config))
    }

    /// F-3 contract (with PMM-E E-2): the CLI prints `ManagedPlan::text()`
    /// and the service's refusal verbatim (pinned by
    /// `manage_cli::…the_cli_prints_the_services_plan_and_refusal_text_byte_for_byte`,
    /// over these same five changes and refusal). The Gateway's `plan` is
    /// byte-identical to that text on the dry run, on `/api/config/apply`
    /// and on the object endpoint (D-089 7), and its `error` on all three;
    /// nothing is written by a dry run or a refusal.
    #[tokio::test]
    async fn the_gateway_returns_the_clis_plan_and_refusal_text_byte_for_byte() {
        let served = with_agent().await;
        let before = served.disk();
        let cases: Vec<(serde_json::Value, ManagedChange)> = vec![
            (
                json!({"kind": "add_route", "hint": "fast", "provider": "openai", "model": "gpt-5.6"}),
                ManagedChange::AddRoute {
                    operation: SelectionOperation::Chat,
                    hint: "fast".into(),
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                },
            ),
            (
                json!({"kind": "set_default", "provider": "openai", "model": "gpt-5.6"}),
                ManagedChange::SetDefault {
                    settings: None,
                    operation: SelectionOperation::Chat,
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                    root_credential: RootCredential::Refuse,
                },
            ),
            (
                json!({"kind": "add_provider", "provider": "openrouter"}),
                ManagedChange::AddProvider {
                    provider: "openrouter".into(),
                },
            ),
            (
                json!({"kind": "bind_agent", "agent": "coder", "provider": "openai", "model": "gpt-5.6"}),
                ManagedChange::BindAgent {
                    agent: "coder".into(),
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                },
            ),
            (
                json!({"kind": "add_model", "provider": "openai", "model": "gpt-5.6", "operations": ["chat"], "native_tools": true}),
                ManagedChange::AddModel(crate::config::schema::ProviderModelConfig {
                    provider: "openai".into(),
                    model: Some("gpt-5.6".into()),
                    operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                    native_tools: Some(true),
                    vision_input: None,
                    streaming: None,
                    structured_output: None,
                    tool_schema_dialect: None,
                    tool_call_dialect: None,
                    context_window: None,
                }),
            ),
        ];
        for (request, change) in cases {
            let expected = cli_service(&served)
                .plan(&change, ConfigActor::Cli)
                .await
                .unwrap_or_else(|error| panic!("test: service plan {request}: {error}"))
                .text();
            let (status, reply) = served
                .call(Method::POST, "/api/config/plans", Some(json!({"change": request})))
                .await;
            assert_eq!(status, 200, "{request}: {reply}");
            assert_eq!(at(&json(&reply), "/plan"), expected.as_str(), "{request}");
            assert_eq!(served.disk(), before, "{request}: a dry run wrote");

            // Applied through `/api/config/apply` and through the object
            // endpoint, each on its own copy of the same configuration, the
            // committed plan is the same text.
            let applied = with_agent().await;
            let (status, reply) = applied
                .call(Method::POST, "/api/config/apply", Some(json!({"change": request})))
                .await;
            assert_eq!(status, 200, "{request}: {reply}");
            assert_eq!(at(&json(&reply), "/plan"), expected.as_str(), "apply {request}");
            let object = with_agent().await;
            let (method, path, body) = object_endpoint(&request);
            let (status, reply) = object.call(method, &path, Some(body)).await;
            assert_eq!(status, 200, "{path}: {reply}");
            assert_eq!(at(&json(&reply), "/plan"), expected.as_str(), "{path}");
        }

        // The same refusal, from the dry run, the object endpoint and apply.
        let expected = cli_service(&served)
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openai".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: refused")
            .to_string();
        let remove = json!({"change": {"kind": "remove_provider", "provider": "openai"}});
        for (method, path, body) in [
            (Method::POST, "/api/config/plans", Some(remove.clone())),
            (Method::POST, "/api/config/apply", Some(remove.clone())),
            (Method::DELETE, "/api/providers/openai", None),
        ] {
            let (status, reply) = served.call(method.clone(), path, body).await;
            assert_eq!(status, 409, "{method} {path}: {reply}");
            assert_eq!(at(&json(&reply), "/error"), expected.as_str(), "{method} {path}");
        }
        assert_eq!(served.disk(), before);
    }

    /// F-3: a change based on a revision that is no longer current answers
    /// 409 with the current revision, and nothing is written.
    #[tokio::test]
    async fn a_stale_revision_answers_409() {
        let served = with_agent().await;
        let (_, reply) = served.call(Method::GET, "/api/defaults", None).await;
        let old = at(&json(&reply), "/revision")
            .as_str()
            .expect("test: revision")
            .to_string();
        let (status, reply) = served
            .call_if_match(
                Method::POST,
                "/api/routes",
                &old,
                Some(json!({"hint": "fast", "provider": "openai", "model": "gpt-5.6"})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let written = served.disk();

        for (method, path, body) in [
            (
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "slow", "provider": "openai", "model": "gpt-5.6"})),
            ),
            (
                Method::POST,
                "/api/config/apply",
                Some(json!({"change": {"kind": "remove_route", "hint": "fast"}})),
            ),
            (
                Method::PUT,
                "/api/agents/coder",
                Some(json!({"provider": "openai", "model": "gpt-5.6"})),
            ),
        ] {
            let (status, reply) = served.call_if_match(method.clone(), path, &old, body).await;
            assert_eq!(status, 409, "{method} {path}: {reply}");
            assert!(at(&json(&reply), "/current_revision").is_string(), "{reply}");
            assert_eq!(served.disk(), written, "{method} {path}: nothing may be written");
        }
    }

    /// F-3: history lists the applied change with its plan; a rollback asks
    /// for confirmation bound to the revision, then restores the
    /// before-image byte for byte.
    #[tokio::test]
    async fn history_and_rollback_restore_the_before_image() {
        let served = with_agent().await;
        let before = served.disk();
        let (status, reply) = served
            .call(
                Method::POST,
                "/api/config/apply",
                Some(
                    json!({"change": {"kind": "add_route", "hint": "fast", "provider": "openai", "model": "gpt-5.6"}}),
                ),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        let record = at(&json(&reply), "/record_id")
            .as_str()
            .expect("test: record")
            .to_string();

        let (status, reply) = served.call(Method::GET, "/api/config/history", None).await;
        assert_eq!(status, 200, "{reply}");
        let history = json(&reply);
        assert_eq!(at(&history, "/records/0/id"), record.as_str(), "{reply}");
        assert_eq!(at(&history, "/records/0/actor"), "gateway_management", "{reply}");
        assert!(
            at(&history, "/records/0/plan")
                .as_str()
                .is_some_and(|plan| plan.contains("  + model_routes[0].hint = \"fast\"\n")),
            "{reply}"
        );
        let revision = at(&history, "/revision").as_str().expect("test: revision").to_string();

        let (status, reply) = served
            .call(Method::POST, "/api/config/rollback", Some(json!({"record_id": record})))
            .await;
        assert_eq!(status, 428, "{reply}");
        assert_eq!(at(&json(&reply), "/required"), "confirm", "{reply}");
        let (status, reply) = served
            .call(
                Method::POST,
                "/api/config/rollback",
                Some(json!({"record_id": record, "confirm": true})),
            )
            .await;
        assert_eq!(status, 428, "{reply}");
        assert_eq!(at(&json(&reply), "/required"), "if_match", "{reply}");
        assert_ne!(served.disk(), before);

        let (status, reply) = served
            .call_if_match(
                Method::POST,
                "/api/config/rollback",
                &revision,
                Some(json!({"record_id": record, "confirm": true})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(served.disk(), before, "the before-image is restored byte for byte");
    }

    /// A raw file dry run goes through the same transaction as the raw
    /// editor: plaintext at an encrypted slot is refused by path, and
    /// neither that refusal nor a parse error quotes the posted secret.
    #[tokio::test]
    async fn a_raw_file_plan_never_echoes_a_posted_secret() {
        let served = with_agent().await;
        let posted = served.disk().replacen(
            "default_model = \"gpt-4o\"",
            &format!("api_key = \"{SENTINEL}\"\ndefault_model = \"gpt-4o\""),
            1,
        );
        let (status, reply) = served
            .call(
                Method::POST,
                "/api/config/plans",
                Some(json!({"change": {"kind": "raw_file", "file": "config.toml", "content": posted}})),
            )
            .await;
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("api_key"), "the refusal names the path: {reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");

        let (status, reply) = served
            .call(
                Method::POST,
                "/api/config/apply",
                Some(json!({"change": {"kind": "raw_file", "file": "config.toml", "content": format!("api_key = \"{SENTINEL}\n")}})),
            )
            .await;
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("Invalid TOML content at line 1"), "{reply}");
        assert!(!reply.contains(SENTINEL), "{reply}");
    }

    /// D-089 2: `If-Match` is bound to the revision the plan was read from.
    /// A raw file planned against the disk revision applies with that
    /// revision while the running configuration has not reloaded a manual
    /// edit yet (before: compared with the desired revision, 409); a typed
    /// change named against the desired revision is refused with 409 by the
    /// commit guard, because the files moved.
    #[tokio::test]
    async fn an_if_match_is_bound_to_the_revision_the_plan_read() {
        let served = with_agent().await;
        let (_, reply) = served.call(Method::GET, "/api/defaults", None).await;
        let desired = at(&json(&reply), "/revision")
            .as_str()
            .expect("test: revision")
            .to_string();
        // A manual edit the watcher has not reloaded yet.
        let edited = served
            .disk()
            .replacen("default_temperature = 0.7", "default_temperature = 0.8", 1);
        assert_ne!(edited, served.disk(), "test precondition: {}", served.disk());
        std::fs::write(served.dir.path().join("config.toml"), &edited).expect("test: manual edit");

        let raw = json!({"change": {"kind": "raw_file", "file": "config.toml", "content": edited.replacen("default_temperature = 0.8", "default_temperature = 0.9", 1)}});
        let (status, reply) = served.call(Method::POST, "/api/config/plans", Some(raw.clone())).await;
        assert_eq!(status, 200, "{reply}");
        let planned = at(&json(&reply), "/revision")
            .as_str()
            .expect("test: revision")
            .to_string();
        assert_ne!(planned, desired, "the raw plan reads the files");
        let (status, reply) = served
            .call_if_match(Method::POST, "/api/config/apply", &planned, Some(raw))
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(served.disk().contains("default_temperature = 0.9"));

        let (status, reply) = served
            .call_if_match(
                Method::POST,
                "/api/routes",
                &desired,
                Some(json!({"hint": "fast", "provider": "openai", "model": "gpt-5.6"})),
            )
            .await;
        assert_eq!(status, 409, "{reply}");
    }

    /// D-089 3: the Gateway gate level is derived from what the plan
    /// changes, so adding a failover provider meets the same Medium gate on
    /// every endpoint: at `supervised` each answers 403 and writes nothing
    /// (before: `POST /api/config`, the raw editor, `raw_file` apply and
    /// rollback applied it at Low). A change outside the provider set stays
    /// Low on the same endpoints.
    #[tokio::test]
    async fn the_same_change_meets_the_same_gate_on_every_endpoint() {
        let served = serve_config(|config| {
            config.reliability.fallback_providers = vec!["openrouter".into()];
            config.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
        })
        .await;
        // The record a rollback would restore, made the way `prx` makes it
        // (the CLI has no Gateway gate): removing the failover provider.
        let service = cli_service(&served);
        let plan = service
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openrouter".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect("test: plan");
        let record = service
            .apply(plan, ConfigActor::Cli, None)
            .await
            .expect("test: apply")
            .record_id
            .expect("test: record");
        let before = served.disk();
        assert!(
            before.contains("fallback_providers = []"),
            "test precondition: {before}"
        );
        let edited = before.replacen("fallback_providers = []", "fallback_providers = [\"openrouter\"]", 1);
        let (_, reply) = served.call(Method::GET, "/api/defaults", None).await;
        let revision = at(&json(&reply), "/revision")
            .as_str()
            .expect("test: revision")
            .to_string();

        for (method, path, body) in [
            (
                Method::POST,
                "/api/providers".to_string(),
                json!({"provider": "openrouter"}),
            ),
            (
                Method::POST,
                "/api/config".to_string(),
                json!({"reliability": {"fallback_providers": ["openrouter"]}}),
            ),
            (
                Method::PUT,
                "/api/config/files/config.toml".to_string(),
                json!({"content": edited}),
            ),
            (
                Method::POST,
                "/api/config/apply".to_string(),
                json!({"change": {"kind": "raw_file", "file": "config.toml", "content": edited}}),
            ),
            (
                Method::POST,
                "/api/config/rollback".to_string(),
                json!({"record_id": record, "confirm": true}),
            ),
        ] {
            let (status, reply) = served.call_if_match(method.clone(), &path, &revision, Some(body)).await;
            assert_eq!(status, 403, "{method} {path}: {reply}");
            assert!(reply.contains("runtime approval grant"), "{method} {path}: {reply}");
            assert_eq!(served.disk(), before, "{method} {path}: nothing may be written");
        }

        let (status, reply) = served
            .call(Method::POST, "/api/config", Some(json!({"default_temperature": 0.5})))
            .await;
        assert_eq!(status, 200, "a Low change stays Low: {reply}");
        let (status, reply) = served
            .call(
                Method::POST,
                "/api/routes",
                Some(json!({"hint": "fast", "provider": "openai", "model": "gpt-5.6"})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
    }

    /// D-089 6: a rollback's `plan` is what the restore writes (every later
    /// change is undone too); `undoes` is the recorded change alone.
    #[tokio::test]
    async fn a_rollback_plan_shows_what_the_restore_writes() {
        let served = with_agent().await;
        let mut record = String::new();
        for hint in ["fast", "slow"] {
            let (status, reply) = served
                .call(
                    Method::POST,
                    "/api/routes",
                    Some(json!({"hint": hint, "provider": "openai", "model": "gpt-5.6"})),
                )
                .await;
            assert_eq!(status, 200, "{reply}");
            if hint == "fast" {
                record = at(&json(&reply), "/record_id")
                    .as_str()
                    .expect("test: record")
                    .to_string();
            }
        }
        let (status, reply) = served
            .call(Method::POST, "/api/config/rollback", Some(json!({"record_id": record})))
            .await;
        assert_eq!(status, 428, "{reply}");
        let refused = json(&reply);
        let plan = at(&refused, "/plan").as_str().map(str::to_string).unwrap_or_default();
        let undoes = at(&refused, "/undoes").as_str().map(str::to_string).unwrap_or_default();
        assert!(
            plan.contains("\"slow\""),
            "the restore also removes the later route: {plan}"
        );
        assert!(plan.contains("\"fast\""), "{plan}");
        assert!(undoes.contains("\"fast\"") && !undoes.contains("\"slow\""), "{undoes}");
    }

    /// `If-Match: *` confirms nothing: a confirmed high-risk change with it
    /// answers 428 `if_match`, and nothing is written.
    #[tokio::test]
    async fn an_if_match_star_confirms_nothing() {
        let served = with_agent().await;
        let before = served.disk();
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let response = client
            .patch(format!("{}/api/providers/openai?confirm_high_risk=true", served.base))
            .bearer_auth(super::super::test_support::TOKEN)
            .header("If-Match", "*")
            .json(&json!({"api_url": "https://proxy.example/v1"}))
            .send()
            .await
            .expect("test: request");
        assert_eq!(response.status().as_u16(), 428);
        let reply = response.text().await.expect("test: body");
        assert_eq!(at(&json(&reply), "/required"), "if_match", "{reply}");
        assert_eq!(served.disk(), before);
    }
    #[tokio::test]
    async fn gateway_model_settings_share_plan_validation_and_commit_with_cli() {
        let served = serve_config(|_| {}).await;
        let before = served.disk();
        let settings = json!({"level":"high","max_output_tokens":64000});
        let change = json!({"kind":"set_model_settings","model":"openai/gpt-5.6","settings":settings});
        let (status, reply) = served
            .call(Method::POST, "/api/config/plans", Some(json!({"change":change})))
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(served.disk(), before);
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/models/settings",
                Some(json!({"model":"openai/gpt-5.6","settings":settings})),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(
            served
                .loaded()
                .model_settings
                .get("openai/gpt-5.6")
                .unwrap()
                .level
                .as_deref(),
            Some("high")
        );
        let (status, reply) = served
            .call(Method::GET, "/api/models/settings?provider=openai&model=gpt-5.6", None)
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(at(&json(&reply), "/settings/level"), "high");
        assert!(at(&json(&reply), "/levels/low").is_object());
        let committed = served.disk();
        let (status, reply) = served
            .call(
                Method::PUT,
                "/api/models/settings",
                Some(json!({"model":"openai/gpt-5.6","settings":{"temperature":0.5}})),
            )
            .await;
        assert_eq!(status, 400, "{reply}");
        assert_eq!(served.disk(), committed);
    }
}

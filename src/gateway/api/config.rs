use super::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tracing::warn;

/// `POST /api/config/reload`: activate the configuration files as they are
/// now (an rf contract route, `agent_crud_v1`). A caller authenticated by a
/// paired bearer is authorised by it alone (A4): the reload tool runs under
/// its own default policy, never one built from Main's autonomy level. Any
/// other caller (pairing off) gets the pre-A4 gate: the tool runs under
/// Main's policy and a refusal answers the pre-A4 `500` body. Errors are
/// coded; a failed load is reported without quoting the files.
pub async fn post_config_reload(State(state): State<AppState>, headers: HeaderMap) -> Response {
    use crate::tools::Tool as _;
    let owner_authenticated = super::owner_authenticated(&state, &headers);
    let tool = if owner_authenticated {
        crate::tools::ConfigReloadTool::new(Arc::clone(&state.config))
    } else {
        let security = crate::runtime::bootstrap::build_security_policy(&state.config.load_full());
        crate::tools::ConfigReloadTool::with_security(Arc::clone(&state.config), security)
    };
    match tool.execute(serde_json::json!({})).await {
        Ok(result) if result.success => {
            let message = if retry_turn_runtime_standby(&state).await {
                report_turn_runtime_rebuilt(result.output)
            } else {
                result.output
            };
            (
                StatusCode::OK,
                Json(serde_json::json!({ "success": true, "message": message })),
            )
                .into_response()
        }
        Ok(result) if !owner_authenticated => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": result.error.unwrap_or_else(|| "Unknown error".into()),
            })),
        )
            .into_response(),
        Ok(_) | Err(_) => {
            warn!("config reload refused or failed");
            super::capabilities::coded_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_unavailable",
                "the configuration files could not be loaded and activated; the running configuration is unchanged",
            )
        }
    }
}

/// After a successful reload, give a turn runtime standing by for a provider
/// credential its retry (see `GatewayTurnRuntimeOwner::retry_standby`). The
/// reload itself already succeeded, so a failed retry is logged and leaves the
/// turn routes answering `503 provider_not_ready`.
/// `true` when the retry made the turn runtime ready.
async fn retry_turn_runtime_standby(state: &AppState) -> bool {
    let Some(owner) = state.turn_runtime.clone() else {
        return false;
    };
    match crate::runtime::blocking::spawn_blocking(move || owner.retry_standby()).await {
        Ok(Ok(rebuilt)) => rebuilt,
        Ok(Err(_)) | Err(_) => {
            warn!("gateway turn runtime could not be rebuilt after the reload");
            false
        }
    }
}

/// The reload report with the turn runtime the retry rebuilt: its name in
/// `rebuilt`, and `status` `applied` where the reload alone changed nothing.
/// A report that does not parse is returned as it was.
fn report_turn_runtime_rebuilt(output: String) -> String {
    let Ok(mut report) = serde_json::from_str::<Value>(&output) else {
        return output;
    };
    let Some(object) = report.as_object_mut() else {
        return output;
    };
    if let Some(rebuilt) = object.get_mut("rebuilt").and_then(Value::as_array_mut) {
        rebuilt.push(Value::from("gateway_turn_runtime"));
    }
    if object.get("status").and_then(Value::as_str) == Some("unchanged") {
        object.insert("status".to_owned(), Value::from("applied"));
    }
    serde_json::to_string_pretty(&report).unwrap_or(output)
}

/// Any method other than `POST` on `/api/config/reload`.
pub(super) async fn reload_method_not_allowed() -> Response {
    super::capabilities::method_not_allowed_allowing("POST")
}

use crate::config::credential_slots::{CredentialSlot, CredentialValue, SlotError, validate_credential};
use crate::config::manage::{
    ApplyOptions, ConfigActor, ConfigChange, ConfigTransactionError, ConfigTransactionOutcome, ConfigTransactions,
    NotAppliedOutcome, RiskReason,
};
use crate::config::secret_paths::{self, REDACTION_MASK};

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ConfigFileSource {
    Main,
    Fragment,
}

#[derive(Serialize)]
struct ConfigFilePayload {
    path: String,
    filename: String,
    /// The file as stored, or, when it holds secrets, its parsed form with
    /// every secret masked (comments and layout are then not preserved).
    content: String,
    source: ConfigFileSource,
    /// `content` is a masked rendering, not the stored bytes.
    redacted: bool,
    /// Whether `content` can be edited and written back. A redacted file is
    /// read-only here: its secrets are write-only.
    editable: bool,
}

#[derive(Deserialize)]
pub struct UpdateConfigFileRequest {
    content: String,
}

/// Query options of the structured and raw config write endpoints.
#[derive(Debug, Default, Deserialize)]
pub struct ConfigWriteQuery {
    /// Confirms a change the transaction layer classifies high risk (it
    /// writes a credential, turns `secrets.encrypt` off, or changes a
    /// provider endpoint). Without it such a change answers `428`.
    #[serde(default)]
    confirm_high_risk: bool,
    /// Explicitly permit a raw file replacement to omit stored secrets.
    /// Does not confirm risk or bypass optimistic concurrency.
    #[serde(default)]
    allow_secret_clear: bool,
}

/// POST /api/config — merge partial JSON into current config, save to disk.
///
/// `If-Match`, when present, names the revision the change is based on: a
/// different revision answers `409`. A high-risk change is applied only
/// with `?confirm_high_risk=true` and `If-Match` naming the revision the
/// `428` reported (D-049 B).
pub async fn post_config(
    State(state): State<AppState>,
    Query(query): Query<ConfigWriteQuery>,
    headers: HeaderMap,
    Json(incoming): Json<Value>,
) -> Response {
    if let Err(error) = super::authorize_resource_mutation(
        &state,
        "gateway_api:config:update",
        crate::security::policy::ResourceRiskLevel::Low,
    ) {
        return error.into_response();
    }
    if !incoming.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Request body must be a JSON object"})),
        )
            .into_response();
    }

    let if_match = match if_match_revision(&headers) {
        Ok(if_match) => if_match,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": message}))).into_response();
        }
    };
    // Merge onto the complete desired tree so deferred fields are never lost.
    let desired = state.config.desired();
    if let Some(conflict) = if_match_conflict(if_match.as_deref(), &desired) {
        return transaction_error_response(ConfigTransactionError::Conflict(conflict), "", &|message: &str| {
            message.to_string()
        });
    }
    let config = Arc::clone(&desired.config);
    let mut current_value = match serde_json::to_value(config.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to serialize current config: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error processing config"})),
            )
                .into_response();
        }
    };

    // Masks the stored configuration already holds (left by the pre-fix
    // round trip). They are never real credentials.
    let stored_masks = secret_paths::scan(&current_value).masked;
    merge_json(&mut current_value, &incoming, &mut Vec::new());

    // A mask that survived the merge either sits in an array the caller
    // changed (it cannot be matched to a stored secret) or was already
    // stored. Either way nothing is written: saving would store the mask
    // again and publish it as the active credential in the new generation.
    let (pre_existing, introduced): (Vec<String>, Vec<String>) = secret_paths::scan(&current_value)
        .masked
        .into_iter()
        .partition(|path| stored_masks.contains(path));
    let masked = introduced;
    if masked.is_empty() && !pre_existing.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "The stored configuration already holds the redaction mask `{REDACTION_MASK}` as the credential at: \
                 {}. It is not a real credential. Set it with PUT /api/config/secrets, or send that array with \
                 real values; nothing was written.",
                pre_existing.join(", ")
            )})),
        )
            .into_response();
    }
    if !masked.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "Secret value in array holds the redaction mask `{REDACTION_MASK}` at: {}. Send the array back \
                 unchanged to keep its stored secrets, or set the credential through PUT /api/config/secrets.",
                masked.join(", ")
            )})),
        )
            .into_response();
    }
    // Deserialization and validation messages can quote a value.
    let incoming_secrets = secret_paths::scan(&incoming);

    // 2. Deserialize merged JSON back into Config
    let mut merged_config: crate::config::Config = match serde_json::from_value(current_value) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": incoming_secrets.scrub(&format!("Invalid config after merge: {e}"))})),
            )
                .into_response();
        }
    };

    // 3. Preserve non-serialized fields
    merged_config.config_path = config.config_path.clone();
    merged_config.workspace_dir = config.workspace_dir.clone();

    // A model name this change sets is stored as the resolver reads it
    // (R-PMMCh C-2).
    crate::providers::selection::trim_changed_model_names(&config, &mut merged_config);

    // 4. Validate
    if let Err(e) = merged_config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": incoming_secrets.scrub(&format!("Config validation failed: {e}"))})),
        )
            .into_response();
    }

    // 5. Commit the complete effective tree as one transaction: it is
    // refused when the files changed since `desired` was read, and restored
    // when the runtime refuses it. Every selection the change affects (the
    // default, each model route, agent binding and embedding route) is an
    // operator selection (D-034): the transaction refuses negative evidence
    // before anything is written, and an unverified model is accepted with a
    // warning in the response.
    let transactions = ConfigTransactions::new(Arc::clone(&state.config));
    let scrub = |message: &str| incoming_secrets.scrub(message);
    let plan = match transactions
        .plan(ConfigChange::Structured(Box::new(merged_config)))
        .await
    {
        Ok(plan) => plan,
        Err(error) => return transaction_error_response(error, "Invalid effective configuration", &scrub),
    };
    // The autonomy gate level follows what the plan changes, the same on
    // every Gateway write path (D-089 3): a provider-set change is Medium.
    if let Err(refused) = super::manage::authorize_plan(&state, "gateway_api:config:update", plan.summary()) {
        return *refused;
    }
    let base_revision = ConfigTransactions::base_revision(&desired);
    let result = transactions
        .apply(
            plan,
            ConfigActor::GatewayConfigApi,
            ApplyOptions {
                expected_revision: base_revision.as_deref(),
                confirm_high_risk: query.confirm_high_risk,
                confirmed_revision: if_match.as_deref(),
            },
        )
        .await;
    match result {
        Ok(outcome) => {
            let mut body = applied_body(&outcome);
            // A removed key in the request is dropped like a file load drops
            // it; the response says so instead of staying silent (B2f).
            let warning = crate::config::deprecations::removed_keys_warning(&incoming)
                .into_iter()
                .chain(outcome.warnings.iter().cloned())
                .collect::<Vec<_>>();
            if !warning.is_empty()
                && let Some(object) = body.as_object_mut()
            {
                object.insert("warning".to_string(), Value::String(warning.join(" ")));
            }
            Json(body).into_response()
        }
        Err(error) => transaction_error_response(error, "Invalid effective configuration", &scrub),
    }
}

/// The success body shared by the structured and raw write endpoints.
pub(super) fn applied_body(outcome: &ConfigTransactionOutcome) -> Value {
    let report = &outcome.report;
    serde_json::json!({
        "status": report.status(),
        "active_generation": report.active_generation.0,
        "active_source_revision": report.active_source_revision.fingerprint_sha256,
        "desired_source_revision": report.desired_source_revision.fingerprint_sha256,
        "applied": report.applied,
        "rebuilt": report.rebuilt,
        "restarted": report.restarted,
        "restart_required": report.restart_required,
        "participant_acks": report.participant_acks,
        "record_id": outcome.record_id,
        "risk": outcome.summary.risk,
        // Selections the change made that the evidence does not verify.
        "unverified": outcome.warnings,
    })
}

/// Map a refused transaction to a response. Every message passes `scrub`,
/// because a staging or activation error can quote a value the request
/// carried.
fn transaction_error_response(
    error: ConfigTransactionError,
    invalid_prefix: &str,
    scrub: &dyn Fn(&str) -> String,
) -> Response {
    match error {
        ConfigTransactionError::Invalid(error)
            if error.is::<crate::providers::catalog::admission::ModelCapabilitiesUnknown>() =>
        {
            super::capabilities::coded_error_with(
                StatusCode::BAD_REQUEST,
                "model_capabilities_unknown",
                &scrub(&error.to_string()),
                None,
            )
        }
        ConfigTransactionError::Invalid(error) => {
            let message = scrub(&format!("{invalid_prefix}: {error:#}"));
            warn!("Refused configuration change: {message}");
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": message}))).into_response()
        }
        ConfigTransactionError::Conflict(conflict) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "The configuration changed since it was read (another writer, a manual edit, or a \
                          reload that has not happened yet). Read it again (POST /api/config/reload picks up a \
                          manual edit) and retry; nothing was written.",
                "current_revision": conflict.actual,
            })),
        )
            .into_response(),
        ConfigTransactionError::ConfirmationRequired {
            required,
            ref reasons,
            ref revision,
        } => {
            let message = scrub(&error.to_string());
            // `revision` is what the refused plan was computed against: a
            // confirmed retry sends it as `If-Match`, so a change in between
            // answers 409 instead of approving risks nobody saw.
            (
                StatusCode::PRECONDITION_REQUIRED,
                Json(serde_json::json!({
                    "error": message,
                    "required": required,
                    "risk_reasons": reasons,
                    "revision": revision,
                })),
            )
                .into_response()
        }
        ConfigTransactionError::NotApplied {
            ref record_id, outcome, ..
        } => {
            let message = scrub(&error.to_string());
            warn!("Configuration change not applied: {message}");
            let status = match outcome {
                NotAppliedOutcome::Restored | NotAppliedOutcome::NothingWritten => StatusCode::UNPROCESSABLE_ENTITY,
                NotAppliedOutcome::RestoredNotReactivated | NotAppliedOutcome::NotRestored => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            (
                status,
                Json(serde_json::json!({
                    "error": message,
                    "record_id": record_id,
                    "restored": outcome.disk_restored(),
                })),
            )
                .into_response()
        }
        ConfigTransactionError::Failed(error) => {
            warn!("Failed to save configuration: {}", scrub(&format!("{error:#}")));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to save configuration"})),
            )
                .into_response()
        }
    }
}

/// Deep-merge `source` into `target` at config path `path`.
///
/// An object member equal to the mask keeps the current value. An array or a
/// scalar is replaced wholesale, except when it equals the current value as
/// `GET /api/config` redacted it: the caller sent it back unchanged, so the
/// stored secrets inside it (or the userinfo of a provider URL) are kept. Any
/// other value that still carries a mask is refused by the caller after the
/// merge.
fn merge_json(target: &mut Value, source: &Value, path: &mut Vec<secret_paths::PathElem>) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, source_val) in source_map {
                // Skip redacted values — don't overwrite real secrets with "***"
                if source_val.as_str() == Some(REDACTION_MASK) {
                    continue;
                }
                // A key shown redacted (a `[cost].prices` key with URL
                // userinfo or query values) stands for the one stored key it
                // was redacted from. When two stored keys redact alike, the
                // posted key names neither: it is kept as posted, so the
                // write boundary refuses its mask instead of editing a guess
                // (D-095 N-3).
                let key = if secret_paths::is_url_userinfo_key_table(path) && !target_map.contains_key(key) {
                    let mut matches = target_map
                        .keys()
                        .filter(|stored| crate::security::redaction::redact_url_value(stored) == key.as_str());
                    match (matches.next(), matches.next()) {
                        (Some(stored), None) => stored.clone(),
                        _ => key.clone(),
                    }
                } else {
                    key.clone()
                };
                let entry = target_map.entry(key.clone()).or_insert(Value::Null);
                path.push(secret_paths::PathElem::Key(key.clone()));
                merge_json(entry, source_val, path);
                path.pop();
            }
        }
        // A value sent back exactly as `GET /api/config` showed it keeps the
        // stored value: an array of secrets, or a provider label whose URL
        // userinfo was redacted.
        (target, source) if equals_redacted(target, source, path) => {}
        (target, source) => {
            *target = source.clone();
        }
    }
}

/// Whether `source` is exactly what `GET /api/config` shows for `current`.
fn equals_redacted(current: &Value, source: &Value, path: &[secret_paths::PathElem]) -> bool {
    let mut redacted = current.clone();
    secret_paths::redact_subtree(&mut redacted, path);
    redacted == *source
}

/// PUT /api/config/secrets — set, replace or clear one credential slot.
///
/// Body: `{"slot": "model_routes.<hint>.api_key", "value": "<credential>"}`;
/// `reliability.api_keys` takes a list; `null` clears. The credential is
/// stored through the structured save (encrypted when `secrets.encrypt =
/// true`) and activated. No response or log line carries the value.
///
/// The autonomy gate is High (a secret write, like the credential
/// reference PUT): at `supervised` the request answers `403` (D-099 7a).
///
/// Rotating a credential at the named slot is confirmed by the request
/// itself. Every other high-risk reason the plan gives (an MCP server's
/// launch environment, an access grant, a re-pointed endpoint, a drifted
/// configuration) answers `428` with the revision; the retry confirms with
/// `?confirm_high_risk=true` and must name that revision in `If-Match`
/// (`*` or no `If-Match` answers `428` again; another revision `409`).
pub async fn put_config_secret(
    State(state): State<AppState>,
    Query(query): Query<ConfigWriteQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(error) = super::authorize_resource_mutation(
        &state,
        "gateway_api:config:secret_update",
        crate::security::policy::ResourceRiskLevel::Low,
    ) {
        return error.into_response();
    }

    let expected = match if_match_revision(&headers) {
        Ok(expected) => expected,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": message}))).into_response();
        }
    };
    let Some(slot) = body.get("slot").and_then(Value::as_str).and_then(CredentialSlot::parse) else {
        return slot_refusal(&SlotError::UnknownSlot);
    };
    let value = match credential_value(&slot, body.get("value").unwrap_or(&Value::Null)) {
        Ok(value) => value,
        Err(error) => return slot_refusal(&error),
    };
    let cleared =
        matches!(&value, CredentialValue::Clear) || matches!(&value, CredentialValue::List(keys) if keys.is_empty());
    let status = if cleared { "cleared" } else { "stored" };
    // No message is scrubbed by replacing the submitted value (D-099 7b,
    // D-089 5 parity): a short value would garble unrelated words. The slot
    // refusals never quote the value, the transaction scrubs its own
    // messages, and a validation message passes the shape-based sanitizer.

    let desired = state.config.desired();
    if let Some(conflict) = if_match_conflict(expected.as_deref(), &desired) {
        return transaction_error_response(ConfigTransactionError::Conflict(conflict), "", &|message: &str| {
            message.to_string()
        });
    }
    let mut config = desired.config.as_ref().clone();
    if let Err(error) = slot.apply(&mut config, value) {
        return slot_refusal(&error);
    }
    if let Err(error) = config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": secret_paths::scrub_message(None, &format!("Config validation failed: {error}"))
            })),
        )
            .into_response();
    }
    // What the slot change alone risks: the credential write at the slot's
    // own path(s). The request confirms exactly that and nothing else.
    let slot_reasons = match crate::config::manage::ConfigChangeSummary::between(Some(&desired.config), &config) {
        Ok(summary) => summary.risk_reasons,
        Err(error) => {
            warn!(slot = %slot, "Failed to describe the credential change: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to save configuration"})),
            )
                .into_response();
        }
    };
    let transactions = ConfigTransactions::new(Arc::clone(&state.config));
    let verbatim = |message: &str| message.to_string();
    let plan = match transactions.plan(ConfigChange::Structured(Box::new(config))).await {
        Ok(plan) => plan,
        Err(error) => return transaction_error_response(error, "Invalid effective configuration", &verbatim),
    };
    // The autonomy gate follows what the plan writes: a credential slot is a
    // secret write, High like the credential reference PUT (D-099 7a).
    if let Err(refused) = super::manage::authorize_plan(&state, "gateway_api:config:secret_update", plan.summary()) {
        return *refused;
    }
    let base_revision = ConfigTransactions::base_revision(&desired);
    // The request names exactly one credential slot: it confirms rotating
    // that credential only. A process environment or an access grant (their
    // own risk reasons), and any other high-risk difference between the
    // plan and disk (a desired configuration that drifted from the files),
    // stay unconfirmed and answer 428 unless the caller confirms them.
    let rotates_only_its_slot = plan
        .summary()
        .risk_reasons
        .iter()
        .all(|reason| matches!(reason, RiskReason::CredentialChanged { .. }) && slot_reasons.contains(reason));
    match transactions
        .apply(
            plan,
            ConfigActor::GatewayCredential,
            ApplyOptions {
                expected_revision: base_revision.as_deref(),
                confirm_high_risk: query.confirm_high_risk || rotates_only_its_slot,
                // A rotation at the named slot is confirmed by the request
                // itself, against the plan it was just computed from; any
                // other confirmation names the refused plan's revision in
                // `If-Match` (D-049 B).
                confirmed_revision: if rotates_only_its_slot {
                    base_revision.as_deref()
                } else {
                    expected.as_deref()
                },
            },
        )
        .await
    {
        Ok(outcome) => {
            tracing::info!(slot = %slot, status, "Credential slot updated through the gateway");
            Json(serde_json::json!({
                "slot": slot.to_string(),
                "status": status,
                "activation": outcome.report.status(),
                "active_generation": outcome.report.active_generation.0,
                "restart_required": outcome.report.restart_required,
                "record_id": outcome.record_id,
            }))
            .into_response()
        }
        Err(error) => transaction_error_response(error, "Invalid effective configuration", &verbatim),
    }
}

fn slot_refusal(error: &SlotError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": error.to_string()})),
    )
        .into_response()
}

/// The request's `value` in the shape `slot` takes: a string, a list of
/// strings for a list slot, or `null` to clear.
fn credential_value(slot: &CredentialSlot, value: &Value) -> Result<CredentialValue, SlotError> {
    match value {
        Value::Null => Ok(CredentialValue::Clear),
        Value::String(raw) if !slot.is_list() => validate_credential(raw).map(CredentialValue::One),
        Value::Array(items) if slot.is_list() => items
            .iter()
            .map(|item| item.as_str().ok_or(SlotError::WrongShape).and_then(validate_credential))
            .collect::<Result<Vec<_>, _>>()
            .map(CredentialValue::List),
        Value::String(_) | Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
            Err(SlotError::WrongShape)
        }
    }
}

pub async fn get_config(State(state): State<AppState>) -> Response {
    let config = state.config.desired();
    let mut value = match serde_json::to_value(config.config.as_ref()) {
        Ok(value) => value,
        Err(error) => {
            warn!("Failed to serialize config: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error processing config"})),
            )
                .into_response();
        }
    };

    secret_paths::redact(&mut value);
    Json(value).into_response()
}

pub async fn get_config_files(State(state): State<AppState>) -> Response {
    let config_path = state.config.desired().config.config_path.clone();
    // The revision is read before the files: a change in between makes a
    // later `If-Match` write conflict, never overwrite unseen content.
    let read = crate::runtime::blocking::spawn_blocking(move || {
        let revision = crate::config::files::current_config_revision(&config_path)?;
        collect_config_files(&config_path).map(|files| (revision, files))
    })
    .await
    .unwrap_or_else(|error| Err(anyhow::anyhow!("config file reader failed: {error}")));
    match read {
        Ok((revision, files)) => ([(header::ETAG, format!("\"{revision}\""))], Json(files)).into_response(),
        Err(error) => {
            warn!("Failed to read config files: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to read configuration files"})),
            )
                .into_response()
        }
    }
}

pub async fn put_config_file(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    Query(query): Query<ConfigWriteQuery>,
    headers: HeaderMap,
    Json(payload): Json<UpdateConfigFileRequest>,
) -> Response {
    // D2: read the path fields from the hot SharedConfig (D), not the cached C Mutex.
    // This route does not use `current` as a write-back base (the persisted content is
    // the request payload, and the published config is re-loaded fresh from disk into
    // `refreshed` below), so there is no stale-C overwrite risk here. We still source
    // `config_path` / `workspace_dir` from D so every write path uniformly reads the
    // authoritative snapshot (these fields are restart-only, so C and D agree on them).
    let desired = state.config.desired();
    let current = Arc::clone(&desired.config);
    if let Err(error) = super::authorize_resource_mutation(
        &state,
        "gateway_api:config:file_update",
        crate::security::policy::ResourceRiskLevel::Low,
    ) {
        return error.into_response();
    }

    if let Err(error) = raw_file_target(&current.config_path, &filename) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": error}))).into_response();
    }

    // Parse the whole document (a `toml::Value` parse accepts one value
    // only). The parser's own message renders the offending source line,
    // which may hold a secret, so only its position is reported.
    let parsed = match toml::from_str::<toml::Table>(&payload.content) {
        Ok(table) => toml::Value::Table(table),
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": crate::config::manage::render::toml_error_location(&payload.content, &error)})),
            )
                .into_response();
        }
    };

    // Secrets are write-only on this API: the shared plan step refuses an
    // introduced mask and, with encryption on, introduced plaintext at an
    // encrypted slot. The scan only scrubs messages here.
    let secrets = secret_paths::scan(&parsed);

    // Optimistic concurrency for the raw editor: `If-Match` carries the
    // revision (`ETag` of `GET /api/config/files`) the edit was based on.
    let expected = match if_match_revision(&headers) {
        Ok(expected) => expected,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": message}))).into_response();
        }
    };
    let transactions = ConfigTransactions::new(Arc::clone(&state.config));
    // Validation messages can quote a value; never return or log a secret
    // the request carried.
    let scrub = |message: &str| secrets.scrub(message);
    let plan = match transactions
        .plan(ConfigChange::RawFile {
            allow_secret_clear: query.allow_secret_clear,
            filename,
            contents: payload.content,
        })
        .await
    {
        Ok(plan) => plan,
        Err(error) => return transaction_error_response(error, "Invalid effective configuration", &scrub),
    };
    // The autonomy gate level follows what the plan changes (D-089 3).
    if let Err(refused) = super::manage::authorize_plan(&state, "gateway_api:config:file_update", plan.summary()) {
        return *refused;
    }
    // Without `If-Match` the plan's own revision guards the commit.
    match transactions
        .apply(
            plan,
            ConfigActor::GatewayConfigFile,
            ApplyOptions {
                expected_revision: expected.as_deref(),
                confirm_high_risk: query.confirm_high_risk,
                confirmed_revision: expected.as_deref(),
            },
        )
        .await
    {
        Ok(outcome) => {
            let mut body = applied_body(&outcome);
            // A removed key in the file is dropped at load (with a log line
            // naming it); the response says so as well, like the structured
            // write paths do (B2f).
            if let (Some(warning), Some(object)) = (removed_keys_warning_of(&parsed), body.as_object_mut()) {
                object.insert("warning".to_string(), Value::String(warning));
            }
            Json(body).into_response()
        }
        Err(error) => transaction_error_response(error, "Invalid effective configuration", &scrub),
    }
}

/// The revision an `If-Match` header names, if any: the fingerprint with
/// optional quotes removed. `*` means "any" (the plan's own revision still
/// guards the commit). A weak validator (`W/"…"`) is refused: `If-Match`
/// uses strong comparison (RFC 9110).
pub(super) fn if_match_revision(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| "If-Match must be a configuration revision")?
        .trim();
    if value == "*" {
        return Ok(None);
    }
    if value.starts_with("W/") {
        return Err("If-Match must be a strong validator (the ETag of GET /api/config/files)");
    }
    let revision = value.trim_matches('"');
    if revision.is_empty() || !revision.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("If-Match must be a configuration revision");
    }
    Ok(Some(revision.to_ascii_lowercase()))
}

/// A `409` conflict when the caller's `If-Match` names another revision than
/// the desired configuration a structured change is merged onto. Checked
/// before the merge, so a change is never built on a configuration the
/// caller did not name.
fn if_match_conflict(
    if_match: Option<&str>,
    desired: &crate::config::DesiredConfigState,
) -> Option<crate::config::files::ConfigRevisionConflict> {
    let if_match = if_match?;
    let base = ConfigTransactions::base_revision(desired).unwrap_or_default();
    (if_match != base).then(|| crate::config::files::ConfigRevisionConflict {
        expected: if_match.to_string(),
        actual: base,
    })
}

pub async fn get_config_schema() -> Response {
    Json(schema_for!(crate::config::Config)).into_response()
}

fn collect_config_files(config_path: &std::path::Path) -> anyhow::Result<Vec<ConfigFilePayload>> {
    let mut files = vec![read_config_file(config_path.to_path_buf(), ConfigFileSource::Main)?];
    for path in crate::config::files::list_config_fragment_paths(config_path)? {
        files.push(read_config_file(path, ConfigFileSource::Fragment)?);
    }
    Ok(files)
}

fn read_config_file(path: PathBuf, source: ConfigFileSource) -> anyhow::Result<ConfigFilePayload> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid config filename: {}", path.display()))?
        .to_string();
    let stored = std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("Failed to read {}: {error}", path.display()))?;
    let relative_path = match source {
        ConfigFileSource::Main => filename.clone(),
        ConfigFileSource::Fragment => format!("config.d/{filename}"),
    };
    let (content, redacted) = redact_file_content(&relative_path, stored);

    Ok(ConfigFilePayload {
        path: relative_path,
        filename,
        content,
        source,
        redacted,
        editable: !redacted,
    })
}

/// The stored bytes when the file holds no secret, live or commented out;
/// otherwise a rendering of its parsed form with every secret masked (and no
/// comments). A file that does not parse cannot be shown to be secret-free,
/// so its content is withheld.
fn redact_file_content(relative_path: &str, stored: String) -> (String, bool) {
    let mut tree = match toml::from_str::<toml::Table>(&stored) {
        Ok(table) => toml::Value::Table(table),
        Err(error) => {
            warn!(
                "Withholding {relative_path} from the raw config API: it does not parse ({})",
                crate::config::manage::render::toml_error_location(&stored, &error)
            );
            return (String::new(), true);
        }
    };
    // A commented-out credential is text the parser drops; the masked
    // rendering drops comments, so it is withheld too.
    if !secret_paths::scan(&tree).has_secret_values() && !secret_paths::has_commented_secret(&stored) {
        return (stored, false);
    }
    secret_paths::redact(&mut tree);
    match toml::to_string_pretty(&tree) {
        Ok(rendered) => (rendered, true),
        Err(error) => {
            warn!("Withholding {relative_path} from the raw config API: masked rendering failed: {error}");
            (String::new(), true)
        }
    }
}

/// The file a raw write replaces, with the checks every raw write makes
/// (`PUT /api/config/files/{name}` and a `raw_file` change on
/// `/api/config/apply`): the name rule, and a `config.d` that is a symlink
/// or not a directory is refused.
pub(super) fn raw_file_target(config_path: &std::path::Path, filename: &str) -> Result<PathBuf, String> {
    let target_path = resolve_config_file_path(config_path, filename)?;
    if target_path != config_path
        && let Some(parent) = target_path.parent()
        && let Ok(metadata) = std::fs::symlink_metadata(parent)
    {
        if metadata.file_type().is_symlink() {
            return Err("config.d path must not be a symlink".to_string());
        }
        if !metadata.is_dir() {
            return Err("config.d path is not a directory".to_string());
        }
    }
    Ok(target_path)
}

/// The warning for removed keys a posted raw file names (they are dropped
/// at load, with a log line naming them), shared by every raw write.
pub(super) fn removed_keys_warning_of(parsed: &toml::Value) -> Option<String> {
    serde_json::to_value(parsed)
        .ok()
        .and_then(|document| crate::config::deprecations::removed_keys_warning(&document))
}

fn resolve_config_file_path(config_path: &std::path::Path, filename: &str) -> Result<PathBuf, String> {
    if filename == "config.toml" {
        return Ok(config_path.to_path_buf());
    }
    // Only plain lowercase ASCII names: a case-insensitive filesystem folds
    // other spellings (`ſecurity.toml`, `Security.toml`) onto managed files.
    // The validator is the single name rule (D-045 I-3): it admits only a
    // non-empty `.toml` name of lowercase ASCII letters, digits, `.`, `_` and
    // `-` without `..`, so no separator or traversal can follow.
    crate::config::files::validate_raw_config_file_name(filename).map_err(|error| error.to_string())?;

    Ok(crate::config::files::config_dir_path(config_path).join(filename))
}

#[cfg(test)]
mod tests;

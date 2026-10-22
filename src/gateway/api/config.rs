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

/// POST /api/config/reload — hot-reload configuration from config.toml (authenticated).
pub async fn post_config_reload(State(state): State<AppState>) -> Response {
    use crate::tools::Tool as _;
    // The reload's own authorization gate reads the sole active generation.
    // Otherwise a prior reload
    // that tightened autonomy / `security.audit` would not constrain the next
    // reload's authz. Build via the shared `build_security_policy` helper so this
    // site cannot drift from the audit-wiring baseline (BUG-D1-01 class).
    let config_snapshot = state.config.load_full();
    let security = crate::runtime::bootstrap::build_security_policy(&config_snapshot);
    let tool = crate::tools::ConfigReloadTool::with_security(Arc::clone(&state.config), security);
    match tool.execute(serde_json::json!({})).await {
        Ok(result) if result.success => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "message": result.output,
            })),
        )
            .into_response(),
        Ok(result) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": result.error.unwrap_or_else(|| "Unknown error".into()),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "success": false,
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
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
mod tests {
    use super::AppState;
    use crate::config::Config;
    use crate::config::secret_paths::{self, fixture};
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::SqliteMemory;
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::security::pairing::PairingGuard;
    use crate::security::policy::AutonomyLevel;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    /// The bearer token the served router accepts. Not a real credential.
    const TOKEN: &str = "zc_config_api_test_token";

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

    fn app_state(config: Config) -> AppState {
        let workspace = config.workspace_dir.clone();
        std::fs::create_dir_all(&workspace).expect("test: workspace dir");
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
            rate_limiter: Arc::new(GatewayRateLimiter::new(10_000, 10_000, 10_000, 10_000)),
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

    /// Serve the real API router, with the auth and rate-limit layers the
    /// daemon mounts, on an ephemeral loopback port.
    async fn serve(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .nest("/api", super::super::router(state.clone()))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind ephemeral port");
        let port = listener.local_addr().expect("test: local addr").port();
        let handle = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::warn!("test: config api server stopped: {error}");
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// Send one request and return `(status, body)`.
    async fn call(
        method: reqwest::Method,
        url: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("test: http client");
        let mut request = client.request(method, url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("test: request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("test: response body");
        (status, text)
    }

    /// Guard (A-1): `GET /api/config` and `prx config show` decide what a
    /// secret is through the same registry, so they mask the same paths and
    /// neither leaks a sentinel. Through the real router and auth layer.
    #[tokio::test]
    async fn get_config_masks_the_same_paths_as_config_show() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        let show = secret_paths::redacted_stored_toml(&config).expect("test: config show rendering");
        let (base, server) = serve(app_state(config)).await;

        let (status, _) = call(reqwest::Method::GET, &format!("{base}/api/config"), None, None).await;
        assert_eq!(status, 401, "the config API must stay behind the auth layer");

        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200, "unexpected body: {body}");
        assert!(
            !body.contains(fixture::SENTINEL_PREFIX),
            "GET /api/config leaked a sentinel: {body}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).expect("test: json body");
        assert_eq!(fixture::masked_paths(&json), fixture::masked_paths(&show));
        server.abort();
    }

    #[test]
    fn config_api_resource_gate_blocks_readonly_mutations() {
        let config = crate::config::Config {
            autonomy: crate::config::AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..crate::config::AutonomyConfig::default()
            },
            ..crate::config::Config::default()
        };

        let error = super::super::authorize_resource_mutation_for_config(
            &config,
            "gateway_api:config:update",
            crate::security::policy::ResourceRiskLevel::Low,
        )
        .unwrap_err();
        assert!(
            error
                .1
                .0
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("read-only mode")
        );
    }

    #[test]
    fn config_api_resource_gate_allows_supervised_mutations() {
        let config = crate::config::Config::default();
        assert!(
            super::super::authorize_resource_mutation_for_config(
                &config,
                "gateway_api:config:update",
                crate::security::policy::ResourceRiskLevel::Low,
            )
            .is_ok()
        );
    }

    // ── Raw config file API (A-3) ─────────────────────────────────────────

    /// A credential-shaped sentinel. Not a real secret.
    const PLAIN: &str = "plain-sentinel-credential";

    /// A config saved to a temp dir through the real save path, loaded through
    /// the real load path, and served with the real router and auth layer.
    async fn on_disk(
        mutate: impl FnOnce(&mut Config),
    ) -> (tempfile::TempDir, AppState, String, tokio::task::JoinHandle<()>) {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = Config::default();
        config.config_path = dir.path().join("config.toml");
        config.workspace_dir = dir.path().join("workspace");
        mutate(&mut config);
        config.save().await.expect("test: save initial config");
        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load initial config");
        let state = app_state(loaded);
        let (base, server) = serve(state.clone()).await;
        (dir, state, base, server)
    }

    fn disk(state: &AppState) -> String {
        std::fs::read_to_string(&state.config.desired().config.config_path).expect("test: read config.toml")
    }

    async fn put_file(base: &str, name: &str, content: &str) -> (u16, String) {
        call(
            reqwest::Method::PUT,
            &format!("{base}/api/config/files/{name}"),
            Some(TOKEN),
            Some(serde_json::json!({ "content": content })),
        )
        .await
    }

    fn route(provider: &str, api_key: Option<&str>) -> crate::config::ModelRouteConfig {
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".into(),
            provider: provider.into(),
            model: "route-model".into(),
            api_key: api_key.map(str::to_string),
            credential_ref: None,
        }
    }

    /// RED before A-3: 200, and the plaintext key was committed to disk and
    /// activated. After: 400 naming the path only; disk, active generation
    /// and response free of the sentinel.
    #[tokio::test]
    async fn put_config_file_refuses_a_plaintext_provider_key() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let before_generation = state.config.active_generation_id();

        let content = format!("api_key = \"{PLAIN}\"\n{before_disk}");
        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the response echoed the secret: {body}");
        assert!(body.contains("api_key"), "the refusal must name the path: {body}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        assert!(!disk(&state).contains(PLAIN));
        assert_eq!(state.config.active_generation_id(), before_generation);
        assert_eq!(state.config.desired().config.api_key, None);
        server.abort();
    }

    /// RED before A-3 (confirms P1-A A4): the TOML parse error reflected the
    /// offending line, secret included. After: line and column only.
    #[tokio::test]
    async fn put_config_file_parse_error_does_not_echo_the_offending_line() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        let content = format!("default_temperature = 0.7\napi_key = \"{PLAIN}\n");
        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the parse error echoed the secret: {body}");
        assert!(body.contains("line 2"), "the error must locate the problem: {body}");
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    /// RED before A-3: a posted-back mask was stored as the credential.
    #[tokio::test]
    async fn put_config_file_refuses_a_redaction_mask() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        let content = format!("api_key = \"***\"\n{before_disk}");
        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(body.contains("api_key"), "the refusal must name the path: {body}");
        assert_eq!(disk(&state), before_disk, "a mask must never be stored");
        assert_eq!(state.config.desired().config.api_key, None);
        server.abort();
    }

    /// A value in the wrong shape at a secret path fails validation; the
    /// error must not carry the value either.
    #[tokio::test]
    async fn put_config_file_validation_error_does_not_echo_a_secret() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        let content = format!("{before_disk}\n[reliability]\napi_keys = \"{PLAIN}\"\n");
        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the validation error echoed the secret: {body}");
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    /// RED before A-3: the raw file API returned plaintext keys verbatim.
    /// After: masked, and the file is flagged redacted and not editable.
    #[tokio::test]
    async fn get_config_files_masks_secrets_and_flags_the_file() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.secrets.encrypt = false;
            config.model_routes = vec![route("openrouter", Some(PLAIN))];
        })
        .await;
        assert!(
            disk(&state).contains(PLAIN),
            "test precondition: a plaintext key on disk"
        );

        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/config/files"),
            Some(TOKEN),
            None,
        )
        .await;

        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the raw file API leaked the key: {body}");
        let files: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        let main = files
            .as_array()
            .and_then(|files| {
                files
                    .iter()
                    .find(|file| file.get("filename") == Some(&serde_json::json!("config.toml")))
            })
            .expect("test: config.toml listed");
        assert_eq!(main.get("redacted"), Some(&serde_json::json!(true)));
        assert_eq!(main.get("editable"), Some(&serde_json::json!(false)));
        let content = main
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            content.contains("api_key = \"***\""),
            "the key must show as masked: {content}"
        );
        server.abort();
    }

    /// A file without secrets is served verbatim and stays editable.
    #[tokio::test]
    async fn get_config_files_serves_a_secret_free_file_verbatim() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/config/files"),
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let files: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        let main = files.get(0).expect("test: config.toml listed");
        assert_eq!(main.get("redacted"), Some(&serde_json::json!(false)));
        assert_eq!(main.get("editable"), Some(&serde_json::json!(true)));
        assert_eq!(main.get("content"), Some(&serde_json::json!(disk(&state))));
        server.abort();
    }

    /// Guard: a file that carries its stored `enc2:` values back unchanged
    /// still stages, commits and reloads a non-secret edit.
    #[tokio::test]
    async fn put_config_file_accepts_an_encrypted_round_trip_with_a_non_secret_edit() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", Some(PLAIN))];
        })
        .await;
        let stored = disk(&state);
        assert!(
            !stored.contains(PLAIN) && stored.contains("enc2:"),
            "test precondition: encrypted route key"
        );
        assert!(
            stored.contains("default_temperature = 0.7"),
            "test precondition: {stored}"
        );

        let edited = stored.replace("default_temperature = 0.7", "default_temperature = 0.9");
        let (status, body) = put_file(&base, "config.toml", &edited).await;

        assert_eq!(status, 200, "unexpected response: {body}");
        assert_eq!(disk(&state), edited);
        let desired = state.config.desired();
        assert!((desired.config.default_temperature - 0.9).abs() < f64::EPSILON);
        assert_eq!(
            desired.config.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(PLAIN)
        );
        server.abort();
    }

    /// With `secrets.encrypt = false` plaintext is the stored form, so a raw
    /// plaintext key is accepted (the operator opted out of encryption).
    #[tokio::test]
    async fn put_config_file_accepts_plaintext_when_encryption_is_disabled() {
        let (_dir, state, base, server) = on_disk(|config| config.secrets.encrypt = false).await;
        let content = format!("api_key = \"{PLAIN}\"\n{}", disk(&state));
        let (status, body) = put_file_confirmed(&base, "config.toml", &content).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN));
        assert_eq!(state.config.desired().config.api_key.as_deref(), Some(PLAIN));
        server.abort();
    }

    // ── Structured config API: masks never persist (A-4) ──────────────────

    async fn get_config_json(base: &str) -> serde_json::Value {
        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "GET /api/config leaked the key: {body}");
        serde_json::from_str(&body).expect("test: json")
    }

    /// A high-risk write (credential, encryption off, endpoint) confirmed
    /// with `?confirm_high_risk=true`.
    /// The revision `GET /api/config/files` reports in its `ETag`.
    async fn files_revision(base: &str) -> String {
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let response = client
            .get(format!("{base}/api/config/files"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("test: request");
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_matches('"').to_string())
            .expect("test: ETag of GET /api/config/files")
    }

    /// A request with an optional `If-Match`.
    async fn call_with_if_match(
        method: reqwest::Method,
        url: &str,
        if_match: Option<&str>,
        body: serde_json::Value,
    ) -> (u16, String) {
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let mut request = client.request(method, url).bearer_auth(TOKEN).json(&body);
        if let Some(revision) = if_match {
            request = request.header("If-Match", revision);
        }
        let response = request.send().await.expect("test: request");
        let status = response.status().as_u16();
        (status, response.text().await.expect("test: body"))
    }

    /// A confirmed `POST /api/config` bound to the current revision
    /// (`If-Match`), as a confirmed retry of a `428` is (D-049 B).
    async fn post_config_confirmed(base: &str, body: serde_json::Value) -> (u16, String) {
        let revision = files_revision(base).await;
        call_with_if_match(
            reqwest::Method::POST,
            &format!("{base}/api/config?confirm_high_risk=true"),
            Some(&format!("\"{revision}\"")),
            body,
        )
        .await
    }

    /// A confirmed raw write bound to the current revision (`If-Match`).
    async fn put_file_confirmed(base: &str, name: &str, content: &str) -> (u16, String) {
        let revision = files_revision(base).await;
        call_with_if_match(
            reqwest::Method::PUT,
            &format!("{base}/api/config/files/{name}?confirm_high_risk=true"),
            Some(&format!("\"{revision}\"")),
            serde_json::json!({ "content": content }),
        )
        .await
    }

    async fn post_config(base: &str, body: serde_json::Value) -> (u16, String) {
        call(
            reqwest::Method::POST,
            &format!("{base}/api/config"),
            Some(TOKEN),
            Some(body),
        )
        .await
    }

    fn stored_route_key(state: &AppState) -> Option<String> {
        let config = state.config.desired().config.clone();
        let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: reload");
        reloaded.model_routes.first().and_then(|route| route.api_key.clone())
    }

    /// RED before A-4: posting back what `GET /api/config` returned replaced
    /// the `model_routes` array wholesale and stored `***` as the route key,
    /// which the router then sent as the bearer credential. After: an array
    /// posted back exactly as it was redacted keeps the stored secret.
    /// D-034: a default changed through the configuration API is an operator
    /// selection: refused on negative evidence with nothing written, accepted
    /// (unverified) on no evidence.
    #[tokio::test]
    async fn post_config_default_change_refuses_only_negative_evidence() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.default_provider = Some("openrouter".to_string());
            config.default_model = Some("vendor/chat-model".to_string());
            config.providers.models.push(
                toml::from_str(
                    "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
                )
                .expect("test: declaration"),
            );
        })
        .await;
        let before = disk(&state);
        let (status, body) = post_config(&base, serde_json::json!({"default_model": "jev-decision-test"})).await;
        assert_eq!(status, 400, "a decision-only default was accepted: {body}");
        assert!(body.contains("cannot be used for chat"), "{body}");
        assert_eq!(disk(&state), before, "nothing was written");

        let (status, body) = post_config(&base, serde_json::json!({"default_model": "vendor/never-seen"})).await;
        assert_eq!(status, 200, "an unverified default is an operator's choice: {body}");
        assert!(
            body.contains("unverified"),
            "the response says it is unverified: {body}"
        );
        server.abort();
    }

    /// RED before PMME-f (D-085 F-3): switching the default provider from
    /// one catalog provider to another while the root `api_key` is set hands
    /// the key to the new vendor. It was applied at once (low risk); it now
    /// needs the high-risk confirmation, and nothing is written before it.
    #[tokio::test]
    async fn post_config_switching_the_default_away_from_the_root_key_needs_confirmation() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.default_provider = Some("openai".to_string());
            config.default_model = Some("gpt-4o".to_string());
            config.api_key = Some("sentinel-root-owner-key".to_string());
        })
        .await;
        let before = disk(&state);
        let (status, body) = post_config(
            &base,
            serde_json::json!({"default_provider": "anthropic", "default_model": "claude-sonnet-4-5"}),
        )
        .await;
        assert_eq!(status, 428, "the root key changed hands unconfirmed: {body}");
        assert!(body.contains("credential_owner_changed"), "{body}");
        assert!(!body.contains("sentinel-root-owner-key"), "{body}");
        assert_eq!(disk(&state), before, "nothing was written");
        server.abort();
    }

    /// D-087 (5): the raw editor gets the same PMME-f classification as
    /// `POST /api/config`: a `config.toml` that switches the default
    /// provider away from the root key is high risk (`428`,
    /// `credential_owner_changed`), nothing is written before the
    /// confirmation, and the confirmed write applies.
    #[tokio::test]
    async fn put_config_file_switching_the_default_away_from_the_root_key_needs_confirmation() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.default_provider = Some("openai".to_string());
            config.default_model = Some("gpt-4o".to_string());
            config.api_key = Some(PLAIN.to_string());
        })
        .await;
        let stored = disk(&state);
        assert!(
            stored.contains("default_provider = \"openai\"") && stored.contains("default_model = \"gpt-4o\""),
            "test precondition: {stored}"
        );
        let edited = stored
            .replace("default_provider = \"openai\"", "default_provider = \"anthropic\"")
            .replace("default_model = \"gpt-4o\"", "default_model = \"claude-sonnet-4-5\"");

        let (status, body) = put_file(&base, "config.toml", &edited).await;
        assert_eq!(status, 428, "the root key changed hands unconfirmed: {body}");
        assert!(body.contains("credential_owner_changed"), "{body}");
        assert!(!body.contains(PLAIN), "{body}");
        assert_eq!(disk(&state), stored, "nothing was written");

        let (status, body) = put_file_confirmed(&base, "config.toml", &edited).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert_eq!(
            state.config.desired().config.default_provider.as_deref(),
            Some("anthropic")
        );
        server.abort();
    }

    /// RED before PMME-h (D-095 N-1): a `POST /api/config` validation
    /// refusal quoted the provider with its URL query values.
    #[tokio::test]
    async fn post_config_refusal_redacts_provider_url_query_values() {
        const PROVIDER: &str = "custom:http://127.0.0.1:1/v1?tenant=sentinel-refusal-tenant";
        let (_dir, state, base, server) = on_disk(|config| {
            config.providers.models.push(
                toml::from_str(&format!(
                    "provider = \"{PROVIDER}\"\nmodel = \"embed-only\"\noperations = [\"embedding\"]\n"
                ))
                .expect("test: declaration"),
            );
        })
        .await;
        let before = disk(&state);
        let (status, body) = post_config(
            &base,
            serde_json::json!({"default_provider": PROVIDER, "default_model": "embed-only"}),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(!body.contains("sentinel-refusal-tenant"), "{body}");
        assert_eq!(disk(&state), before);
        server.abort();
    }

    /// RED before PMME-h (D-095 N-4): a `working_dir` that does not exist was
    /// accepted (as a high-risk launch change) and failed only at launch. The
    /// plan now refuses it; a relative one fails validation.
    #[tokio::test]
    async fn post_config_refuses_a_missing_or_relative_working_dir() {
        let (dir, state, base, server) = on_disk(|_| {}).await;
        let before = disk(&state);
        let missing = dir.path().join("no-such-dir").display().to_string();
        for working_dir in [missing.as_str(), "relative/dir"] {
            let (status, body) = post_config(
                &base,
                serde_json::json!({"mcp": {"servers": {"pinned": {"command": "npx", "working_dir": working_dir}}}}),
            )
            .await;
            assert_eq!(status, 400, "{working_dir}: {body}");
            assert!(body.contains("working_dir"), "{body}");
        }
        assert_eq!(disk(&state), before);
        server.abort();
    }

    /// D-074 P1-1: `POST /api/config` rebuilds the configuration from JSON;
    /// the canonical causal settings survive the round trip, and
    /// the stored file keeps `schema_version`.
    #[tokio::test]
    async fn post_config_keeps_the_causal_settings_and_the_version() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.causal_tree.policy.max_candidates = 5;
            config.causal_tree.scoring.weights.w_cost = Some(0.4);
        })
        .await;
        let (status, body) = post_config(&base, serde_json::json!({"default_temperature": 0.5})).await;
        assert_eq!(status, 200, "{body}");
        let causal = state.config.desired().config.causal_tree.clone();
        assert_eq!(causal.scoring.weights.w_cost, Some(0.4));
        let config = state.config.desired().config.clone();
        let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: reload");
        assert_eq!(reloaded.causal_tree, causal);
        let mut stored = disk(&state);
        if let Ok(entries) = std::fs::read_dir(config.config_path.with_file_name("config.d")) {
            for entry in entries.flatten() {
                stored.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        assert!(
            stored.contains(&format!(
                "schema_version = {}",
                crate::causal_tree::config::CAUSAL_CONFIG_SCHEMA_VERSION
            )),
            "{stored}"
        );
        server.abort();
    }

    /// R-PMMCh C-2: a default model posted with padding is stored trimmed.
    #[tokio::test]
    async fn post_config_stores_the_default_model_trimmed() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.default_provider = Some("openrouter".to_string());
            config.default_model = Some("vendor/chat-model".to_string());
        })
        .await;
        let (status, body) = post_config(&base, serde_json::json!({"default_model": "  vendor/other \t"})).await;
        assert_eq!(status, 200, "{body}");
        let config = state.config.desired().config.clone();
        let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: reload");
        assert_eq!(reloaded.default_model.as_deref(), Some("vendor/other"));
        server.abort();
    }

    #[tokio::test]
    async fn post_config_round_trip_never_persists_a_mask() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", Some(PLAIN))];
            config.reliability.api_keys = vec![PLAIN.to_string()];
        })
        .await;

        let current = get_config_json(&base).await;
        let (status, body) = post_config(&base, current).await;

        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN));
        assert_eq!(
            stored_route_key(&state).as_deref(),
            Some(PLAIN),
            "the stored key must survive"
        );
        assert_eq!(
            state.config.desired().config.reliability.api_keys,
            vec![PLAIN.to_string()]
        );
        assert!(
            !disk(&state).contains(REDACTION_MASK_TEXT),
            "a mask must never reach disk"
        );
        server.abort();
    }

    const REDACTION_MASK_TEXT: &str = "\"***\"";

    /// RED before A-4: a changed array that still carries a mask stored the
    /// mask. After: 400 naming the path; nothing written.
    #[tokio::test]
    async fn post_config_refuses_a_mask_inside_a_changed_array() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", Some(PLAIN))];
        })
        .await;
        let before_disk = disk(&state);

        let mut current = get_config_json(&base).await;
        let model = current
            .pointer_mut("/model_routes/0/model")
            .expect("test: route model present");
        *model = serde_json::json!("another-model");
        let (status, body) = post_config(&base, current).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(
            body.contains("model_routes[0].api_key"),
            "the refusal must name the path: {body}"
        );
        assert_eq!(disk(&state), before_disk, "nothing may be written");

        let (status, body) = post_config(
            &base,
            serde_json::json!({"model_routes": [{"hint": "new", "provider": "openrouter", "model": "m", "api_key": "***"}]}),
        )
        .await;
        assert_eq!(status, 400, "a new route with a mask must be refused: {body}");
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    /// Guard: a real key posted through the structured API is stored
    /// encrypted (A-2) and never echoed.
    #[tokio::test]
    async fn post_config_stores_a_new_route_key_encrypted() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let (status, body) = post_config_confirmed(
            &base,
            serde_json::json!({"model_routes": [{"hint": "fast", "provider": "openrouter", "model": "m", "api_key": PLAIN}]}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN));
        assert!(!disk(&state).contains(PLAIN), "the key must be stored encrypted");
        assert_eq!(stored_route_key(&state).as_deref(), Some(PLAIN));
        server.abort();
    }

    /// A malformed secret value must not come back in the error: serde's
    /// "invalid type" message quotes the string it rejected.
    #[tokio::test]
    async fn post_config_error_does_not_echo_a_secret() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let (status, body) = post_config(&base, serde_json::json!({"reliability": {"api_keys": PLAIN}})).await;
        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the error echoed the secret: {body}");
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    // ── Write-only credential endpoint (A-5) ──────────────────────────────

    const PLAIN_2: &str = "plain-sentinel-credential-two";

    async fn put_secret(base: &str, body: serde_json::Value) -> (u16, String) {
        call(
            reqwest::Method::PUT,
            &format!("{base}/api/config/secrets"),
            Some(TOKEN),
            Some(body),
        )
        .await
    }

    /// A log sink for the current thread. `#[tokio::test]` runs the served
    /// router on this same thread, so the handler's events land here.
    #[derive(Clone, Default)]
    struct LogSink(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The endpoint stores the credential encrypted through the structured
    /// save, activates it, and neither the response, the logs, nor any later
    /// read surface carries it. A list slot takes a list; `null` clears.
    #[tokio::test]
    async fn put_secret_stores_a_route_key_encrypted_and_never_echoes_it() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        // Other tests reach the same callsites on other threads; recompute
        // their cached interest now that this thread has a subscriber.
        tracing::callsite::rebuild_interest_cache();

        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", None)];
        })
        .await;

        let (status, body) = put_secret(
            &base,
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": PLAIN}),
        )
        .await;

        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the response echoed the secret: {body}");
        let reply: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        assert_eq!(reply.get("slot"), Some(&serde_json::json!("model_routes.fast.api_key")));
        assert_eq!(reply.get("status"), Some(&serde_json::json!("stored")));

        let stored = disk(&state);
        assert!(!stored.contains(PLAIN), "the key must be stored encrypted");
        assert!(stored.contains("enc2:"));
        assert_eq!(stored_route_key(&state).as_deref(), Some(PLAIN));
        assert_eq!(
            state
                .config
                .desired()
                .config
                .model_routes
                .first()
                .and_then(|r| r.api_key.as_deref()),
            Some(PLAIN),
            "the credential must be active"
        );

        for surface in ["/api/config", "/api/config/files"] {
            let (status, body) = call(reqwest::Method::GET, &format!("{base}{surface}"), Some(TOKEN), None).await;
            assert_eq!(status, 200);
            assert!(!body.contains(PLAIN), "{surface} leaked the stored credential");
        }

        // A list slot, then clearing a slot.
        let (status, body) = put_secret(
            &base,
            serde_json::json!({"slot": "reliability.api_keys", "value": [PLAIN, PLAIN_2]}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(PLAIN_2));
        assert!(!disk(&state).contains(PLAIN_2));
        assert_eq!(
            state.config.desired().config.reliability.api_keys,
            vec![PLAIN.to_string(), PLAIN_2.to_string()]
        );

        let (status, body) = put_secret(
            &base,
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": null}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let reply: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        assert_eq!(reply.get("status"), Some(&serde_json::json!("cleared")));
        assert_eq!(stored_route_key(&state), None);

        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("model_routes.fast.api_key"),
            "the update must be logged by slot: {logs}"
        );
        assert!(
            logs.contains("reliability.api_keys"),
            "the update must be logged by slot: {logs}"
        );
        assert!(!logs.contains(PLAIN), "the logs carry the credential: {logs}");
        assert!(!logs.contains(PLAIN_2), "the logs carry the credential: {logs}");
        server.abort();
    }

    /// Unknown slot, missing target, a mask, ciphertext, a wrong shape: 400,
    /// nothing written, and the value never echoed.
    #[tokio::test]
    async fn put_secret_refuses_bad_slots_and_values_without_writing() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", None)];
        })
        .await;
        let before_disk = disk(&state);

        for body in [
            serde_json::json!({"slot": format!("channels_config.{PLAIN}"), "value": PLAIN}),
            serde_json::json!({"slot": "gateway.paired_tokens", "value": PLAIN}),
            serde_json::json!({"slot": "model_routes.missing.api_key", "value": PLAIN}),
            serde_json::json!({"slot": "agents.missing.api_key", "value": PLAIN}),
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": "***"}),
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": "enc2:00ff"}),
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": "   "}),
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": format!("{PLAIN}\nX-Injected: 1")}),
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": [PLAIN]}),
            serde_json::json!({"slot": "reliability.api_keys", "value": PLAIN}),
        ] {
            let (status, reply) = put_secret(&base, body.clone()).await;
            assert_eq!(status, 400, "{body} must be refused: {reply}");
            assert!(!reply.contains(PLAIN), "the refusal echoed the value: {reply}");
            assert_eq!(disk(&state), before_disk, "{body} must not write");
        }
        server.abort();
    }

    /// Read-only autonomy blocks the endpoint through the same resource gate
    /// as every other config write.
    #[tokio::test]
    async fn put_secret_is_blocked_in_read_only_mode() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.autonomy.level = AutonomyLevel::ReadOnly;
        })
        .await;
        let before_disk = disk(&state);
        let (status, body) = put_secret(&base, serde_json::json!({"slot": "api_key", "value": PLAIN})).await;
        assert_eq!(status, 403, "unexpected response: {body}");
        assert!(!body.contains(PLAIN));
        assert_eq!(disk(&state), before_disk);

        let (status, _) = call(
            reqwest::Method::PUT,
            &format!("{base}/api/config/secrets"),
            None,
            Some(serde_json::json!({"slot": "api_key", "value": PLAIN})),
        )
        .await;
        assert_eq!(status, 401, "the endpoint must stay behind the auth layer");
        server.abort();
    }

    // ── PMMA-f ─────────────────────────────────────────────────────────────

    /// Put `line` into the stored `[storage.provider.config]` table.
    fn with_storage_line(stored: &str, line: &str) -> String {
        let header = "[storage.provider.config]\n";
        assert!(stored.contains(header), "test precondition: storage table present");
        stored.replacen(header, &format!("{header}{line}\n"), 1)
    }

    /// F1, RED before PMMA-f: `db_url` under a serde alias was not a secret
    /// path, so the raw file API served it in cleartext and flagged the file
    /// editable.
    #[tokio::test]
    async fn get_config_files_masks_db_url_under_every_alias() {
        for alias in ["dbURL", "database_url", "databaseUrl"] {
            let (_dir, state, base, server) = on_disk(|_| {}).await;
            let path = state.config.desired().config.config_path.clone();
            let content = with_storage_line(&disk(&state), &format!("{alias} = \"postgres://u:{PLAIN}@h/db\""));
            std::fs::write(&path, content).expect("test: write alias");

            let (status, body) = call(
                reqwest::Method::GET,
                &format!("{base}/api/config/files"),
                Some(TOKEN),
                None,
            )
            .await;
            assert_eq!(status, 200, "unexpected response: {body}");
            assert!(!body.contains(PLAIN), "{alias} leaked through the raw file API: {body}");
            let files: serde_json::Value = serde_json::from_str(&body).expect("test: json");
            let main = files.get(0).expect("test: config.toml listed");
            assert_eq!(main.get("redacted"), Some(&serde_json::json!(true)), "{alias}");
            server.abort();
        }
    }

    /// F1, RED before PMMA-f: with encryption on, a plaintext `db_url` under
    /// an alias was accepted by the raw PUT and stored in cleartext.
    #[tokio::test]
    async fn put_config_file_refuses_a_plaintext_db_url_under_an_alias() {
        for alias in ["dbURL", "database_url", "databaseUrl"] {
            let (_dir, state, base, server) = on_disk(|_| {}).await;
            let before_disk = disk(&state);
            let content = with_storage_line(&before_disk, &format!("{alias} = \"postgres://u:{PLAIN}@h/db\""));
            let (status, body) = put_file(&base, "config.toml", &content).await;
            assert_eq!(status, 400, "{alias}: unexpected response: {body}");
            assert!(!body.contains(PLAIN), "{alias}: {body}");
            assert!(body.contains(alias), "the refusal must name the path: {body}");
            assert_eq!(disk(&state), before_disk);
            server.abort();
        }
    }

    /// A config whose stored route key and paired token are the literal mask,
    /// as the pre-A-4 `GET`→`POST` round trip left them.
    async fn with_stored_masks() -> (tempfile::TempDir, AppState, String, tokio::task::JoinHandle<()>) {
        on_disk(|config| {
            config.model_routes = vec![route("openrouter", Some("***"))];
            config.gateway.paired_tokens = vec!["***".to_string()];
        })
        .await
    }

    /// F2, RED before PMMA-f: an unrelated change was refused because of a
    /// mask stored earlier, with a message blaming the request's array.
    /// After: still refused (nothing stored, nothing activated), but the error
    /// names the stored slots and the way to repair them.
    #[tokio::test]
    async fn post_config_names_a_pre_existing_stored_mask_and_writes_nothing() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = with_stored_masks().await;
        let before_disk = disk(&state);
        let before_generation = state.config.active_generation_id();

        let (status, body) = post_config(&base, serde_json::json!({"default_temperature": 0.9})).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(
            body.contains("already holds"),
            "the error must say the mask is stored: {body}"
        );
        assert!(body.contains("model_routes[0].api_key"), "{body}");
        assert!(body.contains("gateway.paired_tokens[0]"), "{body}");
        assert!(body.contains("PUT /api/config/secrets"), "{body}");
        assert!(
            !body.contains("Secret value in array"),
            "must not blame the request: {body}"
        );
        assert_eq!(disk(&state), before_disk);
        assert_eq!(state.config.active_generation_id(), before_generation);
        server.abort();
    }

    /// F2 repair path: the credential endpoint fixes the provider slot, a
    /// full-array POST fixes the paired tokens, and then an unrelated change
    /// is accepted. No mask is left on disk or active.
    #[tokio::test]
    async fn stored_masks_are_repaired_through_the_credential_endpoint_and_a_full_array() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = with_stored_masks().await;

        let (status, body) = put_secret(
            &base,
            serde_json::json!({"slot": "model_routes.fast.api_key", "value": PLAIN}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let (status, body) =
            post_config_confirmed(&base, serde_json::json!({"gateway": {"paired_tokens": [PLAIN_2]}})).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let (status, body) = post_config(&base, serde_json::json!({"default_temperature": 0.9})).await;
        assert_eq!(status, 200, "unexpected response: {body}");

        let desired = state.config.desired();
        assert_eq!(
            desired.config.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(PLAIN)
        );
        assert_eq!(desired.config.gateway.paired_tokens, vec![PLAIN_2.to_string()]);
        let tree: toml::Value = toml::from_str(&disk(&state)).expect("test: stored config parses");
        assert!(
            secret_paths::scan(&tree).masked.is_empty(),
            "no mask may remain on disk"
        );
        server.abort();
    }

    /// F4, RED before PMMA-f: a secret-free file was served byte for byte,
    /// comments included, so a commented-out credential came back in clear.
    #[tokio::test]
    async fn get_config_files_does_not_serve_a_commented_out_credential() {
        for comment in [
            format!("# api_key = \"{PLAIN}\""),
            format!("#[[model_routes]] api_key = \"{PLAIN}\""),
            format!("default_temperature = 0.7 # old brave_api_key = \"{PLAIN}\""),
        ] {
            let (_dir, state, base, server) = on_disk(|_| {}).await;
            let path = state.config.desired().config.config_path.clone();
            std::fs::write(
                &path,
                format!("{comment}\n{}", disk(&state).replace("default_temperature = 0.7\n", "")),
            )
            .expect("test: write commented credential");

            let (status, body) = call(
                reqwest::Method::GET,
                &format!("{base}/api/config/files"),
                Some(TOKEN),
                None,
            )
            .await;
            assert_eq!(status, 200, "unexpected response: {body}");
            assert!(
                !body.contains(PLAIN),
                "a commented credential leaked ({comment}): {body}"
            );
            let files: serde_json::Value = serde_json::from_str(&body).expect("test: json");
            assert_eq!(files.pointer("/0/redacted"), Some(&serde_json::json!(true)));
            server.abort();
        }
    }

    /// Ordinary comments keep a secret-free file verbatim and editable.
    #[tokio::test]
    async fn get_config_files_keeps_ordinary_comments_verbatim() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let path = state.config.desired().config.config_path.clone();
        let content = format!("# tuned for the lab box; see api_key docs\n{}", disk(&state));
        std::fs::write(&path, &content).expect("test: write comment");
        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/config/files"),
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let files: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        assert_eq!(files.pointer("/0/redacted"), Some(&serde_json::json!(false)));
        assert_eq!(files.pointer("/0/content"), Some(&serde_json::json!(content)));
        server.abort();
    }

    /// The documented downgrade path: turning encryption off through the
    /// structured API rewrites the route key as plaintext, which an older
    /// release reads correctly.
    #[tokio::test]
    async fn turning_encryption_off_rewrites_route_keys_as_plaintext_for_downgrade() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![route("openrouter", Some(PLAIN))];
        })
        .await;
        assert!(!disk(&state).contains(PLAIN), "test precondition: encrypted");
        let (status, body) = post_config_confirmed(&base, serde_json::json!({"secrets": {"encrypt": false}})).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let tree: toml::Value = toml::from_str(&disk(&state)).expect("test: parses");
        let stored_key = tree
            .get("model_routes")
            .and_then(|routes| routes.get(0))
            .and_then(|route| route.get("api_key"))
            .and_then(toml::Value::as_str);
        assert_eq!(stored_key, Some(PLAIN), "an older release must find the plaintext key");
        server.abort();
    }

    const USERINFO: &str = "userinfo-sentinel-pw";

    async fn userinfo_on_disk() -> (tempfile::TempDir, AppState, String, tokio::task::JoinHandle<()>) {
        on_disk(|config| {
            config.default_provider = Some(format!("custom:http://alice:{USERINFO}@127.0.0.1:1/v1"));
            config.reliability.fallback_providers = vec![format!("custom:http://bob:{USERINFO}@127.0.0.1:2/v1")];
            config.model_routes = vec![route(&format!("custom:http://carol:{USERINFO}@127.0.0.1:3/v1"), None)];
        })
        .await
    }

    /// R-C2Kf S-2: `GET /api/config` returned `default_provider`,
    /// `fallback_providers` and `model_routes[].provider` verbatim, so a
    /// `custom:` URL's userinfo reached every console reader.
    #[tokio::test]
    async fn get_config_redacts_url_userinfo_in_provider_labels() {
        let (_dir, _state, base, server) = userinfo_on_disk().await;
        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200, "unexpected body: {body}");
        assert!(!body.contains(USERINFO), "GET /api/config leaked URL userinfo: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("test: json body");
        assert_eq!(
            json.pointer("/default_provider"),
            Some(&serde_json::json!("custom:http://[redacted]@127.0.0.1:1/v1"))
        );

        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/config/files"),
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(status, 200, "unexpected body: {body}");
        assert!(!body.contains(USERINFO), "the raw file API leaked URL userinfo: {body}");
        server.abort();
    }

    /// The console posts back what it read. The redacted labels it sends
    /// unchanged must keep the stored endpoints, credential included.
    #[tokio::test]
    async fn post_config_round_trip_keeps_the_stored_endpoint_userinfo() {
        let (_dir, state, base, server) = userinfo_on_disk().await;
        let mut current = get_config_json(&base).await;
        *current
            .pointer_mut("/default_temperature")
            .expect("test: temperature present") = serde_json::json!(0.25);
        let (status, body) = post_config(&base, current).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains(USERINFO), "the response leaked URL userinfo: {body}");

        let config = state.config.desired().config.clone();
        assert_eq!(
            config.default_provider.as_deref(),
            Some(format!("custom:http://alice:{USERINFO}@127.0.0.1:1/v1").as_str())
        );
        assert_eq!(
            config.reliability.fallback_providers,
            vec![format!("custom:http://bob:{USERINFO}@127.0.0.1:2/v1")]
        );
        assert_eq!(
            config.model_routes.first().map(|route| route.provider.as_str()),
            Some(format!("custom:http://carol:{USERINFO}@127.0.0.1:3/v1").as_str())
        );
        assert!((config.default_temperature - 0.25).abs() < f64::EPSILON);
        assert!(!disk(&state).contains("[redacted]"), "a mask must never reach disk");
        server.abort();
    }

    /// RED before PMME-g (D-088 C-6): `GET /api/config` redacted only URL
    /// userinfo, so a token in a provider URL's query (any parameter name)
    /// or fragment reached every reader. It is now redacted, and a round
    /// trip of the redacted value keeps the stored one.
    #[tokio::test]
    async fn get_config_redacts_url_query_values_and_a_round_trip_keeps_them() {
        const QUERY: &str = "custom:http://127.0.0.1:2/v1?sig=sentinel-query-sig&flag#sentinel-fragment";
        let (_dir, state, base, server) = on_disk(|config| {
            config.reliability.fallback_providers = vec![QUERY.to_string()];
        })
        .await;
        let mut current = get_config_json(&base).await;
        let shown = current.to_string();
        assert!(
            !shown.contains("sentinel-"),
            "GET /api/config leaked a query value: {shown}"
        );
        assert!(
            shown.contains("custom:http://127.0.0.1:2/v1?sig=[redacted]&[redacted]#[redacted]"),
            "{shown}"
        );

        *current
            .pointer_mut("/default_temperature")
            .expect("test: temperature present") = serde_json::json!(0.25);
        let (status, body) = post_config(&base, current).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!body.contains("sentinel-"), "{body}");
        assert_eq!(
            state.config.desired().config.reliability.fallback_providers,
            vec![QUERY.to_string()],
            "the stored URL is kept"
        );
        server.abort();
    }

    /// A redacted label inside a changed array cannot be matched to the stored
    /// one, and a redacted label for a new endpoint names no endpoint at all:
    /// both are refused and nothing is written.
    #[tokio::test]
    async fn post_config_refuses_a_redacted_label_it_cannot_restore() {
        let (_dir, state, base, server) = userinfo_on_disk().await;
        let before_disk = disk(&state);

        let mut current = get_config_json(&base).await;
        current
            .pointer_mut("/reliability/fallback_providers")
            .and_then(serde_json::Value::as_array_mut)
            .expect("test: fallback list present")
            .push(serde_json::json!("openrouter"));
        let (status, body) = post_config(&base, current).await;
        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(
            body.contains("reliability.fallback_providers[0]"),
            "the refusal must name the path: {body}"
        );
        assert_eq!(disk(&state), before_disk, "nothing may be written");

        let (status, body) = post_config(
            &base,
            serde_json::json!({"default_provider": "custom:http://[redacted]@127.0.0.1:9/v1"}),
        )
        .await;
        assert_eq!(
            status, 400,
            "a redacted label for a new endpoint must be refused: {body}"
        );
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        server.abort();
    }

    /// R-C2BUf F-1b: `GET /api/config` and `prx config show` redact every
    /// spelling the URL parser reads userinfo from, not only `scheme://`.
    #[tokio::test]
    async fn get_config_and_config_show_redact_every_hostile_userinfo_spelling() {
        for (label, secret) in crate::providers::label_redaction_tests::HOSTILE_USERINFO {
            let dir = tempfile::TempDir::new().expect("test: temp config dir");
            let mut config = Config::default();
            config.config_path = dir.path().join("config.toml");
            config.workspace_dir = dir.path().join("workspace");
            config.default_provider = Some((*label).to_string());
            let show = secret_paths::redacted_stored_toml(&config)
                .expect("test: config show rendering")
                .to_string();
            assert!(!show.contains(secret), "config show printed {label:?}: {show}");
            let (base, server) = serve(app_state(config)).await;
            let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
            server.abort();
            assert_eq!(status, 200, "unexpected body: {body}");
            let json: serde_json::Value = serde_json::from_str(&body).expect("test: json body");
            let shown = json
                .pointer("/default_provider")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            assert!(!shown.contains(secret), "GET /api/config printed {label:?}: {shown}");
            assert!(!shown.contains("alice"), "GET /api/config printed {label:?}: {shown}");
        }
    }

    /// RED before PMME-h (D-095 N-3): two stored price keys that redact
    /// alike (`...?sig=A` and `...?sig=B`) both matched a posted redacted
    /// key, and the first one was edited on a guess. The ambiguous key is
    /// now refused and nothing is written.
    #[tokio::test]
    async fn an_ambiguous_redacted_price_key_is_refused() {
        let pricing = |output: f64| crate::config::schema::ModelPricing {
            input: 1.0,
            output,
            cache_write: 0.0,
            cache_read: 0.0,
        };
        let (_dir, state, base, server) = on_disk(|config| {
            config.cost.prices.insert(
                "custom:http://127.0.0.1:1/v1/model-a?sig=sentinel-a".into(),
                pricing(2.0),
            );
            config.cost.prices.insert(
                "custom:http://127.0.0.1:1/v1/model-a?sig=sentinel-b".into(),
                pricing(3.0),
            );
        })
        .await;
        let before = disk(&state);
        let (status, body) = post_config(
            &base,
            serde_json::json!({"cost": {"prices": {"custom:http://127.0.0.1:1/v1/model-a?sig=[redacted]": {
                "input": 9.0, "output": 9.0, "cache_write": 0.0, "cache_read": 0.0
            }}}}),
        )
        .await;
        assert_eq!(status, 400, "an ambiguous redacted key was applied: {body}");
        assert!(body.contains("redaction mask"), "refused as a mask: {body}");
        assert!(!body.contains("sentinel-"), "{body}");
        assert_eq!(disk(&state), before, "nothing was written");
        server.abort();
    }

    /// R-C2BUf F-3 residual: a `[cost].prices` key may spell a provider with
    /// its credentials. The config API and `config show` mask the userinfo in
    /// the key, and a round trip keeps the stored key and its price.
    #[tokio::test]
    async fn a_credentialed_price_key_is_masked_and_survives_a_round_trip() {
        let key = format!("custom:http://alice:{USERINFO}@127.0.0.1:1/v1/model-a");
        let (_dir, state, base, server) = on_disk(|config| {
            config.cost.prices.insert(
                key.clone(),
                crate::config::schema::ModelPricing {
                    input: 1.5,
                    output: 2.5,
                    cache_write: 0.0,
                    cache_read: 0.0,
                },
            );
        })
        .await;
        let show = secret_paths::redacted_stored_toml(&state.config.desired().config)
            .expect("test: config show rendering")
            .to_string();
        assert!(!show.contains(USERINFO), "config show printed the price key: {show}");

        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200, "unexpected body: {body}");
        assert!(
            !body.contains(USERINFO),
            "GET /api/config printed the price key: {body}"
        );
        assert!(
            body.contains("custom:http://[redacted]@127.0.0.1:1/v1/model-a"),
            "{body}"
        );

        let before_keys: std::collections::BTreeSet<String> =
            state.config.desired().config.cost.prices.keys().cloned().collect();
        let mut current: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        *current
            .pointer_mut("/default_temperature")
            .expect("test: temperature present") = serde_json::json!(0.25);
        let (status, body) = post_config(&base, current).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let prices = state.config.desired().config.cost.prices.clone();
        let after_keys: std::collections::BTreeSet<String> = prices.keys().cloned().collect();
        assert_eq!(after_keys, before_keys, "the round trip added or lost a price key");
        assert!(
            prices
                .get(&key)
                .is_some_and(|price| (price.output - 2.5).abs() < f64::EPSILON),
            "the stored price changed"
        );
        assert!(
            prices.contains_key(&key),
            "the stored key was replaced: {:?}",
            prices.keys()
        );
        assert!(!disk(&state).contains("[redacted]"), "a mask must never reach disk");

        // A redacted key for a price nobody stored names no endpoint: refused.
        let before = disk(&state);
        let (status, body) = post_config(
            &base,
            serde_json::json!({"cost": {"prices": {"custom:http://[redacted]@127.0.0.1:9/v1/m": {
                "input": 1.0, "output": 1.0, "cache_write": 0.0, "cache_read": 0.0
            }}}}),
        )
        .await;
        assert_eq!(status, 400, "a redacted key for a new price must be refused: {body}");
        assert_eq!(disk(&state), before, "nothing may be written");
        server.abort();
    }

    /// D-027: `POST /api/config` cannot add a price key that collides with a
    /// stored credentialed key once redacted; nothing is written and the
    /// error names the redacted key only.
    #[tokio::test]
    async fn post_config_refuses_a_colliding_credentialed_price_key() {
        let key = format!("custom:http://alice:{USERINFO}@127.0.0.1:1/v1/model-a");
        let (_dir, state, base, server) = on_disk(|config| {
            config.cost.prices.insert(
                key.clone(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                },
            );
        })
        .await;
        let before = disk(&state);
        let (status, body) = post_config(
            &base,
            serde_json::json!({"cost": {"prices": {"custom:http://bob:other-d027@127.0.0.1:1/v1/model-a": {
                "input": 9.0, "output": 9.0, "cache_write": 0.0, "cache_read": 0.0
            }}}}),
        )
        .await;
        assert_eq!(status, 400, "a colliding price key must be refused: {body}");
        assert!(
            body.contains("custom:http://[redacted]@127.0.0.1:1/v1/model-a"),
            "{body}"
        );
        assert!(!body.contains(USERINFO) && !body.contains("other-d027"), "{body}");
        assert_eq!(disk(&state), before, "nothing may be written");
        server.abort();
    }

    /// INT-2B a55 (R-C2BUh, D-027): the raw file route refuses the same
    /// colliding credentialed price key as `POST /api/config`. Coverage for
    /// working code (no red-before): the refusal comes from the shared
    /// configuration validation every write path stages.
    #[tokio::test]
    async fn put_config_file_refuses_a_colliding_credentialed_price_key() {
        let key = format!("custom:http://alice:{USERINFO}@127.0.0.1:1/v1/model-a");
        let (_dir, state, base, server) = on_disk(|config| {
            config.cost.prices.insert(
                key.clone(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                },
            );
        })
        .await;
        let before = disk(&state);
        let content = format!(
            "default_temperature = 0.7\n\n[cost.prices.\"{key}\"]\ninput = 1.0\noutput = 1.0\n\n\
             [cost.prices.\"custom:http://bob:other-d027@127.0.0.1:1/v1/model-a\"]\ninput = 9.0\noutput = 9.0\n"
        );
        let (status, body) = put_file(&base, "config.toml", &content).await;
        assert_eq!(status, 400, "a colliding price key must be refused: {body}");
        assert!(
            body.contains("custom:http://[redacted]@127.0.0.1:1/v1/model-a"),
            "{body}"
        );
        assert!(!body.contains(USERINFO) && !body.contains("other-d027"), "{body}");
        assert_eq!(disk(&state), before, "nothing may be written");
        server.abort();
    }

    /// B2f (R-INT2BB2 F-10), RED before: `POST /api/config` carrying
    /// `security.audit.sign_events` answered 200 and dropped the key
    /// silently. After: the body carries the removed-key warning a file load
    /// logs, and nothing is stored for the key.
    #[tokio::test]
    async fn post_config_warns_about_a_removed_key() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let (status, body) = post_config(
            &base,
            serde_json::json!({"security": {"audit": {"sign_events": true}}, "default_temperature": 0.4}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("security.audit.sign_events"), "{body}");
        assert!(body.contains("Ignoring removed configuration key"), "{body}");
        assert!(!disk(&state).contains("sign_events"), "nothing is stored for the key");
        server.abort();
    }

    /// B2f, RED before: a raw file write that sets
    /// `security.audit.sign_events` answered 200 with no mention of it (only
    /// the load logged it). After: the response carries the same removed-key
    /// warning as the structured writes; the file is stored as written and
    /// the key has no effect.
    #[tokio::test]
    async fn put_config_file_warns_about_a_removed_key() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let stored = disk(&state);
        assert!(stored.contains("[security.audit]\n"), "test precondition: {stored}");
        let content = stored.replacen("[security.audit]\n", "[security.audit]\nsign_events = true\n", 1);
        let (status, body) = put_file(&base, "config.toml", &content).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("security.audit.sign_events"), "{body}");
        assert!(body.contains("Ignoring removed configuration key"), "{body}");
        server.abort();
    }

    /// INT-2B D045-I4 (R-PMMDi I-4), RED before: a raw file name longer than
    /// the filesystem allows reached staging and answered with the OS error
    /// ("File name too long (os error 63)"). After: a typed 400 naming the
    /// 255-byte limit, before any filesystem access; nothing is written.
    #[tokio::test]
    async fn put_config_file_refuses_an_overlong_name_with_a_typed_error() {
        let (dir, state, base, server) = on_disk(|_| {}).await;
        let before = disk(&state);
        let name = format!("{}.toml", "a".repeat(295));
        let (status, body) = put_file(&base, &name, "[cost]\n").await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("255 bytes"), "the limit is named: {body}");
        assert!(!body.contains("os error"), "no OS error text: {body}");
        assert_eq!(disk(&state), before, "nothing may be written");
        assert!(!dir.path().join("config.d").join(&name).exists());

        // The longest allowed name is still accepted by the name rule.
        let longest = format!("{}.toml", "b".repeat(250));
        assert_eq!(longest.len(), 255);
        crate::config::files::validate_raw_config_file_name(&longest).expect("test: 255 bytes is allowed");
        server.abort();
    }

    // ── Configuration transactions (PMM-D) ───────────────────────────────

    /// A runtime participant that refuses a generation whose default
    /// temperature is above 1.5 or whose root key is `REFUSED_KEY`, as a
    /// provider rebuild refuses an unusable default.
    struct RefusingParticipant;

    /// The root key the refusing participant rejects. Not a real secret.
    const REFUSED_KEY: &str = "refused-sentinel-credential";

    struct AcceptedGeneration;

    impl crate::config::PreparedConfigGeneration for AcceptedGeneration {
        fn commit(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn rollback(&mut self) {}
    }

    impl crate::config::ConfigGenerationParticipant for RefusingParticipant {
        fn name(&self) -> &'static str {
            "test_refusing_participant"
        }

        fn supports_rebuild_field(&self, field: &str) -> bool {
            field == "api_key"
        }

        fn prepares_for_field(&self, field: &str) -> bool {
            field == "default_temperature" || field == "api_key"
        }

        fn prepare(
            &self,
            generation: Arc<crate::config::ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
            if generation.effective.default_temperature > 1.5 {
                anyhow::bail!("test participant refuses a temperature above 1.5");
            }
            if generation.effective.api_key.as_deref() == Some(REFUSED_KEY) {
                anyhow::bail!("test participant refuses this key");
            }
            Ok(Box::new(AcceptedGeneration))
        }
    }

    fn register_refusing_participant(state: &AppState) -> Arc<dyn crate::config::ConfigGenerationParticipant> {
        let participant: Arc<dyn crate::config::ConfigGenerationParticipant> = Arc::new(RefusingParticipant);
        state.config.register_participant(&participant);
        participant
    }

    fn history_records(state: &AppState) -> Vec<serde_json::Value> {
        let dir = state
            .config
            .desired()
            .config
            .config_path
            .parent()
            .expect("test: config dir")
            .join(".config-history");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let path = entry.expect("test: entry").path().join("record.json");
                serde_json::from_str(&std::fs::read_to_string(path).expect("test: record")).expect("test: json")
            })
            .collect()
    }

    /// RED before PMM-D-3: `POST /api/config` saved the candidate, then the
    /// runtime refused it, and disk kept the refused candidate (500 "Saved
    /// configuration but failed to activate it"). After: 422, the previous
    /// bytes are back on disk, the active generation is unchanged, and the
    /// history says `rolled_back`.
    #[tokio::test]
    async fn post_config_refused_by_the_runtime_restores_the_previous_configuration() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let _participant = register_refusing_participant(&state);
        let before_disk = disk(&state);
        let before_generation = state.config.active_generation_id();

        let (status, body) = post_config(&base, serde_json::json!({"default_temperature": 1.9})).await;

        assert_eq!(status, 422, "unexpected response: {body}");
        assert!(body.contains("previous configuration was restored"), "{body}");
        assert_eq!(disk(&state), before_disk, "the refused candidate must not stay on disk");
        assert_eq!(state.config.active_generation_id(), before_generation);
        assert!(state.config.desired().config.default_temperature < 1.5);
        let records = history_records(&state);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records.first().and_then(|record| record.get("status")),
            Some(&serde_json::json!("rolled_back"))
        );
        server.abort();
    }

    /// RED before PMM-D-3: the raw PUT committed the file, activation was
    /// refused, and the answer blamed "TOML syntax" with the refused file
    /// left on disk. After: 422 and the previous file is restored.
    #[tokio::test]
    async fn put_config_file_refused_by_the_runtime_restores_the_previous_file() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let _participant = register_refusing_participant(&state);
        let before_disk = disk(&state);
        let before_generation = state.config.active_generation_id();
        let content = before_disk.replacen("default_temperature = 0.7", "default_temperature = 1.9", 1);
        assert_ne!(content, before_disk, "test precondition: the edit applies");

        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 422, "unexpected response: {body}");
        assert!(!body.contains("TOML syntax"), "{body}");
        assert_eq!(disk(&state), before_disk);
        assert_eq!(state.config.active_generation_id(), before_generation);
        server.abort();
    }

    /// RED before PMM-D-3: the credential endpoint saved a key the runtime
    /// then refused, and the refused key stayed stored. After: 422, the
    /// stored file is the previous one, and nothing echoes the key.
    #[tokio::test]
    async fn put_secret_refused_by_the_runtime_restores_the_previous_key() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let _participant = register_refusing_participant(&state);
        let before_disk = disk(&state);

        let (status, body) = put_secret(&base, serde_json::json!({"slot": "api_key", "value": REFUSED_KEY})).await;

        assert_eq!(status, 422, "unexpected response: {body}");
        assert!(!body.contains(REFUSED_KEY), "{body}");
        assert_eq!(disk(&state), before_disk);
        let reloaded = Config::load_from_path(
            &state.config.desired().config.config_path,
            state.config.desired().config.workspace_dir.clone(),
        )
        .expect("test: reload");
        assert_eq!(reloaded.api_key, None);
        server.abort();
    }

    /// RED before PMM-D-3 (lost update): a manual edit that the process has
    /// not reloaded yet was overwritten by an unrelated `POST`, which merges
    /// onto the older desired configuration. After: 409, the edit survives.
    #[tokio::test]
    async fn post_config_never_overwrites_an_edit_it_has_not_seen() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let path = state.config.desired().config.config_path.clone();
        let edited = disk(&state).replacen("default_temperature = 0.7", "default_temperature = 0.4", 1);
        assert_ne!(edited, disk(&state), "test precondition: the edit applies");
        std::fs::write(&path, &edited).expect("test: manual edit");

        let (status, body) = post_config(&base, serde_json::json!({"default_model": "posted-model"})).await;

        assert_eq!(status, 409, "unexpected response: {body}");
        assert_eq!(disk(&state), edited, "the manual edit must survive");
        server.abort();
    }

    async fn put_file_if_match(base: &str, name: &str, content: &str, revision: &str) -> (u16, String) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("test: http client");
        let response = client
            .put(format!("{base}/api/config/files/{name}"))
            .bearer_auth(TOKEN)
            .header("If-Match", format!("\"{revision}\""))
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .expect("test: request");
        let status = response.status().as_u16();
        (status, response.text().await.expect("test: body"))
    }

    /// RED before PMM-D-3: the raw editor overwrote whatever was on disk.
    /// After: a write based on a stale revision (`If-Match`) is refused with
    /// 409 and nothing is written; a current revision is accepted.
    #[tokio::test]
    async fn put_config_file_with_a_stale_revision_is_refused() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let path = state.config.desired().config.config_path.clone();
        let stale = state.config.desired().source_revision.fingerprint_sha256.clone();
        let edited = disk(&state).replacen("default_temperature = 0.7", "default_temperature = 0.4", 1);
        std::fs::write(&path, &edited).expect("test: another writer");

        let content = edited.replacen("default_temperature = 0.4", "default_temperature = 0.6", 1);
        let (status, body) = put_file_if_match(&base, "config.toml", &content, &stale).await;

        assert_eq!(status, 409, "unexpected response: {body}");
        assert_eq!(disk(&state), edited);
        server.abort();
    }

    /// The revision `GET /api/config/files` reports as its `ETag` is the one
    /// a raw write names in `If-Match`; a current revision is accepted.
    #[tokio::test]
    async fn put_config_file_with_the_current_revision_is_accepted() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let response = client
            .get(format!("{base}/api/config/files"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("test: get files");
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .expect("test: ETag header")
            .trim_matches('"')
            .to_string();
        assert_eq!(etag, state.config.desired().source_revision.fingerprint_sha256);

        let content = disk(&state).replacen("default_temperature = 0.7", "default_temperature = 0.6", 1);
        let (status, body) = put_file_if_match(&base, "config.toml", &content, &etag).await;

        assert_eq!(status, 200, "unexpected response: {body}");
        assert_eq!(disk(&state), content);
        assert!((state.config.load_full().default_temperature - 0.6).abs() < f64::EPSILON);
        let records = history_records(&state);
        assert_eq!(records.len(), 1);
        let record = records.first().expect("test: record");
        assert_eq!(record.get("status"), Some(&serde_json::json!("activated")));
        assert_eq!(record.get("actor"), Some(&serde_json::json!("gateway_config_file")));
        server.abort();
    }

    /// PMMD-f (F-2): a credential written through `POST /api/config` needs
    /// `confirm_high_risk`; without it the answer is 428, naming the path
    /// only, and nothing is written.
    #[tokio::test]
    async fn post_config_credential_write_needs_confirmation() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        let (status, body) = post_config(&base, serde_json::json!({"api_key": PLAIN})).await;

        assert_eq!(status, 428, "unexpected response: {body}");
        assert!(body.contains("confirm_high_risk"), "{body}");
        assert!(body.contains("api_key"), "{body}");
        assert!(!body.contains(PLAIN), "{body}");
        assert_eq!(disk(&state), before_disk);

        let (status, body) = post_config_confirmed(&base, serde_json::json!({"api_key": PLAIN})).await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(!disk(&state).contains(PLAIN), "stored encrypted");
        server.abort();
    }

    /// PMMD-f (F-7): `If-Match` uses strong comparison; a weak validator is
    /// refused.
    #[tokio::test]
    async fn put_config_file_refuses_a_weak_if_match() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let revision = state.config.desired().source_revision.fingerprint_sha256.clone();
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let response = client
            .put(format!("{base}/api/config/files/config.toml"))
            .bearer_auth(TOKEN)
            .header("If-Match", format!("W/\"{revision}\""))
            .json(&serde_json::json!({ "content": disk(&state) }))
            .send()
            .await
            .expect("test: request");
        assert_eq!(response.status().as_u16(), 400);
        server.abort();
    }

    /// RED before PMMD-g (N-5): the credential endpoint confirmed the whole
    /// plan, so a desired configuration that differs from disk under the
    /// same revision (here an endpoint change) was written with the key,
    /// unconfirmed. After: only the slot's own credential change is
    /// confirmed; anything else answers 428 and nothing is written.
    #[tokio::test]
    async fn put_secret_confirms_only_its_own_slot() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let mut drifted = state.config.desired().config.as_ref().clone();
        drifted.api_url = Some("https://elsewhere.example/v1".into());
        state
            .config
            .apply_runtime_config(drifted, crate::config::ConfigReloadTrigger::Test)
            .expect("test: in-memory drift under the disk revision");

        let (status, body) = put_secret(&base, serde_json::json!({"slot": "api_key", "value": PLAIN})).await;

        assert_eq!(status, 428, "unexpected response: {body}");
        assert!(body.contains("api_url"), "{body}");
        assert!(!body.contains(PLAIN), "{body}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        server.abort();
    }

    /// RED before PMMD-h (G-1): the raw file API accepted a case variant of a
    /// managed fragment name; it answers 400 now, naming the managed name.
    #[tokio::test]
    async fn put_config_file_refuses_a_case_variant_of_a_managed_name() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        for name in ["Security.toml", "Routing.toml", "CONFIG.toml"] {
            let (status, body) = put_file(&base, name, "[secrets]\nencrypt = false\n").await;
            assert_eq!(status, 400, "{name}: unexpected response: {body}");
            assert!(body.to_lowercase().contains(&name.to_lowercase()), "{name}: {body}");
        }
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    /// RED before PMMD-h (G-2): a 428 carries the revision the refused plan
    /// was computed against, so a confirmed retry can be bound to it with
    /// `If-Match` (a change in between then answers 409).
    #[tokio::test]
    async fn a_confirmation_required_answer_carries_the_planned_revision() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let revision = state.config.desired().source_revision.fingerprint_sha256.clone();
        let content = format!("api_url = \"https://elsewhere.example/v1\"\n{}", disk(&state));

        let (status, body) = put_file(&base, "config.toml", &content).await;

        assert_eq!(status, 428, "unexpected response: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        assert_eq!(json.get("revision"), Some(&serde_json::json!(revision)), "{body}");

        // The files change while the operator reads the dialog: the bound,
        // confirmed retry is refused instead of approving other risks.
        let path = state.config.desired().config.config_path.clone();
        let edited = disk(&state).replacen("default_temperature = 0.7", "default_temperature = 0.4", 1);
        std::fs::write(&path, &edited).expect("test: another writer");
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let retry = client
            .put(format!("{base}/api/config/files/config.toml?confirm_high_risk=true"))
            .bearer_auth(TOKEN)
            .header("If-Match", format!("\"{revision}\""))
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .expect("test: retry");
        assert_eq!(retry.status().as_u16(), 409);
        assert_eq!(disk(&state), edited);
        server.abort();
    }

    /// RED before PMMD-i (H-1 over HTTP, R-PMMDh probe): the percent-encoded
    /// U+017F alias of `security.toml` answered 200 and turned encryption off.
    #[tokio::test]
    async fn put_config_file_refuses_a_non_ascii_file_name() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);

        for name in ["%C5%BFecurity.toml", "networ%E2%84%AA.toml", "config%C3%A9.toml"] {
            let (status, body) = put_file(&base, name, "[secrets]\nencrypt = false\n").await;
            assert_eq!(status, 400, "{name}: unexpected response: {body}");
        }
        assert_eq!(disk(&state), before_disk);
        assert!(state.config.desired().config.secrets.encrypt);
        server.abort();
    }

    // ── SECRETS-OTHER: channel, gateway and MCP credentials ────────────────

    fn telegram(bot_token: &str) -> crate::config::TelegramConfig {
        toml::from_str(&format!("bot_token = \"{bot_token}\"\nallowed_users = []\n")).expect("test: telegram config")
    }

    fn with_channel_and_mcp(config: &mut Config) {
        config.channels_config.telegram = Some(telegram("stored-telegram-token"));
        let server: crate::config::McpServerConfig =
            toml::from_str("command = \"mcp-local\"\nenv = { LOG_LEVEL = \"info\" }\n").expect("test: mcp server");
        config.mcp.servers.insert("local".into(), server);
    }

    /// RED before SECRETS-OTHER: 400 "unknown credential slot". After: a
    /// channel token and an MCP environment value are set write-only, stored
    /// `enc2:`, activated, and never returned by any read surface.
    #[tokio::test]
    async fn put_secret_sets_a_channel_token_and_an_mcp_env_value_encrypted() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(with_channel_and_mcp).await;

        // A channel token is rotated by the request alone; an MCP launch
        // environment value needs the confirmed, revision-bound retry.
        for (slot, value, confirmed) in [
            ("channels_config.telegram.bot_token", PLAIN, false),
            ("mcp.servers.local.env.GITHUB_PAT", PLAIN_2, true),
        ] {
            let body = serde_json::json!({"slot": slot, "value": value});
            let (status, body) = if confirmed {
                put_secret_confirmed(&base, &state, body).await
            } else {
                put_secret(&base, body).await
            };
            assert_eq!(status, 200, "{slot}: unexpected response: {body}");
            assert!(
                !body.contains(value),
                "{slot}: the response echoed the credential: {body}"
            );
        }

        let stored = disk(&state);
        assert!(
            !stored.contains(PLAIN) && !stored.contains(PLAIN_2),
            "stored as plaintext:\n{stored}"
        );
        let tree: toml::Value = toml::from_str(&stored).expect("test: stored config parses");
        let token = tree
            .get("channels_config")
            .and_then(|channels| channels.get("telegram"))
            .and_then(|telegram| telegram.get("bot_token"))
            .and_then(toml::Value::as_str)
            .expect("test: stored telegram token");
        assert!(crate::security::SecretStore::is_secure_encrypted(token), "{token}");

        let desired = state.config.desired();
        assert_eq!(
            desired
                .config
                .channels_config
                .telegram
                .as_ref()
                .map(|t| t.bot_token.as_str()),
            Some(PLAIN)
        );
        let env = &desired.config.mcp.servers.get("local").expect("test: mcp server").env;
        assert_eq!(env.get("GITHUB_PAT").map(String::as_str), Some(PLAIN_2));
        assert_eq!(
            env.get("LOG_LEVEL").map(String::as_str),
            Some("info"),
            "other variables stay"
        );

        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200);
        assert!(!body.contains(PLAIN) && !body.contains(PLAIN_2), "GET leaked: {body}");
        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/config/files"),
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert!(!body.contains(PLAIN) && !body.contains(PLAIN_2), "files leaked: {body}");
        server.abort();
    }

    /// RED before SECRETS-OTHER: with the high-risk change confirmed, 200,
    /// and the plaintext bot token was committed to disk. After: 400 naming
    /// the path only, nothing written, confirmed or not.
    #[tokio::test]
    async fn put_config_file_refuses_a_plaintext_channel_token() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let before_generation = state.config.active_generation_id();

        let content =
            format!("{before_disk}\n[channels_config.telegram]\nbot_token = \"{PLAIN}\"\nallowed_users = []\n");
        let (status, body) = put_file_confirmed(&base, "config.toml", &content).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(!body.contains(PLAIN), "the response echoed the secret: {body}");
        assert!(
            body.contains("channels_config.telegram.bot_token"),
            "the refusal must name the path: {body}"
        );
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        assert_eq!(state.config.active_generation_id(), before_generation);
        server.abort();
    }

    /// Guard: the whole-config `GET`→`POST` round trip keeps a channel token,
    /// an MCP environment value and a paired token as stored; the masks are
    /// never written.
    #[tokio::test]
    async fn get_post_round_trip_keeps_channel_mcp_and_paired_secrets() {
        let (_dir, state, base, server) = on_disk(|config| {
            with_channel_and_mcp(config);
            config.gateway.paired_tokens = vec!["stored-paired-token".into()];
        })
        .await;
        let (status, body) = call(reqwest::Method::GET, &format!("{base}/api/config"), Some(TOKEN), None).await;
        assert_eq!(status, 200);
        assert!(!body.contains("stored-"), "GET leaked: {body}");
        let mut shown: serde_json::Value = serde_json::from_str(&body).expect("test: json");
        if let Some(temperature) = shown.get_mut("default_temperature") {
            *temperature = serde_json::json!(0.9);
        }
        let (status, body) = post_config(&base, shown).await;
        assert_eq!(status, 200, "unexpected response: {body}");

        let desired = state.config.desired();
        assert_eq!(
            desired
                .config
                .channels_config
                .telegram
                .as_ref()
                .map(|t| t.bot_token.as_str()),
            Some("stored-telegram-token")
        );
        assert_eq!(
            desired
                .config
                .mcp
                .servers
                .get("local")
                .and_then(|server| server.env.get("LOG_LEVEL"))
                .map(String::as_str),
            Some("info")
        );
        assert_eq!(
            desired.config.gateway.paired_tokens,
            vec!["stored-paired-token".to_string()]
        );
        assert!(
            !disk(&state).contains(secret_paths::REDACTION_MASK),
            "a mask was stored"
        );
        server.abort();
    }

    /// `PUT /api/config/secrets?confirm_high_risk=true` with `If-Match`
    /// naming the current revision: the confirmed retry of a `428`.
    async fn put_secret_confirmed(base: &str, state: &AppState, body: serde_json::Value) -> (u16, String) {
        let revision = state.config.desired().source_revision.fingerprint_sha256.clone();
        put_secret_with(base, "?confirm_high_risk=true", Some(&revision), body).await
    }

    async fn put_secret_with(
        base: &str,
        query: &str,
        if_match: Option<&str>,
        body: serde_json::Value,
    ) -> (u16, String) {
        let client = reqwest::Client::builder().no_proxy().build().expect("test: client");
        let mut request = client
            .put(format!("{base}/api/config/secrets{query}"))
            .bearer_auth(TOKEN)
            .json(&body);
        if let Some(revision) = if_match {
            request = request.header("If-Match", format!("\"{revision}\""));
        }
        let response = request.send().await.expect("test: request");
        let status = response.status().as_u16();
        (status, response.text().await.expect("test: body"))
    }

    /// R-SECRETS F1, RED before SECRETS-f: an MCP launch environment
    /// variable (`NODE_OPTIONS`, which changes what the program runs) was
    /// added through the credential endpoint with 200, confirmation implied.
    /// After: 428 naming the reason and the revision, nothing written; the
    /// confirmed retry bound to that revision stores it; a retry bound to a
    /// stale revision answers 409. Replacing an existing variable too.
    #[tokio::test]
    async fn put_secret_asks_to_confirm_an_mcp_launch_environment_change() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(with_channel_and_mcp).await;
        let before_disk = disk(&state);
        let revision = state.config.desired().source_revision.fingerprint_sha256.clone();

        for (variable, value) in [("NODE_OPTIONS", "--require /tmp/x.js"), ("LOG_LEVEL", "debug")] {
            let slot = format!("mcp.servers.local.env.{variable}");
            let (status, body) = put_secret(&base, serde_json::json!({"slot": slot, "value": value})).await;
            assert_eq!(status, 428, "{slot}: unexpected response: {body}");
            let json: serde_json::Value = serde_json::from_str(&body).expect("test: json");
            assert_eq!(
                json.get("required"),
                Some(&serde_json::json!("confirm_high_risk")),
                "{body}"
            );
            assert_eq!(json.get("revision"), Some(&serde_json::json!(revision)), "{body}");
            assert!(body.contains("process_environment_changed"), "{body}");
            assert!(body.contains(&slot), "{body}");
            assert!(!body.contains(value), "the refusal echoed the value: {body}");
            assert_eq!(disk(&state), before_disk, "{slot}: nothing may be written");
        }

        // A retry bound to a stale revision is a conflict, never a write.
        let stale = "0".repeat(64);
        let (status, body) = put_secret_with(
            &base,
            "?confirm_high_risk=true",
            Some(&stale),
            serde_json::json!({"slot": "mcp.servers.local.env.NODE_OPTIONS", "value": "--require /tmp/x.js"}),
        )
        .await;
        assert_eq!(status, 409, "unexpected response: {body}");
        assert_eq!(disk(&state), before_disk);

        let (status, body) = put_secret_confirmed(
            &base,
            &state,
            serde_json::json!({"slot": "mcp.servers.local.env.NODE_OPTIONS", "value": "--require /tmp/x.js"}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        let env = state
            .config
            .desired()
            .config
            .mcp
            .servers
            .get("local")
            .map(|server| server.env.clone())
            .expect("test: server");
        assert_eq!(env.get("NODE_OPTIONS").map(String::as_str), Some("--require /tmp/x.js"));
        server.abort();
    }

    /// R-SECRETS F1, RED before SECRETS-f: the whole paired-token list was
    /// replaced through the credential endpoint with 200 (a planted bearer
    /// that locks every other client out at the next restart). After: 428,
    /// and the confirmed retry replaces it.
    #[tokio::test]
    async fn put_secret_asks_to_confirm_a_paired_token_change() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(|config| {
            config.gateway.paired_tokens = vec!["stored-paired-token".into()];
        })
        .await;
        let before_disk = disk(&state);
        let body = serde_json::json!({"slot": "gateway.paired_tokens", "value": [PLAIN]});

        let (status, reply) = put_secret(&base, body.clone()).await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("access_grant_changed"), "{reply}");
        assert!(!reply.contains(PLAIN), "{reply}");
        assert_eq!(disk(&state), before_disk);
        assert_eq!(
            state.config.desired().config.gateway.paired_tokens,
            vec!["stored-paired-token".to_string()]
        );

        let (status, reply) = put_secret_confirmed(&base, &state, body).await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        assert_eq!(
            state.config.desired().config.gateway.paired_tokens,
            vec![PLAIN.to_string()]
        );
        server.abort();
    }

    /// Guard: rotating a credential at the named slot (a channel token, a
    /// provider key, a node token) stays confirmed by the request itself.
    #[tokio::test]
    async fn put_secret_rotates_a_credential_without_a_separate_confirmation() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(with_channel_and_mcp).await;
        let (status, body) = put_secret(
            &base,
            serde_json::json!({"slot": "channels_config.telegram.bot_token", "value": PLAIN}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert_eq!(
            state
                .config
                .desired()
                .config
                .channels_config
                .telegram
                .as_ref()
                .map(|t| t.bot_token.as_str()),
            Some(PLAIN)
        );
        server.abort();
    }

    /// RED before PMM-H (D-099 7a): a credential slot was written at the Low
    /// gate, so `supervised` let the root key through this endpoint while it
    /// refused the same key as a credential reference (High). A secret write
    /// is High on every endpoint now: `403` at `supervised`, nothing written.
    #[tokio::test]
    async fn a_secret_write_takes_the_credential_gate_on_every_endpoint() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(|config| {
            config.autonomy.level = AutonomyLevel::Supervised;
            config.gateway.paired_tokens = vec!["stored-paired-token".into()];
            with_channel_and_mcp(config);
        })
        .await;
        let before_disk = disk(&state);

        let (status, reply) = put_secret(&base, serde_json::json!({"slot": "api_key", "value": PLAIN})).await;
        assert_eq!(status, 403, "unexpected response: {reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        assert!(!reply.contains(PLAIN), "{reply}");
        let (status, reply) = post_config_confirmed(&base, serde_json::json!({"api_key": PLAIN})).await;
        assert_eq!(status, 403, "unexpected response: {reply}");
        let (status, reply) =
            put_secret(&base, serde_json::json!({"slot": "gateway.paired_tokens", "value": []})).await;
        assert_eq!(status, 403, "unexpected response: {reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        let (status, reply) = post_config_confirmed(
            &base,
            serde_json::json!({"mcp": {"servers": {"local": {"command": "mcp-local", "env": {"LOG_LEVEL": "info", "GITHUB_PAT": PLAIN}}}}}),
        )
        .await;
        assert_eq!(status, 403, "unexpected response: {reply}");
        assert!(reply.contains("runtime approval grant"), "{reply}");
        assert!(!reply.contains(PLAIN), "{reply}");
        assert_eq!(disk(&state), before_disk);
        assert_eq!(state.config.desired().config.api_key, None);
        server.abort();
    }

    /// RED before PMM-H (D-099 7b): the endpoint scrubbed its messages by
    /// replacing the submitted value, so a value that is also a word garbled
    /// the refusal ("who is ***"). Nothing is replaced now, and the value is
    /// still never echoed (no message quotes it).
    #[tokio::test]
    async fn put_secret_messages_are_not_scrubbed_by_substring() {
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(|config| {
            config.gateway.paired_tokens = vec!["stored-paired-token".into()];
        })
        .await;
        let (status, reply) = put_secret(
            &base,
            serde_json::json!({"slot": "gateway.paired_tokens", "value": ["admitted"]}),
        )
        .await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("who is admitted"), "the refusal was garbled: {reply}");
        assert_eq!(
            state.config.desired().config.gateway.paired_tokens,
            vec!["stored-paired-token".to_string()]
        );
        server.abort();
    }

    /// One admission secret per kind (D-048), with the section it needs.
    const ADMISSION_SLOTS: &[(&str, &str)] = &[
        ("[webhook]\ntoken = \"stored-admission\"\n", "webhook.token"),
        (
            "[webhook]\ntoken = \"stored-webhook-token\"\nsigning_secret = \"stored-admission\"\n",
            "webhook.signing_secret",
        ),
        (
            "[nodes.server]\nbearer_token = \"stored-admission\"\n",
            "nodes.server.bearer_token",
        ),
        (
            "[nodes.server]\nhmac_secret = \"stored-admission\"\n",
            "nodes.server.hmac_secret",
        ),
        (
            "[channels_config.whatsapp]\nverify_token = \"stored-admission\"\n",
            "channels_config.whatsapp.verify_token",
        ),
        (
            "[channels_config.whatsapp]\napp_secret = \"stored-admission\"\n",
            "channels_config.whatsapp.app_secret",
        ),
        (
            "[channels_config.lark]\napp_id = \"lark-app\"\napp_secret = \"lark-secret\"\nverification_token = \"stored-admission\"\n",
            "channels_config.lark.verification_token",
        ),
        (
            "[channels_config.lark]\napp_id = \"lark-app\"\napp_secret = \"lark-secret\"\nencrypt_key = \"stored-admission\"\n",
            "channels_config.lark.encrypt_key",
        ),
        (
            "[channels_config.linq]\napi_token = \"linq-api\"\nfrom_phone = \"+10000000000\"\nsigning_secret = \"stored-admission\"\n",
            "channels_config.linq.signing_secret",
        ),
        (
            "[channels_config.nextcloud_talk]\nbase_url = \"http://127.0.0.1:1\"\napp_token = \"nc-app\"\nwebhook_secret = \"stored-admission\"\n",
            "channels_config.nextcloud_talk.webhook_secret",
        ),
        (
            "[channels_config.wacli]\nwebhook_secret = \"stored-admission\"\n",
            "channels_config.wacli.webhook_secret",
        ),
        (
            "[channels_config.webhook]\nport = 1\nsecret = \"stored-admission\"\n",
            "channels_config.webhook.secret",
        ),
        (
            "[gateway]\npaired_tokens = [\"stored-admission\"]\n",
            "gateway.paired_tokens",
        ),
    ];

    /// Overlay `toml` onto the default configuration `on_disk` starts from.
    fn with_overlay(config: &mut Config, toml: &str) {
        let (config_path, workspace_dir) = (config.config_path.clone(), config.workspace_dir.clone());
        let mut tree = toml::Value::try_from(&*config).expect("test: default tree");
        fixture::overlay(&mut tree, toml::from_str(toml).expect("test: overlay parses"));
        *config = tree.try_into().expect("test: overlay deserializes");
        config.config_path = config_path;
        config.workspace_dir = workspace_dir;
    }

    /// The value at a slot's path in the desired configuration.
    fn desired_at(state: &AppState, slot: &str) -> Option<serde_json::Value> {
        let rendered = serde_json::to_value(state.config.desired().config.as_ref()).expect("test: render");
        rendered.pointer(&format!("/{}", slot.replace('.', "/"))).cloned()
    }

    /// D-048, RED before SECRETS-g: every admission secret except the
    /// paired-token list was set through the credential endpoint with 200,
    /// confirmation implied (a new webhook token, channel verify token or
    /// node bearer lets its holder in). After, per kind: 428 naming
    /// `access_grant_changed` and the revision, nothing written; a confirmed
    /// retry bound to a stale revision answers 409; the confirmed retry bound
    /// to the 428's revision stores it.
    #[tokio::test]
    async fn put_secret_asks_to_confirm_every_admission_secret() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        for (setup, slot) in ADMISSION_SLOTS {
            let (_dir, state, base, server) = on_disk(|config| with_overlay(config, setup)).await;
            let before_disk = disk(&state);
            let revision = state.config.desired().source_revision.fingerprint_sha256.clone();
            let list = *slot == "gateway.paired_tokens";
            let value = if list {
                serde_json::json!([PLAIN])
            } else {
                serde_json::json!(PLAIN)
            };
            let body = serde_json::json!({"slot": slot, "value": value});

            let (status, reply) = put_secret(&base, body.clone()).await;
            assert_eq!(status, 428, "{slot}: unexpected response: {reply}");
            let json: serde_json::Value = serde_json::from_str(&reply).expect("test: json");
            assert_eq!(
                json.get("required"),
                Some(&serde_json::json!("confirm_high_risk")),
                "{slot}: {reply}"
            );
            assert_eq!(
                json.get("revision"),
                Some(&serde_json::json!(revision)),
                "{slot}: {reply}"
            );
            assert!(reply.contains("access_grant_changed"), "{slot}: {reply}");
            assert!(!reply.contains(PLAIN), "{slot}: the refusal echoed the value: {reply}");
            assert_eq!(disk(&state), before_disk, "{slot}: nothing may be written");

            let stale = "0".repeat(64);
            let (status, reply) = put_secret_with(&base, "?confirm_high_risk=true", Some(&stale), body.clone()).await;
            assert_eq!(status, 409, "{slot}: unexpected response: {reply}");
            assert_eq!(disk(&state), before_disk, "{slot}: a stale retry may not write");

            let (status, reply) = put_secret_confirmed(&base, &state, body).await;
            assert_eq!(status, 200, "{slot}: unexpected response: {reply}");
            assert_eq!(desired_at(&state, slot), Some(value), "{slot}");
            assert!(!disk(&state).contains(PLAIN), "{slot}: stored as plaintext");
            server.abort();
        }
    }

    // ── CDP-04: the decision plane ─────────────────────────────────────────

    /// CDP-04 (SP-CDP-A 5.4, V6.07 API half): enabling remote decision
    /// processing through `POST /api/config` answers 428 naming
    /// `remote_decision_enabled` (and `endpoint_changed` for the new
    /// endpoint), with nothing written; the confirmed retry applies it.
    #[tokio::test]
    async fn enabling_remote_decision_processing_needs_confirmation() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        // The model is declared for decision_endpoint, so the decision
        // backend check (CDP-12) passes and the confirmation is what is left.
        let body = serde_json::json!({
            "causal_tree": {"backend": {
                "kind": "compatible_http",
                "endpoint": "https://decisions.example.com/v1",
                "model": "decider-1",
                "remote_enabled": true,
            }},
            "providers": {"models": [{
                "provider": "custom:https://decisions.example.com/v1",
                "model": "decider-1",
                "operations": ["decision_endpoint"],
            }]},
        });
        let (status, reply) = post_config(&base, body.clone()).await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("remote_decision_enabled"), "{reply}");
        assert!(reply.contains("causal_tree.backend.remote_enabled"), "{reply}");
        assert!(reply.contains("endpoint_changed"), "{reply}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");

        let (status, reply) = post_config_confirmed(&base, body).await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        assert!(state.config.desired().config.causal_tree.backend.remote_enabled);
        server.abort();
    }

    /// CDP-04 (V6.07 raw file half): a raw file that points the decision
    /// backend at a remote host is refused without `remote_enabled` (the
    /// load's validation), and with it answers 428 until confirmed.
    #[tokio::test]
    async fn local_to_remote_through_a_raw_file_needs_remote_enabled_and_confirmation() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let remote = "\n[causal_tree.backend]\nkind = \"compatible_http\"\nendpoint = \"https://decisions.example.com/v1\"\n\
                      model = \"decider-1\"\n";
        // The model is declared for decision_endpoint (the CDP-12 check).
        let declared = "\n[[providers.models]]\nprovider = \"custom:https://decisions.example.com/v1\"\n\
                        model = \"decider-1\"\noperations = [\"decision_endpoint\"]\n";
        let (status, reply) = put_file(&base, "config.toml", &format!("{before_disk}{remote}{declared}")).await;
        assert_eq!(status, 400, "unexpected response: {reply}");
        assert!(reply.contains("remote_enabled"), "{reply}");
        assert!(!reply.contains("decisions.example.com"), "{reply}");
        assert_eq!(disk(&state), before_disk);

        let enabled = format!("{before_disk}{remote}remote_enabled = true\n{declared}");
        let (status, reply) = put_file(&base, "config.toml", &enabled).await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("remote_decision_enabled"), "{reply}");
        assert_eq!(disk(&state), before_disk);

        let (status, reply) = put_file_confirmed(&base, "config.toml", &enabled).await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        server.abort();
    }

    // ── SECRETS-h (D-049) ──────────────────────────────────────────────────

    /// D-049 A, RED before SECRETS-h (R-SECRETSfg P16): settings that decide
    /// who is admitted inbound (the MCP JWT key, pairing, a channel's sender
    /// allow-list) changed as Low with 200 and no confirmation. After: 428
    /// naming `access_grant_changed` and the revision, nothing written; the
    /// confirmed retry bound to that revision applies it.
    #[tokio::test]
    async fn post_config_asks_to_confirm_an_inbound_trust_change() {
        const PEM: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAplanted\n-----END PUBLIC KEY-----\n";
        for (setup, body, path) in [
            (
                "",
                serde_json::json!({"mcp_server": {"jwt_public_key_pem": PEM}}),
                "mcp_server.jwt_public_key_pem",
            ),
            (
                "",
                serde_json::json!({"gateway": {"require_pairing": false}}),
                "gateway.require_pairing",
            ),
            (
                "",
                serde_json::json!({"mcp_server": {"require_auth": false}}),
                "mcp_server.require_auth",
            ),
            (
                "",
                serde_json::json!({"a2a": {"allowed_peer_issuers": ["spiffe://attacker"]}}),
                "a2a.allowed_peer_issuers[0]",
            ),
            (
                "[channels_config.telegram]\nbot_token = \"stored-bot-token\"\nallowed_users = [\"alice\"]\n",
                serde_json::json!({"channels_config": {"telegram": {"allowed_users": ["alice", "*"]}}}),
                "channels_config.telegram.allowed_users[1]",
            ),
        ] {
            let (_dir, state, base, server) = on_disk(|config| with_overlay(config, setup)).await;
            let before_disk = disk(&state);
            let revision = state.config.desired().source_revision.fingerprint_sha256.clone();

            let (status, reply) = post_config(&base, body.clone()).await;
            assert_eq!(status, 428, "{path}: unexpected response: {reply}");
            let json: serde_json::Value = serde_json::from_str(&reply).expect("test: json");
            assert_eq!(
                json.get("revision"),
                Some(&serde_json::json!(revision)),
                "{path}: {reply}"
            );
            assert!(reply.contains("access_grant_changed"), "{path}: {reply}");
            assert!(reply.contains(path), "{path}: {reply}");
            assert_eq!(disk(&state), before_disk, "{path}: nothing may be written");

            let (status, reply) = call_with_if_match(
                reqwest::Method::POST,
                &format!("{base}/api/config?confirm_high_risk=true"),
                Some(&format!("\"{revision}\"")),
                body,
            )
            .await;
            assert_eq!(status, 200, "{path}: unexpected response: {reply}");
            server.abort();
        }
    }

    /// D-049 B, RED before SECRETS-h (R-SECRETSfg P12, P13): a confirmation
    /// without a revision applied a high-risk change the caller had never
    /// been shown. After, on every write path: `confirm_high_risk=true`
    /// without `If-Match`, or with `If-Match: *`, answers 428 `if_match`
    /// with the revision, and nothing is written.
    #[tokio::test]
    async fn a_confirmation_without_a_revision_confirms_nothing() {
        let (_dir, state, base, server) =
            on_disk(|config| with_overlay(config, "[webhook]\ntoken = \"stored-admission\"\n")).await;
        let before_disk = disk(&state);
        let revision = state.config.desired().source_revision.fingerprint_sha256.clone();

        let secret = serde_json::json!({"slot": "webhook.token", "value": PLAIN});
        for if_match in [None, Some("*")] {
            let (status, reply) = call_with_if_match(
                reqwest::Method::PUT,
                &format!("{base}/api/config/secrets?confirm_high_risk=true"),
                if_match,
                secret.clone(),
            )
            .await;
            assert_eq!(status, 428, "secrets PUT, If-Match {if_match:?}: {reply}");
            let json: serde_json::Value = serde_json::from_str(&reply).expect("test: json");
            assert_eq!(json.get("required"), Some(&serde_json::json!("if_match")), "{reply}");
            assert_eq!(json.get("revision"), Some(&serde_json::json!(revision)), "{reply}");
            assert!(!reply.contains(PLAIN), "{reply}");

            let (status, reply) = call_with_if_match(
                reqwest::Method::POST,
                &format!("{base}/api/config?confirm_high_risk=true"),
                if_match,
                serde_json::json!({"api_key": PLAIN}),
            )
            .await;
            assert_eq!(status, 428, "POST, If-Match {if_match:?}: {reply}");
            assert!(reply.contains("if_match"), "{reply}");

            let content = format!("api_key = \"{PLAIN}\"\n{before_disk}");
            let (status, reply) = call_with_if_match(
                reqwest::Method::PUT,
                &format!("{base}/api/config/files/config.toml?confirm_high_risk=true"),
                if_match,
                serde_json::json!({ "content": content }),
            )
            .await;
            // With encryption on, a plaintext key is refused at the
            // boundary first (400); either way nothing is confirmed.
            assert!(
                status == 428 || status == 400,
                "raw PUT, If-Match {if_match:?}: {status} {reply}"
            );
            assert_eq!(
                disk(&state),
                before_disk,
                "If-Match {if_match:?}: nothing may be written"
            );
        }
        server.abort();
    }

    /// D-049 B: a raw write confirmed without a revision is refused with
    /// 428 `if_match` (encryption off, so the boundary lets plaintext pass).
    #[tokio::test]
    async fn a_raw_write_confirmed_without_a_revision_confirms_nothing() {
        let (_dir, state, base, server) = on_disk(|config| config.secrets.encrypt = false).await;
        let before_disk = disk(&state);
        let content = format!("api_key = \"{PLAIN}\"\n{before_disk}");
        let (status, reply) = call_with_if_match(
            reqwest::Method::PUT,
            &format!("{base}/api/config/files/config.toml?confirm_high_risk=true"),
            None,
            serde_json::json!({ "content": content }),
        )
        .await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("if_match"), "{reply}");
        assert_eq!(disk(&state), before_disk);
        server.abort();
    }

    /// D-049 B, RED before SECRETS-h: `POST /api/config` ignored `If-Match`.
    /// After: a revision other than the one the change is merged onto
    /// answers 409 and writes nothing, even for a low-risk change.
    #[tokio::test]
    async fn post_config_honours_if_match() {
        let (_dir, state, base, server) = on_disk(|_| {}).await;
        let before_disk = disk(&state);
        let stale = format!("\"{}\"", "0".repeat(64));
        let (status, reply) = call_with_if_match(
            reqwest::Method::POST,
            &format!("{base}/api/config"),
            Some(&stale),
            serde_json::json!({"default_temperature": 0.9}),
        )
        .await;
        assert_eq!(status, 409, "unexpected response: {reply}");
        assert_eq!(disk(&state), before_disk);

        let revision = format!("\"{}\"", state.config.desired().source_revision.fingerprint_sha256);
        let (status, reply) = call_with_if_match(
            reqwest::Method::POST,
            &format!("{base}/api/config"),
            Some(&revision),
            serde_json::json!({"default_temperature": 0.9}),
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        server.abort();
    }

    /// D-049 F, RED before SECRETS-h (R-SECRETSfg F-G): one secrets PUT
    /// re-pointed memory storage (`storage.provider.config.db_url`) with 200.
    /// After: the connection string is also an endpoint, so the change is
    /// 428 `endpoint_changed` until confirmed with the revision.
    #[tokio::test]
    async fn put_secret_asks_to_confirm_a_new_database_url() {
        // Reaches the endpoint's success log line, which another test captures.
        let _serial = fixture::LOG_CALLSITE_LOCK.lock().await;
        let (_dir, state, base, server) = on_disk(|config| {
            config.storage.provider.config.db_url = Some("postgres://u:stored@db-a.invalid/prx".into());
        })
        .await;
        let before_disk = disk(&state);
        let body = serde_json::json!({
            "slot": "storage.provider.config.db_url",
            "value": format!("postgres://u:{PLAIN}@db-b.invalid/prx"),
        });
        let (status, reply) = put_secret(&base, body.clone()).await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("endpoint_changed"), "{reply}");
        assert!(!reply.contains(PLAIN), "{reply}");
        assert_eq!(disk(&state), before_disk);

        let (status, reply) = put_secret_confirmed(&base, &state, body).await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        server.abort();
    }

    /// D-049 D, RED before SECRETS-h (R-SECRETSfg P18): `GET /api/mcp/servers`
    /// returned an MCP server URL with its `user:password@`.
    #[tokio::test]
    async fn mcp_server_status_never_shows_url_userinfo() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_from_overlay(
            dir.path(),
            "[mcp.servers.remote]\ntransport = \"http\"\nurl = \"https://mcp-user:userinfo-sentinel-mcp-status@mcp.invalid/mcp\"\n",
        );
        let (base, server) = serve(app_state(config)).await;
        let (status, body) = call(
            reqwest::Method::GET,
            &format!("{base}/api/mcp/servers"),
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(status, 200, "unexpected response: {body}");
        assert!(
            !body.contains("userinfo-sentinel"),
            "the MCP status leaked userinfo: {body}"
        );
        assert!(body.contains("https://[redacted]@mcp.invalid/mcp"), "{body}");
        server.abort();
    }

    /// D-049 amendment, through the HTTP surface: a widening (a user policy
    /// granting owner, a newly exposed MCP tool) answers 428
    /// `privilege_widened` and writes nothing; the narrowing direction of the
    /// same settings is low and applies without confirmation.
    #[tokio::test]
    async fn post_config_confirms_widenings_and_lets_narrowings_through() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.user_policies.push(crate::config::UserPolicyConfig {
                user_id: "u1".into(),
                role: "member".into(),
                projects: Vec::new(),
                visibility_ceiling: "private".into(),
                blocked_patterns: Vec::new(),
            });
        })
        .await;
        let before_disk = disk(&state);

        for body in [
            serde_json::json!({"user_policies": [{"user_id": "u1", "role": "owner"}]}),
            serde_json::json!({"mcp_server": {"exposed_tools": ["shell"]}}),
        ] {
            let (status, reply) = post_config(&base, body.clone()).await;
            assert_eq!(status, 428, "{body}: unexpected response: {reply}");
            assert!(reply.contains("privilege_widened"), "{body}: {reply}");
            assert_eq!(disk(&state), before_disk, "{body}: nothing may be written");
        }

        for body in [
            serde_json::json!({"user_policies": [{"user_id": "u1", "role": "guest"}]}),
            serde_json::json!({"mcp_server": {"exposed_tools": ["memory_search"]}}),
        ] {
            let (status, reply) = post_config(&base, body.clone()).await;
            assert_eq!(status, 200, "{body}: a narrowing is low: {reply}");
        }
        server.abort();
    }

    // ── SECRETS-i (D-050) ──────────────────────────────────────────────────

    /// D-050 item 1, RED before SECRETS-i (R-SECRETSfg P20): over
    /// `default = "deny"`, POSTing `"deny "` was Low for the plan and allow
    /// for the runtime. After: it is the same deny for both, and a value that
    /// is neither allow nor deny is refused with 400.
    #[tokio::test]
    async fn a_padded_scope_default_is_the_same_deny_for_plan_and_runtime() {
        let (_dir, state, base, server) =
            on_disk(|config| config.autonomy.scopes.default = crate::config::ScopeDefault::Deny).await;
        let (status, reply) =
            post_config(&base, serde_json::json!({"autonomy": {"scopes": {"default": "deny "}}})).await;
        assert_eq!(status, 200, "unexpected response: {reply}");
        let desired = state.config.desired();
        let policy =
            crate::security::SecurityPolicy::from_config(&desired.config.autonomy, std::path::Path::new("/tmp"));
        assert!(
            !policy.is_tool_allowed("shell", "uuid:someone", "signal", "direct"),
            "the runtime must still deny an unmatched tool"
        );

        let before_disk = disk(&state);
        let (status, reply) =
            post_config(&base, serde_json::json!({"autonomy": {"scopes": {"default": "allwo"}}})).await;
        assert_eq!(status, 400, "unexpected response: {reply}");
        assert_eq!(disk(&state), before_disk);

        let (status, reply) = post_config(
            &base,
            serde_json::json!({"autonomy": {"scopes": {"default": "allow "}}}),
        )
        .await;
        assert_eq!(status, 428, "deny to allow widens: {reply}");
        assert!(reply.contains("privilege_widened"), "{reply}");
        server.abort();
    }

    /// D-050 item 3, RED before SECRETS-i (R-SECRETSfg P23): with the stored
    /// configuration no longer loading, the plan compared against nothing,
    /// so a raw write that turned pairing off (and repaired the file) was Low
    /// and applied. After: every change is high risk with
    /// `baseline_unavailable` until the configuration loads again, and
    /// nothing is written without confirmation.
    #[tokio::test]
    async fn a_stored_configuration_that_does_not_load_makes_every_change_high_risk() {
        let (dir, state, base, server) = on_disk(|config| config.api_key = Some("sentinel-root-key".into())).await;
        let good = disk(&state);
        assert!(good.contains("require_pairing = true"), "test precondition: {good}");
        // The stored key no longer decrypts: the file is valid TOML, but the
        // configuration does not load.
        std::fs::remove_file(dir.path().join(".secret_key")).expect("test: remove the secret key");
        let before_disk = disk(&state);

        // A raw write that drops the undecryptable key and turns pairing off.
        let rewritten = good
            .lines()
            .filter(|line| !line.starts_with("api_key"))
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .replacen("require_pairing = true", "require_pairing = false", 1);
        let (status, reply) = put_file(&base, "config.toml", &rewritten).await;
        assert_eq!(status, 400, "omitting a stored key requires clear permission: {reply}");
        assert!(reply.contains("api_key"), "{reply}");
        assert!(reply.contains("PUT /api/config/secrets"), "{reply}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        let (status, reply) = put_file(&base, "config.toml?allow_secret_clear=true", &rewritten).await;
        assert_eq!(status, 428, "unexpected response: {reply}");
        assert!(reply.contains("baseline_unavailable"), "{reply}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        server.abort();
    }

    /// D-050 item 4, RED before SECRETS-i (R-SECRETSfg P24): tunnel exposure,
    /// the custom tunnel command and a raised autonomy level were Low.
    #[tokio::test]
    async fn exposure_and_execution_widenings_need_confirmation() {
        let (_dir, state, base, server) =
            on_disk(|config| config.autonomy.level = crate::security::AutonomyLevel::Supervised).await;
        let before_disk = disk(&state);
        for (body, reason) in [
            (
                serde_json::json!({"tunnel": {"provider": "tailscale", "tailscale": {"funnel": true}}}),
                "access_grant_changed",
            ),
            (
                serde_json::json!({"tunnel": {"custom": {"start_command": "/tmp/anything"}}}),
                "privilege_widened",
            ),
            (serde_json::json!({"autonomy": {"level": "full"}}), "privilege_widened"),
        ] {
            let (status, reply) = post_config(&base, body.clone()).await;
            assert_eq!(status, 428, "{body}: unexpected response: {reply}");
            assert!(reply.contains(reason), "{body}: {reply}");
            assert_eq!(disk(&state), before_disk, "{body}: nothing may be written");
        }
        server.abort();
    }

    // ── SECRETS-j (D-051) ──────────────────────────────────────────────────

    /// D-051, RED before SECRETS-j (R-SECRETSfg P27): removing the sandbox,
    /// opening its network to the host and exposing operator tools to
    /// channel sessions were Low with 200. After: each answers 428
    /// `privilege_widened` and nothing is written; the reverse direction of
    /// the same setting applies without confirmation.
    #[tokio::test]
    async fn containment_widenings_need_confirmation() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.runtime.kind = "docker".into();
            config.browser.allowed_domains = vec!["example.com".into()];
            config.runtime.docker.allowed_workspace_roots = vec!["/srv/a".into()];
            config.runtime.docker.mount_workspace = false;
            config.tool_tiering.always_exclude = vec!["shell".into()];
            config.tool_tiering.always_include = vec!["shell".into()];
        })
        .await;
        let before_disk = disk(&state);
        for body in [
            serde_json::json!({"runtime": {"kind": "native"}}),
            serde_json::json!({"runtime": {"docker": {"network": "host"}}}),
            serde_json::json!({"tool_tiering": {"channel_exclude": []}}),
            serde_json::json!({"runtime": {"docker": {"read_only_rootfs": false}}}),
            serde_json::json!({"runtime": {"docker": {"allowed_workspace_roots": ["/srv/a", "/"]}}}),
            serde_json::json!({"browser": {"allowed_domains": ["example.com", "*"]}}),
            // D-052 (R-SECRETSj P27b): the same exposure through always_include.
            serde_json::json!({"tool_tiering": {"always_include": ["proxy_config"]}}),
            serde_json::json!({"runtime": {"docker": {"mount_workspace": true}}}),
            serde_json::json!({"security": {"audit": {"max_size_mb": 1}}}),
            // D-055 (R-SECRETSj K-1): always_exclude wins over always_include.
            serde_json::json!({"tool_tiering": {"always_exclude": []}}),
        ] {
            let (status, reply) = post_config(&base, body.clone()).await;
            assert_eq!(status, 428, "{body}: unexpected response: {reply}");
            assert!(reply.contains("privilege_widened"), "{body}: {reply}");
            assert_eq!(disk(&state), before_disk, "{body}: nothing may be written");
        }

        // Hiding one more tool from channel sessions narrows: no confirmation.
        let mut narrower = state.config.desired().config.tool_tiering.channel_exclude.clone();
        narrower.push("shell".into());
        let (status, reply) = post_config(
            &base,
            serde_json::json!({"tool_tiering": {"channel_exclude": narrower}}),
        )
        .await;
        assert_eq!(status, 200, "a narrowing needs no confirmation: {reply}");
        server.abort();
    }

    /// RED before PMMDtail-f (D-078 T-2): removing a route that the default
    /// and an agent name was not refused up front (the default was refused
    /// only at activation, the agent's dangling binding was written). Now a
    /// 400 lists every selection that names it, and nothing is written.
    #[tokio::test]
    async fn post_config_removing_a_named_route_lists_every_dependent() {
        let (_dir, state, base, server) = on_disk(|config| {
            config.model_routes = vec![crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "fast".into(),
                provider: "openrouter".into(),
                model: "vendor/model-a".into(),
                api_key: None,
                credential_ref: None,
            }];
            config.default_model = Some("hint:fast".into());
            config.agents.insert(
                "helper".into(),
                serde_json::from_value(serde_json::json!({"provider": "openrouter", "model": "hint:fast"}))
                    .expect("test: agent"),
            );
        })
        .await;
        let before_disk = disk(&state);

        let (status, body) = post_config(&base, serde_json::json!({"model_routes": []})).await;

        assert_eq!(status, 400, "unexpected response: {body}");
        assert!(body.contains("default model refused"), "{body}");
        assert!(body.contains("agent `helper` refused"), "{body}");
        assert_eq!(disk(&state), before_disk, "nothing may be written");
        server.abort();
    }
}

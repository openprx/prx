//! The decision-plane configuration, `[causal_tree]` (CDP-PLAN 1.7; CDP 13.1;
//! D-040).
//!
//! - `enabled` is a bool, not a mode, and is always written. It defaults to
//!   `true` together with the versioned init template (CDP-31; DECIDED X-2).
//!   Explicit versioned disables stay disabled; remote processing stays opt-in.
//! - [`CausalTreeConfig::validate`] refuses what can never run: bad
//!   endpoints, hosted backends before CDP-13, unpinned calibration, limits
//!   out of range. Weights are only range-checked here; the scorer owns their
//!   defaults and budget (D-040 OQ-A7).
//! - [`migrate_legacy`] maps the keys a `Config::save` wrote before CDP-03 to
//!   their new homes on every load, keeping operator values and dropping
//!   legacy defaults, and refuses what has no faithful home.
//! - Historical file keys migrate before deserialization; runtime consumers
//!   use only the canonical configuration fields.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::decision::BackendKind;
use crate::decision::loopback::{EndpointError, EndpointLocality, check_endpoint};
use crate::decision::model_pin::is_unpinned_model;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_MS: u32 = 250;
const DEFAULT_MAX_REQUEST_BYTES: u32 = 64 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: u32 = 256 * 1024;
const DEFAULT_FAILURE_THRESHOLD: u32 = 5;
const DEFAULT_COOLDOWN_SECS: u32 = 60;
const DEFAULT_MAX_CANDIDATES: u8 = 12;
const DEFAULT_MAX_DECISION_HOPS: u8 = 2;
const DEFAULT_EXTRA_LATENCY_BUDGET_MS: u64 = 500;
const DEFAULT_COMMIT_THRESHOLD: f32 = 0.62;
const DEFAULT_MINIMUM_CHOICE_CONFIDENCE: f64 = 0.55;
const DEFAULT_MINIMUM_TOP_MARGIN: f64 = 0.10;
const DEFAULT_EXTRA_TOKEN_RATIO_LIMIT: f32 = 0.35;
const DEFAULT_REHEARSAL_TIMEOUT_MS: u32 = 100;
/// The `[causal_tree]` schema version this release reads and writes: 1 is
/// the CDP-PLAN 1.7 schema (D-073). A pre-1.7 file carries no version.
pub const CAUSAL_CONFIG_SCHEMA_VERSION: u32 = 1;
/// The accepted request and response body limits.
const BODY_BYTES: std::ops::RangeInclusive<u32> = 4 * 1024..=1024 * 1024;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Which decision backend runs: `auto` (the factory's order) or one kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub enum BackendSelection {
    /// The factory's order: the configured backend if any, then builtin.
    #[default]
    Auto,
    /// Exactly this backend kind.
    Fixed(BackendKind),
}

impl BackendSelection {
    /// The configured spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(kind) => kind.as_str(),
        }
    }
}

impl TryFrom<String> for BackendSelection {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(match value.trim() {
            "auto" => Self::Auto,
            "builtin" => Self::Fixed(BackendKind::Builtin),
            "compatible_http" => Self::Fixed(BackendKind::CompatibleHttp),
            "typesafe" => Self::Fixed(BackendKind::TypeSafe),
            "openrouter" => Self::Fixed(BackendKind::OpenRouter),
            _ => {
                return Err(
                    "causal_tree.backend.kind must be one of auto, builtin, compatible_http, typesafe, openrouter"
                        .to_string(),
                );
            }
        })
    }
}

impl From<BackendSelection> for String {
    fn from(selection: BackendSelection) -> Self {
        selection.as_str().to_string()
    }
}

/// Which credential a decision backend uses. A reference, never a secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub enum CredentialRefSetting {
    /// Nothing configured (`""`).
    #[default]
    Unset,
    /// Deliberately anonymous (`"none"`).
    None,
    /// A credential reference of the auth-profile store, by id
    /// (`cred-<name>`, PMM-D-6); its owner must be the backend's provider.
    Profile(String),
}

impl TryFrom<String> for CredentialRefSetting {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.trim() {
            "" => Ok(Self::Unset),
            "none" => Ok(Self::None),
            id if crate::config::manage::credentials::CredentialReferences::reference(id).is_ok() => {
                Ok(Self::Profile(id.to_string()))
            }
            // The value is never echoed: whatever was written here might be a
            // secret pasted into the wrong key.
            _ => Err(
                "causal_tree.backend.credential_ref is not a credential reference: it is \"\", \"none\" or \
                 a credential id `cred-<name>` (store one with `prx causal config set-backend \
                 --credential-stdin`)"
                    .to_string(),
            ),
        }
    }
}

impl From<CredentialRefSetting> for String {
    fn from(setting: CredentialRefSetting) -> Self {
        match setting {
            CredentialRefSetting::Unset => Self::new(),
            CredentialRefSetting::None => "none".to_string(),
            CredentialRefSetting::Profile(id) => id,
        }
    }
}

/// The per-backend circuit breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures before the breaker opens (1 to 100).
    pub failure_threshold: u32,
    /// Seconds the breaker stays open (1 to 3600).
    pub cooldown_secs: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        }
    }
}

/// The decision backend (`[causal_tree.backend]`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DecisionBackendConfig {
    /// `auto`, `builtin`, `compatible_http`, or a hosted service:
    /// `typesafe` or `openrouter` (CDP-13).
    pub kind: BackendSelection,
    /// Whether decision state may leave this machine. Default `false`.
    pub remote_enabled: bool,
    /// The System One endpoint (`compatible_http`, required), or the base
    /// URL that replaces a hosted service's (optional, COORD-6: a hosted
    /// backend stays remote whatever the URL). https, or http on a literal
    /// loopback address; never user information.
    pub endpoint: String,
    /// The model the endpoint serves; one exact version for a hosted
    /// service.
    pub model: String,
    /// The exact model snapshot a hosted service must answer with (D-043
    /// C-8). OpenRouter requires the exact dated snapshot returned for the
    /// configured model; for TypeSafe it defaults to `model`. Refused for the
    /// other kinds.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pinned_snapshot: String,
    /// The credential reference: `""`, `"none"` or `cred-<name>`.
    pub credential_ref: CredentialRefSetting,
    /// Transport timeout (10 ms up to the extra latency budget).
    pub timeout_ms: u32,
    /// Largest request body (4 KiB to 1 MiB).
    pub max_request_bytes: u32,
    /// Largest response body (4 KiB to 1 MiB).
    pub max_response_bytes: u32,
    /// The circuit breaker.
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for DecisionBackendConfig {
    fn default() -> Self {
        Self {
            kind: BackendSelection::Auto,
            remote_enabled: false,
            endpoint: String::new(),
            model: String::new(),
            pinned_snapshot: String::new(),
            credential_ref: CredentialRefSetting::Unset,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

impl fmt::Debug for DecisionBackendConfig {
    /// The endpoint prints with any URL user information redacted: it is
    /// refused by validation, but a configuration can be printed before it
    /// is validated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionBackendConfig")
            .field("kind", &self.kind)
            .field("remote_enabled", &self.remote_enabled)
            .field("endpoint", &crate::security::redaction::redact_secrets(&self.endpoint))
            .field("model", &self.model)
            .field("pinned_snapshot", &self.pinned_snapshot)
            .field("credential_ref", &self.credential_ref)
            .field("timeout_ms", &self.timeout_ms)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("circuit_breaker", &self.circuit_breaker)
            .finish()
    }
}

/// A calibration applied from an evaluation report: the backend kind, the
/// endpoint and the exact pinned model it was applied to
/// (`[causal_tree.policy.calibration_profile]`). The endpoint is part of it
/// (D-094 C-7): two servers that serve one model name do not share a
/// calibration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "CalibrationProfileSetting", into = "CalibrationProfileSetting")]
#[schemars(with = "CalibrationProfileSetting")]
pub struct CalibrationProfile {
    /// The backend kind the calibration was measured on.
    pub kind: BackendKind,
    /// The configured endpoint it was applied to, trimmed; empty for a
    /// backend without one.
    pub endpoint: String,
    /// The exact model it was measured on.
    pub model: String,
}

/// `[causal_tree.policy.calibration_profile]` as written.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalibrationProfileSetting {
    /// The backend kind.
    pub kind: String,
    /// The endpoint; empty for a backend without one.
    #[serde(default)]
    pub endpoint: String,
    /// The exact pinned model.
    pub model: String,
}

/// Why a calibration profile is refused; it never repeats a value.
const MALFORMED_PROFILE: &str = "causal_tree.policy.calibration_profile needs a backend kind and an exact model \
                                 version, never an alias or a `latest` spelling";

impl CalibrationProfile {
    /// The backend an evaluation report says it measured:
    /// `"<backend kind>:<pinned model>"`.
    ///
    /// # Errors
    ///
    /// When `measured` is not that, or its model is not pinned.
    pub fn measured(measured: &str) -> Result<(BackendKind, String), String> {
        let (kind, model) = measured.trim().split_once(':').ok_or(MALFORMED_PROFILE)?;
        Self::checked(kind, model)
    }

    fn checked(kind: &str, model: &str) -> Result<(BackendKind, String), String> {
        let BackendSelection::Fixed(kind) =
            BackendSelection::try_from(kind.trim().to_string()).map_err(|_| MALFORMED_PROFILE.to_string())?
        else {
            return Err(MALFORMED_PROFILE.to_string());
        };
        let model = model.trim();
        if is_unpinned_model(model) {
            return Err(MALFORMED_PROFILE.to_string());
        }
        Ok((kind, model.to_string()))
    }
}

impl TryFrom<CalibrationProfileSetting> for CalibrationProfile {
    type Error = String;

    fn try_from(setting: CalibrationProfileSetting) -> Result<Self, Self::Error> {
        let (kind, model) = Self::checked(&setting.kind, &setting.model)?;
        Ok(Self {
            kind,
            endpoint: setting.endpoint.trim().to_string(),
            model,
        })
    }
}

impl From<CalibrationProfile> for CalibrationProfileSetting {
    fn from(profile: CalibrationProfile) -> Self {
        Self {
            kind: profile.kind.as_str().to_string(),
            endpoint: profile.endpoint,
            model: profile.model,
        }
    }
}

/// Decision policy (`[causal_tree.policy]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CausalPolicy {
    /// The most candidates a decision considers (1 to 32).
    pub max_candidates: u8,
    /// The most decision hops in one turn (1 to 4).
    pub max_decision_hops: u8,
    /// The extra latency one turn may spend deciding (50 to 5000 ms).
    pub extra_latency_budget_ms: u64,
    /// Internal presence bit populated by `Config::load_from_path`. A stored
    /// value equal to the default is still an operator value and must not be
    /// replaced by a catalog transport recommendation.
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub extra_latency_budget_ms_explicit: bool,
    /// Minimum score to commit a candidate.
    pub commit_threshold: f32,
    /// Minimum confidence of a Choice answer.
    pub minimum_choice_confidence: f64,
    /// Minimum margin between the best two options (0 to 0.5).
    pub minimum_top_margin: f64,
    /// Maximum ratio of extra tokens to the turn's baseline tokens.
    pub extra_token_ratio_limit: f32,
    /// The calibration in force, if an operator applied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_profile: Option<CalibrationProfile>,
}

impl Default for CausalPolicy {
    fn default() -> Self {
        Self {
            max_candidates: DEFAULT_MAX_CANDIDATES,
            max_decision_hops: DEFAULT_MAX_DECISION_HOPS,
            extra_latency_budget_ms: DEFAULT_EXTRA_LATENCY_BUDGET_MS,
            extra_latency_budget_ms_explicit: false,
            commit_threshold: DEFAULT_COMMIT_THRESHOLD,
            minimum_choice_confidence: DEFAULT_MINIMUM_CHOICE_CONFIDENCE,
            minimum_top_margin: DEFAULT_MINIMUM_TOP_MARGIN,
            extra_token_ratio_limit: DEFAULT_EXTRA_TOKEN_RATIO_LIMIT,
            calibration_profile: None,
        }
    }
}

impl CausalPolicy {
    /// Whether every configured key holds its default.
    fn has_default_settings(&self) -> bool {
        let defaults = Self::default();
        self.max_candidates == defaults.max_candidates
            && self.max_decision_hops == defaults.max_decision_hops
            && self.extra_latency_budget_ms == defaults.extra_latency_budget_ms
            && self.commit_threshold.to_bits() == defaults.commit_threshold.to_bits()
            && self.minimum_choice_confidence.to_bits() == defaults.minimum_choice_confidence.to_bits()
            && self.minimum_top_margin.to_bits() == defaults.minimum_top_margin.to_bits()
            && self.extra_token_ratio_limit.to_bits() == defaults.extra_token_ratio_limit.to_bits()
            && self.calibration_profile.is_none()
    }
}

/// Rehearsal is read-only (CDP 9.3): the only accepted mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyRehearsal {
    #[default]
    ReadOnly,
}

/// Rehearsal (`[causal_tree.rehearsal]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RehearsalConfig {
    /// One rehearsal's timeout (at most the extra latency budget).
    pub timeout_ms: u32,
    /// Always `read_only`.
    pub side_effect_mode: ReadOnlyRehearsal,
}

impl Default for RehearsalConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_REHEARSAL_TIMEOUT_MS,
            side_effect_mode: ReadOnlyRehearsal::ReadOnly,
        }
    }
}

/// Scorer weight overrides; each unset weight takes the scorer's default
/// (D-040 OQ-A7: the scorer owns the defaults and the budget).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WeightOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_choice: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_applicable: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_success: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_gain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_risk: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_uncertainty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_latency: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w_rehearsal: Option<f64>,
}

impl WeightOverrides {
    /// Every weight, set or not.
    const fn values(&self) -> [Option<f64>; 9] {
        [
            self.w_choice,
            self.w_applicable,
            self.w_success,
            self.w_gain,
            self.w_risk,
            self.w_uncertainty,
            self.w_cost,
            self.w_latency,
            self.w_rehearsal,
        ]
    }
}

/// Scoring (`[causal_tree.scoring]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ScoringConfig {
    /// The scorer version the weights were chosen for. When set, a scorer of
    /// another version refuses the configuration rather than apply weights
    /// chosen for different scoring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Weight overrides.
    pub weights: WeightOverrides,
}

/// The decision store (`[causal_tree.store]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DecisionStoreConfig {
    /// Days to keep records (0 follows the fitness retention; at most 3650).
    pub retention_days: u32,
    /// Whether records keep the redacted state projection.
    pub record_redacted_projection: bool,
}

/// Whether exploration runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationMode {
    #[default]
    Off,
    /// Exploration only among read-only candidates.
    ReadOnly,
}

/// Evaluation (`[causal_tree.evaluation]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct EvaluationConfig {
    /// `off` or `read_only`.
    pub exploration: ExplorationMode,
    /// The exploration rate (0 to 0.10, non-zero only with `read_only`).
    pub exploration_rate: f64,
}

/// The `[causal_tree]` section.
///
/// File and JSON reloads share the same canonical keys and defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "CausalTreeConfigWire")]
#[schemars(with = "CausalTreeConfigWire")]
pub struct CausalTreeConfig {
    /// The schema of this section: [`CAUSAL_CONFIG_SCHEMA_VERSION`] for the
    /// CDP-PLAN 1.7 schema. Always written; a file without it predates that
    /// schema, and only such a file gets the legacy rules of
    /// [`migrate_legacy`] that reinterpret a value (D-073).
    pub schema_version: u32,
    /// Whether the decision plane runs. Always written, so an explicit
    /// disable can never be mistaken for a legacy default (DECIDED X-3).
    /// Default `true`; remote processing requires separate explicit opt-in.
    pub enabled: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub backend: DecisionBackendConfig,
    #[serde(skip_serializing_if = "CausalPolicy::has_default_settings")]
    pub policy: CausalPolicy,
    #[serde(skip_serializing_if = "is_default")]
    pub rehearsal: RehearsalConfig,
    #[serde(skip_serializing_if = "is_default")]
    pub scoring: ScoringConfig,
    #[serde(skip_serializing_if = "is_default")]
    pub store: DecisionStoreConfig,
    #[serde(skip_serializing_if = "is_default")]
    pub evaluation: EvaluationConfig,
}

impl Default for CausalTreeConfig {
    fn default() -> Self {
        Self {
            schema_version: CAUSAL_CONFIG_SCHEMA_VERSION,
            enabled: true,
            backend: DecisionBackendConfig::default(),
            policy: CausalPolicy::default(),
            rehearsal: RehearsalConfig::default(),
            scoring: ScoringConfig::default(),
            store: DecisionStoreConfig::default(),
            evaluation: EvaluationConfig::default(),
        }
    }
}

/// The configured keys of `[causal_tree]`, as read from a file or JSON.
#[derive(Deserialize, JsonSchema)]
#[serde(default)]
#[schemars(rename = "CausalTreeConfig")]
pub struct CausalTreeConfigWire {
    /// The schema of this section; 1 is the CDP-PLAN 1.7 schema. Always
    /// written; a file without it predates that schema.
    schema_version: u32,
    /// Whether the decision plane runs. Default `true`; explicit false is preserved.
    enabled: bool,
    backend: DecisionBackendConfig,
    policy: CausalPolicy,
    rehearsal: RehearsalConfig,
    scoring: ScoringConfig,
    store: DecisionStoreConfig,
    evaluation: EvaluationConfig,
}

impl Default for CausalTreeConfigWire {
    fn default() -> Self {
        let defaults = CausalTreeConfig::default();
        Self {
            schema_version: defaults.schema_version,
            enabled: defaults.enabled,
            backend: defaults.backend,
            policy: defaults.policy,
            rehearsal: defaults.rehearsal,
            scoring: defaults.scoring,
            store: defaults.store,
            evaluation: defaults.evaluation,
        }
    }
}

impl From<CausalTreeConfigWire> for CausalTreeConfig {
    fn from(wire: CausalTreeConfigWire) -> Self {
        Self {
            schema_version: wire.schema_version,
            enabled: wire.enabled,
            backend: wire.backend,
            policy: wire.policy,
            rehearsal: wire.rehearsal,
            scoring: wire.scoring,
            store: wire.store,
            evaluation: wire.evaluation,
        }
    }
}

/// Whether `value` equals its type's default, so saving it writes nothing
/// and a later default change reaches it.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// The longest `pinned_snapshot`, like every model identity claim.
const MAX_SNAPSHOT_CHARS: usize = 128;

/// Why a `[causal_tree]` section cannot run. Each message names the key and
/// the rule, and never repeats a value that could be a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CausalConfigError {
    #[error("causal_tree.backend.endpoint {0}")]
    Endpoint(EndpointError),
    #[error("causal_tree.backend.kind = \"compatible_http\" needs causal_tree.backend.endpoint")]
    EndpointRequired,
    #[error(
        "causal_tree.backend.kind = \"{0}\" is a hosted decision service: causal_tree.backend.model must name one \
         exact version, never an alias or a `latest` spelling"
    )]
    HostedModelNotPinned(&'static str),
    #[error(
        "causal_tree.backend.kind = \"openrouter\" needs causal_tree.backend.pinned_snapshot: the exact dated \
         snapshot OpenRouter answers with for causal_tree.backend.model"
    )]
    SnapshotRequired,
    #[error(
        "causal_tree.backend.pinned_snapshot must name one exact version in at most 128 characters, never an alias \
         or a `latest` spelling"
    )]
    SnapshotInvalid,
    #[error("causal_tree.backend.pinned_snapshot applies to the hosted kinds only (typesafe, openrouter)")]
    SnapshotNotHosted,
    #[error(
        "causal_tree.policy.calibration_profile needs causal_tree.backend.kind, causal_tree.backend.endpoint and a \
         pinned causal_tree.backend.model that match the profile"
    )]
    CalibrationNotPinned,
    #[error(
        "causal_tree.backend.endpoint is a remote endpoint; remote decision processing needs \
         causal_tree.backend.remote_enabled = true (a literal loopback endpoint does not)"
    )]
    RemoteNotEnabled,
    #[error("{key} must be {rule}")]
    OutOfRange { key: &'static str, rule: &'static str },
}

const fn ensure(condition: bool, key: &'static str, rule: &'static str) -> Result<(), CausalConfigError> {
    if condition {
        Ok(())
    } else {
        Err(CausalConfigError::OutOfRange { key, rule })
    }
}

fn unit_interval(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

impl CausalTreeConfig {
    /// Refuse a section that can never run (SP-CDP-A 5.3 under D-040).
    ///
    /// # Errors
    ///
    /// The first [`CausalConfigError`] in the order of the rules.
    pub fn validate(&self) -> Result<(), CausalConfigError> {
        let backend = &self.backend;
        let policy = &self.policy;

        // 0. A file written by a newer schema is not guessed at.
        ensure(
            (1..=CAUSAL_CONFIG_SCHEMA_VERSION).contains(&self.schema_version),
            "causal_tree.schema_version",
            "1; a newer schema needs a newer prx",
        )?;
        let hosted = match backend.kind {
            BackendSelection::Fixed(kind @ (BackendKind::TypeSafe | BackendKind::OpenRouter)) => Some(kind),
            _ => None,
        };
        // 1-2. The endpoint.
        if !backend.endpoint.trim().is_empty() {
            let locality = check_endpoint(&backend.endpoint).map_err(CausalConfigError::Endpoint)?;
            // SP-CDP-C V2.05, CXR 4.4: decision state leaves the machine only
            // with the operator's remote switch. A hosted service is remote
            // whatever its URL, and the backend order leaves it out until the
            // switch is on (COORD-6).
            if locality == EndpointLocality::Remote && !backend.remote_enabled && hosted.is_none() {
                return Err(CausalConfigError::RemoteNotEnabled);
            }
        } else if backend.kind == BackendSelection::Fixed(BackendKind::CompatibleHttp) {
            return Err(CausalConfigError::EndpointRequired);
        }
        // 3. A hosted service answers with one pinned model, and the exact
        // snapshot is pinned too (D-043 C-8).
        let snapshot = backend.pinned_snapshot.trim();
        match hosted {
            Some(kind) => {
                if is_unpinned_model(&backend.model) {
                    return Err(CausalConfigError::HostedModelNotPinned(kind.as_str()));
                }
                if kind == BackendKind::OpenRouter && snapshot.is_empty() {
                    return Err(CausalConfigError::SnapshotRequired);
                }
                let invalid = is_unpinned_model(snapshot)
                    || snapshot.chars().count() > MAX_SNAPSHOT_CHARS
                    || snapshot.chars().any(char::is_control);
                if !snapshot.is_empty() && invalid {
                    return Err(CausalConfigError::SnapshotInvalid);
                }
            }
            None if !snapshot.is_empty() => return Err(CausalConfigError::SnapshotNotHosted),
            None => {}
        }
        // 4. A calibration needs the exact pinned model it was measured on:
        // never an alias or a `latest` spelling (D-074), whatever the kind.
        if let Some(profile) = &policy.calibration_profile {
            let pinned = !is_unpinned_model(&backend.model)
                && backend.model.trim() == profile.model
                && backend.endpoint.trim() == profile.endpoint;
            if backend.kind != BackendSelection::Fixed(profile.kind) || !pinned {
                return Err(CausalConfigError::CalibrationNotPinned);
            }
        }
        // 5. Time.
        let budget = policy.extra_latency_budget_ms;
        ensure(
            (50..=5000).contains(&budget),
            "causal_tree.policy.extra_latency_budget_ms",
            "between 50 and 5000",
        )?;
        ensure(
            backend.timeout_ms >= 10 && u64::from(backend.timeout_ms) <= budget,
            "causal_tree.backend.timeout_ms",
            "at least 10 and at most causal_tree.policy.extra_latency_budget_ms",
        )?;
        ensure(
            u64::from(self.rehearsal.timeout_ms) <= budget,
            "causal_tree.rehearsal.timeout_ms",
            "at most causal_tree.policy.extra_latency_budget_ms",
        )?;
        // 6. Counts.
        ensure(
            (1..=32).contains(&policy.max_candidates),
            "causal_tree.policy.max_candidates",
            "between 1 and 32",
        )?;
        ensure(
            (1..=4).contains(&policy.max_decision_hops),
            "causal_tree.policy.max_decision_hops",
            "between 1 and 4",
        )?;
        // 7. Thresholds and weights.
        ensure(
            unit_interval(f64::from(policy.commit_threshold)),
            "causal_tree.policy.commit_threshold",
            "a number between 0 and 1",
        )?;
        ensure(
            unit_interval(policy.minimum_choice_confidence),
            "causal_tree.policy.minimum_choice_confidence",
            "a number between 0 and 1",
        )?;
        ensure(
            policy.minimum_top_margin.is_finite() && (0.0..=0.5).contains(&policy.minimum_top_margin),
            "causal_tree.policy.minimum_top_margin",
            "a number between 0 and 0.5",
        )?;
        ensure(
            policy.extra_token_ratio_limit.is_finite() && policy.extra_token_ratio_limit > 0.0,
            "causal_tree.policy.extra_token_ratio_limit",
            "a finite number greater than 0 (a zero ratio refuses every Agent turn)",
        )?;
        for weight in self.scoring.weights.values() {
            ensure(
                weight.is_none_or(unit_interval),
                "causal_tree.scoring.weights",
                "numbers between 0 and 1",
            )?;
        }
        // 8. Exploration.
        let rate = self.evaluation.exploration_rate;
        ensure(
            rate.is_finite() && (0.0..=0.10).contains(&rate),
            "causal_tree.evaluation.exploration_rate",
            "between 0 and 0.10",
        )?;
        ensure(
            rate == 0.0 || self.evaluation.exploration == ExplorationMode::ReadOnly,
            "causal_tree.evaluation.exploration_rate",
            "0 unless causal_tree.evaluation.exploration = \"read_only\"",
        )?;
        // 9. Breaker, retention and sizes.
        ensure(
            (1..=100).contains(&backend.circuit_breaker.failure_threshold),
            "causal_tree.backend.circuit_breaker.failure_threshold",
            "between 1 and 100",
        )?;
        ensure(
            (1..=3600).contains(&backend.circuit_breaker.cooldown_secs),
            "causal_tree.backend.circuit_breaker.cooldown_secs",
            "between 1 and 3600",
        )?;
        ensure(
            self.store.retention_days <= 3650,
            "causal_tree.store.retention_days",
            "at most 3650",
        )?;
        ensure(
            BODY_BYTES.contains(&backend.max_request_bytes),
            "causal_tree.backend.max_request_bytes",
            "between 4096 and 1048576",
        )?;
        ensure(
            BODY_BYTES.contains(&backend.max_response_bytes),
            "causal_tree.backend.max_response_bytes",
            "between 4096 and 1048576",
        )
    }
}

// ---------------------------------------------------------------------------
// Legacy migration
// ---------------------------------------------------------------------------

/// What [`migrate_legacy`] did to the legacy keys of one configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Legacy keys whose operator value moved to a new home.
    pub moved: Vec<&'static str>,
    /// Legacy keys that held the legacy default and were dropped, so they
    /// take the new default.
    pub dropped: Vec<&'static str>,
}

impl MigrationReport {
    /// Whether the configuration held any legacy key.
    #[must_use]
    pub const fn found_legacy_keys(&self) -> bool {
        !self.moved.is_empty() || !self.dropped.is_empty()
    }
}

/// A legacy key with no faithful new home, a legacy value its new key
/// cannot hold, or a legacy key next to its new home. Names the file that
/// holds the key, the key, what is wrong and the fix; never the value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}: causal_tree.{key} {problem}; {remedy}")]
pub struct MigrationError {
    pub file: String,
    pub key: &'static str,
    pub problem: String,
    pub remedy: String,
}

/// Where a `[causal_tree]` key was written: given the key's full path
/// (`["causal_tree", "policy", "max_branches"]`), the file that holds it.
/// The loader passes one that searches `config.toml` and its `config.d`
/// fragments; `None` when the key cannot be placed.
pub type KeyLocator<'a> = &'a dyn Fn(&[&str]) -> Option<std::path::PathBuf>;

/// The legacy-only keys whose presence marks a `[causal_tree]` table written
/// by a `Config::save` before CDP-03 (DECIDED X-3).
const LEGACY_ONLY_KEYS: [&[&str]; 8] = [
    &["w_confidence"],
    &["write_decision_log"],
    &["write_metrics"],
    &["policy", "max_branches"],
    &["policy", "rehearsal_timeout_ms"],
    &["policy", "default_side_effect_mode"],
    &["policy", "circuit_breaker_threshold"],
    &["policy", "circuit_breaker_cooldown_secs"],
];

/// Keys that were accepted but never read at runtime. They are dropped with
/// the legacy keys' report in any `[causal_tree]` table, whatever its schema
/// version, so a stored file keeps loading: the decision log and metrics
/// switches (`write_decision_log`, `write_metrics` and their
/// `observability.*` successors).
const RETIRED_KEYS: [(&[&str], &str); 4] = [
    (&["write_decision_log"], "write_decision_log"),
    (&["write_metrics"], "write_metrics"),
    (&["observability", "decision_log"], "observability.decision_log"),
    (&["observability", "metrics"], "observability.metrics"),
];

/// Where a legacy key's operator value goes.
enum Home {
    /// It kept its name; only its legacy default is reinterpreted, and only
    /// in a file without `schema_version`.
    SameKey,
    /// It moves to `path` (spelled `name`); an integer must fit `max`.
    Moves {
        path: &'static [&'static str],
        name: &'static str,
        max: Option<i64>,
    },
    /// It has no faithful home; the remedy says what to do instead.
    Nowhere { remedy: &'static str },
}

/// A legacy key: where it lived, its legacy default, and its home.
struct LegacyKey {
    old: &'static [&'static str],
    name: &'static str,
    default: fn(&toml::Value) -> bool,
    home: Home,
}

const LEGACY_KEYS: [LegacyKey; 9] = [
    LegacyKey {
        old: &["policy", "max_branches"],
        name: "policy.max_branches",
        default: |value| value.as_integer() == Some(3),
        home: Home::Moves {
            path: &["policy", "max_candidates"],
            name: "policy.max_candidates",
            max: Some(255),
        },
    },
    LegacyKey {
        old: &["policy", "extra_latency_budget_ms"],
        name: "policy.extra_latency_budget_ms",
        default: |value| value.as_integer() == Some(300),
        home: Home::SameKey,
    },
    LegacyKey {
        old: &["policy", "rehearsal_timeout_ms"],
        name: "policy.rehearsal_timeout_ms",
        default: |value| value.as_integer() == Some(5000),
        home: Home::Moves {
            path: &["rehearsal", "timeout_ms"],
            name: "rehearsal.timeout_ms",
            max: Some(4_294_967_295),
        },
    },
    LegacyKey {
        old: &["policy", "default_side_effect_mode"],
        name: "policy.default_side_effect_mode",
        default: |value| value.as_str() == Some("ReadOnly"),
        home: Home::Nowhere {
            remedy: "rehearsal is read-only (CDP 9.3); remove the key",
        },
    },
    LegacyKey {
        old: &["policy", "circuit_breaker_threshold"],
        name: "policy.circuit_breaker_threshold",
        default: |value| value.as_integer() == Some(5),
        home: Home::Moves {
            path: &["backend", "circuit_breaker", "failure_threshold"],
            name: "backend.circuit_breaker.failure_threshold",
            max: Some(4_294_967_295),
        },
    },
    LegacyKey {
        old: &["policy", "circuit_breaker_cooldown_secs"],
        name: "policy.circuit_breaker_cooldown_secs",
        default: |value| value.as_integer() == Some(60),
        home: Home::Moves {
            path: &["backend", "circuit_breaker", "cooldown_secs"],
            name: "backend.circuit_breaker.cooldown_secs",
            max: Some(4_294_967_295),
        },
    },
    LegacyKey {
        old: &["w_confidence"],
        name: "w_confidence",
        default: |value| value.as_float() == Some(0.5),
        home: Home::Nowhere {
            remedy: "the single confidence weight is retired; set the weights in [causal_tree.scoring.weights] and \
                     remove the key",
        },
    },
    LegacyKey {
        old: &["w_cost"],
        name: "w_cost",
        default: |value| value.as_float() == Some(0.25),
        home: Home::Moves {
            path: &["scoring", "weights", "w_cost"],
            name: "scoring.weights.w_cost",
            max: None,
        },
    },
    LegacyKey {
        old: &["w_latency"],
        name: "w_latency",
        default: |value| value.as_float() == Some(0.25),
        home: Home::Moves {
            path: &["scoring", "weights", "w_latency"],
            name: "scoring.weights.w_latency",
            max: None,
        },
    },
];

fn get<'v>(table: &'v toml::Value, path: &[&str]) -> Option<&'v toml::Value> {
    path.iter().try_fold(table, |value, key| value.get(key))
}

fn take(table: &mut toml::Value, path: &[&str]) -> Option<toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut value = table;
    for key in parents {
        value = value.get_mut(key)?;
    }
    value.as_table_mut()?.remove(*last)
}

fn insert(table: &mut toml::Value, path: &[&str], new_value: toml::Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut value = table;
    for key in parents {
        let Some(map) = value.as_table_mut() else {
            return false;
        };
        value = map
            .entry((*key).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let Some(map) = value.as_table_mut() else {
        return false;
    };
    map.insert((*last).to_string(), new_value);
    true
}

/// `causal_tree` followed by `path`, for the locator.
fn full_path<'p>(path: &[&'p str]) -> Vec<&'p str> {
    std::iter::once("causal_tree").chain(path.iter().copied()).collect()
}

/// Map the legacy keys of the `[causal_tree]` table in `root` (the merged
/// configuration tree) to their new homes, in memory; the next structured
/// save writes the new form, with `schema_version`. Idempotent.
///
/// - A legacy key holding the legacy default is dropped, so it takes the new
///   default (a naive move of the old `rehearsal_timeout_ms` default would
///   fail the latency budget rule).
/// - An operator value moves to its new home; if the new home is set too,
///   or the new key cannot hold the value, that is an error naming both
///   keys.
/// - A value with no faithful home is an error with the fix.
/// - DECIDED X-3 with D-074: in a table without `schema_version`,
///   `enabled = false` is the old serialized default, and is treated as
///   unset, when the table holds at least one legacy-only key (a pre-CDP-03
///   `Config::save`) or nothing but `enabled` (the `prx init` template
///   before CDP-03). Otherwise it is an operator's explicit disable.
///
/// The rules that reinterpret a value where it stands, X-3 and the legacy
/// default of a key that kept its name (`policy.extra_latency_budget_ms =
/// 300`), apply only to a table without `schema_version`, which predates the
/// 1.7 schema. In a versioned table every such value is the operator's and
/// is never rewritten (D-073); CDP-31's default flip reads the same marker.
///
/// # Errors
///
/// A [`MigrationError`] naming the file `locate` places the key in, the key
/// and the fix.
pub fn migrate_legacy(root: &mut toml::Value, locate: KeyLocator<'_>) -> Result<MigrationReport, MigrationError> {
    let mut report = MigrationReport::default();
    let Some(table) = root.get_mut("causal_tree") else {
        return Ok(report);
    };
    let file_of = |path: &[&str]| {
        locate(&full_path(path)).map_or_else(
            || "config.toml or a config.d fragment".to_string(),
            |file| file.display().to_string(),
        )
    };

    let versioned = get(table, &["schema_version"]).is_some();
    if !versioned && get(table, &["enabled"]).and_then(toml::Value::as_bool) == Some(false) {
        let legacy_save = LEGACY_ONLY_KEYS.iter().any(|path| get(table, path).is_some());
        let bare = table.as_table().is_some_and(|keys| keys.len() == 1);
        if legacy_save || bare {
            take(table, &["enabled"]);
            report.dropped.push("enabled");
        }
    }

    for key in &LEGACY_KEYS {
        let Some(value) = get(table, key.old) else {
            continue;
        };
        if matches!(key.home, Home::SameKey) && versioned {
            continue;
        }
        if (key.default)(value) {
            take(table, key.old);
            report.dropped.push(key.name);
            continue;
        }
        let (new, new_name, max) = match key.home {
            Home::SameKey => continue,
            Home::Nowhere { remedy } => {
                return Err(MigrationError {
                    file: file_of(key.old),
                    key: key.name,
                    problem: "holds a non-default value with no equivalent in this release".to_string(),
                    remedy: remedy.to_string(),
                });
            }
            Home::Moves { path, name, max } => (path, name, max),
        };
        if get(table, new).is_some() {
            return Err(MigrationError {
                file: file_of(key.old),
                key: key.name,
                problem: format!(
                    "is set together with causal_tree.{new_name} (in {}), the key that replaces it",
                    file_of(new)
                ),
                remedy: format!("keep only causal_tree.{new_name}"),
            });
        }
        if let (Some(max), Some(number)) = (max, value.as_integer())
            && !(0..=max).contains(&number)
        {
            return Err(MigrationError {
                file: file_of(key.old),
                key: key.name,
                problem: format!("holds a value causal_tree.{new_name}, the key that replaces it, cannot hold"),
                remedy: format!("set causal_tree.{new_name} within its documented range and remove the key"),
            });
        }
        let Some(value) = take(table, key.old) else {
            continue;
        };
        if !insert(table, new, value) {
            return Err(MigrationError {
                file: file_of(key.old),
                key: key.name,
                problem: format!("cannot move to causal_tree.{new_name}, because a parent of it is not a table"),
                remedy: "fix the [causal_tree] section by hand".to_string(),
            });
        }
        report.moved.push(key.name);
    }
    for (path, name) in RETIRED_KEYS {
        if take(table, path).is_some() {
            report.dropped.push(name);
        }
    }
    Ok(report)
}

/// Apply an already classified merged-tree migration to one physical document.
/// Classification must happen once across the complete split configuration:
/// per-file classification would mistake a split explicit disable for X3.
pub(crate) fn normalize_regeneration_document(
    root: &mut toml::Value,
    report: &MigrationReport,
    file: &std::path::Path,
) -> Result<(), MigrationError> {
    let Some(table) = root.get_mut("causal_tree") else {
        return Ok(());
    };
    let mut changed = false;
    for name in &report.dropped {
        let path = name.split('.').collect::<Vec<_>>();
        changed |= take(table, &path).is_some();
    }
    for key in &LEGACY_KEYS {
        if !report.moved.contains(&key.name) {
            continue;
        }
        let Home::Moves { path, .. } = key.home else {
            continue;
        };
        let Some(value) = take(table, key.old) else {
            continue;
        };
        if !insert(table, path, value) {
            return Err(MigrationError {
                file: file.display().to_string(),
                key: key.name,
                problem: "cannot preserve this split-file value at its replacement path".to_string(),
                remedy: "make the replacement key's parents tables in this file before regenerating".to_string(),
            });
        }
        changed = true;
    }
    if changed {
        prune_empty_migration_tables(table);
        if table.as_table().is_some_and(toml::map::Map::is_empty) {
            if let Some(root) = root.as_table_mut() {
                root.remove("causal_tree");
            }
        }
    }
    Ok(())
}

// Empty legacy parents must not become modern-key evidence on the next X3
// classification. This only runs inside a causal section actually migrated.
fn prune_empty_migration_tables(value: &mut toml::Value) {
    if let Some(table) = value.as_table_mut() {
        for (_, child) in table.iter_mut() {
            prune_empty_migration_tables(child);
        }
        table.retain(|_, child| !child.as_table().is_some_and(toml::map::Map::is_empty));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A locator that places no key.
    fn nowhere(_: &[&str]) -> Option<std::path::PathBuf> {
        None
    }

    fn causal(text: &str) -> toml::Value {
        toml::from_str(text).expect("test: toml")
    }

    fn valid() -> CausalTreeConfig {
        let config = CausalTreeConfig::default();
        config.validate().expect("test: the defaults are valid");
        config
    }

    fn broken(change: impl FnOnce(&mut CausalTreeConfig)) -> CausalConfigError {
        let mut config = valid();
        change(&mut config);
        config.validate().expect_err("test: refused")
    }

    fn range(key: &str) -> impl Fn(&CausalConfigError) -> bool + '_ {
        move |error| matches!(error, CausalConfigError::OutOfRange { key: named, .. } if *named == key)
    }

    /// Defaults select local processing without an endpoint or credential.
    #[test]
    fn the_defaults_are_local() {
        let config = valid();
        assert!(config.enabled);
        assert!(!config.backend.remote_enabled);
        assert_eq!(config.backend.kind, BackendSelection::Auto);
        assert!(config.backend.endpoint.is_empty(), "expected an empty value");
        assert_eq!(config.backend.credential_ref, CredentialRefSetting::Unset);
        assert_eq!(config.policy.max_candidates, 12);
        assert_eq!(config.rehearsal.timeout_ms, 100);
        assert_eq!(config.backend.circuit_breaker.failure_threshold, 5);
        assert_eq!(config.backend.circuit_breaker.cooldown_secs, 60);
        assert_eq!(config.scoring.weights.w_cost, None);
        assert_eq!(config.scoring.weights.w_latency, None);
    }

    /// Saving writes `enabled` always and nothing else at the defaults; the
    /// legacy view is never written.
    #[test]
    fn saving_the_defaults_writes_only_the_version_and_enabled() {
        let text = toml::to_string(&valid()).expect("test: serialize");
        assert_eq!(text.trim(), "schema_version = 1\nenabled = true");

        let mut config = valid();
        config.rehearsal.timeout_ms = 80;
        let text = toml::to_string(&config).expect("test: serialize");
        assert!(!text.contains("[policy]"), "{text}");
        assert!(
            !text.contains("rehearsal_timeout_ms") && !text.contains("max_branches"),
            "{text}"
        );
    }

    /// SP-CDP-A 5.3 rules 1 to 9 (V2.05 config rows, V1.07 config half).
    #[test]
    fn validation_rules_13_1() {
        // 0. The schema version.
        assert!(range("causal_tree.schema_version")(&broken(|config| config
            .schema_version =
            0)));
        assert!(range("causal_tree.schema_version")(&broken(|config| config
            .schema_version =
            2)));

        // 1. The endpoint, without echoing it.
        let error = broken(|config| config.backend.endpoint = "https://user:hunter2pw@decisions.example.com/v1".into());
        assert_eq!(error, CausalConfigError::Endpoint(EndpointError::Userinfo));
        let message = error.to_string();
        assert!(message.starts_with("causal_tree.backend.endpoint "), "{message}");
        assert!(
            !message.contains("hunter2pw") && !message.contains("example.com"),
            "{message}"
        );
        for (endpoint, expected) in [
            ("http://decisions.example.com/v1", EndpointError::PlainHttpRemote),
            ("http://127.1/v1", EndpointError::PlainHttpRemote),
            ("https://decisions.example.com/v1?key=x", EndpointError::QueryOrFragment),
        ] {
            assert_eq!(
                broken(|config| config.backend.endpoint = endpoint.into()),
                CausalConfigError::Endpoint(expected),
                "{endpoint}"
            );
        }
        let mut local = valid();
        local.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        local.backend.endpoint = "http://127.0.0.1:8900/v1".into();
        local.validate().expect("test: http on literal loopback");
        local.backend.endpoint = "https://127.0.0.1:8900/v1".into();
        local
            .validate()
            .expect("test: https on literal loopback needs no remote switch");
        // V2.05: `https://host` is allowed only with remote enabled.
        local.backend.endpoint = "https://decisions.example.com/v1".into();
        let error = local.validate().expect_err("test: remote needs the remote switch");
        assert_eq!(error, CausalConfigError::RemoteNotEnabled);
        let message = error.to_string();
        assert!(message.contains("causal_tree.backend.endpoint") && message.contains("remote_enabled"));
        assert!(!message.contains("example.com"), "{message}");
        local.backend.remote_enabled = true;
        local
            .validate()
            .expect("test: https to a remote host with remote enabled");

        // 2. compatible_http needs an endpoint.
        assert_eq!(
            broken(|config| config.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp)),
            CausalConfigError::EndpointRequired
        );

        // 3. Hosted kinds (CDP-13): a pinned model; OpenRouter also pins the
        // snapshot it answers with; a snapshot is for hosted kinds only.
        let hosted = |kind: BackendKind, model: &str, snapshot: &str| {
            let mut config = valid();
            config.backend.kind = BackendSelection::Fixed(kind);
            config.backend.model = model.into();
            config.backend.pinned_snapshot = snapshot.into();
            config.validate()
        };
        hosted(BackendKind::TypeSafe, "jev-1.13.0", "").expect("test: typesafe defaults its snapshot to the model");
        hosted(
            BackendKind::OpenRouter,
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917",
        )
        .expect("test: openrouter with its snapshot");
        for kind in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
            for alias in ["", "latest", "jev-latest", "~typesafe/jev-latest"] {
                assert_eq!(
                    hosted(kind, alias, "typesafe/jev-1.13-20260917"),
                    Err(CausalConfigError::HostedModelNotPinned(kind.as_str())),
                    "{alias:?}"
                );
            }
            for snapshot in ["latest", "jev-latest", "bad\u{7}snapshot"] {
                assert_eq!(
                    hosted(kind, "typesafe/jev-1.13", snapshot),
                    Err(CausalConfigError::SnapshotInvalid),
                    "{snapshot:?}"
                );
            }
            assert_eq!(
                hosted(kind, "typesafe/jev-1.13", &"s".repeat(129)),
                Err(CausalConfigError::SnapshotInvalid)
            );
        }
        assert_eq!(
            hosted(BackendKind::OpenRouter, "typesafe/jev-1.13", ""),
            Err(CausalConfigError::SnapshotRequired)
        );
        assert_eq!(
            broken(|config| config.backend.pinned_snapshot = "jev-1.13.0".into()),
            CausalConfigError::SnapshotNotHosted
        );
        // COORD-6: a hosted endpoint override is checked like any endpoint,
        // and the remote switch is the backend order's to require.
        let mut overridden = valid();
        overridden.backend.kind = BackendSelection::Fixed(BackendKind::TypeSafe);
        overridden.backend.model = "jev-1.13.0".into();
        for endpoint in ["http://127.0.0.1:8900/ts", "https://typesafe.example.com"] {
            overridden.backend.endpoint = endpoint.into();
            overridden.validate().expect("test: a hosted override");
        }
        overridden.backend.endpoint = "http://typesafe.example.com".into();
        assert_eq!(
            overridden.validate(),
            Err(CausalConfigError::Endpoint(EndpointError::PlainHttpRemote))
        );

        // 4. A calibration needs its pinned model and kind.
        let profile = CalibrationProfile {
            kind: BackendKind::CompatibleHttp,
            endpoint: local.backend.endpoint.trim().to_string(),
            model: "decider-2026-09".into(),
        };
        let mut calibrated = local;
        calibrated.backend.model = "decider-2026-09".into();
        calibrated.policy.calibration_profile = Some(profile);
        calibrated.validate().expect("test: calibrated on its pinned model");
        for change in [
            (|config: &mut CausalTreeConfig| config.backend.model = String::new()) as fn(&mut CausalTreeConfig),
            |config| {
                config.backend.model = "latest".into();
                config.policy.calibration_profile = Some(CalibrationProfile {
                    kind: BackendKind::CompatibleHttp,
                    endpoint: config.backend.endpoint.trim().to_string(),
                    model: "latest".into(),
                });
            },
            |config| config.backend.model = "decider-2026-10".into(),
            // D-094 C-7: another server serving the same model.
            |config| config.backend.endpoint = "http://127.0.0.2:8900/v1".into(),
            |config| config.backend.kind = BackendSelection::Auto,
        ] {
            let mut config = calibrated.clone();
            change(&mut config);
            assert_eq!(config.validate(), Err(CausalConfigError::CalibrationNotPinned));
        }

        // 5. Time.
        assert!(range("causal_tree.policy.extra_latency_budget_ms")(&broken(|config| {
            config.policy.extra_latency_budget_ms = 49
        })));
        assert!(range("causal_tree.policy.extra_latency_budget_ms")(&broken(|config| {
            config.policy.extra_latency_budget_ms = 5001
        })));
        assert!(range("causal_tree.backend.timeout_ms")(&broken(|config| config
            .backend
            .timeout_ms =
            9)));
        assert!(range("causal_tree.backend.timeout_ms")(&broken(|config| config
            .backend
            .timeout_ms =
            501)));
        assert!(range("causal_tree.rehearsal.timeout_ms")(&broken(|config| config
            .rehearsal
            .timeout_ms =
            501)));
        let mut edge = valid();
        edge.backend.timeout_ms = 500;
        edge.rehearsal.timeout_ms = 500;
        edge.validate().expect("test: both timeouts may equal the budget");

        // 6. Counts.
        assert!(range("causal_tree.policy.max_candidates")(&broken(|config| config
            .policy
            .max_candidates =
            0)));
        assert!(range("causal_tree.policy.max_candidates")(&broken(|config| config
            .policy
            .max_candidates =
            33)));
        assert!(range("causal_tree.policy.max_decision_hops")(&broken(|config| {
            config.policy.max_decision_hops = 0
        })));
        assert!(range("causal_tree.policy.max_decision_hops")(&broken(|config| {
            config.policy.max_decision_hops = 5
        })));

        // 7. Thresholds and weights.
        assert!(range("causal_tree.policy.commit_threshold")(&broken(|config| config
            .policy
            .commit_threshold =
            f32::NAN)));
        assert!(range("causal_tree.policy.commit_threshold")(&broken(|config| config
            .policy
            .commit_threshold =
            1.5)));
        assert!(range("causal_tree.policy.minimum_choice_confidence")(&broken(
            |config| config.policy.minimum_choice_confidence = -0.1
        )));
        assert!(range("causal_tree.policy.minimum_top_margin")(&broken(|config| {
            config.policy.minimum_top_margin = 0.51
        })));
        assert!(range("causal_tree.policy.extra_token_ratio_limit")(&broken(|config| {
            config.policy.extra_token_ratio_limit = f32::INFINITY
        })));
        for zero in [0.0_f32, -0.0, -0.1, f32::NAN] {
            assert!(range("causal_tree.policy.extra_token_ratio_limit")(&broken(|config| {
                config.policy.extra_token_ratio_limit = zero
            })));
        }
        assert!(range("causal_tree.scoring.weights")(&broken(|config| config
            .scoring
            .weights
            .w_rehearsal =
            Some(1.01))));
        assert!(range("causal_tree.scoring.weights")(&broken(|config| config
            .scoring
            .weights
            .w_choice =
            Some(f64::NAN))));
        let mut weighted = valid();
        weighted.scoring.weights.w_cost = Some(0.0);
        weighted.scoring.weights.w_latency = Some(1.0);
        weighted.validate().expect("test: the bounds are inclusive");

        // 8. Exploration.
        assert!(range("causal_tree.evaluation.exploration_rate")(&broken(|config| {
            config.evaluation.exploration_rate = 0.05
        })));
        assert!(range("causal_tree.evaluation.exploration_rate")(&broken(|config| {
            config.evaluation.exploration = ExplorationMode::ReadOnly;
            config.evaluation.exploration_rate = 0.11;
        })));
        let mut exploring = valid();
        exploring.evaluation.exploration = ExplorationMode::ReadOnly;
        exploring.evaluation.exploration_rate = 0.10;
        exploring.validate().expect("test: read-only exploration up to 0.10");

        // 9. Breaker, retention and sizes.
        assert!(range("causal_tree.backend.circuit_breaker.failure_threshold")(&broken(
            |config| config.backend.circuit_breaker.failure_threshold = 0
        )));
        assert!(range("causal_tree.backend.circuit_breaker.failure_threshold")(&broken(
            |config| config.backend.circuit_breaker.failure_threshold = 101
        )));
        assert!(range("causal_tree.backend.circuit_breaker.cooldown_secs")(&broken(
            |config| config.backend.circuit_breaker.cooldown_secs = 0
        )));
        assert!(range("causal_tree.backend.circuit_breaker.cooldown_secs")(&broken(
            |config| config.backend.circuit_breaker.cooldown_secs = 3601
        )));
        assert!(range("causal_tree.store.retention_days")(&broken(|config| config
            .store
            .retention_days =
            3651)));
        assert!(range("causal_tree.backend.max_request_bytes")(&broken(|config| {
            config.backend.max_request_bytes = 4095
        })));
        assert!(range("causal_tree.backend.max_response_bytes")(&broken(|config| {
            config.backend.max_response_bytes = 1_048_577
        })));
    }

    /// The configured spellings, and refusals that never echo the value.
    #[test]
    fn selections_references_and_profiles_parse_strictly() {
        for (text, expected) in [
            ("auto", BackendSelection::Auto),
            ("builtin", BackendSelection::Fixed(BackendKind::Builtin)),
            ("compatible_http", BackendSelection::Fixed(BackendKind::CompatibleHttp)),
            ("typesafe", BackendSelection::Fixed(BackendKind::TypeSafe)),
            ("openrouter", BackendSelection::Fixed(BackendKind::OpenRouter)),
        ] {
            assert_eq!(BackendSelection::try_from(text.to_string()), Ok(expected));
            assert_eq!(String::from(expected), text);
        }
        assert!(
            BackendSelection::try_from("mock".to_string()).is_err(),
            "the test backend is never configurable"
        );

        assert_eq!(
            CredentialRefSetting::try_from(String::new()),
            Ok(CredentialRefSetting::Unset)
        );
        assert_eq!(
            CredentialRefSetting::try_from("none".to_string()),
            Ok(CredentialRefSetting::None)
        );
        for pasted in ["sk-live-hunter2pw", "profile:cred-hunter2pw"] {
            let error = CredentialRefSetting::try_from(pasted.to_string()).expect_err("test: refused");
            assert!(!error.contains("hunter2pw"), "{error}");
        }

        let (kind, model) = CalibrationProfile::measured("compatible_http:decider-2026-09").expect("test: profile");
        assert_eq!(kind, BackendKind::CompatibleHttp);
        assert_eq!(model, "decider-2026-09");
        for malformed in [
            "",
            "decider",
            "auto:decider",
            "compatible_http:",
            "mock:decider",
            "compatible_http:latest",
            "compatible_http:jev:latest",
            "builtin:auto",
        ] {
            assert!(CalibrationProfile::measured(malformed).is_err(), "{malformed}");
        }
        // The profile is a table naming the endpoint too (D-094 C-7).
        let written =
            "kind = \"compatible_http\"\nendpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"decider-2026-09\"\n";
        let profile: CalibrationProfile = toml::from_str(written).expect("test: table");
        assert_eq!(profile.endpoint, "http://127.0.0.1:8900/v1");
        assert_eq!(toml::to_string(&profile).expect("test: toml"), written);
        assert!(toml::from_str::<CalibrationProfile>("kind = \"compatible_http\"\nmodel = \"latest\"\n").is_err());
    }

    /// V2.03 (config `Debug`): user information in an endpoint never prints,
    /// even before validation refuses it.
    #[test]
    fn debug_never_prints_endpoint_user_information() {
        let mut config = valid();
        config.backend.endpoint = "https://decider:hunter2pw@decisions.example.com/v1".into();
        let printed = format!("{config:?}");
        assert!(!printed.contains("hunter2pw"), "{printed}");
        assert!(printed.contains("decisions.example.com"), "{printed}");
    }

    /// The exact `[causal_tree]` table a `Config::save` wrote at `8a5d160d`.
    const SAVED_BY_8A5D160D: &str = r#"
[causal_tree]
enabled = false
w_confidence = 0.5
w_cost = 0.25
w_latency = 0.25
write_decision_log = true
write_metrics = true

[causal_tree.policy]
max_branches = 3
commit_threshold = 0.6200000047683716
extra_token_ratio_limit = 0.3499999940395355
extra_latency_budget_ms = 300
rehearsal_timeout_ms = 5000
default_side_effect_mode = "ReadOnly"
circuit_breaker_threshold = 5
circuit_breaker_cooldown_secs = 60
"#;

    #[test]
    fn legacy_defaults_are_dropped_and_the_legacy_disable_is_unset() {
        let mut root = causal(SAVED_BY_8A5D160D);
        let report = migrate_legacy(&mut root, &nowhere).expect("test: migrates");
        assert!(report.moved.is_empty(), "{report:?}");
        assert!(report.dropped.contains(&"enabled") && report.dropped.contains(&"w_confidence"));
        let table = &root["causal_tree"];
        assert!(
            table.get("enabled").is_none(),
            "X-3: a legacy save's disable is treated as unset"
        );
        for legacy in [
            "w_confidence",
            "w_cost",
            "w_latency",
            "write_decision_log",
            "write_metrics",
        ] {
            assert!(table.get(legacy).is_none(), "{legacy}");
        }
        let policy = table["policy"].as_table().expect("test: policy");
        let mut kept: Vec<&str> = policy.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["commit_threshold", "extra_token_ratio_limit"]);

        let config: CausalTreeConfig = table.clone().try_into().expect("test: loads");
        assert_eq!(config.policy.commit_threshold, DEFAULT_COMMIT_THRESHOLD);
        assert_eq!(config.policy.extra_token_ratio_limit, DEFAULT_EXTRA_TOKEN_RATIO_LIMIT);
        assert_eq!(config, CausalTreeConfig::default());
    }

    /// X-3 with D-074: before the versioned schema, `enabled = false` next
    /// to a legacy-only key (a `Config::save`) or alone (the `prx init`
    /// template) is the old serialized default and reads as unset; next to a
    /// setting the operator wrote, or in a versioned table, it is an
    /// explicit disable and is kept.
    #[test]
    fn an_explicit_disable_is_kept() {
        for text in [
            "[causal_tree]\nenabled = false\n",
            "[causal_tree]\nenabled = false\nwrite_metrics = true\n",
        ] {
            let mut root = causal(text);
            let report = migrate_legacy(&mut root, &nowhere).expect("test: migrates");
            assert!(report.dropped.contains(&"enabled"), "{text}");
            assert_eq!(root["causal_tree"].get("enabled"), None, "{text}");
        }
        for text in [
            "[causal_tree]\nschema_version = 1\nenabled = false\n",
            "[causal_tree]\nenabled = false\n[causal_tree.policy]\nextra_latency_budget_ms = 400\n",
            "[causal_tree]\nenabled = false\nw_cost = 0.3\n",
        ] {
            let mut root = causal(text);
            migrate_legacy(&mut root, &nowhere).expect("test: migrates");
            assert_eq!(
                root["causal_tree"].get("enabled"),
                Some(&toml::Value::Boolean(false)),
                "{text}"
            );
        }
        let mut root = causal("[causal_tree]\nenabled = true\nwrite_metrics = true\n");
        migrate_legacy(&mut root, &nowhere).expect("test: migrates");
        assert_eq!(root["causal_tree"].get("enabled"), Some(&toml::Value::Boolean(true)));
    }

    /// D-073: the budget reset and the X-3 rule reinterpret a value, so they
    /// apply only to a table without `schema_version` (pre-1.7); in a
    /// versioned table the operator's 300 and `enabled = false` stay.
    #[test]
    fn value_rewrites_apply_only_to_files_before_the_versioned_schema() {
        let mut unversioned = causal(
            "[causal_tree]\nenabled = false\nwrite_metrics = true\n[causal_tree.policy]\nextra_latency_budget_ms = 300\n",
        );
        let report = migrate_legacy(&mut unversioned, &nowhere).expect("test: migrates");
        assert_eq!(
            report.dropped,
            ["enabled", "policy.extra_latency_budget_ms", "write_metrics"]
        );
        let config: CausalTreeConfig = unversioned["causal_tree"].clone().try_into().expect("test: loads");
        assert_eq!(config.policy.extra_latency_budget_ms, DEFAULT_EXTRA_LATENCY_BUDGET_MS);

        let mut versioned = causal(
            "[causal_tree]\nschema_version = 1\nenabled = false\nwrite_metrics = true\n\
             [causal_tree.policy]\nextra_latency_budget_ms = 300\n",
        );
        let report = migrate_legacy(&mut versioned, &nowhere).expect("test: migrates");
        assert_eq!(
            report.dropped,
            ["write_metrics"],
            "a renamed legacy key is still mapped"
        );
        let table = &versioned["causal_tree"];
        assert_eq!(
            table.get("enabled"),
            Some(&toml::Value::Boolean(false)),
            "an explicit disable"
        );
        let config: CausalTreeConfig = table.clone().try_into().expect("test: loads");
        assert_eq!(
            config.policy.extra_latency_budget_ms, 300,
            "an explicit value is never rewritten"
        );
        assert!(!config.enabled);
        config.validate().expect("test: 300 is a valid budget");
    }

    #[test]
    fn legacy_non_default_values_move_to_their_new_home_in_the_tree() {
        let mut root = causal(
            r#"
[causal_tree]
enabled = true
w_cost = 0.4000000059604645
w_latency = 0.25
write_decision_log = false
write_metrics = false
[causal_tree.policy]
max_branches = 5
extra_latency_budget_ms = 800
rehearsal_timeout_ms = 200
circuit_breaker_threshold = 9
circuit_breaker_cooldown_secs = 120
"#,
        );
        let report = migrate_legacy(&mut root, &nowhere).expect("test: migrates");
        assert_eq!(
            report.moved,
            [
                "policy.max_branches",
                "policy.rehearsal_timeout_ms",
                "policy.circuit_breaker_threshold",
                "policy.circuit_breaker_cooldown_secs",
                "w_cost",
            ]
        );
        assert_eq!(
            report.dropped,
            ["w_latency", "write_decision_log", "write_metrics"],
            "the legacy default is dropped, and so are the switches that never had an effect"
        );
        let config: CausalTreeConfig = root["causal_tree"].clone().try_into().expect("test: loads");
        assert!(config.enabled);
        assert_eq!(config.policy.max_candidates, 5);
        assert_eq!(config.policy.extra_latency_budget_ms, 800);
        assert_eq!(config.rehearsal.timeout_ms, 200);
        assert_eq!(config.backend.circuit_breaker.failure_threshold, 9);
        assert_eq!(config.backend.circuit_breaker.cooldown_secs, 120);
        assert_eq!(config.scoring.weights.w_cost, Some(0.400_000_005_960_464_5));
        assert_eq!(config.scoring.weights.w_latency, None, "the legacy default is dropped");
        config.validate().expect("test: the moved values are valid");
    }

    /// The decision log and metrics switches were accepted but never
    /// read. A stored file keeps loading, in a versioned
    /// table too, and the report names each dropped key.
    #[test]
    fn retired_keys_are_dropped_with_a_report_in_any_schema_version() {
        for text in [
            "[causal_tree]\nschema_version = 1\n[causal_tree.observability]\ndecision_log = false\nmetrics = false\n\
             [causal_tree.scoring.weights]\nw_cost = 0.2\n",
            "[causal_tree]\nwrite_decision_log = false\nwrite_metrics = false\n[causal_tree.scoring.weights]\nw_cost = 0.2\n",
        ] {
            let mut root = causal(text);
            let report = migrate_legacy(&mut root, &nowhere).expect("test: migrates");
            assert!(report.found_legacy_keys(), "{text}");
            assert_eq!(report.dropped.len(), 2, "{text}: {report:?}");
            prune_empty_migration_tables(&mut root);
            let config: CausalTreeConfig = root["causal_tree"].clone().try_into().expect("test: loads");
            assert_eq!(config.scoring.weights.w_cost, Some(0.2), "operator weights are kept");
            config.validate().expect("test: valid");
        }
    }

    /// Keys with no faithful home, and a legacy key beside its replacement,
    /// fail naming the file and the key, never the value.
    #[test]
    fn legacy_values_without_a_home_fail_precisely() {
        for (text, key) in [
            ("[causal_tree]\nw_confidence = 0.8123\n", "w_confidence"),
            (
                "[causal_tree.policy]\ndefault_side_effect_mode = \"GuardedWrite\"\n",
                "policy.default_side_effect_mode",
            ),
            (
                "[causal_tree.policy]\nmax_branches = 5\nmax_candidates = 6\n",
                "policy.max_branches",
            ),
            ("[causal_tree.policy]\nmax_branches = 300\n", "policy.max_branches"),
            (
                "[causal_tree.policy]\ncircuit_breaker_cooldown_secs = -1\n",
                "policy.circuit_breaker_cooldown_secs",
            ),
        ] {
            let mut root = causal(text);
            let agent = |_: &[&str]| Some(std::path::PathBuf::from("/cfg/config.d/agent.toml"));
            let error = migrate_legacy(&mut root, &agent).expect_err("test: refused");
            assert_eq!(error.key, key, "{text}");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("/cfg/config.d/agent.toml: causal_tree.{key} ")),
                "{message}"
            );
            assert!(
                !message.contains("0.8123") && !message.contains("GuardedWrite"),
                "{message}"
            );
        }
    }

    /// A legacy key and its replacement name both keys and each one's file;
    /// a value the new key cannot hold names the new key, not a serde type.
    #[test]
    fn a_conflict_names_the_replacing_key_and_its_file() {
        let mut root = causal("[causal_tree.policy]\nmax_branches = 5\nmax_candidates = 6\n");
        let locate = |path: &[&str]| {
            Some(std::path::PathBuf::from(if path.last() == Some(&"max_branches") {
                "/cfg/config.d/agent.toml"
            } else {
                "/cfg/config.toml"
            }))
        };
        let message = migrate_legacy(&mut root, &locate)
            .expect_err("test: refused")
            .to_string();
        assert!(
            message.starts_with(
                "/cfg/config.d/agent.toml: causal_tree.policy.max_branches is set together with \
                                 causal_tree.policy.max_candidates (in /cfg/config.toml)"
            ),
            "{message}"
        );
        let mut root = causal("[causal_tree.policy]\nmax_branches = 300\n");
        let message = migrate_legacy(&mut root, &nowhere)
            .expect_err("test: refused")
            .to_string();
        assert!(
            message.starts_with(
                "config.toml or a config.d fragment: causal_tree.policy.max_branches holds a value \
                                 causal_tree.policy.max_candidates"
            ),
            "{message}"
        );
        assert!(!message.contains("300"), "{message}");
    }

    /// JSON reloads preserve configured values and the schema version.
    #[test]
    fn a_json_round_trip_keeps_configured_values_and_the_version() {
        let mut config = valid();
        config.policy.max_candidates = 5;
        config.rehearsal.timeout_ms = 80;
        config.backend.circuit_breaker = CircuitBreakerConfig {
            failure_threshold: 9,
            cooldown_secs: 30,
        };
        config.scoring.weights.w_cost = Some(0.4);
        let json = serde_json::to_value(&config).expect("test: json");
        assert_eq!(json["schema_version"], serde_json::json!(1));
        assert!(json.get("w_cost").is_none() && json["policy"].get("max_branches").is_none());
        let back: CausalTreeConfig = serde_json::from_value(json).expect("test: back");
        assert_eq!(back, config);
        assert_eq!(back.policy.max_candidates, 5);
        assert_eq!(back.rehearsal.timeout_ms, 80);
        assert_eq!(back.backend.circuit_breaker.failure_threshold, 9);
        assert_eq!(back.backend.circuit_breaker.cooldown_secs, 30);
        assert_eq!(back.scoring.weights.w_cost, Some(0.4));
    }

    #[test]
    fn migration_is_idempotent() {
        let mut root = causal(&format!(
            "{SAVED_BY_8A5D160D}\n[causal_tree.backend]\nkind = \"builtin\"\n"
        ));
        root["causal_tree"]["policy"]
            .as_table_mut()
            .expect("test: policy")
            .insert("max_branches".into(), toml::Value::Integer(7));
        migrate_legacy(&mut root, &nowhere).expect("test: first");
        let once = root.clone();
        let report = migrate_legacy(&mut root, &nowhere).expect("test: second");
        assert!(!report.found_legacy_keys(), "{report:?}");
        assert_eq!(root, once);
        assert!(
            migrate_legacy(&mut causal("title = 1\n"), &nowhere)
                .expect("test: no table")
                .moved
                .is_empty(),
            "expected an empty value"
        );
    }
}

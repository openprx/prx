//! The decision backend (`[decision_engine.backend]`): kind selection,
//! credential reference, circuit breaker and the shared endpoint, hosted
//! model and snapshot rules.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::backend_kind::BackendKind;
use crate::providers::loopback::{EndpointError, EndpointLocality, check_endpoint};
use crate::providers::model_pin::is_unpinned_model;

const DEFAULT_TIMEOUT_MS: u32 = 250;
const DEFAULT_MAX_REQUEST_BYTES: u32 = 64 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: u32 = 256 * 1024;
const DEFAULT_FAILURE_THRESHOLD: u32 = 5;
const DEFAULT_COOLDOWN_SECS: u32 = 60;
/// The longest `pinned_snapshot`, like every model identity claim.
const MAX_SNAPSHOT_CHARS: usize = 128;

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
                    "decision_engine.backend.kind must be one of auto, builtin, compatible_http, typesafe, openrouter"
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
                "decision_engine.backend.credential_ref is not a credential reference: it is \"\", \"none\" or \
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
        DEFAULT_BACKEND.circuit_breaker.clone()
    }
}

/// The decision backend (`[decision_engine.backend]`).
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

/// The backend that applies when `[decision_engine.backend]` is absent: the
/// serde default, the same backend a fresh `prx init` writes.
pub(crate) static DEFAULT_BACKEND: DecisionBackendConfig = DecisionBackendConfig {
    kind: BackendSelection::Auto,
    remote_enabled: false,
    endpoint: String::new(),
    model: String::new(),
    pinned_snapshot: String::new(),
    credential_ref: CredentialRefSetting::Unset,
    timeout_ms: DEFAULT_TIMEOUT_MS,
    max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
    max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
    circuit_breaker: CircuitBreakerConfig {
        failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        cooldown_secs: DEFAULT_COOLDOWN_SECS,
    },
};

impl Default for DecisionBackendConfig {
    fn default() -> Self {
        DEFAULT_BACKEND.clone()
    }
}

impl DecisionBackendConfig {
    /// Validate shared endpoint, hosted model and snapshot rules.
    pub fn validate_shared(&self) -> Result<(), BackendConfigError> {
        let backend = self;
        let hosted = match backend.kind {
            BackendSelection::Fixed(kind @ (BackendKind::TypeSafe | BackendKind::OpenRouter)) => Some(kind),
            _ => None,
        };
        // 1-2. The endpoint.
        if !backend.endpoint.trim().is_empty() {
            let locality = check_endpoint(&backend.endpoint).map_err(BackendConfigError::Endpoint)?;
            // SP-CDP-C V2.05, CXR 4.4: decision state leaves the machine only
            // with the operator's remote switch. A hosted service is remote
            // whatever its URL, and the backend order leaves it out until the
            // switch is on (COORD-6).
            if locality == EndpointLocality::Remote && !backend.remote_enabled && hosted.is_none() {
                return Err(BackendConfigError::RemoteNotEnabled);
            }
        } else if backend.kind == BackendSelection::Fixed(BackendKind::CompatibleHttp) {
            return Err(BackendConfigError::EndpointRequired);
        }
        // 3. A hosted service answers with one pinned model, and the exact
        // snapshot is pinned too (D-043 C-8).
        let snapshot = backend.pinned_snapshot.trim();
        match hosted {
            Some(kind) => {
                if is_unpinned_model(&backend.model) {
                    return Err(BackendConfigError::HostedModelNotPinned(kind.as_str()));
                }
                if kind == BackendKind::OpenRouter && snapshot.is_empty() {
                    return Err(BackendConfigError::SnapshotRequired);
                }
                let invalid = is_unpinned_model(snapshot)
                    || snapshot.chars().count() > MAX_SNAPSHOT_CHARS
                    || snapshot.chars().any(char::is_control);
                if !snapshot.is_empty() && invalid {
                    return Err(BackendConfigError::SnapshotInvalid);
                }
            }
            None if !snapshot.is_empty() => return Err(BackendConfigError::SnapshotNotHosted),
            None => {}
        }
        Ok(())
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

/// Why a `[decision_engine.backend]` table cannot run. Each message names the
/// key and the rule, and never repeats a value that could be a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendConfigError {
    #[error("decision_engine.backend.endpoint {0}")]
    Endpoint(EndpointError),
    #[error("decision_engine.backend.kind = \"compatible_http\" needs decision_engine.backend.endpoint")]
    EndpointRequired,
    #[error(
        "decision_engine.backend.kind = \"{0}\" is a hosted decision service: decision_engine.backend.model must \
         name one exact version, never an alias or a `latest` spelling"
    )]
    HostedModelNotPinned(&'static str),
    #[error(
        "decision_engine.backend.kind = \"openrouter\" needs decision_engine.backend.pinned_snapshot: the exact \
         dated snapshot OpenRouter answers with for decision_engine.backend.model"
    )]
    SnapshotRequired,
    #[error(
        "decision_engine.backend.pinned_snapshot must name one exact version in at most 128 characters, never an \
         alias or a `latest` spelling"
    )]
    SnapshotInvalid,
    #[error("decision_engine.backend.pinned_snapshot applies to the hosted kinds only (typesafe, openrouter)")]
    SnapshotNotHosted,
    #[error(
        "decision_engine.backend.endpoint is a remote endpoint; remote decision processing needs \
         decision_engine.backend.remote_enabled = true (a literal loopback endpoint does not)"
    )]
    RemoteNotEnabled,
}

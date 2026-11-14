//! The `[decision_engine]` configuration: the default-off advisory engine,
//! its backend and the turn limits it reads.
mod backend;
mod backend_kind;

pub(crate) use backend::DEFAULT_BACKEND;
pub use backend::{
    BackendConfigError, BackendSelection, CircuitBreakerConfig, CredentialRefSetting, DecisionBackendConfig,
};
pub use backend_kind::{
    BackendKind, BackendLocality, ConfiguredSelection, configured_kind, configured_selection, locality_of,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_LATENCY_BUDGET_MS: u64 = 500;
const DEFAULT_TOKEN_RATIO_LIMIT: f32 = 0.35;
const DEFAULT_MAX_CANDIDATES: u8 = 12;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    #[default]
    Pre,
    Post,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DecisionEngineConfig {
    pub enabled: bool,
    pub mode: DecisionMode,
    pub prompt_budget_tokens: u32,
    pub candidates: u32,
    pub timeout_ms: u32,
    pub telemetry_cap: u32,
    pub observe_queue: u32,
    /// The extra latency one turn may spend deciding (50 to 5000 ms); also the
    /// backend transport timeout ceiling while the engine is disabled.
    pub latency_budget_ms: u64,
    /// Maximum ratio of extra tokens to the turn's baseline tokens (a finite
    /// number greater than 0).
    pub token_ratio_limit: f32,
    /// The most candidates a turn considers (1 to 32).
    pub max_candidates: u8,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_backend")]
    pub backend: Option<DecisionBackendConfig>,
}
impl Default for DecisionEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: DecisionMode::Pre,
            prompt_budget_tokens: 300,
            candidates: 5,
            timeout_ms: 1_500,
            telemetry_cap: 1_000,
            observe_queue: 64,
            latency_budget_ms: DEFAULT_LATENCY_BUDGET_MS,
            token_ratio_limit: DEFAULT_TOKEN_RATIO_LIMIT,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            backend: None,
        }
    }
}
fn serialize_backend<S: serde::Serializer>(
    backend: &Option<DecisionBackendConfig>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let Some(backend) = backend else {
        return serializer.serialize_none();
    };
    let mut value = toml::Value::try_from(backend).map_err(serde::ser::Error::custom)?;
    if let Some(table) = value.as_table_mut() {
        table.insert("pinned_snapshot".into(), backend.pinned_snapshot.clone().into());
    }
    value.serialize(serializer)
}
#[derive(Debug, thiserror::Error)]
pub enum DecisionEngineConfigError {
    #[error("decision_engine.{key} must be between {min} and {max}")]
    OutOfRange { key: &'static str, min: u32, max: u32 },
    #[error("decision_engine.{key} must be {rule}")]
    Invalid { key: &'static str, rule: &'static str },
    #[error("invalid decision_engine.backend: {0}")]
    Backend(#[source] BackendConfigError),
}

fn ensure_range(key: &'static str, value: u32, min: u32, max: u32) -> Result<(), DecisionEngineConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(DecisionEngineConfigError::OutOfRange { key, min, max })
    }
}

impl DecisionEngineConfig {
    pub fn validate(&self) -> Result<(), DecisionEngineConfigError> {
        for (key, value, min, max) in [
            ("prompt_budget_tokens", self.prompt_budget_tokens, 50, 2_000),
            ("candidates", self.candidates, 1, 20),
            ("timeout_ms", self.timeout_ms, 100, 30_000),
            ("telemetry_cap", self.telemetry_cap, 0, 100_000),
            ("observe_queue", self.observe_queue, 1, 1_024),
        ] {
            ensure_range(key, value, min, max)?;
        }
        if !(50..=5_000).contains(&self.latency_budget_ms) {
            return Err(DecisionEngineConfigError::Invalid {
                key: "latency_budget_ms",
                rule: "between 50 and 5000",
            });
        }
        if !(self.token_ratio_limit.is_finite() && self.token_ratio_limit > 0.0) {
            return Err(DecisionEngineConfigError::Invalid {
                key: "token_ratio_limit",
                rule: "a finite number greater than 0 (a zero ratio refuses every Agent turn)",
            });
        }
        if !(1..=32).contains(&self.max_candidates) {
            return Err(DecisionEngineConfigError::Invalid {
                key: "max_candidates",
                rule: "between 1 and 32",
            });
        }
        if let Some(backend) = &self.backend {
            for (key, value, min, max) in [
                (
                    "backend.timeout_ms",
                    backend.timeout_ms,
                    10,
                    if self.enabled {
                        self.timeout_ms
                    } else {
                        u32::try_from(self.latency_budget_ms).unwrap_or(u32::MAX)
                    },
                ),
                ("backend.max_request_bytes", backend.max_request_bytes, 4_096, 1_048_576),
                (
                    "backend.max_response_bytes",
                    backend.max_response_bytes,
                    4_096,
                    1_048_576,
                ),
                (
                    "backend.circuit_breaker.failure_threshold",
                    backend.circuit_breaker.failure_threshold,
                    1,
                    100,
                ),
                (
                    "backend.circuit_breaker.cooldown_secs",
                    backend.circuit_breaker.cooldown_secs,
                    1,
                    3_600,
                ),
            ] {
                ensure_range(key, value, min, max)?;
            }
            backend.validate_shared().map_err(DecisionEngineConfigError::Backend)?;
        }
        Ok(())
    }
    /// Hosted backends remain excluded until remote processing is enabled.
    pub fn backend_available(&self) -> bool {
        self.backend.as_ref().is_none_or(|b| {
            b.remote_enabled
                || !matches!(
                    b.kind,
                    BackendSelection::Fixed(BackendKind::TypeSafe | BackendKind::OpenRouter)
                )
        })
    }
}

/// The one backend every engine use reads (R55): `[decision_engine.backend]`
/// when present (an explicitly empty table is "all defaults"), otherwise the
/// backend's own serde default.
pub fn effective_engine_backend(config: &crate::config::Config) -> &DecisionBackendConfig {
    config.decision_engine.backend.as_ref().unwrap_or(&DEFAULT_BACKEND)
}

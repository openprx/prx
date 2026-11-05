//! Default-off advisory engine configuration and raw-key migration.
pub use crate::causal_tree::config::DecisionBackendConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    #[default]
    Pre,
    Post,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DecisionEngineConfig {
    pub enabled: bool,
    pub mode: DecisionMode,
    pub prompt_budget_tokens: u32,
    pub candidates: u32,
    pub timeout_ms: u32,
    pub telemetry_cap: u32,
    pub observe_queue: u32,
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
    #[error("invalid decision_engine.backend: {0}")]
    Backend(#[source] crate::causal_tree::config::CausalConfigError),
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
        if let Some(backend) = &self.backend {
            for (key, value, min, max) in [
                (
                    "backend.timeout_ms",
                    backend.timeout_ms,
                    10,
                    if self.enabled { self.timeout_ms } else { 5_000 },
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
        use crate::causal_tree::config::BackendSelection;
        use crate::decision::BackendKind;
        self.backend.as_ref().is_none_or(|b| {
            b.remote_enabled
                || !matches!(
                    b.kind,
                    BackendSelection::Fixed(BackendKind::TypeSafe | BackendKind::OpenRouter)
                )
        })
    }
}

/// Resolve the same inherited backend for disk migration and process seals.
pub(crate) fn effective_backend<'a>(
    explicit: Option<&'a DecisionBackendConfig>,
    legacy: &'a DecisionBackendConfig,
) -> &'a DecisionBackendConfig {
    explicit.unwrap_or(legacy)
}

pub(crate) fn migrate_backend(root: &mut toml::Value) -> anyhow::Result<()> {
    let legacy = root.get("causal_tree").and_then(|v| v.get("backend")).cloned();
    let explicit = root.get("decision_engine").and_then(|v| v.get("backend")).cloned();
    let enabled = root
        .get("decision_engine")
        .and_then(|v| v.get("enabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let warning = match (&legacy, &explicit) {
        (Some(old), Some(new)) if old != new => {
            Some("W2: decision_engine.backend overrides differing causal_tree.backend")
        }
        (Some(_), None) if enabled => Some("W1: copied legacy causal_tree.backend into decision_engine.backend"),
        (Some(old), _)
            if !enabled && old.clone().try_into::<DecisionBackendConfig>()? != DecisionBackendConfig::default() =>
        {
            Some("W3: legacy non-default backend retained while decision_engine is disabled")
        }
        _ => None,
    };
    // Materialize full defaults before empty-table pruning. An empty explicit
    // table is still an override, and must never inherit a legacy backend.
    let inherited = if explicit.is_none() {
        legacy
            .as_ref()
            .map(|raw| -> anyhow::Result<toml::Value> {
                let parsed: DecisionBackendConfig = raw.clone().try_into()?;
                Ok(toml::Value::try_from(effective_backend(None, &parsed))?)
            })
            .transpose()?
    } else {
        None
    };
    if let Some(raw) = explicit.or(inherited) {
        let full = if empty_tables_only(&raw) {
            toml::Value::try_from(DecisionBackendConfig::default())?
        } else {
            raw
        };
        if let Some(table) = root.as_table_mut() {
            let engine = table
                .entry("decision_engine")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            if let Some(engine) = engine.as_table_mut() {
                engine.insert("backend".into(), full);
            }
        }
    }
    if let Some(warning) = warning {
        tracing::warn!(warning, "decision engine backend migration");
    }
    Ok(())
}

// Pruning nested empty tables must not erase a raw backend override either.
fn empty_tables_only(value: &toml::Value) -> bool {
    value
        .as_table()
        .is_some_and(|table| table.values().all(empty_tables_only))
}

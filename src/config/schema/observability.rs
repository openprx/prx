use super::{Deserialize, JsonSchema, Serialize};

// ── Observability ─────────────────────────────────────────────────

/// Observability backend configuration (`[observability]` section).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObservabilityConfig {
    /// "none" | "log" | "prometheus" | "otel"
    #[serde(default = "default_observability_backend")]
    pub backend: String,

    /// OTLP endpoint (e.g. "http://localhost:4318"). Only used when backend = "otel".
    #[serde(default)]
    pub otel_endpoint: Option<String>,

    /// Service name reported to the OTel collector. Defaults to "prx".
    #[serde(default)]
    pub otel_service_name: Option<String>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: default_observability_backend(),
            otel_endpoint: None,
            otel_service_name: None,
        }
    }
}

/// The observability backend a `[observability]` section gets when it names
/// none: off.
pub(super) fn default_observability_backend() -> String {
    "none".into()
}

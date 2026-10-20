use super::{Deserialize, JsonSchema, Serialize};

// ── Cost tracking and budget enforcement ───────────────────────────

/// Cost tracking and budget enforcement configuration (`[cost]` section).
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostConfig {
    /// Daily spending limit in USD (default: 10.00)
    #[serde(default = "default_daily_limit")]
    pub daily_limit_usd: f64,

    /// Monthly spending limit in USD (default: 100.00)
    #[serde(default = "default_monthly_limit")]
    pub monthly_limit_usd: f64,

    /// Warn when spending reaches this percentage of limit (default: 80)
    #[serde(default = "default_warn_percent")]
    pub warn_at_percent: u8,

    /// Allow requests to exceed budget with --override flag (default: false)
    #[serde(default)]
    pub allow_override: bool,

    /// Per-model pricing (USD per 1M tokens)
    #[serde(default)]
    pub prices: std::collections::HashMap<String, ModelPricing>,
}

/// Per-model pricing entry (USD per 1M tokens).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPricing {
    /// Input price per 1M tokens
    #[serde(default)]
    pub input: f64,

    /// Output price per 1M tokens
    #[serde(default)]
    pub output: f64,

    /// Cache write/creation input price per 1M tokens.
    #[serde(default)]
    pub cache_write: f64,

    /// Cache read input price per 1M tokens.
    #[serde(default)]
    pub cache_read: f64,
}

impl ModelPricing {
    /// An invalid component must not cancel another component into a free
    /// estimate or be treated as an unpriced local model.
    pub(crate) fn is_finite_nonnegative(&self) -> bool {
        [self.input, self.output, self.cache_write, self.cache_read]
            .into_iter()
            .all(|price| price.is_finite() && price >= 0.0)
    }
}

pub(super) const fn default_daily_limit() -> f64 {
    10.0
}

pub(super) const fn default_monthly_limit() -> f64 {
    100.0
}

pub(super) const fn default_warn_percent() -> u8 {
    80
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            daily_limit_usd: default_daily_limit(),
            monthly_limit_usd: default_monthly_limit(),
            warn_at_percent: default_warn_percent(),
            allow_override: false,
            // Operator overrides only. Built-in prices come from the embedded
            // release catalog at the point of use.
            prices: std::collections::HashMap::new(),
        }
    }
}

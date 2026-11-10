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

/// The `[cost.prices]` map key `G6` uses for `provider`/`model`.
///
/// A model id that already contains `/` is the key; otherwise `{provider}/{model}`.
/// That is why a `custom:<url>` row whose model is `moonshotai/kimi-k2.5` looks
/// up `[cost.prices."moonshotai/kimi-k2.5"]`, while `kimi-code`/`kimi-for-coding`
/// looks up `[cost.prices."kimi-code/kimi-for-coding"]`.
#[must_use]
pub(crate) fn usage_pricing_key(provider: &str, model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

/// Whether `price_key` is the override `G6` would apply when looking up `lookup_key`.
///
/// Exact match wins; otherwise a key that carries URL userinfo matches when its
/// redacted spelling equals the lookup key's (the same rule as `G6`).
#[must_use]
pub(crate) fn cost_price_key_matches(price_key: &str, lookup_key: &str) -> bool {
    if price_key == lookup_key {
        return true;
    }
    price_key.contains('@')
        && crate::providers::redact_endpoint_userinfo(price_key)
            == crate::providers::redact_endpoint_userinfo(lookup_key)
}

/// Warnings for `[cost.prices]` keys `G6` would never look up for a configured
/// provider/model. Existing configs keep loading: this is a warning, not an error.
/// For a custom provider the text names the expected key.
#[must_use]
pub(crate) fn unmatched_cost_price_key_warnings(config: &super::Config) -> Vec<String> {
    if config.cost.prices.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut lookups = Vec::new();
    for (provider, model) in crate::config::manage::validate::selected_models(config) {
        if provider.trim().is_empty() || model.trim().is_empty() {
            continue;
        }
        if !seen.insert((provider.clone(), model.clone())) {
            continue;
        }
        let key = usage_pricing_key(&provider, &model);
        lookups.push((provider, model, key));
    }
    let mut custom_expected = Vec::new();
    let mut seen_custom = std::collections::BTreeSet::new();
    for (provider, model, expected) in &lookups {
        if !matches!(
            crate::providers::classify_provider(provider),
            crate::providers::ProviderIdentity::ByoEndpoint { .. }
        ) {
            continue;
        }
        if seen_custom.insert(expected.clone()) {
            custom_expected.push((provider.clone(), model.clone(), expected.clone()));
        }
    }
    let custom_clause = custom_expected_clause(&custom_expected);
    let mut keys: Vec<&str> = config.cost.prices.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut warnings = Vec::new();
    for price_key in keys {
        if lookups
            .iter()
            .any(|(_, _, lookup)| cost_price_key_matches(price_key, lookup))
        {
            continue;
        }
        let shown = crate::providers::redact_endpoint_userinfo(price_key);
        let shown = shown.as_ref();
        let mut message = format!(
            "[cost.prices.{shown:?}] matches no configured provider/model; G6 looks up the bare model id when the model contains '/', otherwise {{provider}}/{{model}}"
        );
        if let Some(clause) = custom_clause.as_deref() {
            message.push_str("; ");
            message.push_str(clause);
        }
        warnings.push(message);
    }
    warnings
}

fn custom_expected_clause(custom_expected: &[(String, String, String)]) -> Option<String> {
    if custom_expected.is_empty() {
        return None;
    }
    let named: Vec<String> = custom_expected
        .iter()
        .map(|(provider, model, expected)| {
            format!(
                "for custom provider '{}' model '{}' the expected key is {:?}",
                crate::providers::redact_endpoint_userinfo(provider),
                crate::providers::redact_endpoint_userinfo(model),
                crate::providers::redact_endpoint_userinfo(expected).as_ref()
            )
        })
        .collect();
    Some(named.join("; "))
}

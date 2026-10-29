//! Bounded label values for every metrics backend (OBS-LABELS; D-059;
//! D-060 L-1).
//!
//! Prometheus labels and OpenTelemetry attributes take their values from
//! here, so no input can grow the number of series without bound and no free
//! text reaches a label:
//!
//! - providers, tools, channels and directions map onto fixed sets;
//! - error components onto a capped set per observer;
//! - models onto the **configured-model allowlist** (D-059): the models the
//!   active configuration names, plus the builtin decision model. Any other
//!   model, including a model string a backend claimed in a response, is
//!   [`UNLISTED_MODEL`]. A listed model longer than
//!   [`MAX_MODEL_LABEL_CHARS`] is cut to that length.

use std::borrow::Cow;
use std::collections::HashSet;

use parking_lot::RwLock;

use crate::providers::{ProviderId, ProviderNamespace};

/// The label value for anything outside a label's bounded set.
pub const OTHER_LABEL: &str = "other";

/// The longest free-text value (a model or component name) kept as a label.
const MAX_FREE_TEXT_LABEL_LEN: usize = 128;

/// How many distinct error components one observer keeps as label values.
pub const COMPONENT_LABEL_CAP: usize = 32;

/// Channel kinds kept as `channel` label values (each channel's `name()`).
const KNOWN_CHANNEL_LABELS: &[&str] = &[
    "cli",
    "terminal",
    "telegram",
    "discord",
    "slack",
    "mattermost",
    "webhook",
    "imessage",
    "matrix",
    "signal",
    "whatsapp",
    "wacli",
    "linq",
    "nextcloud_talk",
    "email",
    "irc",
    "lark",
    "dingtalk",
    "qq",
    "gateway",
];

/// The `provider` label: a catalog provider's canonical name, otherwise the
/// kind of endpoint (`custom`, `anthropic-custom`, `wasm`, `decision`, `unknown`).
///
/// A `custom:` label carries the endpoint URL, so using it as the label value
/// opened one series per endpoint; the kind is bounded.
pub fn provider_label(provider: &str) -> String {
    let id = ProviderId::of(provider);
    match id.namespace() {
        ProviderNamespace::Catalog => id.catalog_name().unwrap_or(OTHER_LABEL).to_string(),
        ProviderNamespace::Custom => "custom".to_string(),
        ProviderNamespace::AnthropicCustom => "anthropic-custom".to_string(),
        ProviderNamespace::Wasm => "wasm".to_string(),
        ProviderNamespace::DecisionService => "decision".to_string(),
        ProviderNamespace::Unknown => "unknown".to_string(),
    }
}

/// The `tool` label: a statically registered tool keeps its name, an MCP
/// alias (`<prefix>__<server>__<tool>`) is `mcp`, a skill alias is `skill`,
/// and any other name (for one, a tool the model invented) is `other`.
pub fn tool_label(tool: &str) -> &str {
    if crate::tools::STATIC_PUBLIC_TOOL_NAMES.contains(&tool) {
        tool
    } else if tool.starts_with("skill__") {
        "skill"
    } else if tool.splitn(3, "__").count() == 3 {
        "mcp"
    } else {
        OTHER_LABEL
    }
}

/// The `channel` label: a known channel kind, otherwise `other`.
pub fn channel_label(channel: &str) -> &str {
    if KNOWN_CHANNEL_LABELS.contains(&channel) {
        channel
    } else {
        OTHER_LABEL
    }
}

/// The `direction` label: `inbound`, `outbound` or `other`.
pub fn direction_label(direction: &str) -> &str {
    match direction {
        "inbound" | "outbound" => direction,
        _ => OTHER_LABEL,
    }
}

/// A free-text label whose distinct values are capped: the first `cap`
/// values seen are kept as themselves, every later one is `other`.
///
/// Model names are chosen at run time (configuration, the router, a
/// `/models` switch), so no fixed list covers them; the cap is what keeps the
/// series count of each metric bounded.
pub struct BoundedLabel {
    cap: usize,
    admitted: parking_lot::Mutex<HashSet<Box<str>>>,
}

impl BoundedLabel {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            admitted: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    pub fn label<'a>(&self, value: &'a str) -> &'a str {
        if value.is_empty() || value.len() > MAX_FREE_TEXT_LABEL_LEN {
            return OTHER_LABEL;
        }
        let mut admitted = self.admitted.lock();
        if admitted.contains(value) {
            return value;
        }
        if admitted.len() < self.cap {
            admitted.insert(Box::from(value));
            return value;
        }
        OTHER_LABEL
    }
}

/// The label of a model outside the configured-model allowlist.
pub const UNLISTED_MODEL: &str = "unlisted";

/// The longest model label (one value per configured model).
pub const MAX_MODEL_LABEL_CHARS: usize = 64;

/// The builtin decision backend's model, always listed.
const BUILTIN_MODEL: &str = crate::decision::builtin::weights::MODEL_VERSION;

/// A configured-model allowlist. The process has one ([`ModelAllowlist::global`]),
/// which the configuration generation manager refreshes on every applied
/// generation and every metric reads; a manager can be given its own for
/// tests.
#[derive(Debug, Default)]
pub struct ModelAllowlist {
    models: RwLock<Option<HashSet<Box<str>>>>,
}

static GLOBAL_ALLOWLIST: std::sync::LazyLock<std::sync::Arc<ModelAllowlist>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(ModelAllowlist::default()));

impl ModelAllowlist {
    /// The process allowlist the metrics read.
    #[must_use]
    pub fn global() -> std::sync::Arc<Self> {
        std::sync::Arc::clone(&GLOBAL_ALLOWLIST)
    }

    /// Replace the allowlist with the models `config` names.
    pub fn refresh(&self, config: &crate::config::Config) {
        *self.models.write() = Some(allowlist(configured_models(config)));
    }

    /// The label of `model` against this allowlist.
    #[must_use]
    pub fn label<'a>(&self, model: &'a str) -> Cow<'a, str> {
        label_in(self.models.read().as_ref(), model)
    }
}

/// The models `config` names: the default model, every route, embedding
/// route, delegate agent, router and fallback model, and the decision
/// backend's pinned model.
#[must_use]
pub fn configured_models(config: &crate::config::Config) -> Vec<String> {
    let mut models: Vec<String> = config.default_model.iter().cloned().collect();
    models.extend(config.model_routes.iter().map(|route| route.model.clone()));
    models.extend(config.embedding_routes.iter().map(|route| route.model.clone()));
    models.extend(config.agents.values().map(|agent| agent.model.clone()));
    models.extend(config.router.models.iter().map(|model| model.model_id.clone()));
    for (model, fallbacks) in &config.reliability.model_fallbacks {
        models.push(model.clone());
        models.extend(fallbacks.iter().cloned());
    }
    models.push(config.causal_tree.backend.model.clone());
    models
}

/// The label of `model`: itself (cut to [`MAX_MODEL_LABEL_CHARS`]) when the
/// configuration names it or it is the builtin decision model, otherwise
/// [`UNLISTED_MODEL`].
#[must_use]
pub fn model_label(model: &str) -> Cow<'_, str> {
    GLOBAL_ALLOWLIST.label(model)
}

/// [`model_label`] against the allowlist `models`.
fn label_in<'a>(models: Option<&HashSet<Box<str>>>, model: &'a str) -> Cow<'a, str> {
    let model = model.trim();
    let listed = model == BUILTIN_MODEL || model == "builtin-v1" || models.is_some_and(|models| models.contains(model));
    if !listed || model.is_empty() {
        return Cow::Borrowed(UNLISTED_MODEL);
    }
    match model.char_indices().nth(MAX_MODEL_LABEL_CHARS) {
        Some((cut, _)) => Cow::Owned(model[..cut].to_string()),
        None => Cow::Borrowed(model),
    }
}

/// The allowlist of `models`: trimmed, empty names dropped.
fn allowlist(models: impl IntoIterator<Item = String>) -> HashSet<Box<str>> {
    models
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .map(String::into_boxed_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_configured_models_are_labels() {
        // The process-wide allowlist is replaced by every configuration
        // generation other tests build, so the rule is tested on its own set.
        let long = "m".repeat(80);
        let models = allowlist(["vendor/listed".to_string(), long.clone(), "  ".to_string()]);
        let label = |model| label_in(Some(&models), model);
        assert_eq!(models.len(), 2, "empty names are dropped");
        assert_eq!(label("vendor/listed"), "vendor/listed");
        assert_eq!(label(" vendor/listed "), "vendor/listed");
        assert_eq!(label("builtin-v1"), "builtin-v1");
        assert_eq!(label_in(None, "builtin-v1"), "builtin-v1");
        assert_eq!(label_in(None, "vendor/listed"), UNLISTED_MODEL);
        assert_eq!(label("vendor/claimed-by-a-response"), UNLISTED_MODEL);
        assert_eq!(label(""), UNLISTED_MODEL);
        assert_eq!(label(&long).chars().count(), MAX_MODEL_LABEL_CHARS);
    }

    #[test]
    fn the_configuration_names_its_models() {
        let mut config = crate::config::Config::default();
        config.default_model = Some("default-model".to_string());
        config.causal_tree.backend.model = "decider-2026-09".to_string();
        config
            .reliability
            .model_fallbacks
            .insert("primary".to_string(), vec!["fallback".to_string()]);
        let models = configured_models(&config);
        for expected in ["default-model", "decider-2026-09", "primary", "fallback"] {
            assert!(models.iter().any(|model| model == expected), "{expected}: {models:?}");
        }
    }
}

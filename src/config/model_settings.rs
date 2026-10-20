//! Persistent model choices validated against the embedded release catalog.
use crate::providers::catalog::{CatalogParameter, ModelParameter, ReleaseCatalog, parameters::ParameterLayer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

mod runtime;
pub(crate) use runtime::{runtime_layers, warnings};

/// A model level plus explicit parameter overrides in `[model_settings."<id>"]`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(flatten)]
    pub overrides: BTreeMap<String, Value>,
}

// Preserve malformed saved levels for diagnosis and repair. Runtime ignores
// them, while the transaction validator rejects new malformed selections.
impl<'de> Deserialize<'de> for ModelSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut overrides = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let level = match overrides.get("level") {
            Some(Value::String(value)) => Some(value.clone()),
            _ => None,
        };
        if level.is_some() || overrides.get("level").is_some_and(Value::is_null) {
            overrides.remove("level");
        }
        Ok(Self { level, overrides })
    }
}

impl std::fmt::Debug for ModelSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        crate::config::secret_paths::debug_redacted(f, "ModelSettings", self, "model_settings.<id>")
    }
}

impl ModelSettings {
    /// Refuse unsupported, malformed and out-of-range durable choices.
    pub fn validate(&self, id: &str) -> anyhow::Result<ParameterLayer> {
        let layer = self.parameter_layer(id)?;
        let release = ReleaseCatalog::embedded_ref()?;
        let model = release
            .models
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| anyhow::anyhow!("model settings require a canonical release catalog ID"))?;
        anyhow::ensure!(
            model.is_listed(),
            "retired model settings cannot be selected; migrate to an active replacement"
        );
        crate::providers::catalog::parameters::resolve(model, &release.revision, std::slice::from_ref(&layer))?;
        Ok(layer)
    }

    fn parameter_layer(&self, id: &str) -> anyhow::Result<ParameterLayer> {
        anyhow::ensure!(!self.overrides.contains_key("level"), "model level requires a string");
        let release = ReleaseCatalog::embedded_ref()?;
        let model = release
            .models
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| anyhow::anyhow!("model settings require a canonical release catalog ID"))?;
        if let Some(level) = &self.level
            && !model.levels.contains_key(level)
        {
            anyhow::bail!("unknown model level; allowed levels: low, medium, high");
        }
        let mut layer = ParameterLayer {
            level: self.level.clone(),
            ..Default::default()
        };
        for (name, value) in &self.overrides {
            let unsupported = || {
                let allowed = model
                    .parameters
                    .iter()
                    .filter(|(_, declaration)| !matches!(declaration, CatalogParameter::Unsupported))
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::anyhow!("model parameter is not supported; supported parameters: {allowed}")
            };
            let parameter = ModelParameter::parse(name).ok_or_else(unsupported)?;
            let declaration = model.parameters.get(name).ok_or_else(unsupported)?;
            if matches!(
                parameter,
                ModelParameter::MaxOutputTokens | ModelParameter::ThinkingBudget | ModelParameter::Seed
            ) && !value.is_i64()
                && !value.is_u64()
            {
                anyhow::bail!("parameter {name} requires an integer");
            }
            match declaration {
                CatalogParameter::Unsupported => return Err(unsupported()),
                CatalogParameter::Range { min, max, step, .. } => {
                    let number = value
                        .as_f64()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| anyhow::anyhow!("parameter {name} requires a finite number"))?;
                    if number < *min || number > *max {
                        anyhow::bail!("parameter {name} must be between {min} and {max}");
                    }
                    if let Some(step) = step {
                        let steps = (number - min) / step;
                        if (steps - steps.round()).abs() > 1e-8 {
                            anyhow::bail!("parameter {name} must follow step {step} from {min}");
                        }
                    }
                }
                CatalogParameter::Levels { levels, .. } => {
                    if !value
                        .as_str()
                        .is_some_and(|value| levels.iter().any(|level| level == value))
                    {
                        anyhow::bail!("parameter {name} must be one of {}", levels.join(", "));
                    }
                }
                CatalogParameter::Bool { .. } => {
                    if !value.is_boolean() {
                        anyhow::bail!("parameter {name} requires a boolean");
                    }
                }
            }
            layer.overrides.insert(parameter, value.clone());
        }
        Ok(layer)
    }
}

/// Flattened settings must not absorb misspelled configuration fields.
/// Values remain available for runtime compatibility with an older catalog.
pub(crate) fn validate_keys(config: &crate::config::Config) -> anyhow::Result<()> {
    for (index, route) in config.model_routes.iter().enumerate() {
        for name in route.settings.overrides.keys() {
            anyhow::ensure!(
                name == "level" || ModelParameter::parse(name).is_some(),
                "Unknown configuration path: model_routes.{index}.{name}; configuration is never silently ignored"
            );
        }
    }
    for (id, settings) in &config.model_settings {
        for name in settings.overrides.keys() {
            anyhow::ensure!(
                name == "level" || ModelParameter::parse(name).is_some(),
                "Unknown configuration path: model_settings.{id}.{name}; configuration is never silently ignored"
            );
        }
    }
    Ok(())
}

/// Select the route level before applying explicit model and route parameters.
/// An explicit model value survives a route that only selects a level (design §5).
pub(crate) fn parameter_layers(id: &str, settings: &[&ModelSettings]) -> anyhow::Result<Vec<ParameterLayer>> {
    let layers = settings
        .iter()
        .map(|settings| settings.parameter_layer(id))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(order_layers(layers))
}

fn order_layers(mut layers: Vec<ParameterLayer>) -> Vec<ParameterLayer> {
    let level = layers.iter().rev().find_map(|layer| layer.level.clone());
    for layer in &mut layers {
        layer.level = None;
    }
    if level.is_some() {
        layers.insert(
            0,
            ParameterLayer {
                level,
                ..Default::default()
            },
        );
    }
    layers
}

/// Validate changed settings at the shared transaction boundary.
pub(crate) fn validate_changes(
    current: Option<&crate::config::Config>,
    candidate: &crate::config::Config,
) -> Result<(), String> {
    validate_settings(current, candidate)
}

/// Existing retired/deleted settings remain stored but cannot justify new choices.
pub(crate) fn validate_existing(candidate: &crate::config::Config) -> Result<(), String> {
    validate_keys(candidate).map_err(|error| error.to_string())
}

fn validate_settings(current: Option<&crate::config::Config>, candidate: &crate::config::Config) -> Result<(), String> {
    let release = ReleaseCatalog::embedded_ref().map_err(|error| error.to_string())?;
    for route in &candidate.model_routes {
        let row = release.model(&crate::providers::ProviderId::of(&route.provider), &route.model);
        let model_settings = row.and_then(|row| candidate.model_settings.get(&row.id));
        if current.is_some_and(|current| {
            current.model_routes.iter().any(|old| {
                old.hint == route.hint
                    && old.provider == route.provider
                    && old.model == route.model
                    && old.settings == route.settings
            }) && row.is_none_or(|row| current.model_settings.get(&row.id) == model_settings)
        }) {
            continue;
        }
        if model_settings.is_some() || route.settings.level.is_some() || !route.settings.overrides.is_empty() {
            let row = row.ok_or_else(|| "route settings require a release catalog model".to_owned())?;
            let mut settings = Vec::new();
            if let Some(model) = model_settings {
                settings.push(model);
            }
            settings.push(&route.settings);
            let layers = parameter_layers(&row.id, &settings).map_err(|error| error.to_string())?;
            crate::providers::catalog::parameters::resolve(row, &release.revision, &layers)
                .map_err(|error| error.to_string())?;
        }
    }
    for (id, settings) in &candidate.model_settings {
        if current.and_then(|config| config.model_settings.get(id)) != Some(settings) {
            settings.validate(id).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_settings_loader_refuses_misspelled_route_and_model_keys_without_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let secret = ["prx_loader_", "private_value"].concat();
        for model_settings in [false, true] {
            let mut config = crate::config::Config::default();
            let settings = ModelSettings {
                level: None,
                overrides: [("api_keyy".into(), json!(secret))].into(),
            };
            if model_settings {
                config.model_settings.insert("openai/gpt-5.6".into(), settings);
            } else {
                config.model_routes.push(crate::config::ModelRouteConfig {
                    hint: "prx_route".into(),
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                    settings,
                    api_key: None,
                    credential_ref: None,
                });
            }
            std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
            let error = crate::config::Config::load_from_path(&path, dir.path().join("workspace")).unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("Unknown configuration path"), "{error}");
            assert!(error.contains("api_keyy"), "{error}");
            if model_settings {
                assert!(error.contains("model_settings.openai/gpt-5.6.api_keyy"), "{error}");
                assert!(!error.contains("<id>"), "{error}");
            }
            assert!(!error.contains(&secret), "{error}");
        }
    }

    #[test]
    fn model_settings_loader_keeps_known_flat_route_parameters() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_temperature=0.7\n[[model_routes]]\nhint='prx_route'\nprovider='openai'\nmodel='gpt-5.6'\nlevel='high'\nmax_output_tokens=8192\n").unwrap();
        let config = crate::config::Config::load_from_path(&path, dir.path().join("workspace")).unwrap();
        let route = config.model_routes.first().unwrap();
        assert_eq!(route.settings.level.as_deref(), Some("high"));
        assert_eq!(route.settings.overrides.get("max_output_tokens"), Some(&json!(8192)));
    }

    #[test]
    fn loader_keeps_malformed_saved_levels_available_for_repair() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_temperature=0.7\n[model_settings.\"openai/gpt-5.6\"]\nlevel=42\n[[model_routes]]\nhint='prx_route'\nprovider='openai'\nmodel='gpt-5.6'\nlevel=false\n").unwrap();
        let config = crate::config::Config::load_from_path(&path, dir.path().join("workspace")).unwrap();
        let model = config.model_settings.get("openai/gpt-5.6").unwrap();
        let route = &config.model_routes.first().unwrap().settings;
        assert_eq!(model.overrides.get("level"), Some(&json!(42)));
        assert_eq!(route.overrides.get("level"), Some(&json!(false)));
        assert!(
            warnings(&config)
                .unwrap()
                .iter()
                .any(|note| note.contains("requires a string"))
        );
        assert!(model.validate("openai/gpt-5.6").is_err());
        assert!(route.validate("openai/gpt-5.6").is_err());
    }

    #[test]
    fn model_settings_roundtrip_as_flat_toml_parameters() {
        let mut config = crate::config::Config::default();
        let settings = ModelSettings {
            level: Some("high".into()),
            overrides: [("max_output_tokens".into(), json!(64000))].into(),
        };
        config.model_settings.insert("openai/gpt-5.6".into(), settings.clone());
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[model_settings.\"openai/gpt-5.6\"]"));
        let loaded: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(loaded.model_settings.get("openai/gpt-5.6"), Some(&settings));
    }

    #[test]
    fn durable_model_settings_refuse_instead_of_clamping_or_dropping_invalid_values() {
        for settings in [
            json!({"level":"extra"}),
            json!({"temperature":0.5}),
            json!({"max_output_tokens":0}),
            json!({"max_output_tokens":0.5}),
            json!({"reasoning_effort":"future"}),
            json!({"future_parameter":1}),
            json!({"verbosity":true}),
        ] {
            let settings: ModelSettings = serde_json::from_value(settings).unwrap();
            assert!(settings.validate("openai/gpt-5.6").is_err());
        }
        let settings: ModelSettings = serde_json::from_value(json!({"level":"high","reasoning_effort":"low"})).unwrap();
        let layer = settings.validate("openai/gpt-5.6").unwrap();
        assert_eq!(layer.level.as_deref(), Some("high"));
        assert_eq!(
            layer.overrides.get(&ModelParameter::ReasoningEffort),
            Some(&json!("low"))
        );
        assert!(settings.validate("unverified/model").is_err());
    }
    #[test]
    fn invalid_model_parameters_do_not_leak_credentials_through_debug() {
        let settings: ModelSettings = serde_json::from_value(json!({
            "level":"high", "api_key":"prx_parameter_debug_secret"
        }))
        .expect("test: unvalidated model settings");
        assert!(settings.validate("openai/gpt-5.6").is_err());
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("prx_parameter_debug_secret"), "{rendered}");
        assert!(rendered.contains("high"), "valid non-secret choices remain visible");
    }
}

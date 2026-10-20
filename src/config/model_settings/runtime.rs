//! Saved selections survive catalog changes; new PMM plans remain strict.
use super::{ModelSettings, order_layers};
use crate::providers::catalog::{CatalogParameter, ModelParameter, ReleaseCatalog, ReleaseModel, parameters};
use parameters::ParameterLayer;

fn prepare(model: &ReleaseModel, settings: &[&ModelSettings]) -> (Vec<ParameterLayer>, Vec<String>) {
    if settings
        .iter()
        .all(|settings| settings.level.is_none() && settings.overrides.is_empty())
    {
        return (Vec::new(), Vec::new());
    }
    let mut notes = Vec::new();
    let mut layers = Vec::new();
    for settings in settings {
        let mut layer = ParameterLayer::default();
        if let Some(level) = &settings.level {
            if model.levels.contains_key(level) {
                layer.level = Some(level.clone());
            } else {
                notes.push(format!(
                    "{}: saved level is no longer declared and is ignored",
                    model.id
                ));
            }
        }
        for (name, value) in &settings.overrides {
            if name == "level" {
                notes.push(format!("{}: saved level requires a string and is ignored", model.id));
                continue;
            }
            let Some(parameter) = ModelParameter::parse(name) else {
                // Loader and PMM reject unknown keys. In-memory legacy
                // generations still cannot inject arbitrary request fields.
                notes.push(format!("{}: unknown saved parameter is ignored", model.id));
                continue;
            };
            let integer = matches!(
                parameter,
                ModelParameter::MaxOutputTokens | ModelParameter::ThinkingBudget | ModelParameter::Seed
            );
            let valid = match model.parameters.get(name) {
                Some(CatalogParameter::Range { min, max, step, .. }) => {
                    value.as_f64().filter(|n| n.is_finite()).is_some_and(|number| {
                        if integer && !value.is_i64() && !value.is_u64() {
                            false
                        } else {
                            if number < *min
                                || number > *max
                                || step.is_some_and(|step| {
                                    let n = (number - min) / step;
                                    (n - n.round()).abs() > 1e-8
                                })
                            {
                                notes.push(format!(
                                    "{}: saved {name} is clamped to the current catalog range",
                                    model.id
                                ));
                            }
                            true
                        }
                    })
                }
                Some(CatalogParameter::Levels { levels, .. }) => value
                    .as_str()
                    .is_some_and(|value| levels.iter().any(|level| level == value)),
                Some(CatalogParameter::Bool { .. }) => value.is_boolean(),
                Some(CatalogParameter::Unsupported) | None => false,
            };
            if valid {
                layer.overrides.insert(parameter, value.clone());
            } else {
                notes.push(format!(
                    "{}: saved {name} is invalid or no longer supported and is ignored",
                    model.id
                ));
            }
        }
        layers.push(layer);
    }
    let mut layers = order_layers(layers);
    // A saved thinking budget may outlive a lower output cap in a newer
    // release. Drop that optional budget, retaining the output boundary.
    if parameters::resolve(model, "runtime-validation", &layers).is_err() {
        for layer in &mut layers {
            layer.overrides.remove(&ModelParameter::ThinkingBudget);
        }
        notes.push(format!("{}: incompatible saved parameters are ignored", model.id));
        if parameters::resolve(model, "runtime-validation", &layers).is_err() {
            layers.clear();
        }
    }
    (layers, notes)
}

pub(crate) fn runtime_layers(model: &ReleaseModel, settings: &[&ModelSettings]) -> Vec<ParameterLayer> {
    let (layers, notes) = prepare(model, settings);
    for warning in notes {
        tracing::warn!(%warning, "saved model setting adjusted for the current release");
    }
    layers
}

pub(crate) fn warnings(config: &crate::config::Config) -> anyhow::Result<Vec<String>> {
    let release = ReleaseCatalog::embedded_ref()?;
    let mut notes = Vec::new();
    for (id, settings) in &config.model_settings {
        if let Some(model) = release.models.iter().find(|model| model.id == *id) {
            notes.extend(prepare(model, &[settings]).1);
        } else {
            notes.push("saved settings for a model absent from the release are ignored".into());
        }
    }
    for route in &config.model_routes {
        if let Some(model) = release.model(&crate::providers::ProviderId::of(&route.provider), &route.model) {
            let mut settings = Vec::new();
            if let Some(saved) = config.model_settings.get(&model.id) {
                settings.push(saved);
            }
            settings.push(&route.settings);
            notes.extend(prepare(model, &settings).1);
        } else if route.settings.level.is_some() || !route.settings.overrides.is_empty() {
            notes.push("route settings for a model absent from the release are ignored".into());
        }
    }
    notes.sort();
    notes.dedup();
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stale_saved_layers_drop_invalid_fields_clamp_limits_and_preserve_precedence() {
        let release = ReleaseCatalog::embedded_ref().unwrap();
        let model = release.models.iter().find(|row| row.id == "openai/gpt-5.6").unwrap();
        let model_settings: ModelSettings = serde_json::from_value(
            json!({"level":"future", "max_output_tokens":0,"reasoning_effort":"invalid","temperature":0.7}),
        )
        .unwrap();
        let route: ModelSettings =
            serde_json::from_value(json!({"reasoning_effort":"high","verbosity":false})).unwrap();
        let (layers, notes) = prepare(model, &[&model_settings, &route]);
        let result = parameters::resolve(model, &release.revision, &layers).unwrap();
        assert_eq!(result.values.get(&ModelParameter::MaxOutputTokens), Some(&json!(1)));
        assert_eq!(
            result.values.get(&ModelParameter::ReasoningEffort),
            Some(&json!("high"))
        );
        assert!(!result.values.contains_key(&ModelParameter::Temperature));
        assert_ne!(notes, [] as [String; 0]);
        assert!(model_settings.validate(&model.id).is_err(), "new plans stay strict");
    }

    #[test]
    fn malformed_saved_level_survives_for_repair_but_new_plans_reject_it() {
        let release = ReleaseCatalog::embedded_ref().unwrap();
        let model = release.models.iter().find(|row| row.id == "openai/gpt-5.6").unwrap();
        let saved: ModelSettings = serde_json::from_value(json!({"level":42})).unwrap();
        assert_eq!(serde_json::to_value(&saved).unwrap(), json!({"level":42}));
        assert!(saved.validate(&model.id).is_err());
        let (layers, notes) = prepare(model, &[&saved]);
        assert!(parameters::resolve(model, &release.revision, &layers).is_ok());
        assert!(notes.iter().any(|note| note.contains("requires a string")));
    }

    #[test]
    fn incompatible_saved_thinking_budget_does_not_break_request_resolution() {
        let release = ReleaseCatalog::embedded_ref().unwrap();
        let mut model = release
            .models
            .iter()
            .find(|row| row.id == "anthropic/claude-3.5-sonnet")
            .unwrap()
            .clone();
        model.parameters.insert(
            "thinking_budget".into(),
            CatalogParameter::Range {
                min: 1024.0,
                max: 16384.0,
                step: None,
                default: None,
                excludes: Vec::new(),
            },
        );
        let saved: ModelSettings =
            serde_json::from_value(json!({"max_output_tokens":1,"thinking_budget":8192})).unwrap();
        let (layers, notes) = prepare(&model, &[&saved]);
        assert!(parameters::resolve(&model, &release.revision, &layers).is_ok());
        let resolved = parameters::resolve(&model, &release.revision, &layers).unwrap();
        assert!(!resolved.values.contains_key(&ModelParameter::ThinkingBudget));
        assert_eq!(resolved.values.get(&ModelParameter::MaxOutputTokens), Some(&json!(1)));
        assert_ne!(notes, [] as [String; 0]);
    }
}

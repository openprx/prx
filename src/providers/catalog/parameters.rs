//! Resolve the closed parameter vocabulary before adapter wire mapping.

use super::{CatalogParameter, ModelParameter, ReleaseModel};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// One increasingly specific source of request settings. Callers pass model
/// settings, route settings and explicit call settings in that order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParameterLayer {
    pub level: Option<String>,
    #[serde(default)]
    pub overrides: BTreeMap<ModelParameter, Value>,
}

/// Canonical values and the catalog identity that justified them. Adapters
/// map only these keys; no catalog string becomes an arbitrary request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedParameters {
    pub model: String,
    pub catalog_revision: String,
    pub values: BTreeMap<ModelParameter, Value>,
}

/// Resolve defaults, default level, then the supplied layers. Unsupported
/// values are dropped, numeric values are clamped, and malformed typed values
/// fail locally. A later source wins mutual exclusions in either direction.
pub fn resolve(model: &ReleaseModel, revision: &str, layers: &[ParameterLayer]) -> anyhow::Result<ResolvedParameters> {
    let mut result = ResolvedParameters {
        model: model.id.clone(),
        catalog_revision: revision.to_owned(),
        values: BTreeMap::new(),
    };
    for (name, declaration) in &model.parameters {
        let Some(parameter) = ModelParameter::parse(name) else {
            continue;
        };
        let default = match declaration {
            CatalogParameter::Range { default, .. } => default.clone(),
            CatalogParameter::Levels { default, .. } => default.clone().map(Value::String),
            CatalogParameter::Bool { default, .. } => default.map(Value::Bool),
            CatalogParameter::Unsupported => None,
        };
        if let Some(value) = default {
            insert(model, &mut result.values, parameter, value)?;
        }
    }
    if let Some(level) = &model.default_level {
        apply_level(model, &mut result.values, level)?;
    }
    for layer in layers {
        if let Some(level) = &layer.level {
            apply_level(model, &mut result.values, level)?;
        }
        for (parameter, value) in &layer.overrides {
            insert(model, &mut result.values, *parameter, value.clone())?;
        }
    }
    if matches!(model.provider.as_str(), "anthropic" | "bedrock")
        && let (Some(output), Some(thinking)) = (
            result
                .values
                .get(&ModelParameter::MaxOutputTokens)
                .and_then(Value::as_u64),
            result
                .values
                .get(&ModelParameter::ThinkingBudget)
                .and_then(Value::as_u64),
        )
    {
        anyhow::ensure!(output > thinking, "max_output_tokens must exceed thinking_budget");
    }
    Ok(result)
}

fn apply_level(model: &ReleaseModel, values: &mut BTreeMap<ModelParameter, Value>, level: &str) -> anyhow::Result<()> {
    let settings = model
        .levels
        .get(level)
        .ok_or_else(|| anyhow::anyhow!("unknown model level; select one declared in the release catalog"))?;
    for (name, value) in settings {
        if let Some(parameter) = ModelParameter::parse(name) {
            insert(model, values, parameter, value.clone())?;
        }
    }
    Ok(())
}

fn excludes(declaration: &CatalogParameter, name: &str) -> bool {
    match declaration {
        CatalogParameter::Range { excludes, .. }
        | CatalogParameter::Levels { excludes, .. }
        | CatalogParameter::Bool { excludes, .. } => excludes.iter().any(|excluded| excluded == name),
        CatalogParameter::Unsupported => false,
    }
}

fn insert(
    model: &ReleaseModel,
    values: &mut BTreeMap<ModelParameter, Value>,
    parameter: ModelParameter,
    value: Value,
) -> anyhow::Result<()> {
    let name = parameter.as_str();
    let Some(declaration) = model.parameters.get(name) else {
        tracing::debug!(parameter = name, "dropping undeclared model parameter");
        return Ok(());
    };
    let value = match declaration {
        CatalogParameter::Unsupported => {
            tracing::debug!(parameter = name, "dropping unsupported model parameter");
            return Ok(());
        }
        CatalogParameter::Range { min, max, step, .. } => {
            let number = value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| anyhow::anyhow!("model parameter {name} requires a finite number"))?;
            let mut bounded = number.clamp(*min, *max);
            if let Some(step) = step {
                let stepped = (min + ((bounded - min) / step).round() * step).clamp(*min, *max);
                // Preserve an already aligned value instead of manufacturing
                // float noise and reporting it as a real clamp.
                if (stepped - bounded).abs() > f64::EPSILON * bounded.abs().max(1.0) * 8.0 {
                    bounded = stepped;
                }
            }
            if matches!(
                parameter,
                ModelParameter::MaxOutputTokens | ModelParameter::ThinkingBudget | ModelParameter::Seed
            ) {
                if number.fract() != 0.0
                    || bounded.fract() != 0.0
                    || value.as_i64().is_none() && value.as_u64().is_none()
                    || !(-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&bounded)
                {
                    anyhow::bail!("model parameter {name} requires an integer");
                }
                if bounded != number {
                    tracing::warn!(parameter = name, "clamped model parameter to its catalog range");
                }
                Value::from(bounded as i64)
            } else {
                if bounded != number {
                    if min == max {
                        tracing::debug!(parameter = name, "applied fixed catalog parameter");
                    } else {
                        tracing::warn!(parameter = name, "clamped model parameter to its catalog range");
                    }
                }
                Value::from(bounded)
            }
        }
        CatalogParameter::Levels { levels, .. } => {
            if !value
                .as_str()
                .is_some_and(|value| levels.iter().any(|level| level == value))
            {
                if parameter == ModelParameter::ReasoningEffort
                    && let Some(effort) = value.as_str().and_then(|effort| clamp_effort(effort, levels))
                {
                    tracing::warn!(parameter = name, "clamped reasoning effort to its catalog levels");
                    Value::from(effort)
                } else {
                    anyhow::bail!("model parameter {name} requires a declared value");
                }
            } else {
                value
            }
        }
        CatalogParameter::Bool { .. } => {
            if !value.is_boolean() {
                anyhow::bail!("model parameter {name} requires a boolean");
            }
            value
        }
    };
    values.retain(|existing, _| {
        *existing == parameter
            || !excludes(declaration, existing.as_str())
                && !model
                    .parameters
                    .get(existing.as_str())
                    .is_some_and(|other| excludes(other, name))
    });
    values.insert(parameter, value);
    Ok(())
}

/// Common reasoning values have a stable semantic order. Each model's allowed
/// subset remains release data; an unknown string is never guessed.
fn clamp_effort<'a>(requested: &str, allowed: &'a [String]) -> Option<&'a str> {
    const ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let requested = ORDER.iter().position(|value| *value == requested)?;
    allowed
        .iter()
        .filter_map(|value| {
            ORDER
                .iter()
                .position(|candidate| *candidate == value)
                .map(|index| (index.abs_diff(requested), std::cmp::Reverse(index), value.as_str()))
        })
        .min_by_key(|(distance, index, _)| (*distance, *index))
        .map(|(_, _, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::ReleaseCatalog;
    use serde_json::json;

    fn model() -> ReleaseModel {
        let catalog = ReleaseCatalog::embedded_ref().expect("test: release catalog");
        let mut model = catalog.models.first().expect("test: release model").clone();
        model.parameters = serde_json::from_value(json!({
            "temperature": {"kind":"range","min":0,"max":1,"default":0.7,"step":0.1},
            "max_output_tokens": {"kind":"range","min":1,"max":8192,"default":4096},
            "thinking_budget": {"kind":"range","min":1024,"max":4096,"excludes":["temperature"]},
            "reasoning_effort": {"kind":"levels","levels":["low","medium","high"],"default":"medium"},
            "service_tier": {"kind":"unsupported"},
            "future_parameter": {"kind":"bool","default":true}
        }))
        .expect("test: declarations");
        model.levels = serde_json::from_value(json!({
            "low":{"max_output_tokens":1024,"reasoning_effort":"low"},
            "medium":{"max_output_tokens":4096},
            "high":{"max_output_tokens":8192,"thinking_budget":2048,"reasoning_effort":"high"}
        }))
        .expect("test: levels");
        model.default_level = Some("medium".into());
        model
    }

    fn layer(level: Option<&str>, overrides: Value) -> ParameterLayer {
        ParameterLayer {
            level: level.map(str::to_owned),
            overrides: serde_json::from_value(overrides).expect("test: overrides"),
        }
    }

    #[test]
    fn precedence_applies_model_route_and_caller_after_defaults() {
        let result = resolve(
            &model(),
            "prx-test-revision",
            &[
                layer(Some("high"), json!({"max_output_tokens":7000})),
                layer(Some("low"), json!({"max_output_tokens":2000})),
                layer(None, json!({"max_output_tokens":3000})),
            ],
        )
        .expect("test: resolved");
        assert_eq!(
            result.values.get(&ModelParameter::MaxOutputTokens).unwrap().clone(),
            3000
        );
        assert_eq!(
            result.values.get(&ModelParameter::ReasoningEffort).unwrap().clone(),
            "low"
        );
        assert_eq!(result.catalog_revision, "prx-test-revision");
        assert_eq!(result.model, model().id);
    }

    #[test]
    fn exclusions_apply_in_both_directions_and_follow_source_precedence() {
        let high = resolve(&model(), "test", &[layer(Some("high"), json!({}))]).unwrap();
        assert!(!high.values.contains_key(&ModelParameter::Temperature));
        assert_eq!(high.values.get(&ModelParameter::ThinkingBudget).unwrap().clone(), 2048);
        let caller = resolve(
            &model(),
            "test",
            &[layer(Some("high"), json!({})), layer(None, json!({"temperature":0.5}))],
        )
        .unwrap();
        assert!(!caller.values.contains_key(&ModelParameter::ThinkingBudget));
        assert_eq!(caller.values.get(&ModelParameter::Temperature).unwrap().clone(), 0.5);
    }

    #[test]
    fn unsupported_and_future_parameters_never_enter_the_result() {
        let result = resolve(
            &model(),
            "test",
            &[layer(None, json!({"service_tier":"auto","seed":42}))],
        )
        .unwrap();
        assert!(!result.values.contains_key(&ModelParameter::ServiceTier));
        assert!(!result.values.contains_key(&ModelParameter::Seed));
        assert_eq!(result.values.len(), 3);
    }

    #[test]
    fn numeric_clamping_preserves_integer_wire_types_and_steps() {
        let result = resolve(
            &model(),
            "test",
            &[layer(None, json!({"temperature":3,"max_output_tokens":9000}))],
        )
        .unwrap();
        assert_eq!(result.values.get(&ModelParameter::Temperature).unwrap().clone(), 1.0);
        assert_eq!(
            result
                .values
                .get(&ModelParameter::MaxOutputTokens)
                .unwrap()
                .clone()
                .as_u64(),
            Some(8192)
        );
        let stepped = resolve(&model(), "test", &[layer(None, json!({"temperature":0.76}))]).unwrap();
        assert!(
            (stepped
                .values
                .get(&ModelParameter::Temperature)
                .unwrap()
                .clone()
                .as_f64()
                .unwrap()
                - 0.8)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn malformed_types_unknown_levels_and_fractional_token_counts_fail_locally() {
        for settings in [
            layer(Some("absent"), json!({})),
            layer(None, json!({"temperature":"high"})),
            layer(None, json!({"reasoning_effort":"absent"})),
            layer(None, json!({"max_output_tokens":12.5})),
            layer(None, json!({"max_output_tokens":0.5})),
        ] {
            assert!(resolve(&model(), "test", &[settings]).is_err());
        }
    }

    #[test]
    fn oversized_integer_input_clamps_to_the_declared_output_limit() {
        let result = resolve(
            &model(),
            "test",
            &[layer(None, json!({"max_output_tokens":9_007_199_254_740_992_u64}))],
        )
        .unwrap();
        assert_eq!(
            result.values.get(&ModelParameter::MaxOutputTokens).unwrap().clone(),
            8192
        );
    }
    #[test]
    fn aligned_decimal_step_does_not_change_or_warn_about_the_value() {
        let result = resolve(&model(), "test", &[layer(None, json!({"temperature":0.3}))]).unwrap();
        assert_eq!(*result.values.get(&ModelParameter::Temperature).unwrap(), json!(0.3));
    }

    #[test]
    fn minimal_effort_tie_keeps_reasoning_enabled() {
        assert_eq!(clamp_effort("minimal", &["none".into(), "low".into()]), Some("low"));
    }

    #[test]
    fn anthropic_thinking_budget_must_fit_inside_the_output_cap() {
        for provider in ["anthropic", "bedrock"] {
            let mut row = model();
            row.provider = provider.into();
            assert!(
                resolve(
                    &row,
                    "test",
                    &[layer(None, json!({"max_output_tokens":2048,"thinking_budget":2048}))]
                )
                .is_err()
            );
            assert!(
                resolve(
                    &row,
                    "test",
                    &[layer(None, json!({"max_output_tokens":2049,"thinking_budget":2048}))]
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn gemini_output_cutoff_does_not_require_a_smaller_thinking_budget() {
        let mut row = model();
        row.provider = "gemini".into();
        assert!(
            resolve(
                &row,
                "test",
                &[layer(None, json!({"max_output_tokens":2048,"thinking_budget":2048}))]
            )
            .is_ok()
        );
    }
}

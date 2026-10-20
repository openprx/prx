//! Adapter-owned mappings from canonical parameters to API request fields.

use super::ProviderId;
use super::catalog::{CatalogParameter, ModelParameter, ReleaseCatalog, parameters};
use serde::{Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    OpenAiChat,
    Responses,
    Anthropic,
    Gemini,
    OpenRouter,
    Compatible,
    Ollama,
    Bedrock,
}

impl Format {
    const fn for_family(family: super::catalog::AdapterFamily) -> Option<Self> {
        use super::catalog::AdapterFamily;
        match family {
            AdapterFamily::OpenAi => Some(Self::OpenAiChat),
            AdapterFamily::OpenAiCodex => Some(Self::Responses),
            AdapterFamily::Anthropic => Some(Self::Anthropic),
            AdapterFamily::Gemini => Some(Self::Gemini),
            AdapterFamily::OpenRouter => Some(Self::OpenRouter),
            AdapterFamily::OpenAiCompatible => Some(Self::Compatible),
            AdapterFamily::Ollama => Some(Self::Ollama),
            AdapterFamily::Bedrock => Some(Self::Bedrock),
            AdapterFamily::Copilot
            | AdapterFamily::Mock
            | AdapterFamily::WasmPlugin
            | AdapterFamily::TypeSafeDecision => None,
        }
    }
    const fn path(self, parameter: ModelParameter) -> Option<&'static [&'static str]> {
        use ModelParameter::{
            MaxOutputTokens, ReasoningEffort, Seed, ServiceTier, Temperature, ThinkingBudget, TopP, Verbosity,
        };
        match (self, parameter) {
            (Self::OpenAiChat, MaxOutputTokens) => Some(&["max_completion_tokens"]),
            (Self::Responses, MaxOutputTokens) => Some(&["max_output_tokens"]),
            (Self::Responses | Self::OpenRouter, ReasoningEffort) => Some(&["reasoning", "effort"]),
            (Self::OpenRouter, ThinkingBudget) => Some(&["reasoning", "max_tokens"]),
            (Self::Responses, Verbosity) => Some(&["text", "verbosity"]),
            (Self::Anthropic, MaxOutputTokens) => Some(&["max_tokens"]),
            (Self::Anthropic, ThinkingBudget) => Some(&["thinking", "budget_tokens"]),
            (Self::Anthropic, ReasoningEffort) => Some(&["output_config", "effort"]),
            (Self::Gemini, Temperature) => Some(&["generationConfig", "temperature"]),
            (Self::Gemini, TopP) => Some(&["generationConfig", "topP"]),
            (Self::Gemini, MaxOutputTokens) => Some(&["generationConfig", "maxOutputTokens"]),
            (Self::Gemini, ThinkingBudget) => Some(&["generationConfig", "thinkingConfig", "thinkingBudget"]),
            (Self::Gemini, ReasoningEffort) => Some(&["generationConfig", "thinkingConfig", "thinkingLevel"]),
            (Self::Gemini, Seed) => Some(&["generationConfig", "seed"]),
            (Self::Ollama, Temperature) => Some(&["options", "temperature"]),
            (Self::Ollama, TopP) => Some(&["options", "top_p"]),
            (Self::Ollama, MaxOutputTokens) => Some(&["options", "num_predict"]),
            (Self::Ollama, Seed) => Some(&["options", "seed"]),
            (Self::Ollama, ReasoningEffort) => Some(&["think"]),
            (Self::Bedrock, Temperature) => Some(&["inferenceConfig", "temperature"]),
            (Self::Bedrock, TopP) => Some(&["inferenceConfig", "topP"]),
            (Self::Bedrock, MaxOutputTokens) => Some(&["inferenceConfig", "maxTokens"]),
            (Self::Bedrock, ThinkingBudget) => Some(&["additionalModelRequestFields", "thinking", "budget_tokens"]),
            (Self::Bedrock, ReasoningEffort) => Some(&["additionalModelRequestFields", "output_config", "effort"]),
            (Self::OpenAiChat | Self::Compatible, ReasoningEffort) => Some(&["reasoning_effort"]),
            (Self::OpenAiChat | Self::OpenRouter | Self::Compatible, Verbosity) => Some(&["verbosity"]),
            (Self::OpenRouter | Self::Compatible, MaxOutputTokens) => Some(&["max_tokens"]),
            (
                Self::OpenAiChat | Self::Responses | Self::Anthropic | Self::OpenRouter | Self::Compatible,
                Temperature,
            ) => Some(&["temperature"]),
            (Self::OpenAiChat | Self::Responses | Self::Anthropic | Self::OpenRouter | Self::Compatible, TopP) => {
                Some(&["top_p"])
            }
            (
                Self::OpenAiChat | Self::Responses | Self::Anthropic | Self::OpenRouter | Self::Compatible,
                ServiceTier,
            ) => Some(&["service_tier"]),
            (Self::OpenAiChat | Self::OpenRouter | Self::Compatible, Seed) => Some(&["seed"]),
            _ => None,
        }
    }
}

/// Check parameter expressibility while a selection is still pure and local.
/// Missing release facts preserve the resolver's existing legacy-ID policy.
pub(crate) fn check_selection(
    catalog: &super::catalog::CatalogView,
    provider: &ProviderId,
    model: &str,
) -> Result<(), super::catalog::ResolutionError> {
    let Ok(release) = catalog.release_catalog() else {
        return Ok(());
    };
    let Some(row) = release.model(provider, model) else {
        return Ok(());
    };
    let format = super::catalog::AdapterFamily::of(provider).and_then(Format::for_family);
    for (name, declaration) in &row.parameters {
        let Some(parameter) = ModelParameter::parse(name) else {
            continue;
        };
        if !matches!(declaration, CatalogParameter::Unsupported)
            && format.and_then(|format| format.path(parameter)).is_none()
        {
            return Err(super::catalog::ResolutionError::ModelIncompatible {
                provider: provider.to_string(),
                model: model.to_owned(),
                detail: format!("adapter cannot express model parameter {}", parameter.as_str()),
            });
        }
    }
    parameters::resolve(row, &release.revision, &[]).map_err(|error| {
        super::catalog::ResolutionError::ModelIncompatible {
            provider: provider.to_string(),
            model: model.to_owned(),
            detail: error.to_string(),
        }
    })?;
    Ok(())
}

/// Serialization wrapper shared by ordinary, accounted and streaming sends.
/// Unknown model identities use the base request; known rows send only their
/// declared canonical values. Empty declarations are unverified and preserve
/// the adapter base request, like an unknown model identity.
pub(crate) struct Request<'a, T: ?Sized> {
    provider: ParameterProvider<'a>,
    format: Format,
    model: &'a str,
    body: &'a T,
    record: bool,
}

#[derive(Clone, Copy)]
enum ParameterProvider<'a> {
    Label(&'a str),
    Canonical(&'a ProviderId),
}

pub(crate) const fn request_for<'a, T: Serialize + ?Sized>(
    provider: &'a ProviderId,
    format: Format,
    model: &'a str,
    body: &'a T,
) -> Request<'a, T> {
    Request {
        provider: ParameterProvider::Canonical(provider),
        format,
        model,
        body,
        record: true,
    }
}

pub(crate) const fn request<'a, T: Serialize + ?Sized>(
    provider: &'a str,
    format: Format,
    model: &'a str,
    body: &'a T,
) -> Request<'a, T> {
    Request {
        provider: ParameterProvider::Label(provider),
        format,
        model,
        body,
        record: true,
    }
}

impl<T: Serialize + ?Sized> Serialize for Request<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let catalog = ReleaseCatalog::embedded_ref().map_err(S::Error::custom)?;
        let provider = match self.provider {
            ParameterProvider::Label(label) => std::borrow::Cow::Owned(ProviderId::of(label)),
            ParameterProvider::Canonical(provider) => std::borrow::Cow::Borrowed(provider),
        };
        let Some(model) = catalog.model(&provider, self.model) else {
            return self.body.serialize(serializer);
        };
        if model.parameters.is_empty() {
            return self.body.serialize(serializer);
        }
        encode_recorded(self.body, self.format, model, &catalog.revision, self.record)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

/// A transport retry keeps the first preparation's provenance exactly once.
pub(crate) const fn retry_request<'a, T: Serialize + ?Sized>(
    provider: &'a str,
    format: Format,
    model: &'a str,
    body: &'a T,
) -> Request<'a, T> {
    Request {
        provider: ParameterProvider::Label(provider),
        format,
        model,
        body,
        record: false,
    }
}

#[cfg(test)]
fn encode<T: Serialize + ?Sized>(
    original: &T,
    format: Format,
    model: &super::catalog::ReleaseModel,
    revision: &str,
) -> anyhow::Result<Value> {
    encode_recorded(original, format, model, revision, true)
}

fn encode_recorded<T: Serialize + ?Sized>(
    original: &T,
    format: Format,
    model: &super::catalog::ReleaseModel,
    revision: &str,
    record: bool,
) -> anyhow::Result<Value> {
    let mut body = serde_json::to_value(original)?;
    // OAuth Gemini transports wrap generationConfig inside `request`.
    let target = if matches!(format, Format::Gemini) && body.get("request").is_some() {
        body.get_mut("request")
            .ok_or_else(|| anyhow::anyhow!("Gemini OAuth request envelope is missing"))?
    } else {
        &mut body
    };
    let mut caller = parameters::ParameterLayer::default();
    for parameter in [
        ModelParameter::Temperature,
        ModelParameter::TopP,
        ModelParameter::MaxOutputTokens,
        ModelParameter::ReasoningEffort,
        ModelParameter::ThinkingBudget,
        ModelParameter::Verbosity,
        ModelParameter::ServiceTier,
        ModelParameter::Seed,
    ] {
        if let Some(path) = format.path(parameter)
            && let Some(value) = get(target, path)
        {
            // Ollama's adapter owns its explicit boolean switch. It is not
            // a canonical string effort and needs no catalog declaration.
            if matches!(format, Format::Ollama) && parameter == ModelParameter::ReasoningEffort && value.is_boolean() {
                continue;
            }
            caller.overrides.insert(parameter, value.clone());
        }
    }
    let resolved = parameters::resolve(model, revision, &[caller])?;
    // Effort alone must never leave a partial/adaptive thinking object in an
    // Anthropic request. Budget-based thinking is emitted only when resolved.
    if !resolved.values.contains_key(&ModelParameter::ThinkingBudget) {
        match format {
            Format::Anthropic => remove(target, &["thinking"]),
            Format::Bedrock => remove(target, &["additionalModelRequestFields", "thinking"]),
            _ => {}
        }
    }
    for parameter in [
        ModelParameter::Temperature,
        ModelParameter::TopP,
        ModelParameter::MaxOutputTokens,
        ModelParameter::ReasoningEffort,
        ModelParameter::ThinkingBudget,
        ModelParameter::Verbosity,
        ModelParameter::ServiceTier,
        ModelParameter::Seed,
    ] {
        if let Some(path) = format.path(parameter) {
            if matches!(format, Format::Ollama)
                && parameter == ModelParameter::ReasoningEffort
                && get(target, path).is_some_and(Value::is_boolean)
            {
                continue;
            }
            remove(target, path);
        } else if model
            .parameters
            .get(parameter.as_str())
            .is_some_and(|declaration| !matches!(declaration, CatalogParameter::Unsupported))
        {
            return Err(anyhow::anyhow!(
                "adapter cannot express model parameter {}",
                parameter.as_str()
            ));
        }
    }
    for (parameter, value) in &resolved.values {
        let path = format
            .path(*parameter)
            .ok_or_else(|| anyhow::anyhow!("adapter cannot express model parameter"))?;
        put(target, path, value.clone())?;
        if *parameter == ModelParameter::ThinkingBudget {
            let type_path: &[&str] = match format {
                Format::Anthropic => &["thinking", "type"],
                Format::Bedrock => &["additionalModelRequestFields", "thinking", "type"],
                _ => continue,
            };
            put(target, type_path, Value::from("enabled"))?;
        }
    }
    tracing::debug!(model = %resolved.model, catalog_revision = %resolved.catalog_revision, parameters = ?resolved.values, "resolved model request parameters");
    if record {
        super::parameter_capture::record(resolved);
    }
    Ok(body)
}

fn get<'a>(body: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = body;
    for name in path {
        cursor = cursor.get(*name)?;
    }
    Some(cursor)
}

fn remove(body: &mut Value, path: &[&str]) {
    if let Some((last, prefix)) = path.split_last() {
        let mut cursor = body;
        for name in prefix {
            let Some(next) = cursor.get_mut(*name) else { return };
            cursor = next;
        }
        if let Some(object) = cursor.as_object_mut() {
            object.remove(*last);
        }
    }
}

fn put(body: &mut Value, path: &[&str], value: Value) -> anyhow::Result<()> {
    let Some((last, prefix)) = path.split_last() else {
        anyhow::bail!("empty adapter parameter path")
    };
    let mut cursor = body;
    for name in prefix {
        let object = cursor
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid adapter request object"))?;
        cursor = object.entry(*name).or_insert_with(|| Value::Object(Default::default()));
    }
    cursor
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid adapter parameter object"))?
        .insert((*last).to_owned(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(parameters: Value, level: Value) -> super::super::catalog::ReleaseModel {
        let mut model = ReleaseCatalog::embedded_ref()
            .unwrap()
            .models
            .first()
            .expect("test: release model")
            .clone();
        model.parameters = serde_json::from_value(parameters).unwrap();
        model.levels = serde_json::from_value(json!({"low":level,"medium":level,"high":level})).unwrap();
        model.default_level = Some("medium".into());
        model
    }

    #[test]
    fn output_limits_reach_each_adapters_expected_request_field() {
        let model = model(
            json!({"max_output_tokens":{"kind":"range","min":1,"max":8192}}),
            json!({"max_output_tokens":2048}),
        );
        for (format, pointer) in [
            (Format::OpenAiChat, "/max_completion_tokens"),
            (Format::Responses, "/max_output_tokens"),
            (Format::Anthropic, "/max_tokens"),
            (Format::Gemini, "/generationConfig/maxOutputTokens"),
            (Format::OpenRouter, "/max_tokens"),
            (Format::Compatible, "/max_tokens"),
            (Format::Ollama, "/options/num_predict"),
            (Format::Bedrock, "/inferenceConfig/maxTokens"),
        ] {
            let result = encode(
                &json!({"messages":[{"role":"user","content":"prx test"}]}),
                format,
                &model,
                "test",
            )
            .unwrap();
            assert_eq!(result.pointer(pointer).and_then(Value::as_u64), Some(2048), "{pointer}");
            assert_eq!(result.pointer("/messages/0/content").unwrap().clone(), "prx test");
        }
    }

    #[test]
    fn every_supported_parameter_maps_all_three_model_levels() {
        use ModelParameter::{
            MaxOutputTokens, ReasoningEffort, Seed, ServiceTier, Temperature, ThinkingBudget, TopP, Verbosity,
        };
        let mappings: &[(Format, &[(ModelParameter, &str)])] = &[
            (
                Format::OpenAiChat,
                &[
                    (Temperature, "/temperature"),
                    (TopP, "/top_p"),
                    (MaxOutputTokens, "/max_completion_tokens"),
                    (ReasoningEffort, "/reasoning_effort"),
                    (Verbosity, "/verbosity"),
                    (ServiceTier, "/service_tier"),
                    (Seed, "/seed"),
                ],
            ),
            (
                Format::Responses,
                &[
                    (Temperature, "/temperature"),
                    (TopP, "/top_p"),
                    (MaxOutputTokens, "/max_output_tokens"),
                    (ReasoningEffort, "/reasoning/effort"),
                    (Verbosity, "/text/verbosity"),
                    (ServiceTier, "/service_tier"),
                ],
            ),
            (
                Format::Anthropic,
                &[
                    (Temperature, "/temperature"),
                    (TopP, "/top_p"),
                    (MaxOutputTokens, "/max_tokens"),
                    (ReasoningEffort, "/output_config/effort"),
                    (ThinkingBudget, "/thinking/budget_tokens"),
                    (ServiceTier, "/service_tier"),
                ],
            ),
            (
                Format::Gemini,
                &[
                    (Temperature, "/generationConfig/temperature"),
                    (TopP, "/generationConfig/topP"),
                    (MaxOutputTokens, "/generationConfig/maxOutputTokens"),
                    (ReasoningEffort, "/generationConfig/thinkingConfig/thinkingLevel"),
                    (ThinkingBudget, "/generationConfig/thinkingConfig/thinkingBudget"),
                    (Seed, "/generationConfig/seed"),
                ],
            ),
            (
                Format::OpenRouter,
                &[
                    (Temperature, "/temperature"),
                    (TopP, "/top_p"),
                    (MaxOutputTokens, "/max_tokens"),
                    (ReasoningEffort, "/reasoning/effort"),
                    (ThinkingBudget, "/reasoning/max_tokens"),
                    (Verbosity, "/verbosity"),
                    (ServiceTier, "/service_tier"),
                    (Seed, "/seed"),
                ],
            ),
            (
                Format::Compatible,
                &[
                    (Temperature, "/temperature"),
                    (TopP, "/top_p"),
                    (MaxOutputTokens, "/max_tokens"),
                    (ReasoningEffort, "/reasoning_effort"),
                    (Verbosity, "/verbosity"),
                    (ServiceTier, "/service_tier"),
                    (Seed, "/seed"),
                ],
            ),
            (
                Format::Ollama,
                &[
                    (Temperature, "/options/temperature"),
                    (TopP, "/options/top_p"),
                    (MaxOutputTokens, "/options/num_predict"),
                    (ReasoningEffort, "/think"),
                    (Seed, "/options/seed"),
                ],
            ),
            (
                Format::Bedrock,
                &[
                    (Temperature, "/inferenceConfig/temperature"),
                    (TopP, "/inferenceConfig/topP"),
                    (MaxOutputTokens, "/inferenceConfig/maxTokens"),
                    (ReasoningEffort, "/additionalModelRequestFields/output_config/effort"),
                    (ThinkingBudget, "/additionalModelRequestFields/thinking/budget_tokens"),
                ],
            ),
        ];
        for (format, fields) in mappings {
            for (parameter, pointer) in *fields {
                let (declaration, values) = match parameter {
                    Temperature | TopP => (
                        json!({"kind":"range","min":0,"max":1}),
                        [json!(0.2), json!(0.5), json!(0.8)],
                    ),
                    MaxOutputTokens | ThinkingBudget | Seed => (
                        json!({"kind":"range","min":1,"max":8192}),
                        [json!(1024), json!(2048), json!(4096)],
                    ),
                    ReasoningEffort | Verbosity => (
                        json!({"kind":"levels","levels":["low","medium","high"]}),
                        [json!("low"), json!("medium"), json!("high")],
                    ),
                    ServiceTier => (
                        json!({"kind":"levels","levels":["auto","default","priority"]}),
                        [json!("auto"), json!("default"), json!("priority")],
                    ),
                };
                let mut row = model(json!({(parameter.as_str()):declaration}), json!({}));
                for (level, value) in ["low", "medium", "high"].into_iter().zip(values) {
                    row.levels.insert(
                        level.to_owned(),
                        [(parameter.as_str().to_owned(), value.clone())].into(),
                    );
                    row.default_level = Some(level.to_owned());
                    let result = encode(&json!({"messages":[],"stream":true}), *format, &row, "test").unwrap();
                    assert_eq!(
                        result.pointer(pointer),
                        Some(&value),
                        "{} {level} {pointer}",
                        parameter.as_str()
                    );
                    assert_eq!(result.pointer("/stream").unwrap().clone(), true);
                    assert_eq!(result.pointer("/messages").unwrap().clone(), json!([]));
                    if *parameter == ThinkingBudget && matches!(format, Format::Anthropic | Format::Bedrock) {
                        let thinking = if matches!(format, Format::Bedrock) {
                            result.pointer("/additionalModelRequestFields/thinking").unwrap()
                        } else {
                            result.pointer("/thinking").unwrap()
                        };
                        assert_eq!(thinking.pointer("/type").unwrap().clone(), "enabled");
                    }
                }
            }
        }
    }

    #[test]
    fn all_release_parameter_rows_fit_their_selected_adapter() {
        let release = ReleaseCatalog::embedded_ref().unwrap();
        let catalog = super::super::catalog::CatalogView::default().with_release_catalog(release.clone());
        for row in &release.models {
            check_selection(&catalog, &ProviderId::of(&row.provider), &row.api_model).unwrap();
        }
    }

    #[test]
    fn oauth_gemini_parameters_stay_inside_the_request_envelope() {
        let model = model(
            json!({"max_output_tokens":{"kind":"range","min":1,"max":8192}}),
            json!({"max_output_tokens":2048}),
        );
        let result = encode(
            &json!({"model":"prx-model","request":{"contents":[],"generationConfig":{"maxOutputTokens":2048}}}),
            Format::Gemini,
            &model,
            "test",
        )
        .unwrap();
        assert_eq!(
            result
                .pointer("/request/generationConfig/maxOutputTokens")
                .unwrap()
                .clone(),
            2048
        );
        assert!(result.get("generationConfig").is_none());
        assert_eq!(result.pointer("/model").unwrap().clone(), "prx-model");
    }

    #[test]
    fn unsupported_temperature_is_removed_before_request_serialization() {
        let model = model(json!({"temperature":{"kind":"unsupported"}}), json!({}));
        for (format, body, pointer) in [
            (Format::OpenAiChat, json!({"temperature":0.7}), "/temperature"),
            (Format::Anthropic, json!({"temperature":0.7}), "/temperature"),
            (
                Format::Gemini,
                json!({"generationConfig":{"temperature":0.7}}),
                "/generationConfig/temperature",
            ),
            (
                Format::Ollama,
                json!({"options":{"temperature":0.7,"num_ctx":8192}}),
                "/options/temperature",
            ),
            (
                Format::Bedrock,
                json!({"inferenceConfig":{"temperature":0.7}}),
                "/inferenceConfig/temperature",
            ),
        ] {
            assert!(
                encode(&body, format, &model, "test")
                    .unwrap()
                    .pointer(pointer)
                    .is_none()
            );
        }
    }

    #[test]
    fn responses_and_openrouter_reasoning_use_nested_fields() {
        let model = model(
            json!({"reasoning_effort":{"kind":"levels","levels":["low","medium","high"]}}),
            json!({"reasoning_effort":"high"}),
        );
        for format in [Format::Responses, Format::OpenRouter] {
            let result = encode(&json!({}), format, &model, "test").unwrap();
            assert_eq!(result.pointer("/reasoning/effort").unwrap().clone(), "high");
            assert!(result.get("reasoning_effort").is_none());
        }
        assert_eq!(
            encode(&json!({}), Format::Ollama, &model, "test")
                .unwrap()
                .get("think")
                .unwrap()
                .clone(),
            "high"
        );
    }

    #[test]
    fn unexpressible_declared_parameters_refuse_the_request_locally() {
        let model = model(
            json!({"verbosity":{"kind":"levels","levels":["low","high"]}}),
            json!({"verbosity":"high"}),
        );
        assert!(encode(&json!({"contents":[]}), Format::Gemini, &model, "test").is_err());
    }

    #[test]
    fn selection_refuses_unexpressible_parameters_before_provider_construction() {
        let mut raw: Value = serde_json::from_str(include_str!("../../catalog/models.json")).unwrap();
        let row = raw
            .get_mut("models")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row.pointer("/id").unwrap().clone() == "gemini/gemini-3.5-flash")
            .unwrap();
        *row.get_mut("parameters").unwrap() = json!({"verbosity":{"kind":"levels","levels":["low","high"]}});
        *row.get_mut("levels").unwrap() = json!({"low":{},"medium":{},"high":{"verbosity":"high"}});
        let release = ReleaseCatalog::from_json(&raw.to_string()).unwrap();
        let catalog = super::super::catalog::CatalogView::default().with_release_catalog(release);
        let result = super::super::selection::check_model(
            &catalog,
            "gemini",
            "gemini-3.5-flash",
            &super::super::catalog::Requirement::chat(),
            super::super::catalog::UnknownPolicy::Invocation,
        );
        assert!(matches!(
            result,
            Err(super::super::catalog::ResolutionError::ModelIncompatible { .. })
        ));
        assert!(
            super::super::selection::check_model(
                &catalog,
                "gemini",
                "gemini-2.0-flash",
                &super::super::catalog::Requirement::chat(),
                super::super::catalog::UnknownPolicy::Invocation,
            )
            .is_ok()
        );
    }

    #[test]
    fn unverified_models_preserve_the_body_and_declared_models_keep_valid_caller_values() {
        let body = json!({"model":"prx-model","temperature":0.7,"messages":[]});
        let wrapped = request("custom:https://example.com", Format::Compatible, "prx-model", &body);
        assert_eq!(
            serde_json::to_vec(&wrapped).unwrap(),
            serde_json::to_vec(&body).unwrap()
        );
        let wrapped = request("openai", Format::OpenAiChat, "gpt-4o", &body);
        assert_eq!(
            serde_json::to_vec(&wrapped).unwrap(),
            serde_json::to_vec(&body).unwrap()
        );
        let catalog = ReleaseCatalog::embedded_ref().unwrap();
        assert!(
            catalog
                .model(&ProviderId::of("deepseek"), "deepseek-chat")
                .unwrap()
                .parameters
                .is_empty()
        );
        let wrapped = request("deepseek", Format::Compatible, "prx-unverified-model", &body);
        assert_eq!(
            serde_json::to_vec(&wrapped).unwrap(),
            serde_json::to_vec(&body).unwrap()
        );
    }

    #[test]
    fn known_models_drop_undeclared_wire_fields_from_verified_declarations() {
        let row = model(
            json!({"max_output_tokens":{"kind":"range","min":1,"max":8192,"default":2048}}),
            json!({}),
        );
        let body = json!({"model":"prx-model","temperature":0.7,"top_p":0.9,"seed":7,"max_tokens":2048,"messages":[],"stream":true});
        let result = encode(&body, Format::Compatible, &row, "test").unwrap();
        assert_eq!(
            result,
            json!({"model":"prx-model","max_tokens":2048,"messages":[],"stream":true})
        );
        let unsupported = model(
            json!({"temperature":{"kind":"unsupported"},"top_p":{"kind":"unsupported"},"seed":{"kind":"unsupported"},"max_output_tokens":{"kind":"unsupported"}}),
            json!({}),
        );
        let result = encode(&body, Format::Compatible, &unsupported, "test").unwrap();
        assert_eq!(result, json!({"model":"prx-model","messages":[],"stream":true}));
    }
    #[test]
    fn explicit_caller_output_top_p_and_seed_override_release_defaults() {
        let row = model(
            json!({"max_output_tokens":{"kind":"range","min":1,"max":8192,"default":2048},"top_p":{"kind":"range","min":0,"max":1,"default":1.0},"seed":{"kind":"range","min":0,"max":1000,"default":1}}),
            json!({"max_output_tokens":2048}),
        );
        let body = json!({"max_tokens":4096,"top_p":0.25,"seed":7,"messages":[]});
        assert_eq!(encode(&body, Format::Compatible, &row, "test").unwrap(), body);
    }

    #[test]
    fn explicit_oauth_gemini_output_override_stays_in_the_request_envelope() {
        let row = model(
            json!({"max_output_tokens":{"kind":"range","min":1,"max":8192,"default":2048}}),
            json!({"max_output_tokens":2048}),
        );
        let body = json!({"model":"prx-model","request":{"contents":[],"generationConfig":{"maxOutputTokens":8192}}});
        let encoded = encode(&body, Format::Gemini, &row, "test").unwrap();
        assert_eq!(encoded, body);
        assert!(encoded.get("generationConfig").is_none());
    }
}

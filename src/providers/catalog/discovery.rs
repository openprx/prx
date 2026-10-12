//! Provider catalog metadata as descriptor evidence, and the model cache.
//!
//! A catalog fetch used to keep model ids only, so everything a provider said
//! about a model — its output modalities, whether it takes tools, which
//! generation methods it has — was dropped before anything could check a
//! selection against it. [`DiscoveredModel`] keeps those facts as the provider
//! stated them; [`DiscoveredModel::descriptor`] interprets them.
//!
//! The cache (`state/models_cache.json`) is version 2: every entry keeps its
//! `models` id list, which the channel `/models` listing and older binaries
//! read, and adds `descriptors`. A version-1 file has no descriptors; its
//! models are id-only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::descriptor::{DescriptorSource, FeatureSet, ModelDescriptor, ModelOperation, Support};
use crate::providers::ProviderId;

/// The cache file under `<workspace>/state/`.
pub const MODEL_CACHE_FILE: &str = "models_cache.json";

/// The cache format this build writes.
pub const MODEL_CACHE_VERSION: u32 = 2;

/// One model as a provider catalog described it. Every field but `id` is the
/// provider's own statement, kept verbatim; `None` means it said nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    /// OpenRouter `architecture.input_modalities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    /// OpenRouter `architecture.output_modalities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    /// OpenRouter `supported_parameters`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_parameters: Option<Vec<String>>,
    /// Gemini `supportedGenerationMethods`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_methods: Option<Vec<String>>,
    /// Context length in tokens, when stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl DiscoveredModel {
    /// A model known by id only.
    #[must_use]
    pub fn id_only(id: &str) -> Self {
        Self {
            id: id.to_string(),
            input_modalities: None,
            output_modalities: None,
            supported_parameters: None,
            generation_methods: None,
            context_window: None,
        }
    }

    /// Whether the catalog said anything about this model beyond its id.
    #[must_use]
    pub const fn has_metadata(&self) -> bool {
        self.input_modalities.is_some()
            || self.output_modalities.is_some()
            || self.supported_parameters.is_some()
            || self.generation_methods.is_some()
            || self.context_window.is_some()
    }

    /// The evidence these facts give about chat. A missing fact is `Unknown`,
    /// never support.
    fn chat(&self) -> Support {
        let outputs = self.output_modalities.as_deref().map(|outputs| {
            if contains(outputs, "text") {
                Support::Supported
            } else {
                Support::unsupported("the catalog lists no text output for this model")
            }
        });
        let methods = self.generation_methods.as_deref().map(|methods| {
            if contains(methods, "generateContent") {
                Support::Supported
            } else {
                Support::unsupported("the catalog lists no generateContent method for this model")
            }
        });
        outputs
            .unwrap_or(Support::Unknown)
            .and(methods.unwrap_or(Support::Unknown))
    }

    /// Whether a chat model picker should offer this model: anything the
    /// catalog does not rule out for chat.
    #[must_use]
    pub fn offers_chat(&self) -> bool {
        !self.chat().is_unsupported()
    }

    /// The descriptor evidence for this model of `provider`.
    #[must_use]
    pub fn descriptor(&self, provider: ProviderId, fetched_at_unix: u64) -> ModelDescriptor {
        let mut descriptor = ModelDescriptor::id_only(provider, &self.id);
        if !self.has_metadata() {
            return descriptor;
        }
        descriptor.source = DescriptorSource::Discovery { fetched_at_unix };
        descriptor.context_window = self.context_window;
        descriptor.operations.insert(ModelOperation::Chat, self.chat());
        if let Some(methods) = self.generation_methods.as_deref() {
            let embedding = if contains(methods, "embedContent") {
                Support::Supported
            } else {
                Support::unsupported("the catalog lists no embedContent method for this model")
            };
            descriptor.operations.insert(ModelOperation::Embedding, embedding);
        }
        descriptor.features = FeatureSet {
            native_tools: self
                .supported_parameters
                .as_deref()
                .map_or(Support::Unknown, |parameters| {
                    if contains(parameters, "tools") {
                        Support::Supported
                    } else {
                        Support::unsupported("`tools` is not in the catalog's supported_parameters for this model")
                    }
                }),
            vision_input: self.input_modalities.as_deref().map_or(Support::Unknown, |inputs| {
                if contains(inputs, "image") {
                    Support::Supported
                } else {
                    Support::unsupported("the catalog lists no image input for this model")
                }
            }),
            structured_output: self
                .supported_parameters
                .as_deref()
                .map_or(Support::Unknown, |parameters| {
                    if contains(parameters, "response_format") || contains(parameters, "structured_outputs") {
                        Support::Supported
                    } else {
                        Support::unsupported(
                            "neither `response_format` nor `structured_outputs` is in the catalog's \
                             supported_parameters for this model",
                        )
                    }
                }),
            streaming: Support::Unknown,
        };
        descriptor
    }
}

fn contains(values: &[String], wanted: &str) -> bool {
    values.iter().any(|value| value.trim().eq_ignore_ascii_case(wanted))
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect()
    })
}

fn token_count(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
}

/// Trim ids, drop empty ones, keep the first record of each id, sort by id.
#[must_use]
pub fn normalize(models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    let mut unique: BTreeMap<String, DiscoveredModel> = BTreeMap::new();
    for mut model in models {
        let id = model.id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        model.id.clone_from(&id);
        unique.entry(id).or_insert(model);
    }
    unique.into_values().collect()
}

/// The ids a chat model picker offers, in order.
#[must_use]
pub fn chat_model_ids(models: &[DiscoveredModel]) -> Vec<String> {
    models
        .iter()
        .filter(|model| model.offers_chat())
        .map(|model| model.id.clone())
        .collect()
}

/// An OpenAI-compatible `/models` payload (`{"data": [...]}` or a bare
/// array), including OpenRouter's `architecture`, `supported_parameters` and
/// `context_length`.
#[must_use]
pub fn parse_openai_compatible_models(payload: &Value) -> Vec<DiscoveredModel> {
    let records = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array());
    let models = records
        .into_iter()
        .flatten()
        .filter_map(|record| {
            let id = record.get("id").and_then(Value::as_str)?;
            let architecture = record.get("architecture");
            Some(DiscoveredModel {
                id: id.to_string(),
                input_modalities: string_list(architecture.and_then(|a| a.get("input_modalities"))),
                output_modalities: string_list(architecture.and_then(|a| a.get("output_modalities"))),
                supported_parameters: string_list(record.get("supported_parameters")),
                generation_methods: None,
                context_window: token_count(record.get("context_length")),
            })
        })
        .collect();
    normalize(models)
}

/// A Gemini `models.list` payload. Every model is kept with its
/// `supportedGenerationMethods`; a record without that list is id-only, not
/// generate-capable.
#[must_use]
pub fn parse_gemini_models(payload: &Value) -> Vec<DiscoveredModel> {
    let models = payload
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|record| {
            let name = record.get("name").and_then(Value::as_str)?;
            Some(DiscoveredModel {
                generation_methods: string_list(record.get("supportedGenerationMethods")),
                context_window: token_count(record.get("inputTokenLimit")),
                ..DiscoveredModel::id_only(name.trim_start_matches("models/"))
            })
        })
        .collect();
    normalize(models)
}

/// An Ollama `/api/tags` payload: names only.
#[must_use]
pub fn parse_ollama_models(payload: &Value) -> Vec<DiscoveredModel> {
    let models = payload
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|record| record.get("name").and_then(Value::as_str).map(DiscoveredModel::id_only))
        .collect();
    normalize(models)
}

/// One provider's cached catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheEntry {
    /// The provider spelling the catalog was fetched for.
    pub provider: String,
    pub fetched_at_unix: u64,
    /// The chat-offerable ids, as every version of the cache has them.
    pub models: Vec<String>,
    /// Version 2: every model the catalog listed, with its facts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descriptors: Vec<DiscoveredModel>,
}

/// The whole cache file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheState {
    /// Absent (version 1) in files written before descriptors existed.
    #[serde(default)]
    pub version: u32,
    pub entries: Vec<ModelCacheEntry>,
}

/// `<workspace>/state/models_cache.json`.
#[must_use]
pub fn model_cache_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("state").join(MODEL_CACHE_FILE)
}

/// Read the cache. A missing file is empty; an unreadable body is treated as
/// empty (it is a cache) and reported.
///
/// # Errors
///
/// When the file exists but cannot be read.
pub fn load_model_cache_state(workspace_dir: &Path) -> anyhow::Result<ModelCacheState> {
    let path = model_cache_path(workspace_dir);
    if !path.exists() {
        return Ok(ModelCacheState::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("failed to read model cache at {}", path.display()))?;
    let state = serde_json::from_str::<ModelCacheState>(&raw).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "model cache is not readable; treating it as empty");
        ModelCacheState::default()
    });
    if state.version > MODEL_CACHE_VERSION {
        tracing::warn!(
            path = %path.display(),
            version = state.version,
            supported = MODEL_CACHE_VERSION,
            "model cache was written by a newer PRX; reading the fields this version knows"
        );
    }
    Ok(state)
}

/// Write the cache.
///
/// # Errors
///
/// When the directory or the file cannot be written.
pub fn save_model_cache_state(workspace_dir: &Path, state: &ModelCacheState) -> anyhow::Result<()> {
    let path = model_cache_path(workspace_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create model cache directory {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(state).context("failed to serialize model cache")?;
    std::fs::write(&path, json).with_context(|| format!("failed to write model cache at {}", path.display()))?;
    Ok(())
}

/// Record a fetched catalog for `provider` at `fetched_at_unix`, writing the
/// cache as version 2. An empty catalog changes nothing.
///
/// # Errors
///
/// When the cache cannot be read or written.
pub fn record_discovered(
    workspace_dir: &Path,
    provider: &str,
    models: &[DiscoveredModel],
    fetched_at_unix: u64,
) -> anyhow::Result<()> {
    let descriptors = normalize(models.to_vec());
    if descriptors.is_empty() {
        return Ok(());
    }
    let entry = ModelCacheEntry {
        provider: provider.to_string(),
        fetched_at_unix,
        models: chat_model_ids(&descriptors),
        descriptors,
    };
    let mut state = load_model_cache_state(workspace_dir)?;
    state.version = MODEL_CACHE_VERSION;
    if let Some(existing) = state.entries.iter_mut().find(|existing| existing.provider == provider) {
        *existing = entry;
    } else {
        state.entries.push(entry);
    }
    save_model_cache_state(workspace_dir, &state)
}

/// Every cached model that carries catalog metadata, as descriptor evidence.
/// Id-only models add nothing, so they are not returned.
///
/// # Errors
///
/// When the cache file exists but cannot be read.
pub fn cached_descriptors(workspace_dir: &Path) -> anyhow::Result<Vec<ModelDescriptor>> {
    let state = load_model_cache_state(workspace_dir)?;
    Ok(state
        .entries
        .iter()
        .flat_map(|entry| {
            let provider = ProviderId::of(&entry.provider);
            entry
                .descriptors
                .iter()
                .filter(|model| model.has_metadata())
                .map(move |model| model.descriptor(provider.clone(), entry.fetched_at_unix))
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::providers::catalog::descriptor::{Feature, Requirement};
    use crate::providers::catalog::{CatalogView, UnknownPolicy, resolve};
    use crate::providers::credentials::{ConfigCredentialSource, CredentialRef};
    use serde_json::json;

    fn openrouter_payload() -> Value {
        json!({
            "data": [
                {
                    "id": "vendor/chat-no-tools",
                    "context_length": 32768,
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                    "supported_parameters": ["temperature", "max_tokens"]
                },
                {
                    "id": "vendor/vision-tools",
                    "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
                    "supported_parameters": ["tools", "tool_choice", "response_format"]
                },
                {
                    "id": "vendor/image-maker",
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
                    "supported_parameters": []
                },
                {"id": "vendor/bare"}
            ]
        })
    }

    #[test]
    fn openrouter_metadata_becomes_descriptor_evidence() {
        let models = parse_openai_compatible_models(&openrouter_payload());
        assert_eq!(
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(),
            [
                "vendor/bare",
                "vendor/chat-no-tools",
                "vendor/image-maker",
                "vendor/vision-tools"
            ]
        );
        let provider = ProviderId::of("openrouter");
        let by_id = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.descriptor(provider.clone(), 7))
                .expect("test: model parsed")
        };

        let no_tools = by_id("vendor/chat-no-tools");
        assert_eq!(no_tools.source, DescriptorSource::Discovery { fetched_at_unix: 7 });
        assert!(no_tools.operation(ModelOperation::Chat).is_supported());
        assert!(no_tools.features.native_tools.is_unsupported());
        assert!(no_tools.features.vision_input.is_unsupported());
        assert_eq!(no_tools.features.streaming, Support::Unknown);
        assert_eq!(no_tools.context_window, Some(32768));

        let vision_tools = by_id("vendor/vision-tools");
        assert!(vision_tools.features.native_tools.is_supported());
        assert!(vision_tools.features.vision_input.is_supported());
        assert!(vision_tools.features.structured_output.is_supported());

        assert!(
            by_id("vendor/image-maker")
                .operation(ModelOperation::Chat)
                .is_unsupported()
        );
        let bare = by_id("vendor/bare");
        assert_eq!(bare.source, DescriptorSource::LegacyIdOnly);
        assert_eq!(bare.operation(ModelOperation::Chat), Support::Unknown);

        assert_eq!(
            chat_model_ids(&models),
            ["vendor/bare", "vendor/chat-no-tools", "vendor/vision-tools"],
            "a model the catalog rules out for chat is not offered"
        );
    }

    #[test]
    fn a_gemini_record_without_generation_methods_is_unknown_not_chat_capable() {
        let payload = json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent", "countTokens"], "inputTokenLimit": 1_048_576},
                {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]},
                {"name": "models/gemini-undeclared"}
            ]
        });
        let models = parse_gemini_models(&payload);
        let provider = ProviderId::of("gemini");
        let describe = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.descriptor(provider.clone(), 1))
                .expect("test: model parsed")
        };
        assert!(
            describe("gemini-2.5-pro")
                .operation(ModelOperation::Chat)
                .is_supported()
        );
        assert_eq!(describe("gemini-2.5-pro").context_window, Some(1_048_576));
        assert!(
            describe("text-embedding-004")
                .operation(ModelOperation::Chat)
                .is_unsupported()
        );
        assert!(
            describe("text-embedding-004")
                .operation(ModelOperation::Embedding)
                .is_supported()
        );
        let undeclared = describe("gemini-undeclared");
        assert_eq!(undeclared.operation(ModelOperation::Chat), Support::Unknown);
        assert_eq!(undeclared.source, DescriptorSource::LegacyIdOnly);
        assert_eq!(
            chat_model_ids(&models),
            ["gemini-2.5-pro", "gemini-undeclared"],
            "the picker still offers what it offered before; only the evidence changed"
        );
    }

    #[test]
    fn ollama_tags_are_id_only_and_deduplicated() {
        let payload = json!({"models": [{"name": "llama3.2:latest"}, {"name": " llama3.2:latest "}, {"name": ""}]});
        let models = parse_ollama_models(&payload);
        assert_eq!(models, vec![DiscoveredModel::id_only("llama3.2:latest")]);
        assert!(!models[0].has_metadata());
    }

    #[test]
    fn a_version_one_cache_still_loads_as_id_only() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let path = model_cache_path(temp.path());
        std::fs::create_dir_all(path.parent().expect("test: parent")).expect("test: mkdir");
        std::fs::write(
            &path,
            r#"{"entries":[{"provider":"openrouter","fetched_at_unix":5,"models":["vendor/a","vendor/b"]}]}"#,
        )
        .expect("test: write v1");
        let state = load_model_cache_state(temp.path()).expect("test: load");
        assert_eq!(state.version, 0);
        assert_eq!(state.entries[0].models, ["vendor/a", "vendor/b"]);
        assert!(state.entries[0].descriptors.is_empty(), "expected an empty value");
        assert!(
            cached_descriptors(temp.path()).expect("test: descriptors").is_empty(),
            "expected an empty value"
        );
    }

    #[test]
    fn a_version_two_cache_keeps_the_id_list_older_readers_use() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let models = parse_openai_compatible_models(&openrouter_payload());
        record_discovered(temp.path(), "openrouter", &models, 9).expect("test: record");
        let raw = std::fs::read_to_string(model_cache_path(temp.path())).expect("test: read");
        let value: Value = serde_json::from_str(&raw).expect("test: json");
        assert_eq!(value["version"], json!(2));
        assert_eq!(
            value["entries"][0]["models"],
            json!(["vendor/bare", "vendor/chat-no-tools", "vendor/vision-tools"])
        );
        let v1_reader: serde_json::Value = value["entries"][0].clone();
        assert!(v1_reader.get("provider").is_some() && v1_reader.get("fetched_at_unix").is_some());
    }

    /// The cached facts reach the resolver: a discovered no-tools model is
    /// refused native tools on OpenRouter with nothing constructed.
    #[test]
    fn cached_discovery_evidence_reaches_the_resolver() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        record_discovered(
            temp.path(),
            "openrouter",
            &parse_openai_compatible_models(&openrouter_payload()),
            9,
        )
        .expect("test: record");
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some("openrouter".to_string());
        let catalog = CatalogView::from_config(&config)
            .with_discovery_cache(temp.path())
            .expect("test: cache");
        let selection = crate::providers::catalog::Selection {
            provider: "openrouter",
            model: "vendor/chat-no-tools",
            credential: CredentialRef::ProviderStore,
        };
        let credentials = ConfigCredentialSource::new(&config);
        let error = resolve(
            &selection,
            &Requirement::chat().with(Feature::NativeTools),
            &catalog,
            &credentials,
            UnknownPolicy::Invocation,
        )
        .expect_err("the cached record lists no tools");
        assert!(error.to_string().contains("supported_parameters"), "{error}");
        let resolved = resolve(
            &selection,
            &Requirement::chat(),
            &catalog,
            &credentials,
            UnknownPolicy::DurableSelection,
        )
        .expect("the cached record proves chat");
        assert_eq!(resolved.source(), DescriptorSource::Discovery { fetched_at_unix: 9 });
    }

    /// R-PMMB F-5: two refreshes under different spellings of one provider
    /// leave two entries; the evidence is the newest one, whichever comes
    /// first in the file.
    #[test]
    fn the_newest_cached_record_is_the_evidence() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let with_tools = parse_openai_compatible_models(&json!({"data": [
            {"id": "vendor/m", "architecture": {"output_modalities": ["text"]}, "supported_parameters": ["tools"]}
        ]}));
        let without_tools = parse_openai_compatible_models(&json!({"data": [
            {"id": "vendor/m", "architecture": {"output_modalities": ["text"]}, "supported_parameters": []}
        ]}));
        record_discovered(temp.path(), "OpenRouter", &with_tools, 100).expect("test: record");
        record_discovered(temp.path(), "openrouter", &without_tools, 200).expect("test: record");
        let catalog = CatalogView::default()
            .with_discovery_cache(temp.path())
            .expect("test: cache");
        let described = catalog.describe(&ProviderId::of("openrouter"), "vendor/m");
        assert_eq!(described.source, DescriptorSource::Discovery { fetched_at_unix: 200 });
        assert!(described.features.native_tools.is_unsupported(), "{described:?}");
    }
}

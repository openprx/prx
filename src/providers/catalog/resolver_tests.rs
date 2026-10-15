//! PMM-B-5: the fail-fast resolver, driven through the real factory and a
//! loopback endpoint where transport matters. Every credential is a sentinel
//! and every config directory a fresh temporary one.

#![allow(clippy::indexing_slicing)]

use std::cell::Cell;

use super::descriptor::{DescriptorSource, Feature, FeatureSet, ModelDescriptor, ModelOperation, Requirement, Support};
use super::resolver::{ResolutionError, Selection, UnknownPolicy, Verification, resolve};
use super::{CatalogView, CuratedModel};
use crate::config::Config;
use crate::providers::credential_probe::CredentialProbe;
use crate::providers::credentials::{
    ConfigCredentialSource, CredentialLookupError, CredentialMaterial, CredentialRef, CredentialSource, LegacySlot,
};
use crate::providers::{
    DEFAULT_PROVIDER_FOR_UNSET, Provider, ProviderCapabilityError, ProviderId, create_provider_for,
    create_provider_with_options, provider_runtime_options_from_config,
};
use crate::tools::ToolSchemaDialect;

const ROOT_SENTINEL: &str = "PMMB-RESOLVER-ROOT-SENTINEL";
const ROTATION_SENTINEL: &str = "PMMB-RESOLVER-ROTATION-SENTINEL";
const PROFILE_SENTINEL: &str = "PMMB-RESOLVER-PROFILE-SENTINEL";
const DECISION_MODEL: &str = "jev-decision-test";

fn isolated_config(temp: &tempfile::TempDir, provider: &str, model: &str) -> Config {
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(provider.to_string());
    config.default_model = Some(model.to_string());
    config.api_key = Some(ROOT_SENTINEL.to_string());
    config.api_url = None;
    config.model_routes = Vec::new();
    config.reliability.fallback_providers = Vec::new();
    config.reliability.api_keys = Vec::new();
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config.secrets.encrypt = false;
    config
}

/// A discovered record whose only operation is the decision endpoint.
fn decision_only(provider: &str) -> ModelDescriptor {
    let mut descriptor = ModelDescriptor::id_only(ProviderId::of(provider), DECISION_MODEL);
    descriptor.source = DescriptorSource::Discovery { fetched_at_unix: 1 };
    descriptor
        .operations
        .insert(ModelOperation::DecisionEndpoint, Support::Supported);
    descriptor.operations.insert(
        ModelOperation::Chat,
        Support::unsupported("the catalog lists no text output"),
    );
    descriptor
}

/// A discovered chat model with the given features.
fn chat_model(provider: &str, model: &str, features: FeatureSet) -> ModelDescriptor {
    let mut descriptor = ModelDescriptor::id_only(ProviderId::of(provider), model);
    descriptor.source = DescriptorSource::Discovery { fetched_at_unix: 1 };
    descriptor.operations.insert(ModelOperation::Chat, Support::Supported);
    descriptor.features = features;
    descriptor
}

/// The default selection, resolved and then built — the order every surface
/// adopts in PMM-C.
fn select_and_build(config: &Config, catalog: &CatalogView) -> anyhow::Result<Box<dyn Provider>> {
    let provider = config.default_provider.as_deref().unwrap_or(DEFAULT_PROVIDER_FOR_UNSET);
    let model = config.default_model.as_deref().unwrap_or_default();
    let resolved = resolve(
        &Selection {
            provider,
            model,
            credential: CredentialRef::LegacyInline(LegacySlot::Root),
        },
        &Requirement::chat(),
        catalog,
        &ConfigCredentialSource::new(config),
        UnknownPolicy::Invocation,
    )?;
    create_provider_for(
        &resolved,
        catalog,
        &config.reliability,
        &config.cost,
        &provider_runtime_options_from_config(config),
    )
}

/// A source that records whether it was consulted at all.
#[derive(Default)]
struct CountingSource {
    calls: Cell<usize>,
}

impl CredentialSource for CountingSource {
    fn owner_of(&self, _reference: &CredentialRef) -> Result<Option<ProviderId>, CredentialLookupError> {
        self.calls.set(self.calls.get() + 1);
        Ok(Some(ProviderId::of("openrouter")))
    }

    fn material(&self, _reference: &CredentialRef) -> Result<Option<CredentialMaterial>, CredentialLookupError> {
        self.calls.set(self.calls.get() + 1);
        Ok(Some(CredentialMaterial::secret(ROOT_SENTINEL)))
    }
}

const fn root<'a>(provider: &'a str, model: &'a str) -> Selection<'a> {
    Selection {
        provider,
        model,
        credential: CredentialRef::LegacyInline(LegacySlot::Root),
    }
}

/// A decision-only model persisted as the chat default used to be accepted
/// by validation and construction and to fail only at the upstream, after an
/// authenticated request. The resolver refuses it with nothing sent.
#[tokio::test]
async fn a_decision_only_default_is_refused_before_transport() {
    let probe = CredentialProbe::start().await;
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = probe.custom_provider();
    let config = isolated_config(&temp, &label, DECISION_MODEL);
    let catalog = CatalogView::from_config(&config).with_discovered([decision_only(&label)]);

    let outcome = match select_and_build(&config, &catalog) {
        Ok(provider) => provider
            .chat_with_system(None, "ping", DECISION_MODEL, 0.0)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };

    assert!(
        probe.requests().is_empty(),
        "a decision-only chat default reached the endpoint: {} request(s)",
        probe.requests().len()
    );
    let error = outcome.expect_err("a decision-only model must not serve chat");
    let text = format!("{error:#}");
    assert!(text.contains("cannot be used for chat"), "{text}");
    assert!(text.contains(DECISION_MODEL), "{text}");
    assert!(!text.contains(ROOT_SENTINEL), "{text}");
}

/// A provider without a decision adapter refuses the decision endpoint as
/// having none and never falls back to chat. A provider with one (CDP-02,
/// `decision_adapter`) resolves a model that declares it.
#[test]
fn the_decision_endpoint_is_refused_as_having_no_adapter() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "anthropic", DECISION_MODEL);
    let catalog = CatalogView::from_config(&config).with_discovered([decision_only("anthropic")]);
    let error = resolve(
        &root("anthropic", DECISION_MODEL),
        &Requirement::operation(ModelOperation::DecisionEndpoint),
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect_err("no adapter executes the decision endpoint for this provider");
    assert!(
        matches!(
            &error,
            ResolutionError::OperationUnsupported { operation: ModelOperation::DecisionEndpoint, reason, .. }
                if reason.contains("no adapter")
        ),
        "{error}"
    );

    let config = isolated_config(&temp, "openrouter", DECISION_MODEL);
    let catalog = CatalogView::from_config(&config).with_discovered([decision_only("openrouter")]);
    let resolved = resolve(
        &root("openrouter", DECISION_MODEL),
        &Requirement::operation(ModelOperation::DecisionEndpoint),
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect("the OpenRouter decision adapter executes a declared decision model");
    assert_eq!(resolved.operation(), ModelOperation::DecisionEndpoint);
}

/// C2af-01: a declaration for one model of an endpoint used to be folded
/// into the endpoint's `widest` dialect, so every other model's turn was
/// enforced against the rewritten document. Resolved per model, the other
/// model keeps `Raw`.
#[tokio::test]
async fn a_per_model_declaration_on_one_endpoint_does_not_fold_to_the_widest() {
    let probe = CredentialProbe::start().await;
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = probe.custom_provider();
    let mut config = isolated_config(&temp, &label, "model-b");
    config
        .providers
        .tool_dialects
        .push(crate::config::schema::ProviderToolDialectConfig {
            provider: label.clone(),
            model: Some("model-a".to_string()),
            tool_schema: Some(crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators),
            tool_call: None,
        });
    let catalog = CatalogView::from_config(&config);

    for (model, expected) in [
        ("model-b", ToolSchemaDialect::Raw),
        ("MODEL-A", ToolSchemaDialect::NoRootCombinators),
    ] {
        let resolved = resolve(
            &root(&label, model),
            &Requirement::chat(),
            &catalog,
            &ConfigCredentialSource::new(&config),
            UnknownPolicy::Invocation,
        )
        .expect("test: an id-only model runs unverified at invocation");
        assert_eq!(
            resolved.dialects().tool_schema,
            expected,
            "descriptor dialect for {model}"
        );
        let provider = create_provider_for(
            &resolved,
            &catalog,
            &config.reliability,
            &config.cost,
            &provider_runtime_options_from_config(&config),
        )
        .expect("test: construct");
        assert_eq!(
            provider.tool_schema_dialect_for(model),
            expected,
            "the provider built for {model} enforces another model's dialect"
        );
        assert_eq!(
            provider.tool_schema_dialect(),
            ToolSchemaDialect::Raw,
            "without a model, a model-scoped declaration is not every model's"
        );
    }
    assert!(probe.requests().is_empty(), "resolution and construction send nothing");
}

/// A chat model whose record lists no tools is refused native tools even on
/// OpenRouter, whose provider-wide boolean says every model has them.
#[test]
fn negative_model_evidence_beats_a_provider_wide_boolean() {
    let provider = create_provider_with_options("openrouter", Some("pmmb-test-key"), &Default::default())
        .expect("test: construct openrouter");
    assert!(provider.supports_native_tools(), "the provider-wide boolean is true");

    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "openrouter", "vendor/chat-no-tools");
    let features = FeatureSet {
        native_tools: Support::unsupported("tools is not in supported_parameters"),
        vision_input: Support::unsupported("image is not an input modality"),
        streaming: Support::unsupported("declared without streaming"),
        ..FeatureSet::unknown()
    };
    let catalog =
        CatalogView::from_config(&config).with_discovered([chat_model("openrouter", "vendor/chat-no-tools", features)]);
    for feature in [Feature::NativeTools, Feature::VisionInput, Feature::Streaming] {
        let source = CountingSource::default();
        let error = resolve(
            &root("openrouter", "vendor/chat-no-tools"),
            &Requirement::chat().with(feature),
            &catalog,
            &source,
            UnknownPolicy::Invocation,
        )
        .expect_err("negative evidence refuses");
        assert!(
            matches!(&error, ResolutionError::FeatureUnsupported { feature: refused, .. } if *refused == feature),
            "{error}"
        );
        assert_eq!(source.calls.get(), 0, "a feature refusal read the credential source");
    }
    let plain = resolve(
        &root("openrouter", "vendor/chat-no-tools"),
        &Requirement::chat(),
        &catalog,
        &CountingSource::default(),
        UnknownPolicy::Invocation,
    )
    .expect("plain chat is supported");
    assert_eq!(plain.verification(), &Verification::Verified);
}

/// D-018 Q1: an id-only model keeps running when invoked, marked unverified
/// with a warning; a new durable selection of it is refused until declared.
#[test]
fn an_id_only_model_runs_unverified_but_is_refused_as_a_new_durable_selection() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "openai", "gpt-id-only");
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);
    let selection = root("openai", "gpt-id-only");

    let resolved = resolve(
        &selection,
        &Requirement::chat().with(Feature::NativeTools),
        &catalog,
        &credentials,
        UnknownPolicy::Invocation,
    )
    .expect("an existing selection keeps running");
    assert_eq!(
        resolved.verification(),
        &Verification::Unverified {
            unproven: vec!["chat", "native_tools"]
        }
    );
    let warning = resolved.warning().expect("an unverified model carries a warning");
    assert!(warning.contains("unverified for chat, native_tools"), "{warning}");
    assert_eq!(resolved.credential().api_key(), Some(ROOT_SENTINEL));

    let source = CountingSource::default();
    let error = resolve(
        &selection,
        &Requirement::chat(),
        &catalog,
        &source,
        UnknownPolicy::DurableSelection,
    )
    .expect_err("a new durable selection must be verified");
    assert!(matches!(error, ResolutionError::OperationUnverified { .. }), "{error}");
    assert_eq!(source.calls.get(), 0);
    let text = error.to_string();
    assert!(
        text.contains("[[providers.models]]") && text.contains("prx models refresh"),
        "{text}"
    );

    let declared = catalog.with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::Chat]),
        ..CuratedModel::new(ProviderId::of("openai"), Some("gpt-id-only"))
    }]);
    let verified = resolve(
        &selection,
        &Requirement::chat(),
        &declared,
        &credentials,
        UnknownPolicy::DurableSelection,
    )
    .expect("a declared model is a valid durable selection");
    assert_eq!(verified.verification(), &Verification::Verified);
    assert_eq!(verified.source(), DescriptorSource::Curated);
}

/// An operator declaration cannot lift an adapter's `Unsupported`.
#[test]
fn a_declaration_cannot_lift_an_adapter_refusal() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "anthropic", "claude-embed");
    let catalog = CatalogView::from_config(&config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::Embedding]),
        ..CuratedModel::new(ProviderId::of("anthropic"), None)
    }]);
    let error = resolve(
        &root("anthropic", "claude-embed"),
        &Requirement::operation(ModelOperation::Embedding),
        &catalog,
        &CountingSource::default(),
        UnknownPolicy::Invocation,
    )
    .expect_err("no embedding adapter for anthropic");
    assert!(matches!(error, ResolutionError::OperationUnsupported { .. }), "{error}");
}

/// The root key, base URL and rotation keys belong to the configured
/// provider identity: released together to any spelling of it, and to
/// nothing else — not another vendor, not another port of a custom endpoint.
#[test]
fn the_root_credential_is_released_only_to_its_own_provider_and_as_a_unit() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let mut config = isolated_config(&temp, "openai", "gpt-4o");
    config.api_url = Some("http://127.0.0.1:9/v1".to_string());
    config.reliability.api_keys = vec![ROTATION_SENTINEL.to_string()];
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);

    let released = resolve(
        &root(" OpenAI ", "gpt-4o"),
        &Requirement::chat(),
        &catalog,
        &credentials,
        UnknownPolicy::Invocation,
    )
    .expect("the configured provider receives its own connection");
    assert_eq!(released.credential().api_key(), Some(ROOT_SENTINEL));
    assert_eq!(released.credential().api_url(), Some("http://127.0.0.1:9/v1"));
    assert_eq!(released.credential().rotation_keys(), [ROTATION_SENTINEL.to_string()]);
    let debug = format!("{released:?}");
    assert!(
        !debug.contains(ROOT_SENTINEL) && !debug.contains(ROTATION_SENTINEL),
        "{debug}"
    );

    let error = resolve(
        &root("anthropic", "claude-sonnet-4"),
        &Requirement::chat(),
        &catalog,
        &credentials,
        UnknownPolicy::Invocation,
    )
    .expect_err("openai's credential is not anthropic's");
    assert!(
        matches!(error, ResolutionError::CredentialOwnerMismatch { .. }),
        "{error}"
    );
    let text = error.to_string();
    assert!(
        !text.contains(ROOT_SENTINEL) && !text.contains(ROTATION_SENTINEL),
        "{text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("openai"),
        "the refusal named the configured default: {text}"
    );

    let mut custom = isolated_config(&temp, "custom:http://127.0.0.1:1/v1", "m");
    custom.api_key = Some(ROOT_SENTINEL.to_string());
    let error = resolve(
        &root("custom:http://127.0.0.1:2/v1", "m"),
        &Requirement::chat(),
        &CatalogView::from_config(&custom),
        &ConfigCredentialSource::new(&custom),
        UnknownPolicy::Invocation,
    )
    .expect_err("another port is another provider");
    assert!(
        matches!(error, ResolutionError::CredentialOwnerMismatch { .. }),
        "{error}"
    );
    assert!(!error.to_string().contains("127.0.0.1:1"), "{error}");
}

/// An auth-profile reference resolves only for the provider identity the
/// profile was stored for; a regional alias is a different identity.
#[test]
fn a_profile_reference_is_bound_to_its_owner() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let auth = crate::auth::AuthService::new(temp.path(), false);
    let openai = auth
        .store_provider_token(
            "openai",
            "work",
            PROFILE_SENTINEL,
            std::collections::HashMap::new(),
            false,
        )
        .expect("test: store");
    let qwen = auth
        .store_provider_token("qwen", "cn", PROFILE_SENTINEL, std::collections::HashMap::new(), false)
        .expect("test: store");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);
    let select = |provider: &'static str, id: &str| Selection {
        provider,
        model: "some-model",
        credential: CredentialRef::Profile { id: id.to_string() },
    };

    let released = resolve(
        &select("openai", &openai.id),
        &Requirement::chat(),
        &catalog,
        &credentials,
        UnknownPolicy::Invocation,
    )
    .expect("the owner receives its profile");
    assert_eq!(released.credential().api_key(), Some(PROFILE_SENTINEL));

    for (provider, id) in [("anthropic", &openai.id), ("qwen-us", &qwen.id)] {
        let error = resolve(
            &select(provider, id),
            &Requirement::chat(),
            &catalog,
            &credentials,
            UnknownPolicy::Invocation,
        )
        .expect_err("another identity never receives the profile");
        assert!(
            matches!(error, ResolutionError::CredentialOwnerMismatch { .. }),
            "{provider}: {error}"
        );
        assert!(!error.to_string().contains(PROFILE_SENTINEL));
    }
    let missing = resolve(
        &select("openai", "openai:nope"),
        &Requirement::chat(),
        &catalog,
        &credentials,
        UnknownPolicy::Invocation,
    )
    .expect_err("a missing profile is refused");
    assert!(
        matches!(missing, ResolutionError::CredentialMissing { .. }),
        "{missing}"
    );
}

#[test]
fn anonymous_is_refused_where_the_adapter_needs_a_credential() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let catalog = CatalogView::from_config(&config);
    let anonymous = |provider: &'static str| Selection {
        provider,
        model: "some-model",
        credential: CredentialRef::None,
    };
    let error = resolve(
        &anonymous("anthropic"),
        &Requirement::chat(),
        &catalog,
        &CountingSource::default(),
        UnknownPolicy::Invocation,
    )
    .expect_err("anthropic needs a credential");
    assert!(matches!(error, ResolutionError::CredentialRequired { .. }), "{error}");
    let keyless = resolve(
        &anonymous("custom:http://127.0.0.1:9/v1"),
        &Requirement::chat(),
        &catalog,
        &CountingSource::default(),
        UnknownPolicy::Invocation,
    )
    .expect("a custom endpoint may be keyless");
    assert_eq!(keyless.credential().api_key(), None);
}

#[test]
fn unresolvable_selections_are_refused_with_redacted_text() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let catalog = CatalogView::from_config(&config);
    let source = CountingSource::default();
    for (provider, model) in [
        ("qwen@https://dashscope.aliyuncs.com/compatible-mode/v1", "qwen-max"),
        ("custom:not a url", "m"),
        ("custom:http://alice:pmmb-fixture-pw@127.0.0.1:9/v1", ""),
        ("custom:http://alice:pmmb-fixture-pw@127.0.0.1:9/v1", "hint:fast"),
        ("anthropic", "openai/gpt-4o"),
    ] {
        let error = resolve(
            &root(provider, model),
            &Requirement::chat(),
            &catalog,
            &source,
            UnknownPolicy::Invocation,
        )
        .expect_err("refused");
        let text = error.to_string();
        assert!(!text.contains("pmmb-fixture-pw"), "{text}");
        let capability: ProviderCapabilityError = error.into();
        assert!(!capability.to_string().contains("pmmb-fixture-pw"));
    }
    assert_eq!(source.calls.get(), 0, "a model refusal never reads a credential");
}

/// Agent and route keys belong to the provider their entry names; a route
/// hint that two entries carry is refused rather than guessed.
#[test]
fn agent_and_route_keys_are_bound_to_their_entries_provider() {
    const AGENT_SENTINEL: &str = "PMMB-RESOLVER-AGENT-SENTINEL";
    const ROUTE_SENTINEL: &str = "PMMB-RESOLVER-ROUTE-SENTINEL";
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let mut config = isolated_config(&temp, "openai", "gpt-4o");
    config.agents.insert(
        "coder".to_string(),
        crate::config::DelegateAgentConfig {
            provider: "claude-code".to_string(),
            model: "claude-sonnet-4".to_string(),
            api_key: Some(AGENT_SENTINEL.to_string()),
            credential_ref: None,
            system_prompt: None,
            temperature: None,
            max_depth: 1,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        },
    );
    for hint in ["fast", "dup", "dup"] {
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: hint.to_string(),
            provider: "groq".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
            api_key: Some(ROUTE_SENTINEL.to_string()),
            credential_ref: None,
        });
    }
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);
    let run = |provider: &str, slot: LegacySlot| {
        resolve(
            &Selection {
                provider,
                model: "some-model",
                credential: CredentialRef::LegacyInline(slot),
            },
            &Requirement::chat(),
            &catalog,
            &credentials,
            UnknownPolicy::Invocation,
        )
    };

    let agent = run("anthropic", LegacySlot::Agent("coder".to_string())).expect("any spelling of the owner");
    assert_eq!(agent.credential().api_key(), Some(AGENT_SENTINEL));
    assert_eq!(
        agent.credential().api_url(),
        None,
        "an agent key never borrows the root URL"
    );
    assert!(matches!(
        run("openai", LegacySlot::Agent("coder".to_string())),
        Err(ResolutionError::CredentialOwnerMismatch { .. })
    ));
    assert!(matches!(
        run("anthropic", LegacySlot::Agent("nobody".to_string())),
        Err(ResolutionError::CredentialMissing { .. })
    ));

    let route = run("groq", LegacySlot::ModelRoute("fast".to_string())).expect("the route's own provider");
    assert_eq!(route.credential().api_key(), Some(ROUTE_SENTINEL));
    assert!(route.credential().rotation_keys().is_empty(), "expected an empty value");
    assert!(matches!(
        run("openai", LegacySlot::ModelRoute("fast".to_string())),
        Err(ResolutionError::CredentialOwnerMismatch { .. })
    ));
    let ambiguous = run("groq", LegacySlot::ModelRoute("dup".to_string())).expect_err("ambiguous hint");
    assert!(
        matches!(ambiguous, ResolutionError::CredentialLookup { .. }),
        "{ambiguous}"
    );
    assert!(!ambiguous.to_string().contains(ROUTE_SENTINEL));
}

/// Every spelling of the regional vendors (`is_qwen_alias`, `is_glm_alias`,
/// `is_moonshot_alias`, `is_minimax_alias`, `is_zai_alias`,
/// `is_qianfan_alias`).
const REGIONAL_VENDOR_SPELLINGS: &[&str] = &[
    "qwen",
    "dashscope",
    "qwen-cn",
    "dashscope-cn",
    "qwen-intl",
    "dashscope-intl",
    "qwen-international",
    "dashscope-international",
    "qwen-us",
    "dashscope-us",
    "qwen-code",
    "qwen-oauth",
    "qwen_oauth",
    "dashscope-coding",
    "qwen-coding",
    "dashscope-coding-intl",
    "qwen-coding-intl",
    "glm",
    "zhipu",
    "glm-global",
    "zhipu-global",
    "glm-cn",
    "zhipu-cn",
    "bigmodel",
    "moonshot",
    "kimi",
    "moonshot-cn",
    "kimi-cn",
    "moonshot-intl",
    "moonshot-global",
    "kimi-intl",
    "kimi-global",
    "minimax",
    "minimax-intl",
    "minimax-io",
    "minimax-global",
    "minimax-oauth",
    "minimax-portal",
    "minimax-oauth-global",
    "minimax-portal-global",
    "minimax-cn",
    "minimaxi",
    "minimax-oauth-cn",
    "minimax-portal-cn",
    "zai",
    "z.ai",
    "zai-global",
    "z.ai-global",
    "zai-cn",
    "z.ai-cn",
    "qianfan",
    "baidu",
];

/// D-018 Q8 / R-PMMB F-1: a token pasted with `--provider <spelling>` is
/// owned by the provider identity that spelling names — region included. It
/// is released to that identity and never to another region of the vendor.
#[test]
fn a_pasted_token_keeps_the_region_it_was_issued_for() {
    let mut spellings: Vec<String> = REGIONAL_VENDOR_SPELLINGS.iter().map(ToString::to_string).collect();
    for info in crate::providers::list_providers() {
        spellings.push(info.name.to_string());
        spellings.extend(info.aliases.iter().map(ToString::to_string));
    }
    let mut regional = 0;
    for spelling in &spellings {
        let stored_under = crate::auth::normalize_provider(spelling).expect("test: provider name");
        assert_eq!(
            ProviderId::of(&stored_under),
            ProviderId::of(spelling),
            "`--provider {spelling}` stores the token as '{stored_under}', a different provider identity"
        );
        let vendor = ProviderId::of(spelling)
            .catalog_name()
            .map(ToString::to_string)
            .unwrap_or_default();
        if !vendor.is_empty() && ProviderId::of(&vendor) != ProviderId::of(spelling) {
            regional += 1;
            assert_eq!(
                stored_under,
                spelling.to_ascii_lowercase(),
                "a regional spelling is kept as typed"
            );
        }
    }
    assert!(regional >= 20, "the regional spellings were walked: {regional}");

    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let auth = crate::auth::AuthService::new(temp.path(), false);
    let stored_under = crate::auth::normalize_provider("qwen-us").expect("test: provider name");
    let profile = auth
        .store_provider_token(
            &stored_under,
            "us",
            PROFILE_SENTINEL,
            std::collections::HashMap::new(),
            true,
        )
        .expect("test: store");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);
    let select = |provider: &'static str| {
        resolve(
            &Selection {
                provider,
                model: "qwen-max",
                credential: CredentialRef::Profile { id: profile.id.clone() },
            },
            &Requirement::chat(),
            &catalog,
            &credentials,
            UnknownPolicy::Invocation,
        )
    };
    for provider in ["qwen-us", "dashscope-us"] {
        let released = select(provider).expect("the region it was issued for receives it");
        assert_eq!(released.credential().api_key(), Some(PROFILE_SENTINEL), "{provider}");
    }
    for provider in ["qwen", "qwen-code", "qwen-intl"] {
        let error = select(provider).expect_err("another region never receives it");
        assert!(
            matches!(error, ResolutionError::CredentialOwnerMismatch { .. }),
            "{provider}: {error}"
        );
    }
    // The 42 regional spellings the review counted: every spelling above
    // except the vendor rows' own names and their same-endpoint aliases.
    let vendor_default = [
        "qwen", "glm", "zhipu", "moonshot", "kimi", "minimax", "zai", "z.ai", "qianfan", "baidu",
    ];
    let reviewed: Vec<&str> = REGIONAL_VENDOR_SPELLINGS
        .iter()
        .copied()
        .filter(|spelling| !vendor_default.contains(spelling))
        .collect();
    assert_eq!(reviewed.len(), 42);
    for spelling in reviewed {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let auth = crate::auth::AuthService::new(temp.path(), false);
        let stored_under = crate::auth::normalize_provider(spelling).expect("test: provider name");
        let profile = auth
            .store_provider_token(
                &stored_under,
                "p",
                PROFILE_SENTINEL,
                std::collections::HashMap::new(),
                true,
            )
            .expect("test: store");
        let config = isolated_config(&temp, "openai", "gpt-4o");
        let catalog = CatalogView::from_config(&config);
        let credentials = ConfigCredentialSource::new(&config);
        let run = |provider: &str| {
            resolve(
                &Selection {
                    provider,
                    model: "some-model",
                    credential: CredentialRef::Profile { id: profile.id.clone() },
                },
                &Requirement::chat(),
                &catalog,
                &credentials,
                UnknownPolicy::Invocation,
            )
        };
        let own = run(spelling).expect("released to the spelling it was pasted for");
        assert_eq!(own.credential().api_key(), Some(PROFILE_SENTINEL), "{spelling}");
        let vendor = ProviderId::of(spelling).catalog_name().unwrap_or_default().to_string();
        if ProviderId::of(&vendor) != ProviderId::of(spelling) {
            assert!(
                matches!(run(&vendor), Err(ResolutionError::CredentialOwnerMismatch { .. })),
                "a token pasted for '{spelling}' was released to '{vendor}'"
            );
        }
    }
}

/// R-PMMB F-2: `create_provider_for` must not serve a `model_fallbacks`
/// sibling on the same endpoint in the primary's dialect. The primary is
/// declared `no_root_combinators`, the sibling nothing; a sibling request, if
/// one is made at all, carries the published root `allOf`.
#[tokio::test]
async fn a_fallback_model_never_inherits_the_primarys_lowered_schema() {
    use crate::providers::tool_dialect_config_tests::{mergeable_tool, root_combinators_in};
    use crate::providers::traits::{ChatMessage, ChatRequest};

    let stub = FallbackStub::start().await;
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = stub.label();
    let mut config = isolated_config(&temp, &label, "model-a");
    config
        .providers
        .tool_dialects
        .push(crate::config::schema::ProviderToolDialectConfig {
            provider: label.clone(),
            model: Some("model-a".to_string()),
            tool_schema: Some(crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators),
            tool_call: None,
        });
    config
        .reliability
        .model_fallbacks
        .insert("model-a".to_string(), vec!["model-b".to_string()]);
    let catalog = CatalogView::from_config(&config);
    let resolved = resolve(
        &root(&label, "model-a"),
        &Requirement::chat(),
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect("test: the primary resolves");

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    let outcome = match build_for(&resolved, &catalog, &config) {
        Ok(provider) => provider
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: Some(&tools),
                },
                "model-a",
                0.0,
            )
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    for body in stub.bodies() {
        if body.get("model").and_then(serde_json::Value::as_str) == Some("model-b") {
            assert!(
                !root_combinators_in(&body).is_empty(),
                "the undeclared fallback model was sent the primary's lowered schema: {body}"
            );
        }
    }
    if let Err(error) = outcome {
        let text = format!("{error:#}");
        assert!(text.contains("model-b"), "a refusal names the fallback: {text}");
    }
}

/// A loopback OpenAI-compatible endpoint that records every body, answers
/// the first request `503` and every later one `200`.
struct FallbackStub {
    addr: std::net::SocketAddr,
    bodies: std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FallbackStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FallbackStub {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        let bodies: std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>> = std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&bodies);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 8192];
                let body = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    let Some(split) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buffer.len() >= split + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(&buffer[split + 4..split + 4 + length]).ok();
                    }
                };
                let first = {
                    let mut recorded = sink.lock();
                    recorded.push(body.unwrap_or(serde_json::Value::Null));
                    recorded.len() == 1
                };
                let (status, payload) = if first {
                    (
                        "503 Service Unavailable",
                        r#"{"error":{"message":"overloaded"}}"#.to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#
                            .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self { addr, bodies, server }
    }

    fn label(&self) -> String {
        format!("custom:http://{}/v1", self.addr)
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies.lock().clone()
    }
}

/// Build the chain for a resolved model through the production seam.
fn build_for(
    resolved: &super::resolver::ResolvedModel,
    catalog: &CatalogView,
    config: &Config,
) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_for(
        resolved,
        catalog,
        &config.reliability,
        &config.cost,
        &provider_runtime_options_from_config(config),
    )
}

/// A fallback on the same endpoint declared with the primary's dialect is
/// kept and served in it; a fallback the catalog rules out for chat is
/// refused before anything is built.
#[tokio::test]
async fn a_fallback_with_the_same_declared_dialect_is_served_and_a_non_chat_one_is_refused() {
    use crate::providers::tool_dialect_config_tests::{mergeable_tool, root_combinators_in};
    use crate::providers::traits::{ChatMessage, ChatRequest};

    let stub = FallbackStub::start().await;
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = stub.label();
    let mut config = isolated_config(&temp, &label, "model-a");
    for model in ["model-a", "model-b"] {
        config
            .providers
            .tool_dialects
            .push(crate::config::schema::ProviderToolDialectConfig {
                provider: label.clone(),
                model: Some(model.to_string()),
                tool_schema: Some(crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators),
                tool_call: None,
            });
    }
    config
        .reliability
        .model_fallbacks
        .insert("model-a".to_string(), vec!["model-b".to_string()]);
    let catalog = CatalogView::from_config(&config);
    let resolved = resolve(
        &root(&label, "model-a"),
        &Requirement::chat(),
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect("test: resolves");
    let provider = build_for(&resolved, &catalog, &config).expect("test: same-dialect fallback is kept");
    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "model-a",
            0.0,
        )
        .await
        .expect("test: the fallback serves the turn");
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 2, "primary 503, then the fallback");
    assert_eq!(
        bodies[1].get("model").and_then(serde_json::Value::as_str),
        Some("model-b")
    );
    assert!(
        root_combinators_in(&bodies[1]).is_empty(),
        "the declared fallback is lowered: {}",
        bodies[1]
    );

    let decision = catalog.with_curated([super::CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..super::CuratedModel::new(ProviderId::of(&label), Some("model-b"))
    }]);
    let Err(error) = build_for(&resolved, &decision, &config) else {
        panic!("a decision-only fallback is refused");
    };
    let text = format!("{error:#}");
    assert!(
        text.contains("model-b") && text.contains("cannot be used for chat"),
        "{text}"
    );
}

// ── PMM-C-1: per-member resolution ──────────────────────────────────────

fn declare(config: &mut Config, provider: &str, model: Option<&str>, entry: &str) {
    let parsed: crate::config::schema::ProviderModelConfig = toml::from_str(&format!(
        "provider = \"{provider}\"\n{}{entry}",
        model.map(|m| format!("model = \"{m}\"\n")).unwrap_or_default()
    ))
    .expect("test: a valid [[providers.models]] entry");
    config.providers.models.push(parsed);
}

/// C2af-01 on the path every surface builds today: a declaration for one
/// model of an endpoint no longer decides what another model's turn is held
/// to. With no model to go on, the endpoint answers the strict published
/// contract (`Raw`) unless every model it could serve agrees.
#[test]
fn a_model_scoped_declaration_does_not_widen_the_endpoints_enforcement() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = "custom:http://127.0.0.1:9/v1";
    let mut config = isolated_config(&temp, label, "model-b");
    declare(
        &mut config,
        label,
        Some("model-a"),
        "tool_schema_dialect = \"no_root_combinators\"\n",
    );
    let provider = crate::providers::create_resilient_provider_with_options(
        label,
        None,
        None,
        &config.reliability,
        &provider_runtime_options_from_config(&config),
    )
    .expect("test: construct");
    assert_eq!(
        provider.tool_schema_dialect(),
        ToolSchemaDialect::Raw,
        "a model-b turn on this endpoint is enforced against model-a's rewrite"
    );

    let mut wide = isolated_config(&temp, label, "model-b");
    declare(
        &mut wide,
        label,
        None,
        "tool_schema_dialect = \"no_root_combinators\"\n",
    );
    let provider = crate::providers::create_resilient_provider_with_options(
        label,
        None,
        None,
        &wide.reliability,
        &provider_runtime_options_from_config(&wide),
    )
    .expect("test: construct");
    assert_eq!(
        provider.tool_schema_dialect(),
        ToolSchemaDialect::NoRootCombinators,
        "a provider-wide declaration covers every model"
    );
}

/// R-PMMBf N-3: a model fallback is held to the primary's requirement and
/// policy, and every `fallback_providers` member is described for the models
/// it may serve.
#[test]
fn every_chain_member_is_checked_against_the_primarys_requirement() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let label = "custom:http://127.0.0.1:9/v1";
    let other = "custom:http://127.0.0.1:10/v1";
    let requirement = Requirement::chat().with(Feature::VisionInput);

    let mut config = isolated_config(&temp, label, "model-a");
    declare(
        &mut config,
        label,
        Some("model-a"),
        "operations = [\"chat\"]\nvision_input = true\n",
    );
    declare(
        &mut config,
        label,
        Some("model-b"),
        "operations = [\"chat\"]\nvision_input = false\n",
    );
    config
        .reliability
        .model_fallbacks
        .insert("model-a".to_string(), vec!["model-b".to_string()]);
    let catalog = CatalogView::from_config(&config);
    let resolved = resolve(
        &root(label, "model-a"),
        &requirement,
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::DurableSelection,
    )
    .expect("test: the primary is verified");
    let Err(error) = build_for(&resolved, &catalog, &config) else {
        panic!("a fallback without image input is chained for an image request");
    };
    assert!(format!("{error:#}").contains("model-b"), "{error:#}");

    let mut unknown = isolated_config(&temp, label, "model-a");
    declare(
        &mut unknown,
        label,
        Some("model-a"),
        "operations = [\"chat\"]\nvision_input = true\n",
    );
    unknown
        .reliability
        .model_fallbacks
        .insert("model-a".to_string(), vec!["model-c".to_string()]);
    let catalog = CatalogView::from_config(&unknown);
    let resolved = resolve(
        &root(label, "model-a"),
        &requirement,
        &catalog,
        &ConfigCredentialSource::new(&unknown),
        UnknownPolicy::DurableSelection,
    )
    .expect("test: the primary is verified");
    let Err(error) = build_for(&resolved, &catalog, &unknown) else {
        panic!("an unverified fallback is chained under the durable policy");
    };
    assert!(format!("{error:#}").contains("model-c"), "{error:#}");

    let mut member = isolated_config(&temp, label, "model-a");
    declare(&mut member, label, Some("model-a"), "operations = [\"chat\"]\n");
    declare(&mut member, other, Some("model-a"), "operations = [\"embedding\"]\n");
    member.reliability.fallback_providers = vec![other.to_string()];
    let catalog = CatalogView::from_config(&member);
    let resolved = resolve(
        &root(label, "model-a"),
        &Requirement::chat(),
        &catalog,
        &ConfigCredentialSource::new(&member),
        UnknownPolicy::Invocation,
    )
    .expect("test: the primary resolves");
    let Err(error) = build_for(&resolved, &catalog, &member) else {
        panic!("a fallback provider that cannot chat with the model is chained");
    };
    assert!(format!("{error:#}").contains("127.0.0.1:10"), "{error:#}");
}

/// A caller that knows the model is told the dialect of the destination and
/// model that serve it: a routed Gemini turn is held to the Gemini document,
/// a default-route turn to the published one.
#[test]
fn a_routed_turn_is_enforced_in_its_destinations_dialect() {
    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let routes = vec![crate::config::ModelRouteConfig {
        hint: "vision".to_string(),
        provider: "gemini".to_string(),
        model: "gemini-2.5-pro".to_string(),
        api_key: Some("PMMC-ROUTE-SENTINEL".to_string()),
        credential_ref: None,
    }];
    let router = crate::providers::create_routed_provider_with_options(
        "openai",
        Some(ROOT_SENTINEL),
        None,
        &config.reliability,
        &routes,
        "gpt-4o",
        &provider_runtime_options_from_config(&config),
    )
    .expect("test: construct");
    assert_eq!(router.tool_schema_dialect_for("hint:vision"), ToolSchemaDialect::Gemini);
    assert_eq!(router.tool_schema_dialect_for("gpt-4o"), ToolSchemaDialect::Raw);
    assert_eq!(
        router.tool_schema_dialect(),
        ToolSchemaDialect::Raw,
        "mixed destinations without a model"
    );
}

/// R-PMMBf N-1: an identity whose URL path, userinfo or plugin name is case
/// sensitive keeps its case through `--provider`: a token pasted for one
/// path is released to that path and not to its lowercased neighbour.
#[test]
fn a_pasted_token_keeps_a_case_sensitive_identity() {
    let mut spellings = vec![
        "custom:https://h.invalid/V1",
        "custom:https://Alice:PmmC-Pw@h.invalid/v1",
        "anthropic-custom:https://h.invalid/Api",
    ];
    if cfg!(feature = "wasm-plugins") {
        spellings.push("wasm:EchoPlugin");
    }
    for spelling in spellings {
        let stored_under = crate::auth::normalize_provider(spelling).expect("test: provider name");
        assert_eq!(
            ProviderId::of(&stored_under),
            ProviderId::of(spelling),
            "`--provider {spelling}` stored the token under a different identity"
        );
    }

    let temp = tempfile::TempDir::new().expect("test: temp dir");
    let auth = crate::auth::AuthService::new(temp.path(), false);
    let stored_under = crate::auth::normalize_provider("custom:https://h.invalid/V1").expect("test: name");
    let profile = auth
        .store_provider_token(
            &stored_under,
            "p",
            PROFILE_SENTINEL,
            std::collections::HashMap::new(),
            true,
        )
        .expect("test: store");
    let config = isolated_config(&temp, "openai", "gpt-4o");
    let catalog = CatalogView::from_config(&config);
    let credentials = ConfigCredentialSource::new(&config);
    let run = |provider: &str| {
        resolve(
            &Selection {
                provider,
                model: "m",
                credential: CredentialRef::Profile { id: profile.id.clone() },
            },
            &Requirement::chat(),
            &catalog,
            &credentials,
            UnknownPolicy::Invocation,
        )
    };
    assert_eq!(
        run("custom:https://H.invalid/V1/")
            .expect("same identity")
            .credential()
            .api_key(),
        Some(PROFILE_SENTINEL)
    );
    assert!(matches!(
        run("custom:https://h.invalid/v1"),
        Err(ResolutionError::CredentialOwnerMismatch { .. })
    ));
}

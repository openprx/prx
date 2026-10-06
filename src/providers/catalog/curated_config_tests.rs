//! PMM-B-7: `[[providers.models]]`, from the configuration file through the
//! production loader to the resolver, the factory and a loopback endpoint.
//! Every config lives in a temporary directory; every credential is a
//! sentinel.

#![allow(clippy::indexing_slicing)]

use tempfile::TempDir;

use super::CatalogView;
use super::descriptor::{Feature, ModelOperation, Requirement};
use super::resolver::{ResolutionError, Selection, UnknownPolicy, resolve};
use crate::providers::credentials::{ConfigCredentialSource, CredentialRef, LegacySlot};
use crate::providers::tool_dialect_config_tests::{
    FIXTURE_MODEL, load_config, mergeable_tool, root_combinators_in, spawn_dialect_stub,
};
use crate::providers::traits::{ChatMessage, ChatRequest};
use crate::providers::{create_resilient_provider_with_options, provider_runtime_options_from_config};
use crate::tools::ToolSchemaDialect;

const ROOT_SENTINEL: &str = "PMMB-CURATED-ROOT-SENTINEL";

fn config_for(provider: &str, extra: &str) -> String {
    format!(
        "default_temperature = 0.0\ndefault_provider = \"{provider}\"\ndefault_model = \"{FIXTURE_MODEL}\"\n\
         api_key = \"{ROOT_SENTINEL}\"\n{extra}\n\n[reliability]\nprovider_retries = 0\nprovider_backoff_ms = 1\n\n\
         [secrets]\nencrypt = false\n"
    )
}

fn load_error(body: &str) -> String {
    let dir = TempDir::new().expect("test: temp config dir");
    format!(
        "{:#}",
        load_config(&dir, body).expect_err("test: the config must be refused")
    )
}

/// The C2 closed-port scenario, declared in the new table: the endpoint
/// that rejects a root combinator receives the merged document.
#[tokio::test]
async fn a_curated_no_root_combinators_declaration_reaches_the_wire() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &config_for(
            &label,
            &format!(
                "[[providers.models]]\nprovider = \"{label}\"\nmodel = \"{FIXTURE_MODEL}\"\noperations = [\"chat\"]\n\
                 native_tools = true\ntool_schema_dialect = \"no_root_combinators\"\n"
            ),
        ),
    )
    .expect("test: a curated declaration for a custom endpoint loads");
    let provider = create_resilient_provider_with_options(
        &label,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options_from_config(&config),
    )
    .expect("test: construct");

    let tools = [mergeable_tool()];
    let messages = [ChatMessage::user("look it up")];
    provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            FIXTURE_MODEL,
            0.0,
        )
        .await
        .expect("test: the declared endpoint accepts the lowered document");
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(root_combinators_in(&bodies[0]).is_empty(), "{}", bodies[0]);

    let resolved = resolve(
        &Selection {
            provider: &label,
            model: FIXTURE_MODEL,
            credential: CredentialRef::LegacyInline(LegacySlot::Root),
        },
        &Requirement::chat().with(Feature::NativeTools),
        &CatalogView::from_config(&config),
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::DurableSelection,
    )
    .expect("test: a declared chat model with tools is a verified durable selection");
    assert_eq!(resolved.dialects().tool_schema, ToolSchemaDialect::NoRootCombinators);
}

/// The shipped `[[providers.tool_dialects]]` key stays accepted input; the
/// same declaration in either table gives the same runtime options.
#[test]
fn the_dialect_key_and_the_models_table_give_identical_runtime_options() {
    let label = "custom:http://127.0.0.1:9/v1";
    let dialect_key = config_for(
        label,
        &format!(
            "[[providers.tool_dialects]]\nprovider = \"{label}\"\ntool_call = \"xml_function_tags\"\n\n\
             [[providers.tool_dialects]]\nprovider = \"{label}\"\nmodel = \"{FIXTURE_MODEL}\"\n\
             tool_schema = \"no_root_combinators\"\n"
        ),
    );
    let models_table = config_for(
        label,
        &format!(
            "[[providers.models]]\nprovider = \"{label}\"\ntool_call_dialect = \"xml_function_tags\"\n\n\
             [[providers.models]]\nprovider = \"{label}\"\nmodel = \"{FIXTURE_MODEL}\"\n\
             tool_schema_dialect = \"no_root_combinators\"\n"
        ),
    );
    let first = TempDir::new().expect("test: temp dir");
    let second = TempDir::new().expect("test: temp dir");
    let from_key = provider_runtime_options_from_config(&load_config(&first, &dialect_key).expect("test: key loads"));
    let from_table =
        provider_runtime_options_from_config(&load_config(&second, &models_table).expect("test: table loads"));
    assert_eq!(from_key.tool_schema_dialects, from_table.tool_schema_dialects);
    assert_eq!(from_key.tool_call_dialects, from_table.tool_call_dialects);
    assert_eq!(from_key.tool_schema_dialects.len(), 1);
    assert_eq!(from_key.tool_call_dialects.len(), 1);
}

/// A model declared decision-only is refused for chat by the resolver, with
/// nothing sent to the endpoint.
#[tokio::test]
async fn a_declared_decision_only_model_is_refused_for_chat_before_transport() {
    let stub = spawn_dialect_stub("ok").await;
    let dir = TempDir::new().expect("test: temp config dir");
    let label = stub.custom_label();
    let config = load_config(
        &dir,
        &config_for(
            &label,
            &format!(
                "[[providers.models]]\nprovider = \"{label}\"\nmodel = \"{FIXTURE_MODEL}\"\n\
                 operations = [\"decision_endpoint\"]\n"
            ),
        ),
    )
    .expect("test: loads");
    let error = resolve(
        &Selection {
            provider: &label,
            model: FIXTURE_MODEL,
            credential: CredentialRef::LegacyInline(LegacySlot::Root),
        },
        &Requirement::chat(),
        &CatalogView::from_config(&config),
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect_err("a decision-only model is not a chat model");
    assert!(
        matches!(
            &error,
            ResolutionError::OperationUnsupported {
                operation: ModelOperation::Chat,
                ..
            }
        ),
        "{error}"
    );
    assert!(!error.to_string().contains(ROOT_SENTINEL));
    assert!(stub.bodies().is_empty(), "nothing reaches the endpoint");

    let tools_off = load_config(
        &TempDir::new().expect("test: temp dir"),
        &config_for(
            &label,
            &format!("[[providers.models]]\nprovider = \"{label}\"\nnative_tools = false\n"),
        ),
    )
    .expect("test: loads");
    let error = resolve(
        &Selection {
            provider: &label,
            model: "any-model",
            credential: CredentialRef::ProviderStore,
        },
        &Requirement::chat().with(Feature::NativeTools),
        &CatalogView::from_config(&tools_off),
        &ConfigCredentialSource::new(&tools_off),
        UnknownPolicy::Invocation,
    )
    .expect_err("a provider-wide `native_tools = false` refuses tools for every model");
    assert!(matches!(error, ResolutionError::FeatureUnsupported { .. }), "{error}");
}

#[test]
fn invalid_models_entries_are_refused_at_load_by_index() {
    let custom = "custom:http://127.0.0.1:9/v1";
    for (extra, expected) in [
        (
            format!("[[providers.models]]\nprovider = \"{custom}\"\n"),
            "providers.models[0] declares nothing",
        ),
        (
            "[[providers.models]]\nprovider = \"no-such-provider\"\noperations = [\"chat\"]\n".to_string(),
            "providers.models[0].provider 'no-such-provider' names no provider",
        ),
        (
            format!("[[providers.models]]\nprovider = \"{custom}\"\nmodel = \"\"\noperations = [\"chat\"]\n"),
            "providers.models[0].model must not be empty",
        ),
        (
            format!("[[providers.models]]\nprovider = \"{custom}\"\noperations = []\n"),
            "must list at least one operation",
        ),
        (
            "[[providers.models]]\nprovider = \"openai\"\ntool_schema_dialect = \"no_root_combinators\"\n".to_string(),
            "does not take a tool dialect declaration",
        ),
        (
            format!(
                "[[providers.models]]\nprovider = \"{custom}\"\nmodel = \"m\"\ntool_call_dialect = \"xml_function_tags\"\n"
            ),
            "must be declared on an entry without `model`",
        ),
        (
            format!(
                "[[providers.models]]\nprovider = \"{custom}\"\nmodel = \"M\"\noperations = [\"chat\"]\n\n\
                 [[providers.models]]\nprovider = \"CUSTOM:http://127.0.0.1:9/v1/\"\nmodel = \"m\"\nnative_tools = true\n"
            ),
            "providers.models[1] duplicates providers.models[0]",
        ),
        (
            format!(
                "[[providers.models]]\nprovider = \"{custom}\"\ntool_schema_dialect = \"raw\"\n\n\
                 [[providers.tool_dialects]]\nprovider = \"{custom}\"\ntool_schema = \"no_root_combinators\"\n"
            ),
            "providers.models[0] and providers.tool_dialects[0] both declare a tool dialect",
        ),
        (
            format!("[[providers.models]]\nprovider = \"{custom}\"\ncontext_window = 0\n"),
            "context_window must be greater than zero",
        ),
    ] {
        let error = load_error(&config_for(custom, &extra));
        assert!(error.contains(expected), "expected `{expected}` in: {error}");
    }
    let error = load_error(&config_for(
        custom,
        &format!("[[providers.models]]\nprovider = \"{custom}\"\noperations = [\"chat_completion\"]\n"),
    ));
    assert!(
        error.contains("decision_endpoint"),
        "the accepted values are listed: {error}"
    );
}

#[test]
fn a_models_entry_error_never_prints_url_userinfo() {
    let secret = "custom:http://alice:pmmb-fixture-pw@127.0.0.1:9/v1";
    for extra in [
        format!(
            "[[providers.models]]\nprovider = \"{secret}\"\ntool_schema_dialect = \"raw\"\n\n\
             [[providers.tool_dialects]]\nprovider = \"{secret}\"\ntool_schema = \"raw\"\n"
        ),
        format!(
            "[[providers.models]]\nprovider = \"{secret}\"\noperations = [\"chat\"]\n\n\
             [[providers.models]]\nprovider = \"{secret}\"\nstreaming = true\n"
        ),
    ] {
        let error = load_error(&config_for("openai", &extra));
        assert!(!error.contains("pmmb-fixture-pw"), "{error}");
        assert!(error.contains("[redacted]"), "{error}");
    }
}

#[test]
fn one_file_owns_the_models_table() {
    let dir = TempDir::new().expect("test: temp config dir");
    let fragment_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&fragment_dir).expect("test: config.d");
    let entry = |port: u16| {
        format!("[[providers.models]]\nprovider = \"custom:http://127.0.0.1:{port}/v1\"\nstreaming = true\n")
    };
    std::fs::write(fragment_dir.join("routing.toml"), entry(2)).expect("test: write fragment");
    let error = format!(
        "{:#}",
        load_config(&dir, &config_for("openai", &entry(1))).expect_err("test: two owners are refused")
    );
    assert!(
        error.contains("providers.models") && error.contains("routing.toml") && error.contains("config.toml"),
        "{error}"
    );
    let alone = TempDir::new().expect("test: temp config dir");
    std::fs::create_dir_all(alone.path().join("config.d")).expect("test: config.d");
    std::fs::write(alone.path().join("config.d").join("routing.toml"), entry(2)).expect("test: write fragment");
    let config = load_config(&alone, &config_for("openai", "")).expect("test: one owner loads");
    assert_eq!(config.providers.models.len(), 1);
}

#[test]
fn an_empty_models_table_is_not_written() {
    let config = crate::config::Config::default();
    let rendered = toml::to_string(&config.providers).expect("test: serialize");
    assert!(!rendered.contains("models"), "{rendered}");
}

/// The one-table and duplicate errors show the model as the operator wrote it.
#[test]
fn a_models_entry_error_names_the_model_as_written() {
    let custom = "custom:http://127.0.0.1:9/v1";
    let error = load_error(&config_for(
        custom,
        &format!(
            "[[providers.models]]\nprovider = \"{custom}\"\nmodel = \"Vendor/Model-X\"\ntool_schema_dialect = \"raw\"\n\n\
             [[providers.tool_dialects]]\nprovider = \"{custom}\"\nmodel = \"vendor/model-x\"\ntool_schema = \"raw\"\n"
        ),
    ));
    assert!(error.contains("model 'Vendor/Model-X'"), "{error}");
    let error = load_error(&config_for(
        custom,
        &format!(
            "[[providers.models]]\nprovider = \"{custom}\"\nmodel = \"Vendor/Model-X\"\nstreaming = true\n\n\
             [[providers.models]]\nprovider = \"{custom}\"\nmodel = \"vendor/model-x\"\nstreaming = false\n"
        ),
    ));
    assert!(
        error.contains("model 'vendor/model-x'"),
        "the duplicate names its own spelling: {error}"
    );
}

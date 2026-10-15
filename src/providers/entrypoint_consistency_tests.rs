use super::{configured_connection_for, provider_runtime_options_from_config, scoped_reliability};
use crate::config::Config;
use crate::providers::traits::{ChatMessage, ChatRequest};

const PRIMARY_MODEL: &str = "model-a";
const FALLBACK_MODEL: &str = "model-b";

fn cross_dialect_config(temp: &tempfile::TempDir) -> Config {
    let provider = "custom:http://127.0.0.1:9/v1";
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(provider.to_string());
    config.default_model = Some(PRIMARY_MODEL.to_string());
    config.api_key = None;
    config.api_url = None;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config.secrets.encrypt = false;
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config
        .reliability
        .model_fallbacks
        .insert(PRIMARY_MODEL.to_string(), vec![FALLBACK_MODEL.to_string()]);
    config
        .providers
        .tool_dialects
        .push(crate::config::schema::ProviderToolDialectConfig {
            provider: provider.to_string(),
            model: Some(PRIMARY_MODEL.to_string()),
            tool_schema: Some(crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators),
            tool_call: None,
        });
    config
}

fn refusal<T>(result: anyhow::Result<T>) -> String {
    match result {
        Ok(_) => panic!("cross-dialect fallback reached provider construction"),
        Err(error) => format!("{error:#}"),
    }
}

/// Finding 42: chat, the CLI/cron shared entry, channels, gateway and a
/// session worker must all retain the same resolved model facts through their
/// real production builders. The fallback differs only in tool-schema dialect;
/// every surface must refuse it before any loopback transport is attempted.
#[test]
fn cross_dialect_fallback_is_refused_by_every_production_entry_builder() {
    let temp = tempfile::tempdir().expect("test: temp config dir");
    let config = cross_dialect_config(&temp);
    let provider = config.default_provider.as_deref().expect("test: provider");
    let options = provider_runtime_options_from_config(&config);

    let chat = refusal(crate::chat::chat_provider_chain(
        &config,
        provider,
        provider,
        PRIMARY_MODEL,
        &options,
    ));

    let cli_choice = crate::agent::entry::resolve_agent_model_choice(&config, None, None)
        .expect("test: CLI selection resolves before its fallback chain is built");
    let cli =
        refusal(crate::agent::entry::agent_provider_selection(&config, &cli_choice).build_provider(&config, &options));
    let cron_choice = crate::agent::entry::resolve_agent_model_choice(&config, None, None)
        .expect("test: cron selection resolves before its fallback chain is built");
    let cron =
        refusal(crate::agent::entry::agent_provider_selection(&config, &cron_choice).build_provider(&config, &options));

    let channels = refusal(crate::channels::build_channel_startup_provider(&config, &options));
    let gateway = refusal(crate::gateway::build_gateway_provider_selection(&config, &options));

    let connection =
        configured_connection_for(provider, provider, config.api_key.as_deref(), config.api_url.as_deref());
    let reliability = scoped_reliability(&config.reliability, &connection);
    let worker = refusal(crate::session_worker::runner::build_worker_provider_selection(
        &config,
        provider,
        PRIMARY_MODEL,
        crate::providers::selection::credential_for(&config, provider),
        &connection,
        &reliability,
        &options,
    ));

    let refusals = [
        ("chat", chat),
        ("cli", cli),
        ("cron", cron),
        ("channels", channels),
        ("gateway", gateway),
        ("worker", worker),
    ];
    let expected = &refusals[0].1;
    assert!(expected.contains(FALLBACK_MODEL), "{expected}");
    assert!(expected.contains("speaks tool-schema dialect"), "{expected}");
    for (surface, refusal) in &refusals[1..] {
        assert_eq!(refusal, expected, "{surface} changed the resolver/factory refusal");
    }
}

/// Finding 40's fallback admission must survive the finding-42 entrypoint
/// factories. Both real builders send the tool-bearing request to the primary,
/// then refuse an unpriced fallback and a tool-less fallback before transport.
#[tokio::test]
async fn chat_and_cli_keep_fallback_admission_after_primary_429() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let temp = tempfile::tempdir().expect("test: temp config dir");
    let primary = CredentialProbe::start_with_statuses(vec![429, 429]).await;
    let unpriced = CredentialProbe::start().await;
    let tool_less = CredentialProbe::start().await;
    let primary_name = primary.custom_provider();
    let unpriced_name = unpriced.custom_provider();
    let tool_less_name = tool_less.custom_provider();
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(primary_name.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config.reliability.provider_retries = 0;
    config.reliability.provider_backoff_ms = 0;
    config.reliability.fallback_providers = vec![unpriced_name.clone(), tool_less_name.clone()];
    for (provider, native_tools) in [(&primary_name, true), (&unpriced_name, true), (&tool_less_name, false)] {
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": provider,
                "model": PROBE_MODEL,
                "operations": ["chat"],
                "native_tools": native_tools,
            }))
            .expect("test: model declaration"),
        );
    }
    for provider in [&primary_name, &tool_less_name] {
        config.cost.prices.insert(
            format!("{provider}/{PROBE_MODEL}"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
    }

    let options = provider_runtime_options_from_config(&config);
    let (_, chat) = crate::chat::chat_provider_chain(&config, &primary_name, &primary_name, PROBE_MODEL, &options)
        .expect("test: chat chain builds");
    let cli_choice =
        crate::agent::entry::resolve_agent_model_choice(&config, None, None).expect("test: CLI selection resolves");
    let cli = crate::agent::entry::agent_provider_selection(&config, &cli_choice)
        .build_provider(&config, &options)
        .expect("test: CLI chain builds");
    let messages = [ChatMessage::user("Use probe_tool.")];
    let tools = [crate::tools::ToolSpec {
        name: "probe_tool".to_string(),
        description: "Test fallback admission".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];
    for (surface, provider) in [("chat", chat), ("cli", cli)] {
        let error = provider
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: Some(&tools),
                },
                PROBE_MODEL,
                0.0,
            )
            .await
            .expect_err("test: no inadmissible fallback can serve");
        let shown = format!("{error:#}");
        assert!(
            shown.contains("429") || shown.contains("rate limit"),
            "{surface}: {shown}"
        );
    }

    assert_eq!(primary.requests().len(), 2, "chat and CLI each reach the primary once");
    assert!(
        unpriced.requests().is_empty(),
        "the unpriced fallback reached transport"
    );
    assert!(
        tool_less.requests().is_empty(),
        "the tool-less fallback reached transport"
    );
}

/// Finding 61: every long-lived surface starts when an unrelated route is
/// refused, and selecting that hint returns the same retained refusal.
#[tokio::test]
async fn broken_route_is_retained_as_the_same_refusal_on_every_surface() {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let temp = tempfile::tempdir().expect("test: temp config dir");
    let probe = CredentialProbe::start().await;
    let primary = probe.custom_provider();
    let mut config = Config::default();
    config.workspace_dir = temp.path().to_path_buf();
    config.config_path = temp.path().join("config.toml");
    config.default_provider = Some(primary.clone());
    config.default_model = Some(PROBE_MODEL.to_string());
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = temp.path().join("no-codex-auth.json");
    config.reliability.provider_retries = 0;
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "broken".to_string(),
        provider: "prx-unknown-provider".to_string(),
        model: "prx-unknown-model".to_string(),
        api_key: None,
        credential_ref: None,
    });
    let options = provider_runtime_options_from_config(&config);

    let (_, chat) = crate::chat::chat_provider_chain(&config, &primary, &primary, PROBE_MODEL, &options)
        .expect("chat starts with the refused route retained");
    let choice =
        crate::agent::entry::resolve_agent_model_choice(&config, None, None).expect("CLI default selection resolves");
    let cli = crate::agent::entry::agent_provider_selection(&config, &choice)
        .build_provider(&config, &options)
        .expect("CLI starts with the refused route retained");
    let (channels, _, _) = crate::channels::build_channel_startup_provider(&config, &options)
        .expect("channels start with the refused route retained");
    let (gateway, _) = crate::gateway::build_gateway_provider_selection(&config, &options)
        .expect("gateway starts with the refused route retained");

    let mut refusals = Vec::new();
    for (surface, provider) in [
        ("chat", std::sync::Arc::<dyn crate::providers::Provider>::from(chat)),
        ("cli", std::sync::Arc::<dyn crate::providers::Provider>::from(cli)),
        ("channels", channels),
        ("gateway", gateway),
    ] {
        let error = provider
            .simple_chat("ping", "hint:broken", 0.0)
            .await
            .expect_err("the refused hint cannot reach transport");
        let refusal = format!("{error:#}");
        assert!(refusal.contains("hint:broken"), "{surface}: {refusal}");
        assert!(refusal.contains("prx-unknown-provider"), "{surface}: {refusal}");
        refusals.push((surface, refusal));
    }
    for (surface, refusal) in &refusals[1..] {
        assert_eq!(refusal, &refusals[0].1, "{surface} changed the retained route refusal");
    }
    assert!(
        probe.requests().is_empty(),
        "a refused route reached the default transport"
    );
}

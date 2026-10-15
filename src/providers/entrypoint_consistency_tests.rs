use super::{configured_connection_for, provider_runtime_options_from_config, scoped_reliability};
use crate::config::Config;

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

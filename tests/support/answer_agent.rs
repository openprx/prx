//! Prediction-free Answer admission for the integration callers (R9).
//! Every turn opens without a decision hop; the serving model answers.
use anyhow::Result;
use openprx::agent::agent::{Agent, AgentBuilder};
use openprx::agent::dispatcher::NativeToolDispatcher;
use openprx::config::{Config, schema::ModelPricing};
use openprx::memory::SqliteMemory;
use openprx::providers::Provider;
use serde_json::json;
use std::sync::Arc;

pub async fn run<T, F, Fut>(provider: Box<dyn Provider>, case: F) -> Result<T>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut workspace_builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        workspace_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let workspace = workspace_builder.tempdir()?;
    let mut config = Config {
        workspace_dir: workspace.path().to_owned(),
        config_path: workspace.path().join("config.toml"),
        default_model: Some("prx-local-model".to_owned()),
        ..Config::default()
    };
    config.memory.auto_save = false;
    config.autonomy.level = openprx::security::AutonomyLevel::ReadOnly;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    // Chat only for this exact model; no native-tool authority is requested.
    config.providers.models.push(serde_json::from_value(json!({
        "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":false
    }))?);
    let memory = Arc::new(SqliteMemory::new(workspace.path())?);
    let builder = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(provider)
        .tools(vec![])
        .memory(memory)
        .observer(Arc::new(openprx::observability::NoopObserver))
        .tool_dispatcher(Box::new(NativeToolDispatcher));
    Ok(case(builder, workspace).await)
}

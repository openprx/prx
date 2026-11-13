//! Prediction-free native-tool admission for the integration callers (R9).
//! Every turn opens without a decision hop: the configured Direct tool is
//! published to the serving model, which chooses the call.
use anyhow::Result;
use openprx::agent::agent::{Agent, AgentBuilder};
use openprx::agent::dispatcher::NativeToolDispatcher;
use openprx::config::{Config, schema::ModelPricing};
use openprx::memory::SqliteMemory;
use openprx::providers::{ChatRequest, ChatResponse, Provider};
use serde_json::json;
use std::sync::Arc;

struct NativeScriptedProvider {
    inner: Box<dyn Provider>,
    primary_tool: &'static str,
}
#[async_trait::async_trait]
impl Provider for NativeScriptedProvider {
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_system(system, message, model, temperature).await
    }
    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> Result<ChatResponse> {
        anyhow::ensure!(model == "prx-local-model", "pinned original chat model");
        anyhow::ensure!(
            request
                .tools
                .is_some_and(|tools| tools.iter().any(|tool| tool.name == self.primary_tool)),
            "provider receives actually admitted tool schema"
        );
        self.inner.chat(request, model, temperature).await
    }
}

// Box only the integration fixture future; production and performance fixtures are unchanged.
pub fn run<T, F, Fut>(
    provider: Box<dyn Provider>,
    primary_tool: &'static str,
    case: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<T>>>>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    Box::pin(run_mode(provider, primary_tool, None, case))
}

// This entry is used only by the max-depth positive integration case.
#[allow(dead_code)]
pub fn run_delegate<T, F, Fut>(
    provider: Box<dyn Provider>,
    agents: std::collections::HashMap<String, openprx::config::DelegateAgentConfig>,
    case: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<T>>>>
where
    F: FnOnce(AgentBuilder, tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    Box::pin(run_mode(provider, "delegate", Some(agents), case))
}

async fn run_mode<T, F, Fut>(
    provider: Box<dyn Provider>,
    primary_tool: &'static str,
    agents: Option<std::collections::HashMap<String, openprx::config::DelegateAgentConfig>>,
    case: F,
) -> Result<T>
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
    if let Some(agents) = agents {
        config.agents = agents;
    }
    config.memory.auto_save = false;
    config.autonomy.level = openprx::security::AutonomyLevel::Full;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.tool_tiering.always_include = vec![primary_tool.to_owned()];
    // Only this positive fixture publishes an actual structured tool provider.
    config.providers.models.push(serde_json::from_value(json!({
        "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":true
    }))?);
    let memory = Arc::new(SqliteMemory::new(workspace.path())?);
    let builder = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(NativeScriptedProvider {
            inner: provider,
            primary_tool,
        }))
        .tools(vec![])
        .memory(memory)
        .observer(Arc::new(openprx::observability::NoopObserver))
        .tool_dispatcher(Box::new(NativeToolDispatcher));
    Ok(case(builder, workspace).await)
}

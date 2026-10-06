//! Source-bound public Agent fixtures shared by the integration suites.
use openprx::agent::agent::{Agent, AgentBuilder};
use openprx::agent::dispatcher::ToolDispatcher;
use openprx::config::{Config, schema::ModelPricing};
use openprx::memory::SqliteMemory;
use openprx::observability::NoopObserver;
use openprx::providers::Provider;
use openprx::tools::Tool;
use std::sync::Arc;

pub struct EmbeddedAgentFixture {
    agent: Agent,
    // Drop the Agent and its memory pool before removing the workspace.
    _workspace: tempfile::TempDir,
}
impl EmbeddedAgentFixture {
    // Keep the integration harness stack small without changing production
    // Agent execution or the performance fixtures' allocation measurements.
    pub fn turn<'a>(
        &'a mut self,
        message: &'a str,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<String>> + 'a>> {
        Box::pin(self.agent.turn(message))
    }

    pub fn finish(builder: AgentBuilder, workspace: tempfile::TempDir) -> Self {
        Self {
            agent: builder.build().unwrap(),
            _workspace: workspace,
        }
    }
}
impl std::ops::Deref for EmbeddedAgentFixture {
    type Target = Agent;
    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}
impl std::ops::DerefMut for EmbeddedAgentFixture {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.agent
    }
}

pub fn prepare(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    dispatcher: Box<dyn ToolDispatcher>,
) -> (AgentBuilder, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = Config {
        workspace_dir: workspace.path().to_owned(),
        config_path: workspace.path().join("config.toml"),
        default_model: Some("prx-local-model".to_owned()),
        ..Config::default()
    };
    config.causal_tree.enabled = false;
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
    let memory = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
    let builder = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(provider)
        .tools(tools)
        .memory(memory)
        .observer(Arc::new(NoopObserver {}))
        .tool_dispatcher(dispatcher);
    (builder, workspace)
}

//! Test support for CRON-ORIGIN: a loopback OpenAI-compatible model that makes
//! one scripted `memory_store` call, and a helper that drives a tool through the
//! production [`crate::tools::ToolExecutionService`] as a given surface.
//!
//! Scheduled agent runs build their provider from configuration
//! (`run_with_runtime_envelope`), so the model has to be reachable over HTTP.
//! It never leaves `127.0.0.1`, and every credential is a sentinel.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::config::Config;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::{Tool, ToolExecutionContext, ToolResult};

/// The text a `memory_store` tool result starts with on success.
const STORED_PREFIX: &str = "Stored memory";

#[derive(Clone)]
struct ModelState {
    tool: String,
    arguments: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

/// A loopback chat-completions endpoint whose first answer in a turn is a
/// native `memory_store` call and whose answer after the tool result is text.
pub(crate) struct ScriptedStoreModel {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl ScriptedStoreModel {
    pub(crate) async fn start(key: &str, content: &str) -> Self {
        Self::start_calling(
            "memory_store",
            &json!({"key": key, "content": content, "category": "core"}),
        )
        .await
    }

    /// A model whose first answer is one native call of `tool` with
    /// `arguments`, and whose every later answer is text.
    pub(crate) async fn start_calling(tool: &str, arguments: &Value) -> Self {
        async fn complete(State(state): State<ModelState>, Json(body): Json<Value>) -> Json<Value> {
            let already_called = {
                let mut requests = state.requests.lock();
                let already_called = !requests.is_empty();
                requests.push(body.clone());
                already_called
            };
            let answered = already_called
                || body.get("messages").and_then(Value::as_array).is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message.get("role").and_then(Value::as_str) == Some("tool")
                            || message
                                .get("content")
                                .and_then(Value::as_str)
                                .is_some_and(|content| content.contains(STORED_PREFIX))
                    })
                });
            if answered {
                return Json(json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "done"},
                        "finish_reason": "stop"
                    }]
                }));
            }
            Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_cron_origin_store",
                            "type": "function",
                            "function": {"name": state.tool, "arguments": state.arguments}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }))
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ModelState {
            tool: tool.to_string(),
            arguments: arguments.to_string(),
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(complete))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind the loopback model");
        let addr = listener.local_addr().expect("test: loopback model address");
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::warn!("test: loopback model stopped: {error}");
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            server,
        }
    }

    /// Whether some request the model received carried a successful
    /// `memory_store` result, i.e. the scheduled run's store really executed.
    pub(crate) fn saw_a_successful_store(&self) -> bool {
        self.requests
            .lock()
            .iter()
            .any(|body| body.to_string().contains(STORED_PREFIX))
    }

    /// Every request body the model received, rendered as JSON text.
    pub(crate) fn transcript(&self) -> String {
        self.requests
            .lock()
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A configuration whose provider is this model, rooted at `root`.
    pub(crate) fn config(&self, root: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = root.join("workspace");
        config.config_path = root.join("config.toml");
        let provider = format!("custom:{}", self.base_url);
        config.default_provider = Some(provider.clone());
        config.default_model = Some("cron-origin-model".to_string());
        config
            .providers
            .models
            .push(crate::config::schema::ProviderModelConfig {
                provider,
                model: Some("cron-origin-model".to_string()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(true),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            });
        config.api_key = Some("CRON-ORIGIN-TEST-SENTINEL".to_string());
        config.api_url = None;
        config.model_routes = Vec::new();
        config.reliability.fallback_providers = Vec::new();
        config.reliability.api_keys = Vec::new();
        config.reliability.provider_retries = 0;
        config.reliability.provider_backoff_ms = 0;
        config.reliability.scheduler_retries = 0;
        config.auth.codex_auth_json_auto_import = false;
        config.auth.codex_auth_json_path = root.join("no-codex-auth.json");
        std::fs::create_dir_all(&config.workspace_dir).expect("test: create the workspace");
        config
    }
}

impl Drop for ScriptedStoreModel {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A turn context for `surface`, shaped the way that surface's entry point
/// builds it.
pub(crate) fn surface_context(config: &Config, surface: Surface) -> ToolExecutionContext {
    let workspace = config.workspace_dir.to_string_lossy().to_string();
    let (channel, sender, chat_type, chat_id) = match surface {
        Surface::Webhook => ("webhook", "client-a", "gateway", "gateway:webhook:bearer:0f0f:client-a"),
        Surface::AgentCli => ("cli", "agent", "private", "agent:local"),
        Surface::Terminal => ("terminal", "user", "private", "terminal:user"),
    };
    let mut envelope = RuntimeEnvelope::chat_terminal(workspace, chat_id, crate::memory::MemoryVisibility::Workspace)
        .with_sender(sender)
        .with_channel(channel);
    if matches!(surface, Surface::Webhook) {
        envelope = envelope.with_owner_id(WEBHOOK_OWNER);
    }
    ToolExecutionContext::new(envelope, chat_type).with_chat_id(chat_id)
}

/// The authenticated owner the webhook surface carries in these tests.
pub(crate) const WEBHOOK_OWNER: &str = "owner:cron-origin:webhook:bearer:0f0f:client-a";

#[derive(Debug, Clone, Copy)]
pub(crate) enum Surface {
    /// A `/webhook` turn: a network surface.
    Webhook,
    /// A `prx agent` CLI turn: the local-process operator.
    AgentCli,
    /// A terminal UI turn: the local operator.
    Terminal,
}

/// Execute `tool` once through the production execution service as `context`.
pub(crate) async fn execute_as(tool: Box<dyn Tool>, context: ToolExecutionContext, args: Value) -> ToolResult {
    let name = tool.name().to_string();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![tool]);
    let policy = Arc::new(crate::security::SecurityPolicy::default());
    let service = crate::tools::ToolExecutionService::from_shared_boxed_registry(
        registry,
        Arc::new(crate::tools::SecurityEffectPolicy::new(policy)),
        Arc::new(crate::tools::DenyApprovalStrategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    );
    let outcome = service
        .execute(crate::tools::ToolExecutionCommand::new(name, args), context, None)
        .await;
    outcome.result.expect("test: the tool ran and returned a result")
}

/// Whether the workspace's shared prompt material carries `text`: the
/// `MEMORY.md` mirror and the identity section every surface's system prompt
/// inlines.
pub(crate) fn shared_prompt_contains(config: &Config, text: &str) -> bool {
    let mirror = std::fs::read_to_string(config.workspace_dir.join("MEMORY.md")).unwrap_or_default();
    mirror.contains(text) || crate::agent::prompt::build_identity_prompt(&config.workspace_dir).contains(text)
}

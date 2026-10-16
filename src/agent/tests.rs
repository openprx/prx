//! Comprehensive agent-loop test suite.
//!
//! Tests exercise the full `Agent.turn()` cycle with mock providers and tools,
//! covering every edge case an agentic tool loop must handle:
//!
//!   1. Simple text response (no tools)
//!   2. Single tool call → final response
//!   3. Multi-step tool chain (tool A → tool B → response)
//!   4. Max-iteration bailout
//!   5. Unknown tool name recovery
//!   6. Tool execution failure recovery
//!   7. Parallel tool dispatch
//!   8. History trimming during long conversations
//!   9. Memory auto-save round-trip
//!  10. Native vs XML dispatcher integration
//!  11. Empty / whitespace-only LLM responses
//!  12. Mixed text + tool call responses
//!  13. Multi-tool batch in a single response
//!  14. System prompt generation & tool instructions
//!  15. Context enrichment from memory loader
//!  16. ConversationMessage serialization round-trip
//!  17. Tool call with stringified JSON arguments
//!  18. Conversation history fidelity (tool call → tool result → assistant)
//!  19. Builder validation (missing required fields)
//!  20. Idempotent system prompt insertion

#![cfg(test)]
// Test module: JSON indexing and vector indexing in assertions are intentional.
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use crate::agent::agent::Agent;
use crate::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher, XmlToolDispatcher};
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::config::{AgentConfig, MemoryConfig, TaskRoutingConfig, TaskRoutingIntentConfig, TaskRoutingRule};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{NoopObserver, Observer};
use crate::providers::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilities, ToolCall,
    ToolResultMessage,
};
use crate::tools::{SessionsSpawnTool, Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════════════
// Test Helpers — Mock Provider, Mock Tool, Mock Memory
// ═══════════════════════════════════════════════════════════════════════════

/// A mock LLM provider that returns pre-scripted responses in order.
/// When the queue is exhausted it returns a simple "done" text response.
struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// Records every request for assertion.
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> Result<ChatResponse> {
        self.requests.lock().unwrap().push(request.messages.to_vec());

        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
                reasoning_content: None,
            });
        }
        Ok(guard.remove(0))
    }
}

/// A native-tool model used by the public embedded entrypoint proofs.
/// Requests are retained so the test can prove the tool result returned to the
/// same model turn rather than only observing a backend side effect.
struct NativeScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl NativeScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> (Self, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(responses),
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

#[async_trait]
impl Provider for NativeScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        anyhow::bail!("entrypoint fixture must use the shared native tool loop")
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> Result<ChatResponse> {
        self.requests.lock().unwrap().push(request.messages.to_vec());
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Ok(text_response("follow-up complete"));
        }
        Ok(responses.remove(0))
    }
}

struct SilentEntryChannel;

#[async_trait]
impl Channel for SilentEntryChannel {
    fn name(&self) -> &str {
        "agent-entry"
    }

    async fn send(&self, _message: &SendMessage) -> Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        Ok(())
    }
}

/// A mock provider that always returns an error.
struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        anyhow::bail!("provider error")
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> Result<ChatResponse> {
        anyhow::bail!("provider error")
    }
}

/// A simple echo tool that returns its arguments as output.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string();
        Ok(ToolResult {
            success: true,
            output: msg,
            error: None,
        })
    }
}

/// A tool that always fails execution.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some("intentional failure".into()),
        })
    }
}

/// A tool that panics (tests error propagation).
struct PanickingTool;

#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &str {
        "panicker"
    }

    fn description(&self) -> &str {
        "Panics on execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        anyhow::bail!("catastrophic tool failure")
    }
}

/// A tool that tracks how many times it was called.
struct CountingTool {
    count: Arc<Mutex<usize>>,
}

impl CountingTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let count = Arc::new(Mutex::new(0));
        (Self { count: count.clone() }, count)
    }
}

struct MockSessionsSpawnTool {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Tool for MockSessionsSpawnTool {
    fn name(&self) -> &str {
        "sessions_spawn"
    }

    fn description(&self) -> &str {
        "Delegates tasks"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        self.calls.lock().unwrap().push(args);
        Ok(ToolResult {
            success: true,
            output: "Sub-agent spawned in process mode (run_id: run-123). Will announce result when complete.".into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }

    fn description(&self) -> &str {
        "Counts calls"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        Ok(ToolResult {
            success: true,
            output: format!("call #{}", *c),
            error: None,
        })
    }
}

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, &std::env::temp_dir(), None).unwrap())
}

fn make_sqlite_memory() -> (Arc<dyn Memory>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = MemoryConfig {
        backend: "sqlite".into(),
        ..MemoryConfig::default()
    };
    let mem = Arc::from(memory::create_memory(&cfg, tmp.path(), None).unwrap());
    (mem, tmp)
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

/// Own the workspace for at least as long as the Agent and its SQLite pool.
struct TestAgent {
    agent: Agent,
    workspace: tempfile::TempDir,
}
impl std::ops::Deref for TestAgent {
    type Target = Agent;
    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}
impl std::ops::DerefMut for TestAgent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.agent
    }
}

fn fixture_runtime_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut runtime = crate::config::Config::default();
    runtime.workspace_dir = workspace.to_owned();
    runtime.config_path = workspace.join("config.toml");
    runtime.default_model = Some("prx-local-model".to_owned());
    runtime.causal_tree.enabled = false;
    runtime.memory.auto_save = false;
    runtime.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    runtime.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    runtime
        .providers
        .models
        .push(crate::config::schema::ProviderModelConfig {
            provider: "ollama".to_owned(),
            model: Some("prx-local-model".to_owned()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    runtime
}

fn default_causal_runtime_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut runtime = fixture_runtime_config(workspace);
    runtime.causal_tree = crate::causal_tree::CausalTreeConfig::default();
    runtime.autonomy.level = crate::security::AutonomyLevel::Full;
    runtime
}

fn build_agent_with(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    dispatcher: Box<dyn ToolDispatcher>,
) -> TestAgent {
    let (memory, workspace) = make_sqlite_memory();
    let agent = Agent::builder()
        .runtime_config(fixture_runtime_config(workspace.path()), "ollama")
        .provider(provider)
        .tools(tools)
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(dispatcher)
        .build()
        .unwrap();
    TestAgent { agent, workspace }
}

fn build_agent_with_memory(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    mem: Arc<dyn Memory>,
    auto_save: bool,
    workspace: &std::path::Path,
) -> Agent {
    let runtime = fixture_runtime_config(workspace);
    Agent::builder()
        .runtime_config(runtime, "ollama")
        .provider(provider)
        .tools(tools)
        .memory(mem)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(workspace.to_owned())
        .auto_save(auto_save)
        .build()
        .unwrap()
}

fn build_agent_with_config(provider: Box<dyn Provider>, tools: Vec<Box<dyn Tool>>, config: AgentConfig) -> TestAgent {
    let (memory, workspace) = make_sqlite_memory();
    let agent = Agent::builder()
        .runtime_config(fixture_runtime_config(workspace.path()), "ollama")
        .provider(provider)
        .tools(tools)
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .config(config)
        .build()
        .unwrap();
    TestAgent { agent, workspace }
}

/// Helper: create a ChatResponse with tool calls (native format).
fn tool_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        text: Some(String::new()),
        tool_calls: calls,
        reasoning_content: None,
    }
}

/// Helper: create a plain text ChatResponse.
fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: Some(text.into()),
        tool_calls: vec![],
        reasoning_content: None,
    }
}

/// Helper: create an XML-style tool call response.
fn xml_tool_response(name: &str, args: &str) -> ChatResponse {
    ChatResponse {
        text: Some(format!(
            "<tool_call>\n{{\"name\": \"{name}\", \"arguments\": {args}}}\n</tool_call>"
        )),
        tool_calls: vec![],
        reasoning_content: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Simple text response (no tools)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_returns_text_when_no_tools_called() {
    // Q45: Text return and genuine public API source recording remain intact.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![text_response("Hello world")]));
        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "Hello world");
        // The public API must persist its own source, not borrow CLI authority.
        let conn = rusqlite::Connection::open_with_flags(
            agent.workspace.path().join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_events WHERE role = 'user' AND event_type = 'message.created'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
        let source: (String, String, String, String, String, String) = conn.query_row(
            "SELECT source, channel, sender, content, session_key, owner_id FROM message_events WHERE role = 'user' AND event_type = 'message.created'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).unwrap();
        assert_eq!(
            (&*source.0, &*source.1, &*source.2, &*source.3),
            ("agent", "agent_api", "embedded-caller", "hi")
        );
        assert!(source.4.starts_with("agent_api:"));
        assert!(!source.5.is_empty(), "expected a non-empty value");
    })
    .await;
}

/// A public embedded Agent turn must admit an explicit Delegate request under
/// the default causal configuration, let the model call the real tool, run the
/// configured child provider, and settle child cleanup before another turn.
#[tokio::test]
async fn default_causal_embedded_turn_runs_delegate_child_and_confirms_cleanup() -> Result<()> {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let (memory, workspace) = make_sqlite_memory();
    let child_probe = CredentialProbe::start_replying("delegate child complete").await;
    let child_provider = child_probe.custom_provider();
    let mut config = default_causal_runtime_config(workspace.path());
    assert_eq!(config.causal_tree, crate::causal_tree::CausalTreeConfig::default());
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = workspace.path().join("no-codex-auth.json");
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider": child_provider,
        "model": PROBE_MODEL,
        "operations": ["chat"],
        "native_tools": false
    }))?);
    config.cost.prices.insert(
        format!("{child_provider}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let agents = HashMap::from([(
        "worker".to_owned(),
        crate::config::DelegateAgentConfig {
            provider: child_provider,
            model: PROBE_MODEL.to_owned(),
            system_prompt: None,
            api_key: None,
            credential_ref: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        },
    )]);
    config.agents = agents.clone();
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        workspace.path(),
    ));
    let delegate = crate::tools::DelegateTool::new(agents, Arc::clone(&security))
        .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(&config))
        .with_model_catalog(crate::providers::selection::catalog_for(&config))
        .with_hint_config(&config)
        .with_reliability(config.reliability.clone())
        .with_multimodal_config(config.multimodal.clone())
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            config.agent.compaction.clone(),
            config.router.clone(),
            config.model_routes.clone(),
        ))
        .with_shared_memory(Arc::clone(&memory))
        .with_event_recording(config.memory.event_recording_config());
    let (parent, parent_requests) = NativeScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "delegate-call".to_owned(),
            name: "delegate".to_owned(),
            arguments: serde_json::json!({"agent":"worker", "prompt":"inspect the child request"}).to_string(),
        }]),
        text_response("delegate parent complete"),
    ]);
    let mut agent = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(parent))
        .tools(vec![Box::new(delegate)])
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .security_policy(security)
        .build()?;

    let reply = agent
        .turn("use delegate with worker to inspect the child request")
        .await?;
    assert_eq!(reply, "delegate parent complete");
    assert_eq!(
        child_probe
            .requests()
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .count(),
        1,
        "the configured Delegate child must make one real provider request"
    );
    assert!(
        parent_requests.lock().unwrap().iter().any(|messages| messages
            .iter()
            .any(|message| message.role == "tool" && message.content.contains("delegate child complete"))),
        "the parent model must receive the real Delegate child result"
    );
    assert_eq!(
        agent.turn("hello after delegate cleanup").await?,
        "follow-up complete",
        "a follow-up turn proves the prior entrypoint cleanup acknowledgement settled"
    );
    Ok(())
}

/// The public embedded entrypoint must publish `sessions_spawn` only through
/// the admitted default-causal turn, execute it without a forged approval, run
/// the sealed model-only child, and observe its natural cleanup.
#[tokio::test]
async fn default_causal_embedded_turn_runs_sealed_spawn_child_and_confirms_cleanup() -> Result<()> {
    use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};

    let (memory, workspace) = make_sqlite_memory();
    let child_probe = CredentialProbe::start_replying("spawn child complete").await;
    let provider_name = child_probe.custom_provider();
    let mut config = default_causal_runtime_config(workspace.path());
    assert_eq!(config.causal_tree, crate::causal_tree::CausalTreeConfig::default());
    config.default_provider = Some(provider_name.clone());
    config.default_model = Some(PROBE_MODEL.to_owned());
    config.reliability.provider_retries = 0;
    config.auth.codex_auth_json_auto_import = false;
    config.auth.codex_auth_json_path = workspace.path().join("no-codex-auth.json");
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider": provider_name,
        "model": PROBE_MODEL,
        "operations": ["chat"],
        "native_tools": true
    }))?);
    config.cost.prices.insert(
        format!("{provider_name}/{PROBE_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        workspace.path(),
    ));
    let model_catalog = crate::providers::selection::catalog_for(&config);
    let resolved = crate::providers::selection::check(
        &config,
        &provider_name,
        PROBE_MODEL,
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    let child_provider: Arc<dyn Provider> = Arc::from(crate::providers::create_provider_for(
        &resolved,
        &model_catalog,
        &config.reliability,
        &config.cost,
        &crate::providers::provider_runtime_options_from_config(&config),
    )?);
    let spawn = SessionsSpawnTool::new(
        Arc::new(SilentEntryChannel),
        child_provider,
        &provider_name,
        PROBE_MODEL,
        0.0,
        Arc::clone(&security),
        workspace.path().to_path_buf(),
        config.multimodal.clone(),
        config.agent.compaction.clone(),
        HashMap::new(),
        None,
        crate::providers::provider_runtime_options_from_config(&config),
        config.sessions_spawn.clone(),
    )
    .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(&config))
    .with_model_catalog(model_catalog)
    .with_hint_config(&config)
    .with_shared_memory(Arc::clone(&memory))
    .with_event_recording(config.memory.event_recording_config())
    .with_process_recovery(config.causal_tree.enabled)?;
    let runs = spawn.active_runs_arc();
    let child_registry = Arc::new(Vec::<Box<dyn Tool>>::new());
    assert!(spawn.tools_handle().set(&child_registry));
    let (parent, parent_requests) = NativeScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "spawn-call".to_owned(),
            name: "sessions_spawn".to_owned(),
            arguments: serde_json::json!({
                "action":"spawn",
                "task":"complete the sealed child task",
                "mode":"task"
            })
            .to_string(),
        }]),
        text_response("spawn parent complete"),
    ]);
    let mut agent = Agent::builder()
        .runtime_config(config, provider_name)
        .provider(Box::new(parent))
        .tools(vec![Box::new(spawn)])
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .security_policy(security)
        .build()?;

    let reply = agent.turn("use sessions_spawn for the sealed child task").await?;
    assert_eq!(reply, "spawn parent complete");
    assert!(
        parent_requests.lock().unwrap().iter().any(|messages| messages
            .iter()
            .any(|message| message.role == "tool" && message.content.contains("Sub-agent spawned"))),
        "the parent model must receive the real sessions_spawn result"
    );
    let settlement = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let settled = runs.read().await.first().is_some_and(|run| {
                matches!(run.status, crate::tools::sessions_spawn::SubAgentStatus::Completed(_))
                    && run.task_control.as_ref().and_then(|control| control.finalization())
                        == Some(crate::tools::sessions_spawn::TaskFinalization::Natural)
            });
            if settled {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if settlement.is_err() {
        let snapshot = runs.read().await;
        panic!(
            "the independent sessions_spawn owner did not confirm natural child cleanup: requests={:?} runs={snapshot:?}",
            child_probe.requests()
        );
    }
    assert_eq!(
        child_probe
            .requests()
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .count(),
        1,
        "the spawned child must make exactly one real provider request"
    );
    let runs = runs.read().await;
    let child = runs
        .first()
        .expect("the real sessions_spawn tool must register its child");
    assert!(
        matches!(&child.status, crate::tools::sessions_spawn::SubAgentStatus::Completed(text) if text == "spawn child complete"),
        "the registered child must reach its natural terminal result: {:?}",
        child.status
    );
    assert_eq!(
        child.task_control.as_ref().and_then(|control| control.finalization()),
        Some(crate::tools::sessions_spawn::TaskFinalization::Natural),
        "the independent child owner must confirm natural cleanup"
    );
    drop(runs);
    assert_eq!(
        agent.turn("hello after spawn cleanup").await?,
        "follow-up complete",
        "a follow-up turn proves the prior entrypoint cleanup acknowledgement settled"
    );
    assert!(
        child_registry.is_empty(),
        "the sealed child registry remains the admitted strict subset"
    );
    Ok(())
}

#[tokio::test]
async fn turn_delegates_heavy_task_to_sessions_spawn_tool() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = Box::new(FailingProvider);
    let (memory, workspace) = make_sqlite_memory();
    let mut config = fixture_runtime_config(workspace.path());
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    let mut agent = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(provider)
        .tools(vec![Box::new(MockSessionsSpawnTool { calls: calls.clone() })])
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .task_routing_config(TaskRoutingConfig {
            default_intent: TaskRoutingIntentConfig::Simple,
            rules: vec![TaskRoutingRule {
                keywords: vec!["fix".into(), "deploy".into()],
                intent: TaskRoutingIntentConfig::Delegate,
                model_hint: None,
                sub_agent_model: Some("claude-opus-4-6".into()),
                priority: 10,
            }],
        })
        .build()
        .unwrap();

    let response = agent.turn("please fix this bug for me").await.unwrap();

    assert!(
        response.contains("task run-123"),
        "configured delegation response: {response:?}; history: {:?}",
        agent.history()
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["action"], "spawn");
    assert_eq!(calls[0]["mode"], "process");
    assert_eq!(calls[0]["model"], "claude-opus-4-6");
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Single tool call → final response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_executes_single_tool_then_returns() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello from tool"}"#.into(),
        }]),
        text_response("I ran the tool"),
    ]));

    let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

    let response = agent.turn("run echo").await.unwrap();
    assert!(!response.is_empty(), "Expected non-empty response after tool execution");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Multi-step tool chain (tool A → tool B → response)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multi_step_tool_chain() {
    // Q45: Sequential tool rounds execute and return their original final text.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let (counting_tool, count) = CountingTool::new();

        let provider = Box::new(ScriptedProvider::new(vec![
            tool_response(vec![ToolCall {
                id: "tc1".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            }]),
            tool_response(vec![ToolCall {
                id: "tc2".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            }]),
            tool_response(vec![ToolCall {
                id: "tc3".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            }]),
            text_response("Done after 3 calls"),
        ]));

        let (memory, workspace) = make_sqlite_memory();
        let mut config = fixture_runtime_config(workspace.path());
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.tool_tiering.always_include.push("counter".to_owned());
        let agent = Agent::builder()
            .runtime_config(config, "ollama")
            .provider(provider)
            .tools(vec![Box::new(counting_tool)])
            .memory(memory)
            .observer(make_observer())
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .build()
            .unwrap();
        let mut agent = TestAgent { agent, workspace };

        let response = agent.turn("count 3 times").await.unwrap();
        assert!(
            !response.is_empty(),
            "Expected non-empty response after multi-step chain"
        );
        assert_eq!(*count.lock().unwrap(), 3, "multi-step history: {:?}", agent.history());
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Unknown tool name recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_unknown_tool_gracefully() {
    // Q52: evidence prerequisite only; preserve unknown tool rejection and diagnostic recovery.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            tool_response(vec![ToolCall {
                id: "tc1".into(),
                name: "nonexistent_tool".into(),
                arguments: "{}".into(),
            }]),
            text_response("I couldn't find that tool"),
        ]));

        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        let response = agent.turn("use nonexistent").await.unwrap();
        assert!(
            !response.is_empty(),
            "Expected non-empty response after unknown tool recovery"
        );

        // Verify the tool result mentioned "unknown tool"
        let has_tool_result = agent.history().iter().any(|msg| match msg {
            ConversationMessage::ToolResults(results) => results.iter().any(|r| r.content.contains("unknown tool")),
            _ => false,
        });
        assert!(has_tool_result, "Expected tool result with 'unknown tool' message");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Tool execution failure recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_recovers_from_tool_failure() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "fail".into(),
            arguments: "{}".into(),
        }]),
        text_response("Tool failed but I recovered"),
    ]));

    let mut agent = build_agent_with(provider, vec![Box::new(FailingTool)], Box::new(NativeToolDispatcher));

    let response = agent.turn("try failing tool").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool failure recovery"
    );
}

#[tokio::test]
async fn turn_recovers_from_tool_error() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "panicker".into(),
            arguments: "{}".into(),
        }]),
        text_response("I recovered from the error"),
    ]));

    let mut agent = build_agent_with(provider, vec![Box::new(PanickingTool)], Box::new(NativeToolDispatcher));

    let response = agent.turn("try panicking").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool error recovery"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Provider error propagation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_propagates_provider_error() {
    // Q52: evidence prerequisite only; preserve provider error propagation.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let mut agent = build_agent_with(Box::new(FailingProvider), vec![], Box::new(NativeToolDispatcher));

        let result = agent.turn("hello").await;
        assert!(result.is_err(), "Expected provider error to propagate");

        assert!(
            result
                .as_ref()
                .expect_err("original provider error")
                .chain()
                .any(|cause| cause.to_string() == "provider error"),
            "Q52 must reach the original FailingProvider boundary"
        );
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. History trimming during long conversations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_is_not_trimmed_by_an_arbitrary_message_count() {
    // Q45: Conversation retention is governed by token budget rather than message count.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let turn_count = 11;
        let mut responses = vec![];
        for _ in 0..turn_count {
            responses.push(text_response("ok"));
        }

        let provider = Box::new(ScriptedProvider::new(responses));
        let mut agent = build_agent_with_config(provider, vec![], AgentConfig::default());

        for i in 0..turn_count {
            let _ = agent.turn(&format!("msg {i}")).await.unwrap();
        }

        assert_eq!(agent.history().len(), turn_count * 2 + 1);

        // System prompt should always be preserved
        let first = &agent.history()[0];
        assert!(matches!(first, ConversationMessage::Chat(c) if c.role == "system"));
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Memory auto-save round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_save_stores_only_user_messages_in_memory() {
    let (mem, workspace) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![text_response("I remember everything")]));

    let mut agent = build_agent_with_memory(
        provider,
        vec![],
        mem.clone(),
        true, // auto_save enabled
        workspace.path(),
    );

    let user_message = "Remember this deployment fact for the next maintenance window";
    let _ = agent.turn(user_message).await.unwrap();

    // Auto-save only persists user-stated input, never assistant-generated summaries.
    let count = mem.list(Some(&MemoryCategory::Conversation), None).await.unwrap().len();
    assert_eq!(count, 1, "Expected exactly 1 user memory entry, got {count}");

    let entries = mem.list(Some(&MemoryCategory::Conversation), None).await.unwrap();
    assert!(entries[0].key.starts_with("user_msg:"));
    assert_eq!(
        entries[0].content, user_message,
        "Stored memory must match original user input"
    );
    assert!(entries[0].session_id.as_deref().unwrap().starts_with("agent_api:"));

    let assistant = mem.get("assistant_resp").await.unwrap();
    assert!(assistant.is_none(), "assistant_resp should not be auto-saved anymore");
}

#[tokio::test]
async fn auto_save_disabled_does_not_store() {
    let (mem, workspace) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![text_response("hello")]));

    let mut agent = build_agent_with_memory(
        provider,
        vec![],
        mem.clone(),
        false, // auto_save disabled
        workspace.path(),
    );

    let _ = agent
        .turn("Remember this deployment fact for the next maintenance window")
        .await
        .unwrap();
    let entries = mem.list(Some(&MemoryCategory::Conversation), None).await.unwrap();
    assert!(
        entries.is_empty(),
        "No semantic user memory may be saved when auto_save is off"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. Native vs XML dispatcher integration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn xml_dispatcher_parses_and_loops() {
    let provider = Box::new(ScriptedProvider::new(vec![
        xml_tool_response("echo", r#"{"message": "xml-test"}"#),
        text_response("XML tool completed"),
    ]));

    let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(XmlToolDispatcher));

    let response = agent.turn("test xml").await.unwrap();
    assert!(!response.is_empty(), "Expected non-empty response from XML dispatcher");
}

#[tokio::test]
async fn native_dispatcher_completes_a_text_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

    let response = agent.turn("hi").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response from native dispatcher"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. Empty / whitespace-only LLM responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_empty_text_response() {
    // Q89 supersedes the Ok(empty) oracle: one corrective retry, then the exact error.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![],
                reasoning_content: None,
            },
            ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![],
                reasoning_content: None,
            },
        ]));

        let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

        let error = agent
            .turn("hi")
            .await
            .expect_err("two empty responses must fail closed");
        assert_eq!(
            error.to_string(),
            "model returned an empty response after one corrective retry"
        );
    })
    .await;
}

#[tokio::test]
async fn turn_handles_none_text_response() {
    // Q89 supersedes the Ok(empty) oracle: one corrective retry, then the exact error.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            ChatResponse {
                text: None,
                tool_calls: vec![],
                reasoning_content: None,
            },
            ChatResponse {
                text: None,
                tool_calls: vec![],
                reasoning_content: None,
            },
        ]));

        let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

        let error = agent
            .turn("hi")
            .await
            .expect_err("two empty responses must fail closed");
        assert_eq!(
            error.to_string(),
            "model returned an empty response after one corrective retry"
        );
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Mixed text + tool call responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_preserves_text_alongside_tool_calls() {
    // Q45: Intermediate text accompanying tool calls remains in history.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            ChatResponse {
                text: Some("Let me check...".into()),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "echo".into(),
                    arguments: r#"{"message": "hi"}"#.into(),
                }],
                reasoning_content: None,
            },
            text_response("Here are the results"),
        ]));

        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        let response = agent.turn("check something").await.unwrap();
        assert!(
            !response.is_empty(),
            "Expected non-empty final response after mixed text+tool"
        );

        // The intermediate text should be in history
        let preserves_intermediate = |msg: &ConversationMessage| match msg {
            ConversationMessage::Chat(c) => c.role == "assistant" && c.content.contains("Let me check"),
            ConversationMessage::AssistantToolCalls { text, tool_calls } => {
                text.as_deref() == Some("Let me check...")
                    && tool_calls.len() == 1
                    && tool_calls.first().is_some_and(|call| {
                        call.id == "tc1" && call.name == "echo" && call.arguments == r#"{"message": "hi"}"#
                    })
            }
            _ => false,
        };
        // Q92: Exercise the same history predicate without provider or runtime work.
        let typed_history =
            |text: &str, id: &str, name: &str, arguments: &str| ConversationMessage::AssistantToolCalls {
                text: Some(text.to_owned()),
                tool_calls: vec![ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: arguments.to_owned(),
                }],
            };
        let original_arguments = r#"{"message": "hi"}"#;
        assert!(preserves_intermediate(&typed_history(
            "Let me check...",
            "tc1",
            "echo",
            original_arguments,
        )));
        for (text, id, name, arguments) in [
            ("different text", "tc1", "echo", original_arguments),
            ("Let me check...", "different_id", "echo", original_arguments),
            ("Let me check...", "tc1", "different_tool", original_arguments),
            ("Let me check...", "tc1", "echo", r#"{"message": "different"}"#),
        ] {
            assert!(!preserves_intermediate(&typed_history(text, id, name, arguments)));
        }
        let has_intermediate = agent.history().iter().any(preserves_intermediate);
        assert!(has_intermediate, "Intermediate text should be in history");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. Multi-tool batch in a single response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multiple_tools_in_one_response() {
    // Q45: Multiple calls in one response each execute once.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let (counting_tool, count) = CountingTool::new();

        let provider = Box::new(ScriptedProvider::new(vec![
            tool_response(vec![
                ToolCall {
                    id: "tc1".into(),
                    name: "counter".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "tc2".into(),
                    name: "counter".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "tc3".into(),
                    name: "counter".into(),
                    arguments: "{}".into(),
                },
            ]),
            text_response("All 3 done"),
        ]));

        let (memory, workspace) = make_sqlite_memory();
        let mut config = fixture_runtime_config(workspace.path());
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.tool_tiering.always_include.push("counter".to_owned());
        let agent = Agent::builder()
            .runtime_config(config, "ollama")
            .provider(provider)
            .tools(vec![Box::new(counting_tool)])
            .memory(memory)
            .observer(make_observer())
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .build()
            .unwrap();
        let mut agent = TestAgent { agent, workspace };

        let response = agent.turn("batch").await.unwrap();
        assert!(
            !response.is_empty(),
            "Expected non-empty response after multi-tool batch"
        );
        assert_eq!(
            *count.lock().unwrap(),
            3,
            "All 3 tools should have been called; history: {:?}",
            agent.history()
        );
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. System prompt generation & tool instructions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn system_prompt_injected_on_first_turn() {
    // Q45: First turn inserts one system prompt.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        assert!(agent.history().is_empty(), "History should start empty");

        let _ = agent.turn("hi").await.unwrap();

        // First message should be the system prompt
        let first = &agent.history()[0];
        assert!(
            matches!(first, ConversationMessage::Chat(c) if c.role == "system"),
            "First history entry should be system prompt"
        );
    })
    .await;
}

#[tokio::test]
async fn system_prompt_not_duplicated_on_second_turn() {
    // Q45: Second turn preserves one system prompt.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            text_response("first"),
            text_response("second"),
        ]));
        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        let _ = agent.turn("hi").await.unwrap();
        let _ = agent.turn("hello again").await.unwrap();

        let system_count = agent
            .history()
            .iter()
            .filter(|msg| matches!(msg, ConversationMessage::Chat(c) if c.role == "system"))
            .count();
        assert_eq!(system_count, 1, "System prompt should appear exactly once");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 15. Conversation history fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_contains_all_expected_entries_after_tool_loop() {
    // Q45: Tool loop persists the expected message sequence.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            tool_response(vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "tool-out"}"#.into(),
            }]),
            text_response("final answer"),
        ]));

        let mut agent = build_agent_with(provider, vec![Box::new(EchoTool)], Box::new(NativeToolDispatcher));

        let _ = agent.turn("test").await.unwrap();

        // Expected history entries:
        //   0: system prompt
        //   1: user message "test"
        //   2: AssistantToolCalls
        //   3: ToolResults
        //   4: assistant "final answer"
        let history = agent.history();
        assert!(
            history.len() >= 5,
            "Expected at least 5 history entries, got {}",
            history.len()
        );

        assert!(matches!(&history[0], ConversationMessage::Chat(c) if c.role == "system"));
        assert!(matches!(&history[1], ConversationMessage::Chat(c) if c.role == "user"));
        assert!(matches!(&history[2], ConversationMessage::AssistantToolCalls { .. }));
        assert!(matches!(&history[3], ConversationMessage::ToolResults(_)));
        assert!(
            matches!(&history[4], ConversationMessage::Chat(c) if c.role == "assistant" && c.content == "final answer")
        );
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 16. Builder validation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn builder_fails_without_provider() {
    let result = Agent::builder()
        .tools(vec![])
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::path::PathBuf::from("/tmp"))
        .build();

    assert!(result.is_err(), "Building without provider should fail");
}

// ═══════════════════════════════════════════════════════════════════════════
// 17. Multi-turn conversation maintains context
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_maintains_growing_history() {
    // Q45: Successful consecutive replies grow conversation history.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            text_response("response 1"),
            text_response("response 2"),
            text_response("response 3"),
        ]));

        let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

        let r1 = agent.turn("msg 1").await.unwrap();
        let len_after_1 = agent.history().len();

        let r2 = agent.turn("msg 2").await.unwrap();
        let len_after_2 = agent.history().len();

        let r3 = agent.turn("msg 3").await.unwrap();
        let len_after_3 = agent.history().len();

        assert_eq!(r1, "response 1");
        assert_eq!(r2, "response 2");
        assert_eq!(r3, "response 3");

        // History should grow with each turn (user + assistant per turn)
        assert!(len_after_2 > len_after_1, "History should grow after turn 2");
        assert!(len_after_3 > len_after_2, "History should grow after turn 3");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 20. ConversationMessage serialization round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn conversation_message_serialization_roundtrip() {
    let messages = vec![
        ConversationMessage::Chat(ChatMessage::system("system")),
        ConversationMessage::Chat(ChatMessage::user("hello")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    for msg in &messages {
        let json = serde_json::to_string(msg).unwrap();
        let parsed: ConversationMessage = serde_json::from_str(&json).unwrap();

        // Verify the variant type matches
        match (msg, &parsed) {
            (ConversationMessage::Chat(a), ConversationMessage::Chat(b)) => {
                assert_eq!(a.role, b.role);
                assert_eq!(a.content, b.content);
            }
            (
                ConversationMessage::AssistantToolCalls {
                    text: a_text,
                    tool_calls: a_calls,
                },
                ConversationMessage::AssistantToolCalls {
                    text: b_text,
                    tool_calls: b_calls,
                },
            ) => {
                assert_eq!(a_text, b_text);
                assert_eq!(a_calls.len(), b_calls.len());
            }
            (ConversationMessage::ToolResults(a), ConversationMessage::ToolResults(b)) => {
                assert_eq!(a.len(), b.len());
            }
            _ => panic!("Variant mismatch after serialization"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 22. to_provider_messages conversion
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_converts_history_to_provider_messages() {
    let dispatcher = XmlToolDispatcher;
    let history = vec![
        ConversationMessage::Chat(ChatMessage::system("sys")),
        ConversationMessage::Chat(ChatMessage::user("hi")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    let messages = dispatcher.to_provider_messages(&history);

    // Should have: system, user, assistant (from tool calls), user (tool results), assistant
    assert!(messages.len() >= 4);
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "user");
}

#[test]
fn native_dispatcher_converts_tool_results_to_tool_messages() {
    let dispatcher = NativeToolDispatcher;
    let history = vec![ConversationMessage::ToolResults(vec![
        ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "output1".into(),
        },
        ToolResultMessage {
            tool_call_id: "tc2".into(),
            content: "output2".into(),
        },
    ])];

    let messages = dispatcher.to_provider_messages(&history);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "tool");
    assert_eq!(messages[1].role, "tool");
}

// ═══════════════════════════════════════════════════════════════════════════
// 24. Clear history
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn clear_history_resets_conversation() {
    // Q45: Clearing history resets subsequent conversation state.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let provider = Box::new(ScriptedProvider::new(vec![
            text_response("first"),
            text_response("second"),
        ]));

        let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

        let _ = agent.turn("hi").await.unwrap();
        assert!(!agent.history().is_empty());

        agent.clear_history();
        assert!(agent.history().is_empty());

        // Next turn should re-inject system prompt
        let _ = agent.turn("hello again").await.unwrap();
        assert!(matches!(
            &agent.history()[0],
            ConversationMessage::Chat(c) if c.role == "system"
        ));
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 25. run_single delegates to turn
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn run_single_delegates_to_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("via run_single")]));
    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

    let response = agent.run_single("test").await.unwrap();
    assert!(!response.is_empty(), "Expected non-empty response from run_single");
}

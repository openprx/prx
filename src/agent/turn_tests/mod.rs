#![cfg(test)]
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

pub(crate) mod admission;

use crate::agent::compaction::is_tool_roundtrip_companion;
use crate::agent::context_budget::{budgeted_tokens, estimate_history_tokens_with_source};
use crate::agent::execution_policy::{AgentToolApprovalStrategy, agent_tool_execution_context};
use crate::agent::history::{DEFAULT_MAX_HISTORY_MESSAGES, build_native_assistant_history};
use crate::agent::turn_contracts::ToolLoopRuntimeAdapter;
use crate::memory::{Memory, SqliteMemory};
use crate::providers::traits::ProviderCapabilities;
use crate::providers::{ChatMessage, ChatRequest, ChatResponse, Provider, ToolCall};
use crate::tools::{Tool, ToolCategory, ToolTier};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

pub(super) struct ScriptedProvider {
    pub(super) responses: Arc<Mutex<VecDeque<ChatResponse>>>,
}

impl ScriptedProvider {
    pub(super) fn from_text_responses(responses: Vec<&str>) -> Self {
        let scripted = responses
            .into_iter()
            .map(|text| ChatResponse {
                text: Some(text.to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
            .collect();
        Self {
            responses: Arc::new(Mutex::new(scripted)),
        }
    }
}

/// Distinctive text planted in the fixture's schema and description so a
/// test can prove the durable request event never copies a schema body.
pub(super) const WIDE_SCHEMA_TOOL_BODY_MARKER: &str = "wide-schema-probe-body";

/// A tool whose published description and JSON schema are deliberately
/// large, so the native tool payload dominates the request it travels in.
pub(super) struct WideSchemaTool {
    name: &'static str,
    description: String,
    schema: serde_json::Value,
    output: String,
}

impl WideSchemaTool {
    pub(super) fn with_fields(name: &'static str, fields: usize) -> Self {
        let description = format!("{WIDE_SCHEMA_TOOL_BODY_MARKER} ").repeat(200);
        let properties = (0..fields)
            .map(|index| {
                (
                    format!("field_{index}"),
                    serde_json::json!({"type": "string", "description": description}),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        Self {
            name,
            schema: serde_json::json!({"type": "object", "properties": properties}),
            description,
            output: "ok".to_string(),
        }
    }

    pub(super) fn new(name: &'static str) -> Self {
        Self::with_fields(name, 4)
    }

    pub(super) fn with_output(mut self, output: String) -> Self {
        self.output = output;
        self
    }
}

#[async_trait]
impl Tool for WideSchemaTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: self.output.clone(),
            error: None,
        })
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
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system should not be used in scripted provider tests");
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        let mut responses = self.responses.lock().expect("responses lock should be valid");
        responses
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("scripted provider exhausted responses"))
    }
}

/// FIX-P2-02: a provider that advertises native tool calling and returns
/// structured `tool_calls` from a script, so `run_turn` takes the
/// native-history branch. Used to verify tool-output persistence is not
/// duplicated in native mode.
pub(super) struct NativeScriptedProvider {
    pub(super) responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl NativeScriptedProvider {
    pub(super) fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn requests(&self) -> Vec<Vec<ChatMessage>> {
        self.requests.lock().expect("requests lock should be valid").clone()
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
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system should not be used in native scripted provider tests");
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        self.requests
            .lock()
            .expect("requests lock should be valid")
            .push(request.messages.to_vec());
        let mut responses = self.responses.lock().expect("responses lock should be valid");
        responses
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("native scripted provider exhausted responses"))
    }
}

pub(super) struct SystemSummaryProvider {
    pub(super) response: String,
}

#[async_trait]
impl Provider for SystemSummaryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        anyhow::bail!("chat should not be used in system summary provider tests")
    }
}

pub(super) struct DelayTool {
    name: String,
    delay_ms: u64,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl DelayTool {
    pub(super) fn new(name: &str, delay_ms: u64, active: Arc<AtomicUsize>, max_active: Arc<AtomicUsize>) -> Self {
        Self {
            name: name.to_string(),
            delay_ms,
            active,
            max_active,
        }
    }
}

#[async_trait]
impl Tool for DelayTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Delay tool for testing parallel tool execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now_active, Ordering::SeqCst);

        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;

        self.active.fetch_sub(1, Ordering::SeqCst);

        let value = args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        Ok(crate::tools::ToolResult {
            success: true,
            output: format!("ok:{value}"),
            error: None,
        })
    }
}

pub(super) struct RecordingTool {
    pub(super) name: String,
    pub(super) execution_order: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Records execution order"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.execution_order
            .lock()
            .expect("execution order lock should be valid")
            .push(self.name.clone());
        Ok(crate::tools::ToolResult {
            success: true,
            output: self.name.clone(),
            error: None,
        })
    }
}

pub(super) struct RefreshCountingTool {
    pub(super) name: &'static str,
    pub(super) category: ToolCategory,
    pub(super) refreshes: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for RefreshCountingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Counts dynamic capability refreshes"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: "ok".to_string(),
            error: None,
        })
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        match self.category {
            ToolCategory::System => &[ToolCategory::System],
            ToolCategory::Automation => &[ToolCategory::Automation],
            _ => &[],
        }
    }
}

/// Builds the production tool-execution adapter used by
/// [`execute_tools_with_service`], so scheduler tests exercise the real
/// dispatch path rather than the `cfg(test)` mirror.
pub(super) fn scheduler_test_adapter(tools: Vec<Box<dyn Tool>>, ledger_dir: &TempDir) -> ToolLoopRuntimeAdapter {
    let (policy, tool_execution_context) = agent_tool_execution_context(None, "cli", None);
    let approval_strategy = AgentToolApprovalStrategy {
        manager: None,
        resolver: None,
        channel: "cli".to_string(),
        policy: Arc::clone(&policy),
        cancellation: None,
    };
    // Stateful tools refuse to run without a durable idempotency ledger, so
    // the serial lane needs one before it can be compared with the parallel
    // lane at all.
    let ledger: Arc<dyn Memory> =
        Arc::new(SqliteMemory::new(ledger_dir.path()).expect("test: sqlite ledger should open in a temp dir"));
    let service = crate::tools::ToolExecutionService::from_shared_boxed_registry(
        Arc::new(tools),
        Arc::new(crate::tools::SecurityEffectPolicy::new(policy)),
        Arc::new(approval_strategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    )
    .with_idempotency_memory(ledger);
    ToolLoopRuntimeAdapter {
        provider_trace: None,
        events: None,
        stream_provider: false,
        tool_execution_service: Some(Arc::new(service)),
        tool_execution_context,
        allowed_tool_names: None,
    }
}

/// Ten assistant -> tool -> assistant round-trips, i.e. the shape every real
/// agentic session has. Forty non-system messages is well past the point
/// where a cold window is forced, and the tool envelopes are byte-identical
/// to the ones `run_turn` appends.
pub(super) fn tool_roundtrip_history_fixture(rounds: usize) -> Vec<ChatMessage> {
    tool_roundtrip_history_fixture_with_tools(rounds, &["shell"])
}

/// Same shape, but the round-trips call the given tools in rotation so a
/// test can tell one folded tool apart from another.
pub(super) fn tool_roundtrip_history_fixture_with_tools(rounds: usize, tools: &[&str]) -> Vec<ChatMessage> {
    let mut history = vec![ChatMessage::system("sys")];
    for round in 0..rounds {
        let tool_name = tools.get(round % tools.len().max(1)).copied().unwrap_or("shell");
        history.push(ChatMessage::user(format!(
            "question {round} {}",
            "user-context ".repeat(20)
        )));
        history.push(ChatMessage::assistant(build_native_assistant_history(
            "",
            &[ToolCall {
                id: format!("call-{round}"),
                name: tool_name.to_string(),
                arguments: "{\"command\":\"ls\"}".to_string(),
            }],
            None,
        )));
        history.push(ChatMessage::tool(
            serde_json::json!({
                "tool_call_id": format!("call-{round}"),
                "content": format!("result {round} {}", "tool-output ".repeat(20)),
            })
            .to_string(),
        ));
        history.push(ChatMessage::assistant(format!(
            "answer {round} {}",
            "assistant-context ".repeat(20)
        )));
    }
    history
}

/// Index of the first `role="tool"` message that no longer has the assistant
/// envelope requesting it ahead of it. Providers reject such a transcript.
pub(super) fn first_orphan_tool_index(history: &[ChatMessage]) -> Option<usize> {
    let mut inside_roundtrip = false;
    for (index, message) in history.iter().enumerate() {
        if message.role == "tool" {
            if !inside_roundtrip {
                return Some(index);
            }
            continue;
        }
        inside_roundtrip = message.role == "assistant" && is_tool_roundtrip_companion(message);
    }
    None
}

const _: () = {
    assert!(DEFAULT_MAX_HISTORY_MESSAGES > 0);
    assert!(DEFAULT_MAX_HISTORY_MESSAGES <= 1000);
};

/// Reserve used by the context-budget fixtures below.
pub(super) const CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS: usize = 64;

pub(super) fn tool_specs_of(tools: &[Box<dyn Tool>]) -> Vec<crate::tools::ToolSpec> {
    crate::tools::ToolCatalog::from_tools(tools.iter().map(|tool| tool.as_ref())).tool_specs()
}

/// Business schemas registered by a fixture. `capability_search` is a
/// separately gated control and is absent after exact discovery selects the
/// executable tool.
pub(super) fn business_tool_specs_of(tools: &[Box<dyn Tool>]) -> Vec<crate::tools::ToolSpec> {
    tool_specs_of(tools)
        .into_iter()
        .filter(|spec| spec.name != "capability_search")
        .collect()
}

/// Deterministic history that measures at least `target_tokens`, built from
/// repeated fixed text so the size follows whichever estimator is compiled
/// in.
pub(super) fn context_budget_fixture_history(target_tokens: usize) -> Vec<ChatMessage> {
    let mut history = vec![ChatMessage::system("stable base system prompt")];
    let mut index = 0usize;
    while budgeted_tokens(estimate_history_tokens_with_source(&history)) < target_tokens {
        history.push(ChatMessage::user(format!(
            "turn {index} {}",
            "measured context ".repeat(40)
        )));
        index = index.saturating_add(1);
    }
    history
}

/// A window this history fits in on its own, with `headroom_tokens` to
/// spare and nothing more.
pub(super) fn context_budget_fixture_config(
    history: &[ChatMessage],
    headroom_tokens: usize,
) -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: budgeted_tokens(estimate_history_tokens_with_source(history))
            .saturating_add(headroom_tokens)
            .saturating_add(CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS),
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    }
}

pub(super) fn paging_test_config(_enabled: bool) -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 1,
        keep_recent_messages: 12,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig {
            eviction_threshold: 0.70,
            proactive_retrieval: true,
            max_recalled_pages: 3,
            retrieval_injection_role: crate::config::RetrievalInjectionRole::System,
            keep_recent_messages: 1,
        },
    }
}

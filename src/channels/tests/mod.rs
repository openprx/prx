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
mod dispatch;
mod factory_reload_fixture;
mod history;
mod im_budget;
mod inbound_gate;
mod inbound_policy;
mod persistence_probe;
mod process_message;
mod prompt;
mod provider_switch;
mod provider_tool_reload;
mod sanitize_identity;
mod signal_accounted;
mod smart_group_accounted;
mod supervisor;
mod turn_execution;
mod turn_failure;

use super::dispatch::*;
use super::history::*;
use super::inbound_policy::*;
use super::signal_vision::*;
use super::smart_gate::*;
use super::supervisor::*;
use super::turn::*;
use super::*;
use crate::memory::{Memory, MemoryCategory, MemoryPrincipal, SqliteMemory};
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::tools::{Tool, ToolResult};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

fn make_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    // Create minimal workspace files
    std::fs::write(tmp.path().join("SOUL.md"), "# Soul\nBe helpful.").unwrap();
    std::fs::write(tmp.path().join("IDENTITY.md"), "# Identity\nName: OpenPRX").unwrap();
    std::fs::write(tmp.path().join("USER.md"), "# User\nName: Test User").unwrap();
    std::fs::write(tmp.path().join("AGENTS.md"), "# Agents\nFollow instructions.").unwrap();
    std::fs::write(tmp.path().join("TOOLS.md"), "# Tools\nUse shell carefully.").unwrap();
    std::fs::write(tmp.path().join("HEARTBEAT.md"), "# Heartbeat\nCheck status.").unwrap();
    std::fs::write(tmp.path().join("MEMORY.md"), "# Memory\nUser likes Rust.").unwrap();
    tmp
}

/// Build a minimal `ChannelRuntimeContext` whose conversation history map is
/// seeded with `histories`, for exercising compact/clear in isolation.
pub(super) fn ctx_with_histories(histories: HashMap<String, Vec<ChatMessage>>) -> ChannelRuntimeContext {
    ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(HashMap::new()),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(std::env::temp_dir())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(histories)),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        test_inbound_authorizer: None,
    }
}

// ── Root connection containment on a generation provider switch ──────

struct DummyProvider;

#[async_trait::async_trait]
impl Provider for DummyProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }
}

#[derive(Default)]
struct RecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
    start_typing_calls: AtomicUsize,
    stop_typing_calls: AtomicUsize,
}

#[derive(Default)]
struct TelegramRecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
}

fn response_messages(sent_messages: &[String]) -> Vec<&str> {
    let notice = crate::config::schema::InteractionNoticeConfig::default().text;
    sent_messages
        .iter()
        .map(String::as_str)
        .filter(|message| !message.contains(&notice))
        .collect()
}

fn tiering_for_test_tools(tools: &[Box<dyn Tool>]) -> crate::config::ToolTieringConfig {
    crate::config::ToolTieringConfig {
        always_include: tools.iter().map(|tool| tool.name().to_owned()).collect(),
        ..crate::config::ToolTieringConfig::default()
    }
}

#[async_trait::async_trait]
impl Channel for TelegramRecordingChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent_messages
            .lock()
            .await
            .push(format!("{}:{}", message.recipient, message.content));
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn name(&self) -> &str {
        "test-channel"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent_messages
            .lock()
            .await
            .push(format!("{}:{}", message.recipient, message.content));
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        self.start_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        self.stop_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn tool_call_payload() -> String {
    r#"<tool_call>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</tool_call>"#
        .to_string()
}

/// Counts its own executions, so a test can tell "the tool ran" from "the
/// tool was refused and the model was handed the refusal as tool output".
#[derive(Default)]
struct MockPriceTool {
    executions: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ModelCaptureProvider {
    call_count: AtomicUsize,
    models: parking_lot::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for ModelCaptureProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.models.lock().push(model.to_string());
        Ok("ok".to_string())
    }
}

#[async_trait::async_trait]
impl Tool for MockPriceTool {
    fn name(&self) -> &str {
        "mock_price"
    }

    fn description(&self) -> &str {
        "Return a mocked BTC price"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string" }
            },
            "required": ["symbol"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let symbol = args.get("symbol").and_then(serde_json::Value::as_str);
        if symbol != Some("BTC") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("unexpected symbol".to_string()),
            });
        }

        Ok(ToolResult {
            success: true,
            output: r#"{"symbol":"BTC","price_usd":65000}"#.to_string(),
            error: None,
        })
    }
}

struct ChannelProbeServer {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Drop for ChannelProbeServer {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            // Cancellation requests abort; Drop cannot await an acknowledgement.
            task.abort();
        }
    }
}

/// The dispatcher supplies this ownership in production; direct probes own it here.
async fn run_owned_channel_probe(
    ctx: Arc<ChannelRuntimeContext>,
    message: traits::ChannelMessage,
    cancellation: CancellationToken,
) {
    use crate::runtime::turn_cleanup::{CleanupScope, CleanupTracker};
    use futures_util::FutureExt;
    let ctx = normalize_legacy_channel_fixture(ctx);
    let parent = Arc::new(CleanupTracker::default());
    let cleanup =
        CleanupScope::new(Some(&parent), CancellationToken::new()).expect("fresh channel probe cleanup owner");
    let retained_context = Arc::clone(&ctx);
    let outcome = std::panic::AssertUnwindSafe(cleanup.run(async move {
        process_channel_message(ctx, message, cancellation).await;
        Ok(())
    }))
    .catch_unwind()
    .await;
    let drained = parent
        .drain(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    drop(retained_context);
    match outcome {
        Err(panic) => {
            if let Err(error) = drained {
                let subscriber = tracing_subscriber::fmt()
                    .with_test_writer()
                    .with_max_level(tracing::Level::ERROR)
                    .finish();
                tracing::subscriber::with_default(subscriber, || {
                    tracing::error!(cleanup = %error, "channel probe panicked and cleanup is unconfirmed");
                });
            }
            std::panic::resume_unwind(panic);
        }
        Ok(work) => {
            work.expect("channel probe child cleanup");
            drained.expect("channel probe parent cleanup");
        }
    }
}

/// A real channel turn context whose memory database, hooks and workspace
/// (terminal events, cost ledger) all live under `tmp`, so one scan of
/// `tmp` covers everything the turn persists.
fn channel_turn_probe_ctx(
    tmp: &TempDir,
    memory: Arc<dyn Memory>,
    provider: Arc<dyn Provider>,
    channel: Arc<dyn Channel>,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let mut runtime_config = Config::default();
    runtime_config.workspace_dir = tmp.path().to_path_buf();
    runtime_config.config_path = tmp.path().join("config.toml");
    runtime_config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    runtime_config.default_provider = Some("test-provider".to_string());
    runtime_config.default_model = Some("test-model".to_string());
    let config_manager = crate::config::new_shared(runtime_config);
    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: Arc::clone(&config_manager),
        config_generation: config_manager.pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider,
        default_provider: Arc::new("test-provider".to_string()),
        memory,
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(tmp.path().to_path_buf())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(tmp.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..crate::security::SecurityPolicy::default()
            }),
        })),
        test_inbound_authorizer: None,
    })
}

struct NoopMemory;

#[async_trait::async_trait]
impl Memory for NoopMemory {
    fn name(&self) -> &str {
        "noop"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&crate::memory::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }
}

/// Memory mock that counts `append_conversation_turn` invocations so tests
/// can assert whether the inbound SideEffectGate let the persistence run
/// (FIX-P0-10/11/12). All other ops are no-ops.
#[derive(Default)]
struct CountingMemory {
    append_calls: AtomicUsize,
    /// Counts the per-message autosave write path
    /// (`store_with_context_and_metadata`, the method the channel autosave
    /// branch actually calls), so the autosave-deny / autosave-allow tests
    /// can assert whether the autosave side effect ran.
    store_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Memory for CountingMemory {
    fn name(&self) -> &str {
        "counting"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    // The channel autosave branch writes via store_with_context_and_metadata;
    // override it (rather than the bare store) so store_calls reflects the
    // autosave side effect the gate is meant to permit or suppress.
    async fn store_with_context_and_metadata(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
        _context: Option<&crate::memory::MemoryWriteContext>,
        _metadata: crate::memory::MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&crate::memory::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_conversation_turn(
        &self,
        _session_key: &str,
        _channel: &str,
        _sender: &str,
        _role: &str,
        _content: &str,
        _timestamp: Option<&str>,
        _message_id: Option<&str>,
        _owner_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.append_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Tunable knobs for the inbound-gate test contexts. Defaults reproduce the
/// original `gate_test_ctx` behavior (autosave off, production gate
/// authorizer).
struct GateTestOpts {
    autonomy: crate::security::AutonomyLevel,
    auto_save_memory: bool,
    test_inbound_authorizer: Option<Arc<dyn crate::security::inbound_gate::InboundAuthorizer + Send + Sync>>,
}

impl GateTestOpts {
    fn new(autonomy: crate::security::AutonomyLevel) -> Self {
        Self {
            autonomy,
            auto_save_memory: false,
            test_inbound_authorizer: None,
        }
    }
}

/// Build a minimal `ChannelRuntimeContext` from explicit `GateTestOpts`,
/// wired to the provided counting memory and channel, for the inbound-gate
/// tests below.
fn gate_test_ctx_with(
    opts: GateTestOpts,
    memory: Arc<CountingMemory>,
    channel: Arc<dyn Channel>,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory,
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: opts.auto_save_memory,
        memory_event_recording: MemoryEventRecording::default(),
        read_only_tool_concurrency_window: 2,
        priority_scheduling_enabled: false,
        low_priority_tools: Vec::new(),

        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),

        tool_tiering: crate::config::ToolTieringConfig::default(),
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new(crate::security::SecurityPolicy {
                autonomy: opts.autonomy,
                ..crate::security::SecurityPolicy::default()
            }),
        })),
        test_inbound_authorizer: opts.test_inbound_authorizer,
    })
}

/// Build a minimal `ChannelRuntimeContext` wired to the given autonomy
/// level, the provided counting memory, and a `RecordingChannel`, for the
/// inbound-gate tests below.
fn gate_test_ctx(
    autonomy: crate::security::AutonomyLevel,
    memory: Arc<CountingMemory>,
    channel: Arc<dyn Channel>,
) -> Arc<ChannelRuntimeContext> {
    gate_test_ctx_with(GateTestOpts::new(autonomy), memory, channel)
}

fn gate_test_message() -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: "gate-msg-1".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-gate".to_string(),
        content: "hello gate".to_string(),
        channel: "test-channel".to_string(),
        timestamp: 1,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

#![cfg(test)]
use super::*;

struct ToolCallingProvider;

#[async_trait::async_trait]
impl Provider for ToolCallingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let has_tool_results = messages
            .iter()
            .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
        if has_tool_results {
            Ok("BTC is currently around $65,000 based on latest tool output.".to_string())
        } else {
            Ok(tool_call_payload())
        }
    }
}

#[tokio::test]
async fn process_channel_message_executes_tool_calls_instead_of_sending_raw_json() {
    let dir = TempDir::new().expect("test: tempdir");
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    // The mock provider answers the same canned sentence for any `[Tool
    // results]` block, refusal text included, so the reply alone cannot tell
    // a run tool from a refused one. Count the executions instead.
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(ToolCallingProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool {
            executions: Arc::clone(&executions),
        })]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(dir.path().to_path_buf())),
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
        workspace_dir: Arc::new(dir.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
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
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    run_owned_channel_probe(
        Arc::clone(&runtime_ctx),
        traits::ChannelMessage {
            id: "msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-42".to_string(),
            content: "What is the BTC price now?".to_string(),
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
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool must actually have run; a refusal would also have produced a [Tool results] block"
    );

    let sent_messages = channel_impl.sent_messages.lock().await;
    let responses = response_messages(&sent_messages);
    assert_eq!(responses.len(), 1);
    let response = responses.first().unwrap();
    assert!(response.starts_with("chat-42:"));
    assert!(response.contains("BTC is currently around"));
    assert!(!response.contains("\"tool_calls\""));
    assert!(!response.contains("mock_price"));
    drop(sent_messages);

    let histories = runtime_ctx.conversation_histories.lock();
    let assistant_turns: Vec<&ChatMessage> = histories
        .values()
        .flat_map(|turns| turns.iter())
        .filter(|message| message.role == "assistant")
        .collect();
    assert!(
        assistant_turns
            .iter()
            .any(|message| message.content.contains("BTC is currently around")),
        "the natural-language answer must be retained in channel history: {assistant_turns:?}"
    );
    assert!(
        assistant_turns
            .iter()
            .all(|message| !message.content.starts_with("[Used tools:")),
        "tool provenance must not be injected into assistant history: {assistant_turns:?}"
    );
}

/// Tool with a side effect that survives the process: one line appended per
/// execution, so "was it run twice?" is answered by counting lines rather
/// than by trusting a counter in memory.
struct AppendingTool {
    path: std::path::PathBuf,
}

#[async_trait::async_trait]
impl Tool for AppendingTool {
    fn name(&self) -> &str {
        "append_line"
    }

    fn description(&self) -> &str {
        "Append one line to the audit file"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "appended")?;
        Ok(ToolResult {
            success: true,
            output: "appended one line".to_string(),
            error: None,
        })
    }
}

/// Stands in for a real model on the engine recovery path: it asks for the tool while
/// the history shows no tool has run, and once the history *does* show the
/// result it fails with a context-window error. The canonical engine
/// classifier must recognize this provider wording and retain the executed
/// tool record across every compaction retry.
#[derive(Default)]
struct EngineOverflowProbeProvider {
    /// Whether each `chat_with_history` call saw evidence of an executed tool.
    saw_tool_evidence: parking_lot::Mutex<Vec<bool>>,
}

fn append_tool_call_payload() -> String {
    r#"<tool_call>
{"name":"append_line","arguments":{}}
</tool_call>"#
        .to_string()
}

#[async_trait::async_trait]
impl Provider for EngineOverflowProbeProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(append_tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let evidence = messages.iter().any(is_tool_result_message);
        self.saw_tool_evidence.lock().push(evidence);
        if evidence {
            Err(anyhow::anyhow!("input is too long"))
        } else {
            Ok(append_tool_call_payload())
        }
    }
}

fn seed_channel_history_for_overflow_progress(ctx: &ChannelRuntimeContext, message: &traits::ChannelMessage) {
    let key = ConversationKey::from_message(message);
    let turns = (0..20)
        .map(|index| {
            if index % 2 == 0 {
                ChatMessage::user(format!("older user turn {index}"))
            } else {
                ChatMessage::assistant(format!("older assistant turn {index}"))
            }
        })
        .collect();
    ctx.conversation_histories.lock().insert(key.canonical, turns);
}

/// A channel turn must leave overflow recovery to the engine without
/// re-running tools the failed attempt already executed.
#[tokio::test]
async fn channel_turn_engine_overflow_recovery_does_not_re_execute_already_executed_tools() {
    let dir = TempDir::new().expect("test: tempdir");
    let audit = dir.path().join("audit.log");

    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider = Arc::new(EngineOverflowProbeProvider::default());
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&provider) as Arc<dyn Provider>,
        default_provider: Arc::new("test-provider".to_string()),
        // A real SQLite backend, because a side-effecting tool is refused
        // outright without the durable idempotency ledger. Running the tool
        // *with* the ledger in place is also the point: the ledger keys on
        // the per-call tool id, which a second model tool call would mint afresh, so
        // it cannot be what stops the repeat.
        memory: Arc::clone(&memory),
        tools_registry: Arc::new(vec![Box::new(AppendingTool { path: audit.clone() }) as Box<dyn Tool>]),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
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
        reliability: Arc::new(crate::config::ReliabilityConfig {
            provider_retries: 0,
            ..crate::config::ReliabilityConfig::default()
        }),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(dir.path().to_path_buf()),

        tool_tiering: crate::config::ToolTieringConfig {
            always_include: vec!["append_line".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
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
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    });

    let message = traits::ChannelMessage {
        id: "msg-replay-1".to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-replay".to_string(),
        content: "Append one line please".to_string(),
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
    };
    let history_key = ConversationKey::from_message(&message);
    seed_channel_history_for_overflow_progress(runtime_ctx.as_ref(), &message);
    run_owned_channel_probe(Arc::clone(&runtime_ctx), message, CancellationToken::new()).await;

    let evidence = provider.saw_tool_evidence.lock().clone();
    assert!(
        evidence.len() >= 3,
        "test: the engine must actually compact and retry: {evidence:?}"
    );
    assert!(
        evidence.iter().skip(1).all(|seen| *seen),
        "every call after the tool ran must still see the executed-tool record: {evidence:?}"
    );

    let appended = std::fs::read_to_string(&audit).expect("test: audit file written by the tool");
    let lines = appended.lines().filter(|line| *line == "appended").count();
    assert_eq!(
        lines, 1,
        "engine recovery must not run the tool a second time; audit file: {appended:?}"
    );
    let cached = runtime_ctx.conversation_histories.lock();
    assert!(
        cached
            .get(&history_key.canonical)
            .is_some_and(|turns| turns.iter().any(|turn| turn.content == "older user turn 0")),
        "the channel layer must not run an independent cache compactor after the engine gives up"
    );
}

// ---------------------------------------------------------------------
// Tool outcome fidelity: a channel turn must tell "the tool ran", "the tool
// was refused" and "the tool failed" apart.
//
// A mock provider that answers the same canned sentence whenever the history
// shows *any* `[Tool results]` block passes all three ways, because a refusal
// and a failure are both delivered to the model as tool result text. These
// tests assert the side effect on disk and the exact text the model saw.
// ---------------------------------------------------------------------

/// Writes a file when it runs. The name is deliberately absent from
/// `READ_ONLY_TOOLS`, so its descriptor effect is `ToolEffect::Act` and the
/// idempotency ledger is mandatory for it.
struct OutcomeProbeTool {
    path: std::path::PathBuf,
    /// When set, the tool reports failure instead of writing.
    fail: bool,
}

#[async_trait::async_trait]
impl Tool for OutcomeProbeTool {
    fn name(&self) -> &str {
        "outcome_probe_write"
    }

    fn description(&self) -> &str {
        "Write a marker file"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if self.fail {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("outcome probe was told to fail".to_string()),
            });
        }
        std::fs::write(&self.path, "written by the tool")?;
        Ok(ToolResult {
            success: true,
            output: "outcome probe wrote the marker".to_string(),
            error: None,
        })
    }
}

/// Asks for the side-effecting tool once, then records verbatim every
/// `[Tool results]` block it is shown so a test can assert on the text the
/// model actually received rather than on "the turn did not crash".
#[derive(Default)]
struct OutcomeRecordingProvider {
    observed_tool_results: parking_lot::Mutex<Vec<String>>,
    calls: AtomicU64,
}

fn outcome_probe_call_payload() -> String {
    r#"<tool_call>
{"name":"outcome_probe_write","arguments":{}}
</tool_call>"#
        .to_string()
}

#[async_trait::async_trait]
impl Provider for OutcomeRecordingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(outcome_probe_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tool_results = messages
            .iter()
            .filter(|message| message.content.contains("[Tool results]"))
            .map(|message| message.content.clone())
            .collect::<Vec<_>>();
        if tool_results.is_empty() {
            return Ok(outcome_probe_call_payload());
        }
        self.observed_tool_results.lock().extend(tool_results);
        Ok("acknowledged".to_string())
    }
}

/// Runtime context for the outcome-fidelity tests: an explicit memory backend
/// and workspace, because both decide whether a durable ledger is reachable.
fn outcome_runtime_ctx(
    workspace_dir: &std::path::Path,
    memory: Arc<dyn Memory>,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    channel: Arc<dyn Channel>,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let tool_tiering = tiering_for_test_tools(&tools);
    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider,
        default_provider: Arc::new("test-provider".to_string()),
        memory,
        tools_registry: Arc::new(tools),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(workspace_dir.to_path_buf())),
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
        workspace_dir: Arc::new(workspace_dir.to_path_buf()),

        tool_tiering,
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
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    })
}

/// Drive one channel turn and return the provider calls and any
/// `[Tool results]` text the model saw.
async fn run_outcome_turn(
    workspace_dir: &std::path::Path,
    memory: Arc<dyn Memory>,
    tool: OutcomeProbeTool,
    admission_succeeds: bool,
) -> (u64, Vec<String>) {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let provider = Arc::new(OutcomeRecordingProvider::default());
    let runtime_ctx = outcome_runtime_ctx(
        workspace_dir,
        memory,
        Arc::clone(&provider) as Arc<dyn Provider>,
        vec![Box::new(tool) as Box<dyn Tool>],
        channel,
    );
    let message = evidence_message("outcome-1", "please run the probe");
    if admission_succeeds {
        run_owned_channel_probe(Arc::clone(&runtime_ctx), message, CancellationToken::new()).await;
    } else {
        let runtime_ctx = normalize_legacy_channel_fixture(runtime_ctx);
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
            .expect("fresh channel probe cleanup owner");
        let outcome = cleanup
            .run(async move {
                process_channel_message(runtime_ctx, message, CancellationToken::new()).await;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        assert!(
            outcome.is_err(),
            "the unadmittable turn must report failed terminal cleanup"
        );
    }
    (
        provider.calls.load(Ordering::SeqCst),
        provider.observed_tool_results.lock().clone(),
    )
}

/// The load-bearing assertion: with a ledger reachable, a side-effecting tool
/// invoked by a real channel turn leaves its side effect on disk.
///
/// The memory backend here is `markdown`, which keeps no durable
/// `MessageEvent` log of its own. The ledger must still be there, resolved as
/// runtime infrastructure rather than borrowed from the memory backend.
#[tokio::test]
async fn channel_turn_side_effect_tool_actually_writes_its_file() {
    let dir = TempDir::new().expect("test: tempdir");
    let marker = dir.path().join("marker.txt");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::MarkdownMemory::new(dir.path()));

    let (provider_calls, observed) = run_outcome_turn(
        dir.path(),
        memory,
        OutcomeProbeTool {
            path: marker.clone(),
            fail: false,
        },
        true,
    )
    .await;

    assert!(provider_calls > 0, "the admitted turn must reach the provider");
    let written =
        std::fs::read_to_string(&marker).expect("test: the side-effecting tool must actually have written its file");
    assert_eq!(written, "written by the tool");
    assert!(
        observed
            .iter()
            .any(|text| text.contains("outcome probe wrote the marker")),
        "the model must be shown the tool's own success output: {observed:?}"
    );
    assert!(
        !observed.iter().any(|text| text.contains("refusing side effect")),
        "a reachable ledger must not produce a refusal: {observed:?}"
    );
}

/// A tool that reports failure is distinguishable from one that was refused:
/// no side effect, and the tool's own error text reaches the model.
#[tokio::test]
async fn channel_turn_failing_tool_reports_its_own_error_and_writes_nothing() {
    let dir = TempDir::new().expect("test: tempdir");
    let marker = dir.path().join("marker.txt");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::MarkdownMemory::new(dir.path()));

    let (provider_calls, observed) = run_outcome_turn(
        dir.path(),
        memory,
        OutcomeProbeTool {
            path: marker.clone(),
            fail: true,
        },
        true,
    )
    .await;

    assert!(provider_calls > 0, "the admitted turn must reach the provider");
    assert!(!marker.exists(), "a failing tool must not leave the side effect behind");
    assert!(
        observed
            .iter()
            .any(|text| text.contains("outcome probe was told to fail")),
        "the model must be shown the tool's own error: {observed:?}"
    );
    assert!(
        !observed.iter().any(|text| text.contains("refusing side effect")),
        "a tool failure must not be reported as a ledger refusal: {observed:?}"
    );
}

/// The fail-closed half of the contract: when no ledger can be opened at all,
/// input admission stops before the provider and nothing is written.
///
/// The workspace is a regular file, so the dedicated ledger cannot be created
/// under it and no ledger is reachable by any route.
#[tokio::test]
async fn channel_turn_without_any_reachable_ledger_refuses_the_side_effect() {
    let dir = TempDir::new().expect("test: tempdir");
    let marker = dir.path().join("marker.txt");
    let blocked_workspace = dir.path().join("workspace-is-a-file");
    std::fs::write(&blocked_workspace, "not a directory").expect("test: block the workspace");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::MarkdownMemory::new(dir.path()));

    let (provider_calls, observed) = run_outcome_turn(
        &blocked_workspace,
        memory,
        OutcomeProbeTool {
            path: marker.clone(),
            fail: false,
        },
        false,
    )
    .await;

    assert!(!marker.exists(), "unadmittable input must not produce a side effect");
    assert_eq!(
        provider_calls, 0,
        "unadmittable input must fail closed before provider execution"
    );
    assert!(
        observed.is_empty(),
        "a provider that was not called cannot observe tool results"
    );
}

/// Shared runtime context for the unbounded-turn evidence tests.
///
/// A real SQLite backend, because a side-effecting tool is refused outright
/// without the durable idempotency ledger.
fn evidence_runtime_ctx(
    dir: &TempDir,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    channel: Arc<dyn Channel>,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    let tool_tiering = tiering_for_test_tools(&tools);
    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory,
        tools_registry: Arc::new(tools),
        observer: Arc::new(NoopObserver),
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
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
        workspace_dir: Arc::new(dir.path().to_path_buf()),

        tool_tiering,
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
            security: Arc::new(crate::security::SecurityPolicy::default()),
        })),
        test_inbound_authorizer: None,
    })
}

fn evidence_message(id: &str, content: &str) -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: id.to_string(),
        sender: "alice".to_string(),
        reply_target: "chat-evidence".to_string(),
        content: content.to_string(),
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

/// Answers only after a stretch of wall clock far beyond the deleted per-turn
/// budget, and — like any provider that is genuinely working — keeps the turn
/// visibly alive while it does.
struct GlacialProvider {
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

impl GlacialProvider {
    /// Longest the provider stays quiet between signs of life.
    ///
    /// A slow provider is not a *silent* one: it streams tokens, or reports
    /// a retry, or at minimum keeps a connection producing something. This
    /// models that at the coarsest end of plausible — one signal every ten
    /// minutes — which is comfortably inside
    /// [`crate::agent::idle::DEFAULT_IDLE_HANG_SECS`] (1800s), so the turn
    /// below is long without ever being silent.
    const PROGRESS_SLICE: Duration = Duration::from_mins(10);

    async fn answer_slowly(&self) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut remaining = self.delay;
        while !remaining.is_zero() {
            let slice = remaining.min(Self::PROGRESS_SLICE);
            tokio::time::sleep(slice).await;
            remaining = remaining.saturating_sub(slice);
            crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderStream);
        }
        "here is the slow answer".to_string()
    }
}

#[async_trait::async_trait]
impl Provider for GlacialProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.answer_slowly().await)
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.answer_slowly().await)
    }
}

/// **Core evidence 1** — a channel turn that runs far past the deleted wall
/// clock finishes and answers.
///
/// The deleted budget was `message_timeout_secs` (300) times the iteration
/// scale cap (4) = 1200s; the turn below takes two hours, six times that.
/// Time is paused, so the two hours cost no real seconds — the point is that
/// nothing on the channel path measures them any more. Before this change
/// the turn was aborted at 1200s, the whole turn was replayed once, and the
/// user was sent "⚠️ Request timed out. Please try again shortly."
///
/// The turn is long *and progressing*: the provider signals every ten
/// virtual minutes, the way one that is genuinely working does. That
/// distinction is the whole contract, and it is the only one left — duration
/// is not a fault, silence is, and with the wall clock deleted the idle
/// (no-progress) detector is the sole mechanism that can still end a turn.
/// A fixture that instead sat mute for two hours would not be testing the
/// absence of a wall clock; it would be a stall, and evidence 2 below is
/// what a stall is supposed to look like.
///
/// MUTATION: wrapping the tool loop in `process_channel_message` in a
/// `tokio::time::timeout` again turns this test red.
#[tokio::test(start_paused = true)]
async fn channel_turn_running_far_past_the_deleted_wall_clock_still_answers() {
    let dir = TempDir::new().expect("test: tempdir");
    let channel_impl = Arc::new(RecordingChannel::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(GlacialProvider {
        delay: Duration::from_hours(2),
        calls: Arc::clone(&calls),
    });
    let ctx = evidence_runtime_ctx(
        &dir,
        Arc::clone(&provider) as Arc<dyn Provider>,
        Vec::new(),
        Arc::clone(&channel_impl) as Arc<dyn Channel>,
    );

    run_owned_channel_probe(
        ctx,
        evidence_message("msg-glacial-1", "take as long as you need"),
        CancellationToken::new(),
    )
    .await;

    let sent = channel_impl.sent_messages.lock().await.clone();
    let replies = response_messages(&sent);
    assert!(
        replies
            .iter()
            .any(|message| message.contains("here is the slow answer")),
        "a two-hour turn must still deliver its answer: {replies:?}"
    );
    assert!(
        replies.iter().all(|message| !message.contains("timed out")),
        "nothing may report a timeout any more: {replies:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the turn must run once; a wall clock would have aborted and replayed it"
    );
}

/// A turn that produces nothing at all: no chunk, no tool, no output.
struct HangingProvider;

#[async_trait::async_trait]
impl Provider for HangingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        std::future::pending().await
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        std::future::pending().await
    }
}

/// **Core evidence 2** — deleting the wall clock deleted a clock, not the
/// defence. A turn that genuinely hangs is still ended, by the stall
/// detector, and the termination is visible in the runtime registry.
///
/// Paired with evidence 1 this is the whole claim: duration is not a fault
/// (evidence 1 runs for two hours untouched) but silence is (this one is
/// terminated after a fifth of a second of it).
#[tokio::test]
async fn hung_channel_turn_is_still_terminated_and_recorded_in_the_registry() {
    let dir = TempDir::new().expect("test: tempdir");
    let channel_impl = Arc::new(RecordingChannel::default());
    let ctx = evidence_runtime_ctx(
        &dir,
        Arc::new(HangingProvider) as Arc<dyn Provider>,
        Vec::new(),
        Arc::clone(&channel_impl) as Arc<dyn Channel>,
    );

    let before = crate::runtime::registry::hang_terminations().len();
    crate::agent::idle::with_guard(
        crate::agent::idle::IdleGuard {
            idle: Some(Duration::from_millis(200)),
        },
        run_owned_channel_probe(
            ctx,
            evidence_message("msg-hang-1", "this one wedges"),
            CancellationToken::new(),
        ),
    )
    .await;

    let terminations = crate::runtime::registry::hang_terminations();
    assert!(
        terminations.len() > before,
        "a hung turn must be recorded in the registry's hang ledger"
    );
    let recorded = terminations
        .iter()
        .skip(before)
        .find(|entry| &*entry.label == "test-channel")
        .expect("test: the hung channel turn must appear in the hang ledger");
    assert_eq!(
        recorded.reason,
        crate::agent::idle::HangReason::NoProgress.as_str(),
        "the turn must be reported as silent, not as having run too long"
    );
}

/// Second side-effecting tool, so one iteration can carry a real batch and
/// "did each of them run exactly once?" is answered by counting lines.
struct SecondAppendingTool {
    path: std::path::PathBuf,
}

#[async_trait::async_trait]
impl Tool for SecondAppendingTool {
    fn name(&self) -> &str {
        "append_second_line"
    }

    fn description(&self) -> &str {
        "Append one line to the audit file"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "appended-second")?;
        Ok(ToolResult {
            success: true,
            output: "appended the second line".to_string(),
            error: None,
        })
    }
}

/// Like [`EngineOverflowProbeProvider`] but asks for a *batch* of two side-effecting
/// tools in one iteration, which is the shape that used to lose records.
#[derive(Default)]
struct BatchEngineOverflowProbeProvider {
    saw_tool_evidence: parking_lot::Mutex<Vec<bool>>,
}

fn append_batch_call_payload() -> String {
    r#"<tool_call>
{"name":"append_line","arguments":{}}
</tool_call>
<tool_call>
{"name":"append_second_line","arguments":{}}
</tool_call>"#
        .to_string()
}

#[async_trait::async_trait]
impl Provider for BatchEngineOverflowProbeProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(append_batch_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let evidence = messages.iter().any(is_tool_result_message);
        self.saw_tool_evidence.lock().push(evidence);
        if evidence {
            Err(anyhow::anyhow!("input is too long"))
        } else {
            Ok(append_batch_call_payload())
        }
    }
}

/// **Core evidence 3 (end to end)** — engine overflow recovery re-runs none
/// of the tools a whole batch already executed.
///
/// Two side-effecting tools in a single iteration, one audit file, and the
/// production engine context-overflow path. Each tool must appear exactly
/// once in the file.
#[tokio::test]
async fn channel_turn_engine_overflow_recovery_does_not_re_execute_an_already_executed_batch() {
    let dir = TempDir::new().expect("test: tempdir");
    let audit = dir.path().join("batch-audit.log");
    let channel_impl = Arc::new(RecordingChannel::default());
    let provider = Arc::new(BatchEngineOverflowProbeProvider::default());
    let ctx = evidence_runtime_ctx(
        &dir,
        Arc::clone(&provider) as Arc<dyn Provider>,
        vec![
            Box::new(AppendingTool { path: audit.clone() }) as Box<dyn Tool>,
            Box::new(SecondAppendingTool { path: audit.clone() }) as Box<dyn Tool>,
        ],
        Arc::clone(&channel_impl) as Arc<dyn Channel>,
    );

    let message = evidence_message("msg-batch-replay-1", "append both lines please");
    let history_key = ConversationKey::from_message(&message);
    seed_channel_history_for_overflow_progress(ctx.as_ref(), &message);
    run_owned_channel_probe(Arc::clone(&ctx), message, CancellationToken::new()).await;

    let evidence = provider.saw_tool_evidence.lock().clone();
    assert!(
        evidence.len() >= 3,
        "test: the engine must actually compact and retry: {evidence:?}"
    );
    let appended = std::fs::read_to_string(&audit).expect("test: audit file written by the tools");
    let first = appended.lines().filter(|line| *line == "appended").count();
    let second = appended.lines().filter(|line| *line == "appended-second").count();
    assert_eq!(
        (first, second),
        (1, 1),
        "each tool in the batch must run exactly once across engine recovery; audit file: {appended:?}"
    );
    let cached = ctx.conversation_histories.lock();
    assert!(
        cached
            .get(&history_key.canonical)
            .is_some_and(|turns| turns.iter().any(|turn| turn.content == "older user turn 0")),
        "the channel layer must not run an independent cache compactor after the engine gives up"
    );
}

/// Records every draft write, so "what did the user actually see, and in what
/// order?" is answerable.
#[derive(Default)]
struct DraftRecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
    draft_updates: tokio::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Channel for DraftRecordingChannel {
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

    fn supports_draft_updates(&self) -> bool {
        true
    }

    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(Some("draft-1".to_string()))
    }

    async fn update_draft(&self, _recipient: &str, _message_id: &str, text: &str) -> anyhow::Result<()> {
        self.draft_updates.lock().await.push(text.to_string());
        Ok(())
    }

    async fn finalize_draft(&self, _recipient: &str, _message_id: &str, text: &str) -> anyhow::Result<()> {
        self.sent_messages.lock().await.push(format!("final:{text}"));
        Ok(())
    }
}

/// Asks for the tool once, then answers.
#[derive(Default)]
struct ToolThenAnswerProvider {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for ToolThenAnswerProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(append_tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(append_tool_call_payload())
        } else {
            Ok("all done".to_string())
        }
    }
}

/// Removing the wall clock removed the only thing that ever told a user a
/// long turn was still alive, so the tool notifications — which until now
/// were only logged — are shown in the draft while the turn has produced no
/// text of its own.
///
/// This reports and never terminates: no assertion here involves a deadline.
#[tokio::test]
async fn a_working_turn_shows_progress_in_its_draft_before_any_text_arrives() {
    let dir = TempDir::new().expect("test: tempdir");
    let audit = dir.path().join("progress-audit.log");
    let channel_impl = Arc::new(DraftRecordingChannel::default());
    let ctx = evidence_runtime_ctx(
        &dir,
        Arc::new(ToolThenAnswerProvider::default()) as Arc<dyn Provider>,
        vec![Box::new(AppendingTool { path: audit }) as Box<dyn Tool>],
        Arc::clone(&channel_impl) as Arc<dyn Channel>,
    );

    run_owned_channel_probe(
        ctx,
        evidence_message("msg-progress-1", "do the slow thing"),
        CancellationToken::new(),
    )
    .await;

    let updates = channel_impl.draft_updates.lock().await.clone();
    assert!(
        updates
            .iter()
            .any(|update| update.starts_with("⏳") && update.contains("append_line")),
        "a turn running tools with no text yet must say what it is doing: {updates:?}"
    );
    let sent = channel_impl.sent_messages.lock().await.clone();
    assert!(
        sent.iter().any(|message| message.contains("all done")),
        "the progress notes must not displace the real answer: {sent:?}"
    );
}

#[test]
fn progress_lines_read_as_progress_and_never_as_a_deadline() {
    assert_eq!(
        channel_progress_line("web_fetch", 7, Duration::from_secs(252)),
        "⏳ web_fetch… (step 7, 4m12s)"
    );
    assert_eq!(
        channel_progress_line("", 3, Duration::from_secs(9)),
        "⏳ working… (step 3, 9s)"
    );
}

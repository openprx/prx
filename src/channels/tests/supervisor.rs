#![cfg(test)]
use super::*;

#[test]
fn classify_health_ok_true() {
    let state = classify_health_result(&Ok(true));
    assert_eq!(state, ChannelHealthState::Healthy);
}

#[test]
fn classify_health_ok_false() {
    let state = classify_health_result(&Ok(false));
    assert_eq!(state, ChannelHealthState::Unhealthy);
}

#[tokio::test]
async fn classify_health_timeout() {
    let result = tokio::time::timeout(Duration::from_millis(1), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        true
    })
    .await;
    let state = classify_health_result(&result);
    assert_eq!(state, ChannelHealthState::Timeout);
}

struct AlwaysFailChannel {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

struct BlockUntilClosedChannel {
    name: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Channel for AlwaysFailChannel {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("listen boom")
    }
}

#[async_trait::async_trait]
impl Channel for BlockUntilClosedChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tx.closed().await;
        Ok(())
    }
}

#[tokio::test]
async fn supervised_listener_marks_error_and_restarts_on_failures() {
    let calls = Arc::new(AtomicUsize::new(0));
    let channel: Arc<dyn Channel> = Arc::new(AlwaysFailChannel {
        name: "test-supervised-fail",
        calls: Arc::clone(&calls),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let handle = spawn_supervised_listener(channel, tx, 1, 1, CancellationToken::new());

    tokio::time::sleep(Duration::from_millis(80)).await;
    drop(rx);
    handle.abort();
    let _ = handle.await;

    let snapshot = crate::health::snapshot_json();
    let component = &snapshot["components"]["channel:test-supervised-fail"];
    assert_eq!(component["status"], "error");
    assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
    assert!(component["last_error"].as_str().unwrap_or("").contains("listen boom"));
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

/// Fails the way a webhook listener does when its port is taken.
struct BindFailChannel;

#[async_trait::async_trait]
impl Channel for BindFailChannel {
    fn name(&self) -> &str {
        "test-supervised-bind-fail"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("address already in use")).context("failed to bind webhook server")
    }
}

/// D-072 L-4, RED before: the listener supervisor recorded
/// `to_string()`, the outer context only ("failed to bind webhook
/// server"), so health did not say why. After: the whole chain.
#[tokio::test]
async fn a_failed_listener_reports_the_whole_error_chain() {
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let handle = spawn_supervised_listener(Arc::new(BindFailChannel), tx, 60, 60, CancellationToken::new());
    let error = wait_for(|| {
        crate::health::snapshot_json()["components"]["channel:test-supervised-bind-fail"]["last_error"]
            .as_str()
            .filter(|error| error.contains("failed to bind"))
            .map(str::to_string)
    })
    .await
    .unwrap_or_default();
    drop(rx);
    handle.abort();
    let _ = handle.await;
    assert!(
        error.contains("failed to bind webhook server") && error.contains("address already in use"),
        "{error}"
    );
}

#[test]
fn channel_supervisor_opens_circuit_after_consecutive_failures() {
    assert_eq!(channel_supervisor_sleep_duration(0, 2, 60), Duration::from_secs(2));
    assert_eq!(
        channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES - 1, 16, 60),
        Duration::from_secs(16)
    );
    assert_eq!(
        channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES, 16, 60),
        Duration::from_mins(5)
    );
    assert_eq!(
        channel_supervisor_sleep_duration(CHANNEL_CIRCUIT_BREAKER_FAILURES + 10, 60, 60),
        Duration::from_mins(5)
    );
}

#[tokio::test]
async fn supervised_listener_refreshes_health_while_running() {
    let calls = Arc::new(AtomicUsize::new(0));
    let channel_name = format!("test-supervised-heartbeat-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
        name: channel_name,
        calls: Arc::clone(&calls),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let handle = spawn_supervised_listener_with_health_interval(
        channel,
        tx,
        1,
        1,
        Duration::from_millis(20),
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_millis(35)).await;
    let first_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!first_last_ok.is_empty(), "expected a non-empty value");

    tokio::time::sleep(Duration::from_millis(70)).await;
    let second_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let first = chrono::DateTime::parse_from_rfc3339(&first_last_ok).expect("last_ok should be valid RFC3339");
    let second = chrono::DateTime::parse_from_rfc3339(&second_last_ok).expect("last_ok should be valid RFC3339");
    assert!(second > first, "expected periodic health heartbeat refresh");

    drop(rx);
    let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
    assert!(join.is_ok(), "listener should stop after channel shutdown");
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn supervised_listener_stops_on_shutdown_without_restart() {
    // D5/D9 step 5 regression: a listener blocked inside `listen()` must
    // observe the external shutdown token and break out of its supervisor
    // loop (ListenerOutcome::Shutdown) without restarting, so the owning
    // `handles.await` can complete instead of hanging forever.
    let calls = Arc::new(AtomicUsize::new(0));
    let channel_name = format!("test-supervised-shutdown-{}", uuid::Uuid::new_v4());
    let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
        name: channel_name,
        calls: Arc::clone(&calls),
    });

    // Keep our own `tx` clone alive so the channel never closes on its own:
    // only the shutdown token should be able to stop the listener.
    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle = spawn_supervised_listener(channel, tx, 1, 1, shutdown.clone());

    // Let the listener enter its blocking `listen()` (tx.closed()).
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(calls.load(Ordering::SeqCst) >= 1, "listener should have started");

    // Request shutdown: the supervisor must break and the task must finish.
    shutdown.cancel();
    let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
    assert!(join.is_ok(), "listener should stop promptly on shutdown");

    // It stopped via the shutdown branch, not a restart: only one listen call.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "shutdown must not trigger a listener restart"
    );
}

/// A listener wedged forever inside `listen()`: the receive future never
/// resolves, exactly like a long poll that will never come back or a socket
/// that will never error. It declares a bounded liveness expectation, so the
/// silence is real evidence rather than an unknown.
struct WedgedChannel {
    name: String,
    expectation: Duration,
}

#[async_trait::async_trait]
impl Channel for WedgedChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    fn liveness_expectation(&self) -> activity::LivenessModel {
        activity::LivenessModel::Bounded(self.expectation)
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        // Never returns, never errors, never receives — the failure mode the
        // old timer-based heartbeat reported as perfectly healthy.
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A listener that keeps completing upstream round-trips without ever
/// delivering a message — the "healthy but nobody is talking" case that must
/// NOT be reported as stalled.
struct QuietButLiveChannel {
    name: String,
    expectation: Duration,
    round_trip: Duration,
}

#[async_trait::async_trait]
impl Channel for QuietButLiveChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    fn liveness_expectation(&self) -> activity::LivenessModel {
        activity::LivenessModel::Bounded(self.expectation)
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>) -> anyhow::Result<()> {
        loop {
            activity::record_upstream(self.name());
            tokio::time::sleep(self.round_trip).await;
        }
    }
}

fn component_json(component: &str) -> serde_json::Value {
    crate::health::snapshot_json()["components"][component].clone()
}

async fn drain_supervisor(shutdown: &CancellationToken, handle: tokio::task::JoinHandle<()>) {
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}

#[tokio::test]
async fn wedged_listener_is_reported_stalled_instead_of_healthy() {
    // P0-1 regression: the heartbeat used to call mark_component_ok() on every
    // tick regardless of what the listener was doing, so a channel that could
    // never receive again still published "ok". Health must now follow the
    // recorded activity, not the timer.
    let channel_name = format!("test-wedged-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(WedgedChannel {
        name: channel_name,
        expectation: Duration::from_millis(50),
    });

    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle =
        spawn_supervised_listener_with_health_interval(channel, tx, 1, 1, Duration::from_millis(20), shutdown.clone());

    // Well past the 3x tolerance on a 50ms expectation.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let component = component_json(&component_name);
    drain_supervisor(&shutdown, handle).await;

    assert_eq!(
        component["status"], "degraded",
        "a listener that can never receive again must not be published as healthy"
    );
    assert_eq!(component["state"], "degraded");
    assert_eq!(
        component["activity"]["stalled"], true,
        "the stall must be visible as measured activity, not only as a status word"
    );
    assert_eq!(component["activity"]["liveness"], "bounded");
    assert!(
        component["activity"]["idle_seconds"].as_u64().is_some(),
        "operators need the stall duration, not just a boolean"
    );
    assert!(
        component["last_error"].as_str().unwrap_or("").contains("stalled"),
        "expected an operator-facing stall summary, got {:?}",
        component["last_error"]
    );
}

#[tokio::test]
async fn stalled_listener_stays_visible_without_blocking_readiness() {
    // The stall must be loud in the detailed status and silent in the
    // readiness boolean. A false readiness makes /health answer 503, which
    // makes an orchestrator restart the process and kill every long-running
    // task in flight — automatic termination sneaking back in through the
    // probe after the timeouts were removed.
    let channel_name = format!("test-readiness-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(WedgedChannel {
        name: channel_name,
        expectation: Duration::from_millis(50),
    });

    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle =
        spawn_supervised_listener_with_health_interval(channel, tx, 1, 1, Duration::from_millis(20), shutdown.clone());

    tokio::time::sleep(Duration::from_millis(400)).await;
    let component = component_json(&component_name);
    let readiness = crate::health::readiness();
    drain_supervisor(&shutdown, handle).await;

    // Visible.
    assert_eq!(component["status"], "degraded");
    assert_eq!(component["activity"]["stalled"], true);
    assert!(component["activity"]["idle_seconds"].as_u64().is_some());
    assert!(component["last_error"].as_str().unwrap_or("").contains("stalled"));

    // Inert.
    assert!(
        !readiness.issues.iter().any(|issue| issue.component == component_name),
        "a stalled listener must not be raised as a readiness issue: {:?}",
        readiness.issues
    );
}

#[tokio::test]
async fn quiet_but_live_listener_stays_healthy() {
    // The counterpart to the regression above: a channel whose upstream keeps
    // answering with zero messages is perfectly healthy and must never be
    // reported as stalled just because nobody is talking to it.
    let channel_name = format!("test-quiet-live-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(QuietButLiveChannel {
        name: channel_name,
        expectation: Duration::from_millis(200),
        round_trip: Duration::from_millis(10),
    });

    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle =
        spawn_supervised_listener_with_health_interval(channel, tx, 1, 1, Duration::from_millis(20), shutdown.clone());

    tokio::time::sleep(Duration::from_millis(300)).await;
    let component = component_json(&component_name);
    drain_supervisor(&shutdown, handle).await;

    assert_eq!(component["status"], "ok");
    assert_eq!(component["activity"]["stalled"], false);
    assert!(
        component["activity"]["last_upstream_seconds_ago"].as_u64().is_some(),
        "completed round-trips must be recorded even with no inbound messages"
    );
}

#[tokio::test]
async fn passive_listener_publishes_idle_time_without_a_stall_verdict() {
    // Push-only channels have no cadence to measure against, so claiming a
    // stall would be a guess. They stay "ok" but advertise `passive`, which is
    // how an operator knows the green is not evidence-based.
    let channel_name = format!("test-passive-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
        name: channel_name,
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle =
        spawn_supervised_listener_with_health_interval(channel, tx, 1, 1, Duration::from_millis(20), shutdown.clone());

    tokio::time::sleep(Duration::from_millis(120)).await;
    let component = component_json(&component_name);
    drain_supervisor(&shutdown, handle).await;

    assert_eq!(component["status"], "ok");
    assert_eq!(component["activity"]["liveness"], "passive");
    assert_eq!(component["activity"]["stalled"], false);
    assert!(component["activity"]["stall_threshold_seconds"].is_null());
}

#[test]
fn stalled_status_publishes_a_degraded_component() {
    // Unit-level guard on the mapping itself, so a refactor cannot quietly
    // turn a stalled listener back into a green one.
    let channel_name = format!("test-publish-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");

    activity::register(
        &channel_name,
        activity::LivenessModel::Bounded(Duration::from_millis(1)),
    );
    std::thread::sleep(Duration::from_millis(20));
    publish_listener_health(&channel_name, &component_name);

    let component = component_json(&component_name);
    assert_eq!(component["status"], "degraded");
    assert_eq!(component["activity"]["stalled"], true);
}

#[test]
fn maybe_restart_daemon_systemd_args_regression() {
    assert_eq!(SYSTEMD_STATUS_ARGS, ["--user", "is-active", "prx.service"]);
    assert_eq!(SYSTEMD_RESTART_ARGS, ["--user", "restart", "prx.service"]);
}

#[test]
fn maybe_restart_daemon_openrc_args_regression() {
    assert_eq!(OPENRC_STATUS_ARGS, ["prx", "status"]);
    assert_eq!(OPENRC_RESTART_ARGS, ["prx", "restart"]);
}

// ── Runtime registry coverage for channel turns ─────────────────────────
//
// These two tests are the load-bearing evidence for taking the channel turn
// timeout out. Before it goes, a stuck turn has to be visible and killable
// (first test), and one stuck turn must not be able to stop the process from
// hearing anybody else (second test).

/// Poll `probe` until it yields a value, or give up after ~15s.
async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..600 {
        if let Some(value) = probe() {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

fn registry_probe_ctx(
    channel: Arc<dyn Channel>,
    provider: Arc<dyn Provider>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let tool_tiering = tiering_for_test_tools(tools_registry.as_ref());

    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(Config::default()),
        config_generation: crate::config::new_shared(Config::default()).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider,
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry,
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
        workspace_dir: Arc::new(std::env::temp_dir()),

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

/// Sleep duration used as an unmistakable process label, since the rest of
/// the suite shares this process-wide registry and runs in parallel.
const TURN_PROBE_SLEEP: &str = "302.17";

/// The probe tool borrows the name of a read-effect tool on purpose: this
/// harness has no durable idempotency ledger, so an `Act`-effect tool would
/// be refused before it ever runs and the turn would never reach the state
/// this test is about.
fn turn_probe_tool_call() -> String {
    r#"<tool_call>
{"name":"web_fetch","arguments":{"url":"https://example.invalid/never-returns"}}
</tool_call>"#
        .to_string()
}

struct TurnProbeProvider;

#[async_trait::async_trait]
impl Provider for TurnProbeProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(turn_probe_tool_call())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(turn_probe_tool_call())
    }
}

/// A tool that owns a real child process and never returns on its own —
/// the shape a turn takes when it wedges with side effects still running.
struct TurnProbeBlockingTool;

#[async_trait::async_trait]
impl Tool for TurnProbeBlockingTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Hold a registered child process open until the turn is killed"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let mut command = tokio::process::Command::new("sleep");
        command.arg(TURN_PROBE_SLEEP);
        let mut child = crate::runtime::shell_process::spawn_managed_shell_child(command)?;
        // The only way out is an operator kill reaching this process group,
        // which is precisely what the test measures.
        let _ = child.wait().await;
        child.mark_complete();
        Ok(ToolResult {
            success: true,
            output: "child exited".to_string(),
            error: None,
        })
    }
}

/// A provider whose answer never comes: the turn stays in flight until
/// something aborts it.
struct NeverAnsweringProvider;

#[async_trait::async_trait]
impl Provider for NeverAnsweringProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        std::future::pending::<()>().await;
        Ok(String::new())
    }
}

/// D-072 M-2, RED before: the typing refresher is a detached task that
/// stops only when its token is cancelled, and nothing cancelled it when
/// the runtime aborted the turn, so it kept sending "typing" forever.
/// After: aborting the turn stops the refresher, which sends its stop.
#[tokio::test]
async fn an_aborted_turn_stops_its_typing_refresher() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let runtime_ctx = registry_probe_ctx(channel, Arc::new(NeverAnsweringProvider), Arc::new(Vec::new()));
    let turn = tokio::spawn(run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "typing-abort-probe".to_string(),
            sender: "typing-abort-alice".to_string(),
            reply_target: "typing-abort-room".to_string(),
            content: "take your time".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            ..Default::default()
        },
        CancellationToken::new(),
    ));
    wait_for(|| (channel_impl.start_typing_calls.load(Ordering::SeqCst) > 0).then_some(()))
        .await
        .expect("test: the turn starts typing");
    assert_eq!(channel_impl.stop_typing_calls.load(Ordering::SeqCst), 0);

    // What the runtime does to a turn that outlives a generation swap.
    turn.abort();
    assert!(turn.await.is_err_and(|error| error.is_cancelled()));

    wait_for(|| (channel_impl.stop_typing_calls.load(Ordering::SeqCst) > 0).then_some(()))
        .await
        .expect("the typing refresher of an aborted turn stops and says so");
}

/// Core evidence 1: a live channel turn is a first-class registry row, and
/// killing it with cascade reaches the tool call it started and the child
/// process under that tool call.
#[tokio::test]
async fn channel_turn_is_registered_and_cascade_kill_reaches_its_tool_and_child_process() {
    use crate::runtime::registry;

    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let runtime_ctx = registry_probe_ctx(
        channel,
        Arc::new(TurnProbeProvider),
        Arc::new(vec![Box::new(TurnProbeBlockingTool) as Box<dyn Tool>]),
    );

    let turn = tokio::spawn(run_owned_channel_probe(
        runtime_ctx,
        traits::ChannelMessage {
            id: "turn-registry-probe".to_string(),
            sender: "turn-registry-probe-alice".to_string(),
            reply_target: "turn-registry-probe-room".to_string(),
            content: "hold on to something".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            ..Default::default()
        },
        CancellationToken::new(),
    ));

    // Found by the correspondent's fingerprint, not by their address: the
    // row names the sender the way the outbound path does, so this is the
    // same lookup an operator does with what `prx tasks list` shows them.
    let sender_ref = crate::security::op_id::ref_for_channel_recipient("test-channel", "turn-registry-probe-alice");
    let room_ref = crate::security::op_id::ref_for_channel_recipient("test-channel", "turn-registry-probe-room");
    let turn_row = wait_for(|| {
        registry::snapshot_all()
            .into_iter()
            .find(|row| row.kind == registry::WorkKind::Turn && row.name.contains(&sender_ref))
    })
    .await
    .expect("test: a live channel turn must be listed by the runtime registry");

    assert_eq!(turn_row.state, registry::WorkState::Running);
    assert!(
        turn_row.name.contains("test-channel") && turn_row.name.contains(&room_ref),
        "test: the turn row must identify channel, sender and conversation: {}",
        turn_row.name
    );
    assert!(
        turn_row.run_id.is_some(),
        "test: the turn row must carry the run id its stored records use"
    );

    let child_row = wait_for(|| {
        registry::snapshot_all()
            .into_iter()
            .find(|row| row.name.contains(TURN_PROBE_SLEEP))
    })
    .await
    .expect("test: the child process started inside the turn must be registered");
    assert_eq!(child_row.kind, registry::WorkKind::Process);
    assert!(
        child_row.pgid.is_some(),
        "test: the child must be registered by process group so a kill reaches its forks"
    );

    let results = registry::kill(turn_row.id, true).await;
    let kinds: Vec<registry::WorkKind> = results.iter().map(|result| result.kind).collect();
    assert!(
        kinds.contains(&registry::WorkKind::Turn),
        "test: the turn itself must be a kill target: {results:?}"
    );
    assert!(
        kinds.contains(&registry::WorkKind::ToolCall),
        "test: the turn's tool call must be reached by cascade: {results:?}"
    );
    assert!(
        kinds.contains(&registry::WorkKind::Process),
        "test: the turn's child process must be reached by cascade: {results:?}"
    );
    assert_eq!(
        results
            .iter()
            .find(|result| result.kind == registry::WorkKind::Process)
            .map(|result| result.outcome),
        Some(registry::KillOutcome::Killed),
        "test: the child process must be confirmed dead against the OS: {results:?}"
    );

    assert!(
        wait_for(|| registry::snapshot(turn_row.id).is_none().then_some(()))
            .await
            .is_some(),
        "test: a killed turn must retire from the registry instead of lingering"
    );
    let _ = tokio::time::timeout(Duration::from_secs(20), turn).await;
}

/// More stuck turns than the largest capacity the removed dispatch gate ever
/// handed out (`CHANNEL_MIN_IN_FLIGHT_MESSAGES` was 8; the system pool was
/// 30% of that).
const DISPATCH_GATE_STUCK_TURNS: usize = 12;

async fn respond_or_block_forever(is_ping: bool) -> anyhow::Result<String> {
    if is_ping {
        return Ok("PONG-PROBE".to_string());
    }
    // A turn that never finishes and is never timed out — exactly the shape
    // that used to hold a dispatch permit forever.
    let never: std::convert::Infallible = std::future::pending().await;
    match never {}
}

struct StuckUnlessPingProvider;

#[async_trait::async_trait]
impl Provider for StuckUnlessPingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        respond_or_block_forever(message.contains("PING-PROBE")).await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let is_ping = messages.iter().any(|message| message.content.contains("PING-PROBE"));
        respond_or_block_forever(is_ping).await
    }
}

/// Core evidence 2: with more permanently stuck turns in flight than the old
/// semaphore would ever have admitted, the dispatch loop still receives and
/// answers the next message. With the gate in place this deadlocked — the
/// permit was taken on the dispatch task itself, so the 9th message was
/// never even read off the bus.
#[tokio::test]
async fn dispatch_keeps_answering_while_more_turns_are_stuck_than_the_old_gate_allowed() {
    use crate::runtime::registry;

    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let runtime_ctx = registry_probe_ctx(channel, Arc::new(StuckUnlessPingProvider), Arc::new(vec![]));

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(DISPATCH_GATE_STUCK_TURNS.saturating_add(4));
    for index in 0..DISPATCH_GATE_STUCK_TURNS {
        let sender = format!("dispatch-gate-stuck-{index}");
        tx.send(traits::ChannelMessage {
            id: format!("dispatch-gate-stuck-msg-{index}"),
            sender: sender.clone(),
            reply_target: sender,
            content: "STUCK-PROBE".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            ..Default::default()
        })
        .await
        .expect("test: queue a stuck message");
    }
    tx.send(traits::ChannelMessage {
        id: "dispatch-gate-ping-msg".to_string(),
        sender: "dispatch-gate-ping".to_string(),
        reply_target: "dispatch-gate-ping".to_string(),
        content: "PING-PROBE".to_string(),
        channel: "test-channel".to_string(),
        timestamp: 2,
        ..Default::default()
    })
    .await
    .expect("test: queue the message that must still be answered");
    drop(tx);

    let dispatch = tokio::spawn(run_message_dispatch_loop(rx, runtime_ctx, CancellationToken::new()));

    let mut answered = false;
    for _ in 0..600 {
        {
            let sent = channel_impl.sent_messages.lock().await;
            if sent.iter().any(|message| message.contains("PONG-PROBE")) {
                answered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        answered,
        "test: {DISPATCH_GATE_STUCK_TURNS} wedged turns must not stop the dispatch loop from \
         receiving and answering the next message"
    );

    // The wedged turns are recognised by their senders' fingerprints — the
    // rows name correspondents the way the send path does, so a plaintext
    // prefix match would find nothing.
    let stuck_refs: Vec<String> = (0..DISPATCH_GATE_STUCK_TURNS)
        .map(|index| {
            crate::security::op_id::ref_for_channel_recipient("test-channel", &format!("dispatch-gate-stuck-{index}"))
        })
        .collect();
    let stuck_rows = wait_for(|| {
        let count = registry::snapshot_all()
            .into_iter()
            .filter(|row| {
                row.kind == registry::WorkKind::Turn && stuck_refs.iter().any(|reference| row.name.contains(reference))
            })
            .count();
        (count >= DISPATCH_GATE_STUCK_TURNS).then_some(count)
    })
    .await
    .unwrap_or_default();
    assert_eq!(
        stuck_rows, DISPATCH_GATE_STUCK_TURNS,
        "test: every wedged turn must be listed, so an operator can find and kill it"
    );

    dispatch.abort();
    let _ = dispatch.await;
}

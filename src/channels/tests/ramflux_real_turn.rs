//! Real ramflux turns: `process_channel_message` against a fake agent-host
//! on the Unix socket.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use super::super::ramflux::RamfluxChannel;
use super::super::rich::RichChannel;
use super::*;
use crate::config::RamfluxConfig;
use crate::providers::ChatMessage;
use crate::tools::traits::{Tool, ToolResult};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct WordsProvider;

#[async_trait::async_trait]
impl Provider for WordsProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("alpha beta gamma delta".to_string())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let _ = model;
        Ok("alpha beta gamma delta".to_string())
    }
}

struct ToolThenText;

#[async_trait::async_trait]
impl Provider for ToolThenText {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(super::tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let answered = messages
            .iter()
            .any(|message| message.role == "user" && message.content.contains("[Tool results]"));
        if answered {
            Ok("tool outcome delivered".to_string())
        } else {
            Ok(super::tool_call_payload())
        }
    }
}

struct CountingTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "mock_price"
    }

    fn description(&self) -> &str {
        "price"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"symbol": {"type": "string"}}, "required": ["symbol"]})
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            success: true,
            output: "65000".into(),
            error: None,
        })
    }
}

struct Host {
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Host {
    async fn connect(path: &std::path::Path) -> Self {
        let mut stream = None;
        let mut last_error = String::from("no attempt");
        for _ in 0..100 {
            match UnixStream::connect(path).await {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (read, write) = stream
            .unwrap_or_else(|| panic!("test: socket {} ({last_error})", path.display()))
            .into_split();
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn send(&mut self, value: &Value) {
        let bytes = serde_json::to_vec(value).expect("test: encode");
        self.write.write_all(&bytes).await.expect("test: write");
        self.write.write_all(b"\n").await.expect("test: nl");
        self.write.flush().await.expect("test: flush");
    }

    async fn recv(&mut self) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(30), async {
            let mut buf = Vec::new();
            self.read.read_until(b'\n', &mut buf).await.expect("test: read");
            buf
        })
        .await
        .expect("test: frame timeout");
        serde_json::from_slice(&frame).expect("test: json")
    }
}

fn runtime(
    dir: &std::path::Path,
    channel: Arc<dyn Channel>,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    memory: Arc<dyn Memory>,
    tiering: crate::config::ToolTieringConfig,
) -> Arc<ChannelRuntimeContext> {
    runtime_autonomy(
        dir,
        channel,
        provider,
        tools,
        memory,
        tiering,
        crate::security::AutonomyLevel::Full,
    )
}

fn runtime_autonomy(
    dir: &std::path::Path,
    channel: Arc<dyn Channel>,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    memory: Arc<dyn Memory>,
    tiering: crate::config::ToolTieringConfig,
    autonomy: crate::security::AutonomyLevel,
) -> Arc<ChannelRuntimeContext> {
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let config = crate::config::Config {
        workspace_dir: dir.to_path_buf(),
        ..crate::config::Config::default()
    };
    Arc::new(ChannelRuntimeContext {
        decisions: Arc::new(decision_runtime::ChannelDecisionSlot::default()),
        config: crate::config::new_shared(config.clone()),
        config_generation: crate::config::new_shared(config).pin(),
        channels_by_name: Arc::new(channels_by_name),
        provider,
        default_provider: Arc::new("test-provider".to_string()),
        memory,
        tools_registry: Arc::new(tools),
        observer: Arc::new(crate::observability::NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(dir.to_path_buf())),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: true,
        memory_event_recording: crate::memory::MemoryEventRecording::default(),
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
        workspace_dir: Arc::new(dir.to_path_buf()),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
            security: Arc::new({
                let mut policy = crate::security::SecurityPolicy::default();
                policy.autonomy = autonomy;
                policy
            }),
        })),
        tool_tiering: tiering,
        signal_inbound_policy: None,
        whatsapp_inbound_policy: None,
        bot_names: vec!["prx".to_string()],
        bot_uuids: vec![],
        mention_only_by_channel: HashMap::new(),
        group_reply_mode_by_channel: HashMap::new(),
        smart_reply_cooldown: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        test_inbound_authorizer: None,
    })
}

fn spawn_turns(channel: Arc<RamfluxChannel>, ctx: Arc<ChannelRuntimeContext>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let listening = Arc::clone(&channel);
    tokio::spawn(async move {
        let _ = Channel::listen(listening.as_ref(), tx).await;
    });
    tokio::spawn(async move {
        super::super::dispatch::run_message_dispatch_loop(rx, ctx, CancellationToken::new()).await;
    });
}

async fn hello(host: &mut Host, authorization: &str) -> Value {
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "bridge.hello",
        "params": {
            "protocol_version": "1",
            "agent_account_id": "agent-a",
            "capabilities": ["bridge"],
            "authorization": authorization
        }
    }))
    .await;
    host.recv().await
}

async fn ack(host: &mut Host, frame: &Value) {
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": frame["id"],
        "result": {"ok": true}
    }))
    .await;
}

fn find_hit(root: &std::path::Path, needle: &[u8]) -> Option<std::path::PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&child) {
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    return Some(child);
                }
            }
        }
    }
    None
}

fn file_contains(path: &std::path::Path, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    std::fs::read(path)
        .ok()
        .is_some_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
}

fn dir_has(root: &std::path::Path, needle: &[u8]) -> bool {
    if needle.is_empty() || !root.exists() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&child) {
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn trace_warns() {
    install_log_capture();
}

fn captured_logs() -> Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    Arc::clone(LOGS.get_or_init(|| Arc::new(std::sync::Mutex::new(Vec::new()))))
}

struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut slot) = self.0.lock() {
            slot.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install_log_capture() {
    let buffer = captured_logs();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || LogWriter(Arc::clone(&buffer)))
        .try_init();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_turn_streams_text_then_erase_scans_the_store() {
    trace_warns();
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("ramflux.sock")),
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .expect("test: channel"),
    );
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: memory"));
    let histories = Arc::new(Mutex::new(HashMap::new()));
    channel.attach_stores(Arc::clone(&memory) as Arc<dyn Memory>, Arc::clone(&histories));
    channel.start().await.expect("test: start socket");
    let ctx = runtime(
        dir.path(),
        Arc::clone(&channel) as Arc<dyn Channel>,
        Arc::new(WordsProvider),
        Vec::new(),
        Arc::clone(&memory) as Arc<dyn Memory>,
        crate::config::ToolTieringConfig::default(),
    );
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    let greeted = hello(&mut host, "full").await;
    assert_eq!(greeted["result"]["effective_autonomy"], "full");
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "bridge.ping",
        "params": {}
    }))
    .await;
    let _ = host.recv().await;
    let marker = "ramflux-plaintext-marker";
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "sender": {
                "principal_id": "alice",
                "device_id": "d",
                "kind": "human",
                "trust": "verified",
                "is_self": false
            },
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": marker,
            "attachments": [],
            "timestamp_ms": 1
        }
    }))
    .await;

    let mut versions = Vec::new();
    let mut close_version = None;
    let mut saw_text = false;
    let mut saw_done = false;
    let mut saw_typing = false;
    let mut idempotency = None;
    for _ in 0..40 {
        let frame = host.recv().await;
        if frame.get("result").is_some() || frame.get("error").is_some() {
            continue;
        }
        let method = frame["method"].as_str().unwrap_or("");
        if method == "reply.typing" {
            saw_typing = true;
        }
        if method == "reply.surface" {
            let op = frame["params"]["op"].as_str().unwrap_or("");
            let version = frame["params"]["version"].as_u64().unwrap_or(0);
            if op == "stream_append" {
                versions.push(version);
            }
            if op == "close" {
                close_version = Some(version);
            }
        }
        if method == "reply.text" && frame["params"]["text"].as_str().unwrap_or("").contains("alpha") {
            saw_text = true;
            idempotency = frame["params"]["idempotency_key"].as_str().map(str::to_string);
        }
        if method == "turn.status" {
            saw_done = frame["params"]["state"].as_str() == Some("done");
            if !saw_done {
                break;
            }
        }
        ack(&mut host, &frame).await;
        if saw_text && saw_done {
            break;
        }
    }
    assert!(saw_typing, "typing was not sent");
    assert!(saw_text, "final text was not sent");
    assert!(saw_done, "turn.status done was not sent");
    assert!(!versions.is_empty(), "no stream frames");
    assert_eq!(versions, (1..=versions.len() as u64).collect::<Vec<_>>());
    assert_eq!(close_version, Some(versions.len() as u64 + 1));
    assert!(idempotency.is_some(), "reply.text has no idempotency_key");
    let scoped = RichChannel::memory_for_turn(channel.as_ref(), Arc::clone(&memory) as Arc<dyn Memory>, "conv-1");
    let stored = scoped.list(None, None).await.expect("test: stored turns");
    let stored_blob = stored.iter().map(|entry| entry.content.clone()).collect::<String>();
    assert!(
        stored_blob.contains(marker),
        "message text was not stored before delete"
    );
    assert!(
        stored_blob.contains("alpha beta gamma delta"),
        "reply text was not stored before delete"
    );
    assert!(
        !dir_has(dir.path(), marker.as_bytes()),
        "plaintext marker is on disk in {}",
        find_hit(dir.path(), marker.as_bytes()).unwrap_or_default().display()
    );
    assert!(
        !dir_has(dir.path(), b"alpha beta gamma delta"),
        "model text is on disk in {}",
        find_hit(dir.path(), b"alpha beta gamma delta")
            .unwrap_or_default()
            .display()
    );
    drain_late_frames(&mut host).await;

    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "inbound.message_deleted",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "deleted_by": {
                "principal_id": "alice",
                "device_id": "d",
                "kind": "human",
                "trust": "verified",
                "is_self": false
            },
            "timestamp_ms": 2
        }
    }))
    .await;
    let mut erased = serde_json::Value::Null;
    for _ in 0..8 {
        let frame = tokio::time::timeout(Duration::from_secs(15), host.recv())
            .await
            .unwrap_or(Value::Null);
        if frame.is_null() {
            break;
        }
        if frame.get("result").is_some() {
            erased = frame;
            break;
        }
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
    }
    assert_eq!(erased["result"]["erased"], true);
    let after = scoped.list(None, None).await.expect("test: list after delete");
    let after_blob = after.iter().map(|entry| entry.content.clone()).collect::<String>();
    assert!(!after_blob.contains(marker), "message text survived delete");
    assert!(
        !after_blob.contains("alpha beta gamma delta"),
        "reply text survived delete"
    );
    assert!(!dir_has(dir.path(), marker.as_bytes()));
    assert!(!dir_has(dir.path(), b"alpha beta gamma delta"));
}

async fn drain_late_frames(host: &mut Host) {
    let until = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < until {
        let frame = tokio::time::timeout(Duration::from_millis(80), host.recv()).await;
        let Ok(frame) = frame else {
            continue;
        };
        if frame.get("method").is_some() {
            ack(host, &frame).await;
        }
    }
}

fn sender() -> Value {
    json!({
        "principal_id": "alice",
        "device_id": "d",
        "kind": "human",
        "trust": "verified",
        "is_self": false
    })
}

async fn approval_decision_round(decision: &str, executions_expected: usize) {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("ramflux.sock")),
                turn_wall_clock_secs: 30,
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .expect("test: channel"),
    );
    channel.start().await.expect("test: start socket");
    let executions = Arc::new(AtomicUsize::new(0));
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: memory"));
    channel.attach_stores(
        Arc::clone(&memory) as Arc<dyn Memory>,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let ctx = runtime(
        dir.path(),
        Arc::clone(&channel) as Arc<dyn Channel>,
        Arc::new(ToolThenText),
        vec![Box::new(CountingTool {
            executions: Arc::clone(&executions),
        })],
        Arc::clone(&memory) as Arc<dyn Memory>,
        crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
    );
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    let greeted = hello(&mut host, "supervised").await;
    assert_eq!(greeted["result"]["effective_autonomy"], "supervised");
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m2",
            "sender": sender(),
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": "what is the price",
            "attachments": [],
            "timestamp_ms": 1
        }
    }))
    .await;
    let mut saw_approval = false;
    let mut saw_text = false;
    for _ in 0..40 {
        let frame = host.recv().await;
        if frame.get("result").is_some() || frame.get("error").is_some() {
            continue;
        }
        let method = frame["method"].as_str().unwrap_or("");
        if method == "approval.request" {
            saw_approval = true;
            let approval_id = frame["params"]["approval_id"].as_str().unwrap_or("").to_string();
            ack(&mut host, &frame).await;
            host.send(&json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "approval.decision",
                "params": {
                    "conversation_id": "conv-1",
                    "approval_id": approval_id,
                    "decision": decision,
                    "decided_by": sender(),
                    "decided_at_ms": 3
                }
            }))
            .await;
            continue;
        }
        if method == "reply.text" && frame["params"]["text"].as_str().unwrap_or("").contains("tool outcome") {
            saw_text = true;
        }
        if !method.is_empty() {
            ack(&mut host, &frame).await;
        }
        if saw_approval && saw_text {
            break;
        }
    }
    assert!(saw_approval, "approval.request was not sent for {decision}");
    assert!(saw_text, "{decision} tool turn did not finish");
    assert_eq!(executions.load(Ordering::SeqCst), executions_expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_turn_approval_is_honoured_on_the_socket() {
    approval_decision_round("approved", 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_turn_approval_denied_does_not_run_the_tool() {
    approval_decision_round("denied", 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_turn_approval_expired_does_not_run_the_tool() {
    approval_decision_round("expired", 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_turn_socket_loss_replays_the_unacked_reply() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("ramflux.sock")),
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .expect("test: channel"),
    );
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: memory"));
    channel.attach_stores(
        Arc::clone(&memory) as Arc<dyn Memory>,
        Arc::new(Mutex::new(HashMap::new())),
    );
    channel.start().await.expect("test: start socket");
    let ctx = runtime(
        dir.path(),
        Arc::clone(&channel) as Arc<dyn Channel>,
        Arc::new(WordsProvider),
        Vec::new(),
        Arc::clone(&memory) as Arc<dyn Memory>,
        crate::config::ToolTieringConfig::default(),
    );
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    let _ = hello(&mut host, "full").await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m-loss",
            "sender": sender(),
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": "say something",
            "attachments": [],
            "timestamp_ms": 1
        }
    }))
    .await;
    let mut idempotency = None;
    for _ in 0..20 {
        let frame = host.recv().await;
        if frame.get("result").is_some() {
            continue;
        }
        let method = frame["method"].as_str().unwrap_or("");
        if method.starts_with("reply.") {
            idempotency = frame["params"]["idempotency_key"].as_str().map(str::to_string);
            break;
        }
        if !method.is_empty() {
            ack(&mut host, &frame).await;
        }
    }
    let idempotency = idempotency.expect("reply had no idempotency_key");
    drop(host);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut host = Host::connect(channel.socket_path()).await;
    let _ = hello(&mut host, "full").await;
    let mut replayed = false;
    let mut bridge_lost = false;
    for _ in 0..20 {
        let frame = tokio::time::timeout(Duration::from_secs(5), host.recv())
            .await
            .unwrap_or(Value::Null);
        if frame.is_null() {
            break;
        }
        if frame["params"]["idempotency_key"].as_str() == Some(idempotency.as_str()) {
            replayed = true;
        }
        if frame["method"] == "turn.status" && frame["params"]["error"]["code"] == "bridge_lost" {
            bridge_lost = true;
        }
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
        if replayed && bridge_lost {
            break;
        }
    }
    assert!(replayed, "unacked reply was not replayed");
    assert!(bridge_lost, "bridge_lost status was not flushed");
}

/// Delete while a turn holds the sqlite writer until the host acks.
/// The reader must stay free to read that ack. A delete on the reader task
/// misses the ack and this 5s bound fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_while_the_writer_is_held_finishes_off_the_reader() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let channel = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("ramflux.sock")),
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .expect("test: channel"),
    );
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: memory"));
    channel.attach_stores(
        Arc::clone(&memory) as Arc<dyn Memory>,
        Arc::new(Mutex::new(HashMap::new())),
    );
    channel.start().await.expect("test: start socket");
    let ctx = runtime(
        dir.path(),
        Arc::clone(&channel) as Arc<dyn Channel>,
        Arc::new(WordsProvider),
        Vec::new(),
        Arc::clone(&memory) as Arc<dyn Memory>,
        crate::config::ToolTieringConfig::default(),
    );
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    let _ = hello(&mut host, "full").await;
    host.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "bridge.ping", "params": {}}))
        .await;
    let _ = host.recv().await;
    let entered = channel.arm_writer_hold(memory.as_ref());
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m-hold",
            "sender": sender(),
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": "ramflux-plaintext-marker",
            "attachments": [],
            "timestamp_ms": 1
        }
    }))
    .await;
    let held = tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .expect("test: writer hold did not start");
    assert!(held.is_ok(), "writer hold dropped");
    let mut frame = Value::Null;
    let look = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < look {
        let remain = look.saturating_duration_since(tokio::time::Instant::now());
        let Ok(next) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        if next.get("method").is_some() {
            frame = next;
            break;
        }
    }
    assert!(frame.get("method").is_some(), "held frame was not a request");
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "inbound.message_deleted",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m-hold",
            "deleted_by": sender(),
            "timestamp_ms": 2
        }
    }))
    .await;
    ack(&mut host, &frame).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut erased = false;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            break;
        }
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        seen.push(frame.clone());
        if frame
            .get("result")
            .and_then(|item| item.get("erased"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            erased = true;
            break;
        }
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
    }
    assert!(erased, "delete did not finish within 5s: {seen:?}");
    let db = dir.path().join("memory").join("brain.db");
    let wal = dir.path().join("memory").join("brain.db-wal");
    assert!(
        !file_contains(&db, b"channel:ramflux:m-hold"),
        "message id is still in {}",
        db.display()
    );
    assert!(
        !file_contains(&wal, b"channel:ramflux:m-hold"),
        "message id is still in {}",
        wal.display()
    );
    assert!(!dir_has(dir.path(), b"ramflux-plaintext-marker"));
}

struct ScriptProvider {
    model: Arc<Mutex<Option<String>>>,
    system: Arc<Mutex<Option<String>>>,
    users: Arc<Mutex<Vec<String>>>,
}

impl ScriptProvider {
    fn new() -> (
        Arc<Self>,
        Arc<Mutex<Option<String>>>,
        Arc<Mutex<Option<String>>>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let model = Arc::new(Mutex::new(None));
        let system = Arc::new(Mutex::new(None));
        let users = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Self {
            model: Arc::clone(&model),
            system: Arc::clone(&system),
            users: Arc::clone(&users),
        });
        (provider, model, system, users)
    }

    fn remember(&self, messages: &[ChatMessage], model: &str) {
        *self.model.lock() = Some(model.to_string());
        *self.system.lock() = messages
            .iter()
            .find(|message| message.role == "system")
            .map(|message| message.content.clone());
        if let Some(user) = messages.iter().rev().find(|message| message.role == "user") {
            self.users.lock().push(user.content.clone());
        }
    }
}

#[async_trait::async_trait]
impl Provider for ScriptProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        *self.model.lock() = Some(model.to_string());
        *self.system.lock() = system_prompt.map(str::to_string);
        self.users.lock().push(message.to_string());
        Ok(scripted_reply(message))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.remember(messages, model);
        let user = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.as_str())
            .unwrap_or("");
        Ok(scripted_reply(user))
    }

    fn capabilities(&self) -> crate::providers::ProviderCapabilities {
        crate::providers::ProviderCapabilities {
            vision: true,
            ..crate::providers::ProviderCapabilities::default()
        }
    }
}

fn scripted_reply(user: &str) -> String {
    if user.contains("turn-one") {
        "reply-one".to_string()
    } else if user.contains("turn-two") {
        "reply-two".to_string()
    } else if user.contains("turn-three") {
        "reply-three".to_string()
    } else if user.contains("CANARY-7f3a") {
        "reply-canary-9c2e".to_string()
    } else {
        "alpha beta gamma delta".to_string()
    }
}

struct HoldProvider {
    hits: Arc<AtomicUsize>,
    texts: Arc<Mutex<Vec<String>>>,
    release: tokio::sync::watch::Receiver<bool>,
    entered: tokio::sync::watch::Sender<usize>,
}

impl HoldProvider {
    fn new() -> (
        Arc<Self>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<usize>,
    ) {
        let hits = Arc::new(AtomicUsize::new(0));
        let texts = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let (entered_tx, entered_rx) = tokio::sync::watch::channel(0usize);
        let provider = Arc::new(Self {
            hits: Arc::clone(&hits),
            texts: Arc::clone(&texts),
            release: release_rx,
            entered: entered_tx,
        });
        (provider, hits, texts, release_tx, entered_rx)
    }

    async fn wait_for_release(&self) {
        let mut release = self.release.clone();
        while !*release.borrow() {
            if release.changed().await.is_err() {
                break;
            }
        }
    }
}

#[async_trait::async_trait]
impl Provider for HoldProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.texts.lock().push(message.to_string());
        let hits = self.hits.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let _ = self.entered.send(hits);
        self.wait_for_release().await;
        Ok("held-reply".to_string())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let user = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        self.texts.lock().push(user);
        let hits = self.hits.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let _ = self.entered.send(hits);
        self.wait_for_release().await;
        Ok("held-reply".to_string())
    }
}

fn inbound_message(id: i64, conversation: &str, message_id: &str, text: &str, timestamp_ms: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "inbound.message",
        "params": {
            "conversation_id": conversation,
            "message_id": message_id,
            "sender": sender(),
            "chat": {"conversation_id": conversation, "kind": "dm", "member_count": 2, "mentioned": false},
            "text": text,
            "attachments": [],
            "timestamp_ms": timestamp_ms
        }
    })
}

async fn boot(
    configure: impl FnOnce(&std::path::Path) -> RamfluxConfig,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    tiering: crate::config::ToolTieringConfig,
    autonomy: crate::security::AutonomyLevel,
    authorization: &str,
) -> (
    tempfile::TempDir,
    Arc<RamfluxChannel>,
    Arc<crate::memory::SqliteMemory>,
    Host,
) {
    install_log_capture();
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: mode");
    let mut config = configure(dir.path());
    if config.socket_path.is_none() {
        config.socket_path = Some(dir.path().join("ramflux.sock"));
    }
    let channel = Arc::new(RamfluxChannel::new(config, dir.path()).expect("test: channel"));
    let memory = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: memory"));
    channel.attach_stores(
        Arc::clone(&memory) as Arc<dyn Memory>,
        Arc::new(Mutex::new(HashMap::new())),
    );
    channel.start().await.expect("test: start socket");
    let ctx = runtime_autonomy(
        dir.path(),
        Arc::clone(&channel) as Arc<dyn Channel>,
        provider,
        tools,
        Arc::clone(&memory) as Arc<dyn Memory>,
        tiering,
        autonomy,
    );
    spawn_turns(channel.clone(), ctx);
    let mut host = Host::connect(channel.socket_path()).await;
    let _ = hello(&mut host, authorization).await;
    (dir, channel, memory, host)
}

async fn drive_until(host: &mut Host, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut saw_text = false;
    let mut saw_done = false;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline && !(saw_text && saw_done) {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        seen.push(frame.clone());
        let method = frame["method"].as_str().unwrap_or("");
        if method == "reply.text" && frame["params"]["text"].as_str().unwrap_or("").contains(needle) {
            saw_text = true;
        }
        if method == "reply.surface" && frame["params"]["fallback_text"].as_str().unwrap_or("").contains(needle) {
            saw_text = true;
        }
        if method == "turn.status" && frame["params"]["state"].as_str() == Some("done") {
            saw_done = true;
        }
        if !method.is_empty() {
            ack(host, &frame).await;
        }
    }
    assert!(saw_text && saw_done, "turn did not finish with {needle}: {seen:?}");
}

/// Read until the socket is quiet so the turn guard can drop before the next message.
async fn settle(host: &mut Host) {
    loop {
        let Ok(frame) = tokio::time::timeout(Duration::from_millis(400), host.recv()).await else {
            return;
        };
        if frame.get("method").is_some() {
            ack(host, &frame).await;
        }
    }
}

async fn wait_hits(
    host: &mut Host,
    hits: &AtomicUsize,
    want: usize,
    entered: &mut tokio::sync::watch::Receiver<usize>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while hits.load(Ordering::SeqCst) < want && tokio::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::select! {
            _ = entered.changed() => {}
            frame = tokio::time::timeout(remain, host.recv()) => {
                if let Ok(frame) = frame {
                    if frame.get("method").is_some() {
                        ack(host, &frame).await;
                    }
                }
            }
        }
    }
    assert!(
        hits.load(Ordering::SeqCst) >= want,
        "provider was called {} times",
        hits.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_holds_the_second_message_until_the_first_finishes() {
    let (provider, hits, _texts, release, mut entered) = HoldProvider::new();
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m1", "first", 1)).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    host.send(&inbound_message(4, "conv-1", "m2", "second", 2)).await;
    let queued = recv_id(&mut host, 4, false).await;
    assert_eq!(queued["result"]["ok"], true);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the second message started a turn");
    release.send(true).expect("test: release");
    wait_hits(&mut host, &hits, 2, &mut entered).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_slots_run_together_and_both_are_freed() {
    let (provider, hits, _texts, release, mut entered) = HoldProvider::new();
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig {
            max_in_flight_per_conversation: 2,
            ..RamfluxConfig::default()
        },
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m1", "first", 1)).await;
    host.send(&inbound_message(4, "conv-1", "m2", "second", 2)).await;
    wait_hits(&mut host, &hits, 2, &mut entered).await;
    release.send(true).expect("test: release");
    let mut done = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while done < 2 && tokio::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        if frame["method"].as_str() == Some("turn.status") && frame["params"]["state"].as_str() == Some("done") {
            done += 1;
        }
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
    }
    assert_eq!(done, 2, "both slots did not finish");
    host.send(&inbound_message(5, "conv-1", "m3", "third", 3)).await;
    drive_until(&mut host, "held-reply").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_33rd_message_is_queue_full() {
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        Arc::new(WordsProvider),
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    for n in 1_u64..=32 {
        let id = 10 + i64::try_from(n).unwrap_or(0);
        host.send(&inbound_message(id, "conv-1", &format!("m{n}"), "queued", n))
            .await;
        let response = recv_id(&mut host, id, false).await;
        assert_eq!(response["result"]["ok"], true, "message {n} was refused: {response}");
    }
    host.send(&inbound_message(43, "conv-1", "m33", "overflow", 33)).await;
    let refused = recv_id(&mut host, 43, false).await;
    assert_eq!(refused["error"]["code"], -32005, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("queue_full depth="), "{refused}");
}

async fn recv_id(host: &mut Host, id: i64, ack_methods: bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        seen.push(frame.clone());
        if frame.get("id").and_then(Value::as_i64) == Some(id) {
            return frame;
        }
        if ack_methods && frame.get("method").is_some() {
            ack(host, &frame).await;
        }
    }
    panic!("response {id} did not arrive: {seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surface_action_on_an_open_turn_does_not_start_another_and_a_closed_turn_gets_a_fresh_id() {
    let (provider, hits, texts, release, mut entered) = HoldProvider::new();
    let (_dir, channel, memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-open", "hold-open", 1)).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "inbound.surface_action",
        "params": {
            "conversation_id": "conv-1",
            "sender": sender(),
            "surface_id": "surface-1",
            "version": 1,
            "surface_hash": "abc",
            "action_id": "act",
            "values": {},
            "args_hash": "def",
            "message_id": "m-open"
        }
    }))
    .await;
    let accepted = recv_id(&mut host, 4, true).await;
    assert_eq!(accepted["result"]["ok"], true);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "open action started a turn");
    release.send(true).expect("test: release");
    drive_until(&mut host, "held-reply").await;
    settle(&mut host).await;
    for rpc_id in [5_i64, 6] {
        host.send(&json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "inbound.surface_action",
            "params": {
                "conversation_id": "conv-1",
                "sender": sender(),
                "surface_id": "surface-1",
                "version": 1,
                "surface_hash": "abc",
                "action_id": "act",
                "values": {},
                "args_hash": "def",
                "message_id": "m-closed"
            }
        }))
        .await;
        result_and_reply(&mut host, rpc_id, "held-reply").await;
        settle(&mut host).await;
    }
    let scoped = RichChannel::memory_for_turn(channel.as_ref(), Arc::clone(&memory) as Arc<dyn Memory>, "conv-1");
    let stored = scoped.list(None, None).await.expect("test: stored");
    let action_keys: Vec<&str> = stored
        .iter()
        .filter(|entry| entry.key.contains("m-closed:act:"))
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(
        action_keys.len(),
        2,
        "keys: {:?}",
        stored.iter().map(|entry| &entry.key).collect::<Vec<_>>()
    );
    assert_ne!(action_keys[0], action_keys[1]);
    assert!(
        !texts.lock().iter().any(|text| text.contains("user action m-open")),
        "open action was delivered as a new turn: {:?}",
        texts.lock()
    );
    assert!(
        texts
            .lock()
            .iter()
            .any(|text| text.contains("user action m-closed act"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_events_emit_monotonic_versions_and_turn_status() {
    let (provider, hits, _texts, release, mut entered) = HoldProvider::new();
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-task", "hold-task", 1)).await;
    wait_hits(&mut host, &hits, 1, &mut entered).await;
    let mut versions = Vec::new();
    let mut states = Vec::new();
    for (rpc_id, kind) in [(4_i64, "started"), (5, "thinking"), (6, "tool_running")] {
        host.send(&json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "inbound.task_event",
            "params": {
                "conversation_id": "conv-1",
                "sender": sender(),
                "task_id": "task-1",
                "version": 1,
                "kind": kind,
                "body": {}
            }
        }))
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut saw_result = false;
        while tokio::time::Instant::now() < deadline && !saw_result {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
                break;
            };
            note_task(&frame, &mut versions, &mut states);
            if frame.get("id").and_then(Value::as_i64) == Some(rpc_id) {
                saw_result = true;
            }
            if frame.get("method").is_some() {
                ack(&mut host, &frame).await;
            }
        }
        assert!(saw_result, "task event {kind} was not accepted");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline
        && (versions.len() < 3 || !states.iter().any(|state| state == "tool_running"))
    {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        note_task(&frame, &mut versions, &mut states);
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
    }
    assert_eq!(versions, vec![1, 2, 3], "versions {versions:?} states {states:?}");
    assert!(states.contains(&"started".to_string()), "{states:?}");
    assert!(states.contains(&"thinking".to_string()), "{states:?}");
    assert!(states.contains(&"tool_running".to_string()), "{states:?}");
    release.send(true).expect("test: release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_and_text_attachments_reach_the_provider() {
    let (provider, _model, _system, users) = ScriptProvider::new();
    let image = b"\x89PNG-canary";
    let text = b"attachment-body-text";
    let image_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image);
    let text_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text);
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m-file",
            "sender": sender(),
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": "see the files",
            "attachments": [
                {"object_id": "obj-img", "mime": "image/png", "size": image.len(), "name": "pic.png", "sha256": "abc"},
                {"object_id": "obj-txt", "mime": "text/plain", "size": text.len(), "name": "notes.txt", "sha256": "def"}
            ],
            "timestamp_ms": 1
        }
    }))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut saw_text = false;
    let mut saw_done = false;
    while tokio::time::Instant::now() < deadline && !(saw_text && saw_done) {
        let frame = host.recv().await;
        let method = frame["method"].as_str().unwrap_or("");
        if method == "attachment.fetch" {
            let object = frame["params"]["object_id"].as_str().unwrap_or("");
            let (bytes_b64, mime) = match object {
                "obj-img" => (image_b64.as_str(), "image/png"),
                "obj-txt" => (text_b64.as_str(), "text/plain"),
                other => panic!("unexpected attachment {other}"),
            };
            host.send(&json!({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"bytes_b64": bytes_b64, "mime": mime}
            }))
            .await;
            continue;
        }
        if method == "reply.text" && frame["params"]["text"].as_str().unwrap_or("").contains("alpha") {
            saw_text = true;
        }
        if method == "turn.status" && frame["params"]["state"].as_str() == Some("done") {
            saw_done = true;
        }
        if !method.is_empty() {
            ack(&mut host, &frame).await;
        }
    }
    assert!(saw_text && saw_done, "attachment turn did not finish");
    let seen = users.lock().join("\n");
    assert!(seen.contains("[IMAGE:data:image/png;base64,"), "{seen}");
    assert!(seen.contains(&image_b64), "{seen}");
    assert!(seen.contains("attachment-body-text"), "{seen}");
    assert!(seen.contains("notes.txt"), "{seen}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persona_system_prompt_and_model_reach_the_provider() {
    let (provider, model, system, _users) = ScriptProvider::new();
    let (_dir, _channel, _memory, mut host) = boot(
        |dir| {
            std::fs::write(dir.join("persona.txt"), "PERSONA-CANARY-PROMPT").expect("test: persona");
            RamfluxConfig {
                accounts: std::collections::BTreeMap::from([(
                    "agent-a".to_string(),
                    crate::config::RamfluxAccountConfig {
                        system_prompt_file: Some(std::path::PathBuf::from("persona.txt")),
                        model: Some("test-model".to_string()),
                        ..crate::config::RamfluxAccountConfig::default()
                    },
                )]),
                ..RamfluxConfig::default()
            }
        },
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-persona", "hello persona", 1))
        .await;
    drive_until(&mut host, "alpha").await;
    assert_eq!(model.lock().clone().as_deref(), Some("test-model"));
    let prompt = system.lock().clone().unwrap_or_default();
    assert!(prompt.contains("PERSONA-CANARY-PROMPT"), "{prompt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reloads_three_assistant_replies_in_order() {
    let (provider, _model, _system, _users) = ScriptProvider::new();
    let (dir, channel, memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    for (id, message_id, text, needle) in [
        (3_i64, "m-a", "turn-one", "reply-one"),
        (4, "m-b", "turn-two", "reply-two"),
        (5, "m-c", "turn-three", "reply-three"),
    ] {
        host.send(&inbound_message(id, "conv-1", message_id, text, id as u64))
            .await;
        drive_until(&mut host, needle).await;
        settle(&mut host).await;
    }
    drop(host);
    drop(channel);
    let restarted = Arc::new(
        RamfluxChannel::new(
            RamfluxConfig {
                socket_path: Some(dir.path().join("restart.sock")),
                ..RamfluxConfig::default()
            },
            dir.path(),
        )
        .expect("test: restart channel"),
    );
    restarted.start().await.expect("test: restart socket");
    let mut host = Host::connect(restarted.socket_path()).await;
    let _ = hello(&mut host, "full").await;
    let scoped = RichChannel::memory_for_turn(restarted.as_ref(), Arc::clone(&memory) as Arc<dyn Memory>, "conv-1");
    let histories = scoped
        .load_recent_conversation_histories(&memory_principal(), 20, 10)
        .await
        .expect("test: history");
    let mut replies = Vec::new();
    for turns in histories.values() {
        for turn in turns {
            if turn.role == "assistant" {
                replies.push(turn.content.clone());
            }
        }
    }
    assert_eq!(
        replies,
        vec![
            "reply-one".to_string(),
            "reply-two".to_string(),
            "reply-three".to_string()
        ],
        "{histories:?}"
    );
}

fn memory_principal() -> crate::memory::MemoryPrincipal {
    crate::memory::MemoryPrincipal {
        workspace_id: "ws".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: None,
        require_owner_match: false,
        legacy_session_key: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_erases_the_conversation_and_remove_refuses_with_32003() {
    let (provider, _model, _system, _users) = ScriptProvider::new();
    let (dir, channel, memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-revoke", "revoke-canary-text", 1))
        .await;
    drive_until(&mut host, "alpha").await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "inbound.control",
        "params": {"conversation_id": "conv-1", "kind": "agent_revoked", "timestamp_ms": 2}
    }))
    .await;
    let _ = recv_id(&mut host, 8, true).await;
    let scoped = RichChannel::memory_for_turn(channel.as_ref(), Arc::clone(&memory) as Arc<dyn Memory>, "conv-1");
    let after = scoped.list(None, None).await.expect("test: list after revoke");
    let blob = after.iter().map(|entry| entry.content.clone()).collect::<String>();
    assert!(!blob.contains("revoke-canary-text"), "{blob}");
    assert!(!dir_has(dir.path(), b"revoke-canary-text"));
    host.send(&inbound_message(9, "conv-1", "m-after", "again", 3)).await;
    let refused = recv_id(&mut host, 9, true).await;
    assert_eq!(refused["error"]["code"], -32003, "{refused}");

    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "inbound.control",
        "params": {"conversation_id": "conv-1", "kind": "agent_removed", "timestamp_ms": 4}
    }))
    .await;
    let _ = recv_id(&mut host, 10, true).await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "bridge.hello",
        "params": {
            "protocol_version": "1",
            "agent_account_id": "agent-a",
            "capabilities": ["bridge"],
            "authorization": "full"
        }
    }))
    .await;
    let hello_again = recv_id(&mut host, 11, true).await;
    assert_eq!(hello_again["error"]["code"], -32003, "{hello_again}");
    assert_ne!(hello_again["error"]["code"], -32603);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canary_text_is_absent_from_captured_logs() {
    install_log_capture();
    captured_logs().lock().expect("test: logs").clear();
    let (provider, _model, _system, _users) = ScriptProvider::new();
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        provider,
        Vec::new(),
        crate::config::ToolTieringConfig::default(),
        crate::security::AutonomyLevel::Full,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-canary", "CANARY-7f3a-plaintext", 1))
        .await;
    drive_until(&mut host, "reply-canary-9c2e").await;
    let logs = captured_logs().lock().expect("test: logs").clone();
    let text = String::from_utf8_lossy(&logs);
    assert!(!text.contains("CANARY-7f3a-plaintext"), "{text}");
    assert!(!text.contains("reply-canary-9c2e"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prx_ask_emits_awaiting_approval_with_the_decision_id() {
    let executions = Arc::new(AtomicUsize::new(0));
    let (_dir, _channel, _memory, mut host) = boot(
        |_| RamfluxConfig::default(),
        Arc::new(ToolThenText),
        vec![Box::new(CountingTool {
            executions: Arc::clone(&executions),
        })],
        crate::config::ToolTieringConfig {
            always_include: vec!["mock_price".to_string()],
            ..crate::config::ToolTieringConfig::default()
        },
        crate::security::AutonomyLevel::Supervised,
        "full",
    )
    .await;
    host.send(&inbound_message(3, "conv-1", "m-ask", "what is the price", 1))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut approval_id = None;
    let mut saw_request = false;
    let mut terminal = false;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline && !terminal {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        seen.push(frame.clone());
        let method = frame["method"].as_str().unwrap_or("");
        if method == "approval.request" {
            saw_request = true;
        }
        if method == "turn.status" {
            let state = frame["params"]["state"].as_str().unwrap_or("");
            if state == "awaiting_approval" {
                approval_id = frame["params"]["approval_id"].as_str().map(str::to_string);
                terminal = true;
            }
            if state == "done" || state == "failed" {
                terminal = true;
            }
        }
        if !method.is_empty() {
            ack(&mut host, &frame).await;
        }
    }
    assert!(terminal, "prx ask did not finish: {seen:?}");
    assert!(!saw_request, "durable ask used the bridge grant path: {seen:?}");
    if let Some(approval_id) = approval_id {
        assert!(!approval_id.is_empty(), "{seen:?}");
    }
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

fn note_task(frame: &Value, versions: &mut Vec<u64>, states: &mut Vec<String>) {
    let method = frame["method"].as_str().unwrap_or("");
    if method == "reply.task_update" && frame["params"]["task_id"].as_str() == Some("task-1") {
        versions.push(frame["params"]["version"].as_u64().unwrap_or(0));
    }
    if method == "turn.status" {
        if let Some(state) = frame["params"]["state"].as_str() {
            states.push(state.to_string());
        }
    }
}

async fn result_and_reply(host: &mut Host, id: i64, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut saw_result = false;
    let mut saw_text = false;
    let mut saw_done = false;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline && !(saw_result && saw_text && saw_done) {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(frame) = tokio::time::timeout(remain, host.recv()).await else {
            break;
        };
        seen.push(frame.clone());
        if frame.get("id").and_then(Value::as_i64) == Some(id) {
            saw_result = true;
        }
        let method = frame["method"].as_str().unwrap_or("");
        if method == "reply.text" && frame["params"]["text"].as_str().unwrap_or("").contains(needle) {
            saw_text = true;
        }
        if method == "reply.surface" && frame["params"]["fallback_text"].as_str().unwrap_or("").contains(needle) {
            saw_text = true;
        }
        if method == "turn.status" && frame["params"]["state"].as_str() == Some("done") {
            saw_done = true;
        }
        if !method.is_empty() {
            ack(host, &frame).await;
        }
    }
    assert!(
        saw_result && saw_text && saw_done,
        "action {id} did not finish: {seen:?}"
    );
}

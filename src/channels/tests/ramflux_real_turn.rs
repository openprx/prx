//! Real ramflux turns: `process_channel_message` against a fake agent-host
//! on the Unix socket.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use super::super::dispatch::normalize_legacy_channel_fixture;
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
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);
    let mut config = crate::config::Config {
        workspace_dir: dir.to_path_buf(),
        ..crate::config::Config::default()
    };
    config.causal_tree.enabled = false;
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
            security: Arc::new(crate::security::SecurityPolicy::default()),
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
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let listening = Arc::clone(&channel);
    tokio::spawn(async move {
        let _ = Channel::listen(listening.as_ref(), tx).await;
    });
    tokio::spawn(async move {
        let ctx = normalize_legacy_channel_fixture(ctx);
        while let Some(message) = rx.recv().await {
            super::run_owned_channel_probe(Arc::clone(&ctx), message, CancellationToken::new()).await;
        }
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
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
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
        if saw_text && saw_done && close_version.is_some() {
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
    assert!(!dir_has(dir.path(), marker.as_bytes()), "plaintext marker is on disk");
    assert!(!dir_has(dir.path(), b"alpha beta gamma delta"), "model text is on disk");

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
        let frame = host.recv().await;
        if frame.get("result").is_some() {
            erased = frame;
            break;
        }
        if frame.get("method").is_some() {
            ack(&mut host, &frame).await;
        }
    }
    assert_eq!(erased["result"]["erased"], true);
    assert!(!dir_has(dir.path(), marker.as_bytes()));
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

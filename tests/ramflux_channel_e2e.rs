//! Fake agent-host driving a ramflux channel over a Unix socket.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use openprx::agent::{ApprovalDecision, ApprovalResolver};
use openprx::approval::{ApprovalRequest, ApprovalRisk};
use openprx::channels::RamfluxChannel;
use openprx::channels::rich::{
    RichChannel, RichInbound, RichReply, SurfaceOp, SurfaceReply, TaskStep, TaskUpdateReply, TextReply, ToolEvent,
    ToolEventKind,
};
use openprx::channels::traits::Channel;
use openprx::config::RamfluxConfig;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Host {
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
}

#[tokio::test]
async fn scripted_turn_approval_cancel_and_erase() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: dir");
    let channel = RamfluxChannel::new(
        RamfluxConfig {
            socket_path: Some(dir.path().join("ramflux.sock")),
            ..RamfluxConfig::default()
        },
        dir.path(),
    )
    .expect("test: channel");
    channel.start().await.expect("test: start");
    let mut host = connect(channel.socket_path()).await;
    let hello = request(
        &mut host,
        1,
        "bridge.hello",
        json!({
            "protocol_version": "1",
            "agent_account_id": "agent-a",
            "capabilities": ["bridge"],
            "authorization": "full"
        }),
    )
    .await;
    assert_eq!(hello["result"]["effective_autonomy"], "full");

    let listening = channel.clone();
    let (tx, mut inbound) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let _ = listening.listen_rich(tx).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = request(&mut host, 2, "inbound.message", message("conv-1", "m1", "hello")).await;
    let event = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
        .await
        .expect("test: event")
        .expect("test: message");
    assert!(matches!(event, RichInbound::Message(_)));

    let append = SurfaceReply {
        conversation_id: "conv-1".into(),
        turn_id: "turn-m1".into(),
        op: SurfaceOp::StreamAppend,
        surface_id: "stream-turn-m1".into(),
        version: 1,
        base_hash: None,
        blocks: None,
        actions: None,
        ops: None,
        block_id: None,
        text_delta: Some("Hel".into()),
        is_final: Some(false),
        fallback_text: Some("Hel".into()),
    };
    let sending = channel.clone();
    let append_task = tokio::spawn(async move { sending.send_rich(&RichReply::Surface(append)).await });
    let frame = next_method(&mut host, "reply.surface").await;
    assert_eq!(frame["params"]["op"], "stream_append");
    assert_eq!(frame["params"]["version"], 1);
    ack(&mut host, &frame).await;
    append_task.await.expect("test: join").expect("test: append");

    let tool_channel = channel.clone();
    let tool_task = tokio::spawn(async move {
        tool_channel
            .on_tool_event(&ToolEvent {
                kind: ToolEventKind::Started,
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                iteration: 0,
                duration_ms: None,
                summary: Some("args_len=0".into()),
            })
            .await;
        tool_channel
            .on_tool_event(&ToolEvent {
                kind: ToolEventKind::Finished,
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                iteration: 0,
                duration_ms: Some(4),
                summary: None,
            })
            .await;
    });
    let started = next_method(&mut host, "tool.event").await;
    assert_eq!(started["params"]["kind"], "started");
    ack(&mut host, &started).await;
    let finished = next_method(&mut host, "tool.event").await;
    assert_eq!(finished["params"]["kind"], "finished");
    ack(&mut host, &finished).await;
    tool_task.await.expect("test: tools");

    let update = TaskUpdateReply {
        conversation_id: "conv-1".into(),
        turn_id: "turn-m1".into(),
        task_id: "task-1".into(),
        version: 1,
        state: "running".into(),
        steps: vec![TaskStep {
            id: "step".into(),
            title: "shell".into(),
            state: "done".into(),
            needs_me: None,
        }],
        result_ref: None,
        result_summary: Some("ok".into()),
    };
    let sending = channel.clone();
    let update_task = tokio::spawn(async move { sending.send_rich(&RichReply::TaskUpdate(update)).await });
    let frame = next_method(&mut host, "reply.task_update").await;
    ack(&mut host, &frame).await;
    update_task.await.expect("test: join").expect("test: task update");

    let sending = channel.clone();
    let close_task = tokio::spawn(async move {
        sending
            .send_rich(&RichReply::Surface(SurfaceReply {
                conversation_id: "conv-1".into(),
                turn_id: "turn-m1".into(),
                op: SurfaceOp::Close,
                surface_id: "stream-turn-m1".into(),
                version: 2,
                base_hash: None,
                blocks: None,
                actions: None,
                ops: None,
                block_id: None,
                text_delta: None,
                is_final: Some(true),
                fallback_text: Some("Hello".into()),
            }))
            .await
    });
    let frame = next_method(&mut host, "reply.surface").await;
    assert_eq!(frame["params"]["op"], "close");
    ack(&mut host, &frame).await;
    close_task.await.expect("test: join").expect("test: close");

    let sending = channel.clone();
    let text_task = tokio::spawn(async move {
        sending
            .send_rich(&RichReply::Text(TextReply {
                conversation_id: "conv-1".into(),
                turn_id: "turn-m1".into(),
                text: "Hello".into(),
                is_final: true,
                reply_to: None,
            }))
            .await
    });
    let frame = next_method(&mut host, "reply.text").await;
    assert_eq!(frame["params"]["final"], true);
    ack(&mut host, &frame).await;
    text_task.await.expect("test: join").expect("test: text");

    let token = tokio_util::sync::CancellationToken::new();
    channel
        .as_rich()
        .expect("test: rich")
        .bind_turn("conv-1", "turn-m1", token.clone());
    let removed = request(
        &mut host,
        20,
        "inbound.control",
        json!({"conversation_id": "conv-1", "kind": "agent_removed", "timestamp_ms": 5}),
    )
    .await;
    assert_eq!(removed["result"]["ok"], true);
    assert!(token.is_cancelled());

    let data = dir.path().join("ramflux");
    assert!(!contains_bytes(&data, b"hello"), "plaintext was written to disk");
    let cipher = read_named(&data, "m1").expect("test: ciphertext");
    let erased = request(
        &mut host,
        21,
        "inbound.message_deleted",
        json!({
            "conversation_id": "conv-1",
            "message_id": "m1",
            "deleted_by": sender(),
            "timestamp_ms": 6
        }),
    )
    .await;
    assert_eq!(erased["result"]["erased"], true);
    assert!(!contains_bytes(&data, b"hello"));
    assert!(!contains_bytes(&data, &cipher), "ciphertext survived erase");
}

#[tokio::test]
async fn supervised_approval_approved_denied_and_expired() {
    let dir = tempfile::tempdir().expect("test: temp");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("test: dir");
    let channel = RamfluxChannel::new(
        RamfluxConfig {
            socket_path: Some(dir.path().join("ramflux.sock")),
            ..RamfluxConfig::default()
        },
        dir.path(),
    )
    .expect("test: channel");
    channel.start().await.expect("test: start");
    let mut host = connect(channel.socket_path()).await;
    let hello = request(
        &mut host,
        1,
        "bridge.hello",
        json!({
            "protocol_version": "1",
            "agent_account_id": "agent-a",
            "capabilities": ["bridge"],
            "authorization": "supervised"
        }),
    )
    .await;
    assert_eq!(hello["result"]["effective_autonomy"], "supervised");
    let _ = request(
        &mut host,
        2,
        "inbound.message",
        message("conv-1", "m-appr", "need a tool"),
    )
    .await;
    let resolver = channel
        .as_rich()
        .expect("test: rich")
        .approval_resolver()
        .expect("test: resolver");

    let pending = spawn_decision(&resolver, None);
    let frame = next_method(&mut host, "approval.request").await;
    assert_eq!(frame["params"]["risk"], "high");
    assert!(
        !frame["params"]["arguments_redacted"]
            .to_string()
            .contains("sk-secret-value")
    );
    ack(&mut host, &frame).await;
    host.send(&decision(3, &frame["params"]["approval_id"], "approved"))
        .await;
    let outcome = pending.await.expect("test: join");
    assert!(matches!(outcome, ApprovalDecision::Grant));

    let pending = spawn_decision(&resolver, None);
    let frame = next_method(&mut host, "approval.request").await;
    ack(&mut host, &frame).await;
    host.send(&decision(4, &frame["params"]["approval_id"], "denied")).await;
    let outcome = pending.await.expect("test: join");
    assert!(matches!(outcome, ApprovalDecision::Deny));

    let pending = spawn_decision(&resolver, Some(1));
    let frame = next_method(&mut host, "approval.request").await;
    ack(&mut host, &frame).await;
    let outcome = pending.await.expect("test: join");
    assert!(matches!(outcome, ApprovalDecision::Deny));
}

fn spawn_decision(
    resolver: &std::sync::Arc<dyn ApprovalResolver>,
    expires_at_ms: Option<u64>,
) -> tokio::task::JoinHandle<ApprovalDecision> {
    let resolver = std::sync::Arc::clone(resolver);
    tokio::spawn(async move { resolver.resolve(&tool_request(expires_at_ms), "ramflux").await })
}

fn tool_request(expires_at_ms: Option<u64>) -> ApprovalRequest {
    let mut request = ApprovalRequest::new("shell", json!({"command": "echo", "token": "sk-secret-value"}));
    request.risk = Some(ApprovalRisk::High);
    request.expires_at_ms = expires_at_ms;
    request
}

fn decision(id: u64, approval_id: &Value, decision: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "approval.decision",
        "params": {
            "conversation_id": "conv-1",
            "approval_id": approval_id,
            "decision": decision,
            "decided_by": sender(),
            "decided_at_ms": 10
        }
    })
}

async fn connect(path: &Path) -> Host {
    let stream = UnixStream::connect(path).await.expect("test: connect");
    let (read, write) = stream.into_split();
    Host {
        read: BufReader::new(read),
        write,
    }
}

async fn request(host: &mut Host, id: u64, method: &str, params: Value) -> Value {
    host.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        .await;
    host.recv().await
}

fn message(conversation: &str, id: &str, text: &str) -> Value {
    json!({
        "conversation_id": conversation,
        "message_id": id,
        "sender": sender(),
        "chat": {"conversation_id": conversation, "kind": "dm", "member_count": 2, "mentioned": false},
        "text": text,
        "timestamp_ms": 1
    })
}

fn sender() -> Value {
    json!({
        "principal_id": "human-1",
        "device_id": "device-1",
        "kind": "human",
        "trust": "verified",
        "is_self": false
    })
}

impl Host {
    async fn send(&mut self, value: &Value) {
        let bytes = serde_json::to_vec(value).expect("test: encode");
        self.write.write_all(&bytes).await.expect("test: write");
        self.write.write_all(b"\n").await.expect("test: nl");
        self.write.flush().await.expect("test: flush");
    }

    async fn recv(&mut self) -> Value {
        let mut buf = Vec::new();
        self.read.read_until(b'\n', &mut buf).await.expect("test: read");
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        serde_json::from_slice(&buf).expect("test: json")
    }
}

async fn next_method(host: &mut Host, method: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = host.recv().await;
            if value["method"] == method {
                return value;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("test: missing {method}"))
}

async fn ack(host: &mut Host, frame: &Value) {
    host.send(&json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"ok": true}}))
        .await;
}

fn contains_bytes(root: &Path, needle: &[u8]) -> bool {
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
            if let Ok(bytes) = std::fs::read(&child)
                && bytes.windows(needle.len()).any(|window| window == needle)
            {
                return true;
            }
        }
    }
    false
}

fn read_named(root: &Path, name: &str) -> Option<Vec<u8>> {
    if !root.exists() {
        return None;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = std::fs::read_dir(&path).ok()?;
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                stack.push(child);
                continue;
            }
            if child.file_name().and_then(|file| file.to_str()) == Some(name) {
                return std::fs::read(&child).ok();
            }
        }
    }
    None
}

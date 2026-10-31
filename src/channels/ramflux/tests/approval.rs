use super::super::approval::ApprovalOutcome;
use super::Host;
use crate::approval::{ApprovalRequest, ApprovalRisk};
use crate::channels::rich_turn;
use crate::config::{RamfluxAuthorizationMode, RamfluxConfig};
use serde_json::{Value, json};
use std::time::Duration;

fn request() -> ApprovalRequest {
    let mut request = ApprovalRequest::new("shell", json!({"command": "echo hi", "api_key": "sk1234567890abcdef"}));
    request.risk = Some(ApprovalRisk::High);
    request
}

fn human() -> Value {
    json!({
        "principal_id": "human-1",
        "device_id": "device-1",
        "kind": "human",
        "trust": "verified",
        "is_self": false
    })
}

async fn supervised_host() -> Host {
    let mut config = RamfluxConfig::default();
    config.authorization_mode = RamfluxAuthorizationMode::FollowRf;
    config.turn_wall_clock_secs = 2;
    let mut host = Host::connect(config).await;
    let ack = host.hello("supervised").await;
    assert_eq!(ack["result"]["effective_autonomy"], "supervised");
    host
}

fn spawn_outcome(
    gate: std::sync::Arc<super::super::approval::ApprovalGate>,
    conversation_id: &'static str,
    turn_id: &'static str,
    request: ApprovalRequest,
) -> tokio::task::JoinHandle<ApprovalOutcome> {
    tokio::spawn(async move { rich_turn::scope(conversation_id, turn_id, gate.resolve_outcome(&request)).await })
}

async fn approval_frame(host: &mut Host) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = host.recv().await;
            if value["method"] == "approval.request" {
                return value;
            }
        }
    })
    .await
    .expect("test: approval.request")
}

async fn ack(host: &mut Host, frame: &Value) {
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": frame["id"],
        "result": {"ok": true}
    }))
    .await;
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
            "decided_by": human(),
            "decided_at_ms": 10
        }
    })
}

#[tokio::test]
async fn supervised_approved_denied_and_expired() {
    let mut host = supervised_host().await;
    let pending = spawn_outcome(host.channel.approval_gate(), "conv-1", "turn-1", request());
    let frame = approval_frame(&mut host).await;
    let redacted = frame["params"]["arguments_redacted"].to_string();
    assert!(!redacted.contains("sk1234567890abcdef"), "{redacted}");
    assert_eq!(frame["params"]["args_hash"].as_str().expect("test: hash").len(), 64);
    assert_eq!(frame["params"]["risk"], "high");
    ack(&mut host, &frame).await;
    host.send(&decision(9, &frame["params"]["approval_id"], "approved"))
        .await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("test: join")
        .expect("test: task");
    assert_eq!(outcome, ApprovalOutcome::Approved);

    let mut host = supervised_host().await;
    let pending = spawn_outcome(host.channel.approval_gate(), "conv-1", "turn-2", request());
    let frame = approval_frame(&mut host).await;
    ack(&mut host, &frame).await;
    host.send(&decision(10, &frame["params"]["approval_id"], "denied"))
        .await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("test: join")
        .expect("test: task");
    assert_eq!(outcome, ApprovalOutcome::Denied);

    let mut expiring = request();
    expiring.expires_at_ms = Some(1);
    let mut host = supervised_host().await;
    let pending = spawn_outcome(host.channel.approval_gate(), "conv-1", "turn-3", expiring);
    let frame = approval_frame(&mut host).await;
    ack(&mut host, &frame).await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("test: join")
        .expect("test: task");
    assert_eq!(outcome, ApprovalOutcome::Cancelled);
}

#[tokio::test]
async fn chat_text_is_not_approval_and_full_mode_skips_the_wire() {
    let mut host = supervised_host().await;
    let mut pending = spawn_outcome(host.channel.approval_gate(), "conv-1", "turn-4", request());
    let frame = approval_frame(&mut host).await;
    ack(&mut host, &frame).await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "sender": human(),
            "chat": {"conversation_id": "conv-1", "kind": "dm", "member_count": 2, "mentioned": false},
            "text": "yes approved",
            "timestamp_ms": 12
        }
    }))
    .await;
    let still_waiting = tokio::time::timeout(Duration::from_millis(80), &mut pending).await;
    assert!(still_waiting.is_err(), "test: chat text completed an approval");
    host.send(&decision(12, &frame["params"]["approval_id"], "denied"))
        .await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("test: join")
        .expect("test: task");
    assert_eq!(outcome, ApprovalOutcome::Denied);

    let mut config = RamfluxConfig::default();
    config.authorization_mode = RamfluxAuthorizationMode::FollowRf;
    let mut host = Host::connect(config).await;
    host.hello("full").await;
    let pending = spawn_outcome(host.channel.approval_gate(), "conv-1", "turn-5", request());
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("test: join")
        .expect("test: task");
    assert_eq!(outcome, ApprovalOutcome::Approved);
    let extra = tokio::time::timeout(Duration::from_millis(100), host.recv()).await;
    assert!(extra.is_err(), "test: full mode put an approval on the wire");
}

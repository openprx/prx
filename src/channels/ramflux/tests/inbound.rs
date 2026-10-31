use super::Host;
use crate::channels::rich::RichChannel;
use crate::channels::traits::{Channel, SendMessage};
use crate::config::RamfluxConfig;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn sender() -> serde_json::Value {
    json!({
        "principal_id": "human-1",
        "device_id": "device-1",
        "kind": "human",
        "trust": "verified",
        "is_self": false
    })
}

#[tokio::test]
async fn text_reply_and_message_round_trip() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.hello("supervised").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listening = host.channel.clone();
    tokio::spawn(async move {
        let _ = listening.listen_rich(tx).await;
    });
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "sender": sender(),
            "chat": {
                "conversation_id": "conv-1",
                "kind": "group",
                "title": "room",
                "member_count": 3,
                "mentioned": true
            },
            "text": "hello bridge",
            "timestamp_ms": 20,
            "idempotency_key": "msg-1"
        }
    }))
    .await;
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("test: inbound")
        .expect("test: event");
    let crate::channels::rich::RichInbound::Message(message) = event else {
        panic!("test: expected a message");
    };
    let channel_message = message.to_channel_message();
    assert_eq!(channel_message.sender, "human-1");
    assert_eq!(channel_message.reply_target, "conv-1");
    assert_eq!(channel_message.content, "hello bridge");
    assert!(channel_message.is_group_hint);
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "sender": sender(),
            "chat": {
                "conversation_id": "conv-1",
                "kind": "dm",
                "member_count": 2,
                "mentioned": false
            },
            "text": "hello bridge",
            "timestamp_ms": 21,
            "idempotency_key": "msg-1"
        }
    }))
    .await;
    let duplicate = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
    assert!(duplicate.is_err(), "test: duplicate idempotency key was delivered");

    let sending = host.channel.clone();
    let send = tokio::spawn(async move { sending.send(&SendMessage::new("final answer", "conv-1")).await });
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = host.recv().await;
            if value["method"] == "reply.text" {
                return value;
            }
        }
    })
    .await
    .expect("test: reply.text");
    assert_eq!(frame["params"]["text"], "final answer");
    assert_eq!(frame["params"]["final"], true);
    assert_eq!(frame["params"]["conversation_id"], "conv-1");
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": frame["id"],
        "result": {"ok": true}
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("test: send timed out")
        .expect("test: send task")
        .expect("test: send ok");
    assert!(
        host.channel
            .outbound_attachment("look [IMAGE:/tmp/secret.png]")
            .is_none()
    );
}

#[tokio::test]
async fn control_cancel_and_pause() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.hello("full").await;
    let removed = CancellationToken::new();
    host.channel
        .as_rich()
        .expect("test: rich")
        .bind_turn("conv-9", "turn-9", removed.clone());
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "inbound.control",
        "params": {
            "conversation_id": "conv-9",
            "kind": "agent_removed",
            "timestamp_ms": 30
        }
    }))
    .await;
    let ack = host.recv().await;
    assert_eq!(ack["result"]["ok"], true);
    assert!(removed.is_cancelled());

    let running = CancellationToken::new();
    host.channel
        .as_rich()
        .expect("test: rich")
        .bind_turn("conv-8", "turn-8", running.clone());
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "turn.cancel",
        "params": {
            "conversation_id": "conv-8",
            "turn_id": "turn-8",
            "reason": "host timeout"
        }
    }))
    .await;
    let ack = host.recv().await;
    assert_eq!(ack["result"]["ok"], true);
    assert!(running.is_cancelled());

    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "inbound.control",
        "params": {
            "conversation_id": "conv-1",
            "kind": "agent_offline",
            "timestamp_ms": 31
        }
    }))
    .await;
    let _ = host.recv().await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "inbound.message",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m2",
            "sender": sender(),
            "chat": {
                "conversation_id": "conv-1",
                "kind": "dm",
                "member_count": 2,
                "mentioned": false
            },
            "text": "after offline",
            "timestamp_ms": 32
        }
    }))
    .await;
    let paused = host.recv().await;
    assert_eq!(paused["error"]["code"], -32001);
}

#[tokio::test]
async fn message_deleted_acks_erased() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.hello("supervised").await;
    host.send(&json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "inbound.message_deleted",
        "params": {
            "conversation_id": "conv-1",
            "message_id": "m1",
            "deleted_by": sender(),
            "timestamp_ms": 40
        }
    }))
    .await;
    let ack = host.recv().await;
    assert_eq!(ack["result"]["erased"], true);
}

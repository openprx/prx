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
    let finish_channel = host.channel.clone();
    let finish_token = removed.clone();
    tokio::spawn(async move {
        finish_token.cancelled().await;
        finish_channel.unbind_turn("conv-9", "turn-9");
    });
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

#[tokio::test]
async fn delete_requires_hello_valid_params_and_a_running_eraser() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    let deleted = json!({"jsonrpc":"2.0","id":100,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"m1","deleted_by":sender(),"timestamp_ms":1}});
    host.send(&deleted).await;
    assert_eq!(host.recv().await["error"]["code"], -32002);
    host.hello("full").await;
    host.send(&json!({"jsonrpc":"2.0","id":101,"method":"inbound.message_deleted","params":{}}))
        .await;
    assert_eq!(host.recv().await["error"]["code"], -32602);
    let eraser = host.channel.inner.erase_tx.lock().take();
    host.send(&deleted).await;
    assert_eq!(host.recv().await["error"]["code"], -32603);
    *host.channel.inner.erase_tx.lock() = eraser;
}

#[tokio::test]
async fn unsupported_backend_refuses_erasure_instead_of_acknowledging_success() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.channel.attach_stores(
        std::sync::Arc::new(crate::memory::NoneMemory::new()),
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
    );
    host.hello("full").await;
    host.send(&json!({"jsonrpc":"2.0","id":102,"method":"inbound.message_deleted","params":{"conversation_id":"conv-1","message_id":"m1","deleted_by":sender(),"timestamp_ms":1}})).await;
    assert_eq!(host.recv().await["error"]["code"], -32601);
}

#[tokio::test]
async fn account_removal_marker_is_distinct_and_readd_is_accepted_before_hello() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.hello("full").await;
    // A conversation literally named account must not remove the account.
    super::super::accounts::mark_removed(&host.channel.inner.store, "agent-a", "account", 1).expect("test: marker");
    assert!(!super::super::accounts::is_account_removed(
        &host.channel.inner.store,
        "agent-a"
    ));
    host.send(&json!({"jsonrpc":"2.0","id":110,"method":"inbound.control","params":{"conversation_id":"conv-1","kind":"agent_removed","timestamp_ms":2}})).await;
    assert_eq!(host.recv().await["result"]["ok"], true);
    host.channel
        .inner
        .hello_done
        .store(false, std::sync::atomic::Ordering::Release);
    host.send(&json!({"jsonrpc":"2.0","id":111,"method":"inbound.control","params":{"conversation_id":"conv-1","kind":"agent_added","timestamp_ms":3}})).await;
    assert_eq!(host.recv().await["result"]["ok"], true);
    assert_eq!(host.hello("full").await["result"]["ok"], true);
}

#[tokio::test]
async fn a_closed_turns_surface_action_keeps_values_and_group_kind() {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    host.hello("full").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let channel = host.channel.clone();
    tokio::spawn(async move {
        let _ = Channel::listen(&channel, tx).await;
    });
    host.send(&json!({"jsonrpc":"2.0","id":130,"method":"inbound.message","params":{
        "conversation_id":"group-1","message_id":"initial","sender":sender(),
        "chat":{"conversation_id":"group-1","kind":"group","member_count":3,"mentioned":true},
        "text":"initial group message","timestamp_ms":1
    }}))
    .await;
    let initial = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("test: initial message")
        .expect("test: channel open");
    assert!(initial.is_group_hint);
    host.channel.claim_slot("group-1", "initial", "closed-turn");
    host.channel
        .bind_turn("group-1", "closed-turn", CancellationToken::new());
    host.channel.unbind_turn("group-1", "closed-turn");
    host.send(
        &json!({"jsonrpc":"2.0","id":131,"method":"inbound.surface_action","params":{
            "conversation_id":"group-1","sender":sender(),"surface_id":"card","version":1,
            "surface_hash":"hash","action_id":"choose","args_hash":"args","message_id":"initial",
            "values":{"flavor":"mint","count":2}
        }}),
    )
    .await;
    let action = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("test: action message")
        .expect("test: channel open");
    assert!(action.is_group_hint);
    assert_eq!(action.chat_kind, crate::channels::traits::ChatKind::Group);
    let content: serde_json::Value = serde_json::from_str(&action.content).expect("test: structured action");
    assert_eq!(content["action_id"], "choose");
    assert_eq!(content["values"], json!({"flavor":"mint","count":2}));
}

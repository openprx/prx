//! A forward-loop surface action must not send back into its full input queue.
use super::*;
use crate::channels::traits::Channel;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_surface_action_burst_drains_a_saturated_rich_queue() {
    let mut host = Host::connect(RamfluxConfig {
        on_new_message: crate::config::RamfluxOnNewMessage::Interrupt,
        ..RamfluxConfig::default()
    })
    .await;
    host.hello("full").await;
    let (tx, mut runtime) = tokio::sync::mpsc::channel(1);
    let channel = host.channel.clone();
    let listener = tokio::spawn(async move { Channel::listen(&channel, tx).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while host
            .channel
            .inner
            .inbound_tx
            .lock()
            .as_ref()
            .is_none_or(|tx| tx.max_capacity() != 64)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("test: forward listener installed");
    let message = |id: usize| {
        json!({"jsonrpc":"2.0","method":"inbound.message","params":{
        "conversation_id":"burst","message_id":format!("burst-{id}"),"sender":{
            "principal_id":"human-1","device_id":"device-1","kind":"human","trust":"verified","is_self":false},
        "chat":{"conversation_id":"burst","kind":"dm","member_count":2,"mentioned":false},
        "text":format!("burst input {id}"),"timestamp_ms":id}})
    };
    host.send(&message(0)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.send(&message(1)).await;
    host.send(&json!({"jsonrpc":"2.0","method":"inbound.surface_action","params":{
        "conversation_id":"burst","message_id":"host-card","surface_id":"card","version":1,
        "surface_hash":"hash","action_id":"submit","values":{"answer":"surface burst control"},"args_hash":"args",
        "sender":{"principal_id":"human-1","device_id":"device-1","kind":"human","trust":"verified","is_self":false}}}))
        .await;
    let inner = std::sync::Arc::clone(&host.channel.inner);
    let producer = tokio::spawn(async move {
        for id in 2..130 {
            host.send(&message(id)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.inbound_tx.lock().as_ref().unwrap().capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("test: rich queue must actually saturate");
    // The socket reader has more host frames than fit and is waiting for input
    // capacity before the forward loop is released from runtime backpressure.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let mut inputs = Vec::new();
        for _ in 0..131 {
            inputs.push(runtime.recv().await.expect("test: runtime input"));
        }
        inputs
    })
    .await
    .expect("test: surface re-entry must not deadlock the forward loop");
    assert_eq!(received[0].content, "burst input 0");
    assert_eq!(received[1].content, "burst input 1");
    assert!(received.iter().any(|m| m.content.contains("surface burst control")));
    let plain: Vec<_> = received
        .iter()
        .filter(|m| m.content.starts_with("burst input "))
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(
        plain,
        (0..130).map(|id| format!("burst input {id}")).collect::<Vec<_>>()
    );
    producer.await.unwrap();
    listener.abort();
}

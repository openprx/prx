//! Failed deletion must release admission without claiming erasure succeeded.
use super::*;

async fn next_message_runs_after_delete_failure(mode: &str) {
    let mut host = Host::connect(RamfluxConfig::default()).await;
    assert_eq!(host.hello("full").await["result"]["effective_autonomy"], "full");
    let mut retained_receiver = None;
    match mode {
        "absent" => {
            host.channel.inner.erase_tx.lock().take();
        }
        "closed" | "full" => {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            if mode == "full" {
                let (done, _) = tokio::sync::oneshot::channel();
                tx.try_send(super::super::eraser::EraseJob {
                    account: "agent-a".into(),
                    conversation: "other".into(),
                    message_ids: vec!["other".into()],
                    done,
                })
                .unwrap();
                retained_receiver = Some(rx);
            } else {
                drop(rx);
            }
            *host.channel.inner.erase_tx.lock() = Some(tx);
        }
        "stopped" => {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            *host.channel.inner.erase_tx.lock() = Some(tx);
            tokio::spawn(async move {
                drop(rx.recv().await.expect("test: admitted erase job"));
            });
        }
        "perform" => {
            host.channel.inner.attached_memory.lock().take();
        }
        _ => panic!("test: invalid erase fault"),
    }
    host.send(&json!({"jsonrpc":"2.0","id":200,"method":"inbound.message_deleted","params":{
        "conversation_id":"conv-failure","message_id":"deleted","deleted_by":{
            "principal_id":"human-1","device_id":"device-1","kind":"human","trust":"verified","is_self":false},"timestamp_ms":1}})).await;
    let failure = tokio::time::timeout(Duration::from_secs(2), host.recv())
        .await
        .expect("test: erase error response");
    assert_eq!(
        failure["error"]["code"],
        if mode == "full" { -32005 } else { -32603 },
        "{failure}"
    );
    host.send(&json!({"jsonrpc":"2.0","id":201,"method":"inbound.message","params":{
        "conversation_id":"conv-failure","message_id":"next","sender":{
            "principal_id":"human-1","device_id":"device-1","kind":"human","trust":"verified","is_self":false},
        "chat":{"conversation_id":"conv-failure","kind":"dm","member_count":2,"mentioned":false},
        "text":"next input must run","timestamp_ms":2}}))
        .await;
    assert_eq!(host.recv().await["result"]["ok"], true);
    let event = tokio::time::timeout(Duration::from_secs(2), host.inbound.recv())
        .await
        .expect("test: failed erase must resume the next message")
        .expect("test: next event");
    let crate::channels::rich::RichInbound::Message(message) = event else {
        panic!("test: expected next input");
    };
    assert_eq!(message.message_id, "next");
    assert!(
        host.channel.inner.deleting.lock().is_empty(),
        "failed delete retained an admission marker"
    );
    drop(retained_receiver);
}

#[tokio::test]
async fn r5_absent_eraser_resumes_next_message() {
    next_message_runs_after_delete_failure("absent").await;
}
#[tokio::test]
async fn r5_full_erase_queue_resumes_next_message() {
    next_message_runs_after_delete_failure("full").await;
}
#[tokio::test]
async fn r5_closed_erase_queue_resumes_next_message() {
    next_message_runs_after_delete_failure("closed").await;
}
#[tokio::test]
async fn r5_failed_erase_job_resumes_next_message() {
    next_message_runs_after_delete_failure("perform").await;
}

#[tokio::test]
async fn r5_stopped_eraser_resumes_next_message() {
    next_message_runs_after_delete_failure("stopped").await;
}

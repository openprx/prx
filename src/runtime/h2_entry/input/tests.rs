use super::*;
use crate::runtime::h2_entry::{EntryKind, profile::tests::Fixture};
fn message(channel: &str) -> ChannelMessage {
    ChannelMessage {
        id: format!("{channel}-message"),
        channel: channel.into(),
        sender: "local-user".into(),
        content: "{\"owner\":\"terminal\",\"proof\":true}".into(),
        ..ChannelMessage::default()
    }
}
#[tokio::test]
async fn shared_fifo_keeps_os_proof_while_background_and_json_markers_have_none() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let (sender, mut receiver) = ChatInputSender::channel(4);
    let mut producer = crate::runtime::h2_entry::origin::tests::producer(fixture.profile.clone());
    sender.submit_terminal(&mut producer, message("terminal")).unwrap();
    for channel in ["scheduler", "daemon", "terminal"] {
        sender.clone().send(message(channel)).await.unwrap();
    }
    let first = receiver.recv().await.unwrap();
    assert!(
        first
            .terminal
            .unwrap()
            .matches_message(&first.message.id, &first.message.content)
    );
    for _ in 0..3 {
        assert!(receiver.recv().await.unwrap().terminal.is_none());
    }
}
#[test]
fn legacy_fifo_refuses_to_drop_provenance_and_revocation_stops_full_queue_retry() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let (raw, _receiver) = mpsc::channel::<ChannelMessage>(1);
    let sender: ChatInputSender = raw.into();
    let mut producer = crate::runtime::h2_entry::origin::tests::producer(fixture.profile.clone());
    assert!(sender.submit_terminal(&mut producer, message("terminal")).is_err());
    fixture.profile.revoke();
    let (sender, _receiver) = ChatInputSender::channel(1);
    assert!(sender.submit_terminal(&mut producer, message("terminal")).is_err());
}

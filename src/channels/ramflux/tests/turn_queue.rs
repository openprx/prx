use super::super::super::rich::{Chat, ChatRoomKind, RichMessage, Sender, SenderKind, Trust};
use super::super::turn_queue::{Admission, TurnQueue};
use crate::config::RamfluxOnNewMessage;

fn message(id: &str) -> RichMessage {
    RichMessage {
        conversation_id: "conv".into(),
        message_id: id.into(),
        sender: Sender {
            principal_id: "p".into(),
            device_id: "d".into(),
            kind: SenderKind::Human,
            display_name: None,
            trust: Trust::Verified,
            is_self: false,
        },
        chat: Chat {
            conversation_id: "conv".into(),
            kind: ChatRoomKind::Dm,
            title: None,
            member_count: 2,
            mentioned: false,
        },
        text: id.into(),
        attachments: Vec::new(),
        reply_target: None,
        timestamp_ms: 1,
    }
}

#[test]
fn queue_marks_in_flight_and_drains() {
    let queue = TurnQueue::new(RamfluxOnNewMessage::Queue, 1);
    assert!(matches!(queue.admit(message("1")), Admission::Start(_)));
    assert!(queue.is_running("conv"));
    assert!(matches!(queue.admit(message("2")), Admission::Queued));
    let next = queue.finish("conv").expect("test: queued");
    assert_eq!(next.message_id, "2");
    assert!(queue.is_running("conv"));
    assert!(queue.finish("conv").is_none());
    assert!(!queue.is_running("conv"));

    assert!(matches!(queue.admit(message("3")), Admission::Start(_)));
    assert!(matches!(queue.admit(message("4")), Admission::Queued));
    queue.drain("conv");
    assert!(!queue.is_running("conv"));
    assert!(queue.finish("conv").is_none());

    let queue = TurnQueue::new(RamfluxOnNewMessage::Interrupt, 1);
    assert!(matches!(queue.admit(message("5")), Admission::Start(_)));
    assert!(matches!(queue.admit(message("6")), Admission::Interrupt(_)));
    assert!(queue.finish("conv").is_none());
    assert!(queue.is_running("conv"));
}

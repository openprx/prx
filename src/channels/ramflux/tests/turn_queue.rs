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
fn queue_holds_and_interrupt_replaces() {
    let queue = TurnQueue::new(RamfluxOnNewMessage::Queue);
    assert!(matches!(queue.admit(false, message("1")), Admission::Start(_)));
    assert!(matches!(queue.admit(true, message("2")), Admission::Queued));
    assert_eq!(queue.pop_next("conv").expect("test: queued").message_id, "2");
    assert!(queue.pop_next("conv").is_none());

    let queue = TurnQueue::new(RamfluxOnNewMessage::Interrupt);
    assert!(matches!(queue.admit(true, message("3")), Admission::Interrupt(_)));
}

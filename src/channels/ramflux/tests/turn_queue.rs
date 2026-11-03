use super::super::super::rich::{Chat, ChatRoomKind, RichMessage, Sender, SenderKind, Trust};
use super::super::turn_queue::{Admission, TurnQueue};
use crate::config::RamfluxOnNewMessage;

fn message(id: &str) -> RichMessage {
    RichMessage {
        reservation: None,
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
    let next = queue.finish_owned("conv", "1").expect("test: queued");
    assert_eq!(next.message_id, "2");
    assert!(queue.is_running("conv"));
    assert!(queue.finish_owned("conv", "2").is_none());
    assert!(!queue.is_running("conv"));

    assert!(matches!(queue.admit(message("3")), Admission::Start(_)));
    assert!(matches!(queue.admit(message("4")), Admission::Queued));
    queue.drain("conv");
    assert!(!queue.is_running("conv"));
    assert!(queue.finish_owned("conv", "3").is_none());

    let queue = TurnQueue::new(RamfluxOnNewMessage::Interrupt, 1);
    assert!(matches!(queue.admit(message("5")), Admission::Start(_)));
    assert!(matches!(queue.admit(message("6")), Admission::Interrupt(_)));
    assert!(queue.finish_owned("conv", "5").is_none());
    assert!(queue.is_running("conv"));
}

#[test]
fn four_queued_inputs_transfer_ownership_once_and_skip_deleted_input() {
    let queue = TurnQueue::new(RamfluxOnNewMessage::Queue, 1);
    assert!(matches!(queue.admit(message("a")), Admission::Start(_)));
    for id in ["b", "c", "d", "e"] {
        assert_eq!(queue.admit(message(id)), Admission::Queued);
    }
    queue.remove_queued("conv", "c");
    for (owner, next) in [("a", "b"), ("b", "d"), ("d", "e")] {
        assert_eq!(queue.finish_owned("conv", owner).expect("test: next").message_id, next);
        assert!(
            queue.finish_owned("conv", owner).is_none(),
            "double finish stole the next slot"
        );
        assert!(queue.owns("conv", next));
    }
    assert!(queue.finish_owned("conv", "e").is_none());
    assert!(!queue.is_running("conv"));
}

#[test]
fn repeated_interrupts_do_not_let_stale_guards_release_the_latest_input() {
    let queue = TurnQueue::new(RamfluxOnNewMessage::Interrupt, 1);
    assert!(matches!(queue.admit(message("a")), Admission::Start(_)));
    assert!(matches!(queue.admit(message("b")), Admission::Interrupt(_)));
    assert!(matches!(queue.admit(message("c")), Admission::Interrupt(_)));
    assert!(queue.finish_owned("conv", "a").is_none());
    assert!(queue.finish_owned("conv", "b").is_none());
    assert!(queue.owns("conv", "c"));
    assert!(queue.finish_owned("conv", "c").is_none());
    assert!(!queue.is_running("conv"));
}

#[test]
fn deletion_holds_queue_until_every_checkpoint_completes_and_retry_is_idempotent() {
    let queue = TurnQueue::new(RamfluxOnNewMessage::Queue, 1);
    assert!(matches!(queue.admit(message("a")), Admission::Start(_)));
    assert_eq!(queue.admit(message("b")), Admission::Queued);
    queue.suspend_delete("conv", "a");
    queue.suspend_delete("conv", "a");
    queue.suspend_delete("conv", "other");
    assert!(queue.finish_owned("conv", "a").is_none());
    assert_eq!(queue.admit(message("c")), Admission::Queued);
    assert_eq!(queue.resume_delete("conv", "a"), Vec::<RichMessage>::new());
    let ready = queue.resume_delete("conv", "other");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].message_id, "b");
    assert_eq!(queue.finish_owned("conv", "b").expect("test: next").message_id, "c");
}

#![cfg(test)]

use super::*;
use crate::chat::sessions::SessionEvent;
use crate::chat::sessions::id::SessionId;
use std::collections::HashSet;

#[test]
fn resumed_chat_session_ignores_late_events_from_detached_child_sessions() {
    let old_id = SessionId::from_run_id("old-child");
    let new_id = SessionId::from_run_id("new-child");
    let ignored_session_events = HashSet::from([old_id.clone()]);

    let old_event = SessionEvent::Delta {
        id: old_id,
        text: "must not enter resumed chat session".to_string(),
    };
    let new_event = SessionEvent::Delta {
        id: new_id,
        text: "valid new child output".to_string(),
    };

    assert!(
        should_ignore_session_event_after_chat_resume(&ignored_session_events, &old_event),
        "late events from detached child sessions must be dropped before ring/history routing"
    );
    assert!(
        !should_ignore_session_event_after_chat_resume(&ignored_session_events, &new_event),
        "events from new child sessions must still route normally after resume"
    );
}

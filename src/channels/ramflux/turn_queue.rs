//! Per-conversation turn admission.
//!
//! At most `max_in_flight` turns run for one conversation. Further messages
//! wait, or replace the running turn when the config says `interrupt`.

use super::super::rich::RichMessage;
use crate::config::RamfluxOnNewMessage;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Deliver this message now. Nothing else is running for the conversation.
    Start(RichMessage),
    /// Cancel the running turn, then deliver this message.
    Interrupt(RichMessage),
    /// Hold the message until the running turn finishes.
    Queued,
}

/// Queue of messages that arrived while a conversation already had a turn.
pub struct TurnQueue {
    policy: RamfluxOnNewMessage,
    queued: Mutex<HashMap<String, VecDeque<RichMessage>>>,
}

impl TurnQueue {
    #[must_use]
    pub fn new(policy: RamfluxOnNewMessage) -> Self {
        Self {
            policy,
            queued: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn admit(&self, conversation_in_flight: bool, message: RichMessage) -> Admission {
        if !conversation_in_flight {
            return Admission::Start(message);
        }
        match self.policy {
            RamfluxOnNewMessage::Interrupt => Admission::Interrupt(message),
            RamfluxOnNewMessage::Queue => {
                self.queued
                    .lock()
                    .entry(message.conversation_id.clone())
                    .or_default()
                    .push_back(message);
                Admission::Queued
            }
        }
    }

    #[must_use]
    pub fn pop_next(&self, conversation_id: &str) -> Option<RichMessage> {
        let mut queued = self.queued.lock();
        let next = queued.get_mut(conversation_id).and_then(VecDeque::pop_front);
        if queued.get(conversation_id).is_some_and(VecDeque::is_empty) {
            queued.remove(conversation_id);
        }
        next
    }
}

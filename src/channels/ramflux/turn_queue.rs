//! Per-conversation turn admission.
//!
//! A conversation is marked in flight when a message is admitted, before the
//! turn is bound. Further messages wait, or replace the running turn when the
//! config says `interrupt`. The queue is bounded. Cancel, removal, and revoke
//! drop whatever is still queued.

use super::super::rich::RichMessage;
use crate::config::RamfluxOnNewMessage;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};

/// Messages held behind a live turn. Further arrivals are refused.
const MAX_QUEUED: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Deliver this message now. The conversation is marked in flight.
    Start(RichMessage),
    /// Cancel the running turn, then deliver this message in the same slot.
    Interrupt(RichMessage),
    /// Hold the message until the running turn finishes.
    Queued,
    /// The queue for this conversation is full. The message was not stored.
    Rejected,
}

/// Queue of messages that arrived while a conversation already had a turn.
pub struct TurnQueue {
    policy: RamfluxOnNewMessage,
    max_in_flight: usize,
    running: Mutex<HashMap<String, usize>>,
    queued: Mutex<HashMap<String, VecDeque<RichMessage>>>,
    /// Interrupt replacements occupy the slot the cancelled turn still holds.
    /// The next `finish` for that conversation must not free it.
    suppress_finish: Mutex<HashSet<String>>,
}

impl TurnQueue {
    #[must_use]
    pub fn new(policy: RamfluxOnNewMessage, max_in_flight: usize) -> Self {
        Self {
            policy,
            max_in_flight: max_in_flight.max(1),
            running: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            suppress_finish: Mutex::new(HashSet::new()),
        }
    }

    /// Admit `message`, marking the conversation in flight when it starts.
    #[must_use]
    pub fn admit(&self, message: RichMessage) -> Admission {
        let id = message.conversation_id.clone();
        let mut running = self.running.lock();
        let active = running.get(&id).copied().unwrap_or(0);
        if active < self.max_in_flight {
            running.insert(id, active.saturating_add(1));
            return Admission::Start(message);
        }
        drop(running);
        match self.policy {
            RamfluxOnNewMessage::Interrupt => {
                self.suppress_finish.lock().insert(id);
                self.queued.lock().remove(&message.conversation_id);
                Admission::Interrupt(message)
            }
            RamfluxOnNewMessage::Queue => {
                let mut queued = self.queued.lock();
                let bucket = queued.entry(id).or_default();
                if bucket.len() >= MAX_QUEUED {
                    Admission::Rejected
                } else {
                    bucket.push_back(message);
                    Admission::Queued
                }
            }
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_running(&self, conversation_id: &str) -> bool {
        self.running.lock().get(conversation_id).copied().unwrap_or(0) > 0
    }

    /// Release one in-flight slot and return the next queued message, if any.
    ///
    /// An interrupt replacement consumes this call without freeing the slot:
    /// the replacement is already in flight.
    #[must_use]
    pub fn finish(&self, conversation_id: &str) -> Option<RichMessage> {
        if self.suppress_finish.lock().remove(conversation_id) {
            return None;
        }
        {
            let mut running = self.running.lock();
            if let Some(count) = running.get_mut(conversation_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    running.remove(conversation_id);
                }
            }
        }
        let next = self.pop_next(conversation_id)?;
        let mut running = self.running.lock();
        let count = running.entry(conversation_id.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        Some(next)
    }

    /// Drop queued messages and the in-flight mark for one conversation.
    pub fn drain(&self, conversation_id: &str) {
        self.queued.lock().remove(conversation_id);
        self.running.lock().remove(conversation_id);
        self.suppress_finish.lock().remove(conversation_id);
    }

    /// Drop every queued message and in-flight mark.
    pub fn drain_all(&self) {
        self.queued.lock().clear();
        self.running.lock().clear();
        self.suppress_finish.lock().clear();
    }

    fn pop_next(&self, conversation_id: &str) -> Option<RichMessage> {
        let mut queued = self.queued.lock();
        let next = queued.get_mut(conversation_id).and_then(VecDeque::pop_front);
        if queued.get(conversation_id).is_some_and(VecDeque::is_empty) {
            queued.remove(conversation_id);
        }
        next
    }
}

//! Per-conversation admission with identity-owned slots.

use super::super::rich::RichMessage;
use crate::config::RamfluxOnNewMessage;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_QUEUED: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Start(RichMessage),
    Interrupt(RichMessage),
    Queued,
    Rejected { depth: usize },
}

#[derive(Default)]
struct State {
    running: HashMap<String, HashSet<String>>,
    queued: HashMap<String, VecDeque<RichMessage>>,
    deleting: HashMap<String, HashSet<String>>,
}

/// A slot belongs to a message, including while it waits to bind a turn.
/// Interrupt transfers ownership atomically; the old guard cannot release it.
pub struct TurnQueue {
    policy: RamfluxOnNewMessage,
    max_in_flight: usize,
    state: Mutex<State>,
}

impl TurnQueue {
    pub(super) fn pending_messages(&self, conversation_id: &str) -> HashSet<String> {
        let state = self.state.lock();
        state
            .running
            .get(conversation_id)
            .into_iter()
            .flatten()
            .cloned()
            .chain(
                state
                    .queued
                    .get(conversation_id)
                    .into_iter()
                    .flatten()
                    .map(|message| message.message_id.clone()),
            )
            .collect()
    }
    #[must_use]
    pub fn new(policy: RamfluxOnNewMessage, max_in_flight: usize) -> Self {
        Self {
            policy,
            max_in_flight: max_in_flight.max(1),
            state: Mutex::new(State::default()),
        }
    }

    #[must_use]
    pub fn admit(&self, message: RichMessage) -> Admission {
        let id = message.conversation_id.clone();
        let mut state = self.state.lock();
        if state
            .running
            .get(&id)
            .is_some_and(|ids| ids.contains(&message.message_id))
            || state
                .queued
                .get(&id)
                .is_some_and(|queued| queued.iter().any(|existing| existing.message_id == message.message_id))
        {
            return Admission::Queued;
        }
        let active = state.running.get(&id).map_or(0, HashSet::len);
        if active < self.max_in_flight && !state.deleting.contains_key(&id) {
            state.running.entry(id).or_default().insert(message.message_id.clone());
            return Admission::Start(message);
        }
        let policy = if state.deleting.contains_key(&id) {
            RamfluxOnNewMessage::Queue
        } else {
            self.policy
        };
        match policy {
            RamfluxOnNewMessage::Interrupt => {
                state.queued.remove(&id);
                state.running.insert(id, HashSet::from([message.message_id.clone()]));
                Admission::Interrupt(message)
            }
            RamfluxOnNewMessage::Queue => {
                let bucket = state.queued.entry(id).or_default();
                if active.saturating_add(bucket.len()) >= MAX_QUEUED {
                    Admission::Rejected {
                        depth: active.saturating_add(bucket.len()),
                    }
                } else {
                    bucket.push_back(message);
                    Admission::Queued
                }
            }
        }
    }

    #[cfg(test)]
    pub fn is_running(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .running
            .get(conversation_id)
            .is_some_and(|ids| !ids.is_empty())
    }

    /// Only the owner may release a slot, at most once.
    #[must_use]
    pub fn finish_owned(&self, conversation_id: &str, message_id: &str) -> Option<RichMessage> {
        let mut state = self.state.lock();
        let active = state.running.get_mut(conversation_id)?;
        if !active.remove(message_id) {
            return None;
        }
        if active.is_empty() {
            state.running.remove(conversation_id);
        }
        if state.deleting.contains_key(conversation_id) {
            return None;
        }
        let next = state.queued.get_mut(conversation_id).and_then(VecDeque::pop_front)?;
        state
            .running
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(next.message_id.clone());
        Some(next)
    }

    /// Hold queued work until all erasures for this conversation have committed.
    pub fn suspend_delete(&self, conversation_id: &str, message_id: &str) {
        self.state
            .lock()
            .deleting
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(message_id.to_owned());
    }

    pub fn resume_delete(&self, conversation_id: &str, message_id: &str) -> Vec<RichMessage> {
        let mut state = self.state.lock();
        if let Some(ids) = state.deleting.get_mut(conversation_id) {
            ids.remove(message_id);
            if !ids.is_empty() {
                return Vec::new();
            }
            state.deleting.remove(conversation_id);
        }
        let active = state.running.get(conversation_id).map_or(0, HashSet::len);
        let mut ready = Vec::new();
        for _ in active..self.max_in_flight {
            let Some(next) = state.queued.get_mut(conversation_id).and_then(VecDeque::pop_front) else {
                break;
            };
            state
                .running
                .entry(conversation_id.to_owned())
                .or_default()
                .insert(next.message_id.clone());
            ready.push(next);
        }
        ready
    }

    /// Remove pending input without disturbing the live slot.
    pub fn remove_queued(&self, conversation_id: &str, message_id: &str) {
        if let Some(queued) = self.state.lock().queued.get_mut(conversation_id) {
            queued.retain(|message| message.message_id != message_id);
        }
    }

    pub fn owns(&self, conversation_id: &str, message_id: &str) -> bool {
        self.state
            .lock()
            .running
            .get(conversation_id)
            .is_some_and(|ids| ids.contains(message_id))
    }

    pub fn drain(&self, conversation_id: &str) {
        let mut state = self.state.lock();
        state.queued.remove(conversation_id);
        state.running.remove(conversation_id);
    }

    pub fn drain_all(&self) {
        let mut state = self.state.lock();
        state.running.clear();
        state.queued.clear();
    }
}

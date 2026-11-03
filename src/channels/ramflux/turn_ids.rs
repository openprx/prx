//! Volatile host correlation, independent of internal run identity.

use super::MessageKey;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

struct Entry {
    generation: u64,
    host: Option<String>,
    internal: Option<String>,
    live: bool,
    owner: Weak<Reservation>,
}

#[derive(Default)]
struct State {
    generation: u64,
    entries: HashMap<MessageKey, Entry>,
    seen: HashMap<MessageKey, Option<String>>,
}

#[derive(Default)]
pub(super) struct TurnIds(Mutex<State>);

/// The queue or forwarding future owns this until claim_slot transfers to the
/// turn guard. The map holds only a Weak reference, never a pre-bind owner.
pub(crate) struct Reservation {
    ids: Arc<TurnIds>,
    key: MessageKey,
    generation: u64,
    pub(super) transferred: CancellationToken,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Reservation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ids, &other.ids) && self.generation == other.generation
    }
}
impl Eq for Reservation {}

impl Reservation {
    pub(super) fn may_release_slot(&self) -> bool {
        self.ids
            .0
            .lock()
            .entries
            .get(&self.key)
            .is_none_or(|entry| entry.generation == self.generation)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.ids.release(&self.key, self.generation);
    }
}

impl TurnIds {
    #[cfg(test)]
    pub(super) fn has_host(&self, conversation: &str, host: &str) -> bool {
        self.0
            .lock()
            .entries
            .iter()
            .any(|(key, entry)| key.conversation == conversation && entry.host.as_deref() == Some(host))
    }
    #[cfg(test)]
    pub(super) fn owner(&self, conversation: &str, message: &str) -> Option<Arc<Reservation>> {
        self.0
            .lock()
            .entries
            .get(&MessageKey::new(conversation, message))
            .and_then(|entry| entry.owner.upgrade())
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.0.lock().entries.is_empty()
    }
    pub(super) fn validate(host: Option<&str>) -> Result<(), String> {
        if let Some(host) = host
            && (host.is_empty()
                || host.len() > 128
                || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)))
        {
            return Err("turn_id must be 1-128 ASCII bytes [A-Za-z0-9._:-]".to_owned());
        }
        Ok(())
    }

    pub(super) fn check_message(&self, conversation: &str, message: &str, host: Option<&str>) -> Result<bool, String> {
        Self::validate(host)?;
        let state = self.0.lock();
        if let Some(previous) = state.seen.get(&MessageKey::new(conversation, message)) {
            if previous.as_deref() != host {
                return Err("message_id replay has a different turn_id".to_owned());
            }
            return Ok(state.entries.contains_key(&MessageKey::new(conversation, message)));
        }
        Ok(false)
    }

    pub(super) fn reserve(
        self: &Arc<Self>,
        conversation: &str,
        message: &str,
        host: Option<&str>,
        has_pending: impl Fn(&str) -> bool,
    ) -> Result<Arc<Reservation>, String> {
        Self::validate(host)?;
        let mut state = self.0.lock();
        state
            .entries
            .retain(|_, entry| entry.live || entry.internal.as_deref().is_some_and(&has_pending));
        if let Some(host) = host
            && state
                .entries
                .iter()
                .any(|(key, entry)| key.conversation == conversation && entry.host.as_deref() == Some(host))
        {
            return Err(format!("duplicate turn_id {host} in conversation"));
        }
        state.generation += 1;
        let generation = state.generation;
        let key = MessageKey::new(conversation, message);
        let owner = Arc::new(Reservation {
            ids: Arc::clone(self),
            key: key.clone(),
            generation,
            transferred: CancellationToken::new(),
        });
        state.entries.insert(
            key,
            Entry {
                generation,
                host: host.map(str::to_owned),
                internal: None,
                live: true,
                owner: Arc::downgrade(&owner),
            },
        );
        Ok(owner)
    }

    pub(super) fn remember(&self, conversation: &str, message: &str, host: Option<&str>) {
        self.0
            .lock()
            .seen
            .insert(MessageKey::new(conversation, message), host.map(str::to_owned));
    }

    pub(super) fn bind(&self, conversation: &str, message: &str, internal: &str) -> bool {
        let owner = {
            let mut state = self.0.lock();
            state
                .entries
                .get_mut(&MessageKey::new(conversation, message))
                .and_then(|entry| {
                    let owner = entry.owner.upgrade()?;
                    entry.internal = Some(internal.to_owned());
                    if entry.host.is_none() {
                        entry.host = Some(internal.to_owned());
                    }
                    Some(owner)
                })
        };
        if let Some(owner) = owner {
            owner.transferred.cancel();
            return true;
        }
        false
    }

    pub(super) fn unbind(&self, internal: &str) {
        let mut state = self.0.lock();
        for entry in state
            .entries
            .values_mut()
            .filter(|entry| entry.internal.as_deref() == Some(internal))
        {
            entry.live = false;
        }
    }

    // Direct RichChannel users may bind a turn without inbound admission.
    // Their existing wire identity is the supplied internal ID itself.
    pub(super) fn bind_direct(&self, conversation: &str, internal: &str) {
        let mut state = self.0.lock();
        if state
            .entries
            .values()
            .any(|entry| entry.internal.as_deref() == Some(internal))
        {
            return;
        }
        state.generation += 1;
        let generation = state.generation;
        state.entries.insert(
            MessageKey::new(conversation, internal),
            Entry {
                generation,
                host: Some(internal.to_owned()),
                internal: Some(internal.to_owned()),
                live: true,
                owner: Weak::new(),
            },
        );
    }

    fn release(&self, key: &MessageKey, generation: u64) {
        let mut state = self.0.lock();
        if state
            .entries
            .get(key)
            .is_some_and(|entry| entry.generation == generation && entry.internal.is_none())
        {
            state.entries.remove(key);
        }
    }

    pub(super) fn release_unbound(&self, conversation: Option<&str>) {
        self.release_matching(|key, _| conversation.is_none_or(|conv| key.conversation == conv));
    }

    pub(super) fn release_message(&self, conversation: &str, message: &str) {
        self.release_matching(|key, _| key.conversation == conversation && key.message == message);
    }

    pub(super) fn release_displaced(&self, conversation: &str, pending: &std::collections::HashSet<String>) {
        self.release_matching(|key, seen| key.conversation == conversation && seen && !pending.contains(&key.message));
    }

    fn release_matching(&self, matches: impl Fn(&MessageKey, bool) -> bool) {
        let owners = {
            let mut state = self.0.lock();
            let mut owners = Vec::new();
            let State { entries, seen, .. } = &mut *state;
            entries.retain(|key, entry| {
                let remove = entry.internal.is_none() && matches(key, seen.contains_key(key));
                if remove && let Some(owner) = entry.owner.upgrade() {
                    owners.push(owner);
                }
                !remove
            });
            owners
        };
        for owner in owners {
            owner.transferred.cancel();
        }
    }

    pub(super) fn resolve(&self, conversation: &str, host: &str) -> Option<String> {
        self.0.lock().entries.iter().find_map(|(key, entry)| {
            if key.conversation == conversation && entry.live && entry.host.as_deref() == Some(host) {
                entry.internal.clone()
            } else {
                None
            }
        })
    }

    /// The only outbound translation. Pending/replay/fence keys stay internal.
    pub(super) fn translate(&self, params: &mut serde_json::Value) {
        let Some(internal) = params.get("turn_id").and_then(serde_json::Value::as_str) else {
            return;
        };
        let Some(conversation) = params.get("conversation_id").and_then(serde_json::Value::as_str) else {
            return;
        };
        let state = self.0.lock();
        let Some(host) = state.entries.iter().find_map(|(key, entry)| {
            (key.conversation == conversation && entry.internal.as_deref() == Some(internal))
                .then_some(entry.host.as_deref())
                .flatten()
        }) else {
            return;
        };
        if params
            .get("surface_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|surface| surface.strip_prefix("final-"))
            == Some(internal)
        {
            params["surface_id"] = format!("final-{host}").into();
        }
        params["turn_id"] = host.into();
    }
}

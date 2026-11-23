//! Erase stops active receivers and waits for their retained source caches.
use super::{MessageInput, MessagingRuntime};
use std::sync::{Arc, Weak};
pub(super) type Stop = Arc<dyn Fn() + Send + Sync>;
pub(super) type Cache = Arc<dyn Fn(Arc<dyn Send + Sync>, Vec<tokio_util::sync::CancellationToken>) + Send + Sync>;
pub(super) struct Active {
    pub stop: Stop,
    pub done: tokio_util::sync::CancellationToken,
}
pub(super) struct Cached {
    pub alive: Arc<dyn Fn() -> bool + Send + Sync>,
    pub clear: Cache,
}
pub(crate) struct MessageTurnGuard {
    runtime: Weak<MessagingRuntime>,
    scope: uuid::Uuid,
    id: uuid::Uuid,
    done: tokio_util::sync::CancellationToken,
}
impl Drop for MessageTurnGuard {
    fn drop(&mut self) {
        self.done.cancel();
        if let Some(runtime) = self.runtime.upgrade() {
            let mut active = runtime.turns.lock();
            if let Some(turns) = active.get_mut(&self.scope) {
                turns.remove(&self.id);
                if turns.is_empty() {
                    active.remove(&self.scope);
                }
            }
        }
    }
}
impl MessagingRuntime {
    pub(crate) fn hold_turn(
        self: &Arc<Self>,
        input: &MessageInput,
        stop: Stop,
    ) -> super::MailboxResult<MessageTurnGuard> {
        let event = input
            .receipt
            .adopted_message_event_id
            .ok_or(super::MailboxError::CorruptState)?;
        self.store.with_input(
            &self.workspace,
            &input.message.to,
            event,
            &input.text,
            input.message.id,
            &input.receipt,
            || self.insert_turn(input.scope, stop),
        )
    }
    pub(crate) fn hold_scope(
        self: &Arc<Self>,
        scope: &super::MailboxScope,
        target: &crate::messaging::SessionAddr,
        stop: Stop,
    ) -> super::MailboxResult<MessageTurnGuard> {
        self.store
            .with_live_scope(scope, target, || self.insert_turn(scope.scope_id, stop))
    }
    fn insert_turn(self: &Arc<Self>, scope: uuid::Uuid, stop: Stop) -> MessageTurnGuard {
        let id = uuid::Uuid::new_v4();
        let done = tokio_util::sync::CancellationToken::new();
        self.turns.lock().entry(scope).or_default().insert(
            id,
            Active {
                stop,
                done: done.clone(),
            },
        );
        MessageTurnGuard {
            runtime: Arc::downgrade(self),
            scope,
            id,
            done,
        }
    }
    pub(crate) fn register_cache(
        &self,
        input: &MessageInput,
        key: String,
        alive: Arc<dyn Fn() -> bool + Send + Sync>,
        clear: Cache,
    ) -> super::MailboxResult<()> {
        let event = input
            .receipt
            .adopted_message_event_id
            .ok_or(super::MailboxError::CorruptState)?;
        self.store.with_input(
            &self.workspace,
            &input.message.to,
            event,
            &input.text,
            input.message.id,
            &input.receipt,
            || {
                let mut caches = self.caches.lock();
                caches.retain(|_, entries| {
                    entries.retain(|_, entry| (entry.alive)());
                    !entries.is_empty()
                });
                caches
                    .entry(input.scope)
                    .or_default()
                    .insert(key, Cached { alive, clear });
            },
        )
    }
    pub(super) fn stop_scope(&self, scope: uuid::Uuid) {
        let turns = self.turns.lock().remove(&scope).unwrap_or_default();
        let done: Vec<_> = turns.values().map(|active| active.done.clone()).collect();
        for active in turns.into_values() {
            (active.stop)();
        }
        let caches = self.caches.lock().remove(&scope).unwrap_or_default();
        for cached in caches.into_values() {
            if (cached.alive)() {
                (cached.clear)(self.store.cache_drain_guard(scope), done.clone());
            }
        }
    }
}

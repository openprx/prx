//! Ramflux channel: a same-user Unix socket bridge.
//!
//! prx does not hold ramflux keys, does not sign, and does not know nodes or
//! gateways. The agent-host owns everything beyond the socket.

mod accounts;
mod approval;
mod authorization;
pub mod crypto;
mod erase;
mod erase_index;
mod eraser;
mod hooks;
mod inbound;
mod outbound;
mod protocol;
mod rpc;
mod socket;
mod storage;
mod surface_schema;
pub(crate) mod turn_ids;
mod turn_queue;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod turnid_test_support;
#[cfg(test)]
pub(crate) use turnid_test_support::turnid_code_fixture;

use super::rich::{RichChannel, RichInbound, RichReply, TextReply, TurnError, TurnState, TurnStatus};
use super::traits::{Channel, SendMessage};
use crate::config::RamfluxConfig;
use approval::ApprovalGate;
use async_trait::async_trait;
use authorization::Authority;
use parking_lot::Mutex;
use rpc::RpcHub;
use socket::unix_ms;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

struct TurnSlot {
    conversation_id: String,
    turn_id: String,
    token: CancellationToken,
}

struct LostTurn {
    conversation_id: String,
    turn_id: String,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct MessageKey {
    conversation: String,
    message: String,
}
impl MessageKey {
    fn new(conversation: &str, message: &str) -> Self {
        Self {
            conversation: conversation.to_owned(),
            message: message.to_owned(),
        }
    }
}

#[derive(Clone)]
struct TurnMessage {
    key: MessageKey,
    source: String,
}

// A forwarding future owns the pre-bind slot. Binding removes slot_messages;
// aborted/dropped forwarding releases only a still-unclaimed slot.
struct ForwardSlot {
    inner: Arc<Inner>,
    conversation: String,
    message: String,
    armed: bool,
    reservation: Option<Arc<turn_ids::Reservation>>,
}
impl ForwardSlot {
    const fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for ForwardSlot {
    fn drop(&mut self) {
        let released = {
            let _admission = self.inner.admission.lock();
            self.armed
                && self.reservation.as_ref().is_none_or(|owner| owner.may_release_slot())
                && self.inner.claim_slot_message(&self.conversation, &self.message)
        };
        // Pumping can spawn another owned future. Its immediate destruction
        // during runtime shutdown must not re-enter the admission mutex.
        if released {
            self.inner.pump_next(&self.conversation, &self.message);
        }
    }
}

pub(crate) struct TrackedMessage {
    pub conversation_id: String,
    pub objects: Vec<String>,
    pub sender: super::rich::Sender,
}

pub(crate) struct Inner {
    self_weak: std::sync::Weak<Self>,
    config: RamfluxConfig,
    admission: Mutex<()>,
    revocations: AtomicU64,
    socket_path: PathBuf,
    auth: Arc<Authority>,
    rpc: Arc<RpcHub>,
    approvals: Arc<ApprovalGate>,
    shutdown: CancellationToken,
    hello_done: AtomicBool,
    paused: AtomicBool,
    writer_ready: AtomicBool,
    connection_open: AtomicBool,
    started: AtomicBool,
    start_lock: AsyncMutex<()>,
    last_ping_ms: AtomicU64,
    account_id: Mutex<Option<String>>,
    inbound_tx: Mutex<Option<mpsc::Sender<RichInbound>>>,
    turns: Mutex<HashMap<String, Vec<TurnSlot>>>,
    turns_changed: tokio::sync::Notify,
    tracked: Mutex<HashMap<MessageKey, TrackedMessage>>,
    derived_messages: Mutex<HashMap<MessageKey, String>>,
    deleting: Mutex<std::collections::HashSet<(String, String, String)>>,
    conversations: Mutex<HashMap<String, super::rich::Chat>>,
    reply_targets: Mutex<HashMap<String, String>>,
    attached_memory: Mutex<Option<Arc<dyn crate::memory::Memory>>>,
    histories: Mutex<Option<super::ConversationHistoryMap>>,
    history_sessions: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    attachments: Mutex<HashMap<MessageKey, Vec<super::rich::AttachmentRef>>>,
    task_versions: Mutex<HashMap<String, u64>>,
    slot_messages: Mutex<std::collections::HashSet<MessageKey>>,
    slot_turns: Mutex<HashMap<String, TurnMessage>>,
    expected_peer: Mutex<Option<u32>>,
    scopes: Mutex<HashMap<(String, String), Arc<dyn crate::memory::Memory>>>,
    message_memories: Mutex<HashMap<MessageKey, Arc<dyn crate::memory::Memory>>>,
    erase_tx: Mutex<Option<mpsc::Sender<eraser::EraseJob>>>,
    eraser: Mutex<Option<tokio::task::JoinHandle<()>>>,
    eraser_started: AtomicBool,
    lost_status: Mutex<Vec<LostTurn>>,
    crypto: Arc<crypto::AccountCrypto>,
    store: storage::ChannelStore,
    queue: turn_queue::TurnQueue,
    workspace: PathBuf,
    persona: Mutex<Option<accounts::Persona>>,
    #[cfg(test)]
    forward_pause: Mutex<Option<(tokio::sync::oneshot::Sender<()>, Arc<tokio::sync::Barrier>)>>,
    #[cfg(test)]
    forward_abort: Mutex<Option<tokio::task::AbortHandle>>,
    #[cfg(test)]
    forward_release: Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

impl Inner {
    fn try_claim_connection(&self) -> bool {
        self.connection_open
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release_connection(&self) {
        self.connection_open.store(false, Ordering::Release);
        self.writer_ready.store(false, Ordering::Release);
    }

    fn note_ping(&self, now_ms: u64) {
        self.last_ping_ms.store(now_ms, Ordering::Release);
        self.writer_ready.store(true, Ordering::Release);
    }

    fn set_account(&self, account_id: &str) {
        *self.account_id.lock() = Some(account_id.to_string());
    }

    fn healthy(&self) -> bool {
        if !self.writer_ready.load(Ordering::Acquire) {
            return false;
        }
        let last = self.last_ping_ms.load(Ordering::Acquire);
        if last == 0 {
            return false;
        }
        socket::ping_is_fresh(last, unix_ms())
    }

    async fn emit_inbound(&self, event: RichInbound) -> Result<(), inbound::InboundError> {
        let tx = self.inbound_tx.lock().clone();
        if let Some(tx) = tx {
            tx.send(event)
                .await
                .map_err(|_| inbound::InboundError::Failed("rich listener closed".to_string()))?;
        } else {
            return Err(inbound::InboundError::Failed("rich listener unavailable".to_owned()));
        }
        Ok(())
    }

    fn emit_message(
        self: &Arc<Self>,
        message: super::rich::RichMessage,
    ) -> impl std::future::Future<Output = Result<(), inbound::InboundError>> + Send + 'static {
        // Construct ownership before returning the future, including for a
        // future that is dropped without ever being polled.
        let mut guard = ForwardSlot {
            inner: Arc::clone(self),
            conversation: message.conversation_id.clone(),
            message: message.message_id.clone(),
            armed: true,
            reservation: message.reservation.clone(),
        };
        async move {
            guard.inner.emit_inbound(RichInbound::Message(message)).await?;
            guard.disarm();
            Ok(())
        }
    }

    fn track_message(&self, message_id: &str, tracked: TrackedMessage) {
        self.tracked
            .lock()
            .entry(MessageKey::new(&tracked.conversation_id, message_id))
            .or_insert(tracked);
    }

    fn take_tracked(&self, conversation_id: &str, message_id: &str) -> Option<TrackedMessage> {
        self.tracked
            .lock()
            .remove(&MessageKey::new(conversation_id, message_id))
    }

    fn remember_reply_target(&self, conversation_id: &str, target: &str) {
        if !target.is_empty() {
            self.reply_targets
                .lock()
                .insert(conversation_id.to_string(), target.to_string());
        }
    }

    fn reply_target_for(&self, conversation_id: &str) -> Option<String> {
        self.reply_targets.lock().get(conversation_id).cloned()
    }

    fn bind_turn(&self, conversation_id: &str, turn_id: &str, token: CancellationToken) {
        self.rpc.turn_ids.bind_direct(conversation_id, turn_id);
        let mut turns = self.turns.lock();
        let slots = turns.entry(conversation_id.to_string()).or_default();
        if let Some(existing) = slots.iter_mut().find(|slot| slot.turn_id == turn_id) {
            existing.token = token;
            return;
        }
        slots.push(TurnSlot {
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            token,
        });
    }

    fn unbind_turn(&self, conversation_id: &str, turn_id: &str) {
        self.rpc.turn_ids.unbind(turn_id);
        let mut turns = self.turns.lock();
        if let Some(slots) = turns.get_mut(conversation_id) {
            slots.retain(|slot| slot.turn_id != turn_id);
            if slots.is_empty() {
                turns.remove(conversation_id);
            }
        }
        let message_id = self.slot_turns.lock().remove(turn_id);
        drop(turns);
        self.turns_changed.notify_waiters();
        if let Some(message_id) = message_id {
            self.message_memories.lock().remove(&message_id.key);
            self.pump_next(conversation_id, &message_id.key.message);
        }
    }

    fn cancel_tokens(&self, conversation_id: &str) {
        let slots = self.turns.lock();
        if let Some(slots) = slots.get(conversation_id) {
            for slot in slots {
                slot.token.cancel();
            }
        }
    }

    fn cancel_conversation(&self, conversation_id: &str) {
        self.cancel_tokens(conversation_id);
        self.queue.drain(conversation_id);
        self.rpc.turn_ids.release_unbound(Some(conversation_id));
        self.slot_messages
            .lock()
            .retain(|key| key.conversation != conversation_id);
    }

    fn cancel_turn(&self, conversation_id: &str, turn_id: &str) -> bool {
        let turns = self.turns.lock();
        let Some(slot) = turns
            .get(conversation_id)
            .and_then(|slots| slots.iter().find(|slot| slot.turn_id == turn_id))
        else {
            return false;
        };
        slot.token.cancel();
        // The guard releases ownership after the last write has completed.
        true
    }

    fn pump_next(&self, conversation_id: &str, message_id: &str) {
        let Some(next) = self.queue.finish_owned(conversation_id, message_id) else {
            return;
        };
        self.note_slot_message(&next.conversation_id, &next.message_id);
        if let Some(inner) = self.self_weak.upgrade() {
            let emitting = inner.emit_message(next);
            tokio::spawn(async move {
                if let Err(error) = emitting.await {
                    tracing::debug!(%error, "ramflux queued message was not delivered");
                }
            });
        }
    }

    fn source_message_id(&self, conversation: &str, message: &str) -> String {
        self.derived_messages
            .lock()
            .get(&MessageKey::new(conversation, message))
            .cloned()
            .unwrap_or_else(|| message.to_owned())
    }

    fn resume_delete(&self, account: &str, conversation: &str, message: &str) {
        let ready = {
            let _admission = self.admission.lock();
            self.deleting
                .lock()
                .remove(&(account.to_owned(), conversation.to_owned(), message.to_owned()));
            let ready = self.queue.resume_delete(conversation, message);
            for next in &ready {
                self.note_slot_message(&next.conversation_id, &next.message_id);
            }
            ready
        };
        if let Some(inner) = self.self_weak.upgrade() {
            let emitting: Vec<_> = ready.into_iter().map(|next| inner.emit_message(next)).collect();
            tokio::spawn(async move {
                for emitting in emitting {
                    if emitting.await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    fn cancel_message(&self, conversation_id: &str, message_id: &str) {
        let mut family = vec![MessageKey::new(conversation_id, message_id)];
        family.extend(
            self.derived_messages
                .lock()
                .iter()
                .filter(|(key, source)| key.conversation == conversation_id && source.as_str() == message_id)
                .map(|(key, _)| key.clone()),
        );
        for key in &family {
            self.queue.remove_queued(conversation_id, &key.message);
            self.rpc.turn_ids.release_message(conversation_id, &key.message);
        }
        let run_ids: Vec<String> = self
            .slot_turns
            .lock()
            .iter()
            .filter(|(_, id)| id.key.conversation == conversation_id && id.source == message_id)
            .map(|(run, _)| run.clone())
            .collect();
        self.rpc.fence_turns(&run_ids);
        for run in run_ids {
            self.cancel_turn(conversation_id, &run);
        }
        for key in family {
            let pending = self.slot_messages.lock().remove(&key);
            if pending {
                self.pump_next(conversation_id, &key.message);
            }
        }
    }

    async fn wait_message(&self, conversation_id: &str, message_id: &str) {
        loop {
            let notified = self.turns_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self
                .slot_turns
                .lock()
                .values()
                .any(|id| id.key.conversation == conversation_id && id.source == message_id)
            {
                return;
            }
            notified.await;
        }
    }

    fn cancel_all(&self) {
        let slots = self.drain_turns();
        for slot in slots {
            slot.token.cancel();
        }
        self.queue.drain_all();
        self.rpc.turn_ids.release_unbound(None);
        self.slot_messages.lock().clear();
    }

    fn drain_turns(&self) -> Vec<TurnSlot> {
        let mut turns = self.turns.lock();
        let mut slots = Vec::new();
        for (_, batch) in turns.drain() {
            slots.extend(batch);
        }
        slots
    }

    fn on_bridge_lost(&self) {
        let slots = self.drain_turns();
        let mut lost = self.lost_status.lock();
        for slot in slots {
            slot.token.cancel();
            lost.push(LostTurn {
                conversation_id: slot.conversation_id,
                turn_id: slot.turn_id,
            });
        }
        self.hello_done.store(false, Ordering::Release);
        self.auth.clear_hello();
        self.queue.drain_all();
        self.rpc.turn_ids.release_unbound(None);
        self.slot_messages.lock().clear();
    }

    fn memory_scope(&self, account_id: &str, conversation_id: &str) -> String {
        let persona = self.persona.lock().clone();
        accounts::memory_scope(persona.as_ref(), account_id, conversation_id)
    }

    fn store_tool_event(&self, conversation_id: &str, event: &super::rich::ToolEvent) {
        let account_id = self.account_id.lock().clone();
        let Some(account_id) = account_id else {
            return;
        };
        let Some(active) = crate::channels::rich_turn::active_turn() else {
            return;
        };
        let Some(message_id) = self.slot_turns.lock().get(&active.turn_id).cloned() else {
            return;
        };
        let event_id = format!("{}-{}", event.tool_call_id, event.iteration);
        let record = serde_json::json!({
            "tool_call_id": event.tool_call_id,
            "tool_name": event.tool_name,
            "iteration": event.iteration,
            "duration_ms": event.duration_ms,
        });
        let Ok(bytes) = serde_json::to_vec(&record) else {
            tracing::warn!("ramflux tool event record could not be encoded");
            return;
        };
        if let Err(error) = self.store.put_tool_event(
            &self.crypto,
            &account_id,
            conversation_id,
            &event_id,
            &message_id.source,
            &bytes,
        ) {
            tracing::warn!(error = %error, "ramflux tool event was not stored");
        }
    }

    fn store_media(&self, conversation_id: &str, object_id: &str, bytes: &[u8]) {
        let account_id = self.account_id.lock().clone();
        let Some(account_id) = account_id else {
            return;
        };
        let Some(active) = crate::channels::rich_turn::active_turn() else {
            return;
        };
        let Some(message_id) = self.slot_turns.lock().get(&active.turn_id).cloned() else {
            return;
        };
        if let Err(error) = self.store.put_media(
            &self.crypto,
            &account_id,
            conversation_id,
            object_id,
            &message_id.source,
            bytes,
        ) {
            tracing::warn!(error = %error, "ramflux attachment was not cached");
        }
    }

    fn note_slot_message(&self, conversation_id: &str, message_id: &str) {
        self.slot_messages
            .lock()
            .insert(MessageKey::new(conversation_id, message_id));
    }

    fn claim_slot_message(&self, conversation_id: &str, message_id: &str) -> bool {
        self.slot_messages
            .lock()
            .remove(&MessageKey::new(conversation_id, message_id))
    }

    fn note_slot_turn(&self, conversation_id: &str, message_id: &str, turn_id: &str) {
        let source = self.source_message_id(conversation_id, message_id);
        self.slot_turns.lock().insert(
            turn_id.to_owned(),
            TurnMessage {
                key: MessageKey::new(conversation_id, message_id),
                source,
            },
        );
    }

    fn scoped_memory(
        &self,
        inner: Arc<dyn crate::memory::Memory>,
        account_id: &str,
        conversation_id: &str,
    ) -> Arc<dyn crate::memory::Memory> {
        let scope = self.memory_scope(account_id, conversation_id);
        self.scopes
            .lock()
            .entry((account_id.to_string(), conversation_id.to_string()))
            .or_insert_with(|| {
                crate::memory::encrypted_scope::create_ramflux_scoped_memory(
                    inner,
                    Arc::clone(&self.crypto),
                    account_id.to_string(),
                    scope,
                )
            })
            .clone()
    }

    async fn project_task_event(&self, event: &super::rich::TaskEventInbound) {
        let (state, cancel) = match event.kind.as_str() {
            "start" | "accept" => ("running", false),
            "decline" => ("rejected", true),
            "cancel" => ("cancelled", true),
            "resolve" => ("done", false),
            _ => return,
        };
        let Some(turn_id) = self
            .turns
            .lock()
            .get(&event.conversation_id)
            .and_then(|slots| slots.last().map(|slot| slot.turn_id.clone()))
        else {
            return;
        };
        let version = {
            let mut versions = self.task_versions.lock();
            let next = versions.entry(event.task_id.clone()).or_insert(0);
            *next = next.saturating_add(1);
            *next
        };
        if cancel {
            self.cancel_turn(&event.conversation_id, &turn_id);
        }
        let update = super::rich::TaskUpdateReply {
            conversation_id: event.conversation_id.clone(),
            turn_id,
            task_id: event.task_id.clone(),
            version,
            state: state.to_owned(),
            steps: Vec::new(),
            result_ref: None,
            result_summary: None,
        };
        if let Err(error) = outbound::send_task_update(&self.rpc, &update).await {
            tracing::warn!(%error, "ramflux task intent was not projected");
        }
        // Host intent does not manufacture parent turn lifecycle transitions.
    }

    async fn flush_lost_status(&self) {
        let lost = std::mem::take(&mut *self.lost_status.lock());
        for item in lost {
            let status = TurnStatus {
                conversation_id: item.conversation_id,
                turn_id: item.turn_id,
                state: TurnState::Failed,
                error: Some(TurnError {
                    code: "bridge_lost".to_string(),
                    message: "bridge connection lost".to_string(),
                }),
                approval_id: None,
            };
            if let Err(error) = outbound::send_status(&self.rpc, &status).await {
                tracing::warn!(error = %error, "ramflux could not report bridge_lost");
            }
        }
    }
}

/// What `prx channel doctor` prints for this channel before a peer connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub socket_path: PathBuf,
    pub permission: String,
    pub authorization: String,
}

/// Local ramflux agent-host channel.
#[derive(Clone)]
pub struct RamfluxChannel {
    inner: Arc<Inner>,
}

impl RamfluxChannel {
    /// Build a channel. The socket is created when [`Channel::listen`] runs.
    ///
    /// # Errors
    ///
    /// Returns an error when `config` fails [`RamfluxConfig::validate`].
    pub fn new(config: RamfluxConfig, workspace_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let workspace_dir = workspace_dir.as_ref();
        Self::with_secrets(config, workspace_dir, workspace_dir)
    }

    /// Build a channel whose data keys are wrapped by the secret store in `secrets_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error when `config` fails [`RamfluxConfig::validate`] or the
    /// channel data directory cannot be created.
    pub fn with_secrets(
        config: RamfluxConfig,
        workspace_dir: impl AsRef<Path>,
        secrets_dir: impl AsRef<Path>,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        let workspace_dir = workspace_dir.as_ref();
        let socket_path = config.resolve_socket_path(workspace_dir);
        let store = storage::ChannelStore::open(workspace_dir)?;
        let crypto = Arc::new(crypto::AccountCrypto::new(
            secrets_dir.as_ref(),
            store.root(),
            config.encrypt_at_rest,
        ));
        let auth = Authority::from_config(&config);
        let rpc = Arc::new(RpcHub::new());
        let wait_ms = config.turn_wall_clock_secs.saturating_mul(1000);
        let approvals = ApprovalGate::new(Arc::clone(&rpc), Arc::clone(&auth), wait_ms);
        let max_in_flight = usize::from(config.max_in_flight_per_conversation.max(1));
        let queue = turn_queue::TurnQueue::new(config.on_new_message, max_in_flight);
        Ok(Self {
            inner: Arc::new_cyclic(|self_weak| Inner {
                self_weak: self_weak.clone(),
                config,
                admission: Mutex::new(()),
                revocations: AtomicU64::new(0),
                socket_path,
                auth,
                rpc,
                approvals,
                shutdown: CancellationToken::new(),
                hello_done: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                writer_ready: AtomicBool::new(false),
                connection_open: AtomicBool::new(false),
                started: AtomicBool::new(false),
                start_lock: AsyncMutex::new(()),
                last_ping_ms: AtomicU64::new(0),
                account_id: Mutex::new(None),
                inbound_tx: Mutex::new(None),
                turns: Mutex::new(HashMap::new()),
                turns_changed: tokio::sync::Notify::new(),
                tracked: Mutex::new(HashMap::new()),
                derived_messages: Mutex::new(HashMap::new()),
                deleting: Mutex::new(std::collections::HashSet::new()),
                conversations: Mutex::new(HashMap::new()),
                reply_targets: Mutex::new(HashMap::new()),
                attached_memory: Mutex::new(None),
                histories: Mutex::new(None),
                history_sessions: Mutex::new(HashMap::new()),
                attachments: Mutex::new(HashMap::new()),
                task_versions: Mutex::new(HashMap::new()),
                slot_messages: Mutex::new(std::collections::HashSet::new()),
                slot_turns: Mutex::new(HashMap::new()),
                expected_peer: Mutex::new(None),
                scopes: Mutex::new(HashMap::new()),
                message_memories: Mutex::new(HashMap::new()),
                erase_tx: Mutex::new(None),
                eraser: Mutex::new(None),
                eraser_started: AtomicBool::new(false),
                lost_status: Mutex::new(Vec::new()),
                crypto,
                store,
                queue,
                workspace: workspace_dir.to_path_buf(),
                persona: Mutex::new(None),
                #[cfg(test)]
                forward_pause: Mutex::new(None),
                #[cfg(test)]
                forward_abort: Mutex::new(None),
                #[cfg(test)]
                forward_release: Mutex::new(None),
            }),
        })
    }

    /// Approval gate, for in-crate tests of the three decision outcomes.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn approval_gate(&self) -> Arc<ApprovalGate> {
        Arc::clone(&self.inner.approvals)
    }

    /// Stop the listener and drop in-memory data keys.
    pub fn shutdown(&self) {
        self.inner.crypto.lock_all();
        self.inner.shutdown.cancel();
    }

    pub(crate) fn stash_attachments(
        &self,
        conversation_id: &str,
        message_id: &str,
        attachments: Vec<super::rich::AttachmentRef>,
    ) {
        if attachments.is_empty() {
            return;
        }
        self.inner
            .attachments
            .lock()
            .insert(MessageKey::new(conversation_id, message_id), attachments);
    }

    pub(crate) fn take_attachments(&self, conversation_id: &str, message_id: &str) -> Vec<super::rich::AttachmentRef> {
        self.inner
            .attachments
            .lock()
            .remove(&MessageKey::new(conversation_id, message_id))
            .unwrap_or_default()
    }

    pub(crate) fn note_media(&self, conversation_id: &str, message_id: &str, object_id: &str) {
        let Some(account_id) = self.inner.account_id.lock().clone() else {
            return;
        };
        erase_index::note(
            &self.inner.store,
            &self.inner.crypto,
            &account_id,
            &conversation_id,
            &self.inner.source_message_id(conversation_id, message_id),
            None,
            Some(object_id),
            None,
        );
    }

    #[cfg(test)]
    pub(crate) fn set_expected_peer(&self, uid: Option<u32>) {
        *self.inner.expected_peer.lock() = uid;
    }

    pub(crate) fn claim_admitted(&self, conversation_id: &str, message_id: &str, turn_id: &str) -> bool {
        let _admission = self.inner.admission.lock();
        if !self.inner.queue.owns(conversation_id, message_id) {
            return false;
        }
        if self.inner.claim_slot_message(conversation_id, message_id) {
            if !self.inner.rpc.turn_ids.bind(conversation_id, message_id, turn_id) {
                self.inner.pump_next(conversation_id, message_id);
                return false;
            }
            self.inner.note_slot_turn(conversation_id, message_id, turn_id);
            if let Some(tracked) = self
                .inner
                .tracked
                .lock()
                .get(&MessageKey::new(conversation_id, message_id))
            {
                self.inner.approvals.bind_sender(turn_id, tracked.sender.clone());
            }
            return true;
        }
        false
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    #[must_use]
    pub fn config(&self) -> &RamfluxConfig {
        &self.inner.config
    }

    /// Socket and authorization label without opening the channel store.
    #[must_use]
    pub fn describe(config: &RamfluxConfig, workspace: &Path) -> DoctorReport {
        let socket_path = config.resolve_socket_path(workspace);
        DoctorReport {
            socket_path: socket_path.clone(),
            permission: socket::inspect(&socket_path).report(),
            authorization: authorization::doctor_label(config.authorization_mode, None),
        }
    }

    #[must_use]
    pub fn doctor_report(&self) -> DoctorReport {
        DoctorReport {
            socket_path: self.inner.socket_path.clone(),
            permission: socket::inspect(&self.inner.socket_path).report(),
            authorization: self.inner.auth.label(),
        }
    }

    /// Bind the socket and accept connections until [`Self::shutdown`].
    ///
    /// # Errors
    ///
    /// Returns an error when the socket path fails the ownership checks or
    /// cannot be bound.
    fn ensure_eraser(&self) {
        if self.inner.eraser_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let (tx, rx) = mpsc::channel(eraser::QUEUE_CAP);
        *self.inner.erase_tx.lock() = Some(tx);
        let inner = Arc::clone(&self.inner);
        *self.inner.eraser.lock() = Some(tokio::spawn(eraser::run(inner, rx)));
    }

    async fn join_eraser(&self) {
        self.inner.erase_tx.lock().take();
        let handle = self.inner.eraser.lock().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Next outbound frame holds `memory`'s writer until the host acks it.
    #[cfg(test)]
    pub fn arm_writer_hold(&self, memory: &crate::memory::sqlite::SqliteMemory) -> tokio::sync::oneshot::Receiver<()> {
        self.inner.rpc.arm_writer_hold(memory.writer_hold())
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        self.ensure_eraser();
        let _guard = self.inner.start_lock.lock().await;
        if self.inner.started.load(Ordering::Acquire) {
            return Ok(());
        }
        #[cfg(not(unix))]
        return Err(socket::SocketError::Invalid("ramflux channel requires a Unix domain socket".to_string()).into());
        #[cfg(unix)]
        {
            let listener = socket::bind(&self.inner.socket_path)?;
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                socket::accept_loop(inner, listener).await;
            });
            self.inner.started.store(true, Ordering::Release);
            Ok(())
        }
    }
}

#[async_trait]
impl Channel for RamfluxChannel {
    fn name(&self) -> &str {
        "ramflux"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let turn_id = crate::channels::rich_turn::active_turn()
            .map(|turn| turn.turn_id)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let reply_target = self.inner.reply_target_for(&message.recipient);
        self.send_rich(&RichReply::Text(TextReply {
            conversation_id: message.recipient.clone(),
            turn_id,
            text: message.content.clone(),
            is_final: true,
            reply_target,
        }))
        .await
    }

    async fn listen(&self, tx: mpsc::Sender<crate::channels::traits::ChannelMessage>) -> anyhow::Result<()> {
        let (rich_tx, mut rich_rx) = mpsc::channel(64);
        let forward = async {
            let mut forwarding = tokio::task::JoinSet::new();
            loop {
                let event = tokio::select! {
                    () = self.inner.shutdown.cancelled() => break,
                    event = rich_rx.recv() => match event { Some(event) => event, None => break },
                };
                let (message, reservation) = match event {
                    RichInbound::Message(message) => {
                        self.stash_attachments(
                            &message.conversation_id,
                            &message.message_id,
                            message.attachments.clone(),
                        );
                        (message.to_channel_message(), message.reservation.clone())
                    }
                    RichInbound::SurfaceAction(action) => {
                        let chat = self.inner.conversations.lock().get(&action.conversation_id).cloned();
                        let Some(chat) = chat else {
                            tracing::warn!("ramflux action has no conversation metadata");
                            continue;
                        };
                        // Surface values are user input, never approval authority.
                        // Only the dedicated host decision can resolve a waiter.
                        let content = serde_json::json!({"event":"surface_action", "message_id":action.message_id, "surface_id":action.surface_id, "action_id":action.action_id, "values":action.values}).to_string();
                        let derived_id = action.derived_message_id;
                        let params = serde_json::json!({
                            "conversation_id":action.conversation_id,
                            "message_id":derived_id,
                            "sender":action.sender, "chat":chat, "text":content, "timestamp_ms":unix_ms(),
                        });
                        // Deliver an admitted action at this position in the
                        // input stream. This loop must never send to rich_tx,
                        // whose only consumer is this same loop.
                        let admitted = match inbound::prepare_reserved_message(&self.inner, &params, action.reservation)
                        {
                            Ok(Some(turn_queue::Admission::Interrupt(message))) => {
                                self.inner.cancel_tokens(&message.conversation_id);
                                message
                            }
                            Ok(Some(turn_queue::Admission::Start(message))) => message,
                            Ok(Some(turn_queue::Admission::Rejected { depth })) => {
                                tracing::warn!(depth, "ramflux action queue is full");
                                continue;
                            }
                            Ok(Some(turn_queue::Admission::Queued) | None) => continue,
                            Err(error) => {
                                tracing::warn!(%error, "ramflux action was not admitted");
                                continue;
                            }
                        };
                        (admitted.to_channel_message(), admitted.reservation.clone())
                    }
                    RichInbound::TaskEvent(event) => {
                        self.inner.project_task_event(&event).await;
                        continue;
                    }
                    RichInbound::Control(_) | RichInbound::MessageDeleted(_) => continue,
                };
                let inner = Arc::clone(&self.inner);
                let sender = tx.clone();
                #[cfg(test)]
                let (entered, pause) = match { inner.forward_pause.lock().take() } {
                    Some((entered, pause)) => (Some(entered), Some(pause)),
                    None => (None, None),
                };
                let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
                let slot = ForwardSlot {
                    inner,
                    conversation: message.reply_target.clone(),
                    message: message.id.clone(),
                    armed: true,
                    reservation: reservation.clone(),
                };
                let handle = forwarding.spawn(async move {
                    let _slot = slot;
                    #[cfg(test)]
                    if let Some(barrier) = pause {
                        barrier.wait().await;
                    }
                    let delivered = sender.send(message).await.is_ok();
                    let _ = delivered_tx.send(delivered);
                    if delivered {
                        if let Some(reservation) = reservation {
                            reservation.transferred.cancelled().await;
                        }
                    }
                });
                #[cfg(test)]
                {
                    *self.inner.forward_abort.lock() = Some(handle);
                    if let Some(entered) = entered {
                        let _ = entered.send(());
                    }
                }
                #[cfg(not(test))]
                drop(handle);
                // Keep the original bounded send backpressure. The task only
                // outlives this handoff while a dispatcher is about to bind it.
                let delivered = tokio::select! {
                    () = self.inner.shutdown.cancelled() => break,
                    delivered = delivered_rx => delivered,
                };
                if matches!(delivered, Ok(false)) {
                    anyhow::bail!("channel dispatcher closed");
                }
                while forwarding.try_join_next().is_some() {}
            }
            Ok(())
        };
        let listen = self.listen_rich(rich_tx);
        let outcome = tokio::try_join!(listen, forward);
        self.inner.cancel_all();
        outcome?;
        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.inner.healthy()
    }

    fn outbound_attachment(&self, _text: &str) -> Option<crate::channels::traits::OutboundAttachment> {
        None
    }

    fn supports_draft_updates(&self) -> bool {
        false
    }

    fn as_rich(&self) -> Option<&dyn RichChannel> {
        Some(self)
    }
}

#[async_trait]
impl RichChannel for RamfluxChannel {
    async fn listen_rich(&self, tx: mpsc::Sender<RichInbound>) -> anyhow::Result<()> {
        self.ensure_eraser();
        *self.inner.inbound_tx.lock() = Some(tx);
        self.start().await?;
        self.inner.shutdown.cancelled().await;
        self.join_eraser().await;
        Ok(())
    }

    async fn send_rich(&self, reply: &RichReply) -> anyhow::Result<()> {
        outbound::send_reply(&self.inner, reply).await
    }

    fn approval_resolver(&self) -> Option<Arc<dyn crate::agent::turn_contracts::ApprovalResolver>> {
        Some(Arc::clone(&self.inner.approvals) as Arc<dyn crate::agent::turn_contracts::ApprovalResolver>)
    }

    fn bind_turn(&self, conversation_id: &str, turn_id: &str, token: CancellationToken) {
        self.inner.bind_turn(conversation_id, turn_id, token);
    }

    fn unbind_turn(&self, conversation_id: &str, turn_id: &str) {
        self.inner.unbind_turn(conversation_id, turn_id);
        self.inner.approvals.unbind_sender(turn_id);
    }

    async fn on_tool_event(&self, event: &super::rich::ToolEvent) {
        let Some(active) = crate::channels::rich_turn::active_turn() else {
            tracing::warn!("ramflux tool event dropped because no turn is active");
            return;
        };
        let turn = (active.conversation_id, active.turn_id);
        let state = match event.kind {
            super::rich::ToolEventKind::Started | super::rich::ToolEventKind::Progress => TurnState::ToolRunning,
            super::rich::ToolEventKind::Finished | super::rich::ToolEventKind::Failed => TurnState::Thinking,
        };
        let status = TurnStatus {
            conversation_id: turn.0.clone(),
            turn_id: turn.1.clone(),
            state,
            error: None,
            approval_id: None,
        };
        if let Err(error) = outbound::send_status(&self.inner.rpc, &status).await {
            tracing::warn!(%error, "ramflux tool lifecycle status was not sent");
        }
        self.inner.store_tool_event(&turn.0, event);
        if let Err(error) = outbound::send_tool_event(&self.inner.rpc, &turn.0, &turn.1, event).await {
            tracing::warn!(error = %error, tool = %event.tool_name, "ramflux tool event was not sent");
        }
        let state = match event.kind {
            super::rich::ToolEventKind::Started | super::rich::ToolEventKind::Progress => "running",
            super::rich::ToolEventKind::Finished => "done",
            super::rich::ToolEventKind::Failed => "failed",
        };
        let update = super::rich::TaskUpdateReply {
            conversation_id: turn.0.clone(),
            turn_id: turn.1.clone(),
            task_id: event.tool_call_id.clone(),
            version: {
                let mut versions = self.inner.task_versions.lock();
                let next = versions.entry(event.tool_call_id.clone()).or_insert(0);
                *next = next.saturating_add(1);
                *next
            },
            state: state.to_string(),
            steps: Vec::new(),
            result_ref: None,
            result_summary: None,
        };
        if let Err(error) = outbound::send_task_update(&self.inner.rpc, &update).await {
            tracing::debug!(error = %error, "ramflux task update was not sent");
        }
    }

    fn forces_tool_approval(&self) -> bool {
        self.inner.auth.effective() == authorization::EffectiveAutonomy::Supervised
    }

    fn turn_budget(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(
            self.inner.config.turn_wall_clock_secs.max(1),
        ))
    }

    fn augment_prompt(&self, prompt: &str, _conversation_id: &str) -> String {
        let extra = self
            .inner
            .persona
            .lock()
            .as_ref()
            .and_then(|persona| persona.system_prompt.clone());
        match extra {
            Some(extra) if !extra.is_empty() => format!("{extra}\n\n{prompt}"),
            _ => prompt.to_string(),
        }
    }

    fn preferred_model(&self) -> Option<String> {
        self.inner
            .persona
            .lock()
            .as_ref()
            .and_then(|persona| persona.model.clone())
    }

    fn preferred_provider(&self) -> Option<String> {
        self.inner
            .persona
            .lock()
            .as_ref()
            .and_then(|persona| persona.provider.clone())
    }

    fn memory_for_turn(
        &self,
        inner: Arc<dyn crate::memory::Memory>,
        conversation_id: &str,
    ) -> Arc<dyn crate::memory::Memory> {
        let Some(account_id) = self.inner.account_id.lock().clone() else {
            return inner;
        };
        self.inner.scoped_memory(inner, &account_id, conversation_id)
    }

    fn inbound_idempotency_key(&self, conversation_id: &str, message_id: &str) -> Option<String> {
        let account = self.inner.account_id.lock().clone()?;
        Some(format!(
            "channel:ramflux:{}:{}:{message_id}",
            crypto::id_hash(&account),
            crypto::id_hash(conversation_id)
        ))
    }

    fn memory_for_message(
        &self,
        inner: Arc<dyn crate::memory::Memory>,
        conversation_id: &str,
        message_id: &str,
    ) -> anyhow::Result<Arc<dyn crate::memory::Memory>> {
        let account = self
            .inner
            .account_id
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bridge hello required"))?;
        let key = MessageKey::new(conversation_id, message_id);
        let cached = self.inner.message_memories.lock().get(&key).cloned();
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let memory = self
            .inner
            .scoped_memory(inner, &account, conversation_id)
            .with_erasure_origin(crate::memory::erasure::ErasureOrigin::new(
                &account,
                conversation_id,
                &self.inner.source_message_id(conversation_id, message_id),
            ))?;
        Ok(self.inner.message_memories.lock().entry(key).or_insert(memory).clone())
    }

    fn accepts_message(&self, conversation_id: &str, message_id: &str) -> bool {
        let Some(account) = self.inner.account_id.lock().clone() else {
            return false;
        };
        let origin = crate::memory::erasure::ErasureOrigin::new(
            &account,
            conversation_id,
            &self.inner.source_message_id(conversation_id, message_id),
        );
        match self.inner.store.backend().and_then(|store| store.is_deleted(&origin)) {
            Ok(deleted) => !deleted && self.inner.queue.owns(conversation_id, message_id),
            Err(error) => {
                tracing::warn!(%error, "ramflux input admission refused");
                false
            }
        }
    }

    fn claim_slot(&self, conversation_id: &str, message_id: &str, turn_id: &str) {
        self.claim_admitted(conversation_id, message_id, turn_id);
    }

    fn try_claim_slot(&self, conversation_id: &str, message_id: &str, turn_id: &str) -> bool {
        self.claim_admitted(conversation_id, message_id, turn_id)
    }

    fn record_erasure(&self, conversation_id: &str, message_id: &str, memory_key: &str, history_key: &str) {
        let Some(account_id) = self.inner.account_id.lock().clone() else {
            return;
        };
        erase_index::note(
            &self.inner.store,
            &self.inner.crypto,
            &account_id,
            &conversation_id,
            &self.inner.source_message_id(conversation_id, message_id),
            Some(memory_key),
            None,
            Some(history_key),
        );
    }

    fn record_history_session(&self, conversation_id: &str, session: &str) {
        self.inner
            .history_sessions
            .lock()
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(session.to_owned());
    }

    async fn hydrate_history(&self, conversation_id: &str) -> anyhow::Result<()> {
        let account = self
            .inner
            .account_id
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bridge hello required"))?;
        if !self.inner.history_sessions.lock().contains_key(conversation_id) {
            hooks::refresh_history(&self.inner, &account, conversation_id)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    async fn enrich_inbound(&self, content: &str, conversation_id: &str, message_id: &str) -> String {
        hooks::enrich_inbound(self, content, conversation_id, message_id).await
    }

    async fn project_final_surface(&self, conversation_id: &str, turn_id: &str, text: &str) -> anyhow::Result<()> {
        hooks::project_final_surface(&self.inner, conversation_id, turn_id, text).await
    }

    fn attach_stores(
        &self,
        memory: Arc<dyn crate::memory::Memory>,
        histories: Arc<parking_lot::Mutex<HashMap<String, Vec<crate::providers::ChatMessage>>>>,
    ) {
        match memory.erasure_store() {
            Ok(store) => self.inner.store.attach(store),
            Err(error) => {
                self.inner.store.detach();
                tracing::debug!(%error, "ramflux transactional retention unavailable");
            }
        }
        *self.inner.attached_memory.lock() = Some(memory);
        *self.inner.histories.lock() = Some(histories);
    }
}

impl RamfluxChannel {
    /// Fetch one inbound attachment. Bytes are not logged.
    ///
    /// # Errors
    ///
    /// Returns an error when no turn is active, the host rejects the fetch,
    /// or the payload is not valid base64.
    pub async fn fetch_attachment(&self, object_id: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
        let turn = crate::channels::rich_turn::active_turn()
            .ok_or_else(|| anyhow::anyhow!("ramflux attachment fetch has no active turn"))?;
        let fetched = outbound::fetch_attachment(
            &self.inner.rpc,
            &turn.conversation_id,
            &turn.turn_id,
            object_id,
            max_bytes,
        )
        .await?;
        if fetched.mime.is_empty() {
            anyhow::bail!("attachment mime is empty");
        }
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, fetched.bytes_b64)
            .map_err(|error| anyhow::anyhow!("attachment bytes are not base64: {error}"))?;
        self.inner.store_media(&turn.conversation_id, object_id, &bytes);
        Ok(bytes)
    }

    /// Encrypting memory view for the account bound by `bridge.hello`.
    ///
    /// Returns `None` before hello. The inner backend is unchanged; only this
    /// namespace sees ciphertext.
    #[must_use]
    pub fn scoped_memory(&self, inner: Arc<dyn crate::memory::Memory>) -> Option<Arc<dyn crate::memory::Memory>> {
        let account_id = self.inner.account_id.lock().clone()?;
        let scope = self.inner.memory_scope(&account_id, &account_id);
        Some(crate::memory::encrypted_scope::create_ramflux_scoped_memory(
            inner,
            Arc::clone(&self.inner.crypto),
            account_id,
            scope,
        ))
    }
}

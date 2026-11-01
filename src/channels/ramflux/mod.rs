//! Ramflux channel: a same-user Unix socket bridge.
//!
//! prx does not hold ramflux keys, does not sign, and does not know nodes or
//! gateways. The agent-host owns everything beyond the socket.

mod accounts;
mod approval;
mod authorization;
pub mod crypto;
mod erase;
mod hooks;
mod inbound;
mod outbound;
mod protocol;
mod rpc;
mod socket;
mod storage;
mod surface_schema;
mod turn_queue;

#[cfg(test)]
mod tests;

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

pub(crate) struct TrackedMessage {
    pub content: String,
    pub memory_key: String,
    pub objects: Vec<String>,
}

pub(crate) struct Inner {
    config: RamfluxConfig,
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
    tracked: Mutex<HashMap<String, TrackedMessage>>,
    reply_targets: Mutex<HashMap<String, String>>,
    attached_memory: Mutex<Option<Arc<dyn crate::memory::Memory>>>,
    histories: Mutex<Option<super::ConversationHistoryMap>>,
    lost_status: Mutex<Vec<LostTurn>>,
    crypto: Arc<crypto::AccountCrypto>,
    store: storage::ChannelStore,
    queue: turn_queue::TurnQueue,
    workspace: PathBuf,
    persona: Mutex<Option<accounts::Persona>>,
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
        }
        Ok(())
    }

    fn track_message(&self, message_id: &str, tracked: TrackedMessage) {
        self.tracked.lock().insert(message_id.to_string(), tracked);
    }

    fn take_tracked(&self, message_id: &str) -> Option<TrackedMessage> {
        self.tracked.lock().remove(message_id)
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
        let mut turns = self.turns.lock();
        let Some(slots) = turns.get_mut(conversation_id) else {
            return;
        };
        slots.retain(|slot| slot.turn_id != turn_id);
        if slots.is_empty() {
            turns.remove(conversation_id);
            drop(turns);
            self.pump_next(conversation_id);
        }
    }

    fn cancel_tokens(&self, conversation_id: &str) {
        let slots = self.turns.lock().remove(conversation_id);
        if let Some(slots) = slots {
            for slot in slots {
                slot.token.cancel();
            }
        }
    }

    fn cancel_conversation(&self, conversation_id: &str) {
        self.cancel_tokens(conversation_id);
        self.queue.drain(conversation_id);
    }

    fn cancel_turn(&self, conversation_id: &str, turn_id: &str) -> bool {
        let mut turns = self.turns.lock();
        let Some(slots) = turns.get_mut(conversation_id) else {
            return false;
        };
        let Some(index) = slots.iter().position(|slot| slot.turn_id == turn_id) else {
            return false;
        };
        let slot = slots.remove(index);
        slot.token.cancel();
        if slots.is_empty() {
            turns.remove(conversation_id);
            drop(turns);
            self.pump_next(conversation_id);
        }
        true
    }

    fn pump_next(&self, conversation_id: &str) {
        let Some(next) = self.queue.finish(conversation_id) else {
            return;
        };
        let tx = self.inbound_tx.lock().clone();
        if let Some(tx) = tx {
            tokio::spawn(async move {
                if tx.send(RichInbound::Message(next)).await.is_err() {
                    tracing::debug!("ramflux queued message was not delivered");
                }
            });
        }
    }

    fn cancel_all(&self) {
        let slots = self.drain_turns();
        for slot in slots {
            slot.token.cancel();
        }
        self.queue.drain_all();
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
        let scope = self.memory_scope(&account_id, conversation_id);
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
        if let Err(error) = self
            .store
            .put_tool_event(&self.crypto, &account_id, &scope, &event_id, &bytes)
        {
            tracing::warn!(error = %error, "ramflux tool event was not stored");
        }
    }

    fn store_media(&self, conversation_id: &str, object_id: &str, bytes: &[u8]) {
        let account_id = self.account_id.lock().clone();
        let Some(account_id) = account_id else {
            return;
        };
        let scope = self.memory_scope(&account_id, conversation_id);
        if let Err(error) = self
            .store
            .put_media(&self.crypto, &account_id, &scope, object_id, bytes)
        {
            tracing::warn!(error = %error, "ramflux attachment was not cached");
        }
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
            inner: Arc::new(Inner {
                config,
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
                tracked: Mutex::new(HashMap::new()),
                reply_targets: Mutex::new(HashMap::new()),
                attached_memory: Mutex::new(None),
                histories: Mutex::new(None),
                lost_status: Mutex::new(Vec::new()),
                crypto,
                store,
                queue,
                workspace: workspace_dir.to_path_buf(),
                persona: Mutex::new(None),
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
    pub async fn start(&self) -> anyhow::Result<()> {
        let _guard = self.inner.start_lock.lock().await;
        if self.inner.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let listener = socket::bind(&self.inner.socket_path)?;
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            socket::accept_loop(inner, listener).await;
        });
        self.inner.started.store(true, Ordering::Release);
        Ok(())
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
            while let Some(event) = rich_rx.recv().await {
                let message = match event {
                    RichInbound::Message(message) => message.to_channel_message(),
                    RichInbound::SurfaceAction(action) => hooks::inbound_as_message(
                        &action.message_id,
                        &action.sender.principal_id,
                        &action.conversation_id,
                        &format!(
                            "surface action {} on {} version {}: {}",
                            action.action_id, action.surface_id, action.version, action.values
                        ),
                        action.sender.display_name.clone(),
                    ),
                    RichInbound::TaskEvent(event) => hooks::inbound_as_message(
                        &event.task_id,
                        &event.sender.principal_id,
                        &event.conversation_id,
                        &format!("task event {} {}: {}", event.task_id, event.kind, event.body),
                        event.sender.display_name.clone(),
                    ),
                    RichInbound::Control(_) | RichInbound::MessageDeleted(_) => continue,
                };
                if tx.send(message).await.is_err() {
                    break;
                }
            }
            Ok(())
        };
        let listen = self.listen_rich(rich_tx);
        tokio::try_join!(listen, forward)?;
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
        self.start().await?;
        *self.inner.inbound_tx.lock() = Some(tx);
        self.inner.shutdown.cancelled().await;
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
    }

    async fn on_tool_event(&self, event: &super::rich::ToolEvent) {
        let Some(active) = crate::channels::rich_turn::active_turn() else {
            tracing::warn!("ramflux tool event dropped because no turn is active");
            return;
        };
        let turn = (active.conversation_id, active.turn_id);
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
            version: event.iteration.max(1),
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
        let scope = self.inner.memory_scope(&account_id, conversation_id);
        crate::memory::encrypted_scope::create_ramflux_scoped_memory(
            inner,
            Arc::clone(&self.inner.crypto),
            account_id,
            scope,
        )
    }

    async fn enrich_inbound(&self, content: &str, _conversation_id: &str, _turn_id: &str) -> String {
        hooks::enrich_inbound(self, content).await
    }

    async fn project_final_surface(&self, conversation_id: &str, turn_id: &str, text: &str) -> anyhow::Result<()> {
        hooks::project_final_surface(&self.inner, conversation_id, turn_id, text).await
    }

    fn attach_stores(
        &self,
        memory: Arc<dyn crate::memory::Memory>,
        histories: Arc<parking_lot::Mutex<HashMap<String, Vec<crate::providers::ChatMessage>>>>,
    ) {
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

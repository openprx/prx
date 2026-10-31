//! Ramflux channel: a same-user Unix socket bridge.
//!
//! prx does not hold ramflux keys, does not sign, and does not know nodes or
//! gateways. The agent-host owns everything beyond the socket.

mod accounts;
mod approval;
mod authorization;
pub mod crypto;
mod erase;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnHint {
    pub conversation_id: String,
    pub turn_id: String,
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
    deleted: Mutex<Vec<(String, String)>>,
    lost_status: Mutex<Vec<LostTurn>>,
    crypto: Arc<crypto::AccountCrypto>,
    store: storage::ChannelStore,
    queue: turn_queue::TurnQueue,
    hint: Arc<Mutex<Option<TurnHint>>>,
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

    fn note_deleted(&self, conversation_id: &str, message_id: &str) {
        self.deleted
            .lock()
            .push((conversation_id.to_string(), message_id.to_string()));
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
            if let Some(next) = self.queue.pop_next(conversation_id) {
                let tx = self.inbound_tx.lock().clone();
                if let Some(tx) = tx {
                    tokio::spawn(async move {
                        if tx.send(RichInbound::Message(next)).await.is_err() {
                            tracing::debug!("ramflux queued message was not delivered");
                        }
                    });
                }
            }
        }
    }

    fn cancel_conversation(&self, conversation_id: &str) {
        let slots = self.turns.lock().remove(conversation_id);
        if let Some(slots) = slots {
            for slot in slots {
                slot.token.cancel();
            }
        }
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
        }
        true
    }

    fn cancel_all(&self) {
        let slots = self.drain_turns();
        for slot in slots {
            slot.token.cancel();
        }
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
        let hint = Arc::new(Mutex::new(None));
        let approvals = ApprovalGate::new(Arc::clone(&rpc), Arc::clone(&auth), wait_ms, Arc::clone(&hint));
        let queue = turn_queue::TurnQueue::new(config.on_new_message);
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
                deleted: Mutex::new(Vec::new()),
                lost_status: Mutex::new(Vec::new()),
                crypto,
                store,
                queue,
                hint,
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
        self.send_rich(&RichReply::Text(TextReply {
            conversation_id: message.recipient.clone(),
            turn_id,
            text: message.content.clone(),
            is_final: true,
            reply_target: None,
        }))
        .await
    }

    async fn listen(&self, tx: mpsc::Sender<crate::channels::traits::ChannelMessage>) -> anyhow::Result<()> {
        let (rich_tx, mut rich_rx) = mpsc::channel(64);
        let forward = async {
            while let Some(event) = rich_rx.recv().await {
                if let RichInbound::Message(message) = event {
                    if tx.send(message.to_channel_message()).await.is_err() {
                        break;
                    }
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
        let active = crate::channels::rich_turn::active_turn();
        let hint = self.inner.hint.lock().clone();
        let turn = if let Some(active) = active {
            (active.conversation_id, active.turn_id)
        } else if let Some(hint) = hint {
            (hint.conversation_id, hint.turn_id)
        } else {
            tracing::warn!("ramflux tool event dropped because no turn is active");
            return;
        };
        self.inner.store_tool_event(&turn.0, event);
        if let Err(error) = outbound::send_tool_event(&self.inner.rpc, &turn.0, &turn.1, event).await {
            tracing::warn!(error = %error, tool = %event.tool_name, "ramflux tool event was not sent");
        }
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
        Some(crate::memory::encrypted_scope::create_ramflux_scoped_memory(
            inner,
            Arc::clone(&self.inner.crypto),
            account_id,
        ))
    }
}

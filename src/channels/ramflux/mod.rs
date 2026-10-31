//! Ramflux channel: a same-user Unix socket bridge.
//!
//! prx does not hold ramflux keys, does not sign, and does not know nodes or
//! gateways. The agent-host owns everything beyond the socket.

mod approval;
mod authorization;
mod inbound;
mod outbound;
mod protocol;
mod rpc;
mod socket;

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
        config.validate()?;
        let workspace_dir = workspace_dir.as_ref();
        let socket_path = config.resolve_socket_path(workspace_dir);
        let auth = Authority::from_config(&config);
        let rpc = Arc::new(RpcHub::new());
        let wait_ms = config.turn_wall_clock_secs.saturating_mul(1000);
        let approvals = ApprovalGate::new(Arc::clone(&rpc), Arc::clone(&auth), wait_ms);
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
            }),
        })
    }

    /// Approval gate, for in-crate tests of the three decision outcomes.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn approval_gate(&self) -> Arc<ApprovalGate> {
        Arc::clone(&self.inner.approvals)
    }

    /// Stop the listener. In-flight bridge tasks observe the token.
    pub fn shutdown(&self) {
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
            reply_to: None,
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
        let Some(turn) = crate::channels::rich_turn::active_turn() else {
            tracing::warn!("ramflux tool event dropped because no turn is active");
            return;
        };
        if let Err(error) =
            outbound::send_tool_event(&self.inner.rpc, &turn.conversation_id, &turn.turn_id, event).await
        {
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
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, fetched.bytes_b64)
            .map_err(|error| anyhow::anyhow!("attachment bytes are not base64: {error}"))
    }
}

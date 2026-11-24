//! Runtime-owned sender identity, durable local delivery and inbox wakeups.
mod turns;
use super::mailbox::{MailboxAuthority, MailboxError, MailboxPublished, MailboxReceipt, MailboxResult, MailboxScope};
use super::send::{MailboxDestination, MailboxResolver, MailboxSender};
use super::{PrxMessage, SessionAddr};
use crate::memory::sqlite::mailbox::{SqliteMailbox, SqliteMailboxSender};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::mpsc;
pub(crate) use turns::MessageTurnGuard;

type RuntimeRegistry = Mutex<HashMap<PathBuf, Weak<MessagingRuntime>>>;
static RUNTIMES: OnceLock<RuntimeRegistry> = OnceLock::new();

pub struct MessagingRuntime {
    turns: Mutex<HashMap<uuid::Uuid, HashMap<uuid::Uuid, turns::Active>>>,
    caches: Mutex<HashMap<uuid::Uuid, HashMap<String, turns::Cached>>>,
    pub(crate) store: SqliteMailbox,
    pub(crate) workspace: String,
    pub(crate) instance: String,
    config: RwLock<Arc<crate::config::Config>>,
    inboxes: Mutex<HashMap<SessionAddr, (uuid::Uuid, uuid::Uuid, mpsc::Sender<()>)>>,
    ready: Mutex<HashMap<SessionAddr, std::collections::VecDeque<MessageInput>>>,
}

#[derive(Clone)]
pub struct MessageInput {
    _lease: Arc<dyn Send + Sync>,
    scope: uuid::Uuid,
    pub message: PrxMessage,
    pub receipt: MailboxReceipt,
    pub text: String,
}
impl MessageInput {
    pub(crate) fn verify(&self, runtime: &MessagingRuntime) -> MailboxResult<()> {
        let event = self
            .receipt
            .adopted_message_event_id
            .ok_or(MailboxError::CorruptState)?;
        runtime.store.validate_input(
            &runtime.workspace,
            &self.message.to,
            event,
            &self.text,
            self.message.id,
            &self.receipt,
        )
    }

    pub const fn auto_start(&self) -> bool {
        self.receipt.chain.depth < 8
    }
}

pub struct SessionInbox {
    registration: uuid::Uuid,
    runtime: Arc<MessagingRuntime>,
    address: SessionAddr,
    scope: MailboxScope,
    receiver: mpsc::Receiver<()>,
}
impl Drop for SessionInbox {
    fn drop(&mut self) {
        let mut inboxes = self.runtime.inboxes.lock();
        if inboxes
            .get(&self.address)
            .is_some_and(|(_, registration, _)| *registration == self.registration)
        {
            inboxes.remove(&self.address);
        }
    }
}

impl MessagingRuntime {
    pub fn for_config(config: &crate::config::Config) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            config.memory.backend == "sqlite",
            "session messaging requires the SQLite mailbox backend"
        );
        let path = config.workspace_dir.clone();
        let mut runtimes = RUNTIMES.get_or_init(Mutex::default).lock();
        if let Some(runtime) = runtimes.get(&path).and_then(Weak::upgrade) {
            *runtime.config.write() = Arc::new(config.clone());
            return Ok(runtime);
        }
        let memory = crate::memory::SqliteMemory::new(&path)?;
        let key = crate::security::runtime_key::DecisionKey::load_or_create(&path)?;
        let secret = key.hmac(
            crate::security::runtime_key::KeyDomain::StateFingerprint,
            &[b"prx-mailbox-pins-v1"],
        );
        let instance = if config.a2a.messaging_enabled {
            super::a2a::identity::load(config)?.spiffe_id
        } else {
            super::a2a::identity::optional(config)?
                .map_or_else(|| config.a2a.agent_id.clone(), |identity| identity.spiffe_id)
        };
        let store = SqliteMailbox::new(
            memory,
            instance.clone(),
            secret,
            Arc::new(super::mailbox::SystemMailboxClock),
        )?;
        let runtime = Arc::new(Self {
            turns: Default::default(),
            caches: Default::default(),
            store,
            workspace: path.to_string_lossy().into_owned(),
            instance,
            config: RwLock::new(Arc::new(config.clone())),
            inboxes: Mutex::new(HashMap::new()),
            ready: Mutex::new(HashMap::new()),
        });
        let drainer: Arc<dyn super::mailbox::MailboxCacheDrainer> = runtime.clone();
        runtime.store.register_drainer(&drainer);
        if config.a2a.messaging_enabled {
            super::a2a::RemoteTransport::new(config).start(runtime.clone())?;
        }
        runtimes.insert(path, Arc::downgrade(&runtime));
        Ok(runtime)
    }

    pub fn address(&self, owner: &str, session: &str, agent: &str) -> SessionAddr {
        SessionAddr::new(&self.instance, owner, session, Some(agent))
    }

    pub fn register(self: &Arc<Self>, address: SessionAddr) -> MailboxResult<SessionInbox> {
        let scope = self.store.session_scope(&self.workspace, &address, true)?;
        let (sender, receiver) = mpsc::channel(1);
        let registration = uuid::Uuid::new_v4();
        match self.inboxes.lock().entry(address.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((scope.scope_id, registration, sender));
            }
            std::collections::hash_map::Entry::Occupied(_) => return Err(MailboxError::IdentityImmutable),
        }
        Ok(SessionInbox {
            registration,
            runtime: Arc::clone(self),
            address,
            scope,
            receiver,
        })
    }

    pub(crate) fn register_backend(self: &Arc<Self>, address: SessionAddr) -> MailboxResult<Option<SessionInbox>> {
        match self.register(address) {
            Ok(inbox) => Ok(Some(inbox)),
            Err(MailboxError::IdentityImmutable) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub fn inbound(&self, target: &SessionAddr) -> bool {
        let configured = self.config.read().clone();
        let config: crate::config::Config = if configured.config_path.exists() {
            match crate::config::files::read_merged_toml_with_gate(&configured.config_path)
                .and_then(|tree| tree.try_into().map_err(Into::into))
            {
                Ok(config) => config,
                Err(_) => return false,
            }
        } else {
            (*configured).clone()
        };
        if target.agent == "main" {
            config.agent.messaging.inbound
        } else {
            config
                .agents
                .get(&target.agent)
                .is_some_and(|agent| agent.enabled && agent.messaging.inbound)
        }
    }

    pub fn resolve_scope(&self, target: &SessionAddr) -> MailboxResult<MailboxScope> {
        if !self.inbound(target) {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::InboundDisabledOrConfigInvalid,
            ));
        }
        let registered = self.inboxes.lock().contains_key(target);
        self.store.session_scope(&self.workspace, target, registered)
    }

    pub(crate) fn ready_capacity(&self, target: &SessionAddr) -> bool {
        self.ready.lock().get(target).is_none_or(|queue| queue.len() < 64)
    }
    pub(crate) fn adopted(&self, message: PrxMessage, receipt: MailboxReceipt) -> MailboxResult<()> {
        let text = crate::session_worker::mailbox::user_text(&message)?;
        let target = message.to.clone();
        let scope = self.store.session_scope(&self.workspace, &target, false)?;
        let lease = self.store.hold_input(&scope, &target)?;
        let input = MessageInput {
            _lease: lease,
            scope: scope.scope_id,
            message,
            receipt,
            text,
        };
        self.requeue_back(input)?;
        self.wake(&target);
        Ok(())
    }
    fn requeue_back(&self, input: MessageInput) -> MailboxResult<()> {
        self.queue_input(input, false)
    }
    fn queue_input(&self, input: MessageInput, front: bool) -> MailboxResult<()> {
        let target = input.message.to.clone();
        let event = input
            .receipt
            .adopted_message_event_id
            .ok_or(MailboxError::CorruptState)?;
        self.store.with_input(
            &self.workspace,
            &target,
            event,
            &input.text,
            input.message.id,
            &input.receipt,
            || {
                let mut ready = self.ready.lock();
                let queue = ready.entry(target.clone()).or_default();
                if queue.len() >= 64 {
                    return Err(MailboxError::QueueFull);
                }
                if front {
                    queue.push_front(input.clone());
                } else {
                    queue.push_back(input.clone());
                }
                Ok(())
            },
        )?
    }
    pub fn wake(&self, target: &SessionAddr) {
        if let Some((_, _, sender)) = self.inboxes.lock().get(target) {
            let _ = sender.try_send(());
        }
    }
    pub(crate) fn requeue(&self, input: MessageInput) -> MailboxResult<()> {
        self.queue_input(input, true)
    }

    pub fn bind(self: &Arc<Self>, source: SessionAddr, event: Option<&str>) -> MailboxResult<RuntimeSender> {
        let mut source = source;
        if let Some(agent) = self
            .store
            .logical_agent(&self.workspace, &source.owner, &source.session)?
        {
            source.agent = agent;
        }
        let scope = self.store.session_scope(&self.workspace, &source, true)?;
        let chain = event
            .map(|event| self.store.source_chain(&self.workspace, &source.owner, event))
            .transpose()?
            .flatten();
        let sender = SqliteMailboxSender::new(
            self.store.clone(),
            source.clone(),
            scope.clone(),
            Arc::new(LocalResolver {
                runtime: Arc::clone(self),
                source,
            }),
            None,
        )?
        .with_chain(chain);
        Ok(RuntimeSender {
            runtime: Arc::clone(self),
            sender,
            scope,
        })
    }

    pub fn alias(&self, source: &SessionAddr, target: &serde_json::Value) -> MailboxResult<SessionAddr> {
        let mut address = match target {
            serde_json::Value::String(value) => SessionAddr::new(&source.instance, &source.owner, value, None),
            _ => super::send::parse_target(target, source)?,
        };
        if address.instance != self.instance {
            super::mailbox::validate_address(&address)?;
            return Ok(address);
        }
        if address.owner != source.owner {
            crate::messaging::mailbox::unauthorized(crate::messaging::mailbox::UnauthorizedReason::LocalOwnerMismatch);
            return Err(MailboxError::OwnerMismatch);
        }
        // An explicit non-main agent is the sender's claim about the receiver. It is never
        // replaced by the session's recorded agent: the store and the inbound check then
        // refuse an unconfigured or mismatching agent instead of delivering to another one.
        let explicit_agent = address.agent != "main" && target.get("agent").is_some();
        if matches!(address.session.as_str(), "main" | "parent") {
            address.session = self
                .store
                .parent_session(&self.workspace, &source.owner, &source.session)?
                .unwrap_or_else(|| source.session.clone());
            if !explicit_agent {
                address.agent = "main".into();
            }
        }
        if !explicit_agent
            && let Some(agent) = self
                .store
                .logical_agent(&self.workspace, &source.owner, &address.session)?
        {
            address.agent = agent;
        }
        Ok(address)
    }
}

struct LocalResolver {
    runtime: Arc<MessagingRuntime>,
    source: SessionAddr,
}
#[async_trait]
impl MailboxResolver for LocalResolver {
    async fn resolve(&self, target: &SessionAddr) -> MailboxResult<MailboxDestination> {
        let runtime = Arc::clone(&self.runtime);
        let source = self.source.clone();
        let target = target.clone();
        if target.instance != runtime.instance {
            let config = runtime.config.read().clone();
            return super::a2a::RemoteTransport::new(&config)
                .destination(&runtime, &source, &target)
                .await;
        }
        crate::runtime::blocking::spawn_blocking(move || {
            let authority = MailboxAuthority::local(runtime.workspace.clone(), source.clone(), target.clone())?;
            let scope = runtime.resolve_scope(&target)?;
            let pin = runtime.store.recipient_pin(&authority, &scope, &target.session)?;
            Ok(MailboxDestination {
                authority,
                pin,
                pin_expires_at: chrono::Utc::now().timestamp() + 300,
            })
        })
        .await
        .map_err(|_| MailboxError::Storage)?
    }
}

pub struct RuntimeSender {
    runtime: Arc<MessagingRuntime>,
    sender: SqliteMailboxSender,
    scope: MailboxScope,
}
#[async_trait]
impl MailboxSender for RuntimeSender {
    fn source(&self) -> &SessionAddr {
        self.sender.source()
    }
    async fn send(&self, target: SessionAddr, text: String, mime: String) -> MailboxResult<MailboxPublished> {
        self.send_with_id(target, text, mime, uuid::Uuid::new_v4()).await
    }
}
impl RuntimeSender {
    pub(crate) async fn send_with_id(
        &self,
        target: SessionAddr,
        text: String,
        mime: String,
        id: uuid::Uuid,
    ) -> MailboxResult<MailboxPublished> {
        let mut published = self.sender.send_with_id(target.clone(), text, mime, id).await?;
        if target.instance != self.runtime.instance {
            return Ok(published);
        }
        let authority = MailboxAuthority::local(self.runtime.workspace.clone(), self.source().clone(), target.clone())?;
        let pending = self.runtime.store.pending_outbox(&self.scope, self.source())?;
        let outbound = pending
            .iter()
            .find(|pending| pending.message().id == published.message_id)
            .ok_or(MailboxError::CorruptState)?;
        let receipt = self.runtime.store.accept(&authority, outbound.message().clone())?;
        self.runtime
            .store
            .acknowledge(&self.scope, &target, published.message_id, receipt.state)?;
        published.ack_state = Some(receipt.state);
        self.runtime.wake(&target);
        Ok(published)
    }
}

impl SessionInbox {
    pub(crate) fn live(&self) -> bool {
        self.runtime.store.validate_scope(&self.scope, &self.address).is_ok()
    }
    pub fn take_notification(&mut self) -> bool {
        self.receiver.try_recv().is_ok()
    }
    pub async fn notified(&mut self) {
        let _ = self.receiver.recv().await;
    }
    /// Queue adopted messages whose waking turn never started, e.g. the
    /// process stopped between the adoption commit and the wake. Run once,
    /// right after registration and before the first poll. A message whose
    /// continuation input exists is not returned by the store, so a turn that
    /// started is never repeated.
    pub(crate) fn recover(&self) -> MailboxResult<usize> {
        let mut queued = 0;
        for (message, receipt) in self.runtime.store.unwoken_adopted(&self.scope, &self.address)? {
            let input = MessageInput {
                _lease: self.runtime.store.hold_input(&self.scope, &self.address)?,
                scope: self.scope.scope_id,
                text: crate::session_worker::mailbox::user_text(&message)?,
                message,
                receipt,
            };
            let pending = self
                .runtime
                .ready
                .lock()
                .get(&self.address)
                .is_some_and(|queue| queue.iter().any(|held| held.message.id == input.message.id));
            if !input.auto_start() || pending {
                continue;
            }
            match self.runtime.requeue_back(input) {
                Ok(()) => queued += 1,
                Err(MailboxError::QueueFull) => break,
                Err(_) => {}
            }
        }
        Ok(queued)
    }
    pub fn poll(&mut self) -> MailboxResult<Vec<MessageInput>> {
        self.runtime.store.validate_scope(&self.scope, &self.address)?;
        let execution =
            self.runtime
                .store
                .execution_guard(&self.scope, &self.address, uuid::Uuid::new_v4().to_string())?;
        let queued = {
            let mut ready = self.runtime.ready.lock();
            let value = ready
                .get_mut(&self.address)
                .and_then(std::collections::VecDeque::pop_front);
            if ready
                .get(&self.address)
                .is_some_and(std::collections::VecDeque::is_empty)
            {
                ready.remove(&self.address);
            }
            value
        };
        if let Some(input) = queued {
            drop(execution);
            return Ok(vec![input]);
        }
        let mut inputs = Vec::new();
        for _ in 0..1 {
            let Some(delivery) = self.runtime.store.deliver(&self.scope, &self.address)? else {
                break;
            };
            let message = delivery.message().clone();
            let receipt = self.runtime.store.adopt(&delivery)?;
            let text = crate::session_worker::mailbox::user_text(&message)?;
            inputs.push(MessageInput {
                _lease: self.runtime.store.hold_input(&self.scope, &self.address)?,
                scope: self.scope.scope_id,
                text,
                message,
                receipt,
            });
        }
        drop(execution);
        Ok(inputs)
    }
}

impl super::mailbox::MailboxCacheDrainer for MessagingRuntime {
    fn drain(&self, scope: uuid::Uuid) {
        self.stop_scope(scope);
        self.ready.lock().retain(|_, inputs| {
            inputs.retain(|input| input.scope != scope);
            !inputs.is_empty()
        });
        self.inboxes.lock().retain(|_, (registered, _, sender)| {
            if *registered == scope {
                let _ = sender.try_send(());
                false
            } else {
                true
            }
        });
    }
}

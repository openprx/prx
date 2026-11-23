//! Runtime-owned sender identity, durable local delivery and inbox wakeups.
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

type RuntimeRegistry = Mutex<HashMap<PathBuf, Weak<MessagingRuntime>>>;
static RUNTIMES: OnceLock<RuntimeRegistry> = OnceLock::new();

pub struct MessagingRuntime {
    pub(crate) store: SqliteMailbox,
    pub(crate) workspace: String,
    pub(crate) instance: String,
    config: RwLock<Arc<crate::config::Config>>,
    inboxes: Mutex<HashMap<SessionAddr, (uuid::Uuid, mpsc::Sender<()>)>>,
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
        runtime
            .store
            .validate_input(&runtime.workspace, &self.message.to, event, &self.text)
    }

    pub const fn auto_start(&self) -> bool {
        self.receipt.chain.depth < 8
    }
}

pub struct SessionInbox {
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
            .is_some_and(|(scope, _)| *scope == self.scope.scope_id)
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
            super::a2a::identity::load(config)?.instance_id.to_string()
        } else {
            super::a2a::identity::optional(config)?.map_or_else(
                || config.a2a.agent_id.clone(),
                |identity| identity.instance_id.to_string(),
            )
        };
        let store = SqliteMailbox::new(
            memory,
            instance.clone(),
            secret,
            Arc::new(super::mailbox::SystemMailboxClock),
        )?;
        let runtime = Arc::new(Self {
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
            super::a2a::RemoteTransport::new(config).start(runtime.clone());
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
        match self.inboxes.lock().entry(address.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((scope.scope_id, sender));
            }
            std::collections::hash_map::Entry::Occupied(_) => return Err(MailboxError::IdentityImmutable),
        }
        Ok(SessionInbox {
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
            return Err(MailboxError::Unauthorized);
        }
        let registered = self.inboxes.lock().contains_key(target);
        self.store.session_scope(&self.workspace, target, registered)
    }

    pub(crate) fn adopted(&self, message: PrxMessage, receipt: MailboxReceipt) -> MailboxResult<()> {
        let text = crate::session_worker::mailbox::user_text(&message)?;
        let target = message.to.clone();
        let mut ready = self.ready.lock();
        let queue = ready.entry(target.clone()).or_default();
        if queue.len() >= usize::try_from(super::mailbox::MAX_QUEUE).map_err(|_| MailboxError::CorruptState)? {
            return Err(MailboxError::QueueFull);
        }
        let scope = self.store.session_scope(&self.workspace, &message.to, false)?;
        let lease = self.store.hold_input(&scope, &message.to)?;
        queue.push_back(MessageInput {
            _lease: lease,
            scope: scope.scope_id,
            message,
            receipt,
            text,
        });
        drop(ready);
        self.wake(&target);
        Ok(())
    }
    pub fn wake(&self, target: &SessionAddr) {
        if let Some((_, sender)) = self.inboxes.lock().get(target) {
            let _ = sender.try_send(());
        }
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
            return Err(MailboxError::Unauthorized);
        }
        if matches!(address.session.as_str(), "main" | "parent") {
            address.session = self
                .store
                .parent_session(&self.workspace, &source.owner, &source.session)?
                .unwrap_or_else(|| source.session.clone());
            address.agent = "main".into();
        }
        if let Some(agent) = self
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
        if target.instance != self.runtime.instance {
            return super::a2a::RemoteTransport::new(&self.runtime.config.read()).destination(
                &self.runtime,
                &self.source,
                target,
            );
        }
        let authority = MailboxAuthority::local(self.runtime.workspace.clone(), self.source.clone(), target.clone())?;
        let scope = self.runtime.resolve_scope(target)?;
        let pin = self.runtime.store.recipient_pin(&authority, &scope, &target.session)?;
        Ok(MailboxDestination {
            authority,
            pin,
            pin_expires_at: chrono::Utc::now().timestamp() + 300,
        })
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
    pub fn poll(&mut self) -> MailboxResult<Vec<MessageInput>> {
        self.runtime.store.validate_scope(&self.scope, &self.address)?;
        let execution =
            self.runtime
                .store
                .execution_guard(&self.scope, &self.address, uuid::Uuid::new_v4().to_string())?;
        let mut inputs: Vec<_> = self
            .runtime
            .ready
            .lock()
            .remove(&self.address)
            .unwrap_or_default()
            .into_iter()
            .collect();
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
        self.ready.lock().retain(|_, inputs| {
            inputs.retain(|input| input.scope != scope);
            !inputs.is_empty()
        });
        self.inboxes.lock().retain(|_, (registered, sender)| {
            if *registered == scope {
                let _ = sender.try_send(());
                false
            } else {
                true
            }
        });
    }
}

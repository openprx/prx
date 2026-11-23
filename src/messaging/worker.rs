//! Parent-owned durable worker mailbox, bound to the sealed logical session.
use super::{DeliveryState, runtime::MessagingRuntime, send::MailboxSender};
use crate::session_worker::{
    frames::{WorkerControlQueue, WorkerFrameChannels},
    mailbox_binding::{WorkerMailboxBinding, WorkerMailboxFactory},
    protocol::{WorkerControlFrame, WorkerManifest, WorkerOutputFrame},
};
use anyhow::{Context, Result};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct WorkerMailboxes {
    runtime: Arc<MessagingRuntime>,
    owners: parking_lot::Mutex<HashMap<String, Arc<WorkerOwner>>>,
}
struct WorkerOwner {
    turn: parking_lot::Mutex<Option<crate::messaging::runtime::MessageTurnGuard>>,
    cancel: CancellationToken,
    pending: parking_lot::Mutex<HashMap<uuid::Uuid, crate::memory::sqlite::mailbox::MailboxDelivery>>,
    execution: parking_lot::Mutex<Option<crate::memory::sqlite::mailbox::MailboxExecution>>,
    task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    scope: crate::messaging::mailbox::MailboxScope,
    address: super::SessionAddr,
}
impl WorkerMailboxes {
    pub(crate) fn for_config(config: &crate::config::Config) -> Result<Arc<Self>> {
        static FACTORIES: std::sync::OnceLock<parking_lot::Mutex<HashMap<std::path::PathBuf, Arc<WorkerMailboxes>>>> =
            std::sync::OnceLock::new();
        let runtime = MessagingRuntime::for_config(config)?;
        let mut factories = FACTORIES.get_or_init(Default::default).lock();
        Ok(factories
            .entry(config.workspace_dir.clone())
            .or_insert_with(|| {
                Arc::new(Self {
                    runtime,
                    owners: Default::default(),
                })
            })
            .clone())
    }
}
impl WorkerMailboxFactory for WorkerMailboxes {
    fn bind(&self, manifest: &WorkerManifest) -> Result<WorkerMailboxBinding> {
        let owner = manifest.owner_id.as_deref().context("worker message owner missing")?;
        let address = self.runtime.address(
            owner,
            &manifest.session_scope_key,
            manifest.agent_id.as_deref().unwrap_or("main"),
        );
        let inbox = self.runtime.register(address.clone())?;
        let scope = self.runtime.resolve_scope(&address)?;
        let execution = self
            .runtime
            .store
            .execution_guard(&scope, &address, manifest.run_id.clone())?;
        let sender = self.runtime.bind(
            address.clone(),
            manifest
                .ordinary_source
                .as_ref()
                .and_then(|source| source.input.as_ref())
                .or_else(|| {
                    manifest
                        .operator_source
                        .as_ref()
                        .and_then(|source| source.input.as_ref())
                })
                .map(|input| input.event_id.as_str()),
        )?;
        let auth = crate::session_worker::frame_auth::FrameAuth::parent(&manifest.run_id)?;
        let (controls, receiver) = mpsc::channel(64);
        let (outputs, mut output_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let run = manifest.run_id.clone();
        let handle = tokio::runtime::Handle::try_current()?;
        let turn = self.runtime.hold_scope(
            &scope,
            &address,
            Arc::new(move || {
                if let Some(id) = crate::runtime::registry::resolve_address(&run) {
                    handle.spawn(async move {
                        let _ = crate::runtime::registry::kill(id, true).await;
                    });
                }
            }),
        )?;
        let owner = Arc::new(WorkerOwner {
            turn: parking_lot::Mutex::new(Some(turn)),
            cancel: cancel.clone(),
            pending: Default::default(),
            execution: parking_lot::Mutex::new(Some(execution)),
            task: Default::default(),
            scope: scope.clone(),
            address: address.clone(),
        });
        anyhow::ensure!(
            self.owners
                .lock()
                .insert(manifest.run_id.clone(), owner.clone())
                .is_none(),
            "worker mailbox already bound"
        );
        let runtime = self.runtime.clone();
        let target = address.clone();
        let stop = cancel;
        let actor_owner = owner.clone();
        let task = tokio::spawn(async move {
            let _inbox = inbox;
            let pending = &actor_owner.pending;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                let result: Result<()> = tokio::select! {
                    () = stop.cancelled() => break,
                    frame = output_rx.recv() => {
                        match frame {
                            Some(WorkerOutputFrame::Message { envelope }) => {
                                // Only the target and data are wire input. Sender, chain,
                                // pin, sequence and authority are reconstructed here.
                                async {
                                    let to = runtime.alias(sender.source(), &serde_json::to_value(&envelope.to)?)?;
                                    let text = String::from_utf8(envelope.body.bytes)?;
                                    sender.send_with_id(to, text, envelope.body.mime, envelope.id).await?;
                                    Ok(())
                                }.await
                            }
                            Some(WorkerOutputFrame::Ack { message_id, state: DeliveryState::Consumed }) => {
                                if !runtime.ready_capacity(&target) {continue;}
                                let delivery = pending.lock().remove(&message_id);
                                delivery.map_or_else(||Ok(()),|delivery|runtime.store.adopt(&delivery).and_then(|receipt|runtime.adopted(delivery.message().clone(),receipt)).map_err(Into::into))
                            }
                            Some(WorkerOutputFrame::Ack { .. }) => Ok(()),
                            None => break,
                            _ => Err(anyhow::anyhow!("unregistered worker broker frame")),
                        }
                    }
                    _ = interval.tick() => {
                        async {
                            runtime.store.validate_scope(&scope, &target)?;
                            if pending.lock().len() < 64 && runtime.ready_capacity(&target) {
                                if let Some(delivery) = runtime.store.deliver(&scope, &target)? {
                                    let frame = WorkerControlFrame::Message { envelope: Box::new(delivery.message().clone()) };
                                    let id = delivery.message().id;
                                    pending.lock().insert(id, delivery);
                                    controls.send(frame).await.context("worker control closed")?;
                                }
                            }
                            Ok(())
                        }.await
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(error=%crate::agent::diagnostics::error_class(&error), "worker mailbox stopped");
                    break;
                }
            }
        });
        *owner.task.lock() = Some(task);
        Ok(WorkerMailboxBinding {
            address,
            frames: WorkerFrameChannels {
                controls: WorkerControlQueue {
                    receiver,
                    auth: Some(auth),
                    owner,
                },
                outputs,
            },
        })
    }
    fn release(&self, run: &str, joined: bool) {
        if joined && let Some(owner) = self.owners.lock().remove(run) {
            owner.cancel.cancel();
            let task = owner.task.lock().take();
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                if let Some(task) = task {
                    let _ = task.await;
                }
                owner.pending.lock().clear();
                drop(owner.execution.lock().take());
                drop(owner.turn.lock().take());
                let _ = runtime.store.recover_delivery(&owner.scope, &owner.address);
                runtime.wake(&owner.address);
            });
        }
    }
}

pub(crate) struct WorkerSender {
    pub(crate) address: super::SessionAddr,
}
#[async_trait::async_trait]
impl MailboxSender for WorkerSender {
    fn source(&self) -> &super::SessionAddr {
        &self.address
    }
    async fn send(
        &self,
        target: super::SessionAddr,
        text: String,
        mime: String,
    ) -> super::mailbox::MailboxResult<super::mailbox::MailboxPublished> {
        use super::{
            IdempotencyKey, MessageBody, MessageKind, PrxMessage, RecipientPin,
            mailbox::{MailboxError, MailboxPublished, MailboxSendState},
        };
        super::mailbox::validate_body(&mime, text.as_bytes())?;
        let id = uuid::Uuid::new_v4();
        let message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse(&id.to_string()).map_err(|_| MailboxError::InvalidMetadata)?,
            stream_id: None,
            seq: 0,
            ts: 0,
            from: self.address.clone(),
            to: target,
            to_pin: RecipientPin {
                incarnation: uuid::Uuid::nil(),
                generation: 0,
                token: String::new(),
            },
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime,
                bytes: text.into_bytes(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        crate::session_worker::stdout::frame(&WorkerOutputFrame::Message {
            envelope: Box::new(message),
        })
        .map_err(|_| MailboxError::Storage)?;
        Ok(MailboxPublished {
            row_id: 0,
            message_id: id,
            stream_id: uuid::Uuid::nil(),
            seq: 0,
            state: MailboxSendState::Pending,
            ack_state: None,
            error: None,
        })
    }
}

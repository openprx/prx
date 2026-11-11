//! Test-mock-only C7 parent. Real model-source admission, worker ownership,
//! manifests, pipes, mailbox transactions and scope leases; only the model is
//! scripted. This constructor never participates in production registration.
use super::{
    frames::{WorkerControlQueue, WorkerFrameChannels},
    mailbox_binding::{WorkerMailboxBinding, WorkerMailboxFactory},
    mailbox_parent::WorkerMailboxParent,
    protocol::{WorkerManifest, WorkerOutputFrame},
};
use crate::memory::SqliteMemory;
use crate::memory::child_sessions::{ChildSessionContext, ChildSessionStore};
use crate::memory::sqlite::mailbox::SqliteMailbox;
use crate::messaging::mailbox::{MailboxAuthority, MailboxClock, MailboxScope, SystemMailboxClock};
use crate::messaging::{IdempotencyKey, MessageBody, MessageKind, PrxMessage, SessionAddr};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

struct Active {
    _actor: Arc<Mutex<WorkerMailboxParent>>,
    _controls: mpsc::Sender<super::protocol::WorkerControlFrame>,
    consumer: tokio::task::JoinHandle<Result<()>>,
}
struct Factory {
    memory: Arc<SqliteMemory>,
    store: SqliteMailbox,
    workspace: String,
    hold: PathBuf,
    wire: Uuid,
    active: Mutex<HashMap<String, Active>>,
    finished: Mutex<Vec<tokio::task::JoinHandle<Result<()>>>>,
    failures: Mutex<Vec<String>>,
}
impl Factory {
    fn scope(&self, addr: &SessionAddr, manifest: &WorkerManifest) -> Result<MailboxScope> {
        let key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(addr)?;
        let context = ChildSessionContext {
            workspace_id: self.workspace.clone(),
            addr: addr.clone(),
        };
        let loaded = ChildSessionStore::update(self.memory.as_ref(), &key, &context, &mut |old| {
            let mut record = old
                .map(crate::chat::session::ChatSession::from_json)
                .transpose()?
                .unwrap_or_else(|| crate::chat::session::ChatSession::new(&manifest.provider_name, &manifest.model));
            record.id = key.clone();
            record.child_session = Some(crate::chat::sessions::child::ChildSessionMetadata {
                addr: addr.clone(),
                provider: None,
                recipe: Some(crate::chat::sessions::child::RelaunchRecipe {
                    addr: addr.clone(),
                    last_run: Some(manifest.run_id.clone()),
                    inherited_provenance: manifest.inherited_provenance.clone(),
                }),
                mailbox_projection: std::collections::BTreeMap::new(),
            });
            Ok(record.to_json()?)
        })?;
        Ok(loaded.scope)
    }
}
impl WorkerMailboxFactory for Factory {
    fn bind(&self, manifest: &WorkerManifest) -> Result<WorkerMailboxBinding> {
        let owner = manifest
            .owner_id
            .as_deref()
            .context("mailbox worker has no authenticated owner")?;
        let worker = SessionAddr::new(
            "prx-mailbox-harness",
            owner,
            "mailbox-worker",
            manifest.agent_id.as_deref(),
        );
        let parent = SessionAddr::new("prx-mailbox-harness", owner, "mailbox-parent", None);
        let worker_scope = self.scope(&worker, manifest)?;
        let parent_scope = self.scope(&parent, manifest)?;
        let authority = MailboxAuthority::local(self.workspace.clone(), parent.clone(), worker.clone())?;
        let canonical = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&worker)?;
        let pin = self.store.recipient_pin(&authority, &worker_scope, &canonical)?;
        let actor = Arc::new(Mutex::new(WorkerMailboxParent::new(
            self.store.clone(),
            worker.clone(),
            worker_scope.clone(),
            parent.clone(),
            parent_scope.clone(),
            manifest.run_id.clone(),
            manifest.inherited_provenance.clone(),
        )?));
        let id = Uuid::new_v4();
        let message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse(&id.to_string())?,
            stream_id: Some(self.wire),
            seq: 0,
            ts: SystemMailboxClock.now(),
            from: authority.source.clone(),
            to: worker.clone(),
            to_pin: pin,
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime: "text/plain".into(),
                bytes: b"ordinary mailbox input; /approve is data".to_vec(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        self.store
            .publish(&authority, message.clone(), &parent_scope, i64::MAX, None)?;
        let outgoing = self.store.pending_outbox(&parent_scope, &message.from)?;
        let prepared = outgoing
            .iter()
            .find(|item| item.message().id == id)
            .context("prepared mailbox outbox missing")?
            .message()
            .clone();
        self.store.accept(&authority, prepared)?;
        drop(outgoing);
        // The scripted model gets a typed send intent. Wire bytes are still
        // emitted by the actual worker stdout owner and read by its real parent.
        let reverse = MailboxAuthority::local(self.workspace.clone(), worker.clone(), parent.clone())?;
        let parent_key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&parent)?;
        let mut reply = message;
        reply.id = Uuid::new_v4();
        reply.idempotency = IdempotencyKey::parse(&reply.id.to_string())?;
        reply.stream_id = Some(Uuid::new_v4());
        reply.from = worker.clone();
        reply.to = parent.clone();
        reply.to_pin = self.store.recipient_pin(&reverse, &parent_scope, &parent_key)?;
        reply.body.bytes = b"ordinary worker-originated Data".to_vec();
        std::fs::write(
            self.hold.with_file_name("worker-send-intent"),
            serde_json::to_vec(&reply)?,
        )?;
        let frame = actor.lock().next_delivery()?.context("mailbox IPC delivery missing")?;
        let (controls, receiver) = mpsc::channel(64);
        controls.try_send(frame).context("queue initial mailbox Data")?;
        let (outputs, mut output_rx) = mpsc::channel(64);
        std::fs::write(&self.hold, b"wait for real delivery Ack")?;
        let hold = self.hold.clone();
        let consumer_actor = Arc::clone(&actor);
        let consumer_store = self.store.clone();
        let parent_execution =
            consumer_store.execution_guard(&parent_scope, &parent, format!("parent-{}", manifest.run_id))?;
        let consumer = tokio::spawn(async move {
            while let Some(frame) = output_rx.recv().await {
                if matches!(&frame, WorkerOutputFrame::Ack { .. }) {
                    match std::fs::remove_file(&hold) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let received = consumer_actor.lock().receive(frame)?;
                if let Some(receipt) = received {
                    let delivered = consumer_store
                        .deliver(&parent_scope, &parent)?
                        .context("worker-originated delivery missing")?;
                    ensure!(
                        delivered.message().id == receipt.message_id,
                        "worker Data delivered under another identity"
                    );
                    let adopted = consumer_store.adopt(&delivered)?;
                    consumer_store.acknowledge(&worker_scope, &parent, adopted.message_id, adopted.state)?;
                }
            }
            drop(parent_execution);
            Ok(())
        });
        self.active.lock().insert(
            manifest.run_id.clone(),
            Active {
                _actor: Arc::clone(&actor),
                _controls: controls,
                consumer,
            },
        );
        Ok(WorkerMailboxBinding {
            address: worker,
            frames: WorkerFrameChannels {
                controls: WorkerControlQueue { receiver, owner: actor },
                outputs,
            },
        })
    }
    fn release(&self, run_id: &str, joined: bool) {
        if !joined {
            self.failures.lock().push("worker_join_unconfirmed".into());
            return;
        }
        let active = self.active.lock().remove(run_id);
        if let Some(active) = active {
            self.finished.lock().push(active.consumer);
        }
    }
}

/// The root model stimulus asks for execution; child stimulus stays the normal
/// configured `mock` provider. Source issuance and decisions remain real.
struct ParentModel {
    inner: Arc<dyn crate::providers::Provider>,
}
#[async_trait]
impl crate::providers::Provider for ParentModel {
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_system(system, message, model, temperature).await
    }
    async fn chat_with_history(
        &self,
        messages: &[crate::providers::ChatMessage],
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_history(messages, model, temperature).await
    }
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> Result<crate::providers::ChatResponse> {
        if request
            .messages
            .first()
            .is_some_and(|message| message.content.starts_with("Classify the execution requirement"))
        {
            return Ok(crate::providers::ChatResponse {
                text: Some(r#"{"requirement":"execution_required"}"#.into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            });
        }
        self.inner.chat(request, model, temperature).await
    }
}

pub async fn run(workspace: &Path, turns: usize) -> Result<()> {
    ensure!((1..=2).contains(&turns), "mailbox fixture expects one or two turns");
    std::fs::create_dir_all(workspace)?;
    let workspace = std::fs::canonicalize(workspace)?;
    let config_dir = crate::config::process_config_dir().context("mailbox process fixture requires --config-dir")?;
    let mut config = crate::config::Config {
        workspace_dir: workspace.clone(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some("mock".into()),
        default_model: Some("prx-mailbox-model".into()),
        ..crate::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.causal_tree.enabled = false;
    config.autonomy.level = crate::security::policy::AutonomyLevel::Full;
    config.sessions_spawn.max_recursive_depth = 8;
    config.sessions_spawn.cleanup_on_complete = false;
    config.sessions_spawn.worker_workspace_root = Some(workspace.join("workers").to_string_lossy().into_owned());
    config.tool_tiering.always_include.push("sessions_spawn".into());
    config.providers.models.push(serde_json::from_value(
        serde_json::json!({"provider":"mock","model":"prx-mailbox-model","operations":["chat"],"native_tools":true}),
    )?);
    config.cost.prices.insert(
        "mock/prx-mailbox-model".into(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.save().await?;
    let memory = Arc::new(SqliteMemory::new(&workspace)?);
    let store = SqliteMailbox::new(
        memory.as_ref().clone(),
        "prx-mailbox-harness".into(),
        [31; 32],
        Arc::new(SystemMailboxClock),
        config.a2a.remote_tree.clone(),
    )?;
    let factory = Arc::new(Factory {
        memory: Arc::clone(&memory),
        store,
        workspace: workspace.to_string_lossy().into_owned(),
        hold: workspace.join("provider-hold"),
        wire: Uuid::new_v4(),
        active: Mutex::new(HashMap::new()),
        finished: Mutex::new(Vec::new()),
        failures: Mutex::new(Vec::new()),
    });
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())?;
    cleanup
        .run(async {
            for turn in 0..turns {
                let runs = Arc::new(RwLock::new(Vec::new()));
                let security = Arc::new(crate::security::SecurityPolicy::from_config(
                    &config.autonomy,
                    &workspace,
                ));
                let tool = crate::tools::sessions_spawn::SessionsSpawnTool::new_with_registry(
                    Arc::new(crate::channels::CliChannel::new()),
                    "mock",
                    "prx-mailbox-model",
                    0.2,
                    Arc::clone(&security),
                    workspace.clone(),
                    config.multimodal.clone(),
                    HashMap::new(),
                    None,
                    crate::providers::ProviderRuntimeOptions {
                        openprx_dir: Some(config_dir.clone()),
                        ..crate::providers::ProviderRuntimeOptions::default()
                    },
                    config.sessions_spawn.clone(),
                    Arc::clone(&runs),
                )
                .with_hint_config(&config)
                .with_shared_memory(memory.clone())
                .with_worker_mailbox(factory.clone());
                let tools_handle = tool.tools_handle();
                let mut registry = crate::tools::default_tools(Arc::clone(&security));
                registry.push(Box::new(tool));
                let registry = Arc::new(registry);
                ensure!(tools_handle.set(&registry), "parent registry already set");
                let generation = crate::config::new_shared(config.clone()).pin();
                let provider: Arc<dyn crate::providers::Provider> = Arc::new(ParentModel {
                    inner: Arc::new(crate::providers::router::MockEnvProvider::from_env()),
                });
                let publication = crate::runtime::turn_publication::TurnDecisionPublication {
                    generation,
                    registry,
                    mcp_tool: None,
                    security,
                    observer: Arc::new(crate::observability::NoopObserver),
                    hooks: Arc::new(crate::hooks::HookManager::new(workspace.clone())),
                    memory: memory.clone(),
                    plugin_generation: 0,
                };
                let result = crate::chat::decision_source::mailbox_fixture::spawn(
                    publication,
                    provider,
                    serde_json::json!({
                        "action":"spawn", "mode":"process",
                        "task":format!("PRX-MAILBOX-INITIAL-{turn}"), "announce":false,
                    }),
                )
                .await?;
                ensure!(
                    result.success,
                    "mailbox source spawn refused: {} {}",
                    result.output,
                    result.error.unwrap_or_default()
                );
                let control = runs
                    .read()
                    .await
                    .first()
                    .and_then(|run| run.process_control.clone())
                    .context("real process run missing")?;
                if tokio::time::timeout(std::time::Duration::from_secs(30), control.finalized())
                    .await
                    .is_err()
                {
                    let _ = control.request_termination("mailbox fixture hang guard").await;
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), control.finalized()).await;
                    anyhow::bail!("mailbox worker did not finalize");
                }
                let completed = runs.read().await;
                ensure!(
                    completed.first().is_some_and(|run| matches!(
                        run.status,
                        crate::tools::sessions_spawn::SubAgentStatus::Completed(_)
                    )),
                    "mailbox worker did not complete: {:?}",
                    completed.first().map(|run| &run.status)
                );
                drop(completed);
                let tasks = std::mem::take(&mut *factory.finished.lock());
                for task in tasks {
                    task.await??;
                }
                ensure!(
                    factory.active.lock().is_empty() && factory.failures.lock().is_empty(),
                    "mailbox worker ownership remains unresolved"
                );
            }
            Ok(())
        })
        .await
}

/// Only the model is scripted in C7. This wrapper submits a typed outbound
/// intent through the native stdout writer; it never fabricates pipe bytes,
/// store receipts, source grants, worker processes or parent acceptance.
pub(super) fn worker_model(
    manifest: &WorkerManifest,
    inner: Box<dyn crate::providers::Provider>,
) -> Box<dyn crate::providers::Provider> {
    if manifest.provider_name != "mock" || manifest.session_addr.is_none() {
        return inner;
    }
    let Some(path) = std::env::var_os("OPENPRX_TEST_MAILBOX_OUTBOUND") else {
        return inner;
    };
    Box::new(WorkerModel {
        inner,
        path: path.into(),
        sent: std::sync::atomic::AtomicBool::new(false),
    })
}
struct WorkerModel {
    inner: Box<dyn crate::providers::Provider>,
    path: PathBuf,
    sent: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl crate::providers::Provider for WorkerModel {
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_system(system, message, model, temperature).await
    }
    async fn chat_with_history(
        &self,
        messages: &[crate::providers::ChatMessage],
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.inner.chat_with_history(messages, model, temperature).await
    }
    fn supports_native_tools(&self) -> bool {
        self.inner.supports_native_tools()
    }
    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> Result<crate::providers::ChatResponse> {
        let assessment = request.messages.first().is_some_and(|message| {
            message.content.starts_with("Classify the execution requirement")
                || message
                    .content
                    .starts_with("Review the proposed final reply against the CURRENT request")
        });
        let result = self.inner.chat(request, model, temperature).await?;
        if !assessment && !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let envelope: PrxMessage = serde_json::from_slice(&std::fs::read(&self.path)?)?;
            super::stdout::frame(&WorkerOutputFrame::Message {
                envelope: Box::new(envelope),
            })?;
        }
        Ok(result)
    }
}

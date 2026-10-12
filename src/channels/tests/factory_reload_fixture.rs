#![cfg(test)]

//! Q73B/Q74: real selected retrieval and generation-owned factory wire.
use super::*;
use crate::config::{ConfigGeneration, ConfigGenerationParticipant, PreparedConfigGeneration};
use crate::memory::MemoryWriteContext;
use crate::memory::traits::DecisionMemoryHit;
use futures_util::FutureExt;
use std::sync::atomic::{AtomicUsize, Ordering};

const MODEL: &str = "gpt-4o-mini";
const INPUT: &str = "hello cached default provider";
const SEED: &str = "prx-channel-visible-seed";
const FOREIGN: &str = "prx-channel-foreign-seed";
const KEYS: [&str; 3] = ["prx-fixture-key-zero", "prx-fixture-key-one", "prx-fixture-key-two"];

struct SelectedMemory {
    local: SqliteMemory,
    scope: MemoryWriteContext,
    selected: AtomicUsize,
}

#[async_trait::async_trait]
impl Memory for SelectedMemory {
    fn name(&self) -> &str {
        self.local.name()
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        self.local.store(key, content, category, session).await
    }
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.local.recall(query, limit, session).await
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        self.local.get(key).await
    }
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.local.list(category, session).await
    }
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.local.forget(key).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.local.count().await
    }
    async fn health_check(&self) -> bool {
        self.local.health_check().await
    }
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.local
            .store_with_context(key, content, category, session, context)
            .await
    }
    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.local.count_rehearsal_hits(query, limit, context, deadline).await
    }
    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        // Inspect the runtime's real projection; never replace or tighten it.
        assert_eq!(context.workspace_id, self.scope.workspace_id);
        assert_eq!(context.channel, self.scope.channel);
        assert_eq!(context.chat_id, self.scope.chat_id);
        assert_eq!(context.sender_id, self.scope.sender_id);
        assert_eq!(context.raw_sender, self.scope.raw_sender);
        assert_eq!(context.chat_type.as_deref(), Some("dm"));
        assert!(context.owner_id.is_none());
        assert!(!context.require_owner_match);
        assert!(session.is_none());
        let hits = self
            .local
            .retrieve_decision_memory(query, limit, context, session, deadline)
            .await?;
        assert!(
            hits.iter().any(|hit| hit.key == SEED),
            "selected read must include the genuine private seed"
        );
        assert!(
            hits.iter().all(|hit| hit.key != FOREIGN),
            "foreign private chat must be excluded by the actual reader"
        );
        self.selected.fetch_add(1, Ordering::SeqCst);
        Ok(hits)
    }
}

fn factory(config: &Config) -> Arc<dyn Provider> {
    let selection = providers::selection::check_default(config, providers::catalog::UnknownPolicy::Invocation)
        .expect("fixture real PMM selection");
    Arc::from(
        providers::create_provider_for(
            &selection,
            &providers::selection::catalog_for(config),
            &config.reliability,
            &providers::provider_runtime_options_from_config(config),
        )
        .expect("fixture real provider factory"),
    )
}

struct ReloadParticipant {
    cache: ProviderCacheMap,
    committed: Arc<Mutex<Vec<u64>>>,
}
struct PreparedReload {
    cache: ProviderCacheMap,
    provider: Arc<dyn Provider>,
    previous: Option<Arc<dyn Provider>>,
    committed: Arc<Mutex<Vec<u64>>>,
    generation: u64,
}
impl ConfigGenerationParticipant for ReloadParticipant {
    fn name(&self) -> &'static str {
        "prx-channel-factory-fixture"
    }
    fn supports_rebuild_field(&self, field: &str) -> bool {
        matches!(field, "api_key" | "api_url")
    }
    fn prepare(
        &self,
        generation: Arc<ConfigGeneration>,
        changed: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        anyhow::ensure!(
            changed.iter().all(|field| self.supports_rebuild_field(field)),
            "unexpected fixture reload field"
        );
        Ok(Box::new(PreparedReload {
            cache: Arc::clone(&self.cache),
            provider: factory(&generation.effective),
            previous: self.cache.lock().get("openai").cloned(),
            committed: Arc::clone(&self.committed),
            generation: generation.id.0,
        }))
    }
}
impl PreparedConfigGeneration for PreparedReload {
    fn commit(&mut self) -> anyhow::Result<()> {
        self.cache
            .lock()
            .insert("openai".to_owned(), Arc::clone(&self.provider));
        self.committed.lock().push(self.generation);
        Ok(())
    }
    fn rollback(&mut self) {
        let mut cache = self.cache.lock();
        if let Some(previous) = self.previous.take() {
            cache.insert("openai".to_owned(), previous);
        } else {
            cache.remove("openai");
        }
        self.committed.lock().retain(|id| *id != self.generation);
    }
}

struct Endpoint {
    url: String,
    server: ChannelProbeServer,
    calls: Arc<Mutex<Vec<usize>>>,
    invalid: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}
impl Endpoint {
    async fn start(endpoint_b: bool, memory: Arc<SelectedMemory>) -> std::io::Result<Self> {
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let invalid = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let captured = Arc::clone(&calls);
        let bad = Arc::clone(&invalid);
        let notified = Arc::clone(&entered);
        let barrier = Arc::clone(&release);
        let handler = axum::routing::post(
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&captured);
                let bad = Arc::clone(&bad);
                let notified = Arc::clone(&notified);
                let barrier = Arc::clone(&barrier);
                let memory = Arc::clone(&memory);
                async move {
                    let phase = KEYS.iter().position(|key| {
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .is_some_and(|value| value == format!("Bearer {key}"))
                    });
                    let valid_phase = phase.is_some_and(|phase| if endpoint_b { phase == 2 } else { phase < 2 });
                    if !valid_phase
                        || body.get("model").and_then(serde_json::Value::as_str) != Some(MODEL)
                        || body.get("stream").and_then(serde_json::Value::as_bool) == Some(true)
                        || memory.selected.load(Ordering::SeqCst) == 0
                    {
                        bad.fetch_add(1, Ordering::SeqCst);
                        return axum::http::StatusCode::BAD_REQUEST.into_response();
                    }
                    let phase = phase.expect("validated fixture credential phase");
                    {
                        let mut seen = captured.lock();
                        let expected = if endpoint_b { 2 } else { seen.len() };
                        let cap = if endpoint_b { 1 } else { 2 };
                        if seen.len() >= cap || phase != expected {
                            bad.fetch_add(1, Ordering::SeqCst);
                            return axum::http::StatusCode::BAD_REQUEST.into_response();
                        }
                        seen.push(phase);
                    }
                    if phase == 0 {
                        notified.notify_one();
                        let permit = barrier.acquire().await.expect("owned response barrier");
                        permit.forget();
                    }
                    axum::Json(serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","content":format!("prx-factory-generation-{phase}")},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})).into_response()
                }
            },
        );
        let app = axum::Router::new()
            .route("/v1/chat/completions", handler)
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024));
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        Ok(Self {
            url,
            server: ChannelProbeServer {
                stop,
                task: Some(tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown.cancelled_owned())
                        .await
                })),
            },
            calls,
            invalid,
            entered,
            release,
        })
    }
    async fn close(&mut self) -> anyhow::Result<()> {
        self.release.add_permits(8);
        self.server.stop.cancel();
        if let Some(mut task) = self.server.task.take() {
            match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
                Ok(joined) => {
                    joined??;
                }
                Err(_) => {
                    task.abort();
                    let joined = task.await;
                    anyhow::ensure!(
                        joined.is_err_and(|error| error.is_cancelled()),
                        "listener abort acknowledgement missing"
                    );
                    anyhow::bail!("fixture graceful listener shutdown timed out");
                }
            }
        }
        Ok(())
    }
}
struct ReleaseOnDrop(Arc<tokio::sync::Semaphore>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.add_permits(8);
    }
}

fn message(phase: usize) -> traits::ChannelMessage {
    traits::ChannelMessage {
        id: format!("prx-factory-message-{phase}"),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: INPUT.to_owned(),
        channel: "telegram".to_owned(),
        timestamp: 3,
        thread_ts: None,
        chat_kind: traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}
fn context(
    tmp: &TempDir,
    config: crate::config::SharedConfig,
    memory: Arc<dyn Memory>,
    channel: Arc<dyn Channel>,
    cache: ProviderCacheMap,
    histories: ConversationHistoryMap,
) -> Arc<ChannelRuntimeContext> {
    let generation = config.pin();
    let c = &generation.effective;
    let mut ctx = channel_turn_probe_ctx(tmp, Arc::clone(&memory), factory(c), channel);
    let owned = Arc::get_mut(&mut ctx).expect("fresh unshared factory context");
    owned.default_provider = Arc::new("openai".to_owned());
    owned.model = Arc::new(MODEL.to_owned());
    owned.api_key = c.api_key.clone();
    owned.api_url = c.api_url.clone();
    owned.provider_runtime_options = providers::provider_runtime_options_from_config(c);
    owned.security = Arc::new(arc_swap::ArcSwap::from_pointee(SecurityGen {
        security: Arc::new(crate::security::SecurityPolicy::from_config(&c.autonomy, tmp.path())),
    }));
    owned.tools_registry = Arc::new(vec![Box::new(crate::tools::MemorySearchTool::new(
        tmp.path().to_owned(),
        memory,
        true,
    ))]);
    owned.provider_cache = cache;
    owned.conversation_histories = histories;
    owned.config_generation = generation;
    owned.config = config;
    ctx
}
fn reload(
    config: &crate::config::SharedConfig,
    key: &str,
    url: &str,
    expected_fields: &[&str],
) -> Arc<ConfigGeneration> {
    let old = config.pin();
    let mut desired = old.effective.as_ref().clone();
    assert!(
        !desired.config_path.exists(),
        "reload fingerprint must derive from this in-memory revision"
    );
    desired.api_key = Some(key.to_owned());
    desired.api_url = Some(url.to_owned());
    let report = config
        .apply_runtime_config(desired, crate::config::ConfigReloadTrigger::Test)
        .expect("actual reload");
    assert!(
        report.restart_required.is_empty(),
        "fixture participant must own its rebuilds"
    );
    assert_eq!(
        report
            .rebuilt
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_fields.iter().copied().collect()
    );
    assert_eq!(report.participant_acks, vec!["prx-channel-factory-fixture"]);
    let next = config.pin();
    assert_ne!(old.id, next.id);
    assert_ne!(old.source_revision, next.source_revision);
    assert_eq!(report.active_generation, next.id);
    assert_eq!(report.active_source_revision, next.source_revision);
    next
}

pub(super) async fn run() {
    let tmp = TempDir::new().expect("factory fixture workspace");
    let scope = crate::runtime::envelope::RuntimeEnvelope::channel(
        tmp.path().to_string_lossy().to_string(),
        "telegram",
        "prx_user",
        Some("prx_channel".to_owned()),
    )
    .memory_write_context("dm");
    let memory = Arc::new(SelectedMemory {
        local: SqliteMemory::new(tmp.path()).expect("real semantic ledger"),
        scope: scope.clone(),
        selected: AtomicUsize::new(0),
    });
    memory
        .store_with_context(SEED, INPUT, MemoryCategory::Core, None, Some(&scope))
        .await
        .expect("genuine private seed");
    let foreign = crate::runtime::envelope::RuntimeEnvelope::channel(
        tmp.path().to_string_lossy().to_string(),
        "telegram",
        "prx_foreign_user",
        Some("prx_foreign_channel".to_owned()),
    )
    .memory_write_context("dm");
    memory
        .store_with_context(FOREIGN, INPUT, MemoryCategory::Core, None, Some(&foreign))
        .await
        .expect("genuine foreign private seed");
    // Read-only stored classification proof; no SQL mutation or fabricated ACL.
    {
        let connection = rusqlite::Connection::open_with_flags(
            tmp.path().join("memory/brain.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("stored ACL proof");
        for (key, chat, sender) in [
            (SEED, "prx_channel", "prx_user"),
            (FOREIGN, "prx_foreign_channel", "prx_foreign_user"),
        ] {
            let stored: (String, String, String, String, String) = connection
                .query_row(
                    "SELECT visibility, chat_type, channel, chat_id, raw_sender FROM memories WHERE key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .expect("genuine stored private row");
            assert_eq!(
                stored,
                (
                    "private".to_owned(),
                    "dm".to_owned(),
                    "telegram".to_owned(),
                    chat.to_owned(),
                    sender.to_owned()
                )
            );
        }
    }
    // Prove both records exist and are readable in their own source-derived ACL scopes.
    for (expected, context) in [(SEED, &scope), (FOREIGN, &foreign)] {
        let hits = memory
            .local
            .retrieve_decision_memory(
                INPUT,
                5,
                context,
                None,
                std::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .expect("seed ACL verification");
        assert_eq!(
            hits.iter().map(|hit| hit.key.as_str()).collect::<Vec<_>>(),
            vec![expected]
        );
    }
    let mut a = Endpoint::start(false, Arc::clone(&memory))
        .await
        .expect("first owned endpoint");
    let mut b = match Endpoint::start(true, Arc::clone(&memory)).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            a.close()
                .await
                .expect("first listener cleanup after second bind failure");
            panic!("second fixture listener failed: {error}");
        }
    };
    let outcome = std::panic::AssertUnwindSafe(async {
        let channel = Arc::new(TelegramRecordingChannel::default());
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let histories = Arc::new(Mutex::new(HashMap::new()));
        let mut initial = Config::default(); initial.workspace_dir = tmp.path().to_owned(); initial.config_path = tmp.path().join("config.toml");
        initial.default_provider = Some("openai".to_owned()); initial.default_model = Some(MODEL.to_owned()); initial.default_temperature = 0.0;
        initial.api_key = Some(KEYS[0].to_owned()); initial.api_url = Some(a.url.clone()); initial.memory.auto_save = false;
        assert!(initial.causal_tree.enabled); assert!(!initial.config_path.exists());
        let mut initial_ctx = context(&tmp, crate::config::new_shared(initial), memory.clone(), channel.clone(), Arc::clone(&cache), Arc::clone(&histories));
        let recording = super::super::recording_fixture::attach(Arc::get_mut(&mut initial_ctx).expect("unshared initial context"), tmp.path());
        let config = Arc::clone(&initial_ctx.config); let g0 = config.pin();
        let committed = Arc::new(Mutex::new(Vec::new()));
        let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(ReloadParticipant { cache: Arc::clone(&cache), committed: Arc::clone(&committed) });
        config.register_participant(&participant);
        let release = ReleaseOnDrop(Arc::clone(&a.release));
        let control = std::panic::AssertUnwindSafe(async {
            let _release = release;
            tokio::time::timeout(Duration::from_secs(20), a.entered.notified()).await.expect("real G0 serving request after selected retrieval");
            assert_eq!(*a.calls.lock(), vec![0]); assert!(b.calls.lock().is_empty(), "endpoint B must remain unused");
            reload(&config, KEYS[1], &a.url, &["api_key"])
        }).catch_unwind();
        let work = std::panic::AssertUnwindSafe(run_owned_channel_probe(initial_ctx, message(0), CancellationToken::new())).catch_unwind();
        let (first, reloaded) = tokio::join!(work, control);
        if let Err(panic) = first { std::panic::resume_unwind(panic); }
        let g1 = match reloaded { Ok(generation) => generation, Err(panic) => std::panic::resume_unwind(panic) };
        let recorded: Arc<dyn Memory> = recording.clone();
        let selected_before = memory.selected.load(Ordering::SeqCst);
        run_owned_channel_probe(context(&tmp, Arc::clone(&config), Arc::clone(&recorded), channel.clone(), Arc::clone(&cache), Arc::clone(&histories)), message(1), CancellationToken::new()).await;
        assert!(memory.selected.load(Ordering::SeqCst) > selected_before, "G1 must obtain genuine selected evidence");
        assert_eq!(*a.calls.lock(), vec![0, 1]); assert!(b.calls.lock().is_empty(), "endpoint B must remain unused");
        let g2 = reload(&config, KEYS[2], &b.url, &["api_key", "api_url"]);
        let selected_before = memory.selected.load(Ordering::SeqCst);
        run_owned_channel_probe(context(&tmp, Arc::clone(&config), recorded, channel.clone(), Arc::clone(&cache), histories), message(2), CancellationToken::new()).await;
        assert!(memory.selected.load(Ordering::SeqCst) > selected_before, "G2 must obtain genuine selected evidence");
        assert_eq!(*a.calls.lock(), vec![0, 1]); assert_eq!(*b.calls.lock(), vec![2]);
        assert_eq!(a.invalid.load(Ordering::SeqCst), 0); assert_eq!(b.invalid.load(Ordering::SeqCst), 0);
        assert_eq!(*committed.lock(), vec![g1.id.0, g2.id.0]);
        let principal = crate::runtime::envelope::RuntimeEnvelope::channel(tmp.path().to_string_lossy().to_string(), "telegram", "prx_user", Some("prx_channel".to_owned())).memory_principal();
        let events = recording.list_message_events_recent(&principal, 100).await.expect("actual generation event history");
        let decisions = rusqlite::Connection::open_with_flags(tmp.path().join("self/causal_tree/decisions.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("durable decision evidence");
        for (phase, generation) in [g0, g1, g2].iter().enumerate() {
            let users: Vec<_> = events.iter().filter(|event| event.role == "user" && event.content == INPUT && event.config_generation_id == Some(generation.id.0)).collect();
            assert_eq!(users.len(), 1);
            let user = users.first().expect("unique generation-owned input");
            assert_eq!(user.config_source_revision.as_deref(), Some(generation.source_revision.fingerprint_sha256.as_str()));
            assert!(user.owner_id.is_some()); assert!(user.run_id.is_some());
            let mut query = decisions.prepare("SELECT candidates_json, selected_candidate_id, backend_kind FROM decisions WHERE run_id = ?1 AND source_event_id = ?2 AND enabled_snapshot = 1 LIMIT 33").expect("scoped persisted decision query");
            let rows = query.query_map(rusqlite::params![user.run_id.as_deref(), user.event_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?))).expect("persisted decision rows").collect::<rusqlite::Result<Vec<_>>>().expect("persisted decision decode");
            assert!(!rows.is_empty() && rows.len() <= 32, "bounded complete per-input decision evidence");
            assert!(rows.iter().all(|(_, _, backend)| backend == "builtin"));
            assert!(rows.iter().any(|(candidates, selected, _)| {
                let candidates: Vec<crate::causal_tree::store::RecordedCandidate> = serde_json::from_str(candidates).expect("stored actual candidates");
                candidates.iter().any(|candidate| Some(candidate.id.as_str()) == selected.as_deref() && candidate.action == "retrieve" && candidate.target == "memory")
            }), "actual input must have a persisted selected Memory retrieval");
            let marker = format!("prx-factory-generation-{phase}");
            assert!(events.iter().any(|event| event.role == "assistant" && event.content.contains(&marker) && event.run_id == user.run_id && event.config_generation_id == user.config_generation_id && event.config_source_revision == user.config_source_revision));
            assert!(channel.sent_messages.lock().await.iter().any(|message| message.contains(&marker)));
        }
        assert!(recording.owned_readbacks() >= 3);
        drop(participant);
    }).catch_unwind().await;
    // Release response barriers and join both owned servers even on assertion panic.
    let (closed_a, closed_b) = tokio::join!(a.close(), b.close());
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    closed_a.expect("endpoint A owned shutdown");
    closed_b.expect("endpoint B owned shutdown");
}

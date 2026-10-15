//! Verify that business Effects really execute once EffectExecutor switches from shadow to real mode.
//!
//! This is the core falsification test for Codex P0 — shadow_mode always true + effect no-op + diff=0
//! is circular reasoning; this module builds real mock deps and proves that:
//!   1. SaveSession → memory.store is called
//!   2. CancelDraft → channel.cancel_draft is called
//!   3. NotifyHook → hooks.emit is called (fed back from a spawned subtask, so we must wait)
//!   4. Quit → shutdown.cancel() is called
//!   5. StartTurn → a spawned subtask feeds back Action::RedrawRequested
//!   6. dual_write_guard is set after a persistence effect

#![cfg(test)]
use super::*;
use crate::channels::TerminalChannel;
use crate::chat::session::ChatSession;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryCategory, NoneMemory};
use crate::observability::NoopObserver;
use crate::providers::Provider;
use crate::providers::router::MockEnvProvider;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Wrapper that counts memory.store calls (NoneMemory does not really store, it only traces calls).
struct CountingMemory {
    inner: NoneMemory,
    store_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Memory for CountingMemory {
    fn name(&self) -> &str {
        "counting"
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.store(key, content, category, session_id).await
    }
    async fn recall(&self, q: &str, l: usize, s: Option<&str>) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.inner.recall(q, l, s).await
    }
    async fn get(&self, k: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        self.inner.get(k).await
    }
    async fn list(
        &self,
        c: Option<&MemoryCategory>,
        s: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        self.inner.list(c, s).await
    }
    async fn forget(&self, k: &str) -> anyhow::Result<bool> {
        self.inner.forget(k).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

/// Poll until an atomic counter reaches the target, avoiding a fixed sleep that is flaky on slow machines
async fn wait_for_count(counter: &AtomicUsize, target: usize, timeout: Duration) -> usize {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let cur = counter.load(std::sync::atomic::Ordering::SeqCst);
        if cur >= target || std::time::Instant::now() >= deadline {
            return cur;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Counting HookManager wrapper — reuses HookManager directly but in a temporary directory.
fn build_hook_manager() -> (Arc<HookManager>, TempDir) {
    let temp = TempDir::new().expect("tempdir");
    let mgr = HookManager::new(temp.path().to_path_buf());
    (Arc::new(mgr), temp)
}

/// Build complete EffectDeps with mock providers / memory / channel / hooks.
// Explicit Q42 call sites only; original serving objects are borrowed unchanged.
async fn admit_start_turn_fixture(
    deps: &EffectDeps,
    run: &str,
    input: &str,
    native_tools: bool,
) -> anyhow::Result<Arc<crate::chat::decision_source::ChatTurnBinding>> {
    let model = deps.model.current();
    let registry = deps.tools_registry.clone().unwrap_or_else(|| Arc::new(Vec::new()));
    let mut config = crate::config::Config {
        // Real-mode tests run concurrently. Give every admitted turn its
        // own decision/event database so WAL setup cannot contend with a
        // peer fixture using the same security-policy root.
        workspace_dir: deps
            .tool_security_policy
            .workspace_dir
            .join(format!("turn-fixture-{}", uuid::Uuid::now_v7())),
        default_provider: Some("ollama".to_owned()),
        default_model: Some(model.to_string()),
        tool_tiering: deps.tool_tiering.clone(),
        ..crate::config::Config::default()
    };
    config.config_path = config.workspace_dir.join("config.toml");
    config.autonomy.level = deps.tool_security_policy.autonomy;
    // These fixtures characterize provider streaming and tool-loop
    // behavior. Publish their explicit in-memory registry as the
    // configured direct surface so the production decision service, not
    // a raw Tool call, still owns every execution boundary.
    if native_tools {
        for tool in registry.iter() {
            let name = tool.name();
            if !config
                .tool_tiering
                .always_include
                .iter()
                .any(|included| included == name)
                && !config
                    .tool_tiering
                    .always_exclude
                    .iter()
                    .any(|excluded| excluded == name)
            {
                config.tool_tiering.always_include.push(name.to_owned());
            }
        }
    }
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider":"ollama", "model":model.to_string(), "operations":["chat"],
        "native_tools":native_tools, "streaming":deps.provider.supports_streaming()
    }))?);
    config.cost.prices.insert(
        format!("ollama/{model}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let generation = crate::config::new_shared(config).pin();
    let ledger: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&generation.effective.workspace_dir)?);
    let service = crate::runtime::turn_publication::TurnDecisionPublication {
        generation: Arc::clone(&generation),
        registry,
        mcp_tool: None,
        security: Arc::clone(&deps.tool_security_policy),
        observer: Arc::clone(&deps.observer),
        hooks: Arc::clone(&deps.hooks),
        memory: Arc::clone(&ledger),
        plugin_generation: 0,
    }
    .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
    .await?;
    crate::runtime::turn_cleanup::current_tracker()
        .expect("Q42 scoped fixture owner")
        .retain_service(Arc::clone(&service));
    let session = run;
    let envelope = crate::chat::chat_runtime_envelope(&generation.effective.workspace_dir.to_string_lossy(), session)
        .with_config_generation(&generation)
        .with_run_id(run.to_owned());
    let fabric = crate::memory::MemoryFabric::new(
        Arc::clone(&ledger),
        generation.effective.workspace_dir.to_string_lossy(),
    )
    .with_event_recording(generation.effective.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await?;
    let receipt = crate::chat::decision_source::ChatSourceReceipt::issue(
        ledger.as_ref(),
        &generation,
        session,
        run,
        input,
        &event,
    )
    .await?;
    let selected =
        crate::chat::decision_source::ChatProviderBinding::capture(&generation, &deps.provider, "ollama", &model)?
            .expect("Q42 original serving provider binding");
    let binding = receipt.bind_provider(
        &selected,
        crate::chat::decision_source::ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
    )?;
    let context = binding.decide(&service, &CancellationToken::new()).await?;
    use crate::causal_tree::directive::TurnExecutionDirective;
    let prerequisite = match context.directive() {
        TurnExecutionDirective::Clarify { reason_code } => Some(("Clarify", reason_code.as_str())),
        TurnExecutionDirective::Refuse { policy_code } => Some(("Refuse", policy_code.as_str())),
        _ => None,
    };
    if let Some((directive, reason)) = prerequisite {
        let reason = match reason {
            "insufficient_evidence"
            | "commit_threshold_or_margin"
            | "no_legal_path"
            | "hop_stopped"
            | "choice_outside_legal_set"
            | "all_gated" => reason,
            _ => "unclassified",
        };
        eprintln!("PRX_Q42_ADMISSION directive={directive} reason_code={reason}");
    }
    Ok(binding)
}

async fn admit_owned_start_turn_fixture(
    deps: &EffectDeps,
    run: &str,
    input: &str,
    native_tools: bool,
) -> anyhow::Result<(
    Arc<crate::chat::decision_source::ChatTurnBinding>,
    crate::runtime::turn_cleanup::CleanupScope,
)> {
    let owner = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())?;
    let binding = owner
        .scope_admission(admit_start_turn_fixture(deps, run, input, native_tools))
        .await?;
    Ok((binding, owner))
}

async fn finish_owned_start_turn_fixture(owner: crate::runtime::turn_cleanup::CleanupScope) {
    owner
        .run(async { Ok::<_, anyhow::Error>(()) })
        .await
        .expect("Q42 fixture cleanup");
}

fn build_deps(
    memory: Arc<dyn Memory>,
    shutdown: CancellationToken,
) -> (EffectDeps, mpsc::Receiver<Action>, Arc<HookManager>, TempDir) {
    let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
    let (hooks, temp) = build_hook_manager();
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
    let (action_tx, action_rx) = mpsc::channel::<Action>(64);
    let dual_write_guard = RuntimeDualWriteGuard::new();
    let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
    let deps = EffectDeps {
        provider,
        memory,
        memory_event_recording: MemoryEventRecording::default(),
        channel,
        hooks: Arc::clone(&hooks),
        observer,
        action_tx,
        provider_turn_lifecycle_tx: None,
        dual_write_guard,
        redraw_tx: Some(redraw_tx),
        #[cfg(feature = "terminal-tui")]
        tui_mirror: None,
        shutdown,
        model: ModelSlot::from("test-model"),
        temperature: 0.0,
        tools_registry: None,
        approval_router: Arc::new(ApprovalRouter::new()),
        tool_security_policy: full_tool_security_policy(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };
    (deps, action_rx, hooks, temp)
}

// ─── P1: fill in the real business unit test coverage for Effects (7/7) ──────────────────

/// CountingChannel: records how many times send is called (wraps TerminalChannel).
struct CountingChannel {
    inner: crate::channels::TerminalChannel,
    send_count: Arc<AtomicUsize>,
    finalize_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::channels::Channel for CountingChannel {
    fn name(&self) -> &str {
        "counting"
    }
    async fn send(&self, message: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        self.send_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.send(message).await
    }
    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        self.inner.listen(tx).await
    }
    async fn finalize_draft(&self, recipient: &str, message_id: &str, text: &str) -> anyhow::Result<()> {
        self.finalize_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.finalize_draft(recipient, message_id, text).await
    }
    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        self.inner.cancel_draft(recipient, message_id).await
    }
}

/// Build CountingChannel deps.
fn build_counting_channel_deps(
    send_count: Arc<AtomicUsize>,
    finalize_count: Arc<AtomicUsize>,
    shutdown: CancellationToken,
) -> (EffectDeps, mpsc::Receiver<Action>, TempDir) {
    let provider: Arc<dyn crate::providers::Provider> = Arc::new(crate::providers::router::MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(CountingChannel {
        inner: crate::channels::TerminalChannel::new(true),
        send_count,
        finalize_count,
    });
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let (hooks, temp) = build_hook_manager();
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
    let (action_tx, action_rx) = mpsc::channel::<Action>(64);
    let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
    let deps = EffectDeps {
        provider,
        memory,
        memory_event_recording: MemoryEventRecording::default(),
        channel,
        hooks,
        observer,
        action_tx,
        provider_turn_lifecycle_tx: None,
        dual_write_guard: RuntimeDualWriteGuard::new(),
        redraw_tx: Some(redraw_tx),
        #[cfg(feature = "terminal-tui")]
        tui_mirror: None,
        shutdown,
        model: ModelSlot::from("test-model"),
        temperature: 0.0,
        tools_registry: None,
        approval_router: Arc::new(ApprovalRouter::new()),
        tool_security_policy: full_tool_security_policy(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };
    (deps, action_rx, temp)
}

mod approval_retry;
mod compaction_overflow;
mod compaction_trim;
mod dual_write;
mod effects;
mod replay;
mod start_turn;
mod step5a4;
mod tool_protocol;

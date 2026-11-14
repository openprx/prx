use crate::memory::{Memory, MemoryFabric};
use crate::observability::causal_event::testing::RecordingSink;
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::runtime::turn_context::AgentTurnKind;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_publication::service::test_support::{CountingTransports, sources};
use crate::runtime::turn_publication::{PublicationInputs, build_publication};
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::SurfacePolicy;
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
    TracingToolExecutionAudit,
};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct DriftingCatalogTool {
    changed: Arc<AtomicBool>,
    dispatches: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for DriftingCatalogTool {
    fn name(&self) -> &str {
        "prx_drift_tool"
    }

    fn description(&self) -> &str {
        if self.changed.load(Ordering::SeqCst) {
            "changed PRX fixture description"
        } else {
            "initial PRX fixture description"
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }

    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("catalog drift must not dispatch a tool")
    }
}

struct LinkedFixture {
    workspace: tempfile::TempDir,
    config: crate::config::Config,
    memory: Arc<dyn Memory>,
    service: Arc<TurnRuntimeService>,
    proof: crate::agent::entry::AgentTurnProof,
    commit: crate::agent::terminal::TurnTerminalDraft,
    transports: CountingTransports,
    dispatches: Arc<AtomicUsize>,
}

impl LinkedFixture {
    async fn new() -> Self {
        Self::with_enabled(true).await
    }

    async fn with_enabled(enabled: bool) -> Self {
        let workspace = tempfile::tempdir().expect("owned workspace");
        let mut config = crate::config::Config::default();
        config.decision_engine.enabled = true;
        config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
            crate::config::schema::causal_tree::BackendKind::Builtin,
        );
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = enabled;
        let generation = crate::config::new_shared(config.clone()).pin();
        let changed = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
            changed: Arc::clone(&changed),
            dispatches: Arc::clone(&dispatches),
        })]);
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("owning SQLite backend"));
        let transports = CountingTransports(AtomicUsize::new(0));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let service = build_publication(
            TurnSurfaceKind::AgentLoop,
            PublicationInputs {
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(RecordingSink::default()),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .await
        .expect("actual recorded service");
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            Arc::clone(&memory),
            "  ",
            "prx_scope_run",
        )
        .await;
        changed.store(true, Ordering::SeqCst);
        let context = service
            .open_issued_turn_context(&proof, AgentTurnKind::Repl, &CancellationToken::new())
            .await
            .expect("genuine catalog drift terminal");
        assert!(
            context.execution_refs().is_empty(),
            "test: the drifted catalog grants nothing to the plain answer"
        );
        let now = chrono::Utc::now();
        let mut scope = crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
            .with_owner_id(proof.owner_id())
            .with_channel("cli")
            .with_session_key(proof.session_key())
            .with_run_id(proof.run_id());
        scope.causation_event_id = proof.source_event_id().map(str::to_owned);
        // The terminal is a plain answer served by the model. Record that
        // serving request on the issued trace exactly as the serving loop
        // does, so the terminal carries real serving evidence.
        let outcome = crate::llm::route_decision::ProviderExecutionOutcome::success_for_decision(
            &crate::llm::route_decision::RouteDecision::single_candidate("ollama", "prx-local-model"),
            now,
        );
        let trace = context.runtime().execution_trace.get_or_init(Default::default);
        trace
            .bind_exact_call_key(Arc::clone(&context.runtime().state_key))
            .expect("serving trace binds the decision key");
        trace
            .begin_provider()
            .expect("serving request observed")
            .finish(Some(&outcome.attempts));
        let served = Some(outcome);
        let commit = crate::agent::terminal::TurnTerminalDraft {
            binding: Some(service.terminal_binding(&context).expect("actual source-bound binding")),
            terminal_id: "prx_scope_terminal".to_owned(),
            scope: scope.clone(),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: "PRX local terminal response".to_owned(),
                history_commit_len: 0,
            }),
            history_scope: Some(scope.with_sender("ollama/prx-local-model").with_recipient("local-user")),
            provider_outcome: served,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "linked local scope boundary".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        };
        let fixture = Self {
            workspace,
            config,
            memory,
            service,
            proof,
            commit,
            transports,
            dispatches,
        };
        fixture.assert_no_outcome().await;
        fixture
    }

    fn fabric(&self, memory: Arc<dyn Memory>) -> MemoryFabric {
        MemoryFabric::new(memory, self.workspace.path().to_string_lossy())
    }

    async fn assert_no_outcome(&self) {
        assert_eq!(self.transports.0.load(Ordering::SeqCst), 0);
        assert_eq!(self.dispatches.load(Ordering::SeqCst), 0);
    }

    async fn marker_presence(&self, fabric: &MemoryFabric) -> [bool; 4] {
        let mut presence = [false; 4];
        for (slot, suffix) in presence
            .iter_mut()
            .zip(["decision-claim", "assistant", "usage", "final"])
        {
            *slot = fabric
                .memory()
                .find_message_event_by_idempotency_key(
                    fabric.workspace_id(),
                    &format!("turn:{}:{suffix}", self.commit.terminal_id),
                )
                .await
                .expect("owned marker lookup")
                .is_some();
        }
        presence
    }

    #[allow(clippy::print_stderr)]
    async fn reject_before_writes(&self, fabric: &MemoryFabric, commit: crate::agent::terminal::TurnTerminalDraft) {
        let result =
            crate::agent::terminal::finalize_turn(fabric, commit, &self.config.cost, self.workspace.path()).await;
        // Keep metadata-only red evidence from the genuine stores, not a fake result.
        let marker_presence = self.marker_presence(fabric).await;
        eprintln!(
            "PRX_FOUNDATION_TERMINAL_SCOPE accepted={} markers={marker_presence:?}",
            result.is_ok()
        );
        assert!(
            result.is_err(),
            "linked finalizer must reject foreign scope before any write"
        );
        assert_eq!(marker_presence, [false; 4]);
        assert_eq!(
            self.marker_presence(&self.fabric(Arc::clone(&self.memory))).await,
            [false; 4]
        );
        self.assert_no_outcome().await;
        assert!(!self.workspace.path().join("state/costs.jsonl").exists());
        self.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }
}

#[tokio::test]
async fn linked_finalizer_rejects_same_label_foreign_sqlite_before_writes() {
    let fixture = LinkedFixture::new().await;
    let foreign_dir = tempfile::tempdir().expect("separate owned backend");
    let foreign: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(foreign_dir.path()).expect("foreign real SQLite"));
    assert!(!Arc::ptr_eq(&foreign, &fixture.memory));
    let fabric = fixture.fabric(foreign);
    assert_eq!(fabric.workspace_id(), fixture.workspace.path().to_string_lossy());
    fixture.reject_before_writes(&fabric, fixture.commit.clone()).await;
    assert!(!foreign_dir.path().join("state/costs.jsonl").exists());
}

async fn reject_history_change(mutate: fn(&mut crate::memory::MessageEventScope)) {
    let fixture = LinkedFixture::new().await;
    let mut commit = fixture.commit.clone();
    mutate(commit.history_scope.as_mut().expect("actual alternate history scope"));
    fixture
        .reject_before_writes(&fixture.fabric(Arc::clone(&fixture.memory)), commit)
        .await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_owner_before_writes() {
    reject_history_change(|scope| scope.owner_id = Some("prx_foreign_owner".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_session_before_writes() {
    reject_history_change(|scope| scope.session_key = Some("prx_foreign_session".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_foreign_run_before_writes() {
    reject_history_change(|scope| scope.run_id = Some("prx_foreign_run".to_owned())).await;
}

#[tokio::test]
async fn linked_history_rejects_broader_visibility_before_writes() {
    reject_history_change(|scope| scope.visibility = crate::memory::MemoryVisibility::Global).await;
}

#[tokio::test]
async fn linked_terminal_rejects_broader_visibility_before_writes() {
    let fixture = LinkedFixture::new().await;
    let mut commit = fixture.commit.clone();
    commit.scope.visibility = crate::memory::MemoryVisibility::Global;
    fixture
        .reject_before_writes(&fixture.fabric(Arc::clone(&fixture.memory)), commit)
        .await;
}

#[tokio::test]
async fn linked_finalizer_accepts_own_memory_and_same_scope_presentation() {
    let fixture = LinkedFixture::new().await;
    let fabric = fixture.fabric(Arc::clone(&fixture.memory));
    let first = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("owning link with matching history identity");
    let second = crate::agent::terminal::finalize_turn(
        &fabric,
        fixture.commit.clone(),
        &fixture.config.cost,
        fixture.workspace.path(),
    )
    .await
    .expect("idempotent same scope replay");
    assert_eq!(first.terminal_event.id, second.terminal_event.id);
    assert_eq!(
        first.assistant_event.as_ref().expect("actual history").id,
        second.assistant_event.as_ref().expect("same actual history").id
    );
    assert_eq!(fixture.marker_presence(&fabric).await, [true, true, false, true]);
    let history = first
        .assistant_event
        .as_ref()
        .expect("persisted alternate presentation");
    assert_eq!(history.owner_id.as_deref(), Some(fixture.proof.owner_id()));
    assert_eq!(history.session_key.as_deref(), Some(fixture.proof.session_key()));
    assert_eq!(history.run_id.as_deref(), Some(fixture.proof.run_id()));
    assert_eq!(history.channel.as_deref(), Some("cli"));
    assert_eq!(history.source, crate::memory::MessageEventSource::Agent);
    assert_eq!(history.visibility, crate::memory::MemoryVisibility::Session);
    assert_eq!(history.sender.as_deref(), Some("ollama/prx-local-model"));
    assert_eq!(history.recipient.as_deref(), Some("local-user"));
    assert_eq!(fixture.transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.dispatches.load(Ordering::SeqCst), 0);
    fixture.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

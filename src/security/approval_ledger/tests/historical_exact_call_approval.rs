#![cfg(test)]
//! Filesystem discovery denial plus shared genuine-source approval regression fixtures.
//! Controlled decision answers never manufacture a source, grant, or call digest.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::agent::turn_contracts::ChatMode;
use crate::chat::binding::tests::recorded_approval_source;
use crate::config::schema::causal_tree::{BackendKind, BackendLocality};
use crate::cost::decision_attempt::BackendIdentity;
use crate::cost::decision_attempt::error::DecisionError;
use crate::decision_engine::prediction::backend::DecisionBackend;
use crate::decision_engine::prediction::builtin::BuiltinDecisionBackend;
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, SlotLimits};
use crate::decision_engine::prediction::outbound::{GuardedBody, OutboundSecrets};
use crate::decision_engine::prediction::protocol::{
    DecisionAnswer, DecisionRequest, DecisionResponse, GlobalQuestion, Probability, QuestionId,
};
use crate::decision_engine::prediction::validate::ModelVersionPolicy;
use crate::memory::Memory;
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::runtime::turn_context::AgentTurnKind;
use crate::runtime::turn_decision::tests::historical_support::{
    CountingTransports, SilentEvents, force_recordable_durable, install_factory, recordable_and_durable, sources,
    state_key,
};
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_publication::{PublicationInputs, build_publication};
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{Exposure, SurfacePolicy};
use crate::tools::capability::contract::CapabilityTarget;
use crate::tools::capability::search_tool::{CapabilitySearchTool, with_search_authority};
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::ToolExecutionContext;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
    TracingToolExecutionAudit,
};
use crate::tools::file_write::FileWriteTool;
use crate::tools::traits::{Tool, ToolResult};

struct CountedFileMutation {
    inner: Box<dyn Tool>,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for CountedFileMutation {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(args).await
    }
}

// Test-only predictions target the actual FileSystem tool family.
// Production compilation, gates and exact-call issuance remain authoritative.
struct FilesystemApprovalBackend {
    identity: BackendIdentity,
    /// Capability family the deterministic fixture steers the decision to.
    family: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DecisionBackend for FilesystemApprovalBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
    // This fixture performs only deterministic local computation. Supporting
    // the accounting entry does not mint a physical attempt or fake usage.
    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        self.evaluate(request, body, deadline).await
    }

    async fn evaluate(
        &self,
        request: &DecisionRequest,
        _: &GuardedBody,
        _: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".to_owned();
        response.resolved_model = self.identity.configured_model.to_string();
        for (id, answer) in &mut response.answers {
            // Ordinary family candidates have no namespace suffix; scoped
            // candidates retain the hyphen-delimited namespace form.
            let work = id.as_str().starts_with(&format!("candidate.use_tool-{}.", self.family))
                || id.as_str().starts_with(&format!("candidate.use_tool-{}-", self.family));
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        "use_tool"
                    } else {
                        self.family
                    };
                    if let Some(chosen) = choice.probabilities.keys().find(|key| key.as_str() == code).cloned() {
                        for (key, probability) in &mut choice.probabilities {
                            *probability = Probability::new(if key == &chosen { 1.0 } else { 0.0 })?;
                        }
                        choice.chosen = chosen;
                        choice.confidence = Probability::new(1.0)?;
                    }
                }
                DecisionAnswer::Noul(noul) => {
                    noul.probability_true = Probability::new(
                        if id == &QuestionId::global(GlobalQuestion::EvidenceSufficient) || work {
                            1.0
                        } else {
                            0.0
                        },
                    )?;
                }
                DecisionAnswer::Score(score) => {
                    let gain = id.as_str().ends_with("gain") && (work || !id.as_str().starts_with("candidate."));
                    let chosen = if gain {
                        score.distribution.len().saturating_sub(1)
                    } else {
                        0
                    };
                    for (index, probability) in score.distribution.iter_mut().enumerate() {
                        *probability = Probability::new(if index == chosen { 1.0 } else { 0.0 })?;
                    }
                    score.expected = f64::from(u32::try_from(chosen).expect("bounded score level"));
                    score.confidence = Probability::new(1.0)?;
                }
            }
        }
        Ok(response)
    }
}

#[tokio::test]
async fn unmentioned_file_write_stays_outside_collection_and_execution() {
    Box::pin(exact_file_without_intent(false, None)).await;
}

#[tokio::test]
async fn unmentioned_file_edit_has_no_filesystem_discovery_offer() {
    Box::pin(exact_file_without_intent(true, None)).await;
}

#[tokio::test]
async fn unrelated_chat_input_cannot_create_pending_file_approval() {
    Box::pin(exact_file_without_intent(false, None)).await;
}

mod reported_usage;

#[tokio::test]
async fn bind_keeps_no_ledger_for_tui_line_mode_and_cli() {
    Box::pin(exact_file_without_intent(false, Some(false))).await;
}

#[tokio::test]
async fn bind_preserves_explicit_ledger_identity() {
    Box::pin(exact_file_without_intent(false, Some(true))).await;
}

async fn exact_file_without_intent(edit: bool, bind_ledger: Option<bool>) {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = tempfile::tempdir().unwrap();
    let tool_name = if edit { "file_edit" } else { "file_write" };
    let original = "prefix\noriginal contents\nsuffix\n";
    if edit {
        std::fs::write(workspace.path().join("prx-approved.txt"), original).unwrap();
    }
    std::fs::write(workspace.path().join("prx-unrelated.txt"), "unrelated contents").unwrap();
    let assert_unmodified = || {
        if edit {
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("prx-approved.txt")).unwrap(),
                original
            );
        } else {
            assert!(!workspace.path().join("prx-approved.txt").exists());
        }
    };
    let mut config = crate::config::Config::default();
    config.decision_engine.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::Builtin,
    );
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.causal_tree.enabled = true;
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    config.cost.prices.insert(
        "prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .unwrap(),
    );
    config.save().await.expect("write fixture-only configuration");
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false)),
        Box::new(CountedFileMutation {
            inner: if edit {
                Box::new(crate::tools::file_edit::FileEditTool::new(Arc::clone(&security)))
            } else {
                Box::new(FileWriteTool::new(Arc::clone(&security)))
            },
            calls: Arc::clone(&dispatches),
        }),
        Box::new(CapabilitySearchTool),
    ]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let mut service = build_publication(
        TurnSurfaceKind::Interactive,
        PublicationInputs {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["file_read".to_owned()]),
                excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
                ..SurfacePolicy::default()
            },
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .unwrap();
    use futures_util::FutureExt as _;
    let result = std::panic::AssertUnwindSafe(Box::pin(async {
        // Deterministic local decision fixture, not hosted-backend admission
        // or a claim about builtin multilingual semantic understanding.
        let predictions = Arc::new(AtomicUsize::new(0));
        let owning = Arc::get_mut(&mut service).unwrap();
        let key = state_key(owning);
        install_factory(
            owning,
            DecisionBackendFactory::from_slots(
                vec![Box::new(FilesystemApprovalBackend {
                    identity: BackendIdentity {
                        kind: BackendKind::Mock,
                        provider: None,
                        configured_model: Arc::from("prx-local-model"),
                    },
                    family: "filesystem",
                    calls: Arc::clone(&predictions),
                })],
                key,
                OutboundSecrets::default(),
                SlotLimits::default(),
            ),
        );
        assert!(recordable_and_durable(&service));
        let (proof, envelope) = recorded_approval_source(
            &generation,
            Arc::clone(&memory),
            "Inspect project context",
            ChatMode::Edit,
            "prx_approval_original",
            1,
        )
        .await;
        assert!(service.model_can_call_tools(proof.as_ref()));
        let cancel = CancellationToken::new();
        let context = service
            .open_issued_turn_context(proof.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        assert_eq!(
            predictions.load(Ordering::SeqCst),
            0,
            "the turn opens prediction-free; the backend is never asked (R9)"
        );
        let read_id = service.catalog().entry_for_public_name("file_read").unwrap().id.clone();
        assert!(
            context.execution_refs().iter().any(|reference| reference.id == read_id),
            "the direct surface publishes the read capability"
        );
        let selected = crate::tools::capability::execution_binding::specs::execution_specs(
            &service.surface_port(&context),
            proof.as_ref(),
        )
        .unwrap();
        if let Some(with_ledger) = bind_ledger {
            assert!(
                service.execution_approval_ledger().is_some(),
                "fixture must have a live durable ledger"
            );
            let incoming = if with_ledger {
                ["console", "gateway", "h2", "bg"]
                    .into_iter()
                    .map(|surface| {
                        (
                            surface,
                            ToolExecutionContext::new(envelope.clone(), proof.policy_chat_type())
                                .with_approval_ledger(service.execution_approval_ledger()),
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![
                    ("tui", crate::chat::approval_context::tui(envelope.clone())),
                    ("line_mode", crate::chat::approval_context::line_mode(envelope.clone())),
                    ("cli", crate::agent::entry::cli_tool_execution_context(envelope.clone())),
                ]
            };
            for (surface, incoming) in incoming {
                let (_, bound) = service
                    .bind_agent_execution_with_runtime(proof.as_ref(), &context, &incoming, &selected.specs, None)
                    .expect("real decision execution bind");
                assert_eq!(
                    bound.approval_ledger.is_some(),
                    with_ledger,
                    "bind must preserve {surface} approval routing"
                );
                if let (Some(before), Some(after)) = (&incoming.approval_ledger, &bound.approval_ledger) {
                    assert!(Arc::ptr_eq(&before.memory, &after.memory));
                    assert!(Arc::ptr_eq(&before.state_key, &after.state_key));
                    assert!(Arc::ptr_eq(&before.catalog, &after.catalog));
                }
            }
        }

        assert_eq!(
            selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            ["file_read"]
        );
        // Search stays reachable, but publishing its schema does not offer or
        // admit a file mutation or authorize approval collection.

        assert!(selected.specs.iter().all(|spec| spec.name != tool_name));
        let entry = service.catalog().entry_for_public_name(tool_name).unwrap();
        assert!(
            !context
                .execution_refs()
                .iter()
                .any(|reference| reference.id == entry.id)
        );
        assert!(
            service
                .agent_capability_search_spec(proof.as_ref(), &context)
                .unwrap()
                .is_some()
        );
        assert!(service.capability_search_port(proof.as_ref(), &context).is_ok());
        assert!(
            service
                .admit_discovered_tool(proof.as_ref(), &context, tool_name)
                .unwrap()
                .is_none()
        );
        assert!(
            service
                .agent_appended_execution_specs(proof.as_ref(), &context)
                .unwrap()
                .specs
                .is_empty()
        );
        assert!(
            service
                .inspect_pending_decision_approval(proof.owner_id(), proof.session_key(), context.decision_id())
                .await
                .is_err()
        );
        let pending_key = format!("decision-approval:{}:pending", context.decision_id());
        assert!(
            memory
                .find_owned_message_event_by_idempotency_key(proof.workspace_id(), proof.owner_id(), &pending_key)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert_unmodified();
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("prx-unrelated.txt")).unwrap(),
            "unrelated contents"
        );
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    }))
    .catch_unwind()
    .await;
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    let cleanup = tokio::time::timeout_at(cleanup_deadline.into(), service.drain_writes(cleanup_deadline)).await;
    let confirmed = matches!(cleanup, Ok(true));
    if !confirmed || result.is_err() {
        let retained = workspace.keep();
        use std::io::Write as _;
        if let Err(error) = writeln!(
            std::io::stdout().lock(),
            "filesystem negative fixture evidence retained at {}",
            retained.display()
        ) {
            tracing::error!(%error, "Failed to write retained filesystem negative fixture path");
        }
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(confirmed, "approval fixture final writer cleanup must be confirmed");
}

mod native_usage;

mod ordinary_plane;

/// R9.6 (V-17): every resume turn now opens prediction-free (directive
/// `AnswerDirectly`) whatever the engine setting, so the historical resume
/// check is execution-owned. With the plane on and the engine on or off, the
/// resume surface built from the real execution service and restricted to
/// the prepared tool admits exactly that tool; another tool, a widened
/// surface or a missing surface is refused, and a changed argument is a
/// different command than the prepared one.
#[tokio::test]
async fn historical_resume_survives_prediction_free_opening() {
    use crate::security::approval_ledger::historical::published_only;
    use crate::tools::execution::admission::TurnSurface;
    for engine in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.decision_engine.enabled = engine;
        config.causal_tree.enabled = true;
        config.workspace_dir = workspace.path().to_owned();
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&security), false)),
            Box::new(CountedFileMutation {
                inner: Box::new(FileWriteTool::new(Arc::clone(&security))),
                calls: Arc::clone(&dispatches),
            }),
        ]);
        let execution = ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        );
        let full = TurnSurface::build_from(
            &execution,
            execution.current_descriptors(),
            &config.tool_tiering,
            "prx-local-model",
            false,
            |_| true,
        );
        assert!(
            full.contains("file_write") && full.contains("file_read"),
            "engine {engine}"
        );
        let context = |surface: Option<TurnSurface>| {
            let mut context = ToolExecutionContext::new(
                crate::runtime::envelope::RuntimeEnvelope::agent("workspace-a", "resume-run")
                    .with_channel("terminal")
                    .with_sender("owner-a")
                    .with_owner_id("owner-a"),
                "dm",
            );
            context.turn_surface = surface.map(Arc::new);
            context
        };
        // The surface owner restricts a resume turn to the prepared tool.
        let resume = context(Some(full.restricted_to("file_write")));
        assert!(
            published_only(&resume, "file_write"),
            "engine {engine}: approve executes the prepared call"
        );
        assert!(
            !published_only(&resume, "file_read"),
            "engine {engine}: another tool is refused"
        );
        assert!(
            !published_only(&context(Some(full.clone())), "file_write"),
            "engine {engine}: a widened surface is refused"
        );
        assert!(
            !published_only(&context(None), "file_write"),
            "engine {engine}: no surface is refused"
        );
        // A changed argument is not the prepared command, which the adapter
        // compares byte-for-byte before the surface check.
        let prepared =
            crate::tools::ToolExecutionCommand::new("file_write", serde_json::json!({"path": "a.txt", "content": "x"}));
        let changed =
            crate::tools::ToolExecutionCommand::new("file_write", serde_json::json!({"path": "a.txt", "content": "y"}));
        assert_ne!(prepared, changed, "engine {engine}");
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            0,
            "engine {engine}: no execution without a claim"
        );
    }
}

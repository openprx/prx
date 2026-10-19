#![cfg(test)]
//! Filesystem discovery denial plus shared genuine-source approval regression fixtures.
//! Controlled decision answers never manufacture a source, grant, or call digest.
use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::chat::decision_source::admission_failure_tests::recorded_approval_source;
use crate::tools::capability::search_tool::{CapabilitySearchTool, with_search_authority};
use crate::tools::execution::{ToolExecutionCommand, ToolExecutionContext, ToolExecutionStatus};
use crate::tools::file_write::FileWriteTool;

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
        _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
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
    Box::pin(exact_file_without_intent(false)).await;
}

#[tokio::test]
async fn unmentioned_file_edit_has_no_filesystem_discovery_offer() {
    Box::pin(exact_file_without_intent(true)).await;
}

#[tokio::test]
async fn unrelated_chat_input_cannot_create_pending_file_approval() {
    Box::pin(exact_file_without_intent(false)).await;
}

#[path = "exact_call_approval/reported_usage.rs"]
mod reported_usage;

async fn exact_file_without_intent(edit: bool) {
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
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::Interactive,
        TurnServiceBuild {
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
    .unwrap();
    use futures_util::FutureExt as _;
    let result = std::panic::AssertUnwindSafe(Box::pin(async {
        // Deterministic local decision fixture, not hosted-backend admission
        // or a claim about builtin multilingual semantic understanding.
        let predictions = Arc::new(AtomicUsize::new(0));
        let owning = Arc::get_mut(&mut service).unwrap();
        owning.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(FilesystemApprovalBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                family: "filesystem",
                calls: Arc::clone(&predictions),
            })],
            Arc::clone(&owning.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        assert!(service.recordable && service.durable_key);
        let (proof, _envelope) = recorded_approval_source(
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
            .decide_agent_initial(proof.as_ref(), AgentTurnKind::Repl, &cancel)
            .await
            .unwrap();
        assert!(
            predictions.load(Ordering::SeqCst) > 0,
            "the actual controlled local backend must supply predictions"
        );
        assert!(matches!(context.directive(), TurnExecutionDirective::Execute { .. }));
        let selected = service.agent_execution_specs(proof.as_ref(), &context).unwrap();
        assert_eq!(
            selected.specs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            ["file_read"]
        );
        // This genuine Chat input did not mention either file mutation. Its
        // selected read is not authority to collect or discover a write.
        assert!(context.approval_collection_proof().is_none());
        assert!(service.begin_approval_collection(proof.as_ref(), &context).is_err());
        assert!(selected.specs.iter().all(|spec| spec.name != tool_name));
        let entry = service.catalog.entry_for_public_name(tool_name).unwrap();
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
                .is_none()
        );
        assert!(service.capability_search_port(proof.as_ref(), &context).is_err());
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

#[path = "exact_call_approval/native_usage.rs"]
mod native_usage;

#[path = "exact_call_approval/collection.rs"]
pub(crate) mod collection;

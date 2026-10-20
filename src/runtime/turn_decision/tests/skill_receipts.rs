#![cfg(test)]

//! Finding 84: the production sources of the skill lifecycle receipts. A turn
//! that selects a managed skill leaves a selection receipt in its experience;
//! restart recovery completes the experience with only the facts it can verify.
use super::*;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, Correlation, ExperienceOutcome, OwnerScope, SkillId, SkillLifecycleStore, SourceKind,
    test_support::publish_revision,
};
use crate::tools::capability::search_tool::CapabilitySearchTool;

const SKILL_NAME: &str = "prx_skill";

struct NoopTool;

#[async_trait::async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read project capability metadata"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::bail!("receipt tests must not dispatch a tool")
    }
}

/// A scripted decision backend that commits to executing the skill candidate.
struct SelectSkillBackend {
    identity: BackendIdentity,
}

#[async_trait::async_trait]
impl DecisionBackend for SelectSkillBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
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
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".to_owned();
        response.resolved_model = self.identity.configured_model.to_string();
        for (id, answer) in &mut response.answers {
            let work = id.as_str().starts_with("candidate.use_tool-skill.")
                || id.as_str().starts_with("candidate.use_tool-skill-");
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        "use_tool"
                    } else {
                        "skill"
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
                    score.expected = f64::from(u32::try_from(chosen).unwrap_or(0));
                    score.confidence = Probability::new(1.0)?;
                }
            }
        }
        Ok(response)
    }
}

fn skill_id() -> SkillId {
    SkillId::parse("prx-skill").expect("test: skill id")
}

/// Publishes the managed skill `prx_skill` for `owner` in the workspace.
fn managed_store(workspace: &std::path::Path, owner: &str) -> SkillLifecycleStore {
    let store =
        SkillLifecycleStore::open(workspace, OwnerScope::new(owner).expect("test: owner")).expect("test: store");
    let body = format!("---\nname: {SKILL_NAME}\ndescription: Inspect release metadata\n---\nmanaged body\n");
    let bundle = Bundle::from_entries(vec![BundleEntry {
        path: std::path::PathBuf::from("SKILL.md"),
        bytes: body.into_bytes(),
        executable: false,
    }])
    .expect("test: bundle");
    publish_revision(&store, &skill_id(), SourceKind::Operator, &bundle, 0);
    store
}

fn base_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.skills.open_skills_dir = Some(workspace.join("missing-open-skills").to_string_lossy().to_string());
    config.skills.openclaw_skills_dir = Some(workspace.join("missing-openclaw-skills").to_string_lossy().to_string());
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
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
        .expect("test: model"),
    );
    config
}

struct SelectedSkillTurn {
    workspace: tempfile::TempDir,
    service: Arc<TurnDecisionService>,
    proof: crate::agent::entry::AgentTurnProof,
    context: Arc<TurnDecisionContext>,
}

/// A real turn whose discovery selects and admits the workspace skill `prx_skill`.
async fn selected_skill_turn(lifecycle: bool) -> SelectedSkillTurn {
    let workspace = tempfile::tempdir().expect("test: workspace");
    let skill_dir = workspace.path().join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&skill_dir).expect("test: skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {SKILL_NAME}\ndescription: Inspect release metadata\n---\nlegacy body\n"),
    )
    .expect("test: skill file");
    let mut config = base_config(workspace.path());
    config.skills_lifecycle.enabled = lifecycle;
    config.skills_lifecycle.owner_scope = Some("owner".to_owned());
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(NoopTool),
        Box::new(crate::tools::SkillReadTool::new(
            workspace.path().to_owned(),
            config.clone(),
        )),
        Box::new(CapabilitySearchTool),
    ]);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: memory"));
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from([SKILL_NAME.to_owned()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("test: service");
    let owning = Arc::get_mut(&mut service).expect("test: unshared service");
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(SelectSkillBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
        })],
        Arc::clone(&owning.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "inspect the available release metadata capability",
        "prx_skill_receipts",
    )
    .await;
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
        .await
        .expect("test: decision");
    assert!(
        matches!(context.directive(), TurnExecutionDirective::Execute { .. }),
        "the scripted backend must select the skill"
    );
    SelectedSkillTurn {
        workspace,
        service,
        proof,
        context,
    }
}

fn selected_names(turn: &SelectedSkillTurn) -> anyhow::Result<Vec<String>> {
    Ok(turn
        .service
        .agent_prompt_skills(&turn.proof, &turn.context)?
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

#[tokio::test]
async fn a_turn_selecting_a_managed_skill_leaves_one_selection_receipt() {
    let turn = selected_skill_turn(true).await;
    let store = managed_store(turn.workspace.path(), turn.proof.owner_id());
    // Prompt assembly can run more than once per turn; the receipt is recorded once.
    for _ in 0..3 {
        assert_eq!(selected_names(&turn).expect("test: prompt skills"), [SKILL_NAME]);
    }
    crate::skills::lifecycle::drain_background_for_test(turn.proof.run_id()).await;
    let experience = store
        .read_experience(turn.proof.run_id())
        .expect("test: read")
        .expect("test: a selection receipt exists");
    assert_eq!(experience.selection_receipts.len(), 1);
    let receipt = experience.selection_receipts.first().expect("test: receipt");
    assert_eq!(receipt.skill_id, skill_id());
    assert_eq!(receipt.correlation.run_id.as_deref(), Some(turn.proof.run_id()));
    assert!(experience.used_skills().is_empty(), "selection never credits use");
    crate::skills::lifecycle::release_task_pins(
        &OwnerScope::new(turn.proof.owner_id()).expect("test: owner"),
        turn.proof.run_id(),
    );
}

#[tokio::test]
async fn a_disabled_lifecycle_writes_no_selection_receipt() {
    let turn = selected_skill_turn(false).await;
    let store = managed_store(turn.workspace.path(), turn.proof.owner_id());
    assert_eq!(selected_names(&turn).expect("test: prompt skills"), [SKILL_NAME]);
    assert!(
        store
            .read_experience(turn.proof.run_id())
            .expect("test: read")
            .is_none(),
        "nothing is written while the lifecycle is disabled"
    );
}

#[tokio::test]
async fn a_failing_selection_record_never_fails_the_turn() {
    let turn = selected_skill_turn(true).await;
    let store = managed_store(turn.workspace.path(), turn.proof.owner_id());
    std::fs::write(store.root().join("experiences"), b"not a directory").expect("test: block the capture");
    assert_eq!(
        selected_names(&turn).expect("the turn survives a capture failure"),
        [SKILL_NAME]
    );
    crate::skills::lifecycle::drain_background_for_test(turn.proof.run_id()).await;
    crate::skills::lifecycle::release_task_pins(
        &OwnerScope::new(turn.proof.owner_id()).expect("test: owner"),
        turn.proof.run_id(),
    );
}

/// Finalizes a recorded terminal marker before the workspace has a lifecycle
/// store (so the live call-in joins nothing, as when the process died between
/// the marker and the join), and returns the service that can rejoin it.
async fn crashed_after_marker(
    status: crate::agent::terminal::TurnTerminalStatus,
    provider_success: bool,
    terminal_id: &str,
    input: &str,
    kind: AgentTurnKind,
) -> (tempfile::TempDir, Arc<TurnDecisionService>, String, String) {
    let workspace = tempfile::tempdir().expect("test: workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    let generation = crate::config::new_shared(config.clone()).pin();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: memory"));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let service = agent_service_with_memory(&generation, &provider_catalog, Arc::clone(&memory)).await;
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        input,
        "prx_rejoin_receipts_run",
    )
    .await;
    let context = service
        .decide_agent_initial(&proof, kind, &CancellationToken::new())
        .await
        .expect("test: decision");
    let now = chrono::Utc::now();
    let provider_outcome = provider_success.then(|| crate::llm::route_decision::ProviderExecutionOutcome {
        decision_id: context.decision_id().to_string(),
        started_at: now,
        finished_at: now,
        attempts: Vec::new(),
        final_provider: "prx_test".to_owned(),
        final_model: "prx_test".to_owned(),
        status: crate::llm::route_decision::ExecutionStatus::Success,
        fallback_reason: None,
        tokens_used: Default::default(),
        prepared_parameters: Vec::new(),
    });
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy());
    crate::agent::terminal::finalize_turn_stop_after_marker(
        &fabric,
        crate::agent::terminal::TurnTerminalDraft {
            decision: Some(service.settlement_ref(Arc::clone(&context)).expect("test: link")),
            terminal_id: terminal_id.to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status,
            history: None,
            history_scope: None,
            provider_outcome,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "recovered".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &config.cost,
        workspace.path(),
    )
    .await
    .expect("test: marker");
    (
        workspace,
        service,
        proof.owner_id().to_owned(),
        proof.run_id().to_owned(),
    )
}

fn selected_experience(store: &SkillLifecycleStore, run_id: &str) {
    let pin = store.pin_active(&skill_id()).expect("test: pin").expect("test: bound");
    store
        .record_selection(run_id, &pin, &Correlation::default())
        .expect("test: selection");
}

#[tokio::test]
async fn rejoin_without_provider_facts_joins_the_terminal_once_as_unknown() {
    let (workspace, service, owner, run_id) = crashed_after_marker(
        crate::agent::terminal::TurnTerminalStatus::Completed,
        false,
        "rejoin-unknown-1",
        "  ",
        AgentTurnKind::Repl,
    )
    .await;
    let store = managed_store(workspace.path(), &owner);
    selected_experience(&store, &run_id);
    assert!(
        store
            .read_experience(&run_id)
            .expect("test: read")
            .expect("test: record")
            .terminal
            .is_none()
    );

    service
        .rejoin_terminal("rejoin-unknown-1", &owner)
        .await
        .expect("test: rejoin");
    crate::skills::lifecycle::drain_background_for_test("rejoin-unknown-1").await;
    let first = store
        .read_experience(&run_id)
        .expect("test: read")
        .expect("test: record");
    let joined = first
        .terminal
        .clone()
        .expect("the recovered run completes its experience");
    assert_eq!(joined.terminal_receipt_id, "rejoin-unknown-1");
    assert_eq!(first.outcome, ExperienceOutcome::Unknown);
    assert!(
        !first.learning_eligible(),
        "recovery produces no qualifying evidence it cannot verify"
    );

    // A double rejoin keeps the first join.
    service
        .rejoin_terminal("rejoin-unknown-1", &owner)
        .await
        .expect("test: second rejoin");
    crate::skills::lifecycle::drain_background_for_test("rejoin-unknown-1").await;
    let second = store
        .read_experience(&run_id)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(second.terminal, first.terminal);
    assert_eq!(second.outcome, ExperienceOutcome::Unknown);
    assert_eq!(second.late_evidence, 0);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#[tokio::test]
async fn rejoin_maps_a_settled_failure_like_the_live_terminal() {
    let (workspace, service, owner, run_id) = crashed_after_marker(
        crate::agent::terminal::TurnTerminalStatus::Failed,
        false,
        "rejoin-failed-1",
        "prx test routing input",
        AgentTurnKind::SingleShot,
    )
    .await;
    let store = managed_store(workspace.path(), &owner);
    selected_experience(&store, &run_id);
    service
        .rejoin_terminal("rejoin-failed-1", &owner)
        .await
        .expect("test: rejoin");
    crate::skills::lifecycle::drain_background_for_test("rejoin-failed-1").await;
    let experience = store
        .read_experience(&run_id)
        .expect("test: read")
        .expect("test: record");
    assert_eq!(experience.outcome, ExperienceOutcome::Failure);
    assert_eq!(
        experience.terminal.expect("test: joined").terminal_receipt_id,
        "rejoin-failed-1"
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#[tokio::test]
async fn rejoin_never_creates_a_store_or_an_experience() {
    let (workspace, service, owner, run_id) = crashed_after_marker(
        crate::agent::terminal::TurnTerminalStatus::Completed,
        true,
        "rejoin-bare-1",
        "prx test routing input",
        AgentTurnKind::SingleShot,
    )
    .await;
    service
        .rejoin_terminal("rejoin-bare-1", &owner)
        .await
        .expect("test: rejoin");
    crate::skills::lifecycle::drain_background_for_test("rejoin-bare-1").await;
    assert!(
        SkillLifecycleStore::open_existing(workspace.path(), OwnerScope::new(&owner).expect("test: owner"))
            .expect("test: open")
            .is_none_or(|store| store.read_experience(&run_id).expect("test: read").is_none())
    );
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

#![cfg(test)]
//! Private actual-service fixture characterization. No performance acceptance.
//! This prepares and validates semantic fixtures before formal measurements.

#![allow(clippy::expect_used)] // Characterization fails loudly on invalid owned fixtures.

use super::*;
use crate::causal_tree::branch::Ns8;
use crate::causal_tree::candidate_generator::CandidateSet;
use crate::causal_tree::decision_select::Selection;
use crate::tools::traits::ToolCategory;
use serde_json::{Value, json};

// Inventory profiles measure an admitted execution path, not builtin evidence inference.
// Reuse the approval fixture controlled choice, evidence, and utility answers.
struct InventoryEvidenceBackend {
    identity: BackendIdentity,
}

#[async_trait]
impl DecisionBackend for InventoryEvidenceBackend {
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
            // Both profiles explicitly name file_read. Favor that scoped
            // candidate over the unscoped family fallback in controlled answers.
            let work = id.as_str().starts_with("candidate.use_tool-filesystem-");
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        "use_tool"
                    } else {
                        "filesystem"
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

fn install_inventory_evidence_backend(service: &mut Arc<TurnDecisionService>) {
    let owning = Arc::get_mut(service).expect("unshared inventory fixture service");
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(InventoryEvidenceBackend {
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
}

// Preserve the original no-dispatch observation while using the real file tool.
struct CountedInventoryFileRead {
    inner: crate::tools::file_read::FileReadTool,
    dispatches: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountedInventoryFileRead {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        self.inner.immutable_catalog_specs()
    }
    fn catalog_availability_is_configured(&self) -> bool {
        self.inner.catalog_availability_is_configured()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.inner.supports_owner_filesystem()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [ToolCategory] {
        self.inner.categories()
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(args).await
    }
    async fn execute_with_cancellation(
        &self,
        args: Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        self.inner.execute_with_cancellation(args, cancellation).await
    }
}

struct FixtureTool {
    spec: ToolSpec,
    categories: &'static [ToolCategory],
    dispatches: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters_schema(&self) -> Value {
        self.spec.parameters.clone()
    }
    fn categories(&self) -> &'static [ToolCategory] {
        self.categories
    }
    fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
        Some(std::slice::from_ref(&self.spec))
    }
    fn catalog_availability_is_configured(&self) -> bool {
        true
    }
    async fn execute(&self, _: Value) -> anyhow::Result<ToolResult> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("performance fixture dispatch is forbidden")
    }
}

fn tool(name: &str, categories: &'static [ToolCategory], dispatches: &Arc<AtomicUsize>) -> Box<dyn Tool> {
    Box::new(FixtureTool {
        spec: ToolSpec {
            name: name.to_owned(),
            description: "Read neutral project evidence".to_owned(),
            parameters: json!({"type":"object","properties":{}}),
        },
        categories,
        dispatches: Arc::clone(dispatches),
    })
}

async fn drain_required(service: &TurnDecisionService) {
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(
        service
            .store
            .writer()
            .expect("real decision writer")
            .drain(deadline)
            .await
    );
    assert!(
        service
            .durable_events
            .as_ref()
            .expect("real durable causal sink")
            .drain(deadline)
            .await
    );
}

fn candidates(set: &CandidateSet) -> &[crate::causal_tree::branch::CausalCandidate] {
    match set {
        CandidateSet::Legal { candidates, .. } => candidates,
        CandidateSet::NoLegalPath { terminals, .. } => terminals,
    }
}

mod model_binding;
mod semantic_witness;
mod witness_mutations;

fn semantic_context(service: &TurnDecisionService, proof: &AgentTurnProof, context: &TurnDecisionContext) -> Value {
    semantic_witness::capture(service, proof, context)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit isolated actual-service characterization before formal fixture freeze"]
#[allow(clippy::print_stdout, clippy::expect_used, clippy::panic)]
async fn actual_builtin_fixture_characterization() {
    let extra: usize = std::env::var("PRX_CDP_PERF_EXTRA")
        .expect("explicit N")
        .parse()
        .expect("numeric N");
    assert!([0, 1, 16, 64, 256, 512].contains(&extra));
    let class = std::env::var("PRX_CDP_PERF_CASE").expect("explicit fixture class");
    assert!(["small", "large-raw-state", "max-native-cap32"].contains(&class.as_str()));
    let reverse = std::env::var("PRX_CDP_PERF_REVERSE").is_ok_and(|v| v == "1");
    let workspace = tempfile::tempdir().expect("isolated workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5000;
    config.causal_tree.policy.max_decision_hops = 4;
    config.causal_tree.policy.max_candidates = if class == "max-native-cap32" { 32 } else { 12 };
    let profile = std::env::var("PRX_CDP_PERF_PROFILE").expect("explicit recording profile");
    assert!(["default-recording", "retain-projection"].contains(&profile.as_str()));
    config.causal_tree.store.record_redacted_projection = profile == "retain-projection";
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified local model fixture"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut direct = BTreeSet::new();
    let mut tools = Vec::new();
    if class == "max-native-cap32" {
        for (name, categories) in [
            ("file_read", &[ToolCategory::FileSystem][..]),
            ("skills_list", &[][..]),
            ("memory_search", &[ToolCategory::Memory][..]),
            ("web_search_tool", &[ToolCategory::WebBrowsing][..]),
            ("sessions_history", &[ToolCategory::Communication][..]),
        ] {
            direct.insert(name.to_owned());
            tools.push(tool(name, categories, &dispatches));
        }
    } else {
        direct.insert("file_read".to_owned());
        tools.push(tool("file_read", &[], &dispatches));
    }
    let fixed_count = tools.len() + 1;
    tools.push(tool("image_info", &[], &dispatches));
    for n in 0..extra {
        tools.push(tool(&format!("prx_perf_{n:04}"), &[], &dispatches));
    }
    if reverse {
        tools.reverse();
    }
    let registry = Arc::new(tools);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("actual SQLite"));
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                allowed_tools: None,
                direct_names: direct,
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .expect("actual source-owned builtin service");
    let mut input = if class == "max-native-cap32" {
        "inspect file_read skills_list memory_search web_search_tool sessions_history".to_owned()
    } else {
        "inspect file_read project evidence".to_owned()
    };
    if class == "large-raw-state" {
        while input.len() < 16384 {
            input.push_str(" neutral evidence;");
        }
        input.truncate(16384);
    }
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        &input,
        "prx_perf_run",
    )
    .await;
    drain_required(&service).await;
    let cancellation = CancellationToken::new();
    let process_pilot = std::env::var("PRX_CDP_PROCESS_PILOT").is_ok_and(|v| v == "1");
    // Observer-on diagnostics are a separate run: never enter the allocation
    // gate with capture copies, and never label reconstructed values original.
    let observe_original = std::env::var("PRX_CDP_ORIGINAL_OBSERVATION").is_ok_and(|v| v == "1");
    assert!(
        !(observe_original && process_pilot),
        "original observation must remain outside measured allocation windows"
    );
    let mut original_capture = None;
    let mut measured_material = None;
    let (mut context, process_measurement) = if process_pilot {
        let ((context, return_us, drained_us), allocations) =
            crate::decision::builtin::allocation_counter::count_process_future(async {
                let started = Instant::now();
                let (context, captured) = crate::causal_tree::decision_engine::original_observation::material::capture(
                    "same-invocation-pilot",
                    service.decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation),
                )
                .await
                .expect("bounded same-invocation pilot material");
                let context = context.expect("actual builtin decision");
                let return_us = started.elapsed().as_micros();
                drain_required(&service).await;
                let hops = context.audit_hops().collect::<Vec<_>>();
                measured_material = Some(
                    captured
                        .finish_material(&hops, false)
                        .expect("complete original pilot material inside measured window"),
                );
                drop(hops);
                (context, return_us, started.elapsed().as_micros())
            })
            .await
            .expect("isolated process allocation window");
        assert!(
            drained_us >= return_us,
            "required drains cannot finish before decision return"
        );
        (context, Some((allocations, return_us, drained_us)))
    } else if observe_original {
        let (result, captured) = crate::causal_tree::decision_engine::original_observation::capture(
            "actual-neutral-service-characterization",
            service.decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation),
        )
        .await
        .expect("bounded original-object observation");
        let context = result.expect("actual observed builtin decision");
        drain_required(&service).await;
        original_capture = Some(captured);
        (context, None)
    } else {
        let (context, captured) = crate::causal_tree::decision_engine::original_observation::material::capture(
            "same-invocation-counter-off",
            service.decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation),
        )
        .await
        .expect("bounded same-invocation counter-off material");
        let context = context.expect("actual builtin decision");
        drain_required(&service).await;
        let hops = context.audit_hops().collect::<Vec<_>>();
        measured_material = Some(
            captured
                .finish_material(&hops, false)
                .expect("complete original counter-off material"),
        );
        drop(hops);
        (context, None)
    };
    if let Some(material) = measured_material {
        println!(
            "PRX_ORIGINAL_MATERIAL {}",
            std::str::from_utf8(&material).expect("encoded material JSON")
        );
    }
    if let Some(captured) = original_capture {
        let actual_hops = context.audit_hops().collect::<Vec<_>>();
        let sidecar = captured
            .checked_sidecar(&actual_hops)
            .expect("exact original service-hop inventory and scores");
        assert!(
            captured.checked_sidecar(&[]).is_err(),
            "empty service inventory must fail closed"
        );
        let mut duplicated = actual_hops.clone();
        duplicated.push(*actual_hops.first().expect("actual original hop"));
        assert!(
            captured.checked_sidecar(&duplicated).is_err(),
            "duplicated service inventory must fail closed"
        );
        println!(
            "PRX_CDP_ORIGINAL_OBSERVATION {}",
            json!({
                "scope":"observer-on actual service original objects only; no physical HTTP wire or performance acceptance",
                "class":class,"recording_profile":profile,"extra_inventory":extra,"reverse_registry":reverse,
                "workspace_id":proof.workspace_id(),"owner_id":proof.owner_id(),"session_key":proof.session_key(),
                "run_id":proof.run_id(),"source_event_id":proof.source_event_id(),"observation":sidecar
            })
        );
    }
    let semantics = semantic_context(&service, &proof, &context);
    if std::env::var("PRX_CDP_WITNESS_RED").is_ok_and(|v| v == "blocked-abstain") {
        use crate::causal_tree::gates::GateOutcome;
        let owned = Arc::get_mut(&mut context).expect("unique owned mutation context");
        let selection = &owned.engine.selection;
        let Selection::Abstain { action, .. } = selection else {
            panic!("expected actual terminal");
        };
        let selected = crate::causal_tree::branch::CandidateId::new(
            *action,
            crate::causal_tree::branch::CapabilityTarget::None,
            None,
        );
        let previous = owned
            .engine
            .pre_commit
            .outcomes
            .insert(
                selected.clone(),
                GateOutcome::Blocked {
                    gate: 2,
                    reason_code: "fixture_blocked",
                },
            )
            .expect("actual final gate");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| semantic_context(
                &service, &proof, &context
            )))
            .is_err(),
            "a blocked selected Abstain terminal must fail the semantic witness"
        );
        Arc::get_mut(&mut context)
            .expect("unique context after witness")
            .engine
            .pre_commit
            .outcomes
            .insert(selected, previous);
        assert_eq!(semantic_context(&service, &proof, &context), semantics);
    }

    if std::env::var("PRX_CDP_WITNESS_MUTATIONS").is_ok_and(|v| v == "1") {
        assert_eq!(witness_mutations::run(&service, &proof, &mut context), 9);
    }
    let hops: Vec<_> = context.audit_hops().collect();
    let expected_counts: &[(usize, usize)] = match class.as_str() {
        "small" => &[(6, 4), (3, 0)],
        "large-raw-state" => &[(5, 3)],
        "max-native-cap32" => &[(16, 14), (3, 0)],
        _ => panic!("unsupported class"),
    };
    assert_eq!(hops.len(), expected_counts.len());
    for (hop, (candidate_count, scored_count)) in hops.iter().zip(expected_counts) {
        assert_eq!(candidates(&hop.candidates).len(), *candidate_count);
        assert_eq!(hop.scored_count, *scored_count);
    }
    if class == "max-native-cap32" {
        use crate::causal_tree::branch::{CapabilityTarget, CausalActionKind};
        let first = hops.first().expect("first full hop");
        for (family, public_name) in [
            (CapabilityTarget::Memory, "memory_search"),
            (CapabilityTarget::Session, "sessions_history"),
        ] {
            let mut matching = candidates(&first.candidates)
                .iter()
                .filter(|candidate| candidate.action() == CausalActionKind::Retrieve && candidate.target() == family);
            let candidate = matching.next().expect("exposed bounded retrieval family");
            assert!(matching.next().is_none(), "one retrieval candidate per family");
            let entry = service
                .catalog
                .entry_for_public_name(public_name)
                .expect("actual catalog entry");
            let [reference] = candidate.required_capabilities() else {
                panic!("retrieval candidate must bind exactly one actual capability");
            };
            assert_eq!(reference.id, entry.id);
            assert_eq!(reference.fingerprint, entry.fingerprint);
            assert_eq!(reference.generation, entry.generation);
            assert_eq!(entry.family, family);
        }
    }
    match (&hops.first().expect("first full hop").selection, class.as_str()) {
        (
            Selection::Recover {
                candidate_id,
                reason_code,
            },
            "small",
        ) => {
            assert_eq!(
                candidate_id.action(),
                crate::causal_tree::branch::CausalActionKind::Discover
            );
            assert_eq!(*reason_code, "discover_capability");
        }
        (
            Selection::Recover {
                candidate_id,
                reason_code,
            },
            "max-native-cap32",
        ) => {
            assert_eq!(
                candidate_id.action(),
                crate::causal_tree::branch::CausalActionKind::Retrieve
            );
            assert_eq!(*reason_code, "retrieve_evidence");
        }
        (Selection::Abstain { action, reason_code }, "large-raw-state") => {
            assert_eq!(*action, crate::causal_tree::branch::CausalActionKind::Clarify);
            assert_eq!(*reason_code, "insufficient_evidence");
        }
        _ => panic!("unexpected actual builtin fixture path"),
    }
    if class == "large-raw-state" {
        assert!(
            hops.iter()
                .all(|hop| candidates(&hop.candidates).iter().all(|c| !matches!(
                    c.action(),
                    crate::causal_tree::branch::CausalActionKind::Retrieve
                        | crate::causal_tree::branch::CausalActionKind::Discover
                )))
        );
    }
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    println!(
        "PRX_CDP_CHARACTERIZATION {}",
        json!({"kind":"actual-builtin-fixture-characterization-v2",
        "scope":"semantic characterization only; no allocation/latency/p99 acceptance","class":class,
        "extra_inventory":extra,"fixed_capabilities":fixed_count,"reverse_registry":reverse,
        "recording_profile":profile,"raw_input_bytes":input.len(),"configured_candidate_cap":config.causal_tree.policy.max_candidates,
        "configured_hop_advances":config.causal_tree.policy.max_decision_hops,"semantics":semantics,
        "process_pilot":process_measurement.map(|(allocations,return_us,drained_us)|json!({"rust_allocation_attempts":allocations,"decide_return_us":return_us,"decide_plus_required_drains_us":drained_us,"scope":"debug first decision after initialized/drained setup; all-thread Rust attempts only; no formal A0 or P0 acceptance"}))})
    );
}

mod phases;

mod model_allowlist;

mod mcp_inventory;

#![cfg(test)]
use super::*;

#[test]
fn selected_session_projection_keeps_kind_and_distrusts_event_identity() {
    let now = chrono::Utc::now();
    let hit = crate::memory::traits::DecisionSessionHit {
        event_id: "untrusted-row-event".to_owned(),
        role: "assistant".to_owned(),
        content: "prior local response".to_owned(),
        created_at: now.to_rfc3339(),
    };
    let artifacts = session_artifacts_from_selected_read(std::slice::from_ref(&hit), now);
    assert_eq!(artifacts.len(), 1);
    let artifact = artifacts.first().expect("selected session artifact");
    assert_eq!(artifact.kind, ObservedArtifactKind::Session);
    assert_eq!(artifact.summary, "assistant: prior local response");
    assert!(artifact.source_event_id.is_none());
    let mut invalid = hit.clone();
    invalid.role = "system".to_owned();
    assert!(
        session_artifacts_from_selected_read(&[invalid], now).is_empty(),
        "expected an empty value"
    );
    let mut oversized = hit;
    oversized.content = "x".repeat(4097);
    assert!(
        session_artifacts_from_selected_read(&[oversized], now).is_empty(),
        "expected an empty value"
    );
}

#[test]
fn selected_memory_artifacts_redact_before_projection_cut_and_distrust_row_metadata() {
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 9, 29, 12, 0, 0)
        .single()
        .expect("fixed time");
    let hit = |content: &str, created_at: String, source_event_id: Option<&str>| DecisionMemoryHit {
        key: "prx_memory_key".into(),
        category: MemoryCategory::Core,
        session_id: None,
        id: "prx_memory_hit".into(),
        content: content.into(),
        created_at,
        source_event_id: source_event_id.map(str::to_string),
    };
    let hits = vec![
        hit("  ", now.to_rfc3339(), None),
        hit(
            "local evidence",
            (now - chrono::Duration::seconds(2)).to_rfc3339(),
            Some("spoofed-event"),
        ),
        hit(
            "future evidence",
            (now + chrono::Duration::seconds(1)).to_rfc3339(),
            None,
        ),
        hit("invalid time", "invalid timestamp".into(), None),
        hit(&"é".repeat(200), now.to_rfc3339(), None),
        hit("sixth result", now.to_rfc3339(), None),
    ];
    let artifacts = memory_artifacts_from_selected_read(&hits, now);
    let [first, future, malformed, unicode] = artifacts.as_slice() else {
        panic!("expected four bounded artifacts");
    };
    assert_eq!(first.kind, ObservedArtifactKind::Memory);
    assert_eq!(first.age_ms, 2_000);
    assert_eq!(first.summary, "local evidence");
    assert_eq!(
        first.source_event_id, None,
        "row metadata is not an authenticated receipt"
    );
    assert!(future.age_ms > STALE_ARTIFACT_MS, "future timestamps are stale");
    assert!(malformed.age_ms > STALE_ARTIFACT_MS, "malformed timestamps are stale");
    assert_eq!(unicode.summary.chars().count(), 200);
    assert!(!unicode.summary_was_truncated);
    assert!(!artifacts.iter().any(|artifact| artifact.summary == "sixth result"));
    assert!(
        memory_artifacts_from_selected_read(&[hit(&"x".repeat(4_097), now.to_rfc3339(), None)], now).is_empty(),
        "expected an empty value"
    );
    assert!(
        !format!("{artifacts:?}").contains("local evidence"),
        "Debug redacts raw content"
    );

    let secret = "enc2:123456789abcdef";
    let straddling = format!("{}{}", "a ".repeat(124), secret);
    let evidence = memory_artifacts_from_selected_read(&[hit(&straddling, now.to_rfc3339(), None)], now);
    let artifact = evidence.first().expect("one straddling artifact");
    assert_eq!(artifact.summary.chars().count(), 256);
    assert!(artifact.summary_was_redacted);
    assert!(artifact.summary_was_truncated);
    let key = crate::causal_tree::state::fixtures::key();
    let mut turn = crate::causal_tree::state::fixtures::state(&key, "retrieve evidence");
    turn.observed = ObservedState::from_runtime_records(Vec::new(), evidence, 0);
    let (projection, manifest) =
        crate::causal_tree::projection::redacted_projection(&turn).expect("redacted projection");
    let rendered = projection.as_json().to_string();
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("enc2:123"), "a truncated token prefix must not leak");
    assert_eq!(manifest.redacted_texts, 1, "one source artifact was redacted");
    assert_eq!(manifest.truncated, vec!["artifacts.summary"]);
}

pub(super) struct CountingProbeMemory {
    pub(super) calls: AtomicUsize,
    pub(super) retrieval_calls: AtomicUsize,
    pub(super) return_empty: AtomicBool,
    pub(super) return_error: AtomicBool,
    pub(super) return_never: AtomicBool,
    pub(super) return_stale: AtomicBool,
    pub(super) unique_results: AtomicBool,
    pub(super) drift_on_read: Option<Arc<AtomicBool>>,
    pub(super) read_entered: Arc<tokio::sync::Notify>,
    pub(super) retrieval_args: parking_lot::Mutex<Option<(String, u8, MemoryWriteContext, Instant)>>,
}

#[async_trait]
impl Memory for CountingProbeMemory {
    fn name(&self) -> &str {
        "prx_probe_fixture"
    }

    async fn store(&self, _: &str, _: &str, _: MemoryCategory, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(&self, _: &str, _: usize, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(&self, _: Option<&MemoryCategory>, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }

    async fn count_rehearsal_hits(&self, _: &str, _: u8, _: &MemoryWriteContext, _: Instant) -> anyhow::Result<u32> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        let call = self.retrieval_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(drift) = &self.drift_on_read {
            drift.store(true, Ordering::SeqCst);
        }
        *self.retrieval_args.lock() = Some((query.to_string(), limit, context.clone(), deadline));
        self.read_entered.notify_one();
        if self.return_never.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        if self.return_error.load(Ordering::SeqCst) {
            anyhow::bail!("prx fixture read failure");
        }
        if self.return_empty.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        Ok(vec![DecisionMemoryHit {
            key: "prx_memory_key".into(),
            category: MemoryCategory::Core,
            session_id: session_id.map(str::to_owned),
            id: "prx_memory_hit".into(),
            content: if self.unique_results.load(Ordering::SeqCst) {
                format!("prx scoped memory evidence {call}")
            } else {
                "prx scoped memory evidence".into()
            },
            created_at: if self.return_stale.load(Ordering::SeqCst) {
                (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339()
            } else {
                chrono::Utc::now().to_rfc3339()
            },
            source_event_id: None,
        }])
    }
}

pub(super) struct RetrieveProbeBackend {
    pub(super) identity: BackendIdentity,
    pub(super) requests: Arc<parking_lot::Mutex<Vec<(String, Option<String>)>>>,
    pub(super) stall: bool,
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) drift_after_response: Option<Arc<AtomicBool>>,
}

#[async_trait]
impl DecisionBackend for RetrieveProbeBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }

    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }

    // Explicit accounting-entry support for this deterministic in-process
    // fixture. No physical send, receipt or fabricated charge is produced.
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
        self.requests.lock().push((
            request.manifest().state_fingerprint().to_string(),
            request
                .state()
                .as_json()
                .get("prior_decision_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        ));
        if self.stall {
            self.entered.notify_one();
            return std::future::pending().await;
        }
        if let Some(drift) = &self.drift_after_response {
            drift.store(true, Ordering::SeqCst);
        }
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".into();
        response.resolved_model = self.identity.configured_model.to_string();
        response.answers.insert(
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionAnswer::Noul(NoulAnswer {
                probability_true: Probability::new(0.01)?,
            }),
        );
        if let std::collections::btree_map::Entry::Occupied(mut answer) = response
            .answers
            .entry(QuestionId::global(GlobalQuestion::NeedsFreshState))
        {
            answer.insert(DecisionAnswer::Noul(NoulAnswer {
                probability_true: Probability::new(0.99)?,
            }));
        }
        Ok(response)
    }
}

pub(super) struct ExecuteContextProbeBackend {
    pub(super) identity: BackendIdentity,
    pub(super) calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DecisionBackend for ExecuteContextProbeBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
    // This in-process probe performs no provider I/O or synthetic charge.
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
            let work = id.as_str().starts_with("candidate.use_tool-native_tool.")
                || id.as_str().starts_with("candidate.use_tool-native_tool-");
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        "use_tool"
                    } else {
                        "native_tool"
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
async fn selected_execute_retains_exact_gated_refs_without_dispatch() {
    struct ReadFixtureTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for ReadFixtureTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Read neutral project evidence"
        }
        fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
            if self.name == "web_search_tool" {
                &[crate::tools::traits::ToolCategory::WebBrowsing]
            } else {
                &[]
            }
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("context construction must not dispatch")
        }
    }
    for (enabled, keyless) in [(false, false), (true, false), (true, true)] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = enabled;
        if keyless {
            let directory = workspace.path().join("self/causal_tree");
            std::fs::create_dir_all(&directory).expect("fixture key directory");
            std::fs::write(directory.join(".decision_key"), b"short").expect("unavailable fixture key");
        }
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified main model"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(ReadFixtureTool {
                name: "file_read",
                calls: Arc::clone(&dispatches),
            }),
            Box::new(ReadFixtureTool {
                name: "web_search_tool",
                calls: Arc::clone(&dispatches),
            }),
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
        let memory: Arc<dyn Memory> = if enabled {
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"))
        } else {
            Arc::new(NoneMemory::new())
        };
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy {
                    allowed_tools: None,
                    direct_names: BTreeSet::from(["file_read".to_owned(), "web_search_tool".to_owned()]),
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
        .expect("source-owned fixture service");
        let predictions = Arc::new(AtomicUsize::new(0));
        let owning = Arc::get_mut(&mut service).expect("unshared fixture service");
        // Fixture-only deterministic probabilities. This does not establish
        // configured transport or budget admission through the constructor.
        owning.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(ExecuteContextProbeBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                calls: Arc::clone(&predictions),
            })],
            Arc::clone(&owning.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let proof = if enabled {
            crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                &generation,
                Arc::clone(&memory),
                "Use file_read for the project evidence",
                "prx_execute_context_run",
            )
            .await
        } else {
            crate::agent::entry::test_cli_agent_turn_proof_with_input(
                &generation,
                "ollama",
                "prx-local-model",
                "Use file_read for the project evidence",
            )
        };
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancelled)
                .await,
            Err(AgentDecisionError::Cancelled)
        ));
        assert_eq!(predictions.load(Ordering::SeqCst), 0);
        let context = service
            .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
            .await
            .expect("gated service result");
        assert!(
            context.audit_hops().any(|hop| matches!(&hop.selection,
                crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
                    if candidate_id.action() == crate::causal_tree::branch::CausalActionKind::UseTool
            )),
            "fixture must actually select gated work before checking preservation"
        );
        assert!(
            matches!(context.directive(), TurnExecutionDirective::Execute { .. }),
            "selected Execute must be retained"
        );
        assert_eq!(context.hop_ids().len(), 1, "no additional terminal hop");
        assert_eq!(
            context.terminal_outcome_action(),
            None,
            "selected work has not completed"
        );
        let view = service.catalog.view(&CatalogScope::default());
        let entry = view
            .iter_entries()
            .find(|entry| entry.public_name == "file_read")
            .expect("selected exact capability");
        assert_eq!(
            context.execution_refs(),
            &BTreeSet::from([CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation
            }])
        );
        assert!(
            matches!(&context.engine().candidates,
                crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. }
                    if candidates.iter().any(|candidate| candidate.target() == CapabilityTarget::Web)
            ),
            "another offered legal family must not enter selected execution refs"
        );
        assert_eq!(context.record_admitted.load(Ordering::Acquire), enabled && !keyless);
        if enabled && !keyless {
            let writer = service.store.writer().expect("recorded work writer");
            assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
            let scope = DecisionScope::Owner {
                workspace_id: service.store.workspace_id().expect("store workspace"),
                owner_id: Arc::from(proof.owner_id()),
            };
            let row = service
                .store
                .reader()
                .get(&scope, &context.decision_id())
                .expect("work record")
                .expect("one selected work record");
            assert!(row.enabled_snapshot);
            assert_eq!(row.directive_kind, "use_tool");
            assert!(
                row.outcomes.is_empty(),
                "context construction must not settle selected work"
            );
            assert_eq!(
                row.selected_candidate_id.as_deref(),
                match &context.engine().selection {
                    crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } =>
                        Some(candidate_id.as_str()),
                    _ => None,
                }
            );
        }
        assert!(matches!(context.disposition(), Ok(TurnDisposition::Run)));
        let ports = ReceiptClosePorts {
            receipt: None,
            calls: AtomicUsize::new(0),
            cancellation: None,
        };
        assert!(
            service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(ports.calls.load(Ordering::SeqCst), 0);
        assert_eq!(predictions.load(Ordering::SeqCst), 1);
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        let mut expired = context;
        assert_execute_reference_contract_rejects_changes(
            Arc::get_mut(&mut expired).expect("unshared contract fixture"),
            &service.catalog,
        );
        Arc::get_mut(&mut expired)
            .expect("unshared returned context")
            .engine
            .deadline = Instant::now();
        assert!(matches!(expired.disposition(), Ok(TurnDisposition::Run)));
        let _ = expired.terminal_execution_trace();
        assert!(matches!(expired.disposition(), Err(DirectiveError::NotAdmitted)));
    }
}

// Conversion mutations below are private contract fixtures. They do not
// establish new service admission or authorize a multi-capability action.
fn assert_execute_reference_contract_rejects_changes(context: &mut TurnDecisionContext, catalog: &CapabilityCatalog) {
    use crate::causal_tree::branch::{CandidateSpec, CausalCandidate};
    use crate::causal_tree::candidate_generator::CandidateSet;
    use crate::causal_tree::gates::GateOutcome;
    let view = catalog.view(&CatalogScope::default());
    let original_refs: Vec<_> = context.execution_refs().iter().cloned().collect();
    let extra = view
        .iter_entries()
        .find(|entry| entry.public_name == "web_search_tool")
        .expect("offered extra capability");
    let extra_ref = CapabilityRef {
        id: extra.id.clone(),
        fingerprint: extra.fingerprint,
        generation: extra.generation,
    };
    let original = match &context.engine.candidates {
        CandidateSet::Legal { candidates, .. } => candidates
            .iter()
            .find(|candidate| match &context.engine.selection {
                crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } => {
                    candidate.candidate_id() == candidate_id
                }
                _ => false,
            })
            .expect("selected fixture candidate")
            .clone(),
        _ => panic!("legal fixture candidates"),
    };
    let id = original.candidate_id().clone();
    let expected_ids: Vec<_> = original_refs.iter().map(|reference| reference.id.clone()).collect();
    for ids in [
        Vec::new(),
        [expected_ids.clone(), expected_ids.clone()].concat(),
        [expected_ids, vec![extra.id.clone()]].concat(),
    ] {
        assert!(
            selected_execute_refs(
                &context.engine,
                &TurnExecutionDirective::Execute {
                    allowed_capability_ids: ids
                },
                &view
            )
            .is_err()
        );
    }
    assert!(selected_execute_refs(&context.engine, &TurnExecutionDirective::AnswerDirectly, &view).is_err());
    let blocked = GateOutcome::Blocked {
        gate: 1,
        reason_code: "fixture_blocked",
    };
    let previous = context
        .engine
        .pre_commit
        .outcomes
        .insert(id.clone(), blocked.clone())
        .expect("original final gate");
    assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
    context.engine.pre_commit.outcomes.insert(id.clone(), previous);
    let CandidateSet::Legal { report, .. } = &mut context.engine.candidates else {
        panic!("legal fixture");
    };
    let previous = report
        .outcomes
        .insert(id.clone(), blocked)
        .expect("original admitted gate");
    assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
    let CandidateSet::Legal { report, .. } = &mut context.engine.candidates else {
        panic!("legal fixture");
    };
    report.outcomes.insert(id.clone(), previous);
    let mut stale_fingerprint = original_refs.first().expect("one ref").clone();
    *stale_fingerprint.fingerprint.0.first_mut().expect("fingerprint byte") ^= 1;
    let mut stale_generation = original_refs.first().expect("one ref").clone();
    *stale_generation.generation.0.first_mut().expect("generation byte") ^= 1;
    for refs in [
        Vec::new(),
        [original_refs.clone(), original_refs.clone()].concat(),
        vec![stale_fingerprint],
        vec![stale_generation],
    ] {
        let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        let selected = candidates
            .iter_mut()
            .find(|candidate| candidate.candidate_id() == &id)
            .expect("selected fixture");
        *selected = CausalCandidate::new(
            original.action(),
            original.target(),
            CandidateSpec {
                namespace: id.namespace(),
                required_capabilities: refs,
                side_effect: original.side_effect(),
                approval: original.approval().clone(),
                estimated_cost: original.estimated_cost(),
                estimated_latency_ms: original.estimated_latency_ms(),
            },
        );
        assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
    }
    let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
        panic!("legal fixture");
    };
    *candidates
        .iter_mut()
        .find(|candidate| candidate.candidate_id() == &id)
        .expect("selected fixture") = original.clone();
    let excluded = catalog.view(&CatalogScope {
        families: BTreeSet::from([CapabilityTarget::Web]),
        channel: false,
    });
    assert!(selected_execute_refs(&context.engine, &context.directive, &excluded).is_err());
    // The converter must preserve all selected refs, including a multi-ref
    // synthetic candidate. This is conversion coverage only.
    let refs = [original_refs.clone(), vec![extra_ref]].concat();
    let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
        panic!("legal fixture");
    };
    *candidates
        .iter_mut()
        .find(|candidate| candidate.candidate_id() == &id)
        .expect("selected fixture") = CausalCandidate::new(
        original.action(),
        original.target(),
        CandidateSpec {
            namespace: id.namespace(),
            required_capabilities: refs.clone(),
            side_effect: original.side_effect(),
            approval: original.approval().clone(),
            estimated_cost: original.estimated_cost(),
            estimated_latency_ms: original.estimated_latency_ms(),
        },
    );
    let directive = directive_from_selection(
        &context.engine,
        &DirectiveContext {
            current_generation: view.generation(),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        },
    )
    .expect("synthetic multi-ref conversion");
    assert_eq!(
        selected_execute_refs(&context.engine, &directive, &view).expect("exact multi-ref metadata"),
        refs.into_iter().collect()
    );
    let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
        panic!("legal fixture");
    };
    *candidates
        .iter_mut()
        .find(|candidate| candidate.candidate_id() == &id)
        .expect("selected fixture") = original;
    assert_eq!(
        selected_execute_refs(&context.engine, &context.directive, &view).expect("restored real selection"),
        original_refs.into_iter().collect()
    );
}

pub(super) struct UsageRecordProbeBackend {
    pub(super) identity: BackendIdentity,
    pub(super) request_ids: Arc<parking_lot::Mutex<Vec<String>>>,
    pub(super) stall: bool,
    pub(super) entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl DecisionBackend for UsageRecordProbeBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }

    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }

    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }

    // Deterministic in-process protocol fixture, like RetrieveProbeBackend.
    // Accept the accounted entry explicitly without pretending a physical
    // send, reservation or vendor charge occurred. Production transports
    // retain the default refusal unless they implement real accounting.
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
        self.request_ids.lock().push(request.manifest().request_id().to_owned());
        if self.stall {
            self.entered.notify_one();
            return std::future::pending().await;
        }
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".to_owned();
        response.resolved_model = self.identity.configured_model.to_string();
        response.usage = crate::decision::protocol::DecisionUsage {
            input_tokens: Some(17),
            output_tokens: Some(3),
            cost_micro_usd: Some(42),
        };
        response.provider_request_id = Some("prx_provider_request_1".to_owned());
        Ok(response)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeCancelMode {
    Never,
    Before,
    Backend,
    Read,
    CatalogDriftRead,
}

pub(super) async fn run_retrieve_probe_case(
    unique_results: bool,
    mode: ProbeCancelMode,
) -> (
    usize,
    usize,
    Vec<(String, Option<String>)>,
    Vec<CausalEvent>,
    Result<Arc<TurnDecisionContext>, AgentDecisionError>,
) {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    // This fixture observes an in-process probe backend, not a durable
    // event-store adapter. Its source proof and phase assertions are off-mode.
    config.causal_tree.enabled = false;
    config.causal_tree.policy.max_decision_hops = 3;
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified test model declaration"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let drift = Arc::new(AtomicBool::new(false));
    let memory = Arc::new(CountingProbeMemory {
        calls: AtomicUsize::new(0),
        retrieval_calls: AtomicUsize::new(0),
        return_empty: AtomicBool::new(false),
        return_error: AtomicBool::new(false),
        return_never: AtomicBool::new(mode == ProbeCancelMode::Read),
        return_stale: AtomicBool::new(false),
        unique_results: AtomicBool::new(unique_results),
        drift_on_read: (mode == ProbeCancelMode::CatalogDriftRead).then(|| Arc::clone(&drift)),
        read_entered: Arc::new(tokio::sync::Notify::new()),
        retrieval_args: parking_lot::Mutex::new(None),
    });
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
        workspace.path().to_path_buf(),
        Arc::clone(&memory) as Arc<dyn Memory>,
        false,
    ))];
    if mode == ProbeCancelMode::CatalogDriftRead {
        tools.push(Box::new(DriftingCatalogTool {
            changed: Arc::clone(&drift),
            dispatches: Arc::new(AtomicUsize::new(0)),
        }));
    }
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let events = Arc::new(RecordingSink::default());
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
            memory: Arc::clone(&memory) as Arc<dyn Memory>,
            security,
        },
    )
    .await
    .expect("test service");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let backend_entered = Arc::new(tokio::sync::Notify::new());
    let service_mut = Arc::get_mut(&mut service).expect("unshared test service");
    service_mut.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(RetrieveProbeBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
            requests: Arc::clone(&requests),
            stall: mode == ProbeCancelMode::Backend,
            entered: Arc::clone(&backend_entered),
            drift_after_response: None,
        })],
        Arc::clone(&service_mut.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_cli_agent_turn_proof_with_run(
        &generation,
        "ollama",
        "prx-local-model",
        "find the scoped memory evidence",
        "prx_run_retrieve",
    );
    let cancellation = CancellationToken::new();
    if mode == ProbeCancelMode::Before {
        cancellation.cancel();
    }
    let decision = service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation);
    let context = match mode {
        ProbeCancelMode::Never | ProbeCancelMode::Before | ProbeCancelMode::CatalogDriftRead => decision.await,
        ProbeCancelMode::Backend | ProbeCancelMode::Read => {
            let entered = if mode == ProbeCancelMode::Backend {
                backend_entered
            } else {
                Arc::clone(&memory.read_entered)
            };
            let cancel_when_entered = async {
                entered.notified().await;
                cancellation.cancel();
            };
            let watched = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(cancel_when_entered, decision)
            })
            .await;
            match watched {
                Ok(((), context)) => context,
                Err(_) => {
                    cancellation.cancel();
                    panic!("probe entry/cancellation watchdog expired before both branches completed");
                }
            }
        }
    };
    let calls = memory.retrieval_calls.load(Ordering::SeqCst);
    let transport_calls = transports.0.load(Ordering::SeqCst);
    let requests = requests.lock().clone();
    let emitted = events.events.lock().clone();
    (calls, transport_calls, requests, emitted, context)
}

pub(super) async fn run_retrieve_probe(
    unique_results: bool,
) -> (usize, usize, Vec<(String, Option<String>)>, Arc<TurnDecisionContext>) {
    let (reads, transports, requests, _, context) =
        run_retrieve_probe_case(unique_results, ProbeCancelMode::Never).await;
    (reads, transports, requests, context.expect("bounded selected Retrieve"))
}

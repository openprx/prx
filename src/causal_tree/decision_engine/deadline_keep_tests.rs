//! Real selection, logical deadlines and production decision-record regressions.
#![cfg(test)]

use super::*;
use crate::causal_tree::branch::{CapabilityTarget, CostEstimate};
use crate::causal_tree::candidate_generator::CapabilityCostModel;
use crate::causal_tree::outcome::DecisionRecordBuilder;
use crate::causal_tree::state::{ObservedState, ObservedStepStatus, TrustedState, fixtures};
use crate::decision::factory::{DecisionBackendFactory, SlotLimits};
use crate::decision::outbound::OutboundSecrets;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};

fn fixture_key() -> Arc<DecisionKey> {
    static KEY: std::sync::OnceLock<Arc<DecisionKey>> = std::sync::OnceLock::new();
    Arc::clone(KEY.get_or_init(fixtures::key))
}

/// A validated optimistic prediction makes the first score exceed the commit
/// bar. The interrupted ranking must still account for unseen competitors.
#[derive(Clone)]
struct ScoringPrediction<P> {
    inner: P,
    optimistic: bool,
}
impl<P: PredictionPort> prediction_sealed::Producer for ScoringPrediction<P> {}
impl<P: PredictionPort> PredictionPort for ScoringPrediction<P> {
    fn remaining_budget(&self) -> Option<(u64, u64)> {
        self.inner.remaining_budget()
    }
    fn terminal_fallback_cause(&self, result: &PredictionResult) -> Option<FallbackCause> {
        self.inner.terminal_fallback_cause(result)
    }
    async fn predict<'a>(
        &'a self,
        request: &'a crate::decision::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> Result<PredictionResult, DecisionError> {
        let mut result = Box::pin(self.inner.predict(request, lineage)).await?;
        if self.optimistic {
            use crate::decision::{ChoiceAnswer, DecisionQuestion, NoulAnswer, Probability, ScoreAnswer};
            let probability = |value| Probability::new(value).unwrap();
            let mut response = crate::decision::builtin::BuiltinDecisionBackend::answer(request)?;
            for (id, question) in request.questions() {
                let answer = match question {
                    DecisionQuestion::Choice { criteria, .. } => {
                        let preferred = if id.as_str() == "next_action" {
                            "use_tool"
                        } else {
                            "filesystem"
                        };
                        let chosen = criteria
                            .keys()
                            .find(|option| option.as_str() == preferred)
                            .cloned()
                            .unwrap_or_else(|| criteria.keys().next().unwrap().clone());
                        DecisionAnswer::Choice(ChoiceAnswer {
                            probabilities: criteria
                                .keys()
                                .map(|option| (option.clone(), probability(f64::from(*option == chosen))))
                                .collect(),
                            chosen,
                            confidence: probability(1.0),
                        })
                    }
                    DecisionQuestion::Noul { .. } => DecisionAnswer::Noul(NoulAnswer {
                        probability_true: probability(if id.as_str() == "needs_fresh_state" { 0.0 } else { 1.0 }),
                    }),
                    DecisionQuestion::Score { levels, .. } => {
                        let index = if id.as_str().ends_with("risk") {
                            0
                        } else {
                            levels.len() - 1
                        };
                        DecisionAnswer::Score(ScoreAnswer {
                            distribution: (0..levels.len()).map(|n| probability(f64::from(n == index))).collect(),
                            expected: f64::from(u32::try_from(index).unwrap()),
                            legend: levels.clone(),
                            confidence: probability(1.0),
                        })
                    }
                };
                response.answers.insert(id.clone(), answer);
            }
            result.served = crate::decision::DecisionResponseValidator::validate(
                request,
                response,
                &crate::decision::ValidationContext {
                    received_at: Instant::now(),
                    max_response_bytes: 256 * 1024,
                    version_policy: crate::decision::ModelVersionPolicy::ExactPinned(Arc::from(
                        crate::decision::builtin::weights::MODEL_VERSION,
                    )),
                    local_backend: crate::decision::builtin::BuiltinDecisionBackend::identity(),
                },
            )?;
        }
        Ok(result)
    }
}

async fn decide_turn(
    turn: &str,
    checkpoint: Option<(&'static str, usize)>,
    with_tools: bool,
    prior: Option<(ObservedStepStatus, CandidateId)>,
) -> EngineDecision {
    let key = fixture_key();
    let mut state = fixtures::state(&key, turn);
    state.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
    let mut security = SecurityPolicy::default();
    security.autonomy = AutonomyLevel::Full;
    let registry: Vec<Box<dyn crate::tools::Tool>> = if with_tools {
        vec![
            Box::new(crate::tools::file_read::FileReadTool::new(
                Arc::new(security.clone()),
                false,
            )),
            Box::new(crate::tools::file_write::FileWriteTool::new(Arc::new(security.clone()))),
            Box::new(crate::tools::shell::ShellTool::new(
                Arc::new(crate::runtime::native::NativeRuntime::new()),
                std::env::temp_dir(),
            )),
        ]
    } else {
        Vec::new()
    };
    let surface = SurfacePolicy {
        direct_names: BTreeSet::from(["file_read".to_owned(), "file_write".to_owned(), "shell".to_owned()]),
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.owner_id = Some(Arc::from("prx_user"));
    trusted.workspace_id = Arc::from("prx_workspace");
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.catalog_generation = *catalog.generation();
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    trusted.delivery.interactive = false;
    trusted.delivery.can_ask_question = false;
    state.trusted = TrustedState::from_runtime(trusted);
    if let Some((status, id)) = prior {
        state.lineage = fixtures::next_hop(&state, uuid::Uuid::from_u128(7));
        state.observed = ObservedState::from_runtime_records(vec![fixtures::executed(status, &id)], Vec::new(), 0);
    }
    let view = catalog.view(&CatalogScope::default());
    let estimate = (
        CostEstimate {
            tokens: 1,
            micro_usd: 0,
        },
        1,
    );
    let cost = CapabilityCostModel {
        per_capability: view
            .iter_entries()
            .map(|entry| {
                (
                    CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    },
                    estimate,
                )
            })
            .collect(),
        estimates: CapabilityTarget::ALL
            .into_iter()
            .map(|target| (target, estimate))
            .collect(),
    };
    let call_fingerprints = BTreeMap::new();
    let replayed = BTreeSet::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "prx_user",
        sender: "prx_user",
        policy_channel: "terminal",
        chat_type: "dm",
        model_ref: "anthropic:claude",
        model_can_chat: true,
        model_can_call_tools: true,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &call_fingerprints,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &replayed,
        retry_limit: None,
    };
    let backend = DecisionBackendFactory::from_slots(
        Vec::new(),
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let ports = RehearsalPorts {
        memory: None,
        catalog: None,
        skill: None,
    };
    let discovered_refs = DiscoveredRefs::empty();
    let clock = super::super::logical_clock::ManualClock::new();
    let deadline = clock.instant() + Duration::from_secs(30);
    if let Some((name, occurrence)) = checkpoint {
        clock.arm_checkpoint(name, occurrence, Duration::from_secs(31));
    }
    let config = CausalTreeConfig::default();
    let mut inputs = EngineInputs {
        state: &state,
        catalog: &view,
        config: &config,
        key: &key,
        decision_id: DecisionId::new_runtime(),
        cost_model: &cost,
        discovered_refs: &discovered_refs,
        gates: &gates,
        delegation: None,
        delegation_max_depth: 2,
        prediction: ScoringPrediction {
            inner: backend.prediction_port(None),
            optimistic: checkpoint.is_some_and(|(name, _)| name == "candidate_scored")
                || turn.starts_with("Use file_read"),
        },
        rehearsal_ports: &ports,
        request_limits: RequestLimits::default(),
        model: Some("builtin-v4"),
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        terminal_only: false,
        allow_terminal_answer: false,
        deadline,
        inject_builtin_failure: false,
        after_prediction_pause: None,
    };
    super::super::logical_clock::scope(clock, async {
        let first = Box::pin(CausalDecisionEngine::decide_once(&inputs)).await.unwrap();
        if checkpoint.is_none()
            && !turn.starts_with("Use file_read")
            && matches!(&first.selection, Selection::Recover { candidate_id, .. } if matches!(candidate_id.action(), CausalActionKind::Retrieve | CausalActionKind::Discover))
        {
            // Reproduce the service's terminal pass when bounded recovery stops.
            // No tool result or successful-read observation is fabricated.
            inputs.terminal_only = true;
            CausalDecisionEngine::decide_terminal_prepared(CausalDecisionEngine::prepare_hop(&inputs).unwrap()).unwrap()
        } else {
            first
        }
    })
    .await
}

fn record(decision: &EngineDecision) -> crate::causal_tree::store::DecisionRecord {
    DecisionRecordBuilder::build_hop(decision, &fixture_key(), None, true, false).unwrap()
}

fn assert_answer(decision: &EngineDecision) {
    let directive = directive_from_selection(
        decision,
        &DirectiveContext {
            current_generation: decision.state.trusted.parts().catalog_generation,
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        },
    )
    .unwrap();
    assert!(matches!(directive, TurnExecutionDirective::AnswerDirectly));
    let row = record(decision);
    assert_eq!(row.directive_kind, "answer");
    assert_eq!(row.selected_candidate_id.as_deref(), Some("answer-none"));
    if let Selection::Abstain { reason_code, .. } = decision.selection {
        assert_eq!(row.abstention_reason.as_deref(), Some(reason_code));
        assert_eq!(
            row.features.get("fallback").and_then(serde_json::Value::as_str),
            Some("answer_on_abstention")
        );
    }
}

#[tokio::test]
async fn deadline_keeps_commit_only_after_complete_scoring() {
    for phase in ["selection", "post_selection"] {
        let decision = decide_turn("hello", Some((phase, 1)), false, None).await;
        assert!(
            matches!(decision.selection, Selection::Commit { .. }),
            "{:?}",
            decision.selection
        );
        assert_eq!(decision.fallback_cause, Some(FallbackCause::DeadlineExceeded));
        assert_eq!(
            record(&decision).abstention_reason.as_deref(),
            Some("deadline_exceeded")
        );
    }
    let partial = decide_turn(
        "Read /tmp/prx_fixture/input.md and write the result.",
        Some(("candidate_scored", 1)),
        true,
        None,
    )
    .await;
    assert_eq!(partial.scored_count, 1);
    assert!(
        partial.scores.values().next().unwrap().execution_utility > 0.62,
        "the regression requires an already-scored candidate above the bar"
    );
    assert_eq!(
        partial.scores.len(),
        partial.scored_count,
        "only actual scores are recorded"
    );
    assert!(matches!(
        partial.selection,
        Selection::Abstain {
            action: CausalActionKind::Answer,
            reason_code: "deadline_exceeded"
        }
    ));
    assert_answer(&partial);
}

#[tokio::test]
async fn o9_long_paths_and_output_contract_reaches_the_model() {
    let decision = decide_turn("Read /tmp/prx_fixture/brief.md, /tmp/prx_fixture/src/plan.rs and /tmp/prx_fixture/docs/contract.md. Complete the implementation and write /tmp/prx_fixture/report.md. Include the changed files and verification results. End the report with STATUS: READY_FOR_REVIEW.", None, true, None).await;
    assert_answer(&decision);
}

#[tokio::test]
async fn o5_exact_reply_uses_model_answer() {
    let decision = decide_turn("Reply with exactly one word: hello", None, false, None).await;
    assert_answer(&decision);
}

#[tokio::test]
async fn o14_negations_and_ten_paths_do_not_refuse() {
    let decision = decide_turn("Resume and finish the requested implementation. Do not run git, do not redo completed work, do not deploy, and do not change unrelated files. Inspect /tmp/prx_fixture/a.rs /tmp/prx_fixture/b.rs /tmp/prx_fixture/c.rs /tmp/prx_fixture/d.rs /tmp/prx_fixture/e.rs /tmp/prx_fixture/f.rs /tmp/prx_fixture/g.rs /tmp/prx_fixture/h.rs /tmp/prx_fixture/i.rs /tmp/prx_fixture/j.rs. Write the report with changes, tests and final STATUS.", None, true, None).await;
    assert_answer(&decision);
    assert_ne!(record(&decision).directive_kind, "refuse");
}

#[tokio::test]
async fn a_shell_explanation_never_executes_the_quoted_command() {
    let decision = decide_turn("what does `rm -rf build/` do?", None, true, None).await;
    assert_answer(&decision);
}

#[tokio::test]
async fn failed_tool_cannot_execute_on_the_next_hop() {
    let first = decide_turn("Use file_read on /tmp/prx_fixture/input.md", None, true, None).await;
    let CandidateSet::Legal { candidates, .. } = &first.candidates else {
        panic!("legal set")
    };
    let tool = candidates
        .iter()
        .find(|candidate| candidate.action() == CausalActionKind::UseTool)
        .unwrap()
        .candidate_id()
        .clone();
    assert!(
        matches!(&first.selection, Selection::Commit { candidate_id, .. } if candidate_id == &tool),
        "first-hop tool must be above every bar"
    );
    let second = decide_turn(
        "Use file_read on /tmp/prx_fixture/input.md",
        None,
        true,
        Some((ObservedStepStatus::Failed, tool.clone())),
    )
    .await;
    assert!(
        !matches!(second.selection, Selection::Commit { ref candidate_id, .. } | Selection::Recover { ref candidate_id, .. } if candidate_id == &tool)
    );
    assert_ne!(record(&second).directive_kind, "use_tool");
}

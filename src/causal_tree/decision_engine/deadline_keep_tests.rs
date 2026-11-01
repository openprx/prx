//! Virtual-clock deadline handling and the calibration rows for the three
//! decision reasons. The clock is task-local so production `Instant::now`
//! stays in place when no test installs it.

#![cfg(test)]

use super::super::branch::{ApprovalRequirement, CandidateSpec, CapabilityTarget, CostEstimate, SideEffectClass};
use super::super::candidate_generator::CapabilityCostModel;
use super::super::config::CausalPolicy;
use super::super::decision_score::{CandidatePrediction, CandidateScore, PredictionEvidence};
use super::super::decision_select::SelectorContext;
use super::super::gates::{GateOutcome, GateReport};
use super::super::state::{TrustedState, fixtures};
use super::super::store::{CausalDecisionStore, DecisionScope, RecordedCandidate, testing};
use super::*;
use super::{DecisionClock, PHASE_POST_SELECTION, PHASE_PREDICTION, PHASE_SCORING, PHASE_SELECTION};
use crate::decision::factory::{DecisionBackendFactory, SlotLimits};
use crate::decision::outbound::OutboundSecrets;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use std::sync::atomic::AtomicU8;
use std::time::Duration;

fn terminal_spec() -> CandidateSpec {
    CandidateSpec {
        namespace: None,
        required_capabilities: Vec::new(),
        side_effect: SideEffectClass::ReadOnly,
        approval: ApprovalRequirement::None,
        estimated_cost: Some(CostEstimate {
            tokens: 1,
            micro_usd: 1,
        }),
        estimated_latency_ms: Some(1),
    }
}

fn concrete_artifact_selection() -> Selection {
    let answer = CausalCandidate::new(CausalActionKind::Answer, CapabilityTarget::None, terminal_spec());
    let tool = CausalCandidate::new(
        CausalActionKind::UseTool,
        CapabilityTarget::Filesystem,
        CandidateSpec {
            side_effect: SideEffectClass::LocalWrite,
            ..terminal_spec()
        },
    );
    let clarify = CausalCandidate::new(CausalActionKind::Clarify, CapabilityTarget::None, terminal_spec());
    let refusal = CausalCandidate::new(CausalActionKind::Refuse, CapabilityTarget::None, terminal_spec());
    let mut scores = BTreeMap::new();
    let mut predictions = BTreeMap::new();
    let mut outcomes = BTreeMap::new();
    for candidate in [&answer, &tool, &clarify, &refusal] {
        outcomes.insert(candidate.candidate_id().clone(), GateOutcome::Passed);
    }
    for (candidate, utility, probability) in [(&answer, 0.364, 0.2), (&tool, 0.522, 0.5)] {
        scores.insert(
            candidate.candidate_id().clone(),
            CandidateScore {
                semantic_utility: utility,
                execution_utility: utility,
            },
        );
        predictions.insert(
            candidate.candidate_id().clone(),
            CandidatePrediction {
                choice_probability: probability,
                applicability_probability: 0.8,
                success_probability: 0.8,
                expected_gain: 0.5,
                gain_uncertainty: 0.1,
                semantic_risk: 0.4,
                risk_uncertainty: 0.1,
            },
        );
    }
    let report = GateReport {
        outcomes: outcomes.clone(),
        gate_set_hash: [0; 32],
    };
    let set = CandidateSet::Legal {
        discoverable_families: Default::default(),
        candidates: vec![answer, tool.clone(), clarify, refusal],
        exact_mentions: BTreeSet::new(),
        truncated: Vec::new(),
        approval_scopes: BTreeMap::new(),
        report: GateReport {
            outcomes,
            gate_set_hash: [0; 32],
        },
    };
    let evidence = PredictionEvidence {
        candidates: predictions,
        action_confidence: 0.29,
        target_confidence: 0.47,
        target_probabilities: BTreeMap::from([(CapabilityTarget::Filesystem, 0.76)]),
        chosen_action: "use_tool".to_string(),
        chosen_target: "filesystem".to_string(),
        evidence_sufficient: 0.377_540_668_798_145_4,
        needs_fresh_state: 0.0,
    };
    let policy = CausalPolicy {
        commit_threshold: 0.62,
        minimum_top_margin: 0.10,
        ..CausalPolicy::default()
    };
    let context = SelectorContext {
        interactive: true,
        direct_candidates: BTreeSet::new(),
        discovery_completed: false,
        single_exact_capability: false,
        single_exact_read_capability: false,
        concrete_artefact: true,
        pure_textual_reply: false,
    };
    let selection =
        CausalSelector::select(&set, &scores, &evidence, &report, &policy, &context).expect("test: concrete");
    assert!(
        matches!(
            &selection,
            Selection::Recover { candidate_id, reason_code: "concrete_artifact" }
                if *candidate_id == *tool.candidate_id()
        ),
        "selection={selection:?}"
    );
    selection
}

async fn decide_turn(turn: &str, exceed_at: Option<u8>) -> EngineDecision {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, turn);
    let catalog =
        CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).expect("test: catalog");
    let mut trusted = fixtures::trusted();
    trusted.owner_id = Some(Arc::from("prx_user"));
    trusted.workspace_id = Arc::from("prx_workspace");
    trusted.catalog_generation = *catalog.generation();
    state.trusted = TrustedState::from_runtime(trusted);
    let view = catalog.view(&CatalogScope::default());
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([(
            CapabilityTarget::None,
            (
                CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                },
                1,
            ),
        )]),
    };
    let security = SecurityPolicy::default();
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
        model_can_call_tools: false,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &call_fingerprints,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &replayed,
        retry_limit: Some(0),
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
    let deadline = Instant::now() + Duration::from_secs(30);
    let inputs = EngineInputs {
        state: &state,
        catalog: &view,
        config: &CausalTreeConfig::default(),
        key: &key,
        decision_id: DecisionId::parse("0192f0c4-0000-7000-8000-0000000000d1").expect("test: decision id"),
        cost_model: &cost,
        discovered_refs: &discovered_refs,
        gates: &gates,
        delegation: None,
        delegation_max_depth: 2,
        prediction: backend.prediction_port(None),
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
    let result = if let Some(exceed_at) = exceed_at {
        let clock = Arc::new(DecisionClock {
            before: deadline.checked_sub(Duration::from_secs(1)).expect("test: lookback"),
            after: deadline + Duration::from_secs(1),
            exceed_at,
            phase: AtomicU8::new(PHASE_PREDICTION),
        });
        super::DECISION_CLOCK
            .scope(clock, CausalDecisionEngine::decide_once(&inputs))
            .await
    } else {
        CausalDecisionEngine::decide_once(&inputs).await
    };
    result.expect("test: decide")
}

fn persisted_reason(decision: &EngineDecision) -> &'static str {
    match &decision.selection {
        Selection::Commit { .. } => match decision.fallback_cause {
            Some(FallbackCause::DeadlineExceeded) => "deadline_exceeded",
            other => panic!("kept commit must record the deadline, got {other:?}"),
        },
        Selection::Recover { reason_code, .. } => reason_code,
        other => panic!("unexpected selection {other:?}"),
    }
}

#[tokio::test]
async fn deadline_keeps_commit_above_threshold() {
    for exceed_at in [PHASE_POST_SELECTION, PHASE_SELECTION] {
        let decision = decide_turn("hello", Some(exceed_at)).await;
        assert!(
            matches!(
                &decision.selection,
                Selection::Commit { candidate_id, .. } if candidate_id.action() == CausalActionKind::Answer
            ),
            "phase={exceed_at} selection={:?} fallback={:?} scores={:?}",
            decision.selection,
            decision.fallback_cause,
            decision.scores
        );
        assert_eq!(decision.fallback_cause, Some(FallbackCause::DeadlineExceeded));
        assert!(decision.scored_count > 0, "a kept commit was already scored");
        assert!(decision.prediction.is_some());
    }

    let expired = decide_turn("hello", Some(PHASE_SCORING)).await;
    assert_eq!(expired.scored_count, 0, "scoring after the deadline stays at zero");
    assert_eq!(expired.fallback_cause, Some(FallbackCause::DeadlineExceeded));
    assert!(
        matches!(expired.selection, Selection::Abstain { .. }),
        "selection={:?} scores={:?}",
        expired.selection,
        expired.scores
    );
}

#[tokio::test]
async fn calibration_records_store_the_three_decision_reasons() {
    let concrete = concrete_artifact_selection();
    let pure = decide_turn("Reply with exactly one word: hello", None).await;
    let kept = decide_turn("hello", Some(PHASE_POST_SELECTION)).await;
    let reasons = [
        (
            match &concrete {
                Selection::Recover { reason_code, .. } => *reason_code,
                other => panic!("concrete selection {other:?}"),
            },
            "use_tool",
        ),
        (persisted_reason(&pure), "answer"),
        (persisted_reason(&kept), "answer"),
    ];
    assert_eq!(reasons.first().map(|entry| entry.0), Some("concrete_artifact"));
    assert_eq!(reasons.get(1).map(|entry| entry.0), Some("pure_answer_no_tool"));
    assert_eq!(reasons.get(2).map(|entry| entry.0), Some("deadline_exceeded"));

    let workspace = tempfile::tempdir().expect("test: workspace");
    let store = testing::open_private(
        workspace.path(),
        Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
    );
    let writer = store.writer().expect("test: writer");
    let workspace_id = store.workspace_id().expect("test: workspace id");
    let scope = DecisionScope::Owner {
        workspace_id: Arc::clone(&workspace_id),
        owner_id: Arc::from("prx_user"),
    };
    let mut ids = Vec::new();
    for (index, (reason, action)) in reasons.iter().enumerate() {
        let id = DecisionId::new_runtime();
        let mut record = testing::record(id, &workspace_id, Some("prx_user"), 1_700_000_000_000 + index as i64);
        record.configured_model = "builtin-v4".to_string();
        record.directive_kind = (*action).to_string();
        record.abstention_reason = Some((*reason).to_string());
        record.selected_candidate_id = Some(format!("{action}-none"));
        record.candidates = vec![RecordedCandidate {
            id: format!("{action}-none"),
            action: (*action).to_string(),
            target: "none".to_string(),
            propensity: None,
        }];
        writer
            .record_and_replay(record, scope.clone(), Instant::now() + Duration::from_secs(2))
            .await
            .expect("test: record")
            .expect("test: committed row");
        ids.push(id);
    }
    let reader = store.reader();
    for (id, (reason, action)) in ids.iter().zip(reasons) {
        let row = reader
            .get(&scope, id)
            .expect("test: read")
            .expect("test: stored decision");
        assert_eq!(row.abstention_reason.as_deref(), Some(reason));
        assert_eq!(row.directive_kind, action);
        assert_eq!(row.configured_model, "builtin-v4");
    }
    CausalDecisionStore::close(Arc::new(store), Instant::now() + Duration::from_secs(2)).await;
}

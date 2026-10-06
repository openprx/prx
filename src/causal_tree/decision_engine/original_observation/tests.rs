#![cfg(test)]
//! Component provenance tests; no authenticated runtime source or HTTP claim.

use super::*;
use crate::causal_tree::branch::{CapabilityTarget, CostEstimate};
use crate::causal_tree::candidate_generator::CapabilityCostModel;
use crate::causal_tree::state::{TrustedState, fixtures};
use crate::decision::backend::DecisionBackend;
use crate::decision::builtin::BuiltinDecisionBackend;
use crate::decision::factory::SlotLimits;
use crate::decision::outbound::{GuardedBody, OutboundSecrets};
use crate::decision::validate::ModelVersionPolicy;
use crate::decision::{BackendIdentity, BackendKind, BackendLocality, DecisionResponse};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use std::sync::atomic::{AtomicUsize, Ordering};

struct OriginBackend {
    identity: BackendIdentity,
    matched: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DecisionBackend for OriginBackend {
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
        // Compare at the real factory consumer, before returning a response.
        // An independently recompiled request has a different request id and
        // cannot pass the original manifest comparison.
        let same = ACTIVE
            .try_with(|owner| {
                let recorder = owner.lock();
                recorder.capture.requests.last().is_some_and(|captured| {
                    captured.manifest == *request.manifest()
                        && captured.payload.get("request") == Some(&request_payload(request))
                })
            })
            .unwrap_or(false);
        if same {
            self.matched.fetch_add(1, Ordering::SeqCst);
        }
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".to_owned();
        response.resolved_model = self.identity.configured_model.to_string();
        Ok(response)
    }
}

#[derive(Clone, Copy)]
enum RunMode {
    Once,
    Duplicate,
    Missing,
    MismatchedModel,
}

async fn run(mode: RunMode, event_limit: usize) -> anyhow::Result<(Vec<EngineDecision>, Observation, usize)> {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "prx neutral observation fixture");
    let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default())?;
    let mut trusted = fixtures::trusted();
    trusted.owner_id = Some(Arc::from("prx_user"));
    trusted.workspace_id = Arc::from("prx_workspace");
    trusted.catalog_generation = *catalog.generation();
    trusted.budgets.latency_ms = 500;
    // Keep this component's trusted main-model fact coherent with its G7 input.
    // The paired negative retains the old mismatch and must refuse prediction.
    if !matches!(mode, RunMode::MismatchedModel) {
        trusted.resolved_model = Arc::from("ollama:prx_observation_model");
    }
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
    let calls = BTreeMap::new();
    let replayed = BTreeSet::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "prx_user",
        sender: "prx_user",
        policy_channel: "terminal",
        chat_type: "dm",
        model_ref: "ollama:prx_observation_model",
        model_can_chat: true,
        model_can_call_tools: false,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &calls,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &replayed,
        retry_limit: Some(0),
    };
    let matched = Arc::new(AtomicUsize::new(0));
    let backend = DecisionBackendFactory::from_slots(
        vec![Box::new(OriginBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx_observation_model"),
            },
            matched: Arc::clone(&matched),
        })],
        Arc::clone(&key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let ports = RehearsalPorts {
        memory: None,
        catalog: None,
        skill: None,
    };
    let discovered = DiscoveredRefs::empty();
    let config = CausalTreeConfig::default();
    assert_eq!(config.policy.extra_latency_budget_ms, 500);
    let inputs = EngineInputs {
        state: &state,
        catalog: &view,
        config: &config,
        key: &key,
        decision_id: DecisionId::parse("0192f0c4-0000-7000-8000-000000000018")?,
        cost_model: &cost,
        discovered_refs: &discovered,
        gates: &gates,
        delegation: None,
        delegation_max_depth: 2,
        prediction: backend.prediction_port(None),
        rehearsal_ports: &ports,
        request_limits: RequestLimits::default(),
        model: Some("prx_observation_model"),
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        terminal_only: false,
        allow_terminal_answer: false,
        deadline: Instant::now() + Duration::from_millis(500),
        inject_builtin_failure: false,
        after_prediction_pause: None,
    };
    let work = async {
        tokio::task::yield_now().await;
        let first = CausalDecisionEngine::decide_once(&inputs).await?;
        let mut outcomes = vec![first];
        if matches!(mode, RunMode::Duplicate) {
            outcomes.push(CausalDecisionEngine::decide_once(&inputs).await?);
        }
        Ok::<_, anyhow::Error>(outcomes)
    };
    let (outcomes, captured) = if matches!(mode, RunMode::Missing) {
        let outcomes = work.await?;
        let (_, capture) = capture("prx_original_request", std::future::ready(())).await?;
        (outcomes, capture)
    } else {
        let (outcomes, captured) = capture_bounded("prx_original_request", event_limit, work).await?;
        (outcomes?, captured)
    };
    Ok((outcomes, captured, matched.load(Ordering::SeqCst)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_compiler_object_reaches_backend_and_scores_join_outcome() -> anyhow::Result<()> {
    let (outcomes, captured, matched) = run(RunMode::Once, MAX_EVENTS).await?;
    assert_eq!(
        matched, 1,
        "the factory consumed the same original sealed object values"
    );
    let first = outcomes
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing engine outcome"))?;
    assert!(first.request_compiled && first.scored_count > 0 && first.prediction.is_some());
    let value = captured.checked_sidecar(&outcomes.iter().collect::<Vec<_>>())?;
    let original_expiry = captured
        .requests
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing request"))?
        .manifest
        .expires_at();
    assert_eq!(original_expiry, first.deadline);
    assert_eq!(
        value.pointer("/requests/0/original/request/expiry_process_local_instant_debug"),
        Some(&json!(format!("{original_expiry:?}")))
    );
    assert_eq!(
        value.pointer("/clock_provenance/decisions/0/original_decision_deadline_debug"),
        Some(&json!(format!("{:?}", first.deadline)))
    );
    assert_eq!(
        value.pointer("/clock_provenance/request_expiry_equals_original_decision_deadline"),
        Some(&json!(true))
    );
    let root = tempfile::tempdir()?;
    let path = root.path().join("original-observation.json");
    let bytes = serde_json::to_vec(&value)?;
    std::fs::write(&path, &bytes)?;
    assert_eq!(std::fs::read(&path)?, bytes);
    assert_eq!(captured.requests.len(), 1);
    assert_eq!(captured.scores.len(), first.scored_count);
    for score in &captured.scores {
        assert_eq!(
            score.payload.get("remaining_latency_ms"),
            score.payload.get("remaining_budget_latency_ms")
        );
        assert_eq!(
            score.payload.get("remaining_micro_usd"),
            score.payload.get("remaining_budget_micro_usd")
        );
    }
    Ok(())
}

#[tokio::test]
async fn missing_duplicate_and_overflow_observations_are_not_accepted() -> anyhow::Result<()> {
    for (mode, limit, error) in [
        (RunMode::Missing, MAX_EVENTS, ObservationError::Missing),
        (RunMode::Duplicate, MAX_EVENTS, ObservationError::Duplicate),
        (RunMode::Once, 0, ObservationError::Bound),
    ] {
        let (outcomes, captured, _) = run(mode, limit).await?;
        assert_eq!(
            captured.checked_sidecar(&outcomes.iter().collect::<Vec<_>>()).err(),
            Some(error)
        );
    }
    Ok(())
}

#[tokio::test]
async fn original_request_score_and_source_mutations_are_rejected() -> anyhow::Result<()> {
    let (outcomes, mut captured, _) = run(RunMode::Once, MAX_EVENTS).await?;
    let decisions = outcomes.iter().collect::<Vec<_>>();
    captured.checked_sidecar(&decisions)?;
    // Alter individual retained bindings while leaving their original digest.
    for field in [
        "request_fingerprint",
        "truncated_fields",
        "questions",
        "expiry_process_local_instant_debug",
    ] {
        let request = captured
            .requests
            .first_mut()
            .ok_or_else(|| anyhow::anyhow!("missing request"))?;
        let original = request.payload.clone();
        let target = request
            .payload
            .get_mut("request")
            .and_then(|value| value.get_mut(field))
            .ok_or_else(|| anyhow::anyhow!("missing observed binding"))?;
        *target = json!("prx_changed_original_field");
        assert_eq!(
            captured.checked_sidecar(&decisions).err(),
            Some(ObservationError::Changed)
        );
        captured
            .requests
            .first_mut()
            .ok_or_else(|| anyhow::anyhow!("missing request"))?
            .payload = original;
    }
    let score = captured
        .scores
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("missing score"))?;
    let original = score.payload.clone();
    *score
        .payload
        .get_mut("remaining_latency_ms")
        .ok_or_else(|| anyhow::anyhow!("missing actual denominator"))? = json!(u64::MAX);
    assert_eq!(
        captured.checked_sidecar(&decisions).err(),
        Some(ObservationError::Changed)
    );
    captured
        .scores
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("missing score"))?
        .payload = original;
    captured
        .source
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("missing actual source"))?
        .source = Some(Arc::from("prx_unrelated_event"));
    assert_eq!(
        captured.checked_sidecar(&decisions).err(),
        Some(ObservationError::Source)
    );
    Ok(())
}

#[tokio::test]
async fn mismatched_model_cannot_create_original_observation() -> anyhow::Result<()> {
    let (outcomes, captured, matched) = run(RunMode::MismatchedModel, MAX_EVENTS).await?;
    assert_eq!(matched, 0);
    assert!(
        outcomes
            .iter()
            .all(|outcome| !outcome.request_compiled && outcome.scored_count == 0)
    );
    assert_eq!(
        captured.checked_sidecar(&outcomes.iter().collect::<Vec<_>>()).err(),
        Some(ObservationError::Missing)
    );
    Ok(())
}

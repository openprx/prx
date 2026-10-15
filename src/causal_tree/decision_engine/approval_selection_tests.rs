#![cfg(test)]
//! An exactly named, approval-required tool through the real engine: does the
//! decision ask for approval, or does an ordinary answer win?

use super::*;
use crate::causal_tree::branch::Ns8;
use crate::causal_tree::candidate_generator::{CapabilityCostModel, DiscoveredRefs};
use crate::causal_tree::config::CausalTreeConfig;
use crate::causal_tree::decision_rehearsal::RehearsalPorts;
use crate::causal_tree::state::{TrustedState, fixtures};
use crate::decision::factory::{DecisionBackendFactory, SlotLimits};
use crate::decision::outbound::OutboundSecrets;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};

async fn decide(mcp: bool, with_call_fingerprint: bool) -> anyhow::Result<EngineDecision> {
    let key = fixtures::key();
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Supervised;
    let registry: Vec<Box<dyn crate::tools::Tool>> = if mcp {
        Vec::new()
    } else {
        vec![Box::new(crate::tools::file_write::FileWriteTool::new(Arc::new(
            security.clone(),
        )))]
    };
    let mut sources = CatalogSources::default();
    if mcp {
        sources.mcp.push(crate::tools::mcp::McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "write".into(),
            state_digest: [1; 32],
            spec: crate::tools::traits::ToolSpec {
                name: "mcp__prx_server__write".into(),
                description: "Write remote data".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        });
    }
    let surface = SurfacePolicy {
        direct_names: BTreeSet::from(["file_write".to_owned(), "mcp__prx_server__write".to_owned()]),
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&registry, &sources, &surface)?;
    let view = catalog.view(&CatalogScope::default());
    let entry = view
        .entries()
        .iter()
        .find(|entry| entry.public_name.contains("write"))
        .ok_or_else(|| anyhow::anyhow!("missing file_write"))?;
    let mut state = fixtures::state(
        &key,
        if mcp {
            "please call mcp__prx_server__write now"
        } else {
            "please run file_write now"
        },
    );
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.owner_id = Some(Arc::from("owner-1"));
    trusted.side_effect_mode = crate::causal_tree::state::SideEffectMode::ApprovalRequired;
    state.trusted = TrustedState::from_runtime(trusted);
    let estimate = (
        crate::causal_tree::branch::CostEstimate {
            tokens: 1,
            micro_usd: 0,
        },
        1,
    );
    let mut cost = CapabilityCostModel::default();
    cost.estimates.insert(CapabilityTarget::None, estimate);
    cost.estimates.insert(entry.family, estimate);
    cost.per_capability.insert(
        CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        },
        estimate,
    );
    let exact = CandidateId::new(
        CausalActionKind::UseTool,
        entry.family,
        Some(Ns8::of(&key, &entry.namespace)),
    );
    let calls = if with_call_fingerprint {
        BTreeMap::from([(exact, [7; 32])])
    } else {
        BTreeMap::new()
    };
    let replayed = BTreeSet::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "owner-1",
        sender: "owner-1",
        policy_channel: "terminal",
        chat_type: "dm",
        model_ref: "anthropic:claude",
        model_can_chat: true,
        model_can_call_tools: true,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &calls,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &replayed,
        retry_limit: Some(2),
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
    let discovered = DiscoveredRefs::empty();
    let inputs = EngineInputs {
        state: &state,
        catalog: &view,
        config: &CausalTreeConfig::default(),
        key: &key,
        decision_id: DecisionId::new_runtime(),
        cost_model: &cost,
        discovered_refs: &discovered,
        gates: &gates,
        delegation: None,
        delegation_max_depth: 2,
        prediction: backend.prediction_port(None),
        rehearsal_ports: &ports,
        request_limits: Default::default(),
        model: Some("anthropic:claude"),
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        terminal_only: false,
        allow_terminal_answer: false,
        deadline: Instant::now() + Duration::from_secs(5),
        inject_builtin_failure: false,
        after_prediction_pause: None,
    };
    Ok(CausalDecisionEngine::decide_once(&inputs).await?)
}

/// Finding 66: an exactly named approval-required tool whose approval scope is
/// available (`request_approval-mcp` is a passed terminal) asks for approval
/// instead of answering.
#[tokio::test]
async fn exact_approval_required_tool_requests_approval_instead_of_answering() -> anyhow::Result<()> {
    let decision = decide(true, true).await?;
    assert!(
        matches!(decision.selection, Selection::RequestApproval { .. }),
        "selection={:?}",
        decision.selection
    );
    Ok(())
}

/// The approval-deferred `file_write` path is unchanged: no answer, a terminal.
#[tokio::test]
async fn exact_file_write_stays_on_the_approval_deferred_terminal_path() -> anyhow::Result<()> {
    for fingerprint in [false, true] {
        let decision = decide(false, fingerprint).await?;
        assert!(
            matches!(
                decision.selection,
                Selection::Abstain {
                    action: CausalActionKind::Refuse | CausalActionKind::Clarify,
                    ..
                }
            ),
            "selection={:?}",
            decision.selection
        );
    }
    Ok(())
}

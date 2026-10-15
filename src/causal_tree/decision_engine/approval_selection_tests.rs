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

#[derive(Clone, Copy)]
struct Scenario {
    mcp: bool,
    fingerprint: bool,
    exact: bool,
    approval_channel: bool,
    mode: crate::causal_tree::state::SideEffectMode,
}

impl Scenario {
    const fn mcp(fingerprint: bool) -> Self {
        Self {
            mcp: true,
            fingerprint,
            exact: true,
            approval_channel: true,
            mode: crate::causal_tree::state::SideEffectMode::ApprovalRequired,
        }
    }
}

async fn decide(scenario: Scenario) -> anyhow::Result<(EngineDecision, CandidateId)> {
    let Scenario {
        mcp,
        fingerprint: with_call_fingerprint,
        exact,
        approval_channel,
        mode,
    } = scenario;
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
        direct_names: if exact {
            BTreeSet::from(["file_write".to_owned(), "mcp__prx_server__write".to_owned()])
        } else {
            // Not named and not direct: only the local-discovery diagnostic
            // scope can exist for the tool.
            BTreeSet::new()
        },
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&registry, &sources, &surface)?;
    let view = catalog.view(&CatalogScope::default());
    let entry = view
        .entries()
        .iter()
        .find(|entry| entry.public_name.contains("write"))
        .ok_or_else(|| anyhow::anyhow!("missing write capability"))?;
    let mut state = fixtures::state(
        &key,
        if mcp && !exact {
            "please do something remotely"
        } else if mcp {
            "please call mcp__prx_server__write now"
        } else {
            "please run file_write now"
        },
    );
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.owner_id = Some(Arc::from("owner-1"));
    trusted.side_effect_mode = mode;
    trusted.delivery.approval_channel = approval_channel;
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
    let fingerprinted = if exact {
        CandidateId::new(
            CausalActionKind::UseTool,
            entry.family,
            Some(Ns8::of(&key, &entry.namespace)),
        )
    } else {
        CandidateId::new(CausalActionKind::Discover, entry.family, None)
    };
    let exact_id = CandidateId::new(
        CausalActionKind::UseTool,
        entry.family,
        Some(Ns8::of(&key, &entry.namespace)),
    );
    let calls = if with_call_fingerprint {
        BTreeMap::from([(fingerprinted, [7; 32])])
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
    Ok((CausalDecisionEngine::decide_once(&inputs).await?, exact_id))
}

fn is_terminal_abstain(selection: &Selection) -> bool {
    matches!(
        selection,
        Selection::Abstain {
            action: CausalActionKind::Refuse | CausalActionKind::Clarify,
            ..
        }
    )
}

fn is_answer(selection: &Selection) -> bool {
    matches!(selection, Selection::Commit { candidate_id, .. } if candidate_id.action() == CausalActionKind::Answer)
}

/// Finding 66: an exactly named approval-required tool whose approval scope is
/// available (`request_approval-mcp` is a passed terminal) asks for approval
/// instead of answering, for exactly that tool.
#[tokio::test]
async fn exact_approval_required_tool_requests_approval_instead_of_answering() -> anyhow::Result<()> {
    let (decision, exact) = decide(Scenario::mcp(true)).await?;
    assert!(
        matches!(&decision.selection, Selection::RequestApproval { pending_candidate_id, .. }
            if *pending_candidate_id == exact),
        "selection={:?}",
        decision.selection
    );
    Ok(())
}

/// Pins the accepted decision: without a call fingerprint no approval scope
/// exists (an argumentless turn mints none), so the ordinary answer stands.
#[tokio::test]
async fn exact_mention_without_call_fingerprint_still_answers() -> anyhow::Result<()> {
    let (decision, _) = decide(Scenario::mcp(false)).await?;
    assert!(is_answer(&decision.selection), "selection={:?}", decision.selection);
    Ok(())
}

/// A fingerprint for a tool the turn did not name never turns an answer into
/// an approval request.
#[tokio::test]
async fn approval_only_tool_that_is_not_named_still_answers() -> anyhow::Result<()> {
    let (decision, _) = decide(Scenario {
        exact: false,
        ..Scenario::mcp(true)
    })
    .await?;
    // The scenario does offer an approval scope for the MCP tool, so it is the
    // exact-mention filter that keeps the answer.
    let CandidateSet::Legal { approval_scopes, .. } = &decision.candidates else {
        panic!("expected a legal set with an answer");
    };
    assert!(
        approval_scopes.values().flatten().next().is_some(),
        "the fixture must offer an approval scope"
    );
    assert!(is_answer(&decision.selection), "selection={:?}", decision.selection);
    Ok(())
}

/// No approval channel, or a read-only turn that denies the write, offers no
/// approval request.
#[tokio::test]
async fn unavailable_or_denied_approval_yields_no_request() -> anyhow::Result<()> {
    for scenario in [
        Scenario {
            approval_channel: false,
            ..Scenario::mcp(true)
        },
        Scenario {
            mode: crate::causal_tree::state::SideEffectMode::ReadOnly,
            ..Scenario::mcp(true)
        },
    ] {
        let (decision, _) = decide(scenario).await?;
        assert!(
            !matches!(decision.selection, Selection::RequestApproval { .. }),
            "selection={:?}",
            decision.selection
        );
    }
    Ok(())
}

/// The approval-deferred `file_write` path is unchanged: no answer, a terminal.
#[tokio::test]
async fn exact_file_write_stays_on_the_approval_deferred_terminal_path() -> anyhow::Result<()> {
    for fingerprint in [false, true] {
        let (decision, _) = decide(Scenario {
            mcp: false,
            fingerprint,
            ..Scenario::mcp(true)
        })
        .await?;
        assert!(
            is_terminal_abstain(&decision.selection),
            "selection={:?}",
            decision.selection
        );
    }
    Ok(())
}

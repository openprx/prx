#![cfg(test)]
//! Gate-dependent generator tests, run through the execution admission probe
//! (`HardGatesProbe`). Bodies are unchanged; the shim below adapts the legacy
//! `GateInputs` call shape. Part 2 retires them with the hard gates.

use crate::causal_tree::gates::{GateError, GateInputs, HardGates};
use crate::causal_tree::hard_gates_probe::HardGatesProbe;

struct CausalCandidateGenerator;

impl CausalCandidateGenerator {
    fn generate(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
    ) -> Result<CandidateSet, GateError> {
        crate::decision_engine::candidate_generator::CausalCandidateGenerator::generate(
            state,
            catalog,
            policy,
            &HardGatesProbe { state, catalog, gates },
        )
    }
}

fn select_family_candidate<'a>(
    action: CausalActionKind,
    family: CapabilityTarget,
    entries: impl IntoIterator<Item = &'a CapabilityEntry>,
    bind_namespace: bool,
    state: &CausalState,
    catalog: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    gates: &GateInputs<'_>,
    generation_rejections: &mut BTreeMap<CandidateId, GateOutcome>,
) -> Result<Option<CausalCandidate>, GateError> {
    crate::decision_engine::candidate_generator::select_family_candidate(
        action,
        family,
        entries,
        bind_namespace,
        state,
        catalog,
        policy,
        &HardGatesProbe { state, catalog, gates },
        generation_rejections,
    )
}

use crate::causal_tree::gates::GateOutcome;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::decision_engine::candidate::{CandidateId, CandidateSpec, CausalCandidate};
use crate::decision_engine::candidate_generator::*;
use crate::decision_engine::cost_model::{CapabilityCostModel, CostEstimateSource};
use crate::decision_engine::state::CausalState;
use crate::tools::capability::catalog::{CapabilityEntry, CatalogView, Exposure};
use crate::tools::capability::contract::{
    CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
};
use crate::tools::capability::turn_state::SideEffectMode;

struct CountingCost<'a> {
    inner: &'a CapabilityCostModel,
    entries: AtomicUsize,
}

impl CostEstimateSource for CountingCost<'_> {
    fn estimate_entry(&self, entry: &CapabilityEntry) -> Option<(CostEstimate, u32)> {
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.inner.estimate_entry(entry)
    }

    fn estimate_terminal(&self) -> Option<(CostEstimate, u32)> {
        self.inner.estimate_terminal()
    }
}
static EMPTY_DISCOVERED_REFS: std::sync::LazyLock<DiscoveredRefs> = std::sync::LazyLock::new(DiscoveredRefs::empty);
use crate::decision_engine::state::{RawRoutingInput, fixtures};
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::turn_state::TrustedState;
use crate::tools::mcp::McpCapabilitySpec;
use crate::tools::traits::ToolSpec;
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;

fn fixture_cost_for_catalog(catalog: &CapabilityCatalog, mut cost: CapabilityCostModel) -> CapabilityCostModel {
    let view = catalog.view(&CatalogScope::default());
    for entry in view.entries() {
        let estimate = cost.estimates.get(&entry.family).or_else(|| {
            (entry.public_name == "delegate" || entry.public_name == "sessions_spawn")
                .then(|| cost.estimates.get(&CapabilityTarget::Session))
                .flatten()
        });
        if let Some(estimate) = estimate {
            cost.per_capability.insert(
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                },
                *estimate,
            );
        }
    }
    cost
}

#[test]
fn native_file_plan_keeps_each_ref_and_respects_read_only_and_exclusions() {
    let key = fixtures::key();
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let shared = Arc::new(security.clone());
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&shared), false)),
        Box::new(crate::tools::file_write::FileWriteTool::new(Arc::clone(&shared))),
        Box::new(crate::tools::file_edit::FileEditTool::new(shared)),
    ];
    let surface = SurfacePolicy {
        direct_names: BTreeSet::from(["file_read".into(), "file_write".into(), "file_edit".into()]),
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([
                (
                    CapabilityTarget::Filesystem,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 1,
                        },
                        1,
                    ),
                ),
                (
                    CapabilityTarget::None,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 1,
                        },
                        1,
                    ),
                ),
            ]),
        },
    );
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: None,
    };
    for (read_only, exclude_write, expected) in [(false, false, 3usize), (false, true, 2), (true, false, 1)] {
        let mut state = fixtures::state(&key, "Perform the native workspace file task.");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = if read_only {
            SideEffectMode::ReadOnly
        } else {
            SideEffectMode::GuardedWrite
        };
        if exclude_write {
            trusted.exclusions.push(Arc::from("file_write"));
        }
        state.trusted = TrustedState::from_runtime(trusted);
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap()
        else {
            panic!("legal file plan")
        };
        let file = candidates
            .iter()
            .find(|candidate| {
                candidate.action() == CausalActionKind::UseTool && candidate.target() == CapabilityTarget::Filesystem
            })
            .unwrap();
        assert_eq!(file.required_capabilities().len(), expected);
        assert_eq!(file.estimated_cost().unwrap().tokens, u32::try_from(expected).unwrap());
        assert_eq!(
            file.estimated_cost().unwrap().micro_usd,
            u64::try_from(expected).unwrap()
        );
        assert_eq!(
            file.side_effect(),
            if read_only {
                SideEffectClass::ReadOnly
            } else {
                SideEffectClass::LocalWrite
            }
        );
        assert_eq!(HardGates::check_one(&state, &view, file, &gates), GateOutcome::Passed);
        for reference in file.required_capabilities() {
            let entry = view.entry_any(&reference.id).unwrap();
            assert_eq!(reference.fingerprint, entry.fingerprint);
            assert_eq!(reference.generation, entry.generation);
            assert!(!exclude_write || entry.public_name != "file_write");
        }
    }

    for (input, expected) in [
        (
            "Use file_write to create a file, then file_read to verify it.",
            vec!["file_read", "file_write"],
        ),
        ("Use file_read to inspect the existing file.", vec!["file_read"]),
    ] {
        let mut state = fixtures::state(&key, input);
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap()
        else {
            panic!("legal exact file plan")
        };
        let exact: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                candidate.target() == CapabilityTarget::Filesystem && exact_mentions.contains(candidate.candidate_id())
            })
            .collect();
        assert_eq!(exact.len(), 1, "one complete exact plan: {input}");
        let filesystem_plans = candidates
            .iter()
            .filter(|candidate| candidate.target() == CapabilityTarget::Filesystem)
            .count();
        assert_eq!(
            filesystem_plans,
            if expected.len() > 1 { 1 } else { 2 },
            "a complete named plan supersedes its broader fallback; a single name preserves it"
        );
        let candidate = exact.first().unwrap();
        let mut names: Vec<_> = candidate
            .required_capabilities()
            .iter()
            .map(|reference| view.entry_any(&reference.id).unwrap().public_name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, expected, "all and only the named file operations: {input}");
        assert_eq!(
            HardGates::check_one(&state, &view, candidate, &gates),
            GateOutcome::Passed
        );
    }
}

#[test]
fn priced_answer_does_not_charge_baseline_serving_latency_to_the_decision_budget() {
    let key = fixtures::key();
    let estimate = CostEstimate {
        tokens: 128,
        micro_usd: 7,
    };
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([(CapabilityTarget::None, (estimate, 250))]),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 1,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };

    let answer = terminal(CausalActionKind::Answer, &policy);

    assert_eq!(answer.estimated_cost(), Some(estimate));
    assert_eq!(answer.estimated_latency_ms(), Some(0));
}

#[test]
fn provider_outage_with_no_legal_path_yields_typed_refusal() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "unmatched request");
    let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    state.trusted = TrustedState::from_runtime(trusted);
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
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let security = SecurityPolicy::default();
    let calls_seen = BTreeSet::<[u8; 32]>::new();
    let calls = BTreeMap::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "owner-1",
        sender: "owner-1",
        policy_channel: "terminal",
        chat_type: "dm",
        model_ref: "anthropic:claude",
        model_can_chat: false,
        model_can_call_tools: false,
        decision_endpoint_ready: false,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &calls,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &calls_seen,
        retry_limit: Some(2),
    };
    let view = catalog.view(&CatalogScope::default());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(matches!(&generated, CandidateSet::NoLegalPath { .. }) && !offers_approval(&generated));
}

/// Whether a no-legal-path set proposed an approval request. The generator
/// proposes one exactly when it found an action-bound approval-only scope.
fn offers_approval(set: &CandidateSet) -> bool {
    let CandidateSet::NoLegalPath { report, .. } = set else {
        return false;
    };
    report
        .outcomes
        .keys()
        .any(|id| id.as_str().starts_with("request_approval-"))
}

#[test]
fn exact_mention_requires_identifier_boundaries() {
    assert!(contains_bounded("use prx_read now", "prx_read"));
    assert!(!contains_bounded("use prx_reader now", "prx_read"));
    assert!(!contains_bounded("use other_prx_read now", "prx_read"));
}

#[test]
fn pinned_exact_terms_match_live_lowercase_for_unicode_aliases_and_boundaries() {
    let mut sources = CatalogSources::default();
    sources.mcp.push(McpCapabilitySpec {
        server: "prx_server".into(),
        tool: "\u{130}-Write".into(),
        state_digest: [1; 32],
        spec: ToolSpec {
            name: "mcp__prx_server__write".into(),
            description: "PRX alias fixture".into(),
            parameters: json!({"type":"object","properties":{}}),
        },
    });
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("\u{130}-Node"))];
    let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    for entry in view.entries() {
        for input in [
            "use i\u{307}-node now",
            "use \u{130}-node now",
            "call i\u{307}-write!",
            "call other_i\u{307}-write now",
            "call i\u{307}-writer now",
            "mcp__prx_server__write",
        ] {
            let folded = input.to_lowercase();
            let old = std::iter::once(entry.id.as_str())
                .chain(std::iter::once(entry.public_name.as_str()))
                .chain(entry.aliases.iter().map(String::as_str))
                .any(|name| contains_bounded(&folded, &name.to_lowercase()));
            assert_eq!(
                exact_mention(&folded, entry),
                old,
                "input={input}, entry={}",
                entry.id.as_str()
            );
        }
    }
    assert!(
        view.entries()
            .iter()
            .any(|entry| { entry.exact_mention_terms.iter().any(|term| term == "i\u{307}-write") })
    );
}

#[test]
fn fixed_terminal_candidate_generation_has_inventory_independent_allocations() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "neutral request");
    let mut baseline = None;
    for size in [0, 1, 64, 256, 256, 64, 1, 0] {
        let sources = CatalogSources {
            mcp: (0..size)
                .map(|index| McpCapabilitySpec {
                    server: "prx_server".into(),
                    tool: format!("write_{index}"),
                    state_digest: [0; 32],
                    spec: ToolSpec {
                        name: format!("mcp__prx_server__write_{index}"),
                        description: "PRX unrelated write fixture".into(),
                        parameters: json!({"type":"object","properties":{}}),
                    },
                })
                .collect(),
            ..CatalogSources::default()
        };
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.owner_id = Some(std::sync::Arc::from("prx_user"));
        trusted.resolved_model = std::sync::Arc::from("prx_model");
        trusted.delivery.interactive = false;
        trusted.delivery.can_ask_question = true;
        state.trusted = TrustedState::from_runtime(trusted);
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
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let security = SecurityPolicy::default();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "prx_user",
            sender: "prx_user",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "prx_model",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let (generated, allocations) = crate::decision_engine::prediction::builtin::allocation_counter::count(|| {
            CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap()
        });
        let shape = format!("{generated:?}");
        let CandidateSet::Legal { candidates, .. } = &generated else {
            panic!("expected terminal candidates");
        };
        assert_eq!(candidates.len(), 3);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.required_capabilities().is_empty())
        );
        if let Some((expected_shape, expected_allocations)) = &baseline {
            assert_eq!(
                &shape, expected_shape,
                "complete candidates and gate reports at size {size}"
            );
            assert_eq!(
                &allocations, expected_allocations,
                "generator allocations at size {size}"
            );
        } else {
            baseline = Some((shape, allocations));
        }
    }
}

#[test]
fn single_shot_surface_can_clarify_without_interactive_approval() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "neutral request");
    let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.delivery.interactive = false;
    trusted.delivery.can_ask_question = true;
    state.trusted = TrustedState::from_runtime(trusted);
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
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 3,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let security = SecurityPolicy::default();
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let generated =
        CausalCandidateGenerator::generate(&state, &catalog.view(&CatalogScope::default()), &policy, &gates).unwrap();
    let CandidateSet::Legal { candidates, .. } = generated else {
        panic!("expected answer");
    };
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.action() == CausalActionKind::Clarify)
    );
}

#[test]
fn approval_only_path_carries_concrete_operation_scope() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "unmatched request");
    let mut sources = CatalogSources::default();
    sources.mcp.push(McpCapabilitySpec {
        server: "prx_server".into(),
        tool: "write".into(),
        state_digest: [1; 32],
        spec: ToolSpec {
            name: "mcp__prx_server__write".into(),
            description: "Write remote data".into(),
            parameters: json!({"type": "object", "properties": {}}),
        },
    });
    let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([(
            CapabilityTarget::Mcp,
            (
                CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                },
                1,
            ),
        )]),
    };
    let cost = fixture_cost_for_catalog(&catalog, cost);
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Supervised;
    let calls_seen = BTreeSet::<[u8; 32]>::new();
    let calls = BTreeMap::new();
    let mut gates = GateInputs {
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
        replayed_non_idempotent: &calls_seen,
        retry_limit: Some(2),
    };
    let view = catalog.view(&CatalogScope::default());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(
        matches!(&generated, CandidateSet::NoLegalPath { .. }) && !offers_approval(&generated),
        "a proposed capability has no action-bound approval scope"
    );
    let calls_with = BTreeMap::from([(
        CandidateId::new(CausalActionKind::Discover, CapabilityTarget::Mcp, None),
        [3; 32],
    )]);
    gates.call_fingerprints = &calls_with;
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(offers_approval(&generated));
    gates.model_can_chat = false;
    gates.decision_endpoint_ready = false;
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(offers_approval(&generated));
    let CandidateSet::NoLegalPath { report, .. } = generated else {
        panic!("expected fixed-template closure");
    };
    let approval_id = CandidateId::new(CausalActionKind::RequestApproval, CapabilityTarget::Mcp, None);
    assert_eq!(report.outcomes.get(&approval_id), Some(&GateOutcome::Passed));
    gates.model_can_chat = true;
    gates.decision_endpoint_ready = true;
    let mut restricted = state.trusted.parts().clone();
    restricted.delivery.approval_channel = false;
    state.trusted = TrustedState::from_runtime(restricted);
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(matches!(&generated, CandidateSet::NoLegalPath { .. }) && !offers_approval(&generated));
    let mut restored = state.trusted.parts().clone();
    restored.delivery.approval_channel = true;
    state.trusted = TrustedState::from_runtime(restored);
    gates.model_can_call_tools = false;
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(matches!(&generated, CandidateSet::NoLegalPath { .. }) && !offers_approval(&generated));
    gates.model_can_call_tools = true;
    let mut legal_cost = cost.clone();
    legal_cost.estimates.insert(
        CapabilityTarget::None,
        (
            CostEstimate {
                tokens: 1,
                micro_usd: 0,
            },
            1,
        ),
    );
    let legal_policy = CandidatePolicy {
        cost_model: &legal_cost,
        ..policy
    };
    let generated = CausalCandidateGenerator::generate(&state, &view, &legal_policy, &gates).unwrap();
    let CandidateSet::Legal {
        approval_scopes,
        candidates,
        ..
    } = generated
    else {
        panic!("expected legal answer");
    };
    let request = candidates
        .iter()
        .find(|candidate| candidate.action() == CausalActionKind::RequestApproval)
        .unwrap();
    let scopes = approval_scopes.get(request.candidate_id()).unwrap();
    assert!(
        scopes
            .iter()
            .any(|pending| pending.candidate.target() == CapabilityTarget::Mcp
                && !pending.op_ids.is_empty()
                && !pending.candidate.required_capabilities().is_empty())
    );

    // Exact mention also proposes UseTool. The same argumentless turn
    // must not publish a grant for either that call or discovery.
    gates.call_fingerprints = &calls;
    state.routing_input = RawRoutingInput::new("mcp__prx_server__write".into());
    let generated = CausalCandidateGenerator::generate(&state, &view, &legal_policy, &gates).unwrap();
    let CandidateSet::Legal {
        approval_scopes,
        candidates,
        ..
    } = generated
    else {
        panic!("expected a terminal answer path");
    };
    assert!(approval_scopes.is_empty());
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.action() != CausalActionKind::RequestApproval)
    );
}

#[test]
fn explicit_delegator_selects_only_requested_entrypoint_and_keeps_hard_gates() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "review project without a tool-name mention");
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(ProbeTool("delegate")),
        Box::new(ProbeTool("sessions_spawn")),
        Box::new(ProbeTool("file_read")),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: [
                CapabilityTarget::Session,
                CapabilityTarget::NativeTool,
                CapabilityTarget::None,
            ]
            .into_iter()
            .map(|family| {
                (
                    family,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                )
            })
            .collect(),
        },
    );
    let mut policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: true,
        delegation_target: Some("sessions_spawn"),
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let security = SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Full,
        ..SecurityPolicy::default()
    };
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
    let mut gates = GateInputs {
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let view = catalog.view(&CatalogScope::default());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        ..
    } = generated
    else {
        panic!("configured delegator must have a legal candidate");
    };
    let delegate = candidates
        .iter()
        .find(|candidate| candidate.action() == CausalActionKind::Delegate)
        .unwrap();
    assert_eq!(delegate.required_capabilities().len(), 1);
    let entry = view
        .entry_any(&delegate.required_capabilities().first().unwrap().id)
        .unwrap();
    assert_eq!(entry.public_name, "sessions_spawn");
    assert!(exact_mentions.contains(delegate.candidate_id()));
    assert!(candidates.iter().all(|candidate| matches!(
        candidate.action(),
        CausalActionKind::Delegate
            | CausalActionKind::Clarify
            | CausalActionKind::Refuse
            | CausalActionKind::RequestApproval
    )));
    for missing_target in [false, true] {
        gates.model_can_call_tools = missing_target;
        policy.delegation_target = Some(if missing_target {
            "prx_missing"
        } else {
            "sessions_spawn"
        });
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let candidates = match generated {
            CandidateSet::Legal { candidates, .. } => candidates,
            CandidateSet::NoLegalPath { terminals, .. } => terminals,
        };
        assert!(
            candidates
                .iter()
                .all(|candidate| matches!(candidate.action(), CausalActionKind::Clarify | CausalActionKind::Refuse))
        );
    }

    gates.model_can_call_tools = true;
    policy.delegation_target = None;
    state.routing_input = RawRoutingInput::new("use sessions_spawn for this task".into());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        ..
    } = generated
    else {
        panic!("named delegator must have a legal candidate");
    };
    let delegate = candidates
        .iter()
        .find(|candidate| candidate.action() == CausalActionKind::Delegate)
        .unwrap();
    let entry = view
        .entry_any(&delegate.required_capabilities().first().unwrap().id)
        .unwrap();
    assert_eq!(entry.public_name, "sessions_spawn");
    assert!(exact_mentions.contains(delegate.candidate_id()));
}

#[test]
fn delegate_without_an_action_scope_cannot_offer_approval() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "neutral request");
    let mut sources = CatalogSources::default();
    sources.mcp.push(McpCapabilitySpec {
        server: "prx_service".into(),
        tool: "delegate".into(),
        state_digest: [2; 32],
        spec: ToolSpec {
            name: "delegate".into(),
            description: "Delegate a task".into(),
            parameters: json!({"type":"object","properties":{}}),
        },
    });
    let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([(
            CapabilityTarget::Session,
            (
                CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                },
                1,
            ),
        )]),
    };
    let cost = fixture_cost_for_catalog(&catalog, cost);
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: false,
        may_delegate: true,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Supervised;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let view = catalog.view(&CatalogScope::default());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    assert!(!offers_approval(&generated));
    let CandidateSet::NoLegalPath { report, .. } = generated else {
        panic!("expected a typed terminal path");
    };
    assert!(matches!(
        report.outcomes.get(&CandidateId::new(
            CausalActionKind::Delegate,
            CapabilityTarget::Session,
            None
        )),
        Some(GateOutcome::ApprovalOnly { .. })
    ));
}

struct ProbeTool(&'static str);

struct MemoryProbeTool(&'static str);

#[async_trait]
impl Tool for MemoryProbeTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "PRX memory fixture"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        &[crate::tools::traits::ToolCategory::Memory]
    }
    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

#[test]
fn selected_read_candidates_require_a_representable_exact_source_query() {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(MemoryProbeTool("memory_search")),
        Box::new(ProbeTool("image_info")),
        Box::new(ProbeTool("file_read")),
    ];
    let surface = SurfacePolicy {
        allowed_tools: None,
        direct_names: BTreeSet::from(["file_read".to_owned()]),
        ..SurfacePolicy::default()
    };
    let catalog =
        CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).expect("read-query boundary catalog");
    let view = catalog.view(&CatalogScope::default());
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: [
                CapabilityTarget::Memory,
                CapabilityTarget::NativeTool,
                CapabilityTarget::None,
            ]
            .into_iter()
            .map(|family| {
                (
                    family,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                )
            })
            .collect(),
        },
    );
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "prx_user",
        sender: "prx_user",
        policy_channel: "cli",
        chat_type: "private",
        model_ref: "ollama:prx-local-model",
        model_can_chat: true,
        model_can_call_tools: true,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &calls,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &history,
        retry_limit: Some(0),
    };
    let mut expected_supported = None;
    let mut expected_other = None;
    for (query, supported) in [
        (String::new(), true),
        ("neutral query".to_owned(), true),
        ("a".repeat(1024), true),
        ("\u{e9}".repeat(512), true),
        ("a".repeat(1025), false),
        (format!("{}a", "\u{e9}".repeat(512)), false),
        ("a".repeat(16384), false),
    ] {
        let mut state = fixtures::state(&key, &query);
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = view.generation();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.resolved_model = Arc::from("ollama:prx-local-model");
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        trusted.constraints.clear();
        state.trusted = TrustedState::from_runtime(trusted);
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates)
            .expect("normal gated generation for valid raw routing state");
        let CandidateSet::Legal { candidates, report, .. } = generated else {
            panic!("bounded tools and Answer must remain legal");
        };
        let (reads, other): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|candidate| {
            matches!(
                candidate.action(),
                CausalActionKind::Retrieve | CausalActionKind::Discover
            )
        });
        assert!(
            other
                .iter()
                .any(|candidate| candidate.action() == CausalActionKind::UseTool)
        );
        assert!(
            other
                .iter()
                .any(|candidate| candidate.action() == CausalActionKind::Answer)
        );
        for candidate in other.iter().chain(&reads) {
            assert_eq!(
                report.outcomes.get(candidate.candidate_id()),
                Some(&GateOutcome::Passed)
            );
        }
        if supported {
            assert!(
                reads
                    .iter()
                    .any(|candidate| candidate.action() == CausalActionKind::Retrieve)
            );
            assert!(
                reads
                    .iter()
                    .any(|candidate| candidate.action() == CausalActionKind::Discover)
            );
            if let Some(expected) = &expected_supported {
                assert_eq!(&reads, expected, "full refs and order must remain stable");
            } else {
                expected_supported = Some(reads);
            }
        } else {
            assert!(
                reads.is_empty(),
                "unrepresentable exact source query must not advertise local Reads"
            );
        }
        if let Some(expected) = &expected_other {
            assert_eq!(&other, expected, "UseTool and terminal bodies must remain stable");
        } else {
            expected_other = Some(other);
        }
    }
}

#[test]
fn retrieve_admits_only_exact_read_only_memory_search_on_default_searchable_surface() {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(MemoryProbeTool("memory_forget")),
        Box::new(MemoryProbeTool("memory_search")),
        Box::new(MemoryProbeTool("memory_store")),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
        .expect("memory catalog");
    let view = catalog.view(&CatalogScope::default());
    let eligible: Vec<_> = view
        .entries()
        .iter()
        .filter(|entry| is_memory_retrieval_source(entry))
        .collect();
    assert_eq!(eligible.len(), 1);
    let search = eligible.first().expect("one read-only search source");
    assert_eq!(search.public_name, "memory_search");
    assert!(matches!(search.exposure, Exposure::Searchable));
    assert_eq!(search.effect, SideEffectClass::ReadOnly);
    for name in ["memory_forget", "memory_store"] {
        let write = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == name)
            .expect("write fixture");
        assert_eq!(write.effect, SideEffectClass::LocalWrite);
    }
    let document_registry: Vec<Box<dyn Tool>> = vec![Box::new(MemoryProbeTool("document_search"))];
    let document_catalog = CapabilityCatalog::build(
        &document_registry,
        &CatalogSources::default(),
        &SurfacePolicy::default(),
    )
    .expect("document fixture");
    assert!(
        document_catalog
            .view(&CatalogScope::default())
            .entries()
            .iter()
            .all(|entry| !is_memory_retrieval_source(entry))
    );
    let document_view = document_catalog.view(&CatalogScope::default());
    let document = document_view
        .entries()
        .iter()
        .copied()
        .find(|entry| entry.public_name == "document_search")
        .expect("document anchor");
    assert!(is_document_retrieval_source(&document));
    let mut changed = (*document).clone();
    changed.effect = SideEffectClass::LocalWrite;
    assert!(!is_document_retrieval_source(&changed));
    changed = (*document).clone();
    changed.root = "document_import".to_owned();
    assert!(!is_document_retrieval_source(&changed));
    let mut state = fixtures::state(&key, "prx memory question");
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = view.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    trusted.constraints.clear();
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([
                (
                    CapabilityTarget::Memory,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
                (
                    CapabilityTarget::None,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
            ]),
        },
    );
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
    let gates = GateInputs {
        policy: &security,
        owner_id: "owner-1",
        sender: "owner-1",
        policy_channel: "cli",
        chat_type: "private",
        model_ref: "anthropic:claude",
        model_can_chat: true,
        model_can_call_tools: true,
        decision_endpoint_ready: true,
        child_scope: None,
        verified_approval_calls: BTreeMap::new(),
        call_fingerprints: &calls,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: &history,
        retry_limit: Some(0),
    };
    let write = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "memory_forget")
        .expect("first write source");
    assert_eq!(
        view.entries().first().expect("ordered catalog").public_name,
        "memory_forget"
    );
    let wrongly_labeled = from_entry(
        CausalActionKind::Retrieve,
        CapabilityTarget::Memory,
        write,
        &policy,
        None,
    );
    let old_gate =
        HardGates::apply(&state, &view, std::slice::from_ref(&wrongly_labeled), &gates).expect("permissive gate");
    assert_eq!(
        old_gate.outcomes.get(wrongly_labeled.candidate_id()),
        Some(&GateOutcome::Passed),
        "the old broad Memory loop could select a write tool as Retrieve"
    );
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    let CandidateSet::Legal { candidates, .. } = generated else {
        panic!("safe Memory retrieval should be legal: {generated:?}");
    };
    let retrieves: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.action() == CausalActionKind::Retrieve)
        .collect();
    assert_eq!(retrieves.len(), 1);
    let selected = retrieves.first().expect("one selected Retrieve");
    let source = selected.required_capabilities().first().expect("Retrieve source");
    assert_eq!(source.id, search.id);
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Read local evidence"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

#[test]
fn read_only_family_selection_skips_write_probe_before_pricing() {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_write"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let entry = *view.entries().first().expect("one write capability");
    assert!(entry.effect > SideEffectClass::ReadOnly);
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::NativeTool,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        },
    );
    let counting_cost = CountingCost {
        inner: &cost,
        entries: AtomicUsize::new(0),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 3,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &counting_cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let mut state = fixtures::state(&key, "neutral request");
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    state.trusted = TrustedState::from_runtime(trusted.clone());
    assert_eq!(
        HardGates::check_one(
            &state,
            &view,
            &from_entry(CausalActionKind::UseTool, entry.family, entry, &policy, None),
            &gates,
        ),
        GateOutcome::Blocked {
            gate: 9,
            reason_code: "explicit_restriction",
        }
    );
    counting_cost.entries.store(0, Ordering::Relaxed);
    let skipped = select_family_candidate(
        CausalActionKind::UseTool,
        entry.family,
        [entry],
        false,
        &state,
        &view,
        &policy,
        &gates,
        &mut BTreeMap::new(),
    )
    .unwrap();
    assert!(skipped.is_none());
    assert_eq!(counting_cost.entries.load(Ordering::Relaxed), 0);

    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    state.trusted = TrustedState::from_runtime(trusted);
    let considered = select_family_candidate(
        CausalActionKind::UseTool,
        entry.family,
        [entry],
        false,
        &state,
        &view,
        &policy,
        &gates,
        &mut BTreeMap::new(),
    )
    .unwrap();
    assert!(considered.is_some());
    assert_eq!(counting_cost.entries.load(Ordering::Relaxed), 1);
}

/// Run `check` against one `shell` capability under full autonomy with the
/// given execution estimate and a decision-overhead budget of `budget_ms`.
fn with_shell_gate_fixture(
    estimate: Option<(CostEstimate, u32)>,
    budget_ms: u64,
    check: impl FnOnce(&CausalState, &CatalogView<'_>, &CandidatePolicy<'_>, &GateInputs<'_>, &CapabilityEntry),
) {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("shell"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let entry = *view.entries().first().expect("shell capability");
    assert_eq!(entry.family, CapabilityTarget::Shell);
    let mut per_capability = BTreeMap::new();
    if let Some(estimate) = estimate {
        per_capability.insert(
            CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            },
            estimate,
        );
    }
    let cost = CapabilityCostModel {
        per_capability,
        estimates: BTreeMap::new(),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let mut state = fixtures::state(&key, "neutral request");
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    trusted.budgets = crate::tools::capability::turn_state::TurnBudgets {
        tokens: 1_000,
        latency_ms: budget_ms,
        micro_usd: 1_000,
    };
    state.trusted = TrustedState::from_runtime(trusted);
    check(&state, &view, &policy, &gates, entry);
}

#[test]
fn a_tool_candidate_passes_g6_after_the_decision_spent_most_of_its_budget() {
    let priced = CostEstimate {
        tokens: 10,
        micro_usd: 1,
    };
    // 10 ms of decision budget left and an observed p50 of 5 s.
    with_shell_gate_fixture(Some((priced, 5_000)), 10, |state, view, policy, gates, entry| {
        let candidate = from_entry(CausalActionKind::UseTool, entry.family, entry, policy, None);
        assert_eq!(
            candidate.estimated_latency_ms(),
            Some(5_000),
            "execution latency is kept for scoring"
        );
        assert_eq!(candidate.decision_overhead_ms(), Some(0));
        assert_eq!(
            HardGates::check_one(state, view, &candidate, gates),
            GateOutcome::Passed
        );
        // Control: a candidate whose overhead is the execution estimate is
        // still refused, so G6 keeps enforcing the overhead it is given.
        let slow = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: candidate.required_capabilities().to_vec(),
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(priced),
                estimated_latency_ms: Some(5_000),
            },
        );
        assert_eq!(
            HardGates::check_one(state, view, &slow, gates),
            GateOutcome::Blocked {
                gate: 6,
                reason_code: "budget"
            }
        );
    });
    // An unknown estimate (an unpriced model) is still not free.
    with_shell_gate_fixture(None, 10, |state, view, policy, gates, entry| {
        let candidate = from_entry(CausalActionKind::UseTool, entry.family, entry, policy, None);
        assert_eq!(candidate.decision_overhead_ms(), None);
        assert_eq!(
            HardGates::check_one(state, view, &candidate, gates),
            GateOutcome::Blocked {
                gate: 6,
                reason_code: "unpriced_model"
            }
        );
    });
}

#[test]
fn a_probe_rejected_by_a_gate_is_traced_with_its_gate_and_reason() {
    // tracing callsite interest is process-global. Keep this log assertion
    // isolated from concurrent tests that install or rebuild subscribers.
    const PROBE: &str = "PRX_CANDIDATE_GATE_TRACE_PROBE";
    if std::env::var_os(PROBE).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "causal_tree::hard_gates_probe::legacy_generator_tests::a_probe_rejected_by_a_gate_is_traced_with_its_gate_and_reason",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated trace probe failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("candidates.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::fs::File::create(&path).unwrap())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        // No estimate: G6 rejects the shell probe, which used to vanish.
        with_shell_gate_fixture(None, 500, |state, view, policy, gates, entry| {
            let selected = select_family_candidate(
                CausalActionKind::UseTool,
                entry.family,
                [entry],
                false,
                state,
                view,
                policy,
                gates,
                &mut BTreeMap::new(),
            )
            .unwrap();
            assert!(selected.is_none());
        });
    });
    let log = std::fs::read_to_string(path).unwrap();
    assert!(log.contains("candidate rejected by a hard gate"), "{log}");
    assert!(log.contains("use_tool-shell"), "{log}");
    assert!(log.contains("gate=6"), "{log}");
    assert!(log.contains("unpriced_model"), "{log}");
}

#[test]
fn exact_cost_binding_cannot_price_another_same_family_capability() {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let entries = view.entries();
    assert_eq!(entries.len(), 2);
    let first = *entries.first().expect("first fixture capability");
    let second = *entries.get(1).expect("second fixture capability");
    assert_eq!(first.family, second.family);
    let first_ref = CapabilityRef {
        id: first.id.clone(),
        fingerprint: first.fingerprint,
        generation: first.generation,
    };
    let exact = CostEstimate {
        tokens: 17,
        micro_usd: 3,
    };
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::from([(first_ref, (exact, 77))]),
        estimates: BTreeMap::from([(
            first.family,
            (
                CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                },
                1,
            ),
        )]),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let priced = from_entry(CausalActionKind::UseTool, first.family, first, &policy, None);
    let other = from_entry(CausalActionKind::UseTool, second.family, second, &policy, None);
    assert_eq!(priced.estimated_cost(), Some(exact));
    assert_eq!(priced.estimated_latency_ms(), Some(77));
    assert_eq!(priced.decision_overhead_ms(), Some(0));
    assert_eq!(other.estimated_cost(), None);
    assert_eq!(other.estimated_latency_ms(), None);
    let empty_exact = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: cost.estimates.clone(),
    };
    let empty_policy = CandidatePolicy {
        cost_model: &empty_exact,
        ..policy
    };
    let unpriced = from_entry(CausalActionKind::UseTool, first.family, first, &empty_policy, None);
    assert_eq!(
        unpriced.estimated_cost(),
        None,
        "a family estimate never authorizes a capability"
    );
}

#[test]
fn exact_mention_dominates_same_entry_family_and_unmatched_input_keeps_fallback() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "first unrelated request");
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
    let catalog = CapabilityCatalog::build(
        &registry,
        &CatalogSources::default(),
        &SurfacePolicy {
            direct_names: BTreeSet::from(["prx_read".to_owned()]),
            ..SurfacePolicy::default()
        },
    )
    .unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([
            (
                CapabilityTarget::None,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            ),
            (
                CapabilityTarget::NativeTool,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            ),
        ]),
    };
    let cost = fixture_cost_for_catalog(&catalog, cost);
    let counting_cost = CountingCost {
        inner: &cost,
        entries: AtomicUsize::new(0),
    };
    let view = catalog.view(&CatalogScope::default());
    let discovered_all: BTreeSet<_> = view
        .entries()
        .iter()
        .map(|entry| CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        })
        .collect();
    let discovered_all = DiscoveredRefs::from_refs_for_test(discovered_all);
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &counting_cost,
        discovered_refs: &discovered_all,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls_seen = BTreeSet::<[u8; 32]>::new();
    let calls = BTreeMap::new();
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
        replayed_non_idempotent: &calls_seen,
        retry_limit: Some(2),
    };
    let CandidateSet::Legal { candidates, .. } =
        CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("baseline")
    else {
        panic!("legal baseline");
    };
    let baseline = candidates
        .iter()
        .find(|candidate| candidate.action() == CausalActionKind::UseTool)
        .expect("unmatched input has family fallback");
    let fallback_id = &baseline.required_capabilities().first().expect("fallback entry").id;
    let first = view
        .entries()
        .iter()
        .find(|entry| &entry.id == fallback_id)
        .expect("fallback catalog entry");
    let other = view
        .entries()
        .iter()
        .find(|entry| &entry.id != fallback_id)
        .expect("different same-family entry");
    assert!(matches!(first.exposure, Exposure::Direct));
    let reference = CapabilityRef {
        id: other.id.clone(),
        fingerprint: other.fingerprint,
        generation: other.generation,
    };
    for stale in [false, true] {
        let mut reference = reference.clone();
        if stale {
            reference.fingerprint.0[0] ^= 1;
        }
        let selected = DiscoveredRefs::from_refs_for_test(BTreeSet::from([reference]));
        let selected_policy = CandidatePolicy {
            discovered_refs: &selected,
            ..policy
        };
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &selected_policy, &gates).unwrap()
        else {
            panic!("legal discovered fixture")
        };
        let represented = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::UseTool)
            .unwrap();
        let expected = if stale { first } else { other };
        assert_eq!(
            represented.required_capabilities().first().unwrap().id,
            expected.id,
            "current selected metadata outranks Direct fallback; stale refs cannot do so"
        );
    }
    for (input, blocked, expected_exact, expected_tools) in [
        (format!("use {}", first.public_name), false, true, 1),
        ("unrelated request".to_owned(), false, false, 1),
        (format!("use {}", other.public_name), false, true, 2),
        (format!("use {}", first.public_name), true, false, 1),
    ] {
        state.routing_input = RawRoutingInput::new(input);
        let mut trusted = state.trusted.parts().clone();
        trusted.exclusions.clear();
        if blocked {
            trusted.exclusions.push(first.public_name.clone().into());
        }
        state.trusted = TrustedState::from_runtime(trusted);
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            report,
            ..
        } = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("legal fixture")
        else {
            panic!("expected admitted capability");
        };
        let tools = candidates
            .iter()
            .filter(|candidate| candidate.action() == CausalActionKind::UseTool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), expected_tools);
        assert_eq!(!exact_mentions.is_empty(), expected_exact);
        for entry in [first, other] {
            let count = tools
                .iter()
                .filter(|candidate| {
                    candidate
                        .required_capabilities()
                        .iter()
                        .any(|reference| reference.id == entry.id)
                })
                .count();
            assert!(count <= 1, "exact and family cannot duplicate an entry");
        }
        if blocked {
            assert!(
                report.outcomes.values().any(|outcome| matches!(
                    outcome,
                    GateOutcome::Blocked {
                        gate: 2,
                        reason_code: "excluded"
                    }
                )),
                "the discarded exact candidate keeps its refusal evidence"
            );
            assert!(
                tools.iter().all(|candidate| candidate
                    .required_capabilities()
                    .iter()
                    .all(|reference| reference.id != first.id)),
                "blocked exact entry remains excluded"
            );
            assert!(
                tools.iter().any(|candidate| candidate
                    .required_capabilities()
                    .iter()
                    .any(|reference| reference.id == other.id)),
                "different fallback survives blocked exact mention"
            );
        } else {
            assert!(
                tools.iter().any(|candidate| candidate
                    .required_capabilities()
                    .iter()
                    .any(|reference| reference.id == first.id)),
                "original family entry remains represented"
            );
        }
    }
}

#[test]
fn unrelated_user_text_preserves_candidates_and_exact_name_adds_only_its_pair() {
    let key = fixtures::key();
    let mut state = fixtures::state(&key, "first unrelated request");
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    state.trusted = TrustedState::from_runtime(trusted);
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
        estimates: BTreeMap::from([
            (
                CapabilityTarget::None,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            ),
            (
                CapabilityTarget::NativeTool,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            ),
        ]),
    };
    let cost = fixture_cost_for_catalog(&catalog, cost);
    let counting_cost = CountingCost {
        inner: &cost,
        entries: AtomicUsize::new(0),
    };
    let view = catalog.view(&CatalogScope::default());
    let discovered_all: BTreeSet<_> = view
        .entries()
        .iter()
        .map(|entry| CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        })
        .collect();
    let discovered_all = DiscoveredRefs::from_refs_for_test(discovered_all);
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &counting_cost,
        discovered_refs: &discovered_all,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls_seen = BTreeSet::<[u8; 32]>::new();
    let calls = BTreeMap::new();
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
        replayed_non_idempotent: &calls_seen,
        retry_limit: Some(2),
    };
    let ids = |state: &CausalState| match CausalCandidateGenerator::generate(state, &view, &policy, &gates).unwrap() {
        CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } => (
            candidates
                .iter()
                .map(|candidate| candidate.candidate_id().clone())
                .collect::<Vec<_>>(),
            exact_mentions,
        ),
        CandidateSet::NoLegalPath { .. } => panic!("expected legal candidates"),
    };
    let baseline = ids(&state);
    assert!(
        counting_cost.entries.load(Ordering::Relaxed) > 0,
        "ordinary candidate generation must request exact entry estimates"
    );
    let search_entry = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "prx_search")
        .expect("second searchable fixture");
    let search_ref = CapabilityRef {
        id: search_entry.id.clone(),
        fingerprint: search_entry.fingerprint,
        generation: search_entry.generation,
    };
    let only_search = DiscoveredRefs::from_refs_for_test(BTreeSet::from([search_ref]));
    let prior_policy = CandidatePolicy {
        discovered_refs: &only_search,
        ..policy
    };
    let generic_tool = |policy: &CandidatePolicy<'_>| {
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, policy, &gates).expect("legal candidates")
        else {
            panic!("expected a legal generic tool");
        };
        candidates
            .into_iter()
            .find(|candidate| {
                candidate.action() == CausalActionKind::UseTool && candidate.target() == CapabilityTarget::NativeTool
            })
            .expect("one generic native tool")
    };
    let prior_generic = generic_tool(&prior_policy);
    let discovered_generic = generic_tool(&policy);
    assert_eq!(prior_generic.candidate_id(), discovered_generic.candidate_id());
    assert_ne!(
        prior_generic.required_capabilities(),
        discovered_generic.required_capabilities(),
        "same-family discovery can change the exact bound ref without changing the candidate ID"
    );
    let mut prior_state = state.clone();
    prior_state.derived =
        crate::decision_engine::state::DerivedState::new(vec![crate::decision_engine::state::DerivedCandidate {
            candidate: prior_generic,
            summary: Arc::from("fixture"),
            exact_mention: false,
            available: true,
            effective_approval_required: None,
        }])
        .expect("one prior candidate");
    let mut discovered_state = state.clone();
    discovered_state.derived =
        crate::decision_engine::state::DerivedState::new(vec![crate::decision_engine::state::DerivedCandidate {
            candidate: discovered_generic,
            summary: Arc::from("fixture"),
            exact_mention: false,
            available: true,
            effective_approval_required: None,
        }])
        .expect("one discovered candidate");
    assert_ne!(prior_state.derived.candidates(), discovered_state.derived.candidates());
    let before_discovery = CandidatePolicy {
        discovered_refs: &EMPTY_DISCOVERED_REFS,
        ..policy
    };
    let CandidateSet::Legal { candidates, .. } =
        CausalCandidateGenerator::generate(&state, &view, &before_discovery, &gates).unwrap()
    else {
        panic!("expected legal candidates before discovery");
    };
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.action() != CausalActionKind::UseTool),
        "searchable tools require exact mention or turn-scoped discovery"
    );
    let discovered_entry = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "prx_read")
        .unwrap();
    let stale = DiscoveredRefs::from_refs_for_test(BTreeSet::from([CapabilityRef {
        id: discovered_entry.id.clone(),
        fingerprint: crate::tools::capability::contract::SchemaFingerprint([0; 32]),
        generation: discovered_entry.generation,
    }]));
    let stale_policy = CandidatePolicy {
        discovered_refs: &stale,
        ..policy
    };
    let CandidateSet::Legal { candidates, .. } =
        CausalCandidateGenerator::generate(&state, &view, &stale_policy, &gates).unwrap()
    else {
        panic!("expected legal candidates with stale reference");
    };
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.action() != CausalActionKind::UseTool)
    );
    let discovered = DiscoveredRefs::from_refs_for_test(BTreeSet::from([CapabilityRef {
        id: discovered_entry.id.clone(),
        fingerprint: discovered_entry.fingerprint,
        generation: discovered_entry.generation,
    }]));
    let after_discovery = CandidatePolicy {
        discovered_refs: &discovered,
        ..policy
    };
    let CandidateSet::Legal { candidates, .. } =
        CausalCandidateGenerator::generate(&state, &view, &after_discovery, &gates).unwrap()
    else {
        panic!("expected legal candidates after discovery");
    };
    assert!(candidates.iter().any(|candidate| {
        candidate.action() == CausalActionKind::UseTool
            && candidate
                .required_capabilities()
                .iter()
                .any(|reference| reference.id == discovered_entry.id)
    }));
    state.routing_input = RawRoutingInput::new("different unrelated request".into());
    assert_eq!(ids(&state), baseline);
    state.routing_input = RawRoutingInput::new("use prx_read".into());
    let exact = ids(&state);
    assert_eq!(exact.1.len(), 1);
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        ..
    } = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("exact mention candidates")
    else {
        panic!("expected admitted exact mention");
    };
    let exact_candidate = candidates
        .iter()
        .find(|candidate| exact_mentions.contains(candidate.candidate_id()))
        .expect("exact mention retained");
    let exact_ref = exact_candidate
        .required_capabilities()
        .first()
        .expect("exact entry reference");
    assert!(
        candidates.iter().all(|candidate| {
            candidate.candidate_id() == exact_candidate.candidate_id()
                || !candidate
                    .required_capabilities()
                    .iter()
                    .any(|reference| reference.id == exact_ref.id)
                || candidate.action() != CausalActionKind::UseTool
        }),
        "no family duplicate of the exact entry"
    );
    assert!(
        baseline.0.iter().any(|id| id.as_str() == "use_tool-native_tool"),
        "unmatched input retains its family fallback"
    );
    let mut trusted = state.trusted.parts().clone();
    trusted.exclusions.push("prx_read".into());
    state.trusted = TrustedState::from_runtime(trusted);
    state.routing_input = RawRoutingInput::new("unrelated request".into());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    let CandidateSet::Legal { candidates, .. } = generated else {
        panic!("expected legal fallback");
    };
    let selected = candidates
        .iter()
        .find(|candidate| {
            candidate.action() == CausalActionKind::UseTool && candidate.target() == CapabilityTarget::NativeTool
        })
        .unwrap();
    assert_eq!(
        selected.required_capabilities().first().unwrap().id.as_str(),
        view.entries()
            .iter()
            .find(|entry| entry.public_name == "prx_search")
            .unwrap()
            .id
            .as_str()
    );
    state.routing_input = RawRoutingInput::new("use prx_read and prx_search".into());
    let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        ..
    } = generated
    else {
        panic!("expected legal exact mention");
    };
    assert_eq!(exact_mentions.len(), 1);
    let exact = candidates
        .iter()
        .find(|candidate| exact_mentions.contains(candidate.candidate_id()))
        .unwrap();
    assert_eq!(
        exact.required_capabilities().first().unwrap().id.as_str(),
        selected.required_capabilities().first().unwrap().id.as_str()
    );

    for cap in 0..3 {
        let invalid = CandidatePolicy {
            max_candidates: cap,
            ..policy
        };
        assert!(matches!(
            CausalCandidateGenerator::generate(&state, &view, &invalid, &gates),
            Err(GateError::InvalidCandidateLimit)
        ));
    }
    let small = CandidatePolicy {
        max_candidates: 3,
        ..policy
    };
    let generated = CausalCandidateGenerator::generate(&state, &view, &small, &gates).unwrap();
    assert!(matches!(generated, CandidateSet::Legal { candidates, .. } if candidates.len() <= 3));
}
// Preserve the original transient probe algorithm as the differential oracle.
fn unfiltered_family_candidate<'a>(
    action: CausalActionKind,
    family: CapabilityTarget,
    entries: impl IntoIterator<Item = &'a CapabilityEntry>,
    state: &CausalState,
    catalog: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    gates: &GateInputs<'_>,
) -> Result<Option<CausalCandidate>, GateError> {
    let mut approval_only = None;
    for entry in entries {
        // G9 will reject a write effect under a read-only turn. This
        // transient probe cannot become Passed or ApprovalOnly, so avoid
        // constructing and pricing it during family fallback selection.
        if state.trusted.parts().side_effect_mode == SideEffectMode::ReadOnly
            && entry.effect > SideEffectClass::ReadOnly
        {
            continue;
        }
        let candidate = from_entry(action, family, entry, policy, None);
        let outcome = HardGates::check_one(state, catalog, &candidate, gates);
        match outcome {
            GateOutcome::Passed => return Ok(Some(candidate)),
            GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty() && approval_only.is_none() => {
                approval_only = Some(candidate);
            }
            _ => {}
        }
    }
    Ok(approval_only)
}

#[test]
fn borrowed_exclusions_preserve_family_selection_reports_and_stale_layout() {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(ProbeTool("prx_first")),
        Box::new(ProbeTool("prx_second")),
        Box::new(ProbeTool("prx_third")),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::NativeTool,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        },
    );
    let counting_cost = CountingCost {
        inner: &cost,
        entries: AtomicUsize::new(0),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 3,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &counting_cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let calls = BTreeMap::new();
    let history = BTreeSet::new();
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let mut state = fixtures::state(&key, "neutral request");
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = SideEffectMode::GuardedWrite;
    state.trusted = TrustedState::from_runtime(trusted.clone());

    let entries = view.entries();
    let first = *entries.first().expect("first capability");
    let second = *entries.get(1).expect("second capability");
    let third = *entries.get(2).expect("third capability");
    let other_registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_different_layout"))];
    let other_catalog =
        CapabilityCatalog::build(&other_registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    for scenario in 0..6 {
        trusted.exclusions.clear();
        trusted.model_admission = None;
        trusted.channel = None;
        match scenario {
            0 => trusted.exclusions.push(Arc::from(first.public_name.as_str())),
            1 => trusted
                .exclusions
                .extend(entries.iter().map(|entry| Arc::from(entry.root.as_str()))),
            2 => {
                trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
                    &registry,
                    &view,
                    "prx-model",
                    &[],
                    "prx-policy",
                    &key,
                ))
            }
            3 => {
                trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
                    &other_registry,
                    &other_catalog.view(&CatalogScope::default()),
                    "prx-model",
                    &[],
                    "prx-policy",
                    &key,
                ))
            }
            4 => {}
            _ => trusted.channel = Some(Arc::from("terminal")),
        }
        let surface = SurfacePolicy {
            excluded_names: if scenario == 4 {
                BTreeSet::from([first.public_name.clone()])
            } else {
                BTreeSet::new()
            },
            ..SurfacePolicy::default()
        };
        let view = catalog.view(&CatalogScope::default()).with_surface(&surface);
        state.trusted = TrustedState::from_runtime(trusted.clone());
        counting_cost.entries.store(0, Ordering::Relaxed);
        let expected = unfiltered_family_candidate(
            CausalActionKind::UseTool,
            first.family,
            [first, second, third],
            &state,
            &view,
            &policy,
            &gates,
        )
        .unwrap();
        let reference_pricing = counting_cost.entries.load(Ordering::Relaxed);
        counting_cost.entries.store(0, Ordering::Relaxed);
        let actual = select_family_candidate(
            CausalActionKind::UseTool,
            first.family,
            [first, second, third],
            false,
            &state,
            &view,
            &policy,
            &gates,
            &mut BTreeMap::new(),
        )
        .unwrap();
        let actual_pricing = counting_cost.entries.load(Ordering::Relaxed);
        assert_eq!(
            actual.as_ref().map(CausalCandidate::candidate_id),
            expected.as_ref().map(CausalCandidate::candidate_id)
        );
        assert_eq!(
            actual.as_ref().map(CausalCandidate::required_capabilities),
            expected.as_ref().map(CausalCandidate::required_capabilities)
        );
        let mut expected_proposed = expected.into_iter().collect::<Vec<_>>();
        expected_proposed.push(terminal(CausalActionKind::Refuse, &policy));
        let mut actual_proposed = actual.into_iter().collect::<Vec<_>>();
        actual_proposed.push(terminal(CausalActionKind::Refuse, &policy));
        let expected_report = HardGates::apply(&state, &view, &expected_proposed, &gates).unwrap();
        let actual_report = HardGates::apply(&state, &view, &actual_proposed, &gates).unwrap();
        assert_eq!(actual_report.outcomes, expected_report.outcomes);
        if scenario == 3 {
            assert_eq!(
                actual_pricing, reference_pricing,
                "stale bitmap must not skip construction"
            );
            assert_eq!(
                HardGates::check_one(
                    &state,
                    &view,
                    &from_entry(CausalActionKind::UseTool, first.family, first, &policy, None),
                    &gates,
                ),
                GateOutcome::Blocked {
                    gate: 3,
                    reason_code: "unavailable_or_stale"
                }
            );
        } else {
            assert!(actual_pricing < reference_pricing, "excluded probes must avoid pricing");
        }
    }
}
#[test]
fn borrowed_prefilter_preserves_first_approval_and_later_passed() {
    let key = fixtures::key();
    let mut sources = CatalogSources::default();
    for name in ["prx_first", "prx_second"] {
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_service".into(),
            tool: name.into(),
            state_digest: [2; 32],
            spec: ToolSpec {
                name: name.into(),
                description: "Fixture capability".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
    }
    let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let cost = fixture_cost_for_catalog(
        &catalog,
        CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::Mcp,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        },
    );
    let counting_cost = CountingCost {
        inner: &cost,
        entries: AtomicUsize::new(0),
    };
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 3,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &counting_cost,
        discovered_refs: &EMPTY_DISCOVERED_REFS,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = crate::security::AutonomyLevel::Full;
    let mut calls = BTreeMap::new();
    calls.insert(
        CandidateId::new(CausalActionKind::UseTool, CapabilityTarget::Mcp, None),
        [7; 32],
    );
    let history = BTreeSet::new();
    let mut gates = GateInputs {
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
        replayed_non_idempotent: &history,
        retry_limit: Some(2),
    };
    let mut state = fixtures::state(&key, "neutral request");
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
    state.trusted = TrustedState::from_runtime(trusted.clone());

    let entries = view.entries();
    let first = *entries.first().expect("first MCP capability");
    let second = *entries.get(1).expect("second MCP capability");
    for allow_second in [false, true] {
        gates.verified_approval_calls.clear();
        if allow_second {
            gates.verified_approval_calls.insert(
                [7; 32],
                BTreeSet::from([CapabilityRef {
                    id: second.id.clone(),
                    fingerprint: second.fingerprint,
                    generation: second.generation,
                }]),
            );
        }
        let first_probe = from_entry(CausalActionKind::UseTool, CapabilityTarget::Mcp, first, &policy, None);
        assert!(
            matches!(HardGates::check_one(&state, &view, &first_probe, &gates), GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty())
        );
        let second_probe = from_entry(CausalActionKind::UseTool, CapabilityTarget::Mcp, second, &policy, None);
        let second_outcome = HardGates::check_one(&state, &view, &second_probe, &gates);
        if allow_second {
            assert_eq!(second_outcome, GateOutcome::Passed);
        } else {
            assert!(matches!(second_outcome, GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty()));
        }
        let expected = unfiltered_family_candidate(
            CausalActionKind::UseTool,
            CapabilityTarget::Mcp,
            [first, second],
            &state,
            &view,
            &policy,
            &gates,
        )
        .unwrap()
        .expect("reference selection");
        let actual = select_family_candidate(
            CausalActionKind::UseTool,
            CapabilityTarget::Mcp,
            [first, second],
            false,
            &state,
            &view,
            &policy,
            &gates,
            &mut BTreeMap::new(),
        )
        .unwrap()
        .expect("optimized selection");
        assert_eq!(actual.required_capabilities(), expected.required_capabilities());
        assert_eq!(
            actual.required_capabilities().first().expect("selected ref").id,
            if allow_second { &second.id } else { &first.id }.clone()
        );
        let expected_proposed = [expected, terminal(CausalActionKind::Refuse, &policy)];
        let actual_proposed = [actual, terminal(CausalActionKind::Refuse, &policy)];
        let expected_report = HardGates::apply(&state, &view, &expected_proposed, &gates).unwrap();
        let actual_report = HardGates::apply(&state, &view, &actual_proposed, &gates).unwrap();
        assert_eq!(actual_report.outcomes, expected_report.outcomes);
    }
}

/// Finding 76 review: only a full-name mention of a single-operation
/// approval-required entry closes or collects; bare aliases are prose and
/// multi-operation node entries keep their previous behaviour.
#[test]
fn collection_trigger_is_full_name_and_single_operation_only() {
    let temp = tempfile::tempdir().expect("test: tempdir");
    let mut sources = CatalogSources::default();
    sources.node_key = Some(crate::security::runtime_key::DecisionKey::load_or_create(temp.path()).expect("test: key"));
    sources.nodes.push(crate::tools::capability::catalog::NodeSource {
        id: "prx_node".into(),
        availability: crate::capability::CapabilityAvailability::configured("configured"),
        binding_digest: [2; 32],
    });
    sources.mcp.push(McpCapabilitySpec {
        server: "prx_server".into(),
        tool: "write".into(),
        state_digest: [3; 32],
        spec: ToolSpec {
            name: "mcp__prx_server__write".into(),
            description: "Write remote data".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
    });
    let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).expect("test: catalog");
    let view = catalog.view(&CatalogScope::default());
    let write = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "mcp__prx_server__write")
        .expect("test: mcp entry");
    let node = view
        .entries()
        .iter()
        .find(|entry| entry.family == CapabilityTarget::Shell && entry.op_ids.len() == 2)
        .expect("test: node entry");
    assert!(collectable_approval_entry(write));
    assert!(
        !collectable_approval_entry(node),
        "two-operation nodes are not collectable"
    );
    assert!(
        exact_mention("write a poem about spring", write),
        "the bare alias is an exact-mention term"
    );
    assert!(!names_entry("write a poem about spring", write));
    assert!(!names_entry("search the docs for write", write));
    assert!(names_entry("please call mcp__prx_server__write now", write));
}

/// Only the built-in native `skills_manage` is collectable by name; an
/// entry of the same name from another source is not.
#[test]
fn only_the_built_in_skills_manage_entry_is_collectable() {
    use crate::tools::capability::catalog::ExecutionTarget;
    let mut sources = CatalogSources::default();
    sources.mcp.push(McpCapabilitySpec {
        server: "prx_server".into(),
        tool: "write".into(),
        state_digest: [3; 32],
        spec: ToolSpec {
            name: "mcp__prx_server__write".into(),
            description: "Write remote data".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
    });
    let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).expect("test: catalog");
    let view = catalog.view(&CatalogScope::default());
    let mut builtin = view
        .entries()
        .iter()
        .find(|entry| entry.public_name == "mcp__prx_server__write")
        .map(|entry| CapabilityEntry::clone(entry))
        .expect("test: mcp entry");
    // The entry as the native registry publishes it.
    builtin.public_name = "skills_manage".into();
    builtin.root = "skills_manage".into();
    builtin.family = CapabilityTarget::NativeTool;
    builtin.target = ExecutionTarget::Tool("skills_manage".into());
    builtin.op_ids = Vec::new();
    assert!(builtin.collects_exact_call());
    assert!(collectable_approval_entry(&builtin));
    assert!(builtin.names_collectable_action("run lifecycle_import_executable"));
    assert!(!builtin.names_collectable_action("run lifecycle_status"));

    let mut mcp = builtin.clone();
    mcp.family = CapabilityTarget::Mcp;
    let mut plugin = builtin.clone();
    plugin.root = "wasm_plugin_call".into();
    let mut skill = builtin;
    skill.family = CapabilityTarget::Skill;
    skill.target = ExecutionTarget::Skill {
        name: "skills_manage".into(),
        root: "skill_execute".into(),
    };
    for other in [&mcp, &plugin, &skill] {
        assert!(!other.collects_exact_call());
        assert!(!other.names_collectable_action("run lifecycle_import_executable"));
    }
    assert!(!collectable_approval_entry(&plugin));
    assert!(!collectable_approval_entry(&skill));
}

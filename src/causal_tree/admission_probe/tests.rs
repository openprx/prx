use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::causal_tree::candidate_generator::{
    CandidatePolicy, CandidateSet, CapabilityCostModel, CausalCandidateGenerator, DiscoveredRefs,
};
use crate::causal_tree::state::{SideEffectMode, TrustedState, fixtures};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityRef, CausalActionKind, CostEstimate};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "admission probe fixture"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

/// Runs `check` over a one-shell-tool catalog under `autonomy`, with every
/// capability priced so G6 never decides the outcome.
fn with_fixture(
    autonomy: AutonomyLevel,
    input: &str,
    fingerprint_shell: bool,
    check: impl FnOnce(&CausalState, &CatalogView<'_>, &CandidatePolicy<'_>, &GateInputs<'_>),
) {
    let key = fixtures::key();
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("shell")), Box::new(NamedTool("prx_lookup"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    let per_capability = view
        .entries()
        .iter()
        .map(|entry| {
            (
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                },
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 1,
                    },
                    1,
                ),
            )
        })
        .collect();
    let cost = CapabilityCostModel {
        per_capability,
        estimates: BTreeMap::new(),
    };
    let discovered = DiscoveredRefs::empty();
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: 12,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &discovered,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = autonomy;
    let mut calls = BTreeMap::new();
    if fingerprint_shell {
        for entry in view.entries().iter().filter(|entry| entry.public_name == "shell") {
            calls.insert(
                crate::causal_tree::branch::CandidateId::new(CausalActionKind::UseTool, entry.family, None),
                [7; 32],
            );
            calls.insert(
                crate::causal_tree::branch::CandidateId::new(
                    CausalActionKind::UseTool,
                    entry.family,
                    Some(crate::causal_tree::branch::Ns8::of(&key, &entry.namespace)),
                ),
                [7; 32],
            );
        }
    }
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
    let mut state = fixtures::state(&key, input);
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = match autonomy {
        AutonomyLevel::ReadOnly => SideEffectMode::ReadOnly,
        AutonomyLevel::Supervised => SideEffectMode::ApprovalRequired,
        AutonomyLevel::Full => SideEffectMode::GuardedWrite,
    };
    state.trusted = TrustedState::from_runtime(trusted);
    check(&state, &view, &policy, &gates);
}

#[test]
fn hard_gates_probe_reproduces_pre_separation_candidate_sets() {
    for autonomy in [AutonomyLevel::ReadOnly, AutonomyLevel::Supervised, AutonomyLevel::Full] {
        for input in ["neutral request", "run shell please", "use prx_lookup"] {
            with_fixture(autonomy, input, false, |state, view, policy, gates| {
                let probe = HardGatesProbe {
                    state,
                    catalog: view,
                    gates,
                };
                let set = CausalCandidateGenerator::generate(state, view, policy, &probe).unwrap();
                let (kept, report) = match &set {
                    CandidateSet::Legal { candidates, report, .. } => (candidates.clone(), report.clone()),
                    CandidateSet::NoLegalPath { terminals, report, .. } => (terminals.clone(), report.clone()),
                };
                // Every analysed candidate is exactly what the pre-separation
                // hard gates admit, and the report is the hard gates' report.
                let direct = HardGates::apply(state, view, &kept, gates).unwrap();
                for candidate in &kept {
                    assert_eq!(
                        HardGates::check_one(state, view, candidate, gates),
                        Admission::Passed,
                        "{autonomy:?} {input}"
                    );
                    assert_eq!(
                        report.outcomes.get(candidate.candidate_id()),
                        direct.outcomes.get(candidate.candidate_id())
                    );
                    assert_eq!(probe.check_one(candidate).admission, Admission::Passed);
                }
            });
        }
    }
}

#[test]
fn exact_call_scope_is_independent_of_approval_outcome() {
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        for fingerprinted in [false, true] {
            with_fixture(
                autonomy,
                "run shell please",
                fingerprinted,
                |state, view, policy, gates| {
                    let probe = HardGatesProbe {
                        state,
                        catalog: view,
                        gates,
                    };
                    for (id, _) in gates.call_fingerprints.iter() {
                        let entry = view
                            .entries()
                            .iter()
                            .copied()
                            .find(|entry| entry.public_name == "shell")
                            .unwrap();
                        let candidate = crate::causal_tree::branch::CausalCandidate::new(
                            CausalActionKind::UseTool,
                            entry.family,
                            crate::causal_tree::branch::CandidateSpec {
                                namespace: id.namespace(),
                                required_capabilities: vec![CapabilityRef {
                                    id: entry.id.clone(),
                                    fingerprint: entry.fingerprint,
                                    generation: entry.generation,
                                }],
                                side_effect: entry.effect,
                                approval: entry.approval.clone(),
                                estimated_cost: Some(CostEstimate {
                                    tokens: 1,
                                    micro_usd: 1,
                                }),
                                estimated_latency_ms: Some(1),
                            },
                        );
                        let outcome = probe.check_one(&candidate);
                        // The scope bit follows only the verified call binding.
                        assert!(outcome.exact_call_scope);
                        assert_eq!(outcome.admission, HardGates::check_one(state, view, &candidate, gates));
                    }
                    let terminal = CausalCandidateGenerator::generate(state, view, policy, &probe).unwrap();
                    if let CandidateSet::Legal { candidates, .. } = terminal {
                        for candidate in candidates {
                            assert_eq!(
                                probe.check_one(&candidate).exact_call_scope,
                                gates.call_fingerprints.contains_key(candidate.candidate_id())
                            );
                        }
                    }
                },
            );
        }
    }
}

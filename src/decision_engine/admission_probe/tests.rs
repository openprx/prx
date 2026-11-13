#![cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

use std::sync::Arc;

use super::*;
use crate::causal_tree::gates::{GateInputs, HardGates};
use crate::causal_tree::hard_gates_probe::HardGatesProbe;
use crate::decision_engine::candidate_generator::{
    CandidatePolicy, CandidateSet, CausalCandidateGenerator, DiscoveredRefs,
};
use crate::decision_engine::cost_model::CapabilityCostModel;
use crate::decision_engine::state::CausalState;
use crate::decision_engine::state::fixtures;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::CatalogView;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityRef, CausalActionKind, CostEstimate};
use crate::tools::capability::turn_state::{SideEffectMode, TrustedState};
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
                crate::decision_engine::candidate::CandidateId::new(CausalActionKind::UseTool, entry.family, None),
                [7; 32],
            );
            calls.insert(
                crate::decision_engine::candidate::CandidateId::new(
                    CausalActionKind::UseTool,
                    entry.family,
                    Some(crate::decision_engine::candidate::Ns8::of(&key, &entry.namespace)),
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
    hard_gates_probe_keeps_pre_separation_sets_on_the_ten_fixtures();
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
                    for id in gates.call_fingerprints.keys() {
                        let entry = view
                            .entries()
                            .iter()
                            .copied()
                            .find(|entry| entry.public_name == "shell")
                            .unwrap();
                        let candidate = crate::decision_engine::candidate::CausalCandidate::new(
                            CausalActionKind::UseTool,
                            entry.family,
                            crate::decision_engine::candidate::CandidateSpec {
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

// ---------------------------------------------------------------------------
// R14: the ten fixtures of card 05a1 appendix §B.
// ---------------------------------------------------------------------------

/// How a fixture blocks its target capability.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blocker {
    /// The security policy denies it (G4).
    PolicyDeny,
    /// Supervised approval with no verified call (`ApprovalDeferred`).
    Approval,
    /// A write capability in a read-only turn (G9 prefilter).
    ReadOnlyTurn,
    /// The surface policy excludes it (G2 prefilter).
    Surface,
    /// An explicit name/root exclusion of the turn (G2 prefilter).
    Exclusion,
    /// A matching-layout model admission policy (G2 prefilter).
    ModelAdmission,
    /// Not channel-eligible on a channel turn (G2 prefilter).
    Channel,
}

struct Fixture {
    number: u8,
    tools: fn() -> Vec<Box<dyn Tool>>,
    /// Direct tools; every other tool is only searchable (discovery).
    direct: &'static [&'static str],
    input: &'static str,
    blocker: Blocker,
    /// Public name of the blocked capability and the action that reaches it.
    target: &'static str,
    action: CausalActionKind,
}

fn probe_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(NamedTool("prx_lookup"))]
}

fn file_tools() -> Vec<Box<dyn Tool>> {
    let shared = Arc::new(SecurityPolicy::default());
    vec![
        Box::new(crate::tools::file_read::FileReadTool::new(Arc::clone(&shared), false)),
        Box::new(crate::tools::file_write::FileWriteTool::new(Arc::clone(&shared))),
        Box::new(crate::tools::file_edit::FileEditTool::new(shared)),
    ]
}

fn write_tool() -> Vec<Box<dyn Tool>> {
    vec![Box::new(crate::tools::file_write::FileWriteTool::new(Arc::new(
        SecurityPolicy::default(),
    )))]
}

/// The ten fixtures; 7–10 each have a family and a discovery variant.
fn ten_fixtures() -> Vec<Fixture> {
    let family = |number, blocker| Fixture {
        number,
        tools: probe_tools,
        direct: &["prx_lookup"],
        input: "neutral request",
        blocker,
        target: "prx_lookup",
        action: CausalActionKind::UseTool,
    };
    let discovery = |number, blocker| Fixture {
        number,
        tools: probe_tools,
        direct: &[],
        input: "neutral request",
        blocker,
        target: "prx_lookup",
        action: CausalActionKind::Discover,
    };
    let mut fixtures = vec![
        Fixture {
            input: "use prx_lookup",
            ..family(1, Blocker::PolicyDeny)
        },
        Fixture {
            input: "use prx_lookup",
            ..family(2, Blocker::Approval)
        },
        family(3, Blocker::PolicyDeny),
        Fixture {
            number: 4,
            tools: file_tools,
            direct: &["file_read", "file_write", "file_edit"],
            input: "Perform the native workspace file task.",
            blocker: Blocker::Exclusion,
            target: "file_write",
            action: CausalActionKind::UseTool,
        },
        discovery(5, Blocker::PolicyDeny),
        Fixture {
            number: 6,
            tools: write_tool,
            direct: &["file_write"],
            input: "neutral request",
            blocker: Blocker::ReadOnlyTurn,
            target: "file_write",
            action: CausalActionKind::UseTool,
        },
    ];
    for (number, blocker) in [
        (7, Blocker::Surface),
        (8, Blocker::Exclusion),
        (9, Blocker::ModelAdmission),
        (10, Blocker::Channel),
    ] {
        fixtures.push(family(number, blocker));
        fixtures.push(discovery(number, blocker));
    }
    fixtures
}

/// The expected annotation gate of a fixture's target (`None`: approval).
const fn expected_gate(blocker: Blocker) -> Option<u8> {
    match blocker {
        Blocker::PolicyDeny => Some(4),
        Blocker::Approval => None,
        Blocker::ReadOnlyTurn => Some(9),
        Blocker::Surface | Blocker::Exclusion | Blocker::ModelAdmission | Blocker::Channel => Some(2),
    }
}

/// Builds one fixture's state, catalog view, priced policy and gate inputs.
fn with_ten_fixture(
    fixture: &Fixture,
    max_candidates: usize,
    check: impl FnOnce(&CausalState, &CatalogView<'_>, &CatalogView<'_>, &CandidatePolicy<'_>, &GateInputs<'_>),
) {
    let key = fixtures::key();
    let registry = (fixture.tools)();
    let build_surface = SurfacePolicy {
        direct_names: fixture.direct.iter().map(|name| (*name).to_owned()).collect(),
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &build_surface).unwrap();
    let unrestricted = catalog.view(&CatalogScope::default());
    let per_capability = unrestricted
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
        max_candidates,
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost,
        discovered_refs: &discovered,
    };
    let mut security = SecurityPolicy::default();
    security.autonomy = match fixture.blocker {
        Blocker::PolicyDeny => AutonomyLevel::ReadOnly,
        Blocker::Approval => AutonomyLevel::Supervised,
        _ => AutonomyLevel::Full,
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
    let mut state = fixtures::state(&key, fixture.input);
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = match fixture.blocker {
        Blocker::ReadOnlyTurn => SideEffectMode::ReadOnly,
        Blocker::Approval => SideEffectMode::ApprovalRequired,
        _ => SideEffectMode::GuardedWrite,
    };
    match fixture.blocker {
        Blocker::Exclusion => trusted.exclusions.push(Arc::from(fixture.target)),
        Blocker::ModelAdmission => {
            // An empty allowlist denies every visible entry of this layout.
            trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
                &registry,
                &unrestricted,
                "prx-model",
                &[],
                "prx-policy",
                &key,
            ));
        }
        // Catalog entries are built without `channel_allowed`.
        Blocker::Channel => trusted.channel = Some(Arc::from("terminal")),
        _ => {}
    }
    state.trusted = TrustedState::from_runtime(trusted);
    let surface = SurfacePolicy {
        excluded_names: if fixture.blocker == Blocker::Surface {
            BTreeSet::from([fixture.target.to_owned()])
        } else {
            BTreeSet::new()
        },
        ..build_surface
    };
    let view = catalog.view(&CatalogScope::default()).with_surface(&surface);
    check(&state, &view, &unrestricted, &policy, &gates);
}

/// Whether `candidate` is the fixture's target: its action over a set of
/// capabilities that includes the blocked one.
fn is_target(fixture: &Fixture, view: &CatalogView<'_>, candidate: &CausalCandidate) -> bool {
    candidate.action() == fixture.action
        && candidate.required_capabilities().iter().any(|reference| {
            view.entry_any(&reference.id)
                .is_some_and(|entry| entry.public_name == fixture.target)
        })
}

fn kept_and_report(set: &CandidateSet) -> (&[CausalCandidate], &ProbeReport) {
    match set {
        CandidateSet::Legal { candidates, report, .. } => (candidates, report),
        CandidateSet::NoLegalPath { terminals, report } => (terminals, report),
    }
}

/// A surface-excluded entry is structurally hidden by the view, so the G2
/// surface prefilter is reached only when the caller hands the entry to
/// family selection itself (as the retrieval and delegation paths may).
fn surface_family_selection(
    fixture: &Fixture,
    state: &CausalState,
    view: &CatalogView<'_>,
    unrestricted: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    probe: &dyn AdmissionProbe,
) -> (Option<CausalCandidate>, BTreeMap<CandidateId, Admission>) {
    let entry = unrestricted
        .entries()
        .iter()
        .copied()
        .find(|entry| entry.public_name == fixture.target)
        .unwrap();
    let mut rejections = BTreeMap::new();
    let selected = crate::decision_engine::candidate_generator::select_family_candidate(
        fixture.action,
        entry.family,
        [entry],
        false,
        state,
        view,
        policy,
        probe,
        &mut rejections,
    )
    .unwrap();
    (selected, rejections)
}

#[test]
fn advisory_probe_annotates_blockers_without_filtering() {
    for fixture in ten_fixtures() {
        let number = fixture.number;
        with_ten_fixture(&fixture, 12, |state, view, unrestricted, policy, gates| {
            let hard = HardGatesProbe {
                state,
                catalog: view,
                gates,
            };
            let advisory = AdvisoryProbe { inner: &hard };
            assert!(hard.filters() && !advisory.filters());
            if fixture.blocker == Blocker::Surface {
                // Hidden by the view in both modes (structural); the family
                // prefilter itself annotates instead of skipping.
                let (selected, rejections) =
                    surface_family_selection(&fixture, state, view, unrestricted, policy, &advisory);
                let candidate = selected.expect("advisory constructs the surface-excluded entry");
                assert!(is_target(&fixture, unrestricted, &candidate), "fixture {number}");
                let annotation = rejections.get(candidate.candidate_id()).cloned();
                assert_eq!(
                    annotation,
                    Some(HardGates::check_one(state, view, &candidate, gates)),
                    "fixture {number}"
                );
                assert!(
                    matches!(annotation, Some(Admission::Blocked { gate: 2, .. })),
                    "fixture {number}: {annotation:?}"
                );
                return;
            }
            let set = CausalCandidateGenerator::generate(state, view, policy, &advisory).unwrap();
            let (kept, report) = kept_and_report(&set);
            assert!(matches!(set, CandidateSet::Legal { .. }), "fixture {number}");
            let target = kept
                .iter()
                .find(|candidate| is_target(&fixture, view, candidate))
                .unwrap_or_else(|| panic!("fixture {number}: the advisory set keeps the blocked target"));
            for candidate in kept {
                assert_eq!(
                    report.outcomes.get(candidate.candidate_id()),
                    Some(&HardGates::check_one(state, view, candidate, gates)),
                    "fixture {number}: annotation equals the hard-gate outcome"
                );
            }
            let annotation = report.outcomes.get(target.candidate_id()).cloned();
            match expected_gate(fixture.blocker) {
                Some(gate) => assert!(
                    matches!(annotation, Some(Admission::Blocked { gate: actual, .. }) if actual == gate),
                    "fixture {number}: {annotation:?}"
                ),
                None => assert_eq!(annotation, Some(Admission::ApprovalDeferred), "fixture {number}"),
            }
            if fixture.number == 4 {
                // The structural group keeps every native file member.
                assert_eq!(target.required_capabilities().len(), 3, "fixture {number}");
            }
            // The Answer terminal is never suppressed by admission.
            assert!(
                kept.iter()
                    .any(|candidate| candidate.action() == CausalActionKind::Answer),
                "fixture {number}"
            );
        });
    }
}

#[test]
fn advisory_ordering_and_limits_match_structural_order() {
    let ids = |set: &CandidateSet| {
        kept_and_report(set)
            .0
            .iter()
            .map(|candidate| candidate.candidate_id().clone())
            .collect::<Vec<_>>()
    };
    for fixture in ten_fixtures()
        .into_iter()
        .filter(|fixture| fixture.blocker != Blocker::Surface)
    {
        let number = fixture.number;
        let mut full_working = None;
        with_ten_fixture(&fixture, 12, |state, view, _, policy, gates| {
            let hard = HardGatesProbe {
                state,
                catalog: view,
                gates,
            };
            let advisory = AdvisoryProbe { inner: &hard };
            let first = ids(&CausalCandidateGenerator::generate(state, view, policy, &advisory).unwrap());
            let second = ids(&CausalCandidateGenerator::generate(state, view, policy, &advisory).unwrap());
            assert_eq!(first, second, "fixture {number}: deterministic order");
            // The admitted set keeps its relative order inside the advisory set.
            let legacy = ids(&CausalCandidateGenerator::generate(state, view, policy, &hard).unwrap());
            let mut rest = first.iter();
            assert!(
                legacy.iter().all(|id| rest.any(|advised| advised == id)),
                "fixture {number}: {legacy:?} is ordered within {first:?}"
            );
            full_working = advisory_set_first_working(state, view, policy, &advisory);
        });
        with_ten_fixture(&fixture, 3, |state, view, _, policy, gates| {
            let hard = HardGatesProbe {
                state,
                catalog: view,
                gates,
            };
            let set = CausalCandidateGenerator::generate(state, view, policy, &AdvisoryProbe { inner: &hard }).unwrap();
            let limited = ids(&set);
            assert!(limited.len() <= 3, "fixture {number}: candidate limit");
            // Refuse and Clarify keep their reserved space; the one working
            // slot goes to the first candidate in structural order. Each
            // fixture run mints its own key, so candidates are compared by
            // action and capability ids, not by their keyed namespace.
            let working =
                |id: &&CandidateId| !matches!(id.action(), CausalActionKind::Refuse | CausalActionKind::Clarify);
            let first_working = kept_and_report(&set)
                .0
                .iter()
                .find(|candidate| working(&candidate.candidate_id()))
                .map(|candidate| structural_key(candidate));
            assert_eq!(
                first_working,
                full_working.clone(),
                "fixture {number}: structural priority under the limit"
            );
        });
    }
}

/// A key-independent identity: the action and the required capability ids.
fn structural_key(
    candidate: &crate::decision_engine::candidate::CausalCandidate,
) -> (CausalActionKind, Vec<crate::tools::capability::contract::CapabilityId>) {
    (
        candidate.candidate_id().action(),
        candidate
            .required_capabilities()
            .iter()
            .map(|reference| reference.id.clone())
            .collect(),
    )
}

/// The first non-Refuse/Clarify candidate of the advisory set, structurally.
fn advisory_set_first_working(
    state: &CausalState,
    view: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    advisory: &AdvisoryProbe<'_>,
) -> Option<(CausalActionKind, Vec<crate::tools::capability::contract::CapabilityId>)> {
    let set = CausalCandidateGenerator::generate(state, view, policy, advisory).unwrap();
    kept_and_report(&set)
        .0
        .iter()
        .find(|candidate| {
            !matches!(
                candidate.candidate_id().action(),
                CausalActionKind::Refuse | CausalActionKind::Clarify
            )
        })
        .map(structural_key)
}

/// R14 extension of `hard_gates_probe_reproduces_pre_separation_candidate_sets`:
/// on the ten fixtures the legacy set omits the blocked or prefiltered entry
/// exactly as before the separation.
fn hard_gates_probe_keeps_pre_separation_sets_on_the_ten_fixtures() {
    for fixture in ten_fixtures() {
        let number = fixture.number;
        with_ten_fixture(&fixture, 12, |state, view, unrestricted, policy, gates| {
            let probe = HardGatesProbe {
                state,
                catalog: view,
                gates,
            };
            if fixture.blocker == Blocker::Surface {
                let (selected, rejections) =
                    surface_family_selection(&fixture, state, view, unrestricted, policy, &probe);
                assert!(
                    selected.is_none() && rejections.is_empty(),
                    "fixture {number}: prefiltered"
                );
                return;
            }
            let set = CausalCandidateGenerator::generate(state, view, policy, &probe).unwrap();
            let (kept, report) = kept_and_report(&set);
            let direct = HardGates::apply(state, view, kept, gates).unwrap();
            for candidate in kept {
                assert_eq!(
                    HardGates::check_one(state, view, candidate, gates),
                    Admission::Passed,
                    "fixture {number}"
                );
                assert_eq!(
                    report.outcomes.get(candidate.candidate_id()),
                    direct.outcomes.get(candidate.candidate_id()),
                    "fixture {number}"
                );
            }
            // The legacy set omits the blocked or prefiltered target.
            assert!(
                !kept.iter().any(|candidate| is_target(&fixture, view, candidate)),
                "fixture {number}"
            );
        });
    }
}

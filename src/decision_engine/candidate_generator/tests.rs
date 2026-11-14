use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::*;
use crate::decision_engine::candidate::{CandidateId, CausalCandidate};
use crate::decision_engine::cost_model::CapabilityCostModel;
use crate::decision_engine::state::fixtures;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityId, CausalActionKind, CostEstimate};
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
        "candidate generator fixture"
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

/// The ten structural fixtures; 7–10 each have a family and a discovery
/// variant. The blocker names the condition the deleted admission probe
/// annotated; the structural generator ignores it.
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

/// Builds one fixture's state, catalog view and priced policy.
fn with_ten_fixture(
    fixture: &Fixture,
    max_candidates: usize,
    check: impl FnOnce(&CausalState, &CatalogView<'_>, &CandidatePolicy<'_>),
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
    check(&state, &view, &policy);
}

/// Whether `candidate` is the fixture's target: its action over a set of
/// capabilities that includes the named one.
fn is_target(fixture: &Fixture, view: &CatalogView<'_>, candidate: &CausalCandidate) -> bool {
    candidate.action() == fixture.action
        && candidate.required_capabilities().iter().any(|reference| {
            view.entry_any(&reference.id)
                .is_some_and(|entry| entry.public_name == fixture.target)
        })
}

fn ids(set: &CandidateSet) -> Vec<CandidateId> {
    match set {
        CandidateSet::Legal { candidates, .. } => candidates
            .iter()
            .map(|candidate| candidate.candidate_id().clone())
            .collect(),
        CandidateSet::NoLegalPath => Vec::new(),
    }
}

/// A key-independent identity: the action and the required capability ids.
fn structural_key(candidate: &CausalCandidate) -> (CausalActionKind, Vec<CapabilityId>) {
    (
        candidate.candidate_id().action(),
        candidate
            .required_capabilities()
            .iter()
            .map(|reference| reference.id.clone())
            .collect(),
    )
}

/// The first non-Refuse/Clarify candidate of a set, structurally.
fn first_working(set: &CandidateSet) -> Option<(CausalActionKind, Vec<CapabilityId>)> {
    let CandidateSet::Legal { candidates, .. } = set else {
        return None;
    };
    candidates
        .iter()
        .find(|candidate| {
            !matches!(
                candidate.candidate_id().action(),
                CausalActionKind::Refuse | CausalActionKind::Clarify
            )
        })
        .map(structural_key)
}

#[test]
fn structural_set_rejects_duplicate_candidate_ids() {
    let key = fixtures::key();
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
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
    let refuse = terminal(CausalActionKind::Refuse, &policy);
    let clarify = terminal(CausalActionKind::Clarify, &policy);
    assert!(validate_structure(&[clarify.clone(), refuse.clone()]).is_ok());
    assert!(matches!(
        validate_structure(&[clarify.clone(), clarify, refuse]),
        Err(GenerationError::DuplicateCandidate)
    ));
}

#[test]
fn structural_set_requires_a_refusal_path() {
    let key = fixtures::key();
    let cost = CapabilityCostModel {
        per_capability: BTreeMap::new(),
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
    let answer = terminal(CausalActionKind::Answer, &policy);
    let clarify = terminal(CausalActionKind::Clarify, &policy);
    assert!(matches!(
        validate_structure(&[answer, clarify]),
        Err(GenerationError::MissingRefusal)
    ));
    assert!(matches!(validate_structure(&[]), Err(GenerationError::MissingRefusal)));
}

#[test]
fn structural_ordering_and_limits_are_preserved() {
    for fixture in ten_fixtures()
        .into_iter()
        .filter(|fixture| fixture.blocker != Blocker::Surface)
    {
        let number = fixture.number;
        let mut full_working = None;
        with_ten_fixture(&fixture, 12, |state, view, policy| {
            let first = CausalCandidateGenerator::generate(state, view, policy).unwrap();
            let second = CausalCandidateGenerator::generate(state, view, policy).unwrap();
            assert_eq!(ids(&first), ids(&second), "fixture {number}: deterministic order");
            // No admission filters the structural set: the blocked target
            // stays, and so does the Answer terminal.
            let CandidateSet::Legal { candidates, .. } = &first else {
                panic!("fixture {number}: structural set has a working path");
            };
            assert!(
                candidates.iter().any(|candidate| is_target(&fixture, view, candidate)),
                "fixture {number}: the structural set keeps the target"
            );
            assert!(
                candidates
                    .iter()
                    .any(|candidate| candidate.action() == CausalActionKind::Answer),
                "fixture {number}"
            );
            full_working = first_working(&first);
        });
        with_ten_fixture(&fixture, 3, |state, view, policy| {
            let set = CausalCandidateGenerator::generate(state, view, policy).unwrap();
            assert!(ids(&set).len() <= 3, "fixture {number}: candidate limit");
            // Refuse and Clarify keep their reserved space; the one working
            // slot goes to the first candidate in structural order. Each
            // fixture run mints its own key, so candidates are compared by
            // action and capability ids, not by their keyed namespace.
            assert_eq!(
                first_working(&set),
                full_working.clone(),
                "fixture {number}: structural priority under the limit"
            );
        });
        with_ten_fixture(&fixture, 2, |state, view, policy| {
            assert!(matches!(
                CausalCandidateGenerator::generate(state, view, policy),
                Err(GenerationError::InvalidCandidateLimit)
            ));
        });
    }
}

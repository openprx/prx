#![cfg(test)]
//! Golden: the execution-owned publication rules reproduce the pre-strip
//! direct surface (`direct_answer_refs` over the hard gates' capability
//! publication scope) in Supervised, read-only and Full turns.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::causal_tree::gates::{GateInputs, GateOutcome, HardGates};
use crate::decision_engine::candidate::{CandidateSpec, CausalCandidate};
use crate::decision_engine::state::fixtures;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, Exposure, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityRef, CausalActionKind, CostEstimate, SideEffectClass};
use crate::tools::capability::turn_state::{ConstraintCode, SideEffectMode, TrustedParts, TrustedState};
use crate::tools::traits::{Tool, ToolResult};

use super::publication::{PublicationRules, entry_ref, is_delegator};

struct FixtureTool(&'static str);

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Fixture capability"
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

/// The pre-strip `direct_answer_refs` for a turn without an outgoing child:
/// Direct entries, minus turn restrictions, minus hard-gate publication
/// blocks.
fn pre_strip_direct_surface(
    state: &crate::decision_engine::state::CausalState,
    catalog: &crate::tools::capability::catalog::CatalogView<'_>,
    gates: &GateInputs<'_>,
) -> BTreeSet<CapabilityRef> {
    if !gates.model_can_call_tools {
        return BTreeSet::new();
    }
    let trusted = state.trusted.parts();
    catalog
        .iter_entries()
        .filter_map(|entry| {
            let delegation = is_delegator(&entry.public_name);
            let reference = entry_ref(entry);
            if !matches!(entry.exposure, Exposure::Direct) {
                return None;
            }
            if trusted.constraints.iter().any(|code| {
                code.as_str() == "tools_off" || code.as_str() == "intercepts_writes" && entry.plan_intercepted()
            }) || trusted.side_effect_mode == SideEffectMode::ReadOnly && entry.effect > SideEffectClass::ReadOnly
            {
                return None;
            }
            let candidate = CausalCandidate::new(
                if delegation {
                    CausalActionKind::Delegate
                } else {
                    CausalActionKind::UseTool
                },
                entry.family,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference.clone()],
                    side_effect: entry.effect,
                    approval: entry.approval.clone(),
                    // A zero-cost, zero-latency candidate passes G6-G8, so the
                    // hard gates' outcome is exactly their G1-G5 publication
                    // scope (the retired `check_capability_publication`).
                    estimated_cost: Some(CostEstimate {
                        tokens: 0,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(0),
                },
            );
            let publication = HardGates::check_one(state, catalog, &candidate, gates);
            (!matches!(publication, GateOutcome::Blocked { .. })).then_some(reference)
        })
        .collect()
}

fn trusted_for(catalog: &CapabilityCatalog, mode: SideEffectMode, constraints: &[&'static str]) -> TrustedParts {
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = mode;
    trusted.constraints = constraints.iter().map(|code| ConstraintCode::new(code)).collect();
    trusted
}

fn facts_for(policy: Arc<SecurityPolicy>) -> AdmissionFactsV1 {
    AdmissionFactsV1 {
        policy,
        owner_id: Arc::from("owner-1"),
        sender: Arc::from("owner-1"),
        policy_channel: Arc::from("terminal"),
        chat_type: Arc::from("dm"),
        model_ref: Arc::from("anthropic:claude"),
        model_can_chat: true,
        model_can_call_tools: true,
        child_scope: None,
        child_discovery: false,
        child_served: None,
    }
}

#[test]
fn publication_rules_match_pre_strip_direct_surface() {
    // The fixture surface of `engine_switch::tool_surface_is_built_without_decision_service`
    // and `default_config_discovered_tool_is_published_then_dispatched`:
    // read-only, local-write, process and delegation capabilities.
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(FixtureTool("memory_recall")),
        Box::new(FixtureTool("file_read")),
        Box::new(FixtureTool("file_write")),
        Box::new(FixtureTool("shell")),
        Box::new(FixtureTool("delegate")),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
        .expect("test: catalog");
    let view = catalog.view(&CatalogScope::default());
    let key = fixtures::key();
    let cells: [(AutonomyLevel, SideEffectMode, &[&'static str]); 6] = [
        (AutonomyLevel::Supervised, SideEffectMode::ApprovalRequired, &[]),
        (
            AutonomyLevel::Supervised,
            SideEffectMode::ApprovalRequired,
            &["intercepts_writes"],
        ),
        (
            AutonomyLevel::Supervised,
            SideEffectMode::ApprovalRequired,
            &["tools_off"],
        ),
        (AutonomyLevel::ReadOnly, SideEffectMode::ReadOnly, &[]),
        (AutonomyLevel::Full, SideEffectMode::GuardedWrite, &[]),
        (
            AutonomyLevel::Full,
            SideEffectMode::GuardedWrite,
            &["intercepts_writes"],
        ),
    ];
    for (level, mode, constraints) in cells {
        let policy = Arc::new(SecurityPolicy {
            autonomy: level,
            ..SecurityPolicy::default()
        });
        let trusted = trusted_for(&catalog, mode, constraints);
        let facts = facts_for(Arc::clone(&policy));
        let mut state = fixtures::state(&key, "neutral request");
        state.trusted = TrustedState::from_runtime(trusted.clone());
        let calls = BTreeMap::new();
        let replayed = BTreeSet::new();
        let gates = GateInputs {
            policy: &policy,
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
            retry_limit: None,
        };
        let expected = pre_strip_direct_surface(&state, &view, &gates);
        let rules = PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &view,
        };
        let published = rules.direct_surface(false, &|_| None);
        assert_eq!(
            published, expected,
            "direct surface differs for {level:?} / {constraints:?}"
        );
        assert!(rules.turn_scope_admits(), "turn scope refused for {level:?}");
        // No resume and no outgoing child: the turn surface is the direct core.
        assert_eq!(rules.surface(false, &|_| None), expected);
    }
}

#[test]
fn turn_scope_refuses_foreign_owner_and_other_model() {
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(FixtureTool("memory_recall"))];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
        .expect("test: catalog");
    let view = catalog.view(&CatalogScope::default());
    let trusted = trusted_for(&catalog, SideEffectMode::ApprovalRequired, &[]);
    let mut facts = facts_for(Arc::new(SecurityPolicy::default()));
    assert!(
        PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &view,
        }
        .turn_scope_admits()
    );
    facts.owner_id = Arc::from("forged-owner");
    assert!(
        !PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &view,
        }
        .turn_scope_admits()
    );
    facts.owner_id = Arc::from("owner-1");
    facts.model_ref = Arc::from("other:model");
    assert!(
        !PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &view,
        }
        .turn_scope_admits()
    );
}

/// R51 / P1-02: an attenuated (Supervised) child manifest limits neither
/// membership nor approval in a Full turn: an effectful member and a
/// capability outside the manifest are both published for automatic
/// discovery. In a Supervised turn the member still needs approval and the
/// non-member is blocked.
#[test]
fn full_turn_discovery_ignores_an_attenuated_child_manifest() {
    use super::publication::PublicationAdmission;
    use crate::security::decision_id::DecisionId;
    use crate::tools::capability::child::{AuthorityEnvelope, CapabilityConstraints, ChildCapabilityManifest};
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(FixtureTool("memory_recall")),
        Box::new(FixtureTool("file_write")),
        Box::new(FixtureTool("shell")),
    ];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
        .expect("test: catalog");
    let view = catalog.view(&CatalogScope::default());
    let entry = |name: &str| catalog.entry_for_public_name(name).expect("test: registered entry");
    let cost = Some(CostEstimate {
        tokens: 0,
        micro_usd: 0,
    });
    for (level, mode) in [
        (AutonomyLevel::Full, SideEffectMode::GuardedWrite),
        (AutonomyLevel::Supervised, SideEffectMode::ApprovalRequired),
    ] {
        let trusted = trusted_for(&catalog, mode, &[]);
        let parent = CapabilityConstraints {
            parent_authority_id: DecisionId::parse("0192f0c4-0000-7000-8000-000000000007")
                .expect("test: id")
                .into(),
            allowed: ["memory_recall", "file_write", "shell"]
                .into_iter()
                .map(|name| entry_ref(entry(name)))
                .collect(),
            authority: AuthorityEnvelope {
                owner: "owner-1".into(),
                workspace: trusted.workspace_id.to_string(),
                level: AutonomyLevel::Supervised,
                max_cost_cents: 100,
                remaining_depth: 2,
                delegation: None,
            },
            catalog_generation: *catalog.generation(),
            key: fixtures::key(),
        };
        let manifest = ChildCapabilityManifest::derive_strict_subset(&parent, &[entry_ref(entry("file_write"))])
            .expect("test: attenuated manifest");
        let mut facts = facts_for(Arc::new(SecurityPolicy {
            autonomy: level,
            ..SecurityPolicy::default()
        }));
        facts.child_scope = Some((Arc::new(manifest), Arc::new(parent)));
        let rules = PublicationRules {
            trusted: &trusted,
            facts: &facts,
            catalog: &view,
        };
        let member = rules.capability(entry("file_write"), &entry_ref(entry("file_write")), false, cost);
        let outside = rules.capability(entry("shell"), &entry_ref(entry("shell")), false, cost);
        if level == AutonomyLevel::Full {
            assert_eq!(
                member,
                PublicationAdmission::Passed,
                "Full: effectful member is published"
            );
            assert_eq!(outside, PublicationAdmission::Passed, "Full: non-member is published");
        } else {
            assert_eq!(
                member,
                PublicationAdmission::ApprovalRequired,
                "Supervised: effectful member needs approval"
            );
            assert!(outside.is_blocked(), "Supervised: non-member is blocked");
        }
    }
}

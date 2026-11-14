#![cfg(test)]
//! The execution-owned publication rules: turn scope and child-manifest
//! membership per autonomy level.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::facts::PublicationFacts;
use crate::decision_engine::state::fixtures;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::contract::CostEstimate;
use crate::tools::capability::turn_state::{ConstraintCode, SideEffectMode, TrustedParts};
use crate::tools::traits::{Tool, ToolResult};

use super::publication::{PublicationRules, entry_ref};

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

fn trusted_for(catalog: &CapabilityCatalog, mode: SideEffectMode, constraints: &[&'static str]) -> TrustedParts {
    let mut trusted = fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    trusted.channel = None;
    trusted.exclusions.clear();
    trusted.side_effect_mode = mode;
    trusted.constraints = constraints.iter().map(|code| ConstraintCode::new(code)).collect();
    trusted
}

fn facts_for(policy: Arc<SecurityPolicy>) -> PublicationFacts {
    PublicationFacts {
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

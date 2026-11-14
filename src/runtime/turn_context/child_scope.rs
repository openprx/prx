//! Construct the ordinary outgoing child scope of a turn from its exact
//! mentions and the sealed parent set.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;

use super::OpenedTurn;
use crate::capability::CapabilityAvailabilityLevel;
use crate::decision_engine::state::RawRoutingInput;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::decision_id::DecisionId;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::{CatalogView, Exposure};
use crate::tools::capability::child::{AuthorityEnvelope, CapabilityConstraints};
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::turn_state::{SideEffectMode, TrustedParts, TurnSurfaceKind};

/// The sealed parent authority and the requested strict subset of one
/// outgoing child, before the manifest is derived.
pub(super) struct OutgoingDelegation {
    pub(super) parent: CapabilityConstraints,
    pub(super) requested: Vec<CapabilityRef>,
}

impl TurnRuntimeService {
    pub(super) fn configured_delegation_target(&self, proof: &dyn TurnSourceProof) -> Option<&'static str> {
        let task = &self.generation.effective.task_routing;
        if proof.surface() != TurnSurfaceKind::AgentLoop
            || proof.policy_channel() != "agent_api"
            || proof.policy_sender() != "embedded-caller"
            || proof.config_generation_id() != self.generation.id.0
            || proof.config_source_revision() != self.generation.source_revision.fingerprint_sha256
            || (task.rules.is_empty() && task.default_intent != crate::config::TaskRoutingIntentConfig::Delegate)
        {
            return None;
        }
        let projected = crate::multimodal::project_image_markers_for_routing(proof.routing_input());
        (crate::agent::classifier::classify_intent(task, projected.as_ref()).intent
            == crate::agent::classifier::TaskIntent::Delegate)
            .then_some("sessions_spawn")
    }

    /// Verify the selected subset independently of durable grant issuance.
    /// Disabled child provenance may retain this scope without claiming that
    /// a parent decision was persisted. Process transport is authenticated
    /// separately before reconstructing its ordinary source.
    pub(super) fn verified_delegation_scope(
        &self,
        context: &OpenedTurn,
    ) -> anyhow::Result<crate::tools::capability::child::VerifiedChildScope> {
        anyhow::ensure!(
            !context.runtime().execution_closed.load(Ordering::Acquire)
                && context.catalog_generation() == *self.catalog.generation(),
            "delegation scope is closed or belongs to another catalog"
        );
        // Delegator admission is an execution rule (R28): a published
        // delegator or a discovered `sessions_spawn` admitted without
        // approval may bind the turn's sealed outgoing child manifest.
        let runtime = context.runtime();
        let view = self.catalog_view();
        let rules = crate::tools::surface_owner::publication::PublicationRules {
            trusted: &runtime.trusted,
            facts: &runtime.admission,
            catalog: &view,
        };
        let outgoing = runtime.outgoing_delegation.as_ref();
        let admitted_delegator = rules.admits_published_delegator(&runtime.published, outgoing.is_some());
        let discovered_delegator = runtime.discovery.admission("sessions_spawn").is_some_and(|admission| {
            !admission.requires_approval
                && view.entry_any(&admission.reference.id).is_some_and(|entry| {
                    entry.public_name == "sessions_spawn"
                        && entry.fingerprint == admission.reference.fingerprint
                        && entry.generation == admission.reference.generation
                })
        });
        anyhow::ensure!(
            admitted_delegator || discovered_delegator,
            "answer surface has no admitted delegator"
        );
        let outgoing = outgoing.context("answer surface has no sealed child manifest")?;
        let manifest = &outgoing.manifest;
        // derive_strict_subset preserves owner/level/cost and decrements depth
        // exactly once. Rebuild its original parent from the engine's retained
        // allowed refs, then reverify the seal instead of trusting wire fields.
        let mut authority = manifest.authority.clone();
        authority.remaining_depth = authority
            .remaining_depth
            .checked_add(1)
            .context("child depth cannot reconstruct issued parent")?;
        let parent = CapabilityConstraints {
            parent_authority_id: context.decision_id().into(),
            allowed: outgoing.parent_allowed.clone(),
            authority,
            catalog_generation: context.catalog_generation(),
            key: Arc::clone(&self.state_key),
        };
        Ok(crate::tools::capability::child::VerifiedChildScope::new(
            manifest.clone(),
            &parent,
        )?)
    }

    pub(super) fn outgoing_delegation(
        &self,
        proof: &dyn TurnSourceProof,
        trusted: &TrustedParts,
        routing_input: &RawRoutingInput,
        decision_id: DecisionId,
        catalog: &CatalogView<'_>,
    ) -> Option<OutgoingDelegation> {
        // Ordinary turns without an exact delegator name keep the
        // allocation-free absence path; never scan the catalog. Every child
        // carries the strict subset through an ordinary child publication.
        let permitted = |entry: &crate::tools::capability::catalog::CapabilityEntry| {
            catalog.contains(&entry.id)
                && entry.generation == trusted.catalog_generation
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && !matches!(entry.exposure, Exposure::Unavailable { .. })
                && !trusted.excludes_entry(catalog.model_admission_entry(entry))
                && (trusted.channel.is_none() || entry.channel_eligible)
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
                    entry.policy_tool_name(),
                    proof.policy_sender(),
                    proof.policy_channel(),
                    proof.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny
        };
        let direct_delegator = ["delegate", "sessions_spawn"].into_iter().any(|name| {
            self.catalog
                .entry_for_public_name(name)
                .is_some_and(|entry| matches!(entry.exposure, Exposure::Direct) && permitted(entry))
        });
        // Chat keeps sessions_spawn deferred. Prepare its bounded child
        // manifest now, so a later admitted search hit can use the same
        // durable parent authority without exposing an eager model schema.
        let deferred_chat_delegator = trusted.surface == TurnSurfaceKind::Interactive
            && self
                .catalog
                .entry_for_public_name("sessions_spawn")
                .is_some_and(|entry| entry.supports_metadata_discovery() && permitted(entry));
        let input = routing_input.as_str();
        // This proposal comes from the pinned configuration of a sealed
        // embedded source, not from synthetic text or operator identity.
        let configured = self.configured_delegation_target(proof).is_some();
        let names_delegator = configured
            || input
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .any(|word| word.eq_ignore_ascii_case("delegate") || word.eq_ignore_ascii_case("sessions_spawn"));
        if !names_delegator && !direct_delegator && !deferred_chat_delegator {
            return None;
        }
        let direct = if names_delegator {
            crate::tools::capability::search::CatalogSearch::exact_mentions(
                catalog,
                input,
                usize::from(self.generation.effective.decision_engine.max_candidates),
            )?
        } else {
            Vec::new()
        };
        let inherited = proof.child_scope().map(|scope| scope.gate_inputs().0);
        let mut allowed = BTreeSet::new();
        if let Some(manifest) = inherited {
            // The engine authenticates nesting against the entire incoming
            // manifest. Reconstructing a larger parent from the catalog would
            // turn a discovery into authority that this child never received.
            if manifest.authority.remaining_depth == 0 {
                return None;
            }
            for reference in &manifest.allowed {
                let entry = catalog.entry_any(&reference.id)?;
                if !permitted(entry)
                    || entry.fingerprint != reference.fingerprint
                    || entry.generation != reference.generation
                {
                    return None;
                }
                allowed.insert(reference.clone());
            }
        } else {
            for name in ["delegate", "sessions_spawn"] {
                if let Some(entry) = self
                    .catalog
                    .entry_for_public_name(name)
                    .filter(|entry| permitted(entry))
                {
                    allowed.insert(CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    });
                }
            }
            // A root also owns its actual discovery control. Capture that
            // reference when recursive spawning is permitted, so the child
            // can drop it while retaining an explicitly requested delegator.
            // An inherited child can never recover this root-only reference.
            if self.generation.effective.sessions_spawn.allow_recursive_delegation
                && let Some(entry) = self
                    .catalog
                    .entry_for_public_name("capability_search")
                    .filter(|entry| permitted(entry))
            {
                allowed.insert(CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                });
            }
        }
        if !allowed.iter().any(|reference| {
            catalog
                .entry_any(&reference.id)
                .is_some_and(|entry| matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn"))
        }) {
            return None;
        }
        let mut requested = Vec::new();
        let references = direct
            .into_iter()
            // The named parent entrypoint is not itself requested work.
            .filter(|entry| !matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn"))
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            });
        for reference in references {
            let entry = catalog.entry_any(&reference.id)?;
            if !permitted(entry)
                || entry.fingerprint != reference.fingerprint
                || entry.generation != reference.generation
            {
                return None;
            }
            if inherited.is_some() {
                if !allowed.contains(&reference) {
                    return None;
                }
            } else {
                allowed.insert(reference.clone());
            }
            // A delegation capability may be passed onward, but only if the
            // complete request still strictly narrows the parent.
            requested.push(reference);
        }
        // A model-only child requests no tools. The empty set is a strict
        // subset of a parent that owns a delegator; it must not inherit the
        // parent's tools merely to make delegation possible. The manifest
        // still binds owner, budget, catalog generation and reduced depth.
        if requested.len() >= allowed.len() {
            return None;
        }
        // Strictly shrinking finite sets already bound a chain. This records
        // that bound within the existing wire representation; it introduces
        // no runtime concurrency setting or retired max_spawn_depth key.
        let remaining_depth = u8::try_from(allowed.len()).unwrap_or(u8::MAX);
        let max_cost_cents = u32::try_from(trusted.budgets.micro_usd / 10_000)
            .unwrap_or(u32::MAX)
            .min(self.security.max_cost_per_day_cents);
        let level = if trusted.side_effect_mode == SideEffectMode::ReadOnly {
            AutonomyLevel::ReadOnly
        } else {
            self.security.autonomy
        };
        let parent = CapabilityConstraints {
            parent_authority_id: decision_id.into(),
            allowed,
            authority: inherited.map_or_else(
                || AuthorityEnvelope {
                    owner: proof.owner_id().to_owned(),
                    workspace: proof.workspace_id().to_owned(),
                    level,
                    max_cost_cents,
                    remaining_depth,
                    delegation: None,
                },
                |manifest| {
                    let mut authority = manifest.authority.clone();
                    authority.max_cost_cents = authority.max_cost_cents.min(max_cost_cents);
                    authority
                },
            ),
            catalog_generation: trusted.catalog_generation,
            key: Arc::clone(&self.state_key),
        };
        // Retaining the exact inherited authority must not bypass a tighter
        // current policy or the remaining turn allowance.
        if !authority_within_policy(&parent, trusted, &self.security, parent.authority.remaining_depth) {
            return None;
        }
        Some(OutgoingDelegation { parent, requested })
    }
}

/// The sealed parent authority stays within the current policy and the
/// remaining turn allowance.
fn authority_within_policy(
    parent: &CapabilityConstraints,
    trusted: &TrustedParts,
    policy: &SecurityPolicy,
    max_depth: u8,
) -> bool {
    let level_allowed = match policy.autonomy {
        AutonomyLevel::ReadOnly => parent.authority.level == AutonomyLevel::ReadOnly,
        AutonomyLevel::Supervised => parent.authority.level != AutonomyLevel::Full,
        AutonomyLevel::Full => true,
    };
    level_allowed
        && (trusted.side_effect_mode != SideEffectMode::ReadOnly || parent.authority.level == AutonomyLevel::ReadOnly)
        && parent.authority.max_cost_cents <= policy.max_cost_per_day_cents
        && u64::from(parent.authority.max_cost_cents).saturating_mul(10_000) <= trusted.budgets.micro_usd
        && parent.authority.remaining_depth <= max_depth
}

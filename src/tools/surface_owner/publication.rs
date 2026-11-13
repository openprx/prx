//! Execution-owned publication rules (R28).
//!
//! Which catalog entries a turn publishes, offers through discovery or binds
//! for a delegator, computed from the turn's pinned trusted scope, its
//! admission facts and the catalog. These rules never read an engine
//! decision, a directive or gate inputs. Publication is never dispatch
//! authority: binding and dispatch recheck the live catalog, source, schema,
//! security and approval policy.
//!
//! Full autonomy has no permission limits (R51): read-only and child-manifest
//! restrictions derived from autonomy do not restrict a Full turn. Structural
//! rules (exposure, catalog parity, owner-isolation identity, the historical
//! resume scope and plan-mode write interception) apply in every mode.

use std::collections::BTreeSet;

use crate::capability::CapabilityAvailabilityLevel;
use crate::security::AutonomyLevel;
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityEntry, CatalogView, Exposure};
use crate::tools::capability::contract::{CapabilityRef, CostEstimate, SideEffectClass};
use crate::tools::capability::resume_evidence::VerifiedResumeEvidenceV1;
use crate::tools::capability::turn_state::{SideEffectMode, TrustedParts, TurnSurfaceKind};
use crate::tools::execution::{ToolExecutionDecision, decide_tool_execution};

/// The publication outcome of one capability. `ApprovalRequired` is never
/// executable publication; it may only be retained as a pending admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationAdmission {
    Passed,
    ApprovalRequired,
    Blocked { gate: u8, reason_code: &'static str },
}

impl PublicationAdmission {
    pub(crate) const fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub(crate) const fn is_passed(self) -> bool {
        matches!(self, Self::Passed)
    }

    pub(crate) const fn is_pending(self) -> bool {
        matches!(self, Self::ApprovalRequired)
    }
}

const fn blocked(gate: u8, reason_code: &'static str) -> PublicationAdmission {
    PublicationAdmission::Blocked { gate, reason_code }
}

/// Public names that are delegators: a child is spawned only through them.
pub(crate) fn is_delegator(public_name: &str) -> bool {
    matches!(public_name, "delegate" | "sessions_spawn")
}

/// One turn's publication rules over its pinned scope and facts.
pub(crate) struct PublicationRules<'a, 'c> {
    pub(crate) trusted: &'a TrustedParts,
    pub(crate) facts: &'a AdmissionFactsV1,
    pub(crate) catalog: &'a CatalogView<'c>,
}

impl PublicationRules<'_, '_> {
    fn full_autonomy(&self) -> bool {
        self.facts.policy.autonomy == AutonomyLevel::Full
    }

    fn has_constraint(&self, code: &str) -> bool {
        self.trusted.constraints.iter().any(|item| item.as_str() == code)
    }

    /// The turn's own scope still matches its facts and catalog: workspace,
    /// owner and inherited child seal (G1), catalog and model-admission
    /// generation (G3) and the pinned serving model (G7).
    pub(crate) fn turn_scope_admits(&self) -> bool {
        let trusted = self.trusted;
        let generation = self.catalog.generation();
        let child_valid = self.facts.child_scope().is_none_or(|(manifest, parent)| {
            manifest.verify(parent).is_ok()
                && parent.authority.workspace == trusted.workspace_id.as_ref()
                && parent.authority.owner == self.facts.owner_id.as_ref()
                && manifest.catalog_generation == generation
        });
        !trusted.workspace_id.is_empty()
            && trusted
                .owner_id
                .as_deref()
                .is_none_or(|owner| owner == self.facts.owner_id.as_ref())
            && child_valid
            && trusted.channel.as_deref().is_none_or(|channel| !channel.is_empty())
            && trusted.catalog_generation == generation
            && trusted
                .model_admission
                .as_ref()
                .is_none_or(|policy| policy.matches_layout(generation.0, self.catalog.model_admission_layout()))
            && self.facts.model_ref.as_ref() == trusted.resolved_model.as_ref()
            && self.facts.model_can_chat
    }

    /// Structural turn restrictions a publication can never undo: tools off,
    /// plan-mode write interception and (outside Full) read-only mode.
    /// `resume_exact_call` additionally restricts every capability except the
    /// historical resume's prepared one.
    pub(crate) fn restricts(&self, entry: &CapabilityEntry, resume_scope: bool) -> bool {
        (resume_scope && self.has_constraint("resume_exact_call"))
            || self.has_constraint("tools_off")
            || (self.has_constraint("intercepts_writes") && entry.plan_intercepted())
            || (!self.full_autonomy()
                && self.trusted.side_effect_mode == SideEffectMode::ReadOnly
                && entry.effect > SideEffectClass::ReadOnly)
    }

    /// The side-effect rules discovery applies: plan-mode write interception
    /// and (outside Full) read-only mode. Discovery never offers past these.
    pub(crate) fn restricts_discovery(&self, entry: &CapabilityEntry) -> bool {
        (self.has_constraint("intercepts_writes") && entry.plan_intercepted())
            || (!self.full_autonomy()
                && self.trusted.side_effect_mode == SideEffectMode::ReadOnly
                && entry.effect > SideEffectClass::ReadOnly)
    }

    /// The owner-isolation identity rule: an owner-isolated turn publishes
    /// only owner-aware capabilities, or the private filesystem roots when
    /// that filesystem is bound.
    pub(crate) fn owner_isolation_admits(&self, entry: &CapabilityEntry) -> bool {
        !self.trusted.owner_isolated()
            || crate::tools::capability::policy::admits_owner_isolated_identity(&entry.public_name, &entry.root)
            || (self.has_constraint("owner_private_filesystem_v1")
                && entry.public_name == entry.root
                && matches!(entry.public_name.as_str(), "file_read" | "file_write" | "file_edit"))
    }

    /// The inherited child manifest admits `reference` (outside Full).
    pub(crate) fn child_membership_admits(&self, reference: &CapabilityRef) -> bool {
        self.full_autonomy()
            || self
                .facts
                .child_scope()
                .is_none_or(|(manifest, _)| manifest.allowed.contains(reference))
    }

    /// Publication admission of one capability (scope, owner isolation, turn
    /// controls, exclusions, catalog parity, policy and schema). Approval is
    /// required for an effectful capability under approval-required mode or a
    /// Supervised child, and for a policy Ask, unless `approval_verified`.
    pub(crate) fn capability(
        &self,
        entry: &CapabilityEntry,
        reference: &CapabilityRef,
        delegate: bool,
        estimated_cost: Option<CostEstimate>,
        approval_verified: bool,
    ) -> PublicationAdmission {
        let trusted = self.trusted;
        let generation = self.catalog.generation();
        let full = self.full_autonomy();
        let child_valid = self.facts.child_scope().is_none_or(|(manifest, parent)| {
            manifest.verify(parent).is_ok()
                && parent.authority.workspace == trusted.workspace_id.as_ref()
                && parent.authority.owner == self.facts.owner_id.as_ref()
                && manifest.catalog_generation == generation
                && (full
                    || !(manifest.authority.level == AutonomyLevel::ReadOnly
                        && entry.effect > SideEffectClass::ReadOnly))
                && !(delegate && manifest.authority.remaining_depth == 0)
                && estimated_cost.is_some_and(|cost| {
                    cost.micro_usd <= u64::from(manifest.authority.max_cost_cents).saturating_mul(10_000)
                })
                && (full || manifest.allowed.contains(reference))
        });
        if trusted.workspace_id.is_empty()
            || trusted
                .owner_id
                .as_deref()
                .is_some_and(|owner| owner != self.facts.owner_id.as_ref())
            || !child_valid
            || trusted.channel.as_deref().is_some_and(str::is_empty)
        {
            return blocked(1, "scope");
        }
        if !self.owner_isolation_admits(entry) {
            return blocked(1, "owner_isolation");
        }
        if entry.public_name == "capability_search" {
            return blocked(1, "turn_control_not_candidate");
        }
        if !delegate && is_delegator(&entry.public_name) {
            return blocked(1, "delegation_requires_manifest");
        }
        if self.catalog.surface_excludes(entry)
            || trusted
                .exclusions
                .iter()
                .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
            || trusted.model_admission.as_ref().is_some_and(|policy| {
                policy.matches_layout(generation.0, self.catalog.model_admission_layout())
                    && policy.excludes(self.catalog.model_admission_entry(entry))
            })
            || (trusted.channel.is_some() && !entry.channel_eligible)
        {
            return blocked(2, "excluded");
        }
        if trusted.catalog_generation != generation
            || trusted
                .model_admission
                .as_ref()
                .is_some_and(|policy| !policy.matches_layout(generation.0, self.catalog.model_admission_layout()))
            || reference.generation != generation
            || entry.generation != generation
            || !self.catalog.contains(&entry.id)
            || matches!(entry.exposure, Exposure::Unavailable { .. })
            || entry.availability.level < CapabilityAvailabilityLevel::Configured
        {
            return blocked(3, "unavailable_or_stale");
        }
        let decision = decide_tool_execution(
            &self.facts.policy,
            entry.policy_tool_name(),
            &self.facts.sender,
            &self.facts.policy_channel,
            &self.facts.chat_type,
        );
        if decision == ToolExecutionDecision::Deny {
            return blocked(4, "policy_deny");
        }
        let policy_asks = decision == ToolExecutionDecision::Ask;
        // Full has no permission limits (R51): a Supervised manifest does not
        // make a Full turn's discoveries approval-required.
        let child_supervised = !full
            && self
                .facts
                .child_scope()
                .is_some_and(|(manifest, _)| manifest.authority.level == AutonomyLevel::Supervised);
        let approval_required = !full
            && entry.effect > SideEffectClass::ReadOnly
            && (trusted.side_effect_mode == SideEffectMode::ApprovalRequired || child_supervised);
        if reference.fingerprint != entry.fingerprint {
            return blocked(5, "schema_changed");
        }
        if (approval_required || policy_asks) && !approval_verified {
            PublicationAdmission::ApprovalRequired
        } else {
            PublicationAdmission::Passed
        }
    }

    /// The bounded direct core the serving model sees while it decides
    /// whether to answer or call a tool: every Direct entry, plus a
    /// delegator when the turn sealed an outgoing child manifest on a
    /// non-interactive surface. Entries restricted by the turn or blocked by
    /// [`Self::capability`] are not published.
    pub(crate) fn direct_surface(
        &self,
        outgoing_child: bool,
        estimate: &dyn Fn(usize) -> Option<CostEstimate>,
    ) -> BTreeSet<CapabilityRef> {
        if !self.facts.model_can_call_tools {
            return BTreeSet::new();
        }
        self.catalog
            .iter_entries()
            .filter_map(|entry| {
                let delegate = is_delegator(&entry.public_name);
                let reference = entry_ref(entry);
                let directly_exposed = matches!(entry.exposure, Exposure::Direct);
                let minted_delegator =
                    delegate && outgoing_child && self.trusted.surface != TurnSurfaceKind::Interactive;
                if !directly_exposed && !minted_delegator {
                    return None;
                }
                if self.restricts(entry, true) {
                    return None;
                }
                let cost = self.facts.child_scope().and_then(|_| estimate(entry.schema_bytes));
                (!self.capability(entry, &reference, delegate, cost, false).is_blocked()).then_some(reference)
            })
            .collect()
    }

    /// V-17: a historical resume turn publishes exactly its prepared
    /// capability, whose approval the authenticated evidence verifies.
    pub(crate) fn resume_surface(
        &self,
        evidence: &VerifiedResumeEvidenceV1,
        estimate: &dyn Fn(usize) -> Option<CostEstimate>,
    ) -> BTreeSet<CapabilityRef> {
        if !self.facts.model_can_call_tools {
            return BTreeSet::new();
        }
        let reference = evidence.capability();
        let Some(entry) = self.catalog.entry_any(&reference.id) else {
            return BTreeSet::new();
        };
        if is_delegator(&entry.public_name) || self.restricts(entry, false) {
            return BTreeSet::new();
        }
        let cost = self.facts.child_scope().and_then(|_| estimate(entry.schema_bytes));
        if self.capability(entry, reference, false, cost, true).is_blocked() {
            return BTreeSet::new();
        }
        BTreeSet::from([reference.clone()])
    }

    /// The turn's published surface: the resume surface on a historical
    /// resume turn, else the direct core.
    pub(crate) fn surface(
        &self,
        resume: Option<&VerifiedResumeEvidenceV1>,
        outgoing_child: bool,
        estimate: &dyn Fn(usize) -> Option<CostEstimate>,
    ) -> BTreeSet<CapabilityRef> {
        resume.map_or_else(
            || self.direct_surface(outgoing_child, estimate),
            |evidence| self.resume_surface(evidence, estimate),
        )
    }

    /// A delegator in the published surface may bind the turn's sealed
    /// outgoing child manifest: it is a live delegator entry with the same
    /// schema and generation, published directly or minted on a
    /// non-interactive surface.
    pub(crate) fn admits_published_delegator(&self, published: &BTreeSet<CapabilityRef>, outgoing_child: bool) -> bool {
        published.iter().any(|reference| {
            self.catalog.entry_any(&reference.id).is_some_and(|entry| {
                entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
                    && is_delegator(&entry.public_name)
                    && (matches!(entry.exposure, Exposure::Direct)
                        || outgoing_child && self.trusted.surface != TurnSurfaceKind::Interactive)
            })
        })
    }
}

/// The exact catalog reference of `entry`.
pub(crate) fn entry_ref(entry: &CapabilityEntry) -> CapabilityRef {
    CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    }
}

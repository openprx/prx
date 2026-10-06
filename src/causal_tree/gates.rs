//! Pure, ordered admission checks for causal candidates.
//!
//! Runtime supplies the policy and model facts from the same pinned turn. A
//! decision backend cannot construct or override these inputs.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::branch::{
    ApprovalRequirement, CandidateId, CapabilityRef, CausalActionKind, CausalCandidate, OpId, SideEffectClass,
};
use super::state::{CausalState, SideEffectMode};
use crate::capability::CapabilityAvailabilityLevel;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CatalogView, Exposure};
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest};
use crate::tools::execution::{ToolExecutionDecision, decide_tool_execution};

/// The same checks run before prediction, after read-only rehearsal, and
/// immediately before committing a directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatePass {
    PrePrediction,
    PostRehearsal,
    PreCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    Passed,
    Blocked { gate: u8, reason_code: &'static str },
    ApprovalOnly { op_ids: Vec<OpId> },
    ApprovalDeferred,
}

#[derive(Debug, Clone)]
pub(crate) struct GateReport {
    pub outcomes: BTreeMap<CandidateId, GateOutcome>,
    pub gate_set_hash: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GateError {
    #[error("duplicate candidate id in gate input")]
    DuplicateCandidate,
    #[error("candidate set has no refusal path")]
    MissingRefusal,
    #[error("candidate limit must reserve at least one working and two terminal paths")]
    InvalidCandidateLimit,
}

/// Runtime-only inputs that are not yet fields of `CausalState`. The
/// caller pins this snapshot to the same generation as the catalog and state.
pub(crate) struct GateInputs<'a> {
    pub policy: &'a SecurityPolicy,
    /// Principal owner, separate from the actor string used by tool scope ACLs.
    pub owner_id: &'a str,
    /// The exact actor and channel the execution service will use for policy.
    pub sender: &'a str,
    pub policy_channel: &'a str,
    pub chat_type: &'a str,
    pub model_ref: &'a str,
    pub model_can_chat: bool,
    pub model_can_call_tools: bool,
    pub decision_endpoint_ready: bool,
    pub child_scope: Option<(&'a ChildCapabilityManifest, &'a CapabilityConstraints)>,
    /// Exact call fingerprints whose scoped grants were verified by runtime.
    /// The fingerprint binds arguments, target, operation and candidate.
    /// Exact capability scope independently authenticated with each call.
    pub verified_approval_calls: BTreeMap<[u8; 32], BTreeSet<CapabilityRef>>,
    pub call_fingerprints: &'a BTreeMap<CandidateId, [u8; 32]>,
    pub failed_call_counts: BTreeMap<[u8; 32], u32>,
    pub replayed_non_idempotent: &'a BTreeSet<[u8; 32]>,
    /// No generic runtime limit exists; absence must not become a zero-retry rule.
    pub retry_limit: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GateScope {
    Directive,
    CapabilityPublication,
}

pub(crate) struct HardGates;

impl HardGates {
    pub(crate) fn apply(
        pass: GatePass,
        state: &CausalState,
        catalog: &CatalogView<'_>,
        candidates: &[CausalCandidate],
        inputs: &GateInputs<'_>,
    ) -> Result<GateReport, GateError> {
        Self::apply_with_remaining_budget(pass, state, catalog, candidates, inputs, state.trusted.parts().budgets)
    }

    /// Recheck with a monotonically narrowed runtime accounting snapshot. The
    /// compiled state remains immutable for request/record fingerprint parity.
    pub(crate) fn apply_with_remaining_budget(
        pass: GatePass,
        state: &CausalState,
        catalog: &CatalogView<'_>,
        candidates: &[CausalCandidate],
        inputs: &GateInputs<'_>,
        remaining: super::state::TurnBudgets,
    ) -> Result<GateReport, GateError> {
        let original = state.trusted.parts().budgets;
        let remaining = super::state::TurnBudgets {
            tokens: original.tokens.min(remaining.tokens),
            micro_usd: original.micro_usd.min(remaining.micro_usd),
            latency_ms: original.latency_ms.min(remaining.latency_ms),
        };
        let _ = pass;
        let mut outcomes = BTreeMap::new();
        for candidate in candidates {
            if outcomes
                .insert(
                    candidate.candidate_id().clone(),
                    Self::check(state, catalog, candidate, inputs, GateScope::Directive, remaining),
                )
                .is_some()
            {
                return Err(GateError::DuplicateCandidate);
            }
        }
        Ok(GateReport {
            outcomes,
            gate_set_hash: Sha256::digest(b"prx-cdp-gates-v1:G1,G2,G3,G4,G5,G6,G7,G8,G9").into(),
        })
    }

    /// Evaluate one transient generator probe without allocating a report.
    /// Final candidate sets still use `apply` for duplicate detection and
    /// durable gate evidence.
    pub(crate) fn check_one(
        _pass: GatePass,
        state: &CausalState,
        catalog: &CatalogView<'_>,
        candidate: &CausalCandidate,
        inputs: &GateInputs<'_>,
    ) -> GateOutcome {
        Self::check(
            state,
            catalog,
            candidate,
            inputs,
            GateScope::Directive,
            state.trusted.parts().budgets,
        )
    }

    /// G1-G5 for a discovered definition, without authorizing dispatch.
    /// Model/structural restrictions and actual call approval/budget checks
    /// remain with the caller and executor. Approval outcomes stay explicit.
    pub(crate) fn check_capability_publication(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        candidate: &CausalCandidate,
        inputs: &GateInputs<'_>,
    ) -> GateOutcome {
        Self::check(
            state,
            catalog,
            candidate,
            inputs,
            GateScope::CapabilityPublication,
            state.trusted.parts().budgets,
        )
    }

    fn check(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        candidate: &CausalCandidate,
        inputs: &GateInputs<'_>,
        scope: GateScope,
        remaining: super::state::TurnBudgets,
    ) -> GateOutcome {
        let trusted = state.trusted.parts();
        let references = candidate.required_capabilities();
        let single_entry;
        let multiple_entries;
        let entries: &[Option<&crate::tools::capability::catalog::CapabilityEntry>] = match references.len() {
            0 => &[],
            1 => {
                single_entry = [references
                    .first()
                    .and_then(|reference| catalog.entry_any(&reference.id))];
                &single_entry
            }
            _ => {
                multiple_entries = references
                    .iter()
                    .map(|reference| catalog.entry_any(&reference.id))
                    .collect::<Vec<_>>();
                &multiple_entries
            }
        };

        // G1: authenticated owner and workspace scope, including child scope.
        let child_valid = inputs.child_scope.is_none_or(|(manifest, parent)| {
            manifest.verify(parent).is_ok()
                && parent.authority.workspace == trusted.workspace_id.as_ref()
                && parent.authority.owner == inputs.owner_id
                && manifest.catalog_generation == catalog.generation()
                && !(manifest.authority.level == crate::security::AutonomyLevel::ReadOnly
                    && effective_effect(candidate, entries) > SideEffectClass::ReadOnly)
                && !(candidate.action() == CausalActionKind::Delegate && manifest.authority.remaining_depth == 0)
                && candidate.estimated_cost().is_some_and(|cost| {
                    cost.micro_usd <= u64::from(manifest.authority.max_cost_cents).saturating_mul(10_000)
                })
                && candidate
                    .required_capabilities()
                    .iter()
                    .all(|reference: &CapabilityRef| manifest.allowed.contains(reference))
        });
        if trusted.workspace_id.is_empty()
            || (trusted.owner_id.is_some() && trusted.owner_id.as_deref() != Some(inputs.owner_id))
            || !child_valid
            || trusted.channel.as_deref().is_some_and(|channel| channel.is_empty())
        {
            return blocked(1, "scope");
        }

        // Approval cannot make an owner-unaware backend safe for this origin.
        // Check before G4, including the argument-collection candidate path.
        if trusted.owner_isolated()
            && entries.iter().flatten().any(|entry| {
                !(crate::tools::capability::policy::admits_owner_isolated_identity(&entry.public_name, &entry.root)
                    || (trusted
                        .constraints
                        .iter()
                        .any(|code| code.as_str() == "owner_private_filesystem_v1")
                        && entry.public_name == entry.root
                        && matches!(entry.public_name.as_str(), "file_read" | "file_write" | "file_edit")))
            })
        {
            return blocked(1, "owner_isolation");
        }

        // Discovery is a turn control port, never a selected business action
        // or a capability through which another discovery can be proposed.
        if entries
            .iter()
            .flatten()
            .any(|entry| entry.public_name == "capability_search")
        {
            return blocked(1, "turn_control_not_candidate");
        }

        // Spawning is a Delegate action, never an ordinary tool selection.
        // Otherwise execution binds no child grant and a selected tool can
        // enter an unconstrained legacy child path. Recheck at every pass,
        // including replay/resume candidates that bypass enumeration.
        if candidate.action() == CausalActionKind::UseTool
            && entries
                .iter()
                .flatten()
                .any(|entry| matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn"))
        {
            return blocked(1, "delegation_requires_manifest");
        }

        // G2: use the effective exclusions already computed by runtime from
        // global, channel and exact-model configuration.
        if entries.iter().flatten().any(|entry| {
            catalog.surface_excludes(entry)
                || (trusted
                    .exclusions
                    .iter()
                    .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
                    || trusted
                        .model_admission
                        .as_ref()
                        .is_some_and(|policy| policy.matches_catalog(catalog) && policy.excludes(catalog, entry)))
                || trusted.channel.is_some() && !entry.channel_eligible
        }) {
            return blocked(2, "excluded");
        }

        // G3: the catalog view, references and state must share a generation.
        if trusted.catalog_generation != catalog.generation()
            || trusted
                .model_admission
                .as_ref()
                .is_some_and(|policy| !policy.matches_catalog(catalog))
            || candidate
                .required_capabilities()
                .iter()
                .any(|reference| reference.generation != catalog.generation())
            || entries.iter().any(Option::is_none)
            || entries.iter().flatten().any(|entry| !catalog.contains(&entry.id))
            || entries.iter().flatten().any(|entry| {
                matches!(entry.exposure, Exposure::Unavailable { .. })
                    || entry.availability.level < CapabilityAvailabilityLevel::Configured
            })
        {
            return blocked(3, "unavailable_or_stale");
        }

        // G4: mirror the execution policy. A generic tool's exact OpIds are
        // call-argument-dependent; an empty list is never a grant.
        let mut approval_ids = Vec::new();
        let mut policy_asks = false;
        for entry in entries.iter().flatten() {
            let decision = decide_tool_execution(
                inputs.policy,
                &entry.public_name,
                inputs.sender,
                inputs.policy_channel,
                inputs.chat_type,
            );
            if decision == ToolExecutionDecision::Deny {
                return blocked(4, "policy_deny");
            }
            if decision == ToolExecutionDecision::Ask {
                policy_asks = true;
                approval_ids.extend(entry.op_ids.iter().cloned());
            }
        }
        let child_supervised = inputs
            .child_scope
            .is_some_and(|(manifest, _)| manifest.authority.level == crate::security::AutonomyLevel::Supervised);
        let approval_required = effective_effect(candidate, entries) > SideEffectClass::ReadOnly
            && (trusted.side_effect_mode == SideEffectMode::ApprovalRequired || child_supervised);
        let call_fingerprint = inputs.call_fingerprints.get(candidate.candidate_id());
        let grant_verified = call_fingerprint
            .and_then(|fingerprint| inputs.verified_approval_calls.get(fingerprint))
            .is_some_and(|approved| {
                !references.is_empty()
                    && approved.len() == references.len()
                    && approved.iter().all(|reference| references.contains(reference))
            });
        if matches!(candidate.approval(), ApprovalRequirement::Satisfied { .. }) && !grant_verified {
            return blocked(4, "unverified_approval");
        }
        if let ApprovalRequirement::Required { op_ids } = candidate.approval() {
            approval_ids.extend(op_ids.iter().cloned());
        }

        // G5: stale schema refs are rejected even if the id still exists.
        if candidate
            .required_capabilities()
            .iter()
            .zip(entries)
            .any(|(reference, entry)| entry.is_none_or(|entry| reference.fingerprint != entry.fingerprint))
        {
            return blocked(5, "schema_changed");
        }

        if scope == GateScope::CapabilityPublication {
            return approval_outcome(approval_required || policy_asks, grant_verified, approval_ids);
        }

        // G6: unknown cost or latency is not free for an executable path.
        if candidate.action() != CausalActionKind::Refuse {
            let budget = remaining;
            if candidate.estimated_cost().is_none_or(|estimate| {
                u64::from(estimate.tokens) > budget.tokens || estimate.micro_usd > budget.micro_usd
            }) || candidate
                .estimated_latency_ms()
                .is_none_or(|latency| u64::from(latency) > budget.latency_ms)
            {
                return blocked(6, "budget");
            }
        }

        // G7: operation support is a PMM-resolved model fact, never inferred
        // from the model name.
        let fixed_terminal = matches!(
            candidate.action(),
            CausalActionKind::Clarify | CausalActionKind::RequestApproval | CausalActionKind::Refuse
        );
        if (!fixed_terminal && inputs.model_ref != trusted.resolved_model.as_ref())
            || (!fixed_terminal && requires_tool_operation(candidate) && !inputs.model_can_call_tools)
            || (!fixed_terminal
                && !((approval_required || policy_asks) && !grant_verified)
                && (!inputs.model_can_chat || !inputs.decision_endpoint_ready))
        {
            return blocked(7, "model_operation");
        }

        // G8: repeated failed calls and non-idempotent replay are recorded by
        // runtime; the candidate cannot clear them by changing user text.
        // A turn plan names capabilities before model tool-call arguments
        // exist. Only a concrete call fingerprint can be compared with call
        // history here; dispatch must derive and require its own exact call
        // proof before executing the plan's allowed capability.
        if call_fingerprint.is_some_and(|fingerprint| {
            inputs.replayed_non_idempotent.contains(fingerprint)
                || inputs
                    .failed_call_counts
                    .get(fingerprint)
                    .is_some_and(|count| inputs.retry_limit.is_some_and(|limit| *count >= limit))
        }) {
            return blocked(8, "retry_or_replay");
        }

        // G9: only explicit structural restrictions; no prompt parsing.
        // A resume decision may reconsider or refuse, but cannot transfer the
        // authenticated approval to another executable candidate.
        if trusted
            .constraints
            .iter()
            .any(|code| code.as_str() == "resume_exact_call")
            && !references.is_empty()
            && !grant_verified
        {
            return blocked(9, "resume_scope");
        }
        if (trusted.constraints.iter().any(|code| code.as_str() == "tools_off") && requires_tool_operation(candidate))
            || (trusted
                .constraints
                .iter()
                .any(|code| code.as_str() == "intercepts_writes")
                && (entries
                    .iter()
                    .flatten()
                    .any(|entry| crate::tools::capability::policy::plan_intercepts_entry(entry))
                    || entries.is_empty() && candidate.side_effect() > SideEffectClass::ReadOnly))
            || (trusted.side_effect_mode == SideEffectMode::ReadOnly
                && effective_effect(candidate, entries) > SideEffectClass::ReadOnly)
        {
            return blocked(9, "explicit_restriction");
        }
        approval_outcome(approval_required || policy_asks, grant_verified, approval_ids)
    }
}

fn approval_outcome(required: bool, verified: bool, approval_ids: Vec<OpId>) -> GateOutcome {
    if required && !verified {
        let ids: BTreeSet<_> = approval_ids.into_iter().collect();
        if ids.is_empty() {
            GateOutcome::ApprovalDeferred
        } else {
            GateOutcome::ApprovalOnly {
                op_ids: ids.into_iter().collect(),
            }
        }
    } else {
        GateOutcome::Passed
    }
}

const fn blocked(gate: u8, reason_code: &'static str) -> GateOutcome {
    GateOutcome::Blocked { gate, reason_code }
}

fn requires_tool_operation(candidate: &CausalCandidate) -> bool {
    matches!(
        candidate.action(),
        CausalActionKind::UseTool | CausalActionKind::Discover | CausalActionKind::Delegate
    ) || !candidate.required_capabilities().is_empty()
}

fn effective_effect(
    candidate: &CausalCandidate,
    entries: &[Option<&crate::tools::capability::catalog::CapabilityEntry>],
) -> SideEffectClass {
    entries
        .iter()
        .flatten()
        .fold(candidate.side_effect(), |effect, entry| effect.max(entry.effect))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{
        CandidateSpec, CapabilityTarget, CatalogGeneration, CostEstimate, SchemaFingerprint,
    };
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::state::{ConstraintCode, TrustedState, fixtures};
    use crate::security::AutonomyLevel;
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::capability::child::AuthorityEnvelope;
    use crate::tools::mcp::McpCapabilitySpec;
    use crate::tools::traits::{Tool, ToolResult, ToolSpec};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    struct ProbeTool(&'static str);

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

    fn probe_candidate(reference: CapabilityRef) -> CausalCandidate {
        CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        )
    }

    fn probe_inputs<'a>(
        policy: &'a SecurityPolicy,
        _empty: &'a BTreeSet<[u8; 32]>,
        calls: &'a BTreeMap<CandidateId, [u8; 32]>,
        history: &'a BTreeSet<[u8; 32]>,
    ) -> GateInputs<'a> {
        GateInputs {
            policy,
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
            call_fingerprints: calls,
            failed_call_counts: history.iter().map(|fingerprint| (*fingerprint, 1)).collect(),
            replayed_non_idempotent: history,
            retry_limit: Some(0),
        }
    }

    #[test]
    fn owner_check_and_execution_scope_use_distinct_source_identities() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("memory_recall"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let entry = view.entries().first().expect("fixture capability");
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.scope_default_allow = false;
        policy.scope_rules.push(crate::config::ScopeRule {
            user: Some("agent".into()),
            channel: Some("cli".into()),
            chat_type: Some("private".into()),
            tools_allow: vec!["memory_recall".into()],
            tools_deny: Vec::new(),
            send_allow: Vec::new(),
            send_deny: Vec::new(),
            assign_allow: Vec::new(),
            assign_deny: Vec::new(),
        });
        assert_eq!(
            decide_tool_execution(&policy, "memory_recall", "agent", "cli", "private"),
            ToolExecutionDecision::Allow
        );
        assert_eq!(
            decide_tool_execution(&policy, "memory_recall", "owner-1", "terminal", "private"),
            ToolExecutionDecision::Deny
        );
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let mut inputs = probe_inputs(&policy, &empty, &calls, &empty);
        inputs.sender = "agent";
        inputs.policy_channel = "cli";
        inputs.chat_type = "private";
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed)
        );
        inputs.owner_id = "forged_owner";
        let denied = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            denied.outcomes.get(candidate.candidate_id()),
            Some(&blocked(1, "scope"))
        );
    }

    #[test]
    fn capability_outside_catalog_view_is_blocked_with_valid_reference() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("shell"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Full;
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &empty);
        let view = catalog.view(&CatalogScope {
            families: BTreeSet::from([CapabilityTarget::Memory]),
            channel: false,
        });
        assert!(view.entries().is_empty());
        assert!(view.entry_any(&entry.id).is_some());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(3, "unavailable_or_stale"))
        );
    }

    #[test]
    fn all_three_passes_reject_unknown_cost_and_keep_refusal_available() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "unmatched neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let inputs = GateInputs {
            policy: &policy,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "unavailable:model",
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
        let answer = CausalCandidate::new(
            CausalActionKind::Answer,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let refusal = CausalCandidate::new(
            CausalActionKind::Refuse,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 0,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(0),
            },
        );
        let view = catalog.view(&CatalogScope::default());
        let fixed_terminals: Vec<_> = [CausalActionKind::Clarify, CausalActionKind::RequestApproval]
            .into_iter()
            .map(|action| {
                CausalCandidate::new(
                    action,
                    CapabilityTarget::None,
                    CandidateSpec {
                        namespace: None,
                        required_capabilities: Vec::new(),
                        side_effect: SideEffectClass::ReadOnly,
                        approval: ApprovalRequirement::None,
                        estimated_cost: Some(CostEstimate {
                            tokens: 0,
                            micro_usd: 0,
                        }),
                        estimated_latency_ms: Some(0),
                    },
                )
            })
            .collect();
        for pass in [GatePass::PrePrediction, GatePass::PostRehearsal, GatePass::PreCommit] {
            let report = HardGates::apply(
                pass,
                &state,
                &view,
                &[
                    answer.clone(),
                    refusal.clone(),
                    fixed_terminals.first().unwrap().clone(),
                    fixed_terminals.get(1).unwrap().clone(),
                ],
                &inputs,
            )
            .unwrap();
            assert_eq!(report.outcomes.get(answer.candidate_id()), Some(&blocked(6, "budget")));
            assert_eq!(report.outcomes.get(refusal.candidate_id()), Some(&GateOutcome::Passed));
            for candidate in &fixed_terminals {
                assert_eq!(
                    report.outcomes.get(candidate.candidate_id()),
                    Some(&GateOutcome::Passed)
                );
            }
        }
    }

    #[test]
    fn approval_label_cannot_supply_its_own_grant() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let candidate = CausalCandidate::new(
            CausalActionKind::Answer,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::Satisfied {
                    grant_id: "untrusted-label".into(),
                },
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let inputs = GateInputs {
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
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(4, "unverified_approval"))
        );
    }

    #[test]
    fn plan_uses_resolved_execution_name_not_effect_labels() {
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(ProbeTool("grep")),
            Box::new(ProbeTool("skill_read")),
            Box::new(ProbeTool("memory_search")),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let mut state = fixtures::state(&fixtures::key(), "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        trusted.constraints.push(ConstraintCode::new("intercepts_writes"));
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };
        let history = BTreeSet::new();
        let calls = BTreeMap::new();
        let inputs = probe_inputs(&policy, &history, &calls, &history);
        for entry in view.entries() {
            // Existing descriptors make grep Act but skill_read Read. The
            // scheduler's established Plan names deliberately differ.
            let expected_intercept = entry.public_name == "skill_read";
            if entry.public_name == "grep" {
                assert_eq!(entry.effect, SideEffectClass::LocalWrite);
            } else {
                assert_eq!(entry.effect, SideEffectClass::ReadOnly);
            }
            let candidate = probe_candidate(CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            });
            let report = HardGates::apply(
                GatePass::PreCommit,
                &state,
                &view,
                std::slice::from_ref(&candidate),
                &inputs,
            )
            .unwrap();
            if expected_intercept {
                assert_eq!(
                    report.outcomes.get(candidate.candidate_id()),
                    Some(&blocked(9, "explicit_restriction"))
                );
            } else {
                assert!(matches!(
                    report.outcomes.get(candidate.candidate_id()),
                    Some(GateOutcome::Passed | GateOutcome::ApprovalOnly { .. } | GateOutcome::ApprovalDeferred)
                ));
            }
        }
        for action in [CausalActionKind::Answer, CausalActionKind::Retrieve] {
            let candidate = CausalCandidate::new(
                action,
                CapabilityTarget::Memory,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: Vec::new(),
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: Some(CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(1),
                },
            );
            let report = HardGates::apply(
                GatePass::PreCommit,
                &state,
                &view,
                std::slice::from_ref(&candidate),
                &inputs,
            )
            .unwrap();
            assert!(matches!(
                report.outcomes.get(candidate.candidate_id()),
                Some(GateOutcome::Passed | GateOutcome::ApprovalOnly { .. } | GateOutcome::ApprovalDeferred)
            ));
        }
    }

    #[test]
    fn ref_free_tool_actions_obey_model_and_explicit_restrictions() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        state.trusted = TrustedState::from_runtime(trusted.clone());
        let policy = SecurityPolicy::default();
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let mut inputs = GateInputs {
            policy: &policy,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: false,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        for action in [
            CausalActionKind::UseTool,
            CausalActionKind::Discover,
            CausalActionKind::Delegate,
        ] {
            let candidate = CausalCandidate::new(
                action,
                CapabilityTarget::NativeTool,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: Vec::new(),
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: Some(CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(1),
                },
            );
            let report = HardGates::apply(
                GatePass::PrePrediction,
                &state,
                &view,
                std::slice::from_ref(&candidate),
                &inputs,
            )
            .unwrap();
            assert_eq!(
                report.outcomes.get(candidate.candidate_id()),
                Some(&blocked(7, "model_operation"))
            );
        }
        inputs.model_can_call_tools = true;
        trusted.constraints.push(ConstraintCode::new("tools_off"));
        state.trusted = TrustedState::from_runtime(trusted);
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(9, "explicit_restriction"))
        );
        let mut unrestricted = state.trusted.parts().clone();
        unrestricted.constraints.retain(|code| code.as_str() != "tools_off");
        state.trusted = TrustedState::from_runtime(unrestricted);
        for action in [CausalActionKind::Discover, CausalActionKind::Delegate] {
            let candidate = CausalCandidate::new(
                action,
                CapabilityTarget::Session,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: Vec::new(),
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: Some(CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(1),
                },
            );
            let report = HardGates::apply(
                GatePass::PreCommit,
                &state,
                &view,
                std::slice::from_ref(&candidate),
                &inputs,
            )
            .unwrap();
            assert_eq!(
                report.outcomes.get(candidate.candidate_id()),
                Some(&GateOutcome::Passed)
            );
        }
    }

    #[test]
    fn empty_approval_scope_is_deferred_and_duplicate_ids_fail() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let inputs = GateInputs {
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
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::LocalWrite,
                approval: ApprovalRequirement::Required { op_ids: Vec::new() },
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let view = catalog.view(&CatalogScope::default());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::ApprovalDeferred)
        );
        assert_eq!(
            Some(&HardGates::check_one(
                GatePass::PrePrediction,
                &state,
                &view,
                &candidate,
                &inputs
            )),
            report.outcomes.get(candidate.candidate_id())
        );
        let scoped = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::LocalWrite,
                approval: ApprovalRequirement::Required {
                    op_ids: vec![OpId::new("prx:scoped")],
                },
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let scoped_report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&scoped),
            &inputs,
        )
        .unwrap();
        let scoped_outcome = HardGates::check_one(GatePass::PrePrediction, &state, &view, &scoped, &inputs);
        assert_eq!(
            scoped_report.outcomes.get(scoped.candidate_id()),
            Some(&GateOutcome::ApprovalOnly {
                op_ids: vec![OpId::new("prx:scoped")]
            })
        );
        assert_eq!(Some(&scoped_outcome), scoped_report.outcomes.get(scoped.candidate_id()));
        assert!(matches!(
            HardGates::apply(
                GatePass::PrePrediction,
                &state,
                &view,
                &[candidate.clone(), candidate],
                &inputs
            ),
            Err(GateError::DuplicateCandidate)
        ));
    }

    #[test]
    fn exclusions_generation_and_schema_report_their_own_first_gate() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted.clone());
        let policy = SecurityPolicy::default();
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let mut inputs = probe_inputs(&policy, &empty, &calls, &history);
        let view = catalog.view(&CatalogScope::default());
        let retrieval = CausalCandidate::new(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        inputs.model_can_call_tools = false;
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&retrieval),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(retrieval.candidate_id()),
            Some(&blocked(7, "model_operation"))
        );
        inputs.model_can_call_tools = true;
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&retrieval),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(retrieval.candidate_id()),
            Some(&GateOutcome::Passed)
        );
        let discover = CausalCandidate::new(
            CausalActionKind::Discover,
            CapabilityTarget::Memory,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&discover),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(discover.candidate_id()), Some(&GateOutcome::Passed));
        let mut stale = reference.clone();
        stale.generation = CatalogGeneration([91; 32]);
        let candidate = probe_candidate(stale);
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(3, "unavailable_or_stale"))
        );
        let mut changed = reference.clone();
        changed.fingerprint = SchemaFingerprint([92; 32]);
        let candidate = probe_candidate(changed);
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(5, "schema_changed"))
        );

        // Compact model exclusions keep G2 ahead of a stale capability ref.
        trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
            &registry,
            &view,
            "prx-model",
            &[],
            "prx-policy",
            &key,
        ));
        state.trusted = TrustedState::from_runtime(trusted.clone());
        let mut stale_model_ref = reference.clone();
        stale_model_ref.generation = CatalogGeneration([93; 32]);
        let model_candidate = probe_candidate(stale_model_ref);
        assert_eq!(
            HardGates::check_one(GatePass::PrePrediction, &state, &view, &model_candidate, &inputs),
            blocked(2, "excluded")
        );
        // A bitmap from a different slot layout cannot report a model-policy G2
        // or authorize a capability in this catalog. It is a stale G3 binding.
        let other_registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_other"))];
        let other_catalog =
            CapabilityCatalog::build(&other_registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
            &other_registry,
            &other_catalog.view(&CatalogScope::default()),
            "prx-model",
            &[],
            "prx-policy",
            &key,
        ));
        state.trusted = TrustedState::from_runtime(trusted.clone());
        assert_eq!(
            HardGates::check_one(
                GatePass::PrePrediction,
                &state,
                &view,
                &probe_candidate(reference.clone()),
                &inputs
            ),
            blocked(3, "unavailable_or_stale")
        );
        trusted.model_admission = None;
        state.trusted = TrustedState::from_runtime(trusted.clone());

        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("prx_read".to_string());
        let excluded_catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
        let hidden_view = excluded_catalog.view(&CatalogScope::default());
        let excluded_entry = hidden_view.entry_any(&reference.id).unwrap();
        let excluded_ref = CapabilityRef {
            id: excluded_entry.id.clone(),
            fingerprint: excluded_entry.fingerprint,
            generation: excluded_entry.generation,
        };
        trusted.catalog_generation = *excluded_catalog.generation();
        trusted.exclusions.push("prx_read".into());
        state.trusted = TrustedState::from_runtime(trusted);
        assert!(hidden_view.entries().is_empty());
        let candidate = probe_candidate(excluded_ref);
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &hidden_view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(2, "excluded"))
        );
        assert_eq!(
            Some(&HardGates::check_one(
                GatePass::PrePrediction,
                &state,
                &hidden_view,
                &candidate,
                &inputs
            )),
            report.outcomes.get(candidate.candidate_id())
        );
    }

    #[test]
    fn ordered_reference_lookup_keeps_missing_and_second_stale_refs_blocked() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let references: Vec<_> = view
            .entries()
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        assert_eq!(references.len(), 2);
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Full;
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &empty);
        let candidate_for = |required_capabilities| {
            CausalCandidate::new(
                CausalActionKind::UseTool,
                CapabilityTarget::NativeTool,
                CandidateSpec {
                    namespace: None,
                    required_capabilities,
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: Some(CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(1),
                },
            )
        };
        let missing_id =
            crate::causal_tree::branch::CapabilityId::new(CapabilityTarget::NativeTool, "native", "prx_missing")
                .unwrap();
        let mut missing = references.first().unwrap().clone();
        missing.id = missing_id;
        let single_missing = candidate_for(vec![missing.clone()]);
        assert_eq!(
            HardGates::check_one(GatePass::PrePrediction, &state, &view, &single_missing, &inputs),
            blocked(3, "unavailable_or_stale")
        );
        let second_missing = candidate_for(vec![references.first().unwrap().clone(), missing]);
        assert_eq!(
            HardGates::check_one(GatePass::PrePrediction, &state, &view, &second_missing, &inputs),
            blocked(3, "unavailable_or_stale")
        );
        let mut stale_second = references.get(1).unwrap().clone();
        stale_second.fingerprint = SchemaFingerprint([99; 32]);
        let second_stale = candidate_for(vec![references.first().unwrap().clone(), stale_second]);
        assert_eq!(
            HardGates::check_one(GatePass::PrePrediction, &state, &view, &second_stale, &inputs),
            blocked(5, "schema_changed")
        );
        let valid = candidate_for(references);
        assert_eq!(
            HardGates::check_one(GatePass::PrePrediction, &state, &view, &valid, &inputs),
            GateOutcome::Passed
        );
    }

    #[test]
    fn retry_gate_uses_exact_call_fingerprint() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let candidate = probe_candidate(CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        });
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let empty = BTreeSet::new();
        let mut calls = BTreeMap::from([(candidate.candidate_id().clone(), [1; 32])]);
        let mut history = BTreeSet::from([[2; 32]]);
        let view = catalog.view(&CatalogScope::default());
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed)
        );
        assert_eq!(
            Some(&HardGates::check_one(
                GatePass::PreCommit,
                &state,
                &view,
                &candidate,
                &inputs
            )),
            report.outcomes.get(candidate.candidate_id())
        );
        let mut inputs = probe_inputs(&policy, &empty, &calls, &history);
        inputs.retry_limit = None;
        inputs.failed_call_counts.insert([1; 32], 99);
        let absent_limit = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .expect("gate report");
        assert_eq!(
            absent_limit.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed),
            "failure facts alone do not invent a retry limit"
        );
        inputs.retry_limit = Some(2);
        inputs.failed_call_counts = BTreeMap::from([([1; 32], 1), ([2; 32], 99)]);
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed)
        );
        inputs.failed_call_counts.insert([1; 32], 2);
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(8, "retry_or_replay"))
        );
        assert_eq!(
            Some(&HardGates::check_one(
                GatePass::PreCommit,
                &state,
                &view,
                &candidate,
                &inputs
            )),
            report.outcomes.get(candidate.candidate_id())
        );
        history.insert([1; 32]);
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(8, "retry_or_replay"))
        );
        calls.clear();
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed)
        );
    }

    #[test]
    fn owner_isolation_blocks_before_missing_or_verified_approval() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("file_write"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let view = catalog.view(&CatalogScope::default());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::ApprovalDeferred)
        );
        let original = state.material_fingerprint(&key);
        let mut isolated = state.trusted.parts().clone();
        isolated.constraints.push(ConstraintCode::new("owner_isolated"));
        state.trusted = TrustedState::from_runtime(isolated);
        assert_ne!(state.material_fingerprint(&key), original);
        let calls = BTreeMap::from([(candidate.candidate_id().clone(), [1; 32])]);
        let mut approved = probe_inputs(&policy, &empty, &calls, &history);
        approved
            .verified_approval_calls
            .insert([1; 32], candidate.required_capabilities().iter().cloned().collect());
        for pass in [GatePass::PrePrediction, GatePass::PostRehearsal, GatePass::PreCommit] {
            for inputs in [&inputs, &approved] {
                assert_eq!(
                    HardGates::check_one(pass, &state, &view, &candidate, inputs),
                    blocked(1, "owner_isolation")
                );
            }
        }
    }

    #[test]
    fn generic_write_without_call_scoped_operation_is_never_approvable() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("file_write"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let policy = SecurityPolicy::default();
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let view = catalog.view(&CatalogScope::default());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::ApprovalDeferred)
        );
        let mut scoped_calls = BTreeMap::from([(candidate.candidate_id().clone(), [1; 32])]);
        let mut scoped = probe_inputs(&policy, &empty, &scoped_calls, &history);
        scoped
            .verified_approval_calls
            .insert([1; 32], candidate.required_capabilities().iter().cloned().collect());
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &scoped,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::Passed)
        );
        scoped.model_can_chat = false;
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &scoped,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(7, "model_operation"))
        );
        scoped.model_can_chat = true;
        scoped.decision_endpoint_ready = false;
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &scoped,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(7, "model_operation"))
        );
        scoped_calls.insert(candidate.candidate_id().clone(), [2; 32]);
        let mut changed = probe_inputs(&policy, &empty, &scoped_calls, &history);
        changed
            .verified_approval_calls
            .insert([1; 32], candidate.required_capabilities().iter().cloned().collect());
        let report = HardGates::apply(
            GatePass::PreCommit,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &changed,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&GateOutcome::ApprovalDeferred)
        );
    }

    #[test]
    fn mcp_alias_acl_denial_uses_executable_public_name() {
        let alias = "mcp__prx_server__write";
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "write".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: alias.into(),
                description: "Write remote data".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        });
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let full_view = catalog.view(&CatalogScope::default());
        let entry = full_view.entries().first().unwrap();
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::Mcp,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.scope_default_allow = true;
        policy.scope_rules.push(crate::config::ScopeRule {
            user: None,
            channel: None,
            chat_type: None,
            tools_allow: Vec::new(),
            tools_deny: vec![alias.into()],
            send_allow: Vec::new(),
            send_deny: Vec::new(),
            assign_allow: Vec::new(),
            assign_deny: Vec::new(),
        });
        assert_ne!(
            decide_tool_execution(&policy, "mcp_call", "owner-1", "terminal", "dm"),
            ToolExecutionDecision::Deny
        );
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        let view = catalog.view(&CatalogScope::default());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&candidate),
            &inputs,
        )
        .unwrap();
        assert_eq!(
            report.outcomes.get(candidate.candidate_id()),
            Some(&blocked(4, "policy_deny"))
        );
    }

    #[test]
    fn full_autonomy_gateway_keeps_configured_acting_tools() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("shell")), Box::new(ProbeTool("file_write"))];
        let surface = SurfacePolicy {
            allowed_tools: None,
            channel_allowed: true,
            ..SurfacePolicy::default()
        };
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = Some("gateway".into());
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = crate::security::AutonomyLevel::Full;
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let inputs = probe_inputs(&policy, &empty, &calls, &history);
        for entry in view.entries() {
            let candidate = CausalCandidate::new(
                CausalActionKind::UseTool,
                entry.family,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    }],
                    side_effect: entry.effect,
                    approval: entry.approval.clone(),
                    estimated_cost: Some(CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    }),
                    estimated_latency_ms: Some(1),
                },
            );
            let report = HardGates::apply(
                GatePass::PrePrediction,
                &state,
                &view,
                std::slice::from_ref(&candidate),
                &inputs,
            )
            .unwrap();
            assert_eq!(
                report.outcomes.get(candidate.candidate_id()),
                Some(&GateOutcome::Passed)
            );
        }
    }

    #[test]
    fn child_gate_checks_sealed_reference_and_authority() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let refs: Vec<_> = view
            .entries()
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let key = fixtures::key();
        let parent = CapabilityConstraints {
            decision_id: DecisionId::legacy(&[9; 32]),
            allowed: refs.iter().cloned().collect(),
            authority: AuthorityEnvelope {
                owner: "owner-1".into(),
                workspace: "ws-1".into(),
                level: AutonomyLevel::Full,
                max_cost_cents: 100,
                remaining_depth: 2,
            },
            catalog_generation: *catalog.generation(),
            key: key.clone(),
        };
        let manifest = ChildCapabilityManifest::derive_strict_subset(&parent, refs.get(..1).unwrap()).unwrap();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let mut policy = SecurityPolicy::default();
        policy.autonomy = AutonomyLevel::Full;
        let empty = BTreeSet::new();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let mut inputs = probe_inputs(&policy, &empty, &calls, &history);
        inputs.child_scope = Some((&manifest, &parent));
        let allowed = probe_candidate(refs.first().unwrap().clone());
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&allowed),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(allowed.candidate_id()), Some(&GateOutcome::Passed));
        let mut stale = refs.first().unwrap().clone();
        stale.fingerprint = SchemaFingerprint([99; 32]);
        let denied = probe_candidate(stale);
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&denied),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(denied.candidate_id()), Some(&blocked(1, "scope")));
        let mut forged = manifest.clone();
        forged.authority.owner = "other_owner".into();
        inputs.child_scope = Some((&forged, &parent));
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&allowed),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(allowed.candidate_id()), Some(&blocked(1, "scope")));
        let read_only_parent = CapabilityConstraints {
            decision_id: parent.decision_id,
            allowed: parent.allowed.clone(),
            authority: AuthorityEnvelope {
                level: AutonomyLevel::ReadOnly,
                ..parent.authority.clone()
            },
            catalog_generation: parent.catalog_generation,
            key: parent.key.clone(),
        };
        let read_only =
            ChildCapabilityManifest::derive_strict_subset(&read_only_parent, refs.get(..1).unwrap()).unwrap();
        inputs.child_scope = Some((&read_only, &read_only_parent));
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&allowed),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(allowed.candidate_id()), Some(&blocked(1, "scope")));

        let cost_parent = CapabilityConstraints {
            decision_id: parent.decision_id,
            allowed: parent.allowed.clone(),
            authority: AuthorityEnvelope {
                max_cost_cents: 0,
                ..parent.authority.clone()
            },
            catalog_generation: parent.catalog_generation,
            key: parent.key.clone(),
        };
        let no_cost = ChildCapabilityManifest::derive_strict_subset(&cost_parent, refs.get(..1).unwrap()).unwrap();
        inputs.child_scope = Some((&no_cost, &cost_parent));
        let costly = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![refs.first().unwrap().clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 1,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&costly),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(costly.candidate_id()), Some(&blocked(1, "scope")));

        let depth_parent = CapabilityConstraints {
            decision_id: parent.decision_id,
            allowed: parent.allowed.clone(),
            authority: AuthorityEnvelope {
                remaining_depth: 1,
                ..parent.authority.clone()
            },
            catalog_generation: parent.catalog_generation,
            key: parent.key.clone(),
        };
        let depth_zero = ChildCapabilityManifest::derive_strict_subset(&depth_parent, refs.get(..1).unwrap()).unwrap();
        inputs.child_scope = Some((&depth_zero, &depth_parent));
        let delegate = CausalCandidate::new(
            CausalActionKind::Delegate,
            CapabilityTarget::Session,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![refs.first().unwrap().clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(1),
            },
        );
        let report = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&delegate),
            &inputs,
        )
        .unwrap();
        assert_eq!(report.outcomes.get(delegate.candidate_id()), Some(&blocked(1, "scope")));
    }
}

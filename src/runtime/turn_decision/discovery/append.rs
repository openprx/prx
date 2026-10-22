//! Recheck only references actually offered by this live turn's search port.
use super::{
    BTreeMap, BTreeSet, CapabilityRef, CausalActionKind, Context, GateInputs, Ordering, ToolSpec, TurnDecisionContext,
    TurnDecisionService, TurnSourceProof,
};
use crate::causal_tree::branch::{CandidateSpec, CausalCandidate, SideEffectClass};
use crate::causal_tree::candidate_generator::CostEstimateSource;
use crate::causal_tree::gates::{GateOutcome, HardGates};
use crate::causal_tree::state::SideEffectMode;

/// Most discovered tools one turn may declare, counted across every search.
pub(crate) const MAX_DISCOVERED_TOOLS_PER_TURN: usize = 10;

/// A checked proposal, not an execution binding. Approval outcomes must be
/// handled before a caller publishes or dispatches the definition.
pub(crate) struct DiscoveredToolProposal {
    pub(crate) reference: CapabilityRef,
    pub(crate) root: String,
    pub(crate) spec: ToolSpec,
    pub(crate) admission: GateOutcome,
}

/// Created only from a freshly gated real offer. Pending entries may be bound
/// to the approval recorder but must never dispatch before durable approval.
#[derive(Clone)]
pub(in crate::runtime::turn_decision) struct DiscoveryAdmission {
    pub(in crate::runtime::turn_decision) reference: CapabilityRef,
    pub(in crate::runtime::turn_decision) requires_approval: bool,
}

impl TurnDecisionContext {
    pub(in crate::runtime::turn_decision) fn has_discovery_admission(&self, reference: &CapabilityRef) -> bool {
        self.discovery_admissions.get().is_some_and(|admissions| {
            admissions
                .lock()
                .values()
                .any(|admission| &admission.reference == reference)
        })
    }

    /// Terminal evidence only; callers must not treat these retained refs as
    /// authority after execution_closed. Live consumers recheck every offer.
    pub(crate) fn discovered_execution_refs(&self) -> Vec<CapabilityRef> {
        self.discovery_admissions.get().map_or_else(Vec::new, |admissions| {
            admissions
                .lock()
                .values()
                .filter(|admission| !admission.requires_approval)
                .map(|admission| admission.reference.clone())
                .collect()
        })
    }

    pub(in crate::runtime::turn_decision) fn discovery_admission(&self, name: &str) -> Option<DiscoveryAdmission> {
        self.discovery_admissions.get()?.lock().get(name).cloned()
    }
}

impl TurnDecisionService {
    /// Select an actually returned hit. Pending approval is retained as a
    /// separate state, never silently promoted to executable publication.
    pub(crate) fn admit_discovered_tool(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        name_or_id: &str,
    ) -> anyhow::Result<Option<DiscoveredToolProposal>> {
        let Some(proposal) = self.prepare_discovered_tool(proof, context, name_or_id)? else {
            return Ok(None);
        };
        if context.execution_refs.contains(&proposal.reference) {
            // An ID-form call may name an already selected capability. Keep
            // it in the immutable core instead of publishing it twice.
            return Ok(Some(proposal));
        }
        let requires_approval = match &proposal.admission {
            GateOutcome::Passed => false,
            GateOutcome::ApprovalOnly { .. } | GateOutcome::ApprovalDeferred => true,
            GateOutcome::Blocked { .. } => return Ok(Some(proposal)),
        };
        let mut admissions = context.discovery_admissions.get_or_init(Default::default).lock();
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire),
            "discovery admission turn is closed"
        );
        let admitted = DiscoveryAdmission {
            reference: proposal.reference.clone(),
            requires_approval,
        };
        if let Some(previous) = admissions.get(&proposal.spec.name) {
            anyhow::ensure!(
                previous.reference == admitted.reference && previous.requires_approval == admitted.requires_approval,
                "discovery admission changed within the turn"
            );
        } else {
            admissions.insert(proposal.spec.name.clone(), admitted);
        }
        Ok(Some(proposal))
    }

    /// Offer the hits of a completed `capability_search` to the NEXT model
    /// request. Every returned hit goes through the same gates as a call by
    /// name (`admit_discovered_tool`: `check_capability_publication`, which
    /// covers policy, capability, owner and exclusion; it does not apply G6),
    /// so the search result itself never grants anything; a hit that is
    /// blocked, pending approval or hidden by `excluded` is not published.
    /// The request already sent is not rewritten: call this before the next
    /// request's tool surface is built. Hits are taken in search-rank order, at
    /// most `MAX_SEARCH_HITS` newly admitted per call and
    /// `MAX_DISCOVERED_TOOLS_PER_TURN` admitted over the whole turn. Returns
    /// the count newly admitted.
    pub(crate) fn admit_offered_discoveries(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        excluded: impl Fn(&str, &str) -> bool,
    ) -> usize {
        self.admit_offered_discoveries_up_to(proof, context, MAX_DISCOVERED_TOOLS_PER_TURN, excluded)
    }

    /// [`Self::admit_offered_discoveries`] with an explicit per-turn cap.
    pub(crate) fn admit_offered_discoveries_up_to(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        turn_cap: usize,
        excluded: impl Fn(&str, &str) -> bool,
    ) -> usize {
        let Some(offers) = context.discovery_offers.get() else {
            return 0;
        };
        let names = offers.lock().names();
        let mut newly_admitted = 0;
        for name in names {
            let admitted_this_turn = context
                .discovery_admissions
                .get()
                .map_or(0, |admissions| admissions.lock().len());
            if newly_admitted >= crate::tools::capability::search_tool::MAX_SEARCH_HITS
                || admitted_this_turn >= turn_cap
            {
                break;
            }
            if context.discovery_admission(&name).is_some() {
                continue;
            }
            let proposal = match self.prepare_discovered_tool(proof, context, &name) {
                Ok(Some(proposal)) => proposal,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(tool = %name, %error, "discovered capability is not offered to the next request");
                    continue;
                }
            };
            // A core capability is already declared; it is never "newly admitted".
            if context.execution_refs().contains(&proposal.reference) {
                continue;
            }
            if proposal.admission != GateOutcome::Passed || excluded(&proposal.spec.name, &proposal.root) {
                continue;
            }
            match self.admit_discovered_tool(proof, context, &name) {
                Ok(Some(admitted)) if admitted.admission == GateOutcome::Passed => newly_admitted += 1,
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(tool = %name, %error, "discovered capability admission was refused");
                }
            }
        }
        newly_admitted
    }

    /// Resolve only separately admitted definitions, with fresh source and
    /// schema gates. This never changes the original decision's exact refs.
    pub(crate) fn agent_appended_execution_specs(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<super::super::execution::ConstrainedToolSelection> {
        let names = context.discovery_admissions.get().map_or_else(Vec::new, |admissions| {
            admissions
                .lock()
                .iter()
                .filter(|(_, admission)| !admission.requires_approval)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        });
        let mut roots = BTreeSet::new();
        let mut specs = Vec::with_capacity(names.len());
        for name in names {
            let proposal = self
                .prepare_discovered_tool(proof, context, &name)?
                .context("admitted discovery offer is missing")?;
            let admitted = context
                .discovery_admission(&name)
                .context("discovery admission missing")?;
            anyhow::ensure!(
                !admitted.requires_approval
                    && admitted.reference == proposal.reference
                    && proposal.admission == GateOutcome::Passed,
                "appended capability no longer passes its gates"
            );
            roots.insert(proposal.root);
            specs.push(proposal.spec);
        }
        Ok(super::super::execution::ConstrainedToolSelection { roots, specs })
    }

    pub(crate) fn prepare_discovered_tool(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        name_or_id: &str,
    ) -> anyhow::Result<Option<DiscoveredToolProposal>> {
        let Some(offers) = context.discovery_offers.get() else {
            return Ok(None);
        };
        let reference = offers.lock().find(name_or_id);
        let Some(reference) = reference else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.agent_capability_search_spec(proof, context)?.is_some(),
            "turn no longer permits discovered capability admission"
        );
        let entries = self.catalog.resolve(std::slice::from_ref(&reference))?;
        let entry = entries.first().context("offered capability is absent")?;
        let trusted = context.engine.state.trusted.parts();
        // Publication gates stop at G5, but a discovery cannot undo the
        // original turn's structural restrictions or exact resume scope.
        anyhow::ensure!(
            !(trusted.constraints.iter().any(|code| {
                matches!(code.as_str(), "resume_exact_call" | "tools_off")
                    || code.as_str() == "intercepts_writes" && entry.plan_intercepted()
            }) || trusted.side_effect_mode == SideEffectMode::ReadOnly && entry.effect > SideEffectClass::ReadOnly),
            "discovered capability conflicts with turn restrictions"
        );
        let model = self.resolve_agent_model(proof)?;
        let observed_latency = BTreeMap::new();
        let costs =
            crate::runtime::turn_cost::RuntimeCostModel::new(&model, &self.generation.effective, &observed_latency);
        let estimate = costs.estimate_entry(entry);
        let candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: estimate.map(|(cost, _)| cost),
                estimated_latency_ms: estimate.map(|(_, latency)| latency),
            },
        );
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let call_fingerprints = BTreeMap::new();
        let replayed_non_idempotent = BTreeSet::new();
        let gates = GateInputs {
            policy: &self.security,
            owner_id: proof.owner_id(),
            sender: proof.policy_sender(),
            policy_channel: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            model_ref: &model_ref,
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            decision_endpoint_ready: true,
            child_scope: proof.child_scope().map(|scope| scope.gate_inputs()),
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &call_fingerprints,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed_non_idempotent,
            retry_limit: None,
        };
        let admission =
            HardGates::check_capability_publication(&context.engine.state, &self.catalog_view(), &candidate, &gates);
        let descriptor = self
            .execution
            .current_descriptor(&entry.public_name)
            .context("offered capability has no executable descriptor")?;
        let spec = ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        };
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &spec.name,
                &spec.description,
                &spec.parameters,
            ) == reference.fingerprint
                && !context.execution_closed.load(Ordering::Acquire),
            "offered capability changed or its turn closed"
        );
        Ok(Some(DiscoveredToolProposal {
            reference,
            root: entry.root.clone(),
            spec,
            admission,
        }))
    }
}

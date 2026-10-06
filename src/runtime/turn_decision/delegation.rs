//! Construct outgoing authority from exact mentions or admitted discoveries.
use super::{
    Arc, BTreeSet, CapabilityAvailabilityLevel, CapabilityRef, CatalogView, CausalState, ChildExecutionPolicy, Context,
    DecisionId, DiscoveredRefs, Exposure, Instant, Ordering, TurnDecisionContext, TurnDecisionService,
    TurnExecutionDirective, TurnSourceProof, TurnSurfaceKind,
};
use crate::tools::capability::child::{AuthorityEnvelope, CapabilityConstraints};

pub(super) struct OutgoingDelegation {
    pub parent: CapabilityConstraints,
    pub requested: Vec<CapabilityRef>,
}

impl TurnDecisionService {
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
        (crate::agent::classifier::classify_intent(task, proof.routing_input()).intent
            == crate::agent::classifier::TaskIntent::Delegate)
            .then_some("sessions_spawn")
    }

    /// Children may read the parent immediately, including in another process.
    /// Queue admission alone cannot authorize that launch.
    pub(super) async fn validate_delegation_storage(&self, context: Arc<TurnDecisionContext>) -> anyhow::Result<()> {
        let deadline = context.engine.deadline;
        anyhow::ensure!(
            Instant::now() < deadline,
            "delegation persistence exceeded the decision deadline"
        );
        let settlement = self.settlement_ref(Arc::clone(&context))?;
        tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            settlement.validate_durable_chain(),
        )
        .await
        .context("delegation persistence exceeded the decision deadline")??;
        let workspace = self.store.workspace_id().context("parent decision store missing")?;
        let trusted = context.engine.state.trusted.parts();
        let scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: Arc::clone(&workspace),
            owner_id: trusted.owner_id.clone().context("parent decision owner missing")?,
        };
        let records = crate::causal_tree::outcome::DecisionRecordBuilder::build_enabled_chain(
            &context,
            &self.state_key,
            true,
            self.generation.effective.causal_tree.store.record_redacted_projection,
        )?;
        let reader = self.store.reader();
        for record in records {
            anyhow::ensure!(
                Instant::now() < deadline,
                "delegation readback exceeded the decision deadline"
            );
            let stored = reader
                .replay_record(&scope, &record.decision_id)?
                .context("delegation parent hop was not durably stored")?;
            anyhow::ensure!(
                stored.digest == record.replay_digest(&workspace)?,
                "stored delegation parent differs from the issued decision chain"
            );
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "delegation readback exceeded the decision deadline"
        );
        Ok(())
    }

    /// Verify the selected subset independently of durable grant issuance.
    /// Disabled child provenance may retain this scope without claiming that
    /// a parent decision was persisted. Process transport is authenticated
    /// separately before reconstructing its ordinary source.
    pub(super) fn verified_delegation_scope(
        &self,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<crate::tools::capability::child::VerifiedChildScope> {
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire)
                && context.catalog_generation() == *self.catalog.generation(),
            "delegation scope is closed or belongs to another catalog"
        );
        let TurnExecutionDirective::Delegate { manifest } = &context.directive else {
            anyhow::bail!("selected decision is not delegation");
        };
        // derive_strict_subset preserves owner/level/cost and decrements depth
        // exactly once. Rebuild its original parent from the engine's retained
        // allowed refs, then reverify the seal instead of trusting wire fields.
        let mut authority = manifest.authority.clone();
        authority.remaining_depth = authority
            .remaining_depth
            .checked_add(1)
            .context("child depth cannot reconstruct issued parent")?;
        let parent = CapabilityConstraints {
            decision_id: context.decision_id(),
            allowed: context
                .engine
                .outgoing_parent_allowed
                .clone()
                .context("parent capabilities missing")?,
            authority,
            catalog_generation: context.catalog_generation(),
            key: Arc::clone(&self.state_key),
        };
        Ok(crate::tools::capability::child::VerifiedChildScope::new(
            manifest.clone(),
            &parent,
        )?)
    }

    pub(super) fn issue_child_grant(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Arc<crate::runtime::child_source::IssuedChildGrant>> {
        self.agent_execution_specs(proof, context)?;
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire)
                && context.record_admitted.load(Ordering::Acquire)
                && self.generation.effective.causal_tree.enabled
                && self.durable_key,
            "parent decision cannot issue child authority"
        );
        let scope = self.verified_delegation_scope(context)?;
        let store_workspace = self.store.workspace_id().context("parent decision store missing")?;
        let records = crate::causal_tree::outcome::DecisionRecordBuilder::build_enabled_chain(
            context,
            &self.state_key,
            true,
            self.generation.effective.causal_tree.store.record_redacted_projection,
        )?;
        let parent_digest = records
            .last()
            .context("parent decision record missing")?
            .replay_digest(&store_workspace)?;
        Ok(Arc::new(crate::runtime::child_source::IssuedChildGrant::new(
            Arc::new(scope),
            proof,
            Arc::clone(&self.catalog),
            Arc::clone(&self.generation),
            Arc::new(self.provider_catalog.clone()),
            (store_workspace, parent_digest),
            ChildExecutionPolicy {
                security: Arc::clone(&self.security),
            },
            crate::runtime::child_source::ChildGrantRuntime {
                decision_store: Arc::clone(&self.store),
                source_fabric: crate::memory::MemoryFabric::new(Arc::clone(&self.memory), proof.workspace_id())
                    .with_event_recording(self.generation.effective.memory.event_recording_config()),
                security: Arc::clone(&self.security),
            },
        )?))
    }

    pub(super) fn outgoing_delegation(
        &self,
        proof: &dyn TurnSourceProof,
        state: &CausalState,
        decision_id: DecisionId,
        discovered: &DiscoveredRefs,
        catalog: &CatalogView<'_>,
    ) -> Option<OutgoingDelegation> {
        // Ordinary turns without a discovery or an exact delegator name
        // keep the allocation-free absence path; never scan the catalog.
        // Enabled delegation requires a durable parent grant. Disabled turns
        // carry the same strict subset through an ordinary child publication.
        if self.generation.effective.causal_tree.enabled && (!self.durable_key || !self.recordable) {
            return None;
        }
        let direct = if discovered.iter().next().is_none() {
            let input = state.routing_input.as_str();
            // This proposal comes from the pinned configuration of a sealed
            // embedded source, not from synthetic text or operator identity.
            let configured = self.configured_delegation_target(proof).is_some();
            let names_delegator = configured
                || input
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .any(|word| word.eq_ignore_ascii_case("delegate") || word.eq_ignore_ascii_case("sessions_spawn"));
            if !names_delegator {
                return None;
            }
            crate::tools::capability::search::CatalogSearch::exact_mentions(
                catalog,
                input,
                usize::from(self.generation.effective.causal_tree.policy.max_candidates),
            )?
        } else {
            Vec::new()
        };
        let trusted = state.trusted.parts();
        let permitted = |entry: &crate::tools::capability::catalog::CapabilityEntry| {
            catalog.contains(&entry.id)
                && entry.generation == trusted.catalog_generation
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && !matches!(entry.exposure, Exposure::Unavailable { .. })
                && !trusted.excludes_entry(catalog, entry)
                && (trusted.channel.is_none() || entry.channel_eligible)
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
                    &entry.public_name,
                    proof.policy_sender(),
                    proof.policy_channel(),
                    proof.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny
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
        }
        if !allowed.iter().any(|reference| {
            catalog
                .entry_any(&reference.id)
                .is_some_and(|entry| matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn"))
        }) {
            return None;
        }
        let mut requested = Vec::new();
        let references = discovered.iter().cloned().chain(
            direct.into_iter()
                // The named parent entrypoint is not itself requested work.
                .filter(|entry| !matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn"))
                .map(|entry| CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }),
        );
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
            // A discovered delegation capability may be passed onward, but
            // only if the complete request still strictly narrows the parent.
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
        let level = if trusted.side_effect_mode == crate::causal_tree::state::SideEffectMode::ReadOnly {
            crate::security::AutonomyLevel::ReadOnly
        } else {
            self.security.autonomy
        };
        let parent = CapabilityConstraints {
            decision_id,
            allowed,
            authority: inherited.map_or_else(
                || AuthorityEnvelope {
                    owner: proof.owner_id().to_owned(),
                    workspace: proof.workspace_id().to_owned(),
                    level,
                    max_cost_cents,
                    remaining_depth,
                },
                |manifest| manifest.authority.clone(),
            ),
            catalog_generation: trusted.catalog_generation,
            key: Arc::clone(&self.state_key),
        };
        // Retaining the exact inherited authority must not bypass a tighter
        // current policy or the remaining turn allowance.
        if !crate::causal_tree::decision_engine::authority_within_policy(
            &parent,
            trusted,
            &self.security,
            parent.authority.remaining_depth,
        ) {
            return None;
        }
        Some(OutgoingDelegation { parent, requested })
    }
}

//! Source-bound selection of exact tool schemas for the shared turn loop.
use super::{
    Arc, CapabilityAvailabilityLevel, ConfigGeneration, Context, Memory, SecurityPolicy, ToolExecutionService,
    TurnDecisionContext, TurnDecisionService, TurnSourceProof, TurnSurfaceKind,
};
use crate::runtime::child_source::publication::OrdinaryChildPublication;
use crate::tools::ToolSpec;

impl TurnDecisionService {
    /// Local operator commands retain the actual Interactive publication.
    /// This validates source provenance without inventing a model decision.
    pub(crate) fn verify_operator_spawn_source(
        &self,
        source: &dyn TurnSourceProof,
        generation: &Arc<ConfigGeneration>,
        memory: &Arc<dyn Memory>,
        security: &Arc<SecurityPolicy>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.surface == TurnSurfaceKind::Interactive
                && Arc::ptr_eq(&self.generation, generation)
                && Arc::ptr_eq(&self.memory, memory)
                && Arc::ptr_eq(&self.security, security)
                && self.child_scope().is_none()
                && source.policy_channel() == "terminal"
                && source.policy_sender() == "local-user",
            "operator spawn differs from its actual runtime publication"
        );
        super::validate_agent_turn_origin(&self.generation, self.surface, source)?;
        self.check_live_parity()?;
        let entry = self
            .catalog
            .entry_for_public_name("sessions_spawn")
            .context("operator spawn capability is absent from its publication")?;
        anyhow::ensure!(
            self.catalog_view().contains(&entry.id)
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
                    entry.policy_tool_name(),
                    source.policy_sender(),
                    source.policy_channel(),
                    source.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny,
            "operator spawn capability is unavailable or denied by publication policy"
        );
        // Ask still requires the explicit command's resource-operation grant
        // at the existing spawn gate; source validation never mints approval.
        Ok(())
    }
}

impl TurnDecisionService {
    pub(crate) fn bind_agent_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        self.bind_execution(proof, context, execution_context, specs)
    }

    /// Bind the decision-owned capability set and scope while preserving an
    /// ingress-owned canonical execution service. Chat uses this for its
    /// approval router and preparation events. Every actual call still passes
    /// the issued execution binding, whose parity check covers the complete
    /// live catalog before the backend can run.
    pub(crate) fn bind_agent_execution_with_runtime(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
        runtime_execution: Option<Arc<ToolExecutionService>>,
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        let (published_execution, execution_context) =
            self.bind_agent_execution(proof, context, execution_context, specs)?;
        Ok((runtime_execution.unwrap_or(published_execution), execution_context))
    }

    fn bind_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        crate::tools::capability::execution_binding::specs::admit_bound_specs(
            &self.surface_port(context),
            proof,
            execution_context,
            specs,
        )?;
        self.verify_console_filesystem_context(proof, context)?;
        let mut execution_context = execution_context.clone();
        execution_context.execution_admission = Some(crate::tools::capability::execution_binding::ExecutionAdmission {
            catalog: Arc::clone(&self.catalog),
            scope: execution_context.envelope.clone(),
        });
        crate::runtime::child_source::turn_scope::carry_child_authority(
            &mut execution_context,
            proof,
            &self.catalog,
            &self.state_key,
        );
        // Approval routing belongs to the ingress builder, including an explicit None.
        execution_context.owner_filesystem = context.owner_filesystem.clone();
        execution_context.set_h2_origin(proof.h2_entry_origin().cloned());
        if let Some(case) = execution_context.controlled_case().cloned() {
            case.verify_config(&self.generation.effective)?;
            case.apply(&mut execution_context)?;
        }

        execution_context.capability_search = specs
            .iter()
            .any(|spec| spec.name == "capability_search")
            .then(|| self.capability_search_port(proof, context))
            .transpose()?;
        execution_context.ordinary_child_publication = (!self.generation.effective.causal_tree.enabled
            && specs
                .iter()
                .any(|spec| matches!(spec.name.as_str(), "delegate" | "sessions_spawn")))
        .then(|| {
            let scope = self.verified_delegation_scope(context)?;
            let (issued, parent) = scope.gate_inputs();
            // Retain the accounting owner before the parent model can spend.
            let allowance = context
                .runtime()
                .issue_recursive_allowance(proof)
                .context("ordinary parent budget owner missing")?;
            let authority = crate::runtime::child_source::ParentAuthority::for_run(
                proof.run_id(),
                parent.allowed.clone(),
                parent.authority.clone(),
                parent.catalog_generation,
                Arc::clone(&parent.key),
                allowance,
            );
            OrdinaryChildPublication::capture(
                self,
                context,
                proof,
                authority,
                &issued.allowed.iter().cloned().collect::<Vec<_>>(),
            )
        })
        .transpose()?;
        let delegation_bound = specs
            .iter()
            .any(|spec| matches!(spec.name.as_str(), "delegate" | "sessions_spawn"));
        execution_context.child_grant = if delegation_bound && self.generation.effective.causal_tree.enabled {
            Some(self.issue_child_grant(proof, context)?)
        } else if delegation_bound {
            {
                anyhow::ensure!(
                    execution_context.ordinary_child_publication.is_some(),
                    "ordinary delegation execution lacks its parent publication"
                );
            }
            None
        } else {
            None
        };
        if execution_context.call_budget.is_none() {
            execution_context.call_budget = Some(
                crate::agent::send_admission::call_token_budget(self, context, proof).context("called-tool budget")?,
            );
        }
        let trace = context.execution_trace.get_or_init(Default::default);
        trace.bind_exact_call_key(Arc::clone(&self.state_key))?;
        execution_context.execution_trace = Some(trace.clone());
        Ok((Arc::clone(&self.execution), execution_context))
    }

    /// Delegates to the R25 owner in `runtime::turn_terminal::silence` (until Part 2).
    pub(crate) fn record_agent_silence(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        compiled: &crate::agent::turn_context::CompiledToolContext,
    ) -> anyhow::Result<()> {
        crate::runtime::turn_terminal::silence::record_agent_silence(self, proof, context, compiled)
    }
}

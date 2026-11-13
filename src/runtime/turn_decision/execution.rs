//! Source-bound selection of exact tool schemas for the shared turn loop.
use super::{
    Arc, CancellationToken, CapabilityAvailabilityLevel, ConfigGeneration, Context, Memory, SecurityPolicy,
    ToolExecutionService, TurnDecisionContext, TurnDecisionService, TurnSourceProof, TurnSurfaceKind,
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

struct DecisionResumeDispatch {
    service: Arc<ToolExecutionService>,
    context: crate::tools::ToolExecutionContext,
    command: crate::tools::ToolExecutionCommand,
}

struct ResumeExecutionGuard<'a>(&'a TurnDecisionContext);
impl Drop for ResumeExecutionGuard<'_> {
    fn drop(&mut self) {
        self.0.terminal_execution_trace();
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
        anyhow::ensure!(
            context.resume.is_none(),
            "resumed execution requires the dedicated durable claim adapter"
        );
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

    /// Returns only the original prepared command and exact selected schema;
    /// a resume never asks a model to recreate approved arguments.
    fn bind_resume_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<DecisionResumeDispatch> {
        let resume = context.resume.as_ref().context("resume admission missing")?;
        let (command, dialect) = resume.command()?;
        // V-17: the resume turn's published surface (execution rules, R28)
        // is exactly the prepared tool, and the dispatch surface is
        // restricted to it.
        let selected = self.agent_execution_specs(proof, context)?;
        anyhow::ensure!(
            context.runtime().resume_tool_name() == Some(command.name.as_str())
                && selected.specs.len() == 1
                && selected.specs.first().is_some_and(|spec| spec.name == command.name),
            "resume surface differs from approved call"
        );
        let (service, mut execution_context) =
            self.bind_execution(proof, context, execution_context, &selected.specs)?;
        execution_context.turn_surface = execution_context
            .turn_surface
            .as_ref()
            .map(|surface| Arc::new(surface.restricted_to(&command.name)));
        execution_context.schema_dialect = dialect;
        execution_context.set_h2_origin(proof.h2_entry_origin().cloned());
        execution_context.set_h2_review(self.resume_local_owner_review(context, proof)?);
        execution_context.decision_resume = Some(crate::tools::capability::execution_binding::DecisionResumePort::new(
            Arc::new(
                crate::security::approval_ledger::historical::ExecutionResumeAdapter::new(
                    Arc::clone(self)
                        as Arc<dyn crate::security::approval_ledger::historical::HistoricalApprovalRuntime>,
                    Arc::new(super::pending_approval::DecisionResumeTurn::new(
                        Arc::clone(self),
                        Arc::clone(context),
                    )),
                    proof,
                    execution_context.envelope.clone(),
                ),
            ),
        ));
        Ok(DecisionResumeDispatch {
            service,
            context: execution_context,
            command: command.clone(),
        })
    }

    /// Execute exactly the approved call, without a model round trip. The
    /// owning ingress must durably finalize the returned outcome. Cancellation
    /// or dropping this future seals its trace and prevents further dispatch.
    pub(crate) async fn execute_agent_resume(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<crate::tools::ToolExecutionOutcome> {
        let dispatch = self.bind_resume_execution(proof, context, execution_context)?;
        let _guard = ResumeExecutionGuard(context);
        Ok(dispatch
            .service
            .execute(dispatch.command, dispatch.context, Some(cancellation.clone()))
            .await)
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
        execution_context.decision_resume = None;
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

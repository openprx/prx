//! Bind the turn's exact tool schemas, scope and child publication onto the
//! shared execution context.
use std::sync::Arc;

use anyhow::Context;

use super::OpenedTurn;
use crate::runtime::child_source::publication::OrdinaryChildPublication;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::ToolSpec;
use crate::tools::execution::ToolExecutionService;

impl TurnRuntimeService {
    pub(crate) fn bind_agent_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<OpenedTurn>,
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
        context: &Arc<OpenedTurn>,
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
        context: &Arc<OpenedTurn>,
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
        execution_context.ordinary_child_publication = specs
            .iter()
            .any(|spec| matches!(spec.name.as_str(), "delegate" | "sessions_spawn"))
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
        if execution_context.call_budget.is_none() {
            execution_context.call_budget = Some(
                crate::agent::send_admission::call_token_budget(self, context, proof).context("called-tool budget")?,
            );
        }
        let trace = context.runtime().execution_trace.get_or_init(Default::default);
        trace.bind_exact_call_key(Arc::clone(&self.state_key))?;
        execution_context.execution_trace = Some(trace.clone());
        Ok((Arc::clone(&self.execution), execution_context))
    }

    /// Delegates to the R25 owner in `runtime::turn_terminal::silence`.
    pub(crate) fn record_agent_silence(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        compiled: &crate::agent::turn_context::CompiledToolContext,
    ) -> anyhow::Result<()> {
        crate::runtime::turn_terminal::silence::record_agent_silence(self, proof, context, compiled)
    }
}

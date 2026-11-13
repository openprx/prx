//! Model, limits and parity owner of one runtime publication (R25).
//!
//! [`PublicationScope`] borrows the publication-scoped inputs (pinned
//! configuration generation, provider and capability catalogs, executor, the
//! runtime holder and the child/operator publication authorities) and owns
//! every model-resolution, budget and live-parity rule a turn of that
//! publication reads. The decision service only lends its fields until the
//! Part-2 builders own them.

use std::sync::Arc;

use crate::config::ConfigGeneration;
use crate::cost::estimate::DecisionTokenReservation;
use crate::memory::Memory;
use crate::providers::catalog::{CatalogView as ProviderCatalogView, Requirement, ResolvedModel, UnknownPolicy};
use crate::runtime::child_source::operator_process::OperatorProcessAuthority;
use crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority;
use crate::runtime::child_source::ordinary_task::OrdinaryTaskAuthority;
use crate::runtime::child_source::publication::OrdinaryChildPublication;
use crate::runtime::operator_spawn::task::OperatorTaskAuthority;
use crate::runtime::turn_context::{TurnOriginMismatch, TurnRuntimeHolder, validate_agent_turn_origin};
use crate::runtime::turn_decision::{AgentDecisionBudget, AgentDecisionBudgetError, SendLimits, TurnModelError};
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogView, ParityError, SurfacePolicy};
use crate::tools::capability::child::VerifiedChildScope;
use crate::tools::capability::turn_state::{TurnBudgets, TurnSurfaceKind};
use crate::tools::execution::ToolExecutionService;

/// Borrowed publication-scoped inputs of the model, limits and parity rules.
pub(crate) struct PublicationScope<'a> {
    pub(crate) generation: &'a ConfigGeneration,
    pub(crate) surface: TurnSurfaceKind,
    pub(crate) provider_catalog: &'a ProviderCatalogView,
    pub(crate) catalog: &'a CapabilityCatalog,
    pub(crate) publication_policy: &'a SurfacePolicy,
    pub(crate) execution: &'a ToolExecutionService,
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) security: &'a SecurityPolicy,
    pub(crate) memory_loader: Option<&'a dyn crate::agent::memory_loader::MemoryLoader>,
    pub(crate) state_key: &'a DecisionKey,
    pub(crate) holder: &'a TurnRuntimeHolder,
    pub(crate) ordinary_parent: Option<&'a OrdinaryChildPublication>,
    pub(crate) ordinary_task: Option<&'a OrdinaryTaskAuthority>,
    pub(crate) operator_task: Option<&'a OperatorTaskAuthority>,
    pub(crate) operator_process: Option<&'a OperatorProcessAuthority>,
    pub(crate) ordinary_process: Option<&'a OrdinaryProcessAuthority>,
}

impl<'a> PublicationScope<'a> {
    pub(crate) const fn memory(&self) -> &'a Arc<dyn Memory> {
        self.memory
    }

    pub(crate) const fn execution(&self) -> &'a ToolExecutionService {
        self.execution
    }

    pub(crate) const fn catalog(&self) -> &'a CapabilityCatalog {
        self.catalog
    }

    pub(crate) const fn security(&self) -> &'a SecurityPolicy {
        self.security
    }

    pub(crate) fn tool_tiering(&self) -> &'a crate::config::ToolTieringConfig {
        &self.generation.effective.tool_tiering
    }

    pub(crate) const fn prompt_memory_loader(&self) -> Option<&'a dyn crate::agent::memory_loader::MemoryLoader> {
        self.memory_loader
    }

    pub(crate) fn catalog_view(&self) -> CatalogView<'a> {
        self.catalog
            .view(&CatalogScope::default())
            .with_surface(self.publication_policy)
    }

    /// Bind runtime adapter ceilings to the provider catalog pinned by this
    /// exact configuration generation.
    pub(crate) fn effective_provider_capabilities(
        &self,
        provider: &str,
        model: &str,
        adapter: crate::providers::ProviderCapabilities,
    ) -> crate::providers::ProviderCapabilities {
        self.provider_catalog
            .effective_provider_capabilities(provider, model, adapter)
    }

    /// Resolve model limits against the provider catalog pinned to this
    /// configuration generation. Runtime surfaces must not reconstruct a
    /// release-only view after admission has already selected stronger
    /// operator or discovery evidence.
    pub(crate) fn effective_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::providers::context_window::EffectiveCompactionConfig {
        let config = &self.generation.effective;
        crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
            &config.agent.compaction,
            provider,
            model,
            crate::providers::context_window::ContextWindowSources::from_config(config),
            &config.model_routes,
            self.provider_catalog,
        )
    }

    /// Snapshot the initial decision budget after the source proof and PMM
    /// selection have been rechecked. Execution must recheck spend under its
    /// own ledger lock; this read-only snapshot grants no tool authority.
    pub(crate) fn agent_budget_snapshot(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> Result<AgentDecisionBudget, AgentDecisionBudgetError> {
        let _model = self.resolve_agent_model(proof)?;
        let config = &self.generation.effective;
        let effective = self.effective_compaction(proof.provider_name(), proof.model_name());
        let extra_token_ratio_limit = config.causal_tree.policy.extra_token_ratio_limit;
        let reservation =
            DecisionTokenReservation::from_effective_compaction(&effective.config, extra_token_ratio_limit)?;
        let main_compaction = reservation.reserved_compaction(&effective.config, extra_token_ratio_limit)?;
        let remaining = self
            .holder
            .spend_ceiling(proof)?
            .live()
            .map_err(AgentDecisionBudgetError::Spend)?;
        Ok(AgentDecisionBudget {
            budgets: TurnBudgets {
                tokens: u64::try_from(reservation.decision_tokens).unwrap_or(u64::MAX),
                latency_ms: config.causal_tree.policy.extra_latency_budget_ms,
                micro_usd: remaining,
            },
            main_compaction,
        })
    }

    /// Live spend limit and price inputs for one provider send of this turn.
    /// The spend limit is re-read from the ledger on every call so an earlier
    /// round's charge is visible.
    pub(crate) fn send_limits(&self, proof: &dyn TurnSourceProof) -> Result<SendLimits, AgentDecisionBudgetError> {
        let budget = self.agent_budget_snapshot(proof)?;
        let effective = self.effective_compaction(proof.provider_name(), proof.model_name());
        Ok(SendLimits {
            response_reserve: effective.config.reserve_tokens,
            remaining_micro_usd: budget.budgets.micro_usd,
            model: self.resolve_agent_model(proof)?,
            cost: self.generation.effective.cost.clone(),
        })
    }

    /// A published catalog cannot authorize a capability after its live
    /// registry schema or binding has changed. Call before candidate exposure
    /// and again immediately before committing a directive.
    pub(crate) fn check_live_parity(&self) -> Result<(), ParityError> {
        if let Some(parent) = self.ordinary_parent {
            parent.verify_live().map_err(|_| ParityError)?;
        }
        if let Some(task) = self.ordinary_task {
            task.verify().map_err(|_| ParityError)?;
        }
        if let Some(task) = self.operator_task {
            task.verify().map_err(|_| ParityError)?;
        }
        if let Some(process) = self.operator_process {
            process.verify().map_err(|_| ParityError)?;
            return self
                .catalog
                .check_published_parity(self.execution, process.snapshots().publication.allowed());
        }
        if let Some(process) = self.ordinary_process {
            process.verify().map_err(|_| ParityError)?;
        }
        self.child_scope().map_or_else(
            || self.catalog.check_parity(self.execution),
            |scope| self.catalog.check_child_parity(self.execution, scope),
        )
    }

    pub(crate) fn child_scope(&self) -> Option<&'a Arc<VerifiedChildScope>> {
        crate::runtime::child_source::turn_scope::issued_child_scope(
            self.holder.parent_grant().map(AsRef::as_ref),
            self.ordinary_parent,
            self.ordinary_task,
            self.ordinary_process,
        )
    }

    pub(crate) fn resolve_agent_model(&self, proof: &dyn TurnSourceProof) -> Result<ResolvedModel, TurnModelError> {
        validate_agent_turn_origin(self.generation, self.surface, proof)?;
        match self.operator_process {
            Some(process) => process
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.operator_process_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match self.operator_task {
            Some(task) => task
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.operator_task_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match self.ordinary_process {
            Some(process) => process
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.ordinary_process_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }

        if let Some(parent) = self.ordinary_parent {
            parent
                .verify_live()
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?;
        }
        match self.ordinary_task {
            Some(task) => task
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.ordinary_task_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match (self.child_scope(), proof.child_scope()) {
            (None, None) => {}
            (Some(issued), Some(scope)) if Arc::ptr_eq(issued, scope) => {}
            _ => return Err(TurnModelError::Origin(TurnOriginMismatch)),
        }
        if let Some(scope) = proof.child_scope() {
            scope
                .validate_runtime(
                    self.state_key,
                    proof.owner_id(),
                    proof.workspace_id(),
                    self.catalog_view().generation(),
                )
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?;
        }
        let config = &self.generation.effective;
        let pinned_route = crate::providers::selection::route_target(
            self.provider_catalog,
            proof.provider_name(),
            proof.model_name(),
        )?;
        if pinned_route != proof.model_binding().route() {
            return Err(TurnModelError::RouteDrift);
        }
        let resolved = crate::providers::selection::check_with(
            config,
            self.provider_catalog,
            proof.provider_name(),
            proof.model_name(),
            self.credential_for_proof(proof),
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )?;
        if !proof.model_binding().matches(&resolved) {
            return Err(TurnModelError::SelectionDrift);
        }
        Ok(resolved)
    }

    pub(crate) fn credential_for_proof(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> crate::providers::credentials::CredentialRef {
        if self.child_scope().is_some() || self.operator_task.is_some() || self.operator_process.is_some() {
            proof.model_binding().credential_reference().clone()
        } else {
            crate::providers::selection::credential_for(&self.generation.effective, proof.provider_name())
        }
    }

    /// G7 permits tool calls only when ingress configuration allows them
    /// and PMM verified NativeTools for the exact model on this catalog.
    /// An id-only invocation can still chat
    /// under PMM's invocation policy, but cannot gain tool authority.
    pub(crate) fn model_can_call_tools(&self, proof: &dyn TurnSourceProof) -> bool {
        if !proof.tool_calls_allowed() || self.resolve_agent_model(proof).is_err() {
            return false;
        }
        crate::runtime::turn_model::can_call_tools(
            &self.generation.effective,
            self.provider_catalog,
            proof,
            self.credential_for_proof(proof),
        )
    }
}

//! Per-turn runtime state that execution owns independently of the decision
//! plane. Built once per turn at opening from the values the opening path
//! already computes; the advisory driver and execution read it.
//!
//! [`TurnRuntimeHolder`] is the publication-scoped half: the recovery holder,
//! the recursive child allowances, the incoming child funding and the spend
//! ceiling inputs. Every [`TurnRuntimeContext`] opened by one publication
//! shares it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::decision_engine::state::{ObservedState, RawRoutingInput};
use crate::providers::catalog::ReleaseCatalog;
use crate::providers::catalog::parameters::ResolvedParameters;
use crate::providers::parameter_capture::Capture;
use crate::runtime::child_source::IssuedChildGrant;
use crate::runtime::child_source::recursive_allowance::{ContextAllowance, RecursiveAllowance, RunAllowances};
use crate::runtime::decision_accounting::AccountingScope;
use crate::runtime::ingress_recovery::RecoveryScope;
use crate::runtime::turn_decision::{AdmittedDecisionResume, AgentDecisionBudgetError, SendLimits};
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityCatalog, SurfacePolicy};
use crate::tools::capability::contract::CatalogGeneration;
use crate::tools::capability::resume_evidence::VerifiedResumeEvidenceV1;
use crate::tools::capability::turn_state::{TrustedParts, TurnBudgets};

/// Live spend inputs of one provider send or called tool. `live` re-reads the
/// ledger and the incoming funding on every call, so a later boundary sees a
/// charge or a funding change made after the snapshot.
pub(crate) struct SpendCeiling {
    cost: crate::config::schema::CostConfig,
    workspace_dir: PathBuf,
    security_cents: u32,
    funding: Option<Arc<AtomicU64>>,
    manifest_micro: Option<u64>,
}

impl SpendCeiling {
    /// Minimum of the read-only daily, monthly and security remaining, the
    /// incoming child funding and the sealed child manifest cents x 10_000.
    pub(crate) fn live(&self) -> anyhow::Result<u64> {
        let remaining = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &self.cost,
            &self.workspace_dir,
            self.security_cents,
        )?;
        let remaining = self
            .funding
            .as_ref()
            .map_or(remaining, |money| remaining.min(money.load(Ordering::Acquire)));
        Ok(self
            .manifest_micro
            .map_or(remaining, |manifest| remaining.min(manifest)))
    }
}

/// Recovery inputs of a root publication: the cleanup owner, the prepared
/// workspace key and the advisory store opened for recovery.
pub(crate) struct RecoveryPreparation {
    pub(crate) owner: Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    pub(crate) workspace_key: Option<Arc<DecisionKey>>,
    pub(crate) advisory: crate::runtime::ingress_recovery::PreparedAdvisory,
}

/// Publication-scoped runtime state shared by every turn of one publication:
/// recovery holder, recursive child allowances, incoming child funding and the
/// spend-ceiling inputs pinned by the publication's configuration generation.
pub(crate) struct TurnRuntimeHolder {
    recovery: Arc<RecoveryScope>,
    recursive_allowances: RunAllowances,
    ordinary_money: Option<Arc<AtomicU64>>,
    parent_grant: Option<Arc<IssuedChildGrant>>,
    cost: crate::config::schema::CostConfig,
    workspace_dir: PathBuf,
    security_cents: u32,
}

impl TurnRuntimeHolder {
    pub(crate) fn new(
        recovery: Arc<RecoveryScope>,
        ordinary_money: Option<Arc<AtomicU64>>,
        parent_grant: Option<Arc<IssuedChildGrant>>,
        config: &crate::config::Config,
        security: &crate::security::SecurityPolicy,
    ) -> Self {
        Self {
            recovery,
            recursive_allowances: RunAllowances::default(),
            ordinary_money,
            parent_grant,
            cost: config.cost.clone(),
            workspace_dir: config.workspace_dir.clone(),
            security_cents: security.max_cost_per_day_cents,
        }
    }

    /// Prepare the recovery holder of a root (or incoming-grant) publication
    /// and hold it with the publication's spend inputs.
    pub(crate) async fn prepare(
        generation: &crate::config::ConfigGeneration,
        memory: Arc<dyn crate::memory::Memory>,
        surface: crate::tools::capability::turn_state::TurnSurfaceKind,
        parent_grant: Option<Arc<IssuedChildGrant>>,
        recovery: RecoveryPreparation,
        security: &crate::security::SecurityPolicy,
    ) -> anyhow::Result<Self> {
        let scope = RecoveryScope::prepare(
            generation,
            memory,
            surface,
            parent_grant.as_deref(),
            &recovery.owner,
            recovery.workspace_key,
            recovery.advisory,
        )
        .await?;
        Ok(Self::new(scope, None, parent_grant, &generation.effective, security))
    }

    /// Recovery retains its own pins and queues independently of any
    /// decision service.
    pub(crate) fn recovery_scope(&self) -> Arc<RecoveryScope> {
        Arc::clone(&self.recovery)
    }

    pub(in crate::runtime) const fn recursive_allowances(&self) -> &RunAllowances {
        &self.recursive_allowances
    }

    pub(crate) const fn parent_grant(&self) -> Option<&Arc<IssuedChildGrant>> {
        self.parent_grant.as_ref()
    }

    /// The money that funds this run's recursive children: the incoming
    /// grant's allowance, the publication's own child budget, or the run's
    /// issued allowance.
    pub(crate) fn recursive_money(&self, proof: &dyn TurnSourceProof) -> Option<Arc<AtomicU64>> {
        if let Some(incoming) = &self.parent_grant {
            return incoming
                .recursive_child_allowance()
                .and_then(|owner| owner.funded_money());
        }
        if let Some(money) = &self.ordinary_money {
            return Some(Arc::clone(money));
        }
        self.recursive_allowances
            .lookup(proof)
            .and_then(|owner| owner.funded_money())
    }

    /// A child turn without incoming funding refuses: the workspace remaining
    /// alone never stands in for a child allowance.
    pub(crate) fn spend_ceiling(&self, proof: &dyn TurnSourceProof) -> Result<SpendCeiling, AgentDecisionBudgetError> {
        let funding = self.recursive_money(proof);
        let child = proof.child_scope();
        if child.is_some() && funding.is_none() {
            return Err(AgentDecisionBudgetError::ChildFunding);
        }
        let manifest_micro =
            child.map(|scope| u64::from(scope.gate_inputs().0.authority.max_cost_cents).saturating_mul(10_000));
        Ok(SpendCeiling {
            cost: self.cost.clone(),
            workspace_dir: self.workspace_dir.clone(),
            security_cents: self.security_cents,
            funding,
            manifest_micro,
        })
    }
}

/// The execution-owned runtime context of one turn.
pub(crate) struct TurnRuntimeContext {
    pub(crate) decision_id: DecisionId,
    /// The trusted parts captured at opening (pinned turn scope). Terminal
    /// scope checks compare against this, never against a decision hop.
    pub(crate) trusted: TrustedParts,
    pub(crate) routing_input: RawRoutingInput,
    pub(crate) goal: RawRoutingInput,
    pub(crate) has_conversation_history: bool,
    pub(crate) observed: ObservedState,
    pub(crate) budgets: TurnBudgets,
    /// The one accounting scope of the turn: send admission, dispatch
    /// admission and the advisory driver all read and charge it.
    pub(crate) accounting: Option<Arc<AccountingScope>>,
    pub(crate) send_limits: SendLimits,
    pub(crate) catalog: Arc<CapabilityCatalog>,
    pub(crate) publication_policy: SurfacePolicy,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) admission: AdmissionFactsV1,
    /// Shared handle to the same value the decision context holds.
    pub(crate) resume: Option<Arc<AdmittedDecisionResume>>,
    pub(crate) resume_evidence: Option<Arc<VerifiedResumeEvidenceV1>>,
    /// The same key the service uses for `Ns8::of`.
    pub(crate) state_key: Arc<DecisionKey>,
    /// Publication-scoped holder shared with every turn of the publication.
    pub(crate) holder: Arc<TurnRuntimeHolder>,
    /// The provider release catalog pinned by the publication generation.
    pub(crate) model_catalog: Option<Arc<ReleaseCatalog>>,
    pub(crate) main_parameters: OnceLock<Capture>,
    pub(in crate::runtime) recursive_allowance: OnceLock<ContextAllowance>,
}

impl TurnRuntimeContext {
    /// Remaining G6 tokens: the opening slice narrowed by everything the
    /// turn's accounting scope has already charged.
    pub(crate) fn remaining_g6_tokens(&self) -> u64 {
        self.accounting.as_ref().map_or(self.budgets.tokens, |scope| {
            self.budgets.tokens.min(scope.remaining_budgets().tokens)
        })
    }

    /// Remaining money of the turn's sends, as admitted at opening and
    /// narrowed by the accounting scope.
    pub(crate) fn remaining_micro_usd(&self) -> u64 {
        self.accounting
            .as_ref()
            .map_or(self.send_limits.remaining_micro_usd, |scope| {
                self.send_limits
                    .remaining_micro_usd
                    .min(scope.remaining_budgets().micro_usd)
            })
    }

    /// The single tool a historical resume turn may publish and dispatch.
    pub(crate) fn resume_tool_name(&self) -> Option<&str> {
        self.resume.as_deref().map(AdmittedDecisionResume::prepared_tool_name)
    }

    pub(crate) fn model_catalog(&self) -> Option<&ReleaseCatalog> {
        self.model_catalog.as_deref()
    }

    pub(crate) fn capture_main_parameters(&self, enabled: bool) -> Capture {
        self.main_parameters.get_or_init(|| Capture::new(enabled)).clone()
    }

    pub(crate) fn prepared_main_parameters(&self) -> Vec<ResolvedParameters> {
        self.main_parameters.get().map_or_else(Vec::new, Capture::snapshot)
    }

    /// The recursive allowance this turn already retained or issued.
    pub(crate) fn recursive_allowance(&self) -> Option<&RecursiveAllowance> {
        self.recursive_allowance.get().map(|owner| &***owner)
    }

    /// Retain (or issue once) the allowance that funds this run's recursive
    /// children. An incoming grant funds them; otherwise the turn's
    /// accounting scope, the publication's child money or the turn's opening
    /// money does.
    pub(crate) fn issue_recursive_allowance(&self, proof: &dyn TurnSourceProof) -> Option<Arc<RecursiveAllowance>> {
        if let Some(incoming) = self.holder.parent_grant() {
            return incoming.recursive_child_allowance();
        }
        Some(Arc::clone(self.recursive_allowance.get_or_init(|| {
            let money = self.accounting.as_ref().map_or_else(
                || {
                    self.holder
                        .recursive_money(proof)
                        .unwrap_or_else(|| Arc::new(AtomicU64::new(self.budgets.micro_usd)))
                },
                |scope| scope.money_owner(),
            );
            self.holder.recursive_allowances().issue(proof, money)
        })))
    }

    pub(crate) fn retire_recursive_allowance(&self) {
        if let Some(owner) = self.recursive_allowance.get() {
            owner.retire();
        }
    }
}

#[cfg(test)]
mod tests;

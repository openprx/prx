//! Per-turn runtime state that execution owns independently of the decision
//! plane. Built once per turn at opening from the values the opening path
//! already computes; the advisory driver and execution read it.

use std::sync::Arc;

use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::state::{ObservedState, RawRoutingInput, TrustedParts, TurnBudgets};
use crate::runtime::decision_accounting::AccountingScope;
use crate::runtime::turn_decision::{AdmittedDecisionResume, SendLimits};
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityCatalog, SurfacePolicy};
use crate::tools::capability::contract::CatalogGeneration;
use crate::tools::capability::resume_evidence::VerifiedResumeEvidenceV1;

/// The execution-owned runtime context of one turn.
pub(crate) struct TurnRuntimeContext {
    pub(crate) decision_id: DecisionId,
    /// The trusted parts captured at opening (pinned turn scope).
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
}

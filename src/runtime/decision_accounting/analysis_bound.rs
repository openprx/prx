//! The advisory analysis' bounded view of a turn's accounting scope.
//!
//! Analysis is charged to the turn's own scope (owner ruling 2026-11-13), but
//! it must never consume what the first serving send needs to be admitted.
//! Every analysis attempt therefore reserves at most the remaining budget
//! minus that protected send, re-read when the attempt begins, so even an
//! uncertain remote attempt that retains its whole reservation leaves the
//! protected send allowance untouched.

use std::sync::Arc;

use async_trait::async_trait;

use super::{AccountingScope, PhysicalEstimate};
use crate::cost::decision_attempt::{
    self, AttemptBinding, DecisionAttemptAccounting, DecisionAttemptReceipt, DecisionError,
};
use crate::tools::capability::turn_state::TurnBudgets;

/// `(tokens, micro_usd)`.
pub(crate) type Budget = (u64, u64);

/// The token and money allowance one analysis may reserve: the remaining
/// budget minus the protected first send, with the token allowance shrunk
/// until its worst-case price (`worst_case_money`, the same estimate an
/// attempt reserves) fits the money allowance. `None` when nothing is left
/// or the analysis model has no usable price.
pub(crate) fn analysis_allowance(
    remaining: Budget,
    protected: Budget,
    worst_case_money: impl Fn(u64) -> Option<u64>,
) -> Option<Budget> {
    let tokens = remaining.0.checked_sub(protected.0)?;
    let money = remaining.1.checked_sub(protected.1)?;
    let fits = |tokens: u64| worst_case_money(tokens).is_some_and(|price| price <= money);
    let tokens = if fits(tokens) {
        tokens
    } else {
        if !fits(1) {
            return None;
        }
        // Largest affordable allowance: `fits(low)` and `!fits(high)`; the
        // worst-case price is monotone in tokens.
        let (mut low, mut high) = (1_u64, tokens);
        while high - low > 1 {
            let middle = low + (high - low) / 2;
            if fits(middle) {
                low = middle;
            } else {
                high = middle;
            }
        }
        low
    };
    (tokens > 0).then_some((tokens, money))
}

/// The analysis' accounting owner: charges the turn's scope, bounded by the
/// allowance and by the protected first send.
pub(crate) struct AnalysisAccounting {
    scope: Arc<AccountingScope>,
    protected: Budget,
    allowance: Budget,
}

impl AnalysisAccounting {
    pub(crate) const fn new(scope: Arc<AccountingScope>, protected: Budget, allowance: Budget) -> Self {
        Self {
            scope,
            protected,
            allowance,
        }
    }

    /// What one attempt may still reserve now: the live remaining budget
    /// minus the protected send, never more than the allowance.
    fn available(&self) -> Budget {
        let remaining = self.scope.remaining_budgets();
        (
            remaining.tokens.saturating_sub(self.protected.0).min(self.allowance.0),
            remaining
                .micro_usd
                .saturating_sub(self.protected.1)
                .min(self.allowance.1),
        )
    }
}

impl decision_attempt::sealed::AccountingOwner for AnalysisAccounting {}

#[async_trait]
impl DecisionAttemptAccounting for AnalysisAccounting {
    fn remaining_budget(&self) -> Budget {
        self.available()
    }

    async fn begin(&self, attempt: AttemptBinding<'_>) -> Result<Box<dyn DecisionAttemptReceipt>, DecisionError> {
        // The attempt reserves its token allowance priced at the highest
        // applicable rate, so the allowance is shrunk to what the money left
        // for analysis can pay for this attempt's provider and model.
        let (tokens, micro_usd) = analysis_allowance(self.available(), (0, 0), |tokens| {
            self.scope
                .worst_case_micro_usd(attempt.provider(), attempt.model(), tokens)
        })
        .ok_or(DecisionError::BudgetExceeded)?;
        self.scope
            .begin_physical_within(
                PhysicalEstimate {
                    purpose: "decision",
                    provider: attempt.provider(),
                    model: attempt.model(),
                    body: attempt.guarded_body(),
                    transport_binding: attempt.transport_binding(),
                    request_id: attempt.request_id(),
                    deadline: attempt.deadline(),
                    remaining_attempts: attempt.remaining_attempts(),
                },
                TurnBudgets {
                    tokens,
                    micro_usd,
                    latency_ms: u64::MAX,
                },
            )
            .await
            .map(|receipt| Box::new(receipt) as Box<dyn DecisionAttemptReceipt>)
    }
}

#[cfg(test)]
mod tests {
    use super::analysis_allowance;

    #[test]
    fn allowance_leaves_the_protected_send_and_fits_the_money() {
        // One micro-USD per token: 1,000 money allows 1,000 tokens.
        let price = |tokens: u64| Some(tokens);
        assert_eq!(
            analysis_allowance((8_064, 5_000), (128, 4_000), price),
            Some((1_000, 1_000))
        );
        assert_eq!(
            analysis_allowance((8_064, 50_000), (128, 4_000), price),
            Some((7_936, 46_000))
        );
        assert_eq!(analysis_allowance((100, 50_000), (128, 4_000), price), None);
        assert_eq!(analysis_allowance((8_064, 3_999), (128, 4_000), price), None);
        assert_eq!(analysis_allowance((8_064, 50_000), (128, 4_000), |_| None), None);
        assert_eq!(analysis_allowance((8_064, 4_000), (128, 4_000), price), None);
    }
}

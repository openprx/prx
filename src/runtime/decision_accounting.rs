//! Source-owned accounting for physical decision sends. Accounting never grants
//! tool, provider, or source authority; callers validate their ingress first.
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::turn_cleanup::{CleanupTicket, CleanupTracker};
use crate::causal_tree::state::TurnBudgets;
use crate::config::Config;
use crate::cost::{
    reservation::{EstimatedReservationRequest, ReservationReceipt, ReservationRequest},
    tracker::CostTracker,
};
use crate::decision::budget::{self, AttemptBinding, DecisionAttemptAccounting, DecisionAttemptReceipt};
use crate::decision::protocol::DecisionUsage;
use crate::decision::{DecisionError, DecisionKey, KeyDomain};
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource, UsagePrice, usage_cost_resolution};

#[cfg(test)]
pub(crate) mod timing;

#[cfg(feature = "llm-router")]
mod cl100k_prefix_bound;

/// Pinned configuration and a real cleanup owner are borrowed through Arc;
/// builtin/cache-only calls never initialize the token owner or cost storage.
pub(crate) struct AccountingScope {
    config: Arc<Config>,
    key: Arc<DecisionKey>,
    cleanup: Arc<CleanupTracker>,
    scope_id: [u8; 32],
    budgets: TurnBudgets,
    deadline: Instant,
    remaining: OnceLock<Arc<RemainingBudgets>>,
}

struct RemainingBudgets {
    tokens: AtomicU64,
    micro_usd: AtomicU64,
}

impl AccountingScope {
    pub(crate) fn remaining_budgets(&self) -> TurnBudgets {
        TurnBudgets {
            tokens: self
                .remaining
                .get()
                .map_or(self.budgets.tokens, |value| value.tokens.load(Ordering::Acquire)),
            micro_usd: self
                .remaining
                .get()
                .map_or(self.budgets.micro_usd, |value| value.micro_usd.load(Ordering::Acquire)),
            latency_ms: self.budgets.latency_ms.min(
                u64::try_from(self.deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX),
            ),
        }
    }

    pub(crate) const fn new(
        config: Arc<Config>,
        key: Arc<DecisionKey>,
        cleanup: Arc<CleanupTracker>,
        scope_id: [u8; 32],
        budgets: TurnBudgets,
        deadline: Instant,
    ) -> Self {
        Self {
            config,
            key,
            cleanup,
            scope_id,
            budgets,
            deadline,
            remaining: OnceLock::new(),
        }
    }
}

impl budget::sealed::AccountingOwner for AccountingScope {}

/// Reserve the declared available decision allowance at the highest applicable
/// PMM rate. This is a versioned local estimate, never a vendor billing cap.
fn estimated_cost(
    config: &crate::config::schema::CostConfig,
    provider: &str,
    model: &str,
    tokens: u64,
) -> Result<u64, DecisionError> {
    let mut maximum = 0.0_f64;
    for (input, output, write, read) in [
        (tokens, 0, 0, 0),
        (0, tokens, 0, 0),
        (tokens, 0, tokens, 0),
        (tokens, 0, 0, tokens),
    ] {
        let UsagePrice::Priced(cost) = usage_cost_resolution(provider, model, input, output, write, read, config)
        else {
            return Err(DecisionError::BudgetExceeded);
        };
        maximum = maximum.max(cost);
    }
    crate::cost::tracker::ceil_micro_usd(maximum)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DecisionError::BudgetExceeded)
}

#[async_trait]
impl DecisionAttemptAccounting for AccountingScope {
    fn remaining_budget(&self) -> (u64, u64) {
        let budget = self.remaining_budgets();
        (budget.tokens, budget.micro_usd)
    }
    async fn begin(&self, attempt: AttemptBinding<'_>) -> Result<Box<dyn DecisionAttemptReceipt>, DecisionError> {
        self.begin_physical(PhysicalEstimate {
            purpose: "decision",
            provider: attempt.provider(),
            model: attempt.model(),
            body: attempt.guarded_body(),
            transport_binding: attempt.transport_binding(),
            request_id: attempt.request_id(),
            deadline: attempt.deadline(),
            remaining_attempts: attempt.remaining_attempts(),
        })
        .await
        .map(|receipt| Box::new(receipt) as Box<dyn DecisionAttemptReceipt>)
    }
}

pub(super) struct PhysicalEstimate<'a> {
    /// Fixed runtime purpose joins actual journal IDs without persisting request data.
    pub purpose: &'static str,
    pub provider: &'a str,
    pub model: &'a str,
    pub body: &'a [u8],
    pub transport_binding: &'a [u8; 32],
    pub request_id: &'a str,
    pub deadline: Instant,
    pub remaining_attempts: u8,
}

impl AccountingScope {
    pub(super) fn scope_id(&self) -> String {
        hex::encode(self.scope_id)
    }
    #[cfg(test)]
    pub(crate) fn scope_id_for_tests(&self) -> String {
        self.scope_id()
    }
    pub(super) const fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(super) async fn begin_physical(&self, attempt: PhysicalEstimate<'_>) -> Result<OwnedReceipt, DecisionError> {
        let auxiliary = matches!(
            attempt.purpose,
            "signal_vision" | "smart_group_classifier" | "automix_premium" | "router_embedding"
        );
        let owner_deadline = self.deadline;
        let deadline = owner_deadline.min(attempt.deadline);
        #[cfg(test)]
        let mut diagnostic_timing = timing::Span::current(deadline);
        #[cfg(test)]
        if let Some(timing) = &mut diagnostic_timing {
            timing.mark("accounting_entered");
        }
        if Instant::now() >= deadline {
            return Err(DecisionError::BudgetExceeded);
        }
        let remaining = Arc::clone(self.remaining.get_or_init(|| {
            Arc::new(RemainingBudgets {
                tokens: AtomicU64::new(self.budgets.tokens),
                micro_usd: AtomicU64::new(self.budgets.micro_usd),
            })
        }));
        let planned = u64::from(attempt.remaining_attempts);
        if !(1..=2).contains(&planned) || (auxiliary && planned != 1) {
            return Err(DecisionError::BudgetExceeded);
        }
        let cancel = CancellationToken::new();
        let ticket = self
            .cleanup
            .register(cancel.clone())
            .map_err(|_| DecisionError::Cancelled)?;
        let attempt_id = format!("{}:{}", attempt.purpose, uuid::Uuid::new_v4());
        let binding = self.key.hmac(
            KeyDomain::TerminalClaim,
            &[
                b"decision-accounting-estimate-v2",
                &attempt.remaining_attempts.to_be_bytes(),
                &self.scope_id,
                attempt.transport_binding,
                attempt.request_id.as_bytes(),
                attempt.provider.as_bytes(),
                attempt.model.as_bytes(),
                attempt.body,
            ],
        );
        let turn_id = hex::encode(self.scope_id);
        let budgets = self.budgets;
        // Copy only on a real configured physical attempt; the same full
        // guarded bytes are measured without removing image or request data.
        let request_body = attempt.body.to_vec();
        let config = Arc::clone(&self.config);
        let provider = attempt.provider.to_owned();
        let model = attempt.model.to_owned();
        // The ticket moves into the blocking operation before the first await.
        // A dropped caller cannot abandon a ledger transition in progress.
        #[cfg(test)]
        if let Some(timing) = &mut diagnostic_timing {
            timing.mark("spawn_submitted");
        }
        let runtime = tokio::runtime::Handle::current();
        let claim_cancel = cancel.clone();
        let work = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(timing) = &mut diagnostic_timing {
                timing.mark("blocking_entered");
            }
            let mut ticket = Some(ticket);
            let mut confirmed = true;
            let mut restore_tokens = true;
            let mut tokens = 0;
            let mut maximum = 0;
            let mut money_claimed = false;
            #[cfg(test)]
            let mut diagnostic_stage = "decode";
            let result = (|| -> anyhow::Result<OwnedReceipt> {
                let text = std::str::from_utf8(&request_body)?;
                #[cfg(test)]
                {
                    diagnostic_stage = "input_fit";
                    if let Some(timing) = &mut diagnostic_timing {
                        timing.mark("input_fit");
                    }
                }
                #[cfg(test)]
                let token_timing = diagnostic_timing
                    .as_ref()
                    .map(|_| crate::agent::context_budget::token_timing::Scope::new());
                tokens = claim_input_tokens(&remaining.tokens, planned, text, || {
                    let measured = crate::agent::context_budget::count_tokens_with_source(text);
                    crate::agent::context_budget::estimate_request_tokens(
                        measured,
                        crate::agent::context_budget::TokenMeasurement::NONE,
                    )
                })?;
                #[cfg(test)]
                if let Some(scope) = token_timing {
                    if let Some(timing) = &mut diagnostic_timing {
                        timing.tokenizer(scope.finish());
                        timing.mark("input_fit_finished");
                    }
                }
                maximum = estimated_cost(&config.cost, &provider, &model, tokens)?;
                anyhow::ensure!(
                    remaining
                        .micro_usd
                        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_sub(maximum))
                        .is_ok(),
                    "physical request exceeds the remaining monetary allowance"
                );
                money_claimed = true;
                #[cfg(test)]
                {
                    diagnostic_stage = "deadline_after_estimation";
                    if let Some(timing) = &mut diagnostic_timing {
                        timing.mark("deadline_after_estimation");
                    }
                }
                anyhow::ensure!(
                    !cancel.is_cancelled() && Instant::now() < deadline,
                    "physical accounting deadline elapsed during estimation"
                );
                let request = EstimatedReservationRequest {
                    reservation: ReservationRequest {
                        turn_id,
                        attempt_id,
                        binding_sha256: hex::encode(binding),
                        maximum_micro_usd: maximum,
                        turn_limit_micro_usd: budgets.micro_usd,
                        security_daily_micro_usd: u64::from(config.autonomy.max_cost_per_day_cents) * 10_000,
                    },
                    reserved_tokens: tokens,
                    turn_token_limit: budgets.tokens,
                };
                #[cfg(test)]
                {
                    diagnostic_stage = "tracker_open";
                    if let Some(timing) = &mut diagnostic_timing {
                        timing.mark("tracker_open");
                    }
                }
                let tracker = CostTracker::new(config.cost.clone(), &config.workspace_dir)?;
                #[cfg(test)]
                {
                    diagnostic_stage = "reserve";
                    if let Some(timing) = &mut diagnostic_timing {
                        timing.mark("reserve");
                    }
                }
                let receipt = match tracker.reserve_dispatched_estimated_attempt(request, || {
                    !cancel.is_cancelled() && Instant::now() < deadline
                }) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        if error.is::<crate::cost::reservation::ReservationWriteUnconfirmed>() {
                            confirmed = false;
                            restore_tokens = false;
                        }
                        return Err(error);
                    }
                };
                // From this point the reservation and its one-use dispatched
                // state are durable. Unknown send/usage retains the liability.
                restore_tokens = false;
                Ok(OwnedReceipt {
                    tracker,
                    receipt,
                    config,
                    provider,
                    model,
                    ticket: ticket.take(),
                    remaining: Arc::clone(&remaining),
                    reserved_tokens: tokens,
                    reserved_micro_usd: maximum,
                    deadline: owner_deadline,
                })
            })();
            #[cfg(test)]
            if let Some(timing) = &mut diagnostic_timing {
                timing.mark(if result.is_ok() {
                    "operation_ok"
                } else {
                    "operation_error"
                });
            }
            #[cfg(test)]
            drop(diagnostic_timing);
            if restore_tokens {
                remaining.tokens.fetch_add(tokens, Ordering::AcqRel);
                if money_claimed {
                    remaining.micro_usd.fetch_add(maximum, Ordering::AcqRel);
                }
            }
            if let Some(ticket) = ticket {
                ticket.finish(confirmed);
            }
            #[cfg(test)]
            if result.is_err() {
                let _ = std::io::Write::write_fmt(
                    &mut std::io::stderr().lock(),
                    format_args!("prx_accounting_failure stage={diagnostic_stage} allowance_tokens={tokens}\n"),
                );
            }
            result
                .map(|receipt| UnclaimedReceipt {
                    receipt: Some(receipt),
                    runtime,
                })
                .map_err(|_| DecisionError::BudgetExceeded)
        });
        let receipt = tokio::time::timeout_at(deadline.into(), work)
            .await
            .map_err(|_| {
                #[cfg(test)]
                accounting_diagnostic_failure("blocking_timeout");
                DecisionError::BudgetExceeded
            })?
            .map_err(|_| {
                #[cfg(test)]
                accounting_diagnostic_failure("blocking_join");
                DecisionError::BudgetExceeded
            })??;
        if claim_cancel.is_cancelled() || Instant::now() >= deadline {
            return Err(DecisionError::BudgetExceeded);
        }
        // No await separates receipt delivery from ownership transfer. Dropping
        // the join result before here releases an attempt that cannot be sent.
        receipt.claim()
    }
}

struct UnclaimedReceipt {
    receipt: Option<OwnedReceipt>,
    runtime: tokio::runtime::Handle,
}

impl UnclaimedReceipt {
    fn claim(mut self) -> Result<OwnedReceipt, DecisionError> {
        self.receipt.take().ok_or(DecisionError::BudgetExceeded)
    }
}

impl Drop for UnclaimedReceipt {
    fn drop(&mut self) {
        let Some(mut receipt) = self.receipt.take() else {
            return;
        };
        // The cleanup ticket stays owned until the compensating journal write
        // finishes, including when the caller timed out during the first fsync.
        self.runtime.spawn_blocking(move || {
            let released = receipt.tracker.release_unclaimed_estimated_attempt(&receipt.receipt);
            if released.is_ok() {
                receipt
                    .remaining
                    .tokens
                    .fetch_add(receipt.reserved_tokens, Ordering::AcqRel);
                receipt
                    .remaining
                    .micro_usd
                    .fetch_add(receipt.reserved_micro_usd, Ordering::AcqRel);
            } else {
                tracing::warn!("unclaimed decision reservation release was not confirmed");
            }
            if let Some(ticket) = receipt.ticket.take() {
                ticket.finish(released.is_ok());
            }
        });
    }
}

pub(super) struct OwnedReceipt {
    tracker: CostTracker,
    receipt: ReservationReceipt,
    config: Arc<Config>,
    provider: String,
    model: String,
    ticket: Option<CleanupTicket>,
    remaining: Arc<RemainingBudgets>,
    reserved_tokens: u64,
    reserved_micro_usd: u64,
    deadline: Instant,
}

impl Drop for OwnedReceipt {
    fn drop(&mut self) {
        // Cancellation or absent usage leaves a known durable Dispatched hold.
        // The cleanup owner is released only when the transport relinquishes
        // this receipt; uncertain ledger writes retain failure separately.
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(true);
        }
    }
}

impl budget::sealed::AttemptReceipt for OwnedReceipt {}

#[async_trait]
impl DecisionAttemptReceipt for OwnedReceipt {
    fn deadline(&self) -> Instant {
        self.deadline
    }

    async fn settle(self: Box<Self>, usage: DecisionUsage) -> Result<(), DecisionError> {
        self.settle_provider_usage(crate::llm::route_decision::TokenUsage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            source: TokenUsageSource::Reported,
            ..crate::llm::route_decision::TokenUsage::default()
        })
        .await
    }
}

impl OwnedReceipt {
    pub(super) async fn settle_provider_usage(
        mut self: Box<Self>,
        usage: crate::llm::route_decision::TokenUsage,
    ) -> Result<(), DecisionError> {
        if usage.source != TokenUsageSource::Reported {
            return Ok(());
        }
        let (Some(input), Some(output)) = (usage.prompt_tokens, usage.completion_tokens) else {
            // Durable Dispatched is deliberately retained, including its whole
            // token and monetary estimate. Unknown usage is never zero.
            return Ok(());
        };
        let ticket = self.ticket.take().ok_or(DecisionError::Cancelled)?;
        let work = tokio::task::spawn_blocking(move || {
            let result = (|| -> anyhow::Result<()> {
                let input = u64::from(input);
                let output = u64::from(output);
                let cache_write = usage.cache_creation_input_tokens.map_or(0, u64::from);
                let cache_read = usage.cache_read_input_tokens.map_or(0, u64::from);
                let UsagePrice::Priced(cost) = usage_cost_resolution(
                    &self.provider,
                    &self.model,
                    input,
                    output,
                    cache_write,
                    cache_read,
                    &self.config.cost,
                ) else {
                    anyhow::bail!("decision usage has no valid pinned PMM price");
                };
                let total = input + output;
                let record = MeteredTokenUsageRecord {
                    settlement_id: Some(self.receipt.attempt_id().to_owned()),
                    provider: crate::providers::redact_endpoint_userinfo(&self.provider).into_owned(),
                    model: self.model.clone(),
                    prompt_tokens: input,
                    completion_tokens: output,
                    total_tokens: total,
                    cache_creation_input_tokens: cache_write,
                    cache_read_input_tokens: cache_read,
                    source: TokenUsageSource::Reported,
                    cost_usd: Some(cost),
                };
                self.tracker.settle_metered(&record)?;
                self.tracker.reconcile_estimated_attempt(&self.receipt, &record)?;
                let actual_micro_usd = crate::cost::tracker::ceil_micro_usd(cost)?;
                if actual_micro_usd <= u128::from(self.reserved_micro_usd) {
                    self.remaining.micro_usd.fetch_add(
                        self.reserved_micro_usd - u64::try_from(actual_micro_usd)?,
                        Ordering::AcqRel,
                    );
                } else {
                    let excess =
                        u64::try_from(actual_micro_usd - u128::from(self.reserved_micro_usd)).unwrap_or(u64::MAX);
                    let _ = self
                        .remaining
                        .micro_usd
                        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                            Some(value.saturating_sub(excess))
                        });
                }

                reconcile_token_allowance(&self.remaining.tokens, self.reserved_tokens, total);
                Ok(())
            })();
            ticket.finish(result.is_ok());
            result.map_err(|_| DecisionError::BudgetExceeded)
        });
        work.await.map_err(|_| DecisionError::BudgetExceeded)?
    }
}

fn reconcile_token_allowance(remaining: &AtomicU64, reserved: u64, actual: u64) {
    if actual <= reserved {
        remaining.fetch_add(reserved - actual, Ordering::AcqRel);
    } else {
        let excess = actual - reserved;
        let _ = remaining.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.saturating_sub(excess))
        });
    }
}

#[cfg(test)]
mod tranche_tests {
    use super::*;

    #[test]
    fn a_proven_small_input_keeps_the_equal_retry_share_without_tokenizing() -> anyhow::Result<()> {
        let remaining = AtomicU64::new(2059);
        let calls = std::cell::Cell::new(0);
        let tokens = claim_input_tokens(&remaining, 2, "{}", || {
            calls.set(calls.get() + 1);
            2
        })?;
        assert_eq!(tokens, 1029);
        assert_eq!(remaining.load(Ordering::Acquire), 1030);
        assert_eq!(calls.get(), 0);
        Ok(())
    }

    #[test]
    fn an_input_above_the_share_claims_only_the_required_larger_share_and_measures_once() -> anyhow::Result<()> {
        let remaining = AtomicU64::new(2059);
        let calls = std::cell::Cell::new(0);
        let tokens = claim_input_tokens(&remaining, 2, &"large input ".repeat(1024), || {
            calls.set(calls.get() + 1);
            1036
        })?;
        assert_eq!(tokens, 1037);
        assert_eq!(remaining.load(Ordering::Acquire), 1022);
        assert_eq!(calls.get(), 1);
        Ok(())
    }

    #[test]
    fn input_equal_to_the_total_still_refuses_without_claiming_tokens() {
        let remaining = AtomicU64::new(2059);
        let calls = std::cell::Cell::new(0);
        assert!(
            claim_input_tokens(&remaining, 2, &"large input ".repeat(2048), || {
                calls.set(calls.get() + 1);
                2059
            })
            .is_err()
        );
        assert_eq!(remaining.load(Ordering::Acquire), 2059);
        assert_eq!(calls.get(), 1);
        assert!(claim_input_tokens(&remaining, 0, "{}", || 2).is_err());
        assert!(claim_input_tokens(&remaining, 3, "{}", || 2).is_err());
    }

    #[test]
    fn concurrent_large_inputs_cannot_claim_the_same_total_twice() {
        let remaining = AtomicU64::new(2059);
        let barrier = std::sync::Barrier::new(2);
        let text = "large input ".repeat(1024);
        std::thread::scope(|scope| {
            let claim = || {
                barrier.wait();
                claim_input_tokens(&remaining, 2, &text, || 1036)
            };
            let first = scope.spawn(claim);
            let second = scope.spawn(claim);
            let results = [
                first.join().expect("test: first claimant"),
                second.join().expect("test: second claimant"),
            ];
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(results.into_iter().filter_map(Result::ok).sum::<u64>(), 1037);
        });
        assert_eq!(remaining.load(Ordering::Acquire), 1022);
    }

    #[test]
    fn a_changed_allowance_rechecks_fit_without_repeating_the_estimator() {
        for (other_claim, expected) in [(10, Some(1037)), (1030, None)] {
            let remaining = AtomicU64::new(2059);
            let calls = std::cell::Cell::new(0);
            let result = claim_input_tokens(&remaining, 2, &"large input ".repeat(1024), || {
                calls.set(calls.get() + 1);
                remaining.fetch_sub(other_claim, Ordering::AcqRel);
                1036
            });
            assert_eq!(result.ok(), expected);
            assert_eq!(
                remaining.load(Ordering::Acquire),
                2059 - other_claim - expected.unwrap_or(0)
            );
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn reported_usage_above_tranche_consumes_unallocated_tokens() {
        let remaining = AtomicU64::new(2048);
        reconcile_token_allowance(&remaining, 2048, 3000);
        assert_eq!(remaining.load(Ordering::Acquire), 1096);
    }

    #[test]
    fn reported_usage_above_owner_budget_exhausts_tokens() {
        let remaining = AtomicU64::new(2048);
        reconcile_token_allowance(&remaining, 2048, 5000);
        assert_eq!(remaining.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reported_usage_below_tranche_releases_only_unused_estimate() {
        let remaining = AtomicU64::new(2048);
        reconcile_token_allowance(&remaining, 2048, 1000);
        assert_eq!(remaining.load(Ordering::Acquire), 3096);
    }
}

#[cfg(test)]
mod transport_tests;

// This is a sufficient proof for the existing local input proxy, never usage
// or a vendor token cap. cl100k emits at most one token per nonempty byte
// span; the fallback uses the existing scaled character heuristic. Keep the
// full serialized body. An inconclusive upper bound MUST run the old proxy.
fn input_token_upper_bound(text: &str) -> usize {
    use crate::agent::context_budget::{TokenMeasurement, TokenMeasurementSource, estimate_request_tokens};
    text.len().max(estimate_request_tokens(
        TokenMeasurement {
            tokens: text.chars().count().div_ceil(4),
            source: TokenMeasurementSource::Heuristic,
        },
        TokenMeasurement::NONE,
    ))
}

/// Prefer a fair retry share, but do not reject the first request merely
/// because its input needs more than that share. The existing owner allowance
/// is the hard limit; a claimed unknown attempt keeps everything it reserved.
fn claim_input_tokens(
    remaining: &AtomicU64,
    planned: u64,
    text: &str,
    mut estimate: impl FnMut() -> usize,
) -> anyhow::Result<u64> {
    anyhow::ensure!((1..=2).contains(&planned), "invalid physical attempt plan");
    let mut measured = None;
    let mut measure = || *measured.get_or_insert_with(&mut estimate);
    let mut available = remaining.load(Ordering::Acquire);
    loop {
        let share = available / planned;
        let tokens = if input_token_preflight(text, share, &mut measure)? {
            share
        } else {
            // The failed preflight has already measured the input.
            // Keep the existing strict input < reservation rule while leaving
            // all unnecessary tokens available to the bounded remaining plan.
            let required = u64::try_from(measure())?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("physical input token estimate overflow"))?;
            anyhow::ensure!(
                required <= available,
                "physical request exceeds the remaining decision token allowance"
            );
            required
        };
        match remaining.compare_exchange_weak(available, available - tokens, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(tokens),
            Err(current) => available = current,
        }
    }
}

fn input_token_preflight(
    text: &str,
    allowance: u64,
    estimate: impl FnOnce() -> usize,
) -> Result<bool, std::num::TryFromIntError> {
    #[cfg(feature = "llm-router")]
    if text.len() >= cl100k_prefix_bound::MIN_INPUT_BYTES
        && u64::try_from(cl100k_prefix_bound::upper_bound(text).unwrap_or(usize::MAX))
            .is_ok_and(|upper| upper < allowance)
    {
        return Ok(true);
    }
    if u64::try_from(input_token_upper_bound(text)).is_ok_and(|upper| upper < allowance) {
        return Ok(true);
    }
    Ok(u64::try_from(estimate())? < allowance)
}

#[cfg(test)]
mod token_preflight_tests;

#[cfg(test)]
fn accounting_diagnostic_failure(stage: &'static str) {
    let _ = std::io::Write::write_fmt(
        &mut std::io::stderr().lock(),
        format_args!("prx_accounting_failure stage={stage}\n"),
    );
}

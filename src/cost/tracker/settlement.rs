//! Metered cost and estimated-liability reconciliation under one ledger lock.
use super::{
    CostRecord, CostSettlement, CostStorage, CostTracker, LedgerBlocked, TokenUsage, acquire_cost_ledger_lock,
    budget_check, ceil_micro_usd, settlement_fingerprint,
};
use crate::cost::reservation::{ReservationReceipt, Reservations, SubscriptionCall, cost_row_sha256};
use crate::cost::types::SUBSCRIPTION_BILLING;
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;

#[cfg(test)]
#[derive(Debug, thiserror::Error)]
#[error("test: crash")]
pub(crate) struct TestCrash;

#[cfg(test)]
thread_local! {
    static CRASH_BEFORE_APPEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CRASH_AFTER_APPEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_IO_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn crash_before_append(crash: bool) {
    CRASH_BEFORE_APPEND.with(|cell| cell.set(crash));
}

#[cfg(test)]
pub(crate) fn crash_after_append(crash: bool) {
    CRASH_AFTER_APPEND.with(|cell| cell.set(crash));
}

#[cfg(test)]
pub(crate) fn force_io_error(force: bool) {
    FORCE_IO_ERROR.with(|cell| cell.set(force));
}

#[cfg(test)]
pub(crate) struct RecoveryProjectionFaults {
    before: bool,
    after: bool,
    io: bool,
}
#[cfg(test)]
pub(crate) struct RecoveryProjectionFaultGuard(RecoveryProjectionFaults);
#[cfg(test)]
pub(crate) fn take_recovery_projection_faults() -> RecoveryProjectionFaults {
    RecoveryProjectionFaults {
        before: CRASH_BEFORE_APPEND.with(|cell| cell.replace(false)),
        after: CRASH_AFTER_APPEND.with(|cell| cell.replace(false)),
        io: FORCE_IO_ERROR.with(|cell| cell.replace(false)),
    }
}
#[cfg(test)]
impl RecoveryProjectionFaults {
    pub(crate) fn install(self) -> RecoveryProjectionFaultGuard {
        RecoveryProjectionFaultGuard(Self {
            before: CRASH_BEFORE_APPEND.with(|cell| cell.replace(self.before)),
            after: CRASH_AFTER_APPEND.with(|cell| cell.replace(self.after)),
            io: FORCE_IO_ERROR.with(|cell| cell.replace(self.io)),
        })
    }
}
#[cfg(test)]
impl Drop for RecoveryProjectionFaultGuard {
    fn drop(&mut self) {
        CRASH_BEFORE_APPEND.with(|cell| cell.set(self.0.before));
        CRASH_AFTER_APPEND.with(|cell| cell.set(self.0.after));
        FORCE_IO_ERROR.with(|cell| cell.set(self.0.io));
    }
}

/// What one subscription append did.
enum SubscriptionAppend {
    /// The `$0` row was written now.
    Appended(Box<CostRecord>),
    /// The settlement already had its verified subscription row.
    Replayed,
}

impl CostTracker {
    /// Project authenticated recovery usage into the cost ledger, by the
    /// outbox row's signed pricing (D8). No budget check.
    pub(crate) fn project_recovery_usage(
        &self,
        record: &MeteredTokenUsageRecord,
        pricing: crate::memory::recovery_authority::RecoveryPricing,
    ) -> Result<()> {
        use crate::memory::recovery_authority::RecoveryPricing;
        let settlement_id = record
            .settlement_id
            .as_deref()
            .ok_or_else(|| anyhow!("Metered usage settlement is missing settlement_id"))?;
        anyhow::ensure!(!settlement_id.trim().is_empty(), "Metered usage settlement_id is empty");
        let cost_usd = match pricing {
            RecoveryPricing::Metered => record
                .cost_usd
                .ok_or_else(|| anyhow!("recovery cost projection requires a priced usage record"))?,
            RecoveryPricing::Subscription => {
                anyhow::ensure!(
                    record.cost_usd.is_none_or(|cost| cost.to_bits() == 0.0_f64.to_bits()),
                    LedgerBlocked::Conflict
                );
                0.0
            }
            RecoveryPricing::Unknown => return Err(UnknownRecoveryPricing.into()),
        };
        anyhow::ensure!(
            cost_usd.is_finite() && cost_usd >= 0.0,
            "Metered usage cost must be a finite, non-negative value"
        );
        #[cfg(test)]
        if CRASH_BEFORE_APPEND.with(std::cell::Cell::get) {
            CRASH_BEFORE_APPEND.with(|cell| cell.set(false));
            return Err(TestCrash.into());
        }
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        if pricing == RecoveryPricing::Subscription {
            if matches!(
                self.append_subscription(&mut storage, settlement_id, record)?,
                SubscriptionAppend::Replayed
            ) {
                sync_replayed(&storage)?;
            }
            return Ok(());
        }
        if storage.has_settlement(settlement_id) {
            storage.verify_replayed_settlement(record, cost_usd)?;
            return sync_replayed(&storage);
        }
        let usage = TokenUsage {
            model: format!("{}/{}", record.provider, record.model),
            input_tokens: record.prompt_tokens,
            output_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
            cost_usd,
            timestamp: Utc::now(),
        };
        let mut cost_record = CostRecord::from_settlement(&self.session_id, settlement_id, usage);
        cost_record.settlement_sha256 = Some(settlement_fingerprint(record, cost_usd)?);
        cost_record.settlement_provider = Some(record.provider.clone());
        cost_record.settlement_source = Some(record.source);
        cost_record.cache_creation_input_tokens = Some(record.cache_creation_input_tokens);
        cost_record.cache_read_input_tokens = Some(record.cache_read_input_tokens);
        #[cfg(test)]
        if FORCE_IO_ERROR.with(std::cell::Cell::get) {
            FORCE_IO_ERROR.with(|cell| cell.set(false));
            anyhow::bail!("test: recovery cost projection I/O error");
        }
        storage.add_record(cost_record)?;
        #[cfg(test)]
        if CRASH_AFTER_APPEND.with(std::cell::Cell::get) {
            CRASH_AFTER_APPEND.with(|cell| cell.set(false));
            return Err(TestCrash.into());
        }
        Ok(())
    }

    /// The one subscription append every path uses (D9). The caller holds
    /// the ledger lock on a refreshed snapshot. A stored row for the
    /// settlement must be the same `$0` subscription row (else
    /// [`LedgerBlocked::Conflict`]); otherwise one `$0` row with the
    /// unchanged settlement fingerprint and `billing = "subscription"` is
    /// appended. No budget check runs.
    fn append_subscription(
        &self,
        storage: &mut CostStorage,
        settlement_id: &str,
        record: &MeteredTokenUsageRecord,
    ) -> Result<SubscriptionAppend> {
        if storage.has_settlement(settlement_id) {
            storage.verify_replayed_settlement(record, 0.0)?;
            anyhow::ensure!(
                storage.settlement_billing(settlement_id)?.as_deref() == Some(SUBSCRIPTION_BILLING),
                LedgerBlocked::Conflict
            );
            return Ok(SubscriptionAppend::Replayed);
        }
        subscription_settlement(record)?;
        let usage = TokenUsage {
            model: format!("{}/{}", record.provider, record.model),
            input_tokens: record.prompt_tokens,
            output_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
            cost_usd: 0.0,
            timestamp: Utc::now(),
        };
        let mut cost_record = CostRecord::from_settlement(&self.session_id, settlement_id, usage);
        cost_record.settlement_sha256 = Some(settlement_fingerprint(record, 0.0)?);
        cost_record.settlement_provider = Some(record.provider.clone());
        cost_record.settlement_source = Some(record.source);
        cost_record.cache_creation_input_tokens = Some(record.cache_creation_input_tokens);
        cost_record.cache_read_input_tokens = Some(record.cache_read_input_tokens);
        cost_record.billing = Some(SUBSCRIPTION_BILLING.to_owned());
        #[cfg(test)]
        if FORCE_IO_ERROR.with(std::cell::Cell::get) {
            FORCE_IO_ERROR.with(|cell| cell.set(false));
            anyhow::bail!("test: subscription cost append I/O error");
        }
        storage.add_record(cost_record.clone())?;
        #[cfg(test)]
        if CRASH_AFTER_APPEND.with(std::cell::Cell::get) {
            CRASH_AFTER_APPEND.with(|cell| cell.set(false));
            return Err(TestCrash.into());
        }
        Ok(SubscriptionAppend::Appended(Box::new(cost_record)))
    }

    /// The canonical cost append remains durable before the journal changes.
    /// Reuse the same locked snapshot and the row just written instead of
    /// reopening, rescanning and reacquiring the ledger between both steps.
    pub(crate) fn settle_and_reconcile_estimated(
        &self,
        receipt: &ReservationReceipt,
        record: &MeteredTokenUsageRecord,
    ) -> Result<CostSettlement> {
        anyhow::ensure!(
            record.source == TokenUsageSource::Reported,
            "estimated settlement requires reported usage"
        );
        self.settle_metered_inner(record, Some(receipt))
    }

    pub(super) fn settle_metered_inner(
        &self,
        record: &MeteredTokenUsageRecord,
        receipt: Option<&ReservationReceipt>,
    ) -> Result<CostSettlement> {
        let config = self.config.clone();
        let settlement_id = record
            .settlement_id
            .as_deref()
            .ok_or_else(|| anyhow!("Metered usage settlement is missing settlement_id"))?;
        anyhow::ensure!(!settlement_id.trim().is_empty(), "Metered usage settlement_id is empty");
        let Some(cost_usd) = record.cost_usd else {
            if matches!(
                crate::llm::route_decision::usage_cost_resolution(
                    &record.provider,
                    &record.model,
                    record.prompt_tokens,
                    record.completion_tokens,
                    record.cache_creation_input_tokens,
                    record.cache_read_input_tokens,
                    &config,
                ),
                crate::llm::route_decision::UsagePrice::Subscription
            ) {
                let settlement = subscription_settlement(record)?;
                let mut storage = self.lock_storage();
                let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
                storage.refresh_from_disk()?;
                let appended = self.append_subscription(&mut storage, settlement_id, record)?;
                if let SubscriptionAppend::Appended(row) = &appended {
                    self.lock_session_costs().push(row.as_ref().clone());
                }
                if let Some(receipt) = receipt {
                    reconcile_subscription_hold(&storage, receipt, record)?;
                }
                return Ok(match appended {
                    SubscriptionAppend::Appended(_) => settlement,
                    SubscriptionAppend::Replayed => CostSettlement::Replayed,
                });
            }
            anyhow::ensure!(receipt.is_none(), "reserved settlement price is unknown");
            return Ok(CostSettlement::UnknownPricing);
        };
        anyhow::ensure!(
            cost_usd.is_finite() && cost_usd >= 0.0,
            "Metered usage cost must be a finite, non-negative value"
        );
        let mut storage = self.lock_storage();
        let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
        storage.refresh_from_disk()?;
        if storage.has_settlement(settlement_id) {
            storage.verify_replayed_settlement(record, cost_usd)?;
            if let Some(receipt) = receipt {
                reconcile_durable_projection(&storage, receipt, record, ceil_micro_usd(cost_usd)?, None)?;
            }
            return Ok(CostSettlement::Replayed);
        }
        let (daily_cost, monthly_cost) = storage.get_aggregated_costs()?;
        let budget = budget_check(
            &config,
            daily_cost,
            monthly_cost,
            storage.daily_micro_usd,
            storage.monthly_micro_usd,
            cost_usd,
        );
        let usage = TokenUsage {
            model: format!("{}/{}", record.provider, record.model),
            input_tokens: record.prompt_tokens,
            output_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
            cost_usd,
            timestamp: Utc::now(),
        };
        let mut cost_record = CostRecord::from_settlement(&self.session_id, settlement_id, usage);
        cost_record.settlement_sha256 = Some(settlement_fingerprint(record, cost_usd)?);
        cost_record.settlement_provider = Some(record.provider.clone());
        cost_record.settlement_source = Some(record.source);
        cost_record.cache_creation_input_tokens = Some(record.cache_creation_input_tokens);
        cost_record.cache_read_input_tokens = Some(record.cache_read_input_tokens);
        storage.add_record(cost_record.clone())?;
        // Usage was incurred: retain its session projection even if subsequent
        // budget assessment or journal reconciliation fails.
        self.lock_session_costs().push(cost_record.clone());
        let budget = budget?;
        if let Some(receipt) = receipt {
            // Use the same JSON round trip as the ledger reader, including
            // floating-point decoding, without rescanning the whole file.
            let projection: CostRecord = serde_json::from_slice(&serde_json::to_vec(&cost_record)?)?;
            reconcile_durable_projection(&storage, receipt, record, ceil_micro_usd(cost_usd)?, Some(&projection))?;
        }
        Ok(CostSettlement::Recorded { budget })
    }
}

/// A recovery projection was asked to project an unknown-priced row, which
/// has no ledger row.
#[derive(Debug, thiserror::Error)]
#[error("recovery cost projection refuses an unknown-priced row")]
pub(crate) struct UnknownRecoveryPricing;

/// Make a verified replay durable before its outbox row is acknowledged.
fn sync_replayed(storage: &CostStorage) -> Result<()> {
    if let Some((file, _)) = super::open_cost_file(&storage.path, super::CostFileAccess::Read)? {
        file.sync_all()
            .with_context(|| format!("Failed to sync cost storage at {}", storage.path.display()))?;
    }
    #[cfg(unix)]
    if let Some(parent) = storage.path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn reconcile_durable_projection(
    storage: &CostStorage,
    receipt: &ReservationReceipt,
    record: &MeteredTokenUsageRecord,
    actual: u128,
    known_projection: Option<&CostRecord>,
) -> Result<()> {
    anyhow::ensure!(
        record.settlement_id.as_deref() == Some(receipt.attempt_id()),
        "reserved settlement does not identify this physical attempt"
    );
    let cost = record.cost_usd.context("reserved settlement price is unknown")?;
    let digest = settlement_fingerprint(record, cost)?;
    let replayed;
    let projection = if let Some(projection) = known_projection {
        projection
    } else {
        let mut found = None;
        storage.for_each_record(|stored| {
            if stored.settlement_id == record.settlement_id {
                found = Some(stored);
            }
        })?;
        replayed = found.context("reservation settlement cost projection is missing")?;
        &replayed
    };
    anyhow::ensure!(
        projection.settlement_sha256.as_deref() == Some(digest.as_str()),
        "reservation settlement requires a canonical cost projection"
    );
    let row_digest = cost_row_sha256(projection)?;
    let workspace = storage.workspace()?;
    let mut reservations = Reservations::load(workspace)?;
    let actual_tokens = u128::from(record.prompt_tokens) + u128::from(record.completion_tokens);
    anyhow::ensure!(
        actual_tokens == u128::from(record.total_tokens),
        "estimated settlement token total conflicts"
    );
    reservations.settle_estimated(workspace, receipt, actual, actual_tokens, digest, row_digest)
}

fn subscription_settlement(record: &MeteredTokenUsageRecord) -> Result<CostSettlement> {
    anyhow::ensure!(
        u128::from(record.prompt_tokens).checked_add(u128::from(record.completion_tokens))
            == Some(u128::from(record.total_tokens)),
        "subscription settlement token total conflicts"
    );
    Ok(CostSettlement::Subscription {
        provider: record.provider.clone(),
        model: record.model.clone(),
        prompt_tokens: record.prompt_tokens,
        completion_tokens: record.completion_tokens,
        total_tokens: record.total_tokens,
    })
}

fn reconcile_subscription_hold(
    storage: &CostStorage,
    receipt: &ReservationReceipt,
    record: &MeteredTokenUsageRecord,
) -> Result<()> {
    anyhow::ensure!(
        record.settlement_id.as_deref() == Some(receipt.attempt_id()),
        "reserved settlement does not identify this physical attempt"
    );
    subscription_settlement(record)?;
    let workspace = storage.workspace()?;
    let mut reservations = Reservations::load(workspace)?;
    reservations.settle_subscription(
        workspace,
        receipt,
        SubscriptionCall {
            provider: record.provider.clone(),
            model: record.model.clone(),
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
        },
    )
}

#[cfg(test)]
mod subscription_tests;
#[cfg(test)]
mod tests;

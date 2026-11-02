//! Metered cost and estimated-liability reconciliation under one ledger lock.
use super::{
    CostRecord, CostSettlement, CostStorage, CostTracker, TokenUsage, acquire_cost_ledger_lock, budget_check,
    ceil_micro_usd, settlement_fingerprint,
};
use crate::cost::reservation::{ReservationReceipt, Reservations, SubscriptionCall, cost_row_sha256};
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;

impl CostTracker {
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
                if let Some(receipt) = receipt {
                    let mut storage = self.lock_storage();
                    let _ledger_lock = acquire_cost_ledger_lock(&storage.path)?;
                    storage.refresh_from_disk()?;
                    reconcile_subscription_hold(&storage, receipt, record)?;
                }
                return Ok(settlement);
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
mod tests;

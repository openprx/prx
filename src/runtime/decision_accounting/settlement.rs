//! Known usage can overlap semantic admission while its durable hold stays owned.
use super::{OwnedReceipt, reconcile_token_allowance};
use crate::decision::DecisionError;
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource, UsagePrice, usage_cost_resolution};
use std::sync::atomic::Ordering;

impl OwnedReceipt {
    pub(in crate::runtime) fn settle_provider_usage(
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
        let result = (|| -> anyhow::Result<_> {
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
            let actual_micro_usd = crate::cost::tracker::ceil_micro_usd(cost)?;

            Ok((record, total, actual_micro_usd))
        })();
        let (record, total, actual_micro_usd) = match result {
            Ok(prepared) => prepared,
            Err(_) => {
                ticket.finish(false);
                return Err(DecisionError::BudgetExceeded);
            }
        };
        if self.remaining.settlement_failed.load(Ordering::Acquire) {
            ticket.finish(false);
            return Err(DecisionError::BudgetExceeded);
        }
        if actual_micro_usd <= u128::from(self.reserved_micro_usd) {
            self.remaining.micro_usd.fetch_add(
                self.reserved_micro_usd - u64::try_from(actual_micro_usd).unwrap_or(u64::MAX),
                Ordering::AcqRel,
            );
        } else {
            let excess = u64::try_from(actual_micro_usd - u128::from(self.reserved_micro_usd)).unwrap_or(u64::MAX);
            let _ = self
                .remaining
                .micro_usd
                .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some(value.saturating_sub(excess))
                });
        }

        reconcile_token_allowance(&self.remaining.tokens, self.reserved_tokens, total);
        // Memory reflects known usage immediately; the durable dispatched hold
        // remains conservative until both journal writes finish. Another physical
        // send waits for the gate already retained by this receipt. No await
        // separates reported usage from cleanup-owned persistence; cancellation
        // cannot discard a settlement while waiting behind another reservation.
        tokio::task::spawn_blocking(move || {
            let result = self.tracker.settle_and_reconcile_estimated(&self.receipt, &record);
            if result.is_err() {
                self.remaining.settlement_failed.store(true, Ordering::Release);
                tracing::warn!("decision usage persistence was not confirmed; further accounting is closed");
            }
            ticket.finish(result.is_ok());
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;

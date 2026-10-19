#![cfg(test)]
#![cfg(unix)]

use super::super::CostTracker;
use crate::config::schema::CostConfig;
use crate::cost::reservation::{EstimatedReservationRequest, JOURNAL, ReservationRequest};
use crate::cost::tracker::remaining_micro_usd_read_only;
use crate::cost::types::CostSettlement;
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

fn config() -> CostConfig {
    CostConfig {
        daily_limit_usd: 1.0,
        monthly_limit_usd: 1.0,
        ..CostConfig::default()
    }
}
fn request() -> EstimatedReservationRequest {
    EstimatedReservationRequest {
        reservation: ReservationRequest {
            turn_id: "prx_settlement_turn".to_owned(),
            attempt_id: "prx_settlement_attempt".to_owned(),
            binding_sha256: hex::encode([3_u8; 32]),
            maximum_micro_usd: 250_000,
            turn_limit_micro_usd: 1_000_000,
            security_daily_micro_usd: 1_000_000,
        },
        reserved_tokens: 1024,
        turn_token_limit: 4096,
    }
}
fn usage() -> MeteredTokenUsageRecord {
    MeteredTokenUsageRecord {
        settlement_id: Some("prx_settlement_attempt".to_owned()),
        provider: "prx_provider".to_owned(),
        model: "prx_model".to_owned(),
        prompt_tokens: 8,
        completion_tokens: 4,
        total_tokens: 12,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: TokenUsageSource::Reported,
        cost_usd: Some(0.125),
    }
}
fn rows(path: &Path) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        rows.push(serde_json::from_str(line)?);
    }
    Ok(rows)
}

#[test]
fn combined_settlement_is_durable_idempotent_and_rejects_conflicting_replay() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(request(), || true)?;
    let usage = usage();
    assert!(matches!(
        tracker.settle_and_reconcile_estimated(&receipt, &usage)?,
        CostSettlement::Recorded { .. }
    ));
    let stored = rows(&workspace.path().join("state/costs.jsonl"))?;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored
            .first()
            .context("canonical row")?
            .get("settlement_id")
            .and_then(Value::as_str),
        Some("prx_settlement_attempt")
    );
    let journal = rows(&workspace.path().join("state").join(JOURNAL))?;
    assert_eq!(journal.len(), 3);
    let settled = journal.last().context("settlement event")?;
    assert_eq!(
        settled.get("event").and_then(Value::as_str),
        Some("estimated_settled_v1")
    );
    assert_eq!(settled.get("actual_micro_usd").and_then(Value::as_str), Some("125000"));
    assert_eq!(settled.get("actual_tokens").and_then(Value::as_str), Some("12"));
    assert_eq!(remaining_micro_usd_read_only(&config(), workspace.path())?, 875_000);
    let reopened = CostTracker::new(config(), workspace.path())?;
    assert!(matches!(
        reopened.settle_and_reconcile_estimated(&receipt, &usage)?,
        CostSettlement::Replayed
    ));
    let mut conflict = usage;
    conflict.completion_tokens += 1;
    conflict.total_tokens += 1;
    assert!(reopened.settle_and_reconcile_estimated(&receipt, &conflict).is_err());
    assert_eq!(rows(&workspace.path().join("state/costs.jsonl"))?, stored);
    assert_eq!(rows(&workspace.path().join("state").join(JOURNAL))?, journal);
    Ok(())
}

#[test]
fn journal_failure_keeps_the_durable_charge_and_session_snapshot() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(request(), || true)?;
    fs::write(workspace.path().join("state").join(JOURNAL), b"malformed journal\n")?;
    assert!(tracker.settle_and_reconcile_estimated(&receipt, &usage()).is_err());
    assert_eq!(rows(&workspace.path().join("state/costs.jsonl"))?.len(), 1);
    let summary = tracker.get_summary()?;
    assert_eq!(summary.session_cost_usd, 0.125);
    assert_eq!(summary.total_tokens, 12);
    assert_eq!(summary.request_count, 1);
    assert!(remaining_micro_usd_read_only(&config(), workspace.path()).is_err());
    Ok(())
}

#[test]
fn incomplete_or_unpriced_usage_keeps_the_dispatched_liability() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(request(), || true)?;
    let before = rows(&workspace.path().join("state").join(JOURNAL))?;
    let mut estimated = usage();
    estimated.source = TokenUsageSource::Estimated;
    assert!(tracker.settle_and_reconcile_estimated(&receipt, &estimated).is_err());
    let mut unpriced = usage();
    unpriced.cost_usd = None;
    assert!(tracker.settle_and_reconcile_estimated(&receipt, &unpriced).is_err());
    assert_eq!(rows(&workspace.path().join("state").join(JOURNAL))?, before);
    assert_eq!(remaining_micro_usd_read_only(&config(), workspace.path())?, 750_000);
    assert!(!workspace.path().join("state/costs.jsonl").exists());
    Ok(())
}

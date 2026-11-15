//! CF-R8 (D9): the `$0` subscription ledger row on the non-recovery path,
//! over a real ledger and reservation journal in a temp workspace.
#![cfg(test)]
#![cfg(unix)]

use super::super::CostTracker;
use crate::config::schema::CostConfig;
use crate::cost::reservation::{EstimatedReservationRequest, ReservationRequest, cost_row_sha256};
use crate::cost::tracker::{
    crash_after_append, force_io_error, remaining_micro_usd_read_only, settlement_row_is_authentic,
};
use crate::cost::types::{CostRecord, CostSettlement, TokenUsage};
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};
use anyhow::{Context, Result};
use std::path::Path;

const ATTEMPT: &str = "prx_subscription_attempt";

fn config() -> CostConfig {
    CostConfig {
        daily_limit_usd: 1.0,
        monthly_limit_usd: 1.0,
        ..CostConfig::default()
    }
}

fn reservation() -> EstimatedReservationRequest {
    EstimatedReservationRequest {
        reservation: ReservationRequest {
            turn_id: "prx_subscription_turn".to_owned(),
            attempt_id: ATTEMPT.to_owned(),
            binding_sha256: hex::encode([5_u8; 32]),
            maximum_micro_usd: 0,
            turn_limit_micro_usd: 1_000_000,
            security_daily_micro_usd: 1_000_000,
        },
        reserved_tokens: 1024,
        turn_token_limit: 4096,
        group: None,
    }
}

fn subscription_usage() -> MeteredTokenUsageRecord {
    MeteredTokenUsageRecord {
        settlement_id: Some(ATTEMPT.to_owned()),
        provider: "kimi-code".to_owned(),
        model: "kimi-for-coding".to_owned(),
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: TokenUsageSource::Reported,
        cost_usd: None,
        settlement_class: None,
    }
}

fn ledger_rows(workspace: &Path) -> Result<Vec<CostRecord>> {
    let path = workspace.join("state/costs.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn assert_one_subscription_row(workspace: &Path) -> Result<()> {
    let rows = ledger_rows(workspace)?;
    assert_eq!(rows.len(), 1, "exactly one ledger row");
    let row = rows.first().context("subscription row")?;
    assert_eq!(row.billing.as_deref(), Some("subscription"));
    assert_eq!(row.usage.cost_usd.to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        (row.usage.input_tokens, row.usage.output_tokens, row.usage.total_tokens),
        (10, 5, 15)
    );
    assert_eq!(row.settlement_id.as_deref(), Some(ATTEMPT));
    assert!(
        settlement_row_is_authentic(row),
        "the existing fingerprint verifies the row"
    );
    Ok(())
}

fn expected_settlement(record: &MeteredTokenUsageRecord) -> CostSettlement {
    CostSettlement::Subscription {
        provider: record.provider.clone(),
        model: record.model.clone(),
        prompt_tokens: record.prompt_tokens,
        completion_tokens: record.completion_tokens,
        total_tokens: record.total_tokens,
    }
}

#[test]
fn no_receipt_settlement_writes_one_row_and_replays() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let record = subscription_usage();
    let tracker = CostTracker::new(config(), workspace.path())?;
    assert_eq!(tracker.settle_metered(&record)?, expected_settlement(&record));
    assert_one_subscription_row(workspace.path())?;
    let second = CostTracker::new(config(), workspace.path())?;
    assert_eq!(second.settle_metered(&record)?, CostSettlement::Replayed);
    assert_one_subscription_row(workspace.path())?;
    let summary = second.get_summary()?;
    assert_eq!(summary.daily_cost_usd.to_bits(), 0.0_f64.to_bits());
    assert_eq!(summary.monthly_cost_usd.to_bits(), 0.0_f64.to_bits());
    assert_eq!(remaining_micro_usd_read_only(&config(), workspace.path())?, 1_000_000);
    Ok(())
}

#[test]
fn receipt_settlement_writes_one_row_and_reconciles_the_hold() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let record = subscription_usage();
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(reservation(), || true)?;
    assert_eq!(
        tracker.settle_and_reconcile_estimated(&receipt, &record)?,
        expected_settlement(&record)
    );
    assert_one_subscription_row(workspace.path())?;
    let reopened = CostTracker::new(config(), workspace.path())?;
    assert_eq!(
        reopened.settle_and_reconcile_estimated(&receipt, &record)?,
        CostSettlement::Replayed
    );
    assert_one_subscription_row(workspace.path())?;
    let usage = reopened.physical_scope_usage("prx_subscription_turn")?;
    assert_eq!((usage.settled_calls, usage.tokens, usage.micro_usd), (1, 15, 0));
    assert_eq!(remaining_micro_usd_read_only(&config(), workspace.path())?, 1_000_000);
    Ok(())
}

#[test]
fn conflicting_replay_is_refused_and_keeps_one_row() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let record = subscription_usage();
    let tracker = CostTracker::new(config(), workspace.path())?;
    tracker.settle_metered(&record)?;
    let mut changed = record.clone();
    changed.completion_tokens += 1;
    changed.total_tokens += 1;
    let refused = tracker.settle_metered(&changed).expect_err("different tokens conflict");
    assert!(crate::runtime::cost_recovery::is_blocked(&refused), "{refused:#}");
    assert_one_subscription_row(workspace.path())?;

    // A stored metered row with the same settlement id is not a subscription row.
    let metered_workspace = tempfile::tempdir()?;
    let metered_tracker = CostTracker::new(config(), metered_workspace.path())?;
    let mut metered = record;
    metered.provider = "prx_provider".to_owned();
    metered.model = "prx_model".to_owned();
    metered.cost_usd = Some(0.0);
    assert!(matches!(
        metered_tracker.settle_metered(&metered)?,
        CostSettlement::Recorded { .. }
    ));
    let mut same_projection = metered;
    same_projection.cost_usd = None;
    let refused = metered_tracker
        .project_recovery_usage(
            &same_projection,
            crate::memory::recovery_authority::RecoveryPricing::Subscription,
        )
        .expect_err("a metered row is not a subscription replay");
    assert!(crate::runtime::cost_recovery::is_blocked(&refused), "{refused:#}");
    assert_eq!(ledger_rows(metered_workspace.path())?.len(), 1);
    Ok(())
}

#[test]
fn crash_after_append_then_retry_ends_with_one_row_and_a_reconciled_hold() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let record = subscription_usage();
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(reservation(), || true)?;
    crash_after_append(true);
    assert!(tracker.settle_and_reconcile_estimated(&receipt, &record).is_err());
    crash_after_append(false);
    assert_eq!(
        ledger_rows(workspace.path())?.len(),
        1,
        "the row was appended before the crash"
    );
    assert_eq!(tracker.physical_scope_usage("prx_subscription_turn")?.settled_calls, 0);
    assert_eq!(
        tracker.settle_and_reconcile_estimated(&receipt, &record)?,
        CostSettlement::Replayed
    );
    assert_one_subscription_row(workspace.path())?;
    assert_eq!(tracker.physical_scope_usage("prx_subscription_turn")?.settled_calls, 1);
    Ok(())
}

#[test]
fn io_error_then_retry_ends_with_one_row_and_a_reconciled_hold() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let record = subscription_usage();
    let tracker = CostTracker::new(config(), workspace.path())?;
    let receipt = tracker.reserve_dispatched_estimated_attempt(reservation(), || true)?;
    force_io_error(true);
    assert!(tracker.settle_and_reconcile_estimated(&receipt, &record).is_err());
    force_io_error(false);
    assert!(ledger_rows(workspace.path())?.is_empty());
    assert_eq!(
        tracker.settle_and_reconcile_estimated(&receipt, &record)?,
        expected_settlement(&record)
    );
    assert_one_subscription_row(workspace.path())?;
    assert_eq!(tracker.physical_scope_usage("prx_subscription_turn")?.settled_calls, 1);
    Ok(())
}

/// A metered row serializes exactly as before `billing` existed: the field is
/// skipped, so its bytes and `cost_row_sha256` are unchanged.
#[test]
fn metered_row_bytes_and_row_digest_are_unchanged() -> Result<()> {
    let usage = TokenUsage {
        model: "prx_provider/prx_model".to_owned(),
        input_tokens: 8,
        output_tokens: 4,
        total_tokens: 12,
        cost_usd: 0.125,
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")?.with_timezone(&chrono::Utc),
    };
    let mut row = CostRecord::from_settlement("prx_session", "prx_settlement", usage);
    row.settlement_sha256 = Some("ab".repeat(32));
    row.settlement_provider = Some("prx_provider".to_owned());
    row.settlement_source = Some(TokenUsageSource::Reported);
    row.cache_creation_input_tokens = Some(0);
    row.cache_read_input_tokens = Some(0);
    let main_bytes = concat!(
        r#"{"id":"prx_settlement","settlement_id":"prx_settlement","settlement_sha256":""#,
        "abababababababababababababababababababababababababababababababab",
        r#"","usage":{"model":"prx_provider/prx_model","input_tokens":8,"output_tokens":4,"total_tokens":12,"#,
        r#""cost_usd":0.125,"timestamp":"2026-01-02T03:04:05Z"},"session_id":"prx_session","#,
        r#""settlement_provider":"prx_provider","settlement_source":"reported","#,
        r#""cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#
    );
    assert_eq!(serde_json::to_string(&row)?, main_bytes);
    let mut hash = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hash, b"prx-cost-reservation-projection-v1\0");
    sha2::Digest::update(&mut hash, main_bytes.as_bytes());
    assert_eq!(cost_row_sha256(&row)?, hex::encode(sha2::Digest::finalize(hash)));
    Ok(())
}

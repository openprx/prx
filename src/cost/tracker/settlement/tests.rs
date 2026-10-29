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
use std::{fs, path::Path, sync::Arc};

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
        group: None,
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

#[test]
fn experiment_physical_call_cap_survives_unknown_usage_and_reopen() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let first = tracker.reserve_dispatched_estimated_attempt_with_limit(request(), Some(1), || true)?;
    drop(first);
    let usage = tracker.physical_scope_usage("prx_settlement_turn")?;
    assert_eq!(
        (usage.calls, usage.settled_calls, usage.tokens, usage.micro_usd),
        (1, 0, 1024, 250_000)
    );
    assert!(usage.unresolved);
    let mut second = request();
    second.reservation.attempt_id = "experiment-retry".into();
    assert!(
        tracker
            .reserve_dispatched_estimated_attempt_with_limit(second.clone(), Some(1), || true)
            .is_err()
    );
    drop(tracker);
    let reopened = CostTracker::new(config(), workspace.path())?;
    assert!(
        reopened
            .reserve_dispatched_estimated_attempt_with_limit(second, Some(1), || true)
            .is_err()
    );
    assert_eq!(rows(&workspace.path().join("state").join(JOURNAL))?.len(), 2);
    Ok(())
}

#[test]
fn experiment_competing_physical_requests_share_one_durable_call_cap() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    CostTracker::new(config(), workspace.path())?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers = [0, 1].map(|index| {
        let barrier = barrier.clone();
        let path = workspace.path().to_path_buf();
        std::thread::spawn(move || {
            let tracker = CostTracker::new(config(), &path).unwrap();
            let mut request = request();
            request.reservation.attempt_id = format!("experiment-competitor-{index}");
            barrier.wait();
            tracker.reserve_dispatched_estimated_attempt_with_limit(request, Some(1), || true)
        })
    });
    assert_eq!(
        workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .count(),
        1
    );
    let usage = CostTracker::new(config(), workspace.path())?.physical_scope_usage("prx_settlement_turn")?;
    assert_eq!(usage.calls, 1);
    assert!(usage.unresolved);
    Ok(())
}

fn grouped_request(stage: &str, calls: u64, tokens: u64, micro_usd: u64) -> EstimatedReservationRequest {
    let mut request = request();
    request.reservation.turn_id = stage.into();
    request.reservation.attempt_id = format!("{stage}-attempt");
    request.group = Some(crate::cost::reservation::ReservationGroup {
        id: hex::encode([9; 32]),
        calls,
        tokens,
        micro_usd,
    });
    request
}

#[test]
fn experiment_parent_limits_include_unknown_requests_from_other_stages_after_reopen() -> Result<()> {
    for (calls, tokens, money) in [(1, 4096, 1_000_000), (4, 1500, 1_000_000), (4, 4096, 400_000)] {
        let workspace = tempfile::tempdir()?;
        let tracker = CostTracker::new(config(), workspace.path())?;
        tracker.reserve_dispatched_estimated_attempt(grouped_request("stage-a", calls, tokens, money), || true)?;
        let reopened = CostTracker::new(config(), workspace.path())?;
        assert!(
            reopened
                .reserve_dispatched_estimated_attempt(grouped_request("stage-b", calls, tokens, money), || true)
                .is_err()
        );
        assert_eq!(rows(&workspace.path().join("state").join(JOURNAL))?.len(), 2);
    }
    Ok(())
}

#[test]
fn experiment_parent_records_late_overrun_and_refuses_a_different_stage() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let first = grouped_request("stage-a", 8, 4096, 500_000);
    let receipt = tracker.reserve_dispatched_estimated_attempt(first.clone(), || true)?;
    let mut actual = usage();
    actual.settlement_id = Some(first.reservation.attempt_id);
    actual.cost_usd = Some(0.6);
    tracker.settle_and_reconcile_estimated(&receipt, &actual)?;
    let reopened = CostTracker::new(config(), workspace.path())?;
    assert_eq!(reopened.physical_scope_usage("stage-a")?.micro_usd, 600_000);
    let mut second = grouped_request("stage-b", 8, 4096, 500_000);
    second.reservation.maximum_micro_usd = 1;
    assert!(reopened.reserve_dispatched_estimated_attempt(second, || true).is_err());
    Ok(())
}

#[test]
fn experiment_different_stage_writers_compete_under_one_parent_lock() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    CostTracker::new(config(), workspace.path())?;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers = [0, 1].map(|index| {
        let barrier = barrier.clone();
        let path = workspace.path().to_owned();
        std::thread::spawn(move || {
            let tracker = CostTracker::new(config(), &path).unwrap();
            let request = grouped_request(&format!("stage-{index}"), 1, 4096, 1_000_000);
            barrier.wait();
            tracker.reserve_dispatched_estimated_attempt(request, || true)
        })
    });
    assert_eq!(
        workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .count(),
        1
    );
    Ok(())
}

#[test]
fn experiment_parent_token_overrun_blocks_a_different_stage_without_clamping_usage() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    let first = grouped_request("stage-a", 8, 2048, 1_000_000);
    let receipt = tracker.reserve_dispatched_estimated_attempt(first.clone(), || true)?;
    let mut actual = usage();
    actual.settlement_id = Some(first.reservation.attempt_id);
    actual.prompt_tokens = 2050;
    actual.completion_tokens = 4;
    actual.total_tokens = 2054;
    tracker.settle_and_reconcile_estimated(&receipt, &actual)?;
    let reopened = CostTracker::new(config(), workspace.path())?;
    assert_eq!(reopened.physical_scope_usage("stage-a")?.tokens, 2054);
    let mut next = grouped_request("stage-b", 8, 2048, 1_000_000);
    next.reserved_tokens = 1;
    assert!(reopened.reserve_dispatched_estimated_attempt(next, || true).is_err());
    Ok(())
}

#[test]
fn experiment_case_usage_excludes_prior_unknown_attempts_without_freeing_parent_liability() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = CostTracker::new(config(), workspace.path())?;
    tracker.reserve_dispatched_estimated_attempt(request(), || true)?;
    let before = tracker.physical_scope_usage("prx_settlement_turn")?;
    assert!(before.unresolved);
    let mut next = request();
    next.reservation.attempt_id = "next-case-attempt".into();
    let receipt = tracker.reserve_dispatched_estimated_attempt(next, || true)?;
    let mut actual = usage();
    actual.settlement_id = Some("next-case-attempt".into());
    actual.model = "next-case-model".into();
    tracker.settle_and_reconcile_estimated(&receipt, &actual)?;
    let current = tracker.physical_scope_usage_since("prx_settlement_turn", &before.attempt_ids)?;
    assert_eq!((current.calls, current.tokens, current.micro_usd), (1, 12, 125_000));
    assert_eq!(current.models, ["prx_provider/next-case-model"]);
    assert!(!current.unresolved);
    let parent = tracker.physical_scope_usage("prx_settlement_turn")?;
    assert!(parent.unresolved);
    assert_eq!((parent.calls, parent.tokens, parent.micro_usd), (2, 1036, 375_000));
    Ok(())
}

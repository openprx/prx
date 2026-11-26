use super::*;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn private_state_dir_non_cost_creator_under_group_umask() {
    const TEST: &str = "cost::tracker::tests::private_state_dir_non_cost_creator_under_group_umask";
    if crate::util::test_helpers::run_in_isolated_process(TEST, "PRX_TEST_PRIVATE_STATE_CHILD") {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o002));
    let temp = TempDir::new().expect("test: temp root");
    let workspace = temp.path().join("fresh-workspace");
    crate::providers::catalog::discovery::save_model_cache_state(
        &workspace,
        &crate::providers::catalog::discovery::ModelCacheState::default(),
    )
    .expect("test: non-cost creator writes model cache first");
    let tracker = CostTracker::new(enabled_config(), &workspace).expect("test: open cost tracker after cache");
    tracker
        .record_usage(TokenUsage::new("prx-model", 100, 50, 1.0, 1.0))
        .expect("test: record real spend");
    assert!(
        tracker.get_summary().expect("test: summary").session_cost_usd > 0.0,
        "recorded spend must appear in session summary"
    );
    for (path, mode) in [(workspace.join("state"), 0o700), (workspace, 0o775)] {
        assert_eq!(
            fs::metadata(&path)
                .expect("test: directory metadata")
                .permissions()
                .mode()
                & 0o777,
            mode,
            "creation mode for {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn private_state_dir_existing_writable_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = TempDir::new().expect("test: workspace");
    let state = temp.path().join("state");
    fs::create_dir(&state).expect("test: existing state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o775)).expect("test: shared state mode");
    crate::util::create_private_dir_all(&state).expect("test: helper leaves existing leaf alone");
    let error = match CostTracker::new(enabled_config(), temp.path()) {
        Ok(_) => panic!("test: writable state must be refused"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains(&state.display().to_string()) && message.contains("chmod 700") && message.contains("0o775"),
        "refusal must include path, mode and manual remediation: {message}"
    );
    assert_eq!(
        fs::metadata(state)
            .expect("test: unchanged state metadata")
            .permissions()
            .mode()
            & 0o777,
        0o775,
        "existing state must never be narrowed automatically"
    );
}

fn enabled_config() -> CostConfig {
    CostConfig::default()
}

fn reservation(attempt: &str, maximum: u64, turn_limit: u64) -> super::super::reservation::ReservationRequest {
    super::super::reservation::ReservationRequest {
        turn_id: "prx-turn".into(),
        attempt_id: attempt.into(),
        binding_sha256: "a".repeat(64),
        maximum_micro_usd: maximum,
        turn_limit_micro_usd: turn_limit,
        security_daily_micro_usd: 1_000_000,
    }
}

fn estimated(
    attempt: &str,
    money: u64,
    tokens: u64,
    token_limit: u64,
) -> super::super::reservation::EstimatedReservationRequest {
    super::super::reservation::EstimatedReservationRequest {
        reservation: reservation(attempt, money, 1_000_000),
        reserved_tokens: tokens,
        turn_token_limit: token_limit,
        group: None,
    }
}

#[test]
fn estimated_attempt_overrun_records_actual_without_legacy_poison() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let receipt = tracker
        .reserve_estimated_attempt(estimated("prx-estimated", 1, 10, 30))
        .unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    assert!(tracker.mark_attempt_dispatched(&receipt).is_err());
    let record = metered_record("prx-estimated", Some(0.25));
    assert!(tracker.reconcile_estimated_attempt(&receipt, &record).is_err());
    tracker.settle_metered(&record).unwrap();
    tracker.reconcile_estimated_attempt(&receipt, &record).unwrap();
    tracker.reconcile_estimated_attempt(&receipt, &record).unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
    // Actual 15 tokens replaces estimate 10, rather than clamping it.
    assert!(
        tracker
            .reserve_estimated_attempt(estimated("prx-over-tokens", 1, 16, 30))
            .is_err()
    );
    let next = tracker
        .reserve_estimated_attempt(estimated("prx-next-estimate", 1, 15, 30))
        .unwrap();
    tracker.release_unsent_attempt(&next).unwrap();
    let reopened = CostTracker::new(config, tmp.path()).unwrap();
    assert!(
        reopened
            .reserve_estimated_attempt(estimated("prx-after-restart", 1, 15, 30))
            .is_ok()
    );
}

#[test]
fn estimated_unknown_drop_retains_tokens_and_money_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let receipt = tracker
        .reserve_estimated_attempt(estimated("prx-unknown", 250_000, 30, 30))
        .unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    assert!(tracker.release_unsent_attempt(&receipt).is_err());
    drop(receipt);
    drop(tracker);
    let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
    assert!(
        reopened
            .reserve_estimated_attempt(estimated("prx-no-token-reset", 1, 1, 30))
            .is_err()
    );
}

#[test]
fn estimated_dispatch_batches_existing_journal_rows_before_authority_returns() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let request = estimated("prx-batched-dispatch", 250_000, 30, 30);
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let receipt = tracker
        .reserve_dispatched_estimated_attempt(request.clone(), || true)
        .unwrap();
    assert!(tracker.mark_attempt_dispatched(&receipt).is_err());
    assert!(tracker.release_unsent_attempt(&receipt).is_err());
    let rows: Vec<serde_json::Value> =
        std::fs::read_to_string(tmp.path().join("state").join(super::super::reservation::JOURNAL))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.first()
            .and_then(|row| row.get("event"))
            .and_then(serde_json::Value::as_str),
        Some("estimated_reserved_v1")
    );
    assert_eq!(
        rows.get(1)
            .and_then(|row| row.get("event"))
            .and_then(serde_json::Value::as_str),
        Some("dispatched")
    );
    let reserved_attempt_id = rows
        .first()
        .and_then(|row| row.get("request"))
        .and_then(|request| request.get("reservation"))
        .and_then(|reservation| reservation.get("attempt_id"));
    let dispatched_attempt_id = rows.get(1).and_then(|row| row.get("attempt_id"));
    assert!(reserved_attempt_id.is_some());
    assert_eq!(reserved_attempt_id, dispatched_attempt_id);

    drop(receipt);
    drop(tracker);
    let reopened = CostTracker::new(config, tmp.path()).unwrap();
    assert!(reopened.reserve_dispatched_estimated_attempt(request, || true).is_err());
}

#[test]
fn expired_reservation_wait_never_marks_attempt_dispatched() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let tracker = Arc::new(CostTracker::new(config, tmp.path()).unwrap());
    let lock_path = tmp.path().join("state/costs.jsonl.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(40);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let blocked = {
        let tracker = Arc::clone(&tracker);
        std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            tracker
                .reserve_dispatched_estimated_attempt(estimated("prx-expired-before-dispatch", 250_000, 30, 30), || {
                    std::time::Instant::now() < deadline
                })
        })
    };
    entered_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(80));
    lock.unlock().unwrap();

    let error = blocked.join().unwrap().expect_err("expired attempt must not dispatch");
    assert!(error.to_string().contains("deadline elapsed"), "{error:#}");
    let journal = tmp.path().join("state").join(super::super::reservation::JOURNAL);
    assert!(
        !journal.exists() || std::fs::read_to_string(journal).unwrap().is_empty(),
        "an expired wait must not leave a reservation or dispatched hold"
    );
}

#[test]
fn reservation_reconciliation_requires_cost_first_and_preserves_turn_spend() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let request = reservation("prx-attempt-settle", 750_000, 1_000_000);
    let receipt = tracker.reserve_attempt(request.clone()).unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    let record = metered_record("prx-attempt-settle", Some(0.25));
    assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
    tracker.settle_metered(&record).unwrap();
    // Simulate interruption after durable projection but before hold release.
    drop(tracker);
    drop(receipt);
    let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 0);
    let recovered = reopened.recover_dispatched_attempt(request).unwrap();
    assert!(reopened.mark_attempt_dispatched(&recovered).is_err());
    reopened.reconcile_reserved_attempt(&recovered, &record).unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 750_000);
    let journal = tmp.path().join("state/costs.reservations.jsonl");
    let before = fs::read(&journal).unwrap();
    reopened.reconcile_reserved_attempt(&recovered, &record).unwrap();
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert!(
        reopened
            .reserve_attempt(reservation("prx-too-large", 800_000, 1_000_000))
            .is_err()
    );
    assert!(
        reopened
            .reserve_attempt(reservation("prx-remaining", 750_000, 1_000_000))
            .is_ok()
    );
}

#[test]
fn over_bound_settlement_is_durable_and_blocks_further_admission() {
    let tmp = TempDir::new().unwrap();
    let config = enabled_config();
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let request = reservation("prx-over-bound", 250_000, 1_000_000);
    let receipt = tracker.reserve_attempt(request.clone()).unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    let record = metered_record("prx-over-bound", Some(0.5));
    tracker.settle_metered(&record).unwrap();
    // Cost projection alone must close the crash window before the
    // explicit terminal reservation transition can be appended.
    assert!(tracker.check_budget(0.0).is_err());
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    let before_reconciliation = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert!(
        before_reconciliation
            .reserve_attempt(reservation("prx-crash-window", 1, 1_000_000))
            .is_err()
    );
    assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
    drop(tracker);
    let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, 0.5);
    assert!(reopened.check_budget(0.0).is_err());
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    assert!(reopened.reserve_attempt(reservation("prx-next", 1, 1_000_000)).is_err());
    let recovered = reopened.recover_dispatched_attempt(request).unwrap();
    assert!(reopened.reconcile_reserved_attempt(&recovered, &record).is_err());
    assert!(reopened.release_unsent_attempt(&recovered).is_err());
}

#[test]
fn over_bound_settlement_above_u64_micro_usd_reopens_without_losing_the_charge() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    let request = reservation("prx-large-over-bound", 1, 1);
    let receipt = tracker.reserve_attempt(request.clone()).unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    let actual = 2_f64.powi(45);
    assert!(ceil_micro_usd(actual).unwrap() > u128::from(u64::MAX));
    let record = metered_record("prx-large-over-bound", Some(actual));
    tracker.settle_metered(&record).unwrap();
    assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
    let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    let recovered = reopened.recover_dispatched_attempt(request).unwrap();
    assert!(reopened.reconcile_reserved_attempt(&recovered, &record).is_err());
    assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, actual);
    assert!(reopened.check_budget(0.0).is_err());
}

#[test]
fn reconciled_hold_rejects_missing_or_conflicting_cost_projection() {
    let tmp = TempDir::new().unwrap();
    let config = enabled_config();
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let request = reservation("prx-projection", 250_000, 1_000_000);
    let receipt = tracker.reserve_attempt(request).unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    let record = metered_record("prx-projection", Some(0.125));
    tracker.settle_metered(&record).unwrap();
    tracker.reconcile_reserved_attempt(&receipt, &record).unwrap();
    let mut conflict = record;
    conflict.prompt_tokens += 1;
    assert!(tracker.reconcile_reserved_attempt(&receipt, &conflict).is_err());
    let path = tmp.path().join("state/costs.jsonl");
    // A damaged/restored ledger must not turn terminal journal state into free capacity.
    fs::write(&path, b"").unwrap();
    assert!(tracker.check_budget(0.0).is_err());
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
}

#[test]
fn reservation_recovery_binds_full_cost_row_despite_unchanged_settlement_digest() {
    let tmp = TempDir::new().unwrap();
    let config = enabled_config();
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let request = reservation("prx-full-projection", 250_000, 1_000_000);
    let receipt = tracker.reserve_attempt(request.clone()).unwrap();
    tracker.mark_attempt_dispatched(&receipt).unwrap();
    let record = metered_record("prx-full-projection", Some(0.125));
    tracker.settle_metered(&record).unwrap();
    tracker.reconcile_reserved_attempt(&receipt, &record).unwrap();
    let path = tmp.path().join("state/costs.jsonl");
    let journal = tmp.path().join("state/costs.reservations.jsonl");
    let original = fs::read(&path).unwrap();
    let journal_before = fs::read(&journal).unwrap();
    let stored: CostRecord = serde_json::from_slice(&original).unwrap();
    let mut altered = Vec::new();
    let mut row = stored.clone();
    row.usage.model = "prx/other-model".into();
    altered.push(row);
    let mut row = stored.clone();
    row.usage.input_tokens += 1;
    altered.push(row);
    let mut row = stored.clone();
    row.usage.timestamp -= chrono::Duration::days(40);
    altered.push(row);
    let mut row = stored.clone();
    row.session_id = "prx-other-session".into();
    altered.push(row);
    let mut row = stored.clone();
    row.id = "prx-other-record".into();
    altered.push(row);
    for row in altered {
        assert_eq!(row.settlement_sha256, stored.settlement_sha256);
        assert_eq!(row.usage.cost_usd, stored.usage.cost_usd);
        fs::write(&path, format!("{}\n", serde_json::to_string(&row).unwrap())).unwrap();
        assert!(tracker.check_budget(0.0).is_err());
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
        assert!(tracker.recover_dispatched_attempt(request.clone()).is_err());
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
    }
    fs::write(path, original).unwrap();
    assert!(tracker.recover_dispatched_attempt(request).is_ok());
    assert!(tracker.check_budget(0.0).is_ok());
}

#[test]
fn unpriced_and_unsent_attempts_cannot_be_reconciled() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    let request = reservation("prx-unsent", 250_000, 1_000_000);
    let receipt = tracker.reserve_attempt(request.clone()).unwrap();
    assert!(tracker.recover_dispatched_attempt(request).is_err());
    let unknown = metered_record("prx-unsent", None);
    assert!(tracker.reconcile_reserved_attempt(&receipt, &unknown).is_err());
    let record = metered_record("prx-unsent", Some(0.125));
    tracker.settle_metered(&record).unwrap();
    assert!(tracker.reconcile_reserved_attempt(&receipt, &record).is_err());
    tracker.release_unsent_attempt(&receipt).unwrap();
}

#[test]
fn reserved_budget_survives_reopen_and_sent_liability_cannot_be_released() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let first = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let receipt = first
        .reserve_attempt(reservation("prx-attempt-a", 750_000, 1_000_000))
        .unwrap();
    let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert!(
        reopened
            .reserve_attempt(reservation("prx-attempt-b", 500_000, 1_000_000))
            .is_err()
    );
    assert!(matches!(
        reopened.check_budget(0.5).unwrap(),
        BudgetCheck::Exceeded { .. }
    ));
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 250_000);
    reopened.mark_attempt_dispatched(&receipt).unwrap();
    assert!(first.mark_attempt_dispatched(&receipt).is_err());
    assert!(first.release_unsent_attempt(&receipt).is_err());
    drop(receipt);
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 250_000);
}

#[test]
fn only_unsent_reservations_release_and_receipts_do_not_cross_workspaces() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let first = CostTracker::new(enabled_config(), a.path()).unwrap();
    let other = CostTracker::new(enabled_config(), b.path()).unwrap();
    let request = reservation("prx-attempt", 100, 200);
    let receipt = first.reserve_attempt(request.clone()).unwrap();
    let other_receipt = other.reserve_attempt(request.clone()).unwrap();
    assert!(other.mark_attempt_dispatched(&receipt).is_err());
    first.release_unsent_attempt(&receipt).unwrap();
    assert!(first.mark_attempt_dispatched(&receipt).is_err());
    assert!(first.reserve_attempt(request).is_err());
    other.release_unsent_attempt(&other_receipt).unwrap();
    assert!(first.reserve_attempt(reservation("prx-new-attempt", 200, 200)).is_ok());
    assert!(first.reserve_attempt(reservation("prx-expanded-turn", 1, 300)).is_err());
}

#[test]
fn competing_trackers_cannot_reserve_the_same_remaining_balance() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let first = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let second = CostTracker::new(config, tmp.path()).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            first.reserve_attempt(reservation("prx-a", 750_000, 1_000_000))
        });
        let b = scope.spawn(|| {
            barrier.wait();
            second.reserve_attempt(reservation("prx-b", 750_000, 1_000_000))
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.is_ok(), b.is_ok());
}

#[test]
fn incomplete_reservation_tail_blocks_admission_without_repair() {
    let tmp = TempDir::new().unwrap();
    let config = enabled_config();
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    tracker.reserve_attempt(reservation("prx-a", 10, 20)).unwrap();
    let path = tmp.path().join("state/costs.reservations.jsonl");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"event\":").unwrap();
    file.sync_all().unwrap();
    let original = fs::read(&path).unwrap();
    assert!(tracker.check_budget(0.0).is_err());
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    assert!(tracker.reserve_attempt(reservation("prx-b", 10, 20)).is_err());
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn budget_rounds_each_charge_up_and_survives_reopen() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(2_f64.powi(-19)),
        monthly_limit_usd: Some(1.0),
        ..enabled_config()
    };
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    for _ in 0..2 {
        let mut usage = TokenUsage::new("prx/model", 1, 0, 0.0, 0.0);
        usage.cost_usd = 2_f64.powi(-22);
        tracker.record_usage(usage).unwrap();
    }
    // Each sub-micro charge consumes one micro-USD. The binary-exact
    // daily limit allows only one, though the floating sum is below it.
    assert!(matches!(
        tracker.check_budget(0.0).unwrap(),
        BudgetCheck::Exceeded {
            period: UsagePeriod::Day,
            ..
        }
    ));
    let reopened = CostTracker::new(config.clone(), tmp.path()).unwrap();
    assert!(matches!(
        reopened.check_budget(0.0).unwrap(),
        BudgetCheck::Exceeded {
            period: UsagePeriod::Day,
            ..
        }
    ));
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 0);
}

#[test]
fn budget_does_not_lose_small_charge_at_large_limit() {
    let limit = 2_f64.powi(53);
    let config = CostConfig {
        daily_limit_usd: Some(limit),
        monthly_limit_usd: Some(limit),
        ..enabled_config()
    };
    let total = ceil_micro_usd(limit).unwrap();
    assert_eq!(limit + 2_f64.powi(-22), limit);
    assert!(matches!(
        budget_check(&config, limit, limit, total, total, 2_f64.powi(-22)).unwrap(),
        BudgetCheck::Exceeded {
            period: UsagePeriod::Day,
            ..
        }
    ));
}

#[test]
fn invalid_limits_refuse_admission_but_preserve_incurred_settlement() {
    for invalid in [f64::NAN, f64::INFINITY, -1.0] {
        for daily in [true, false] {
            let tmp = TempDir::new().unwrap();
            let mut config = enabled_config();
            if daily {
                config.daily_limit_usd = Some(invalid);
            } else {
                config.monthly_limit_usd = Some(invalid);
            }
            let tracker = CostTracker::new(config, tmp.path()).unwrap();
            assert!(tracker.check_budget(0.0).is_err());
            let record = metered_record("prx-invalid-limit-settlement", Some(0.125));
            assert!(tracker.settle_metered(&record).is_err());
            let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
            assert_eq!(reopened.get_summary().unwrap().daily_cost_usd, 0.125);
            assert!(matches!(
                reopened.settle_metered(&record).unwrap(),
                CostSettlement::Replayed
            ));
        }
    }
}

#[test]
fn interrupted_cost_ledger_tail_blocks_reopen_and_append_without_repairing_evidence() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig::default();
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let usage = TokenUsage::new("test/model", 1, 1, 1.0, 1.0);
    tracker.record_usage(usage.clone()).unwrap();
    let path = tmp.path().join("state/costs.jsonl");
    let committed = fs::read(&path).unwrap();
    let encoded = serde_json::to_vec(&CostRecord::new("prx_session", usage.clone())).unwrap();
    for tail in [encoded.as_slice(), b"{\"usage\":".as_slice(), b" ".as_slice()] {
        let mut interrupted = committed.clone();
        interrupted.extend_from_slice(tail);
        fs::write(&path, &interrupted).unwrap();
        assert!(CostTracker::new(config.clone(), tmp.path()).is_err());
        assert!(tracker.check_budget(0.0).is_err());
        assert!(tracker.record_usage(usage.clone()).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            interrupted,
            "never truncate or repair a possibly charged tail"
        );
    }
    // A newline is the commit delimiter, including for a blank legacy row.
    let mut complete = committed;
    complete.extend_from_slice(&encoded);
    complete.extend_from_slice(b"\n \n");
    fs::write(&path, complete).unwrap();
    let reopened = CostTracker::new(config, tmp.path()).unwrap();
    reopened.record_usage(usage).unwrap();
    assert_eq!(
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        3
    );
}

#[cfg(unix)]
#[test]
fn read_only_budget_snapshot_bounds_spend_without_creating_files() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(2.0),
        monthly_limit_usd: Some(3.0),
        ..Default::default()
    };
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 2_000_000);
    assert!(!tmp.path().join("state").exists());
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    tracker
        .record_usage(TokenUsage::new("test/model", 1_000_000, 0, 1.0, 0.0))
        .unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 1_000_000);
}

#[cfg(unix)]
#[test]
fn read_only_security_daily_ceiling_subtracts_prior_spend_and_rejects_bad_ledger() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(100.0),
        monthly_limit_usd: Some(100.0),
        ..Default::default()
    };
    assert_eq!(
        remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).unwrap(),
        1_000_000
    );
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    tracker
        .record_usage(TokenUsage::new("test/model", 1_000_000, 0, 0.875, 0.0))
        .unwrap();
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 99_125_000);
    assert_eq!(
        remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).unwrap(),
        125_000,
        "the security daily limit must account for today's prior spend"
    );
    assert_eq!(
        remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 50).unwrap(),
        0
    );
    fs::write(tmp.path().join("state/costs.jsonl"), b"malformed\n").unwrap();
    assert!(remaining_micro_usd_read_only_with_security_ceiling(&config, tmp.path(), 100).is_err());
}

#[cfg(unix)]
#[test]
fn read_only_budget_snapshot_rejects_unsafe_file_types_and_links() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let config = CostConfig::default();
    let state = tmp.path().join("state");
    symlink(outside.path(), &state).unwrap();
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    fs::remove_file(&state).unwrap();
    fs::create_dir(&state).unwrap();
    let ledger = state.join("costs.jsonl");
    let outside_file = outside.path().join("outside.jsonl");
    fs::write(&outside_file, "").unwrap();
    symlink(&outside_file, &ledger).unwrap();
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    fs::remove_file(&ledger).unwrap();
    fs::hard_link(&outside_file, &ledger).unwrap();
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    fs::remove_file(&ledger).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&ledger)
            .status()
            .unwrap()
            .success()
    );
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn read_only_budget_snapshot_rejects_unknown_or_incomplete_spend() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig::default();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    let ledger = state.join("costs.jsonl");
    let oversized = "x".repeat(MAX_READ_ONLY_BUDGET_BYTES as usize + 1);
    for content in ["not json\n", "{}", oversized.as_str()] {
        fs::write(&ledger, content).unwrap();
        assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
    }
    fs::remove_file(&ledger).unwrap();
    fs::create_dir(tmp.path().join(".openprx")).unwrap();
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_ok());
    fs::write(tmp.path().join(".openprx/costs.db"), "legacy").unwrap();
    assert!(remaining_micro_usd_read_only(&config, tmp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn read_only_budget_snapshot_rounds_each_charge_up_and_limits_down() {
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    let mut ledger = File::create(state.join("costs.jsonl")).unwrap();
    for cost in [10_000_000_000.0, 0.000_000_1, 0.000_000_1] {
        let mut usage = TokenUsage::new("test/model", 0, 0, 0.0, 0.0);
        usage.cost_usd = cost;
        writeln!(
            ledger,
            "{}",
            serde_json::to_string(&CostRecord::new("prx_session", usage)).unwrap()
        )
        .unwrap();
    }
    ledger.sync_all().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(10_000_000_001.0),
        monthly_limit_usd: Some(10_000_000_000.5),
        ..Default::default()
    };
    assert_eq!(remaining_micro_usd_read_only(&config, tmp.path()).unwrap(), 499_998);
}

#[test]
fn read_only_budget_micro_conversions_use_exact_binary64_value() {
    assert_eq!(ceil_micro_usd(3e-6).unwrap(), 4);
    assert_eq!(floor_micro_usd(1e-6).unwrap(), 0);
    assert_eq!(ceil_micro_usd(f64::from_bits(1)).unwrap(), 1);
    assert_eq!(floor_micro_usd(f64::from_bits(1)).unwrap(), 0);
    assert_eq!(ceil_micro_usd(0.5).unwrap(), 500_000);
    assert_eq!(floor_micro_usd(0.5).unwrap(), 500_000);
    assert!(ceil_micro_usd(f64::MAX).is_err());
}

#[test]
fn cost_tracker_initialization() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    assert!(!tracker.session_id().is_empty(), "expected a non-empty value");
}

#[test]
fn budget_check_is_always_enforced() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig::default();

    let tracker = CostTracker::new(config, tmp.path()).unwrap();
    let check = tracker.check_budget(1000.0).unwrap();
    assert!(matches!(check, BudgetCheck::Exceeded { .. }));
}

#[test]
fn record_usage_and_get_summary() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

    let usage = TokenUsage::new("test/model", 1000, 500, 1.0, 2.0);
    tracker.record_usage(usage).unwrap();

    let summary = tracker.get_summary().unwrap();
    assert_eq!(summary.request_count, 1);
    assert!(summary.session_cost_usd > 0.0);
    assert_eq!(summary.by_model.len(), 1);
}

#[test]
fn budget_exceeded_daily_limit() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(0.01), // Very low limit
        ..Default::default()
    };

    let tracker = CostTracker::new(config, tmp.path()).unwrap();

    // Record a usage that exceeds the limit
    let usage = TokenUsage::new("test/model", 10000, 5000, 1.0, 2.0); // ~0.02 USD
    tracker.record_usage(usage).unwrap();

    let check = tracker.check_budget(0.01).unwrap();
    assert!(matches!(check, BudgetCheck::Exceeded { .. }));
}

#[test]
fn summary_by_model_is_session_scoped() {
    let tmp = TempDir::new().unwrap();
    let storage_path = resolve_storage_path(tmp.path()).unwrap();
    if let Some(parent) = storage_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    let old_record = CostRecord::new("old-session", TokenUsage::new("legacy/model", 500, 500, 1.0, 1.0));
    let mut file = OpenOptions::new().create(true).append(true).open(storage_path).unwrap();
    writeln!(file, "{}", serde_json::to_string(&old_record).unwrap()).unwrap();
    file.sync_all().unwrap();

    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    tracker
        .record_usage(TokenUsage::new("session/model", 1000, 1000, 1.0, 1.0))
        .unwrap();

    let summary = tracker.get_summary().unwrap();
    assert_eq!(summary.by_model.len(), 1);
    assert!(summary.by_model.contains_key("session/model"));
    assert!(!summary.by_model.contains_key("legacy/model"));
}

#[test]
fn append_after_period_rollover_counts_new_record_once() {
    let tmp = TempDir::new().unwrap();
    // CostStorage accepts a physical path; the public tracker resolves aliases.
    let path = tmp.path().canonicalize().unwrap().join("costs.jsonl");
    let mut storage = CostStorage::new(&path).unwrap();
    let mut usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
    usage.cost_usd = 0.25;
    storage
        .add_record(CostRecord::new("prx-session", usage.clone()))
        .unwrap();
    // Force a stale period without waiting for a wall-clock boundary.
    storage.cached_day = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    storage.cached_year = 2000;
    storage.cached_month = 1;
    storage.add_record(CostRecord::new("prx-session", usage)).unwrap();
    let cached = (storage.daily_cost_usd.to_bits(), storage.monthly_cost_usd.to_bits());
    storage
        .rebuild_aggregates(storage.cached_day, storage.cached_year, storage.cached_month)
        .unwrap();
    assert_eq!(
        cached,
        (storage.daily_cost_usd.to_bits(), storage.monthly_cost_usd.to_bits())
    );
    assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 2);
}

#[test]
fn malformed_ledger_rejects_reopen_and_existing_tracker_without_writing() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    let usage = TokenUsage::new("test/model", 1000, 0, 1.0, 1.0);
    tracker.record_usage(usage.clone()).unwrap();
    let storage_path = resolve_storage_path(tmp.path()).unwrap();
    let mut file = OpenOptions::new().append(true).open(&storage_path).unwrap();
    writeln!(file, "not-a-json-line").unwrap();
    file.sync_all().unwrap();
    let before = fs::read(&storage_path).unwrap();

    assert!(CostTracker::new(enabled_config(), tmp.path()).is_err());
    assert!(tracker.check_budget(0.0).is_err());
    assert!(tracker.record_usage(usage).is_err());
    assert!(
        tracker
            .settle_metered(&metered_record("prx-settlement", Some(0.001)))
            .is_err()
    );
    assert!(tracker.get_summary().is_err());
    assert_eq!(fs::read(storage_path).unwrap(), before);
}

#[test]
fn invalid_persisted_amount_and_aggregate_overflow_reject_budget_refresh() {
    for amounts in [vec![-1.0], vec![f64::MAX, f64::MAX]] {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&storage_path)
            .unwrap();
        for amount in amounts {
            let mut usage = TokenUsage::new("test/model", 1, 0, 1.0, 1.0);
            usage.cost_usd = amount;
            let record = CostRecord::new("prx-session", usage);
            writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
        }
        file.sync_all().unwrap();
        let before = fs::read(&storage_path).unwrap();
        assert!(tracker.check_budget(0.0).is_err());
        assert!(CostTracker::new(enabled_config(), tmp.path()).is_err());
        assert_eq!(fs::read(storage_path).unwrap(), before);
    }
}

#[test]
fn invalid_budget_estimate_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

    let err = tracker.check_budget(f64::NAN).unwrap_err();
    assert!(
        err.to_string()
            .contains("Estimated cost must be a finite, non-negative value")
    );
}

fn metered_record(settlement_id: &str, cost_usd: Option<f64>) -> crate::llm::route_decision::MeteredTokenUsageRecord {
    crate::llm::route_decision::MeteredTokenUsageRecord {
        root_agent: None,
        settlement_id: Some(settlement_id.to_string()),
        provider: "provider-a".to_string(),
        model: "model-a".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: crate::llm::route_decision::TokenUsageSource::Reported,
        cost_usd,
        settlement_class: None,
    }
}

#[test]
fn older_configuration_lookup_cannot_restore_a_new_views_limits() {
    let tmp = TempDir::new().unwrap();
    let older_config = CostConfig {
        daily_limit_usd: Some(10.0),
        monthly_limit_usd: Some(20.0),
        ..enabled_config()
    };
    let newer_config = CostConfig {
        daily_limit_usd: Some(0.001),
        ..older_config.clone()
    };
    let older = CostTracker::for_workspace(older_config.clone(), tmp.path()).unwrap();
    let newer = CostTracker::for_workspace(newer_config.clone(), tmp.path()).unwrap();
    let old_turn = CostTracker::for_workspace(older_config, tmp.path()).unwrap();
    assert!(Arc::ptr_eq(&older.storage, &newer.storage));
    assert!(Arc::ptr_eq(&newer.storage, &old_turn.storage));
    assert!(
        newer.config == newer_config,
        "new caller retains its exact configuration"
    );
    assert!(
        newer
            .reserve_attempt(reservation("prx-new-limits", 2000, 10_000))
            .is_err()
    );
    assert!(matches!(
        newer.check_budget(0.002).unwrap(),
        BudgetCheck::Exceeded { .. }
    ));
    assert!(!matches!(
        old_turn.check_budget(0.002).unwrap(),
        BudgetCheck::Exceeded { .. }
    ));
    older
        .settle_metered(&metered_record("prx-shared-configuration", Some(0.002)))
        .unwrap();
    assert_eq!(newer.get_summary().unwrap().request_count, 1);
    assert_eq!(old_turn.get_summary().unwrap().request_count, 1);
}

#[test]
fn canonical_settlement_is_process_owned_idempotent_and_budgeted() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(0.001),
        monthly_limit_usd: Some(1.0),
        ..Default::default()
    };
    let first = CostTracker::for_workspace(config.clone(), tmp.path()).unwrap();
    let second = CostTracker::for_workspace(config, tmp.path()).unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    let record = metered_record("turn-1", Some(0.002));
    let settled = first.settle_metered(&record).unwrap();
    assert!(matches!(
        settled,
        CostSettlement::Recorded {
            budget: BudgetCheck::Exceeded { .. }
        }
    ));
    assert_eq!(second.settle_metered(&record).unwrap(), CostSettlement::Replayed);
    assert_eq!(first.get_summary().unwrap().request_count, 1);
}

#[test]
fn reopened_settlement_rejects_changed_charge_or_usage_without_writing() {
    let tmp = TempDir::new().unwrap();
    let first = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    let record = metered_record("prx-settlement", Some(0.002));
    first.settle_metered(&record).unwrap();
    let path = first.lock_storage().path.clone();
    let before = fs::read(&path).unwrap();
    drop(first);

    let reopened = CostTracker::new(enabled_config(), tmp.path()).unwrap();
    assert_eq!(reopened.settle_metered(&record).unwrap(), CostSettlement::Replayed);
    let mut changed_cost = record.clone();
    changed_cost.cost_usd = Some(0.004);
    let mut changed_model = record.clone();
    changed_model.model = "prx-other-model".to_owned();
    let mut changed_cache = record;
    changed_cache.cache_read_input_tokens = 1;
    for changed in [changed_cost, changed_model, changed_cache] {
        assert!(
            reopened
                .settle_metered(&changed)
                .unwrap_err()
                .to_string()
                .contains("conflicts")
        );
    }
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn separate_tracker_instances_serialize_the_same_settlement() {
    let tmp = TempDir::new().unwrap();
    let first = Arc::new(CostTracker::new(enabled_config(), tmp.path()).unwrap());
    let second = Arc::new(CostTracker::new(enabled_config(), tmp.path()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let first_result = {
        let tracker = Arc::clone(&first);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            tracker.settle_metered(&metered_record("shared-settlement", Some(0.002)))
        })
    };
    let second_result = {
        let tracker = Arc::clone(&second);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            tracker.settle_metered(&metered_record("shared-settlement", Some(0.002)))
        })
    };

    let results = [
        first_result.join().unwrap().unwrap(),
        second_result.join().unwrap().unwrap(),
    ];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, CostSettlement::Recorded { .. }))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, CostSettlement::Replayed))
            .count(),
        1
    );
    let persisted = std::fs::read_to_string(tmp.path().join("state").join("costs.jsonl")).unwrap();
    assert_eq!(persisted.lines().filter(|line| !line.trim().is_empty()).count(), 1);
}

#[test]
fn separate_tracker_instances_refresh_budget_and_summary_from_disk() {
    let tmp = TempDir::new().unwrap();
    let config = CostConfig {
        daily_limit_usd: Some(1.0),
        monthly_limit_usd: Some(10.0),
        ..Default::default()
    };
    let writer = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let reader = CostTracker::new(config, tmp.path()).unwrap();

    assert!(matches!(
        writer
            .settle_metered(&metered_record("external-settlement", Some(0.9)))
            .unwrap(),
        CostSettlement::Recorded { .. }
    ));
    assert!(matches!(
        reader.check_budget(0.2).unwrap(),
        BudgetCheck::Exceeded {
            period: UsagePeriod::Day,
            ..
        }
    ));
    let summary = reader.get_summary().unwrap();
    assert!((summary.daily_cost_usd - 0.9).abs() < f64::EPSILON);
    assert!((summary.monthly_cost_usd - 0.9).abs() < f64::EPSILON);
}

#[test]
fn unknown_price_is_not_recorded_as_zero_cost() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::for_workspace(enabled_config(), tmp.path()).unwrap();
    let settlement = tracker.settle_metered(&metered_record("turn-unknown", None)).unwrap();
    assert_eq!(settlement, CostSettlement::UnknownPricing);
    assert_eq!(tracker.get_summary().unwrap().request_count, 0);
}

#[test]
fn subscription_settlement_accepts_legacy_status_only() {
    let settlement: CostSettlement = serde_json::from_str(r#"{"status":"subscription"}"#).unwrap();
    assert_eq!(
        settlement,
        CostSettlement::Subscription {
            provider: String::new(),
            model: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        }
    );
}

#[test]
fn subscription_usage_is_recorded_as_one_zero_cost_subscription_row() {
    let tmp = TempDir::new().unwrap();
    let tracker = CostTracker::for_workspace(enabled_config(), tmp.path()).unwrap();
    let mut record = metered_record("turn-subscription", None);
    record.provider = "kimi-code".to_string();
    record.model = "kimi-for-coding".to_string();
    let settlement = tracker.settle_metered(&record).unwrap();
    assert_eq!(
        settlement,
        CostSettlement::Subscription {
            provider: "kimi-code".to_owned(),
            model: "kimi-for-coding".to_owned(),
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            total_tokens: record.total_tokens,
        }
    );
    let encoded = serde_json::to_value(&settlement).unwrap();
    assert_eq!(
        encoded.get("status").and_then(serde_json::Value::as_str),
        Some("subscription")
    );
    assert!(encoded.get("cost_usd").is_none(), "{encoded}");
    let summary = tracker.get_summary().unwrap();
    assert_eq!(summary.request_count, 1);
    assert_eq!(summary.session_cost_usd.to_bits(), 0.0_f64.to_bits());
    let persisted = std::fs::read_to_string(tmp.path().join("state").join("costs.jsonl")).unwrap();
    assert_eq!(persisted.lines().count(), 1, "{persisted}");
    assert!(persisted.contains(r#""billing":"subscription""#), "{persisted}");
}

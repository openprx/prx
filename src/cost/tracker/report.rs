//! Bounded, read-only workspace cost reporting for the standalone CLI.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Debug, Default, Serialize)]
pub struct CostTotals {
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub requests: u64,
    pub reported_requests: u64,
}
impl CostTotals {
    fn add(&mut self, row: &super::CostRecord) -> Result<()> {
        self.cost_usd += row.usage.cost_usd;
        ensure!(self.cost_usd.is_finite(), "cost report amount overflows");
        self.input_tokens = self
            .input_tokens
            .checked_add(row.usage.input_tokens)
            .context("input token sum overflows")?;
        self.output_tokens = self
            .output_tokens
            .checked_add(row.usage.output_tokens)
            .context("output token sum overflows")?;
        self.total_tokens = self
            .total_tokens
            .checked_add(row.usage.total_tokens)
            .context("total token sum overflows")?;
        self.requests += 1;
        if row.settlement_source == Some(crate::llm::route_decision::TokenUsageSource::Reported) {
            self.reported_requests += 1;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct WorkspaceCostReport {
    pub as_of_utc: DateTime<Utc>,
    pub today: CostTotals,
    pub month: CostTotals,
    pub all_time: CostTotals,
    /// All-time totals, grouped by redacted model label.
    pub by_model: BTreeMap<String, CostTotals>,
    pub reserved_requests: usize,
    pub unsettled_dispatched_requests: usize,
    /// Includes estimates. This is not an additional settled charge or a vendor billing cap.
    pub outstanding_reservation_micro_usd: u128,
}

/// Reads existing state only. A malformed or unsafe ledger is an error, never
/// an empty report. Concurrent changes can require retrying this advisory read.
pub fn workspace_cost_report(workspace: &Path) -> Result<WorkspaceCostReport> {
    let workspace = workspace.canonicalize().context("cost workspace cannot be resolved")?;
    let ledger = super::read_state_ledger_bounded(&workspace, "costs.jsonl")?;
    ensure!(
        ledger.is_some() || !super::legacy_cost_ledger_present(&workspace)?,
        "legacy cost ledger needs migration before a read-only cost report"
    );
    let reservations = crate::cost::reservation::Reservations::load(&workspace)?;
    let outstanding_reservation_micro_usd = reservations.active_micro_usd()?;
    let (reserved_requests, unsettled_dispatched_requests) = reservations.active_request_counts();
    ensure!(
        ledger == super::read_state_ledger_bounded(&workspace, "costs.jsonl")?,
        "cost ledger changed during the snapshot; retry the report"
    );
    summarize(
        ledger.as_deref().unwrap_or(""),
        Utc::now(),
        reserved_requests,
        unsettled_dispatched_requests,
        outstanding_reservation_micro_usd,
    )
}

fn summarize(
    ledger: &str,
    now: DateTime<Utc>,
    reserved: usize,
    dispatched: usize,
    outstanding: u128,
) -> Result<WorkspaceCostReport> {
    let mut report = WorkspaceCostReport {
        as_of_utc: now,
        today: CostTotals::default(),
        month: CostTotals::default(),
        all_time: CostTotals::default(),
        by_model: BTreeMap::new(),
        reserved_requests: reserved,
        unsettled_dispatched_requests: dispatched,
        outstanding_reservation_micro_usd: outstanding,
    };
    let mut settlements = HashSet::new();
    for (index, line) in ledger.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: super::CostRecord =
            serde_json::from_str(line).with_context(|| format!("cost ledger row {} is malformed", index + 1))?;
        ensure!(
            row.usage.cost_usd.is_finite() && row.usage.cost_usd >= 0.0,
            "cost ledger contains an invalid amount"
        );
        if let Some(id) = &row.settlement_id {
            ensure!(
                settlements.insert(id.clone()),
                "cost ledger contains a duplicate settlement"
            );
        }
        report.all_time.add(&row)?;
        if row.usage.timestamp.date_naive() == now.date_naive() {
            report.today.add(&row)?;
        }
        if (row.usage.timestamp.year(), row.usage.timestamp.month()) == (now.year(), now.month()) {
            report.month.add(&row)?;
        }
        let model = crate::security::redaction::redact_provider_text(&row.usage.model);
        report.by_model.entry(model).or_default().add(&row)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::types::{CostRecord, TokenUsage};

    #[test]
    fn periods_and_reported_usage_are_distinct_from_pending_liability() {
        let now: DateTime<Utc> = "2026-04-15T12:00:00Z".parse().unwrap();
        let mut rows = Vec::new();
        for (date, cost) in [
            ("2026-04-15T01:00:00Z", 1.0),
            ("2026-04-01T01:00:00Z", 2.0),
            ("2026-03-31T01:00:00Z", 4.0),
        ] {
            let mut row = CostRecord::new("scope", TokenUsage::new("model", 10, 5, 0.0, 0.0));
            row.usage.timestamp = date.parse().unwrap();
            row.usage.cost_usd = cost;
            row.settlement_source = Some(crate::llm::route_decision::TokenUsageSource::Reported);
            rows.push(serde_json::to_string(&row).unwrap());
        }
        let report = summarize(&rows.join("\n"), now, 1, 2, 500_000).unwrap();
        assert_eq!(report.today.cost_usd, 1.0);
        assert_eq!(report.month.cost_usd, 3.0);
        assert_eq!(report.all_time.cost_usd, 7.0);
        assert_eq!(report.all_time.total_tokens, 45);
        assert_eq!(report.all_time.reported_requests, 3);
        assert_eq!(report.unsettled_dispatched_requests, 2);
        assert_eq!(report.outstanding_reservation_micro_usd, 500_000);
    }

    #[test]
    fn empty_workspace_is_not_initialized_and_bad_rows_are_not_hidden() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(workspace_cost_report(temp.path()).unwrap().all_time.requests, 0);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        let state = temp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let ledger = state.join("costs.jsonl");
        std::fs::write(&ledger, "invalid\n").unwrap();
        assert!(workspace_cost_report(temp.path()).is_err());
        assert_eq!(std::fs::read_to_string(&ledger).unwrap(), "invalid\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_ledger_is_refused_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let target = temp.path().join("outside");
        std::fs::write(&target, "private").unwrap();
        std::os::unix::fs::symlink(&target, state.join("costs.jsonl")).unwrap();
        assert!(workspace_cost_report(temp.path()).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "private");
    }
    #[test]
    fn zero_priced_dispatched_calls_remain_visible_until_settled() {
        use crate::cost::reservation::{ReservationRequest, Reservations};
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("state")).unwrap();
        let mut journal = Reservations::default();
        let receipt = journal
            .reserve(
                temp.path(),
                ReservationRequest {
                    turn_id: "scope".into(),
                    attempt_id: "attempt".into(),
                    binding_sha256: "a".repeat(64),
                    maximum_micro_usd: 0,
                    turn_limit_micro_usd: 0,
                    security_daily_micro_usd: 0,
                },
                0,
            )
            .unwrap();
        journal.transition(temp.path(), &receipt, true).unwrap();
        let report = workspace_cost_report(temp.path()).unwrap();
        assert_eq!(report.all_time.requests, 0);
        assert_eq!(report.outstanding_reservation_micro_usd, 0);
        assert_eq!(report.unsettled_dispatched_requests, 1);
    }

    #[test]
    fn duplicate_settlements_are_refused_and_model_labels_are_redacted() {
        let now = Utc::now();
        let row = CostRecord::from_settlement(
            "scope",
            "settlement",
            TokenUsage::new(
                "custom:https://user:password-sentinel@example.com/v1/model",
                10,
                5,
                0.0,
                0.0,
            ),
        );
        let line = serde_json::to_string(&row).unwrap();
        assert!(summarize(&format!("{line}\n{line}\n"), now, 0, 0, 0).is_err());
        let report = summarize(&line, now, 0, 0, 0).unwrap();
        assert!(!serde_json::to_string(&report).unwrap().contains("password-sentinel"));
    }
}

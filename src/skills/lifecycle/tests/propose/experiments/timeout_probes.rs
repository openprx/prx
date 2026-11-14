#![cfg(test)]

//! Candidate timeout diagnostics (the decision-store timers went with the store, P2-G5a) do not identify a historical failure without its error detail.
use super::*;
use crate::skills::lifecycle::{CleanupState, RunObservation, RunReceipt, RunTerminal};

#[tokio::test]
async fn candidate_timers_report_their_actual_contended_failure_detail() -> Result<()> {
    let mut receipts = Vec::new();
    for candidate in ["bounded-busy", "cost-ledger"] {
        let started = std::time::Instant::now();
        let detail = match candidate {
            "bounded-busy" => {
                tokio::task::spawn_blocking(crate::runtime::sqlite_pool::probe_bounded_busy_contention).await??
            }
            "cost-ledger" => tokio::task::spawn_blocking(crate::cost::tracker::probe_ledger_lock_contention).await??,
            _ => anyhow::bail!("test: unknown timer candidate"),
        };
        assert!(!detail.is_empty(), "test: candidate must retain its actual error");
        let receipt = RunReceipt::from_observation(&RunObservation {
            run_id: candidate.into(),
            terminal: RunTerminal::Failed,
            response: String::new(),
            elapsed_ms: u64::try_from(started.elapsed().as_millis())?,
            environment_hash: "candidate-probe-only".into(),
            installed: None,
            hydrations: Vec::new(),
            experience_terminal: None,
            artifacts: BTreeMap::new(),
            cleanup: CleanupState::Removed,
            tokens: None,
            cost_micros: None,
            models_used: Vec::new(),
            failure_detail: detail,
        });
        assert_ne!(receipt.failure_detail, "");
        receipts.push(receipt);
    }
    // Kept as a standalone diagnostic artifact, never a claim about the original run.
    let path = std::env::temp_dir().join("candidate-timer-probe-receipts.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&receipts)?)?;
    Ok(())
}

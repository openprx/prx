#![cfg(test)]
//! CF-R8: subscription terminals project one `$0` ledger row through the
//! recovery outbox, over a real SQLite store and a real ledger. Each restart
//! opens a new `SqliteMemory` on the same directory and runs one projector
//! pass.

use super::auth_02_3_tests::{
    SkipInline, assert_subscription_row, first_outbox, home_workspace, ledger_rows, spawn_recovery_child,
    subscription_commit,
};
use crate::agent::terminal::{bind_terminal_fixture, finalize_turn, priced_cost_config};
use crate::config::schema::CostConfig;
use crate::memory::fabric::MemoryFabric;
use crate::memory::sqlite::fixture_readback;
use crate::memory::{Memory, SqliteMemory};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Cell {
    dir: tempfile::TempDir,
    workspace: PathBuf,
    fabric: MemoryFabric,
    commit: crate::agent::terminal::TurnTerminalDraft,
    cost: CostConfig,
    service: Arc<crate::runtime::turn_publication::TurnRuntimeService>,
}

async fn subscription_cell(terminal_id: &str) -> Cell {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test: open recovery database"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = subscription_commit(terminal_id);
    let service = bind_terminal_fixture(&workspace, memory, &mut commit, &cost).await;
    Cell {
        dir,
        workspace,
        fabric,
        commit,
        cost,
        service,
    }
}

impl Cell {
    async fn finalize(&self) {
        finalize_turn(&self.fabric, self.commit.clone(), &self.cost, &self.workspace)
            .await
            .expect("test: subscription terminal commits");
    }

    /// A restart: a new store handle on the same directory and one pass.
    fn restart(&self) {
        let memory = SqliteMemory::new(&self.workspace).expect("test: reopen recovery database");
        crate::runtime::cost_recovery::run_pass(
            &memory,
            &self.workspace.to_string_lossy(),
            &self.workspace,
            &self.cost,
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("test: projector pass after restart");
    }

    async fn close(self) {
        self.service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        drop(self.dir);
    }
}

fn rows_for(workspace: &Path, terminal_id: &str) -> usize {
    ledger_rows(workspace, terminal_id).len()
}

fn assert_settled(workspace: &Path, terminal_id: &str) {
    assert_eq!(fixture_readback::outbox_rows(workspace).len(), 1, "one outbox row");
    assert_eq!(first_outbox(workspace).state, "acknowledged");
    assert_subscription_row(workspace, terminal_id);
}

pub(super) async fn recovery_subscription_commit_survives_restart() {
    {
        let id = "prx-sub-skip-inline";
        let cell = subscription_cell(id).await;
        let skip = SkipInline;
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        cell.finalize().await;
        drop(skip);
        assert_eq!(first_outbox(&cell.workspace).state, "pending");
        assert_eq!(rows_for(&cell.workspace, id), 0);
        cell.restart();
        assert_settled(&cell.workspace, id);
        cell.close().await;
    }
    {
        let id = "prx-sub-crash-after-append";
        let cell = subscription_cell(id).await;
        crate::cost::tracker::crash_after_append(true);
        cell.finalize().await;
        assert_eq!(first_outbox(&cell.workspace).state, "pending");
        assert_eq!(rows_for(&cell.workspace, id), 1);
        cell.restart();
        assert_settled(&cell.workspace, id);
        cell.close().await;
    }
    {
        let id = "prx-sub-crash-before-ack";
        let cell = subscription_cell(id).await;
        crate::agent::terminal::recovery_commit::crash_before_ack(true);
        cell.finalize().await;
        crate::agent::terminal::recovery_commit::crash_before_ack(false);
        assert_eq!(first_outbox(&cell.workspace).state, "pending");
        assert_eq!(rows_for(&cell.workspace, id), 1);
        cell.restart();
        assert_settled(&cell.workspace, id);
        cell.close().await;
    }
    {
        let id = "prx-sub-io-error";
        let cell = subscription_cell(id).await;
        crate::cost::tracker::force_io_error(true);
        cell.finalize().await;
        assert_eq!(first_outbox(&cell.workspace).state, "error");
        assert_eq!(rows_for(&cell.workspace, id), 0);
        cell.restart();
        assert_settled(&cell.workspace, id);
        cell.close().await;
    }
    {
        let id = "prx-sub-twice";
        let cell = subscription_cell(id).await;
        cell.finalize().await;
        assert_settled(&cell.workspace, id);
        // The terminal-commit replay keeps the recorded outbox state.
        cell.finalize().await;
        assert_settled(&cell.workspace, id);
        cell.close().await;
    }
    {
        let id = "prx-sub-conflict";
        let cell = subscription_cell(id).await;
        let tracker =
            crate::cost::tracker::CostTracker::for_workspace(cell.cost.clone(), &cell.workspace).expect("test: ledger");
        let prior = crate::llm::route_decision::MeteredTokenUsageRecord {
            settlement_id: Some(id.to_owned()),
            provider: "kimi-code".to_owned(),
            model: "kimi-for-coding".to_owned(),
            prompt_tokens: 20,
            completion_tokens: 5,
            total_tokens: 25,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source: crate::llm::route_decision::TokenUsageSource::Reported,
            cost_usd: None,
            settlement_class: None,
        };
        tracker.settle_metered(&prior).expect("test: prior subscription row");
        let before = std::fs::read(cell.workspace.join("state/costs.jsonl")).expect("test: ledger bytes");
        cell.finalize().await;
        assert_eq!(first_outbox(&cell.workspace).state, "blocked");
        assert_eq!(
            std::fs::read(cell.workspace.join("state/costs.jsonl")).expect("test: ledger bytes"),
            before,
            "a blocked projection leaves the ledger unchanged"
        );
        cell.restart();
        assert_eq!(
            first_outbox(&cell.workspace).state,
            "blocked",
            "a blocked row is not retried"
        );
        assert_eq!(rows_for(&cell.workspace, id), 1);
        cell.close().await;
    }
}

pub(super) fn recovery_subscription_restart_from_killed_process() {
    let (dir, workspace) = home_workspace();
    let id = "prx-killed-subscription";
    let (mut child, ack) = spawn_recovery_child(&workspace, "commit-subscription");
    let payload = ack.recv_timeout(Duration::from_secs(120));
    child.kill().expect("test: kill committed process");
    child.wait().expect("test: reap killed process");
    let payload: serde_json::Value = serde_json::from_str(&payload.expect("test: child ack")).expect("test: ack json");
    assert_eq!(
        payload.get("outbox_state").and_then(serde_json::Value::as_str),
        Some("pending")
    );
    assert_eq!(rows_for(&workspace, id), 0, "no ledger row before the restart");

    for repair in 0..2 {
        let (mut restarted, ack) = spawn_recovery_child(&workspace, "repair");
        let repaired = ack.recv_timeout(Duration::from_secs(120));
        if repaired.is_err() {
            restarted.kill().expect("test: kill process without acknowledgement");
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = restarted.try_wait().expect("test: poll restarted process") {
                break status;
            }
            assert!(Instant::now() < deadline, "test: restarted process exceeded deadline");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "repair {repair} process failed");
        let repaired: serde_json::Value =
            serde_json::from_str(&repaired.expect("test: repair ack")).expect("test: repair json");
        assert_eq!(
            repaired.get("outbox_state").and_then(serde_json::Value::as_str),
            Some("acknowledged"),
            "repair {repair}"
        );
        assert_eq!(fixture_readback::outbox_rows(&workspace).len(), 1, "repair {repair}");
        assert_subscription_row(&workspace, id);
    }
    drop(dir);
}

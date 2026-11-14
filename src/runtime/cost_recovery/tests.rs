use super::{CostOutboxProjector, run_pass, warn_legacy_files};
use crate::agent::terminal::{bind_terminal_fixture, completed_commit, finalize_turn, priced_cost_config};
use crate::memory::fabric::MemoryFabric;
use crate::memory::recovery_authority::RecoveryCostState;
use crate::memory::sqlite::fixture_readback;
use crate::memory::{Memory, SqliteMemory};
use std::path::Path;
use std::sync::Arc;

struct SkipInline;
impl SkipInline {
    fn on() -> Self {
        crate::agent::terminal::recovery_commit::skip_inline_projection(true);
        Self
    }
}
impl Drop for SkipInline {
    fn drop(&mut self) {
        crate::agent::terminal::recovery_commit::skip_inline_projection(false);
    }
}

fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = crate::runtime::ingress_recovery::tombstone_fixture::home_dir();
    let path = dir.path().canonicalize().expect("test: workspace");
    (dir, path)
}

fn ledger_lines(workspace: &Path, settlement: &str) -> usize {
    std::fs::read_to_string(workspace.join("state").join("costs.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(settlement))
        .count()
}

async fn commit_metered(workspace: &Path, memory: &Arc<dyn Memory>, terminal_id: &str) {
    let fabric = MemoryFabric::new(Arc::clone(memory), workspace.to_string_lossy());
    let cost = priced_cost_config();
    let mut commit = completed_commit(terminal_id);
    let _service = bind_terminal_fixture(workspace, Arc::clone(memory), &mut commit, &cost).await;
    finalize_turn(&fabric, commit, &cost, workspace)
        .await
        .expect("test: metered terminal commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_restart_projects_every_pending_row() {
    let cost = priced_cost_config();

    // Cell 1: 150 pending metered rows, recording disabled, zero turns.
    let (_dir, ws) = workspace();
    {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws).expect("test: memory"));
        let _skip = SkipInline::on();
        for index in 0..150 {
            commit_metered(&ws, &memory, &format!("prx-idle-{index:03}")).await;
        }
    }
    assert_eq!(fixture_readback::outbox_rows(&ws).len(), 150);
    assert!(
        fixture_readback::outbox_rows(&ws)
            .iter()
            .all(|row| row.state == "pending")
    );
    // Restart: a fresh handle; the projector never reads recording settings.
    let reopened: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws).expect("test: reopen"));
    let report = run_pass(reopened.as_ref(), &ws.to_string_lossy(), &ws, &cost).expect("test: pass");
    assert!(report.exhausted);
    assert_eq!(report.progressed, 150);
    for index in 0..150 {
        assert_eq!(
            ledger_lines(&ws, &format!("prx-idle-{index:03}")),
            1,
            "one ledger row per settlement"
        );
    }
    assert!(
        fixture_readback::outbox_rows(&ws)
            .iter()
            .all(|row| row.state == "acknowledged")
    );
    assert!(!ws.join("self/causal_tree").exists());
    assert!(!ws.join("self/child_authority").exists());

    // Cell 2: an `Error` row is acknowledged on the next periodic pass.
    let (_dir2, ws2) = workspace();
    let memory2: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws2).expect("test: memory"));
    {
        let _skip = SkipInline::on();
        commit_metered(&ws2, &memory2, "prx-error-row").await;
    }
    let repository = memory2.recovery_repository().expect("test: repository");
    let row = repository
        .recovery_cost_row(&ws2.to_string_lossy(), "prx-error-row")
        .expect("test: lookup")
        .expect("test: row");
    repository
        .finish_recovery_cost(
            &ws2.to_string_lossy(),
            "prx-error-row",
            &row.usage_digest,
            RecoveryCostState::Error,
        )
        .expect("test: force error");
    assert_eq!(
        fixture_readback::outbox_rows(&ws2)
            .first()
            .map(|row| row.state.as_str())
            .expect("test: outbox row"),
        "error"
    );
    CostOutboxProjector::run_once(Arc::clone(&memory2), ws2.clone(), cost.clone()).await;
    assert_eq!(
        fixture_readback::outbox_rows(&ws2)
            .first()
            .map(|row| row.state.as_str())
            .expect("test: outbox row"),
        "acknowledged"
    );
    assert_eq!(ledger_lines(&ws2, "prx-error-row"), 1);

    // Cell 3: a concurrent projection of the same settlement yields one ledger row.
    let (_dir3, ws3) = workspace();
    let memory3: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws3).expect("test: memory"));
    {
        let _skip = SkipInline::on();
        commit_metered(&ws3, &memory3, "prx-concurrent").await;
    }
    let workspace_id = ws3.to_string_lossy().into_owned();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| run_pass(memory3.as_ref(), &workspace_id, &ws3, &cost));
        let second = scope.spawn(|| run_pass(memory3.as_ref(), &workspace_id, &ws3, &cost));
        first.join().expect("test: first").expect("test: first pass");
        second.join().expect("test: second").expect("test: second pass");
    });
    assert_eq!(ledger_lines(&ws3, "prx-concurrent"), 1);
    assert_eq!(
        fixture_readback::outbox_rows(&ws3)
            .first()
            .map(|row| row.state.as_str())
            .expect("test: outbox row"),
        "acknowledged"
    );

    // Cell 4: 70 invalid-MAC rows sort ahead of one valid row.
    let (_dir4, ws4) = workspace();
    let sqlite = Arc::new(SqliteMemory::new(&ws4).expect("test: memory"));
    let memory4: Arc<dyn Memory> = sqlite.clone();
    {
        let _skip = SkipInline::on();
        commit_metered(&ws4, &memory4, "b000").await;
    }
    let workspace_id = ws4.to_string_lossy().into_owned();
    let valid = memory4
        .recovery_repository()
        .expect("test: repository")
        .recovery_cost_row(&workspace_id, "b000")
        .expect("test: lookup")
        .expect("test: valid row");
    for index in 0..70 {
        crate::memory::sqlite::insert_raw_for_test(
            &sqlite,
            &workspace_id,
            &valid.owner_id,
            &format!("a{index:03}"),
            1,
            &valid.key_id,
            &valid.usage_digest,
            "metered",
            "pending",
            &valid.facts,
            &[0u8; 32],
            &[0u8; 32],
        )
        .expect("test: invalid row");
    }
    let report = run_pass(memory4.as_ref(), &workspace_id, &ws4, &cost).expect("test: pass");
    assert!(report.exhausted, "exhaustion only after the scan passes b000");
    assert_eq!(report.retryable_seen, 1);
    assert_eq!(report.progressed, 1);
    assert_eq!(ledger_lines(&ws4, "b000"), 1);
    let rows = fixture_readback::outbox_rows(&ws4);
    assert_eq!(rows.len(), 71);
    for row in &rows {
        if row.settlement_id == "b000" {
            assert_eq!(row.state, "acknowledged");
        } else {
            assert_eq!(row.state, "pending", "invalid rows stay pending and unprojected");
            assert_eq!(ledger_lines(&ws4, &row.settlement_id), 0);
        }
    }
}

#[cfg(unix)]
#[test]
fn legacy_files_warn_once_and_are_never_opened() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let ws = dir.path();
    for relative in super::LEGACY_FILES {
        let path = ws.join(relative);
        std::fs::create_dir_all(path.parent().expect("test: parent")).expect("test: legacy dir");
        // A FIFO is the open spy: any open without O_NONBLOCK would block.
        rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::from_raw_mode(0o600)).expect("test: fifo");
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let probe = ws.to_path_buf();
    std::thread::spawn(move || {
        let mut logged = Vec::new();
        for _restart in 0..3 {
            let warned = std::sync::atomic::AtomicBool::new(false);
            let first = warn_legacy_files(&warned, &probe);
            let second = warn_legacy_files(&warned, &probe);
            logged.push((first, second));
        }
        let _ = sender.send(logged);
    });
    let logged = receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("test: detection never opens a legacy file");
    assert_eq!(logged, vec![(true, false); 3], "exactly one warning per process");

    let clean = tempfile::tempdir().expect("test: clean workspace");
    let warned = std::sync::atomic::AtomicBool::new(false);
    assert!(!warn_legacy_files(&warned, clean.path()));
    assert!(!clean.path().join("self").exists(), "detection creates nothing");
}

#![cfg(test)]

use super::{Cadence, CostOutboxProjector, PassObserver, PassReport, run_pass, warn_legacy_files};
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

/// The cost ledger read is bounded; a larger ledger fails the test.
const MAX_LEDGER_BYTES: u64 = 1 << 20;

fn ledger_lines(workspace: &Path, settlement: &str) -> usize {
    use std::io::Read as _;
    let file = match std::fs::File::open(workspace.join("state").join("costs.jsonl")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!("test: cost ledger: {error}"),
    };
    let mut text = String::new();
    file.take(MAX_LEDGER_BYTES + 1)
        .read_to_string(&mut text)
        .expect("test: cost ledger read");
    assert!(
        u64::try_from(text.len()).expect("test: ledger length") <= MAX_LEDGER_BYTES,
        "test: the cost ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    text.lines().filter(|line| line.contains(settlement)).count()
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
    let report = run_pass(
        reopened.as_ref(),
        &ws.to_string_lossy(),
        &ws,
        &cost,
        &tokio_util::sync::CancellationToken::new(),
    )
    .expect("test: pass");
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
        let first = scope.spawn(|| {
            run_pass(
                memory3.as_ref(),
                &workspace_id,
                &ws3,
                &cost,
                &tokio_util::sync::CancellationToken::new(),
            )
        });
        let second = scope.spawn(|| {
            run_pass(
                memory3.as_ref(),
                &workspace_id,
                &ws3,
                &cost,
                &tokio_util::sync::CancellationToken::new(),
            )
        });
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
    let report = run_pass(
        memory4.as_ref(),
        &workspace_id,
        &ws4,
        &cost,
        &tokio_util::sync::CancellationToken::new(),
    )
    .expect("test: pass");
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
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
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

/// The outbox state of `settlement` in `workspace`.
fn outbox_state(workspace: &Path, settlement: &str) -> Option<String> {
    fixture_readback::outbox_rows(workspace)
        .into_iter()
        .find(|row| row.settlement_id == settlement)
        .map(|row| row.state)
}

/// Poll `ready` every 10 ms until it holds or `limit` passes.
async fn eventually(limit: std::time::Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if ready() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A short cadence for the real background scheduler.
const TEST_CADENCE: Cadence = Cadence {
    retry: std::time::Duration::from_millis(50),
    max: std::time::Duration::from_millis(200),
};

/// Scheduler passes a test observes at most; a full channel drops reports.
const OBSERVED_PASSES: usize = 64;
/// The longest a test waits for one observed scheduler pass.
const PASS_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The next observed pass report that satisfies `wanted`, skipping others
/// (another test's re-arm can wake the scheduler for an extra pass), within
/// [`PASS_WAIT`].
async fn observed_pass(
    passes: &mut tokio::sync::mpsc::Receiver<PassReport>,
    wanted: impl Fn(&PassReport) -> bool,
) -> Option<PassReport> {
    tokio::time::timeout(PASS_WAIT, async {
        while let Some(report) = passes.recv().await {
            if wanted(&report) {
                return Some(report);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// P2-R5/OV-11: the real background scheduler, idle after an empty startup
/// pass, is re-armed when an inline projection leaves its row in `error`
/// (here the ledger cannot be opened). While the failure lasts, the row stays
/// retryable and the scheduler retries it on its back-off; once the failure
/// clears, the next scheduled pass acknowledges the row exactly once. Each
/// transition is observed from the scheduler's own pass reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_scheduler_rearms_after_empty_pass_and_retries_transient_failures() {
    let cost = priced_cost_config();
    let (_dir, ws) = workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws).expect("test: memory"));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (sender, mut passes) = tokio::sync::mpsc::channel(OBSERVED_PASSES);
    let projector = CostOutboxProjector::spawn(
        Arc::clone(&memory),
        ws.clone(),
        cost.clone(),
        shutdown.child_token(),
        TEST_CADENCE,
        PassObserver { sender: Some(sender) },
    );
    // The startup pass completes an exhausted scan of the empty outbox: the
    // scheduler goes idle and waits for a re-arm.
    let startup = observed_pass(&mut passes, |_| true)
        .await
        .expect("test: the startup pass is observed");
    assert!(
        startup.exhausted && startup.retryable_left == 0,
        "the startup pass is an exhausted scan: {startup:?}"
    );
    assert!(fixture_readback::outbox_rows(&ws).is_empty());

    // The turn is admitted against a healthy ledger; then a transient ledger
    // failure (the ledger path becomes a directory) hits its terminal.
    let fabric = MemoryFabric::new(Arc::clone(&memory), ws.to_string_lossy());
    let mut commit = completed_commit("prx-rearm");
    let _service = bind_terminal_fixture(&ws, Arc::clone(&memory), &mut commit, &cost).await;
    let ledger = ws.join("state").join("costs.jsonl");
    if ledger.exists() {
        std::fs::remove_file(&ledger).expect("test: clear ledger");
    }
    std::fs::create_dir_all(&ledger).expect("test: ledger blocked by a directory");
    finalize_turn(&fabric, commit, &cost, &ws)
        .await
        .expect("test: metered terminal commit");
    assert_eq!(
        outbox_state(&ws, "prx-rearm").as_deref(),
        Some("error"),
        "the inline projection left the row retryable"
    );
    // The re-armed scheduler runs a pass that sees the row and fails to
    // project it while the failure lasts: the row stays retryable and
    // nothing reaches the ledger.
    let failed = observed_pass(&mut passes, |report| {
        report.retryable_seen > 0 && report.retryable_left > 0
    })
    .await
    .expect("test: a failed scheduled projection is observed");
    assert_eq!(failed.progressed, 0, "the failed pass changed nothing: {failed:?}");
    assert_eq!(outbox_state(&ws, "prx-rearm").as_deref(), Some("error"));
    assert!(ledger.is_dir(), "nothing replaced the blocked ledger");

    // Only now the failure clears; a later scheduled pass on the back-off
    // acknowledges the row.
    std::fs::remove_dir(&ledger).expect("test: clear the ledger failure");
    let acknowledged = observed_pass(&mut passes, |report| {
        report.progressed > 0 && report.retryable_left == 0
    })
    .await
    .expect("test: the scheduler retried the row after the failure cleared");
    assert!(acknowledged.retryable_seen > 0, "{acknowledged:?}");
    assert_eq!(outbox_state(&ws, "prx-rearm").as_deref(), Some("acknowledged"));
    assert_eq!(ledger_lines(&ws, "prx-rearm"), 1);
    assert!(projector.stop().await);
}

/// A failed or deadline-cut pass is not exhaustion: the scheduler retries it
/// on the back-off. Only a complete scan without a retryable row idles.
#[test]
fn failed_or_incomplete_passes_are_retried_on_the_back_off() {
    let mut interval = TEST_CADENCE.retry;
    let failed = PassReport::default();
    assert_eq!(
        TEST_CADENCE.next_wait(&failed, &mut interval),
        Some(TEST_CADENCE.retry * 2)
    );
    assert_eq!(TEST_CADENCE.next_wait(&failed, &mut interval), Some(TEST_CADENCE.max));
    assert_eq!(TEST_CADENCE.next_wait(&failed, &mut interval), Some(TEST_CADENCE.max));
    let progressed = PassReport {
        retryable_seen: 2,
        progressed: 1,
        retryable_left: 1,
        exhausted: false,
    };
    assert_eq!(
        TEST_CADENCE.next_wait(&progressed, &mut interval),
        Some(TEST_CADENCE.retry)
    );
    let left = PassReport {
        retryable_seen: 1,
        progressed: 0,
        retryable_left: 1,
        exhausted: true,
    };
    assert_eq!(
        TEST_CADENCE.next_wait(&left, &mut interval),
        Some(TEST_CADENCE.retry * 2)
    );
    let idle = PassReport {
        retryable_seen: 1,
        progressed: 1,
        retryable_left: 0,
        exhausted: true,
    };
    assert_eq!(TEST_CADENCE.next_wait(&idle, &mut interval), None);
    assert_eq!(interval, TEST_CADENCE.retry);
}

/// P2-R5: the projector ends when the process shutdown token is cancelled,
/// and its completion handle is awaited within the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projector_ends_with_the_process_shutdown_token() {
    let (_dir, ws) = workspace();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&ws).expect("test: memory"));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let projector = CostOutboxProjector::start(memory, ws, priced_cost_config(), &shutdown);
    shutdown.cancel();
    assert!(
        eventually(std::time::Duration::from_secs(5), || projector.task.is_finished()).await,
        "the shutdown token ends the task"
    );
    assert!(projector.stop().await);
}

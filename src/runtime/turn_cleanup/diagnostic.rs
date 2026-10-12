#![cfg(test)]
//! Explicitly scoped test-only scalar evidence from the real cleanup owner.
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
const LIMIT: usize = 64;
#[derive(Clone, Serialize)]
struct Row {
    owner: u64,
    stage: &'static str,
    ticket: u64,
    pending: usize,
    failed: bool,
    ok: Option<bool>,
    present: Option<bool>,
}
#[derive(Serialize)]
struct Evidence {
    rows: Vec<Row>,
    overflow: u64,
    first_failure: Option<Row>,
    fixture: Option<FixtureSummary>,
}
pub(crate) struct Recorder {
    path: std::path::PathBuf,
    evidence: parking_lot::Mutex<Evidence>,
    next_owner: AtomicU64,
    active: AtomicU64,
    changed: tokio::sync::Notify,
}
impl Recorder {
    pub(crate) fn new(path: std::path::PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            evidence: parking_lot::Mutex::new(Evidence {
                rows: Vec::with_capacity(LIMIT),
                overflow: 0,
                first_failure: None,
                fixture: None,
            }),
            next_owner: AtomicU64::new(0),
            active: AtomicU64::new(0),
            changed: Default::default(),
        })
    }
    pub(crate) async fn scope<F: std::future::Future>(self: &Arc<Self>, work: F) -> F::Output {
        CURRENT
            .scope(
                Binding {
                    recorder: Some(Arc::clone(self)),
                    owner: 0,
                },
                work,
            )
            .await
    }
    fn record(&self, row: Row) {
        let mut evidence = self.evidence.lock();
        if row.ok == Some(false) && evidence.first_failure.is_none() {
            evidence.first_failure = Some(row.clone());
        }
        if evidence.rows.len() < LIMIT {
            evidence.rows.push(row);
        } else {
            evidence.overflow = evidence.overflow.saturating_add(1);
        }
    }
    /// Wait only for already-created diagnostic owners to finish/drop; never change their deadlines.
    pub(crate) async fn wait_for_completion(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
    pub(crate) fn fixture_result(&self, summary: FixtureSummary) {
        self.evidence.lock().fixture = Some(summary);
    }
    /// Called once by the private fixture after completion, never from tracker/queue/ACK locks.
    pub(crate) fn flush(&self) -> anyhow::Result<()> {
        use std::io::Write as _;
        anyhow::ensure!(
            self.active.load(Ordering::Acquire) == 0,
            "diagnostic owners still active"
        );
        let bytes = {
            let evidence = self.evidence.lock();
            serde_json::to_vec_pretty(&*evidence)?
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.path)?;
        file.write_all(&bytes)?;
        Ok(())
    }
}
#[derive(Clone)]
pub(crate) struct Binding {
    recorder: Option<Arc<Recorder>>,
    owner: u64,
}
impl Default for Binding {
    fn default() -> Self {
        let recorder = CURRENT.try_with(|binding| binding.recorder.clone()).ok().flatten();
        let owner = recorder
            .as_ref()
            .map_or(0, |r| r.next_owner.fetch_add(1, Ordering::Relaxed) + 1);
        Self { recorder, owner }
    }
}
impl Binding {
    pub(super) fn start_completion(&self) -> Option<Completion> {
        let recorder = self.recorder.as_ref()?;
        recorder.active.fetch_add(1, Ordering::AcqRel);
        Some(Completion {
            binding: self.clone(),
            finished: false,
        })
    }
    pub(super) fn for_parent(parent: Option<&Self>) -> Self {
        let mut binding = Self::default();
        if binding.recorder.is_none() {
            binding.recorder = parent.and_then(|parent| parent.recorder.clone());
            binding.owner = binding
                .recorder
                .as_ref()
                .map_or(0, |r| r.next_owner.fetch_add(1, Ordering::Relaxed) + 1);
        }
        binding
    }
    pub(super) fn owner(&self) -> u64 {
        self.owner
    }
    pub(super) fn record(&self, stage: &'static str, ticket: u64, pending: usize, failed: bool, ok: Option<bool>) {
        if let Some(recorder) = &self.recorder {
            recorder.record(Row {
                owner: self.owner,
                stage,
                ticket,
                pending,
                failed,
                ok,
                present: None,
            });
        }
    }
    pub(super) async fn scope<F: std::future::Future>(&self, work: F) -> F::Output {
        CURRENT.scope(self.clone(), work).await
    }
}
tokio::task_local! { static CURRENT: Binding; }
pub(crate) fn queue(stage: &'static str, drained: bool) {
    let _ = CURRENT.try_with(|binding| binding.record(stage, 0, 0, false, Some(drained)));
}

pub(crate) fn queue_presence(stage: &'static str, present: bool) {
    let _ = CURRENT.try_with(|binding| {
        if let Some(recorder) = &binding.recorder {
            recorder.record(Row {
                owner: binding.owner,
                stage,
                ticket: 0,
                pending: 0,
                failed: false,
                ok: None,
                present: Some(present),
            });
        }
    });
}

#[derive(Serialize)]
pub(crate) struct FixtureSummary {
    pub(crate) calls: usize,
    pub(crate) result_present: bool,
    pub(crate) success: Option<bool>,
    pub(crate) depth_limit: bool,
    pub(crate) turn_ok: Option<bool>,
    pub(crate) panicked: bool,
}
pub(super) struct Completion {
    binding: Binding,
    finished: bool,
}
impl Completion {
    pub(super) fn mark_finished(&mut self) {
        self.finished = true;
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(recorder) = &self.binding.recorder {
            self.binding
                .record("cleanup_owner_completion", 0, 0, !self.finished, Some(self.finished));
            recorder.active.fetch_sub(1, Ordering::AcqRel);
            recorder.changed.notify_waiters();
        }
    }
}

/// Test-only boundary witness: false means reached but not passed (Err/drop/panic).
/// Closed test-only stage labels; never accept caller-provided error/payload text.
#[derive(Clone, Copy)]
pub(crate) enum StageName {
    TerminalFlush,
    TerminalDropFinalize,
    TerminalFinalize,
    TerminalReceiptOwner,
    TerminalRejoin,
    RejoinPreconditions,
    RejoinLookup,
    RejoinReplay,
    RejoinReplayScope,
    RejoinFacts,
    RejoinCorrelator,
    RejoinRecord,
    RejoinDrain,
    RejoinConfirmAndRepairs,
}
impl StageName {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalFlush => "terminal_flush",
            Self::TerminalDropFinalize => "terminal_drop_finalize",
            Self::TerminalFinalize => "terminal_finalize",
            Self::TerminalReceiptOwner => "terminal_receipt_owner",
            Self::TerminalRejoin => "terminal_rejoin",
            Self::RejoinPreconditions => "rejoin_preconditions",
            Self::RejoinLookup => "rejoin_lookup",
            Self::RejoinReplay => "rejoin_replay",
            Self::RejoinReplayScope => "rejoin_replay_scope",
            Self::RejoinFacts => "rejoin_facts",
            Self::RejoinCorrelator => "rejoin_correlator",
            Self::RejoinRecord => "rejoin_record",
            Self::RejoinDrain => "rejoin_drain",
            Self::RejoinConfirmAndRepairs => "rejoin_confirm_and_repairs",
        }
    }
}
pub(crate) struct Stage {
    binding: Binding,
    ticket: u64,
    name: StageName,
    passed: bool,
}
impl Stage {
    pub(crate) fn current(name: StageName) -> Self {
        let binding = CURRENT.try_with(Clone::clone).unwrap_or_default();
        Self {
            binding,
            ticket: 0,
            name,
            passed: false,
        }
    }
    pub(crate) fn ticket(ticket: Option<&super::CleanupTicket>, name: StageName) -> Self {
        let mut stage = Self::current(name);
        if let Some(ticket) = ticket {
            stage.binding = ticket.tracker.diagnostic.clone();
            stage.ticket = ticket.id;
        }
        stage
    }
    pub(crate) fn next(&mut self, name: StageName) {
        self.binding
            .record(self.name.as_str(), self.ticket, 0, false, Some(true));
        self.name = name;
    }
    pub(crate) fn finish(mut self) {
        self.passed = true;
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        self.binding
            .record(self.name.as_str(), self.ticket, 0, !self.passed, Some(self.passed));
    }
}
/// Tokio does not inherit task locals; retain only the existing recorder/owner.
pub(crate) fn terminal_binding(ticket: Option<&super::CleanupTicket>) -> Binding {
    ticket.map_or_else(Binding::default, |ticket| ticket.tracker.diagnostic.clone())
}
pub(crate) async fn terminal_scope<F: std::future::Future>(binding: Binding, work: F) -> F::Output {
    CURRENT.scope(binding, work).await
}

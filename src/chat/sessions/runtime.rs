//! Thin chat-side handle over the shared sessions registries.
//!
//! [`ChatSessionsHandle`] is **not** a supervisor and does **not** own a second
//! registry. It is the chat-facing child TUI registry projection: one display
//! sequence space over agents, shells, and PTYs. It wraps the single-source
//! `Arc<RwLock<Vec<SubAgentRun>>>` that the chat main loop builds once and
//! shares with the four sessions tools
//! (`sessions_spawn`/`sessions_list`/`session_status`/`sessions_send`), plus
//! chat-owned shell / PTY registries. The chat `/sessions` and `/kill` commands
//! read/act through these same registries.
//!
//! The short display alias `#N` lives only here, in the chat main loop's
//! single-threaded state (`seq_map`); it is never shared across a lock.

use super::id::SessionId;
use super::model::{
    ManagedKind, ManagedSessionView, ManagedStatus, project_run, project_shell, project_shell_status, project_status,
};
use super::shell::ShellSession;
use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared, single-source registry of background shell sessions (v2).
///
/// `parking_lot::Mutex` (synchronous, never held across `.await`): shell
/// entries are short clonable handles, added/removed/scanned without any await
/// while the lock is held.
pub type ShellRegistry = Arc<Mutex<Vec<ShellSession>>>;

/// Shared, single-source registry of interactive PTY shell sessions (v3a).
///
/// Same rationale as [`ShellRegistry`]: clonable handles, short `parking_lot`
/// critical sections, never held across `.await`.
#[cfg(feature = "terminal-tui")]
pub type PtyRegistry = Arc<Mutex<Vec<super::pty::PtyShellSession>>>;

/// One line of a session's recent output, projected for `/attach` display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailLine {
    /// History entry role (e.g. `user` / `assistant` / `tool`).
    pub role: String,
    /// Entry content (already owned; the registry lock is released before
    /// returning these so we never hold a lock across the print `.await`).
    pub content: String,
}

/// A child session that has just reached a terminal state, surfaced once
/// to the chat main loop for the v1b summary reflow.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedSession {
    /// Display sequence `#N`.
    pub seq: u64,
    /// Underlying session id (run UUID for agents, shell id for shells); used by
    /// the caller to dedup "already reported".
    pub run_id: String,
    /// What kind of session finished (agent vs shell), for the reflow label.
    pub kind: ManagedKind,
    /// Who initiated the session (v5): operator (`/bg`/`/shell`/`/pty`) vs the
    /// model (a mid-turn `sessions_spawn`). Carried so the persisted summary
    /// fallback (when the live view is already gone) still records the correct
    /// provenance.
    pub origin: super::model::SessionOrigin,
    /// Terminal status (`Completed` / `Failed` / `Cancelled`).
    pub status: ManagedStatus,
    /// Result / failure text recorded by the run (the `Completed`/`Failed`
    /// payload), used as the reflow summary body.
    pub summary: String,
    /// When the child session started.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the child session reached its terminal state.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub token_usage_records: Vec<crate::chat::session::SessionTokenUsageRecord>,
}

/// Retention and archive limits for chat-side session cleanup.
#[derive(Debug, Clone)]
pub struct ReapPolicy {
    /// How long terminal sessions remain in the live strip after finishing.
    pub terminal_ttl: Duration,
    /// Always keep at least this many newest terminal sessions visible.
    pub keep_last_terminal: usize,
    /// Keep full archived logs for this many newest reaped sessions.
    pub archive_keep_last: usize,
    /// Keep full archived logs for this long after reaping.
    pub archive_ttl: Duration,
    /// Maximum log lines stored for one archived session.
    pub archive_max_lines: usize,
    /// Maximum UTF-8 bytes stored for one archived session log.
    pub archive_max_bytes: usize,
    /// Warn about detached shell/PTY entries after this much idle time.
    pub idle_warn_after: Duration,
}

impl Default for ReapPolicy {
    fn default() -> Self {
        Self {
            terminal_ttl: Duration::minutes(10),
            keep_last_terminal: 5,
            archive_keep_last: 5,
            archive_ttl: Duration::minutes(10),
            archive_max_lines: 200,
            archive_max_bytes: 64 * 1024,
            idle_warn_after: Duration::minutes(10),
        }
    }
}

/// Compact metadata for a session removed from the live registries.
#[derive(Debug, Clone, PartialEq)]
pub struct ReapedSession {
    pub id: SessionId,
    pub seq: u64,
    pub kind: ManagedKind,
    pub summary: super::model::PersistedSessionSummary,
    pub terminal_at: DateTime<Utc>,
    pub reaped_at: DateTime<Utc>,
}

/// Result of one cleanup pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReapOutcome {
    pub reaped: Vec<ReapedSession>,
}

/// Result of shutting down every child owned by the chat session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShutdownReport {
    pub summaries: Vec<super::model::PersistedSessionSummary>,
    pub ignored_ids: Vec<SessionId>,
    pub aborted_agents: usize,
    pub killed_shells: usize,
    #[cfg(feature = "terminal-tui")]
    pub killed_ptys: usize,
}

#[derive(Debug, Clone)]
struct ReapCandidate {
    id: SessionId,
    seq: u64,
    kind: ManagedKind,
    view: ManagedSessionView,
    summary_text: String,
    terminal_at: DateTime<Utc>,
}

/// Chat-side handle over the shared agent + shell registries.
///
/// Unifies two single-source registries into **one display seq space** so
/// `/sessions`, the Ctrl+G switcher, the status line, `/attach`, `/kill`, and
/// `/logs` treat background agents and background shells uniformly (plan §v2
/// "unified session list"):
///
/// - **agents**: `runs` is the same `Arc<RwLock<Vec<SubAgentRun>>>` injected into
///   the four sessions tools via `SessionsSpawnTool::new_with_registry`.
/// - **shells**: `shells` is the chat-side shell registry (v2); shell sessions
///   are added here by `/shell` and reaped lazily (they keep their terminal
///   status for `/sessions` history until chat exit).
pub struct ChatSessionsHandle {
    /// The single-source `active_runs` agent registry.
    runs: Arc<RwLock<Vec<SubAgentRun>>>,
    /// The single-source shell-session registry (v2).
    shells: ShellRegistry,
    /// The single-source interactive PTY-session registry (v3a).
    #[cfg(feature = "terminal-tui")]
    ptys: PtyRegistry,
    /// `#N` -> session id, assigned in first-seen order across **both** kinds.
    /// Owned by the main loop (single-threaded), so a plain `Vec` with no lock is
    /// correct.
    seq_map: Vec<(u64, SessionId)>,
    /// Next sequence number to hand out.
    next_seq: u64,
    /// First time this handle observed a PTY as exited. PTY sessions do not
    /// carry `finished_at`, so cleanup ages them from this observation point.
    pty_terminal_seen_at: HashMap<SessionId, DateTime<Utc>>,
    /// Compact metadata for sessions reaped from live registries. This lets
    /// `/logs #N` distinguish "was reaped" from "never existed" after the full
    /// bounded archive ages out.
    reaped_sessions: Vec<ReapedSession>,
}

impl ChatSessionsHandle {
    /// Build a handle over the supplied single-source agent registry Arc, with a
    /// fresh empty shell registry.
    #[must_use]
    pub fn new(runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self {
            runs,
            shells: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "terminal-tui")]
            ptys: Arc::new(Mutex::new(Vec::new())),
            seq_map: Vec::new(),
            next_seq: 1,
            pty_terminal_seen_at: HashMap::new(),
            reaped_sessions: Vec::new(),
        }
    }

    /// The PTY registry Arc, so the chat exit path can `kill` all interactive
    /// PTY sessions (and tests can assert on its contents). Cheap `Arc` clone.
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    pub fn pty_registry(&self) -> PtyRegistry {
        Arc::clone(&self.ptys)
    }

    /// Register a freshly spawned interactive PTY session and return its display
    /// seq `#N`. Pure main-loop state mutation plus a short `parking_lot` lock.
    #[cfg(feature = "terminal-tui")]
    pub fn add_pty(&mut self, session: super::pty::PtyShellSession) -> u64 {
        let id = session.id.clone();
        self.ptys.lock().push(session);
        self.seq_for(&id)
    }

    /// Find a PTY session by its display seq `#N`, cloning the (cheap) handle out
    /// of the registry. Returns `None` if the seq does not map to a PTY session.
    #[cfg(feature = "terminal-tui")]
    fn pty_for_seq(&self, seq: u64) -> Option<super::pty::PtyShellSession> {
        let id = self.id_for_seq(seq)?.clone();
        self.ptys.lock().iter().find(|s| s.id == id).cloned()
    }

    /// Public accessor for [`Self::pty_for_seq`], so the chat loop's `/attach`
    /// branch can fetch a live PTY handle to re-attach to it (v3b). The seq map is
    /// assumed already refreshed by a preceding `kind_for_seq`/`snapshot` in the
    /// same command handling; callers that need a guaranteed-fresh map should
    /// `snapshot()` first. Returns `None` if the seq is unknown or not a PTY.
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    pub fn pty_for_seq_public(&self, seq: u64) -> Option<super::pty::PtyShellSession> {
        self.pty_for_seq(seq)
    }

    /// Count *live* (not-yet-exited) PTY sessions, for the v3b spawn cap. Reaps
    /// any sessions whose child has exited first (so the count reflects only
    /// re-attachable sessions and dead drain threads do not linger).
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    pub fn live_pty_count(&self) -> usize {
        self.reap_dead_ptys();
        self.ptys.lock().iter().filter(|s| !s.has_exited()).count()
    }

    /// Reap PTY sessions whose child has exited: tear down their persistent drain
    /// reader (stop + bounded join) so no drain thread outlives the child. Dead
    /// sessions are KEPT in the registry (their `#N` stays visible as `exited` in
    /// `/sessions`) but their thread/fd resources are released. Idempotent.
    #[cfg(feature = "terminal-tui")]
    pub fn reap_dead_ptys(&self) {
        let dead: Vec<super::pty::PtyShellSession> =
            self.ptys.lock().iter().filter(|s| s.has_exited()).cloned().collect();
        for pty in &dead {
            pty.reap_reader();
        }
    }

    /// Kill the interactive PTY session at display seq `#N` (terminating its
    /// whole process group). Refreshes the seq map first so a just-`/pty`-ed
    /// session is addressable. Returns an error (never panics) if the seq is
    /// unknown or is not a PTY session.
    #[cfg(feature = "terminal-tui")]
    pub async fn kill_pty(&mut self, seq: u64) -> Result<()> {
        self.refresh_seqs().await;
        let pty = self.pty_for_seq(seq).ok_or_else(|| anyhow!("no PTY session #{seq}"))?;
        pty.kill().await
    }

    /// The shell registry Arc, so the chat exit path can `kill` all shells (and
    /// tests can assert on its contents). Cheap `Arc` clone.
    #[must_use]
    pub fn shell_registry(&self) -> ShellRegistry {
        Arc::clone(&self.shells)
    }

    /// Register a freshly spawned shell session and return its display seq `#N`.
    /// Pure main-loop state mutation plus a short `parking_lot` lock (no await).
    pub fn add_shell(&mut self, session: ShellSession) -> u64 {
        let id = session.id.clone();
        self.shells.lock().push(session);
        self.seq_for(&id)
    }

    /// Assign a stable `#N` to a run UUID if it has not been seen before,
    /// returning the sequence number. Pure main-loop state mutation, no lock.
    fn seq_for(&mut self, id: &SessionId) -> u64 {
        if let Some((seq, _)) = self.seq_map.iter().find(|(_, mapped)| mapped == id) {
            return *seq;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.seq_map.push((seq, id.clone()));
        seq
    }

    /// Resolve a display sequence `#N` to the underlying run UUID.
    fn id_for_seq(&self, seq: u64) -> Option<&SessionId> {
        self.seq_map.iter().find(|(mapped, _)| *mapped == seq).map(|(_, id)| id)
    }

    /// Resolve a [`SessionId`] to its display sequence `#N`, refreshing the seq
    /// map first so a freshly spawned (or freshly suspended) session always has a
    /// number. Returns `None` only if the id is not in either live registry.
    ///
    /// Used by the NeedsInput main-loop branch to label the `/approve <N>` hint.
    pub async fn seq_for_id(&mut self, id: &SessionId) -> Option<u64> {
        self.refresh_seqs().await;
        self.seq_map
            .iter()
            .find(|(_, mapped)| mapped == id)
            .map(|(seq, _)| *seq)
    }

    /// Refresh the `#N` -> session-id mapping from **both** live registries,
    /// assigning a new sequence to any session not seen before. Agents are
    /// enumerated first, then shells, in first-seen order, so display numbers stay
    /// stable across calls. Takes only a read lock on agents and a short
    /// `parking_lot` lock on shells.
    ///
    /// This is the single place seqs are minted: `snapshot` (for `/sessions`),
    /// `resolve_run_id` (for `/kill`/`/steer`), and `kind_for_seq` (for the
    /// kind-dependent dispatch) all call it, so a freshly spawned agent or shell
    /// gets a `#N` even when addressed before the next `/sessions`.
    ///
    /// Returns the agent run snapshot (shells are read separately via the registry
    /// when needed) so existing agent-only callers keep their shape.
    async fn refresh_seqs(&mut self) -> Vec<SubAgentRun> {
        let runs: Vec<SubAgentRun> = self.runs.read().await.clone();
        for run in &runs {
            let _ = self.seq_for(&SessionId::from_run_id(&run.id));
        }
        // Shells come after agents in seq order. Clone the ids out under the lock,
        // then assign (no await while the lock is held).
        let shell_ids: Vec<SessionId> = self.shells.lock().iter().map(|s| s.id.clone()).collect();
        for id in &shell_ids {
            let _ = self.seq_for(id);
        }
        // PTY sessions come after shells in seq order (v3a). Same lock discipline.
        #[cfg(feature = "terminal-tui")]
        {
            let pty_ids: Vec<SessionId> = self.ptys.lock().iter().map(|s| s.id.clone()).collect();
            for id in &pty_ids {
                let _ = self.seq_for(id);
            }
        }
        runs
    }

    /// Compact metadata for a previously reaped session, by display sequence.
    #[must_use]
    pub fn reaped_session(&self, seq: u64) -> Option<&ReapedSession> {
        self.reaped_sessions.iter().rev().find(|session| session.seq == seq)
    }

    /// Reap terminal sessions that are older than the retention window, while
    /// always keeping the newest terminal sessions visible.
    pub async fn reap(&mut self, policy: &ReapPolicy, now: DateTime<Utc>) -> ReapOutcome {
        let runs = self.refresh_seqs().await;
        let mut candidates = Vec::new();

        for run in &runs {
            let status = project_status(&run.status);
            if !matches!(
                status,
                ManagedStatus::Completed | ManagedStatus::Failed | ManagedStatus::Cancelled
            ) {
                continue;
            }
            let seq = self.seq_for(&SessionId::from_run_id(&run.id));
            let view = project_run(run, seq);
            let summary_text = match &run.status {
                SubAgentStatus::Completed(text) | SubAgentStatus::Failed(text) => compact_reaped_summary(text),
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => String::new(),
            };
            candidates.push(ReapCandidate {
                id: SessionId::from_run_id(&run.id),
                seq,
                kind: ManagedKind::Agent,
                view,
                summary_text,
                terminal_at: run.finished_at.unwrap_or(run.started_at),
            });
        }

        let shells = self.shells.lock().clone();
        for shell in &shells {
            let status = project_shell_status(&shell.status());
            if !matches!(
                status,
                ManagedStatus::Completed | ManagedStatus::Failed | ManagedStatus::Cancelled
            ) {
                continue;
            }
            let seq = self.seq_for(&shell.id);
            let view = project_shell(shell, seq);
            let summary_text = match shell.status() {
                super::shell::ShellStatus::Failed(reason) => compact_reaped_summary(&reason),
                super::shell::ShellStatus::Running
                | super::shell::ShellStatus::Completed
                | super::shell::ShellStatus::Cancelled => String::new(),
            };
            candidates.push(ReapCandidate {
                id: shell.id.clone(),
                seq,
                kind: ManagedKind::Shell,
                view,
                summary_text,
                terminal_at: shell.finished_at().unwrap_or(now),
            });
        }

        #[cfg(feature = "terminal-tui")]
        {
            let ptys = self.ptys.lock().clone();
            for pty in &ptys {
                if !pty.has_exited() {
                    self.pty_terminal_seen_at.remove(&pty.id);
                    continue;
                }
                let terminal_at = *self.pty_terminal_seen_at.entry(pty.id.clone()).or_insert(now);
                let seq = self.seq_for(&pty.id);
                candidates.push(ReapCandidate {
                    id: pty.id.clone(),
                    seq,
                    kind: ManagedKind::Pty,
                    view: super::model::project_pty(pty, seq),
                    summary_text: String::new(),
                    terminal_at,
                });
            }
        }

        candidates.sort_by(|a, b| b.terminal_at.cmp(&a.terminal_at).then_with(|| b.seq.cmp(&a.seq)));
        let keep_ids = candidates
            .iter()
            .enumerate()
            .filter_map(|(idx, candidate)| {
                let within_ttl = now.signed_duration_since(candidate.terminal_at) <= policy.terminal_ttl;
                (idx < policy.keep_last_terminal || within_ttl).then(|| candidate.id.clone())
            })
            .collect::<HashSet<_>>();

        let reaped = candidates
            .into_iter()
            .filter(|candidate| !keep_ids.contains(&candidate.id))
            .map(|candidate| ReapedSession {
                id: candidate.id,
                seq: candidate.seq,
                kind: candidate.kind,
                summary: super::model::PersistedSessionSummary::from_view(&candidate.view, candidate.summary_text),
                terminal_at: candidate.terminal_at,
                reaped_at: now,
            })
            .collect::<Vec<_>>();

        if reaped.is_empty() {
            return ReapOutcome { reaped };
        }

        let reap_ids = reaped.iter().map(|session| session.id.clone()).collect::<HashSet<_>>();
        {
            let mut runs = self.runs.write().await;
            runs.retain(|run| !reap_ids.contains(&SessionId::from_run_id(&run.id)));
        }
        self.shells.lock().retain(|shell| !reap_ids.contains(&shell.id));
        #[cfg(feature = "terminal-tui")]
        {
            let removed_ptys = self
                .ptys
                .lock()
                .iter()
                .filter(|pty| reap_ids.contains(&pty.id))
                .cloned()
                .collect::<Vec<_>>();
            for pty in &removed_ptys {
                pty.reap_reader();
                self.pty_terminal_seen_at.remove(&pty.id);
            }
            self.ptys.lock().retain(|pty| !reap_ids.contains(&pty.id));
        }
        self.seq_map.retain(|(_, id)| !reap_ids.contains(id));
        self.reaped_sessions.extend(reaped.clone());

        ReapOutcome { reaped }
    }

    /// Display sequences for detached shell/PTY sessions that should carry an
    /// idle warning in the strip. This is warning-only: it never kills sessions.
    pub async fn idle_warning_seqs(
        &mut self,
        policy: &ReapPolicy,
        now: DateTime<Utc>,
        session_rings: &HashMap<SessionId, super::event::SessionRing>,
    ) -> HashSet<u64> {
        let _ = self.refresh_seqs().await;
        let mut warned = HashSet::new();
        let runs = self.runs.read().await.clone();
        for run in &runs {
            if !matches!(
                &run.status,
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
            ) {
                continue;
            }
            let id = SessionId::from_run_id(&run.id);
            let last_activity = session_rings
                .get(&id)
                .and_then(super::event::SessionRing::last_pushed_at)
                .unwrap_or(run.started_at);
            if now.signed_duration_since(last_activity) >= policy.idle_warn_after {
                warned.insert(self.seq_for(&id));
            }
        }
        let shells = self.shells.lock().clone();
        for shell in &shells {
            if shell.is_terminal() {
                continue;
            }
            let last_activity = session_rings
                .get(&shell.id)
                .and_then(super::event::SessionRing::last_pushed_at)
                .unwrap_or(shell.started_at);
            if now.signed_duration_since(last_activity) >= policy.idle_warn_after {
                warned.insert(self.seq_for(&shell.id));
            }
        }
        #[cfg(feature = "terminal-tui")]
        {
            let ptys = self.ptys.lock().clone();
            for pty in &ptys {
                if pty.has_exited() {
                    continue;
                }
                let last_activity = pty.last_output_at().unwrap_or(pty.started_at);
                if now.signed_duration_since(last_activity) >= policy.idle_warn_after {
                    warned.insert(self.seq_for(&pty.id));
                }
            }
        }
        warned
    }

    /// Find a shell session by its display seq `#N`, cloning the (cheap) handle
    /// out of the registry. Returns `None` if the seq does not map to a shell.
    fn shell_for_seq(&self, seq: u64) -> Option<ShellSession> {
        let id = self.id_for_seq(seq)?.clone();
        self.shells.lock().iter().find(|s| s.id == id).cloned()
    }

    /// Snapshot all child TUI sessions (agents + shells + PTYs) as chat-side views in a
    /// single seq space, assigning/refreshing display sequence numbers.
    pub async fn snapshot(&mut self) -> Vec<ManagedSessionView> {
        let runs = self.refresh_seqs().await;
        let mut views = Vec::with_capacity(runs.len());
        for run in &runs {
            // `refresh_seqs` already assigned a seq for every present run, so the
            // lookup below cannot fail; `seq_for` is idempotent regardless.
            let seq = self.seq_for(&SessionId::from_run_id(&run.id));
            views.push(project_run(run, seq));
        }
        // Shells, projected with their already-assigned seqs (after agents).
        let shells = self.shells.lock().clone();
        for shell in &shells {
            let seq = self.seq_for(&shell.id);
            views.push(project_shell(shell, seq));
        }
        // Interactive PTY sessions, after shells (v3a).
        #[cfg(feature = "terminal-tui")]
        {
            let ptys = self.ptys.lock().clone();
            for pty in &ptys {
                let seq = self.seq_for(&pty.id);
                views.push(super::model::project_pty(pty, seq));
            }
        }
        views.sort_by_key(|v| v.seq);
        views
    }

    /// Detach all live child-session registries before switching the owning
    /// chat session. Returns display-only persisted summaries plus ids that may
    /// still emit late events, so the caller can ignore stale event-bridge
    /// messages from the old chat session.
    ///
    /// This never revives or migrates child processes into the new chat session.
    /// Agent tasks are aborted best-effort; shell/PTY processes are killed
    /// best-effort and their registries are cleared from the chat projection.
    pub async fn detach_for_chat_session_switch(
        &mut self,
    ) -> (Vec<super::model::PersistedSessionSummary>, Vec<SessionId>) {
        let report = self.shutdown_all("chat-session-switch").await;
        (report.summaries, report.ignored_ids)
    }

    /// Terminate every live child and clear all live registries.
    pub async fn shutdown_all(&mut self, reason: &str) -> ShutdownReport {
        let views = self.snapshot().await;
        let summaries = views
            .iter()
            .map(|view| super::model::PersistedSessionSummary::from_view(view, String::new()))
            .collect::<Vec<_>>();
        let ignored_ids = views.iter().map(|view| view.id.clone()).collect::<Vec<_>>();
        let mut aborted_agents = 0usize;
        {
            let mut runs = self.runs.write().await;
            for run in runs.iter() {
                if let Some(handle) = run.abort_handle.as_ref() {
                    handle.abort();
                    aborted_agents = aborted_agents.saturating_add(1);
                }
            }
            runs.clear();
        }
        let shells = self.shells.lock().clone();
        let mut killed_shells = 0usize;
        for shell in &shells {
            if !shell.is_terminal() {
                match shell.kill().await {
                    Ok(()) => killed_shells = killed_shells.saturating_add(1),
                    Err(e) => tracing::warn!(error = %e, reason, "failed to terminate shell during chat shutdown"),
                }
            }
        }
        self.shells.lock().clear();
        #[cfg(feature = "terminal-tui")]
        let mut killed_ptys = 0usize;
        #[cfg(feature = "terminal-tui")]
        {
            let ptys = self.ptys.lock().clone();
            for pty in &ptys {
                if !pty.has_exited() {
                    match pty.kill().await {
                        Ok(()) => killed_ptys = killed_ptys.saturating_add(1),
                        Err(e) => tracing::warn!(error = %e, reason, "failed to terminate PTY during chat shutdown"),
                    }
                }
            }
            self.ptys.lock().clear();
        }
        self.seq_map.clear();
        self.next_seq = 1;
        self.pty_terminal_seen_at.clear();

        ShutdownReport {
            summaries,
            ignored_ids,
            aborted_agents,
            killed_shells,
            #[cfg(feature = "terminal-tui")]
            killed_ptys,
        }
    }

    /// Poll the registry for sessions that have reached a terminal state and
    /// have not yet been reported, for the v1b summary reflow.
    ///
    /// `reported` is the caller-owned set of run UUIDs already surfaced; this
    /// method appends newly-finished run ids to it and returns one
    /// [`FinishedSession`] per newly-finished run. It is a **read-only poll**
    /// (no event bus — that is v1.1): the chat main loop calls it on a timer.
    /// Running sessions are skipped; only `Completed` / `Failed` / `Cancelled`
    /// are reported, and each run is reported exactly once.
    pub async fn poll_finished(&mut self, reported: &mut std::collections::HashSet<String>) -> Vec<FinishedSession> {
        let runs = self.refresh_seqs().await;
        let mut finished = Vec::new();
        for run in &runs {
            let status = project_status(&run.status);
            let is_terminal = matches!(
                status,
                ManagedStatus::Completed | ManagedStatus::Failed | ManagedStatus::Cancelled
            );
            if !is_terminal {
                continue;
            }
            if reported.contains(&run.id) {
                continue;
            }
            reported.insert(run.id.clone());
            let seq = self.seq_for(&SessionId::from_run_id(&run.id));
            let summary = match &run.status {
                SubAgentStatus::Completed(s) | SubAgentStatus::Failed(s) => s.clone(),
                // Non-terminal states are filtered out above; reached only
                // defensively, so they carry no completion summary.
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => String::new(),
            };
            let updated_at = run.finished_at.unwrap_or_else(chrono::Utc::now);
            finished.push(FinishedSession {
                seq,
                run_id: run.id.clone(),
                kind: ManagedKind::Agent,
                origin: super::model::SessionOrigin::from_parent_run_id(run.parent_run_id.as_ref()),
                status,
                summary,
                created_at: run.started_at,
                updated_at,
                token_usage_records: run.token_usage_records.clone(),
            });
        }
        // Shells: same once-only terminal reporting, keyed by shell id.
        let shells = self.shells.lock().clone();
        for shell in &shells {
            let shell_status = shell.status();
            let status = project_shell_status(&shell_status);
            let is_terminal = matches!(
                status,
                ManagedStatus::Completed | ManagedStatus::Failed | ManagedStatus::Cancelled
            );
            if !is_terminal {
                continue;
            }
            let key = shell.id.as_str().to_string();
            if reported.contains(&key) {
                continue;
            }
            reported.insert(key.clone());
            let seq = self.seq_for(&shell.id);
            let summary = match &shell_status {
                super::shell::ShellStatus::Failed(reason) => reason.clone(),
                _ => String::new(),
            };
            finished.push(FinishedSession {
                seq,
                run_id: key,
                kind: ManagedKind::Shell,
                origin: match shell.origin {
                    super::shell::ShellOrigin::User => super::model::SessionOrigin::User,
                    super::shell::ShellOrigin::Model => super::model::SessionOrigin::Model,
                },
                status,
                summary,
                created_at: shell.started_at,
                updated_at: shell.finished_at().unwrap_or_else(chrono::Utc::now),
                token_usage_records: Vec::new(),
            });
        }
        finished
    }

    /// Resolve a display seq `#N` to the kind of session it addresses (agent vs
    /// shell), refreshing the seq map first. Returns an error (never panics) if
    /// the seq is unknown. Lets the chat loop dispatch `/kill` / `/logs` to the
    /// right backend.
    pub async fn kind_for_seq(&mut self, seq: u64) -> Result<ManagedKind> {
        self.refresh_seqs().await;
        let id = self
            .id_for_seq(seq)
            .cloned()
            .ok_or_else(|| anyhow!("no session #{seq}"))?;
        if self.shells.lock().iter().any(|s| s.id == id) {
            return Ok(ManagedKind::Shell);
        }
        #[cfg(feature = "terminal-tui")]
        if self.ptys.lock().iter().any(|s| s.id == id) {
            return Ok(ManagedKind::Pty);
        }
        Ok(ManagedKind::Agent)
    }

    /// Kill the shell session at display seq `#N` (terminating its whole process
    /// group). Refreshes the seq map first so a just-`/shell`-ed session is
    /// addressable. Returns an error (never panics) if the seq is unknown or is
    /// not a shell.
    pub async fn kill_shell(&mut self, seq: u64) -> Result<()> {
        self.refresh_seqs().await;
        let shell = self
            .shell_for_seq(seq)
            .ok_or_else(|| anyhow!("no shell session #{seq}"))?;
        shell.kill().await
    }

    /// Resolve a display sequence `#N` to the underlying run UUID, refreshing the
    /// seq map from the live registry first so newly spawned (e.g. just-`/bg`-ed)
    /// runs are addressable without a prior `/sessions`.
    ///
    /// This does **not** perform the kill itself: the chat loop delegates the
    /// actual termination to the `sessions_spawn` tool's `kill` action so the
    /// shared kill semantics (side-effect gate authorization, completed/failed
    /// status check, `task.killed` event, `steer_tx` cleanup, channel
    /// announcement) apply uniformly. Returns an error (never panics) if the
    /// sequence is unknown after refresh.
    pub async fn resolve_run_id(&mut self, seq: u64) -> Result<String> {
        self.refresh_seqs().await;
        self.id_for_seq(seq)
            .map(|id| id.as_str().to_string())
            .ok_or_else(|| anyhow!("no session #{seq}"))
    }

    /// Whether the session at display sequence `#N` has reached a terminal state
    /// (`Completed` / `Failed` / `Cancelled`). Refreshes the seq map first so a
    /// just-spawned run is addressable. Returns an error (never panics) if the
    /// sequence is unknown.
    ///
    /// Used by `/attach` to decide its replay strategy: a terminal session's
    /// final answer already lives in the registry history (printed as the tail),
    /// and the same content was also captured in the live ring via `on_delta`, so
    /// replaying the ring would duplicate it. Running sessions keep ring replay +
    /// live follow so new incremental output is still visible.
    pub async fn is_terminal_for_seq(&mut self, seq: u64) -> Result<bool> {
        let runs = self.refresh_seqs().await;
        let target_id = self
            .id_for_seq(seq)
            .map(|id| id.as_str().to_string())
            .ok_or_else(|| anyhow!("no session #{seq}"))?;
        // Shell first (its id never appears in the agent registry).
        if let Some(shell) = self.shells.lock().iter().find(|s| s.id.as_str() == target_id) {
            return Ok(shell.is_terminal());
        }
        // PTY sessions: terminal once the child has exited (v3a).
        #[cfg(feature = "terminal-tui")]
        if let Some(pty) = self.ptys.lock().iter().find(|s| s.id.as_str() == target_id) {
            return Ok(pty.has_exited());
        }
        let run = runs
            .iter()
            .find(|r| r.id == target_id)
            .ok_or_else(|| anyhow!("no session #{seq}"))?;
        Ok(matches!(
            project_status(&run.status),
            ManagedStatus::Completed | ManagedStatus::Failed | ManagedStatus::Cancelled
        ))
    }

    /// Read a read-only tail of a child session's accumulated history.
    ///
    /// v1b `/attach` is a **read-only snapshot** (plan §v1b): it polls the run's
    /// `history` once and returns at most `last_n` most-recent entries. It does
    /// not subscribe to a live stream and never routes input into the session
    /// (that is v1.1). Returns an error (never panics) if the sequence is
    /// unknown.
    ///
    /// The registry read lock and the per-run history read lock are both
    /// released before this returns (the entries are cloned into owned
    /// [`TailLine`]s), so the caller can `.await` on printing without holding any
    /// lock across the await point (iron law).
    pub async fn tail(&mut self, seq: u64, last_n: usize) -> Result<Vec<TailLine>> {
        let runs = self.refresh_seqs().await;
        let target_id = self
            .id_for_seq(seq)
            .map(|id| id.as_str().to_string())
            .ok_or_else(|| anyhow!("no session #{seq}"))?;
        // Shells have no registry history; their stdout/stderr lives only in the
        // chat-side ring (replayed by `/attach` and dumped by `/logs`). Return an
        // empty tail (not an error) so `/attach <shell>` falls straight through to
        // ring replay.
        if self.shells.lock().iter().any(|s| s.id.as_str() == target_id) {
            return Ok(Vec::new());
        }
        // PTY sessions are full terminal handoffs with no captured history; an
        // empty tail (not an error) keeps `/attach`/`/logs` graceful (v3a).
        #[cfg(feature = "terminal-tui")]
        if self.ptys.lock().iter().any(|s| s.id.as_str() == target_id) {
            return Ok(Vec::new());
        }
        let history = {
            let run = runs
                .iter()
                .find(|r| r.id == target_id)
                .ok_or_else(|| anyhow!("no session #{seq}"))?;
            // Clone the `Arc<RwLock<…>>` so the registry read lock (held by the
            // `refresh_seqs` snapshot above is already dropped here) is not a
            // concern; we only take the per-run history lock below.
            Arc::clone(&run.history)
        };
        let entries = history.read().await;
        let start = entries.len().saturating_sub(last_n);
        let lines = entries
            .iter()
            .skip(start)
            .map(|e| TailLine {
                role: e.role.clone(),
                content: e.content.clone(),
            })
            .collect();
        Ok(lines)
    }
}

const REAP_SUMMARY_MAX_CHARS: usize = 120;

fn compact_reaped_summary(text: &str) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return String::new();
    };
    let mut chars = line.chars();
    let mut out = String::new();
    for _ in 0..REAP_SUMMARY_MAX_CHARS {
        match chars.next() {
            Some(ch) => out.push(ch),
            None => return out,
        }
    }
    if chars.next().is_some() && !out.is_empty() {
        out.pop();
        out.push('…');
    }
    out
}

/// Build the persistent status-line summary from a session snapshot.
///
/// Returns an empty string when there are no sessions (the chat status row is
/// then hidden). Otherwise produces a compact `running/completed/failed/
/// cancelled` count line, e.g. `sessions: 2 running · 1 completed`.
#[must_use]
pub fn status_summary(views: &[ManagedSessionView]) -> String {
    if views.is_empty() {
        return String::new();
    }
    let (mut running, mut needs_input, mut completed, mut failed, mut cancelled) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for v in views {
        match v.status {
            ManagedStatus::Running => running += 1,
            ManagedStatus::NeedsInput => needs_input += 1,
            ManagedStatus::Completed => completed += 1,
            ManagedStatus::Failed => failed += 1,
            ManagedStatus::Cancelled => cancelled += 1,
        }
    }
    let mut parts: Vec<String> = Vec::with_capacity(5);
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if needs_input > 0 {
        parts.push(format!("{needs_input} needs-input"));
    }
    if completed > 0 {
        parts.push(format!("{completed} completed"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if cancelled > 0 {
        parts.push(format!("{cancelled} cancelled"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("sessions: {}", parts.join(" \u{00B7} "))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::tools::sessions_spawn::{HistoryEntry, SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_run(id: &str, task: &str, status: SubAgentStatus) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: task.to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::<HistoryEntry>::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: String::new(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        }
    }

    fn finished_run(id: &str, finished_at: chrono::DateTime<Utc>) -> SubAgentRun {
        let mut run = make_run(
            id,
            &format!("task {id}"),
            SubAgentStatus::Completed(format!("done {id}")),
        );
        run.started_at = finished_at - chrono::Duration::minutes(1);
        run.finished_at = Some(finished_at);
        run
    }

    fn usage_record(
        source: crate::llm::route_decision::TokenUsageSource,
    ) -> crate::llm::route_decision::MeteredTokenUsageRecord {
        crate::llm::route_decision::MeteredTokenUsageRecord {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            prompt_tokens: 8_000,
            completion_tokens: 4_300,
            total_tokens: 12_300,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source,
            cost_usd: Some(0.0042),
        }
    }

    // ── Unified agent + shell session list (v2) ─────────────────────────────

    fn permissive_security() -> Arc<crate::security::SecurityPolicy> {
        Arc::new(crate::security::SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..crate::security::SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn reap_keeps_ten_minute_window_and_last_five_terminal_sessions() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:00:00Z")
            .expect("test timestamp")
            .with_timezone(&Utc);
        let mut runs = Vec::new();
        runs.push(finished_run("recent", now - chrono::Duration::minutes(9)));
        for idx in 1..=6 {
            runs.push(finished_run(
                &format!("old-{idx}"),
                now - chrono::Duration::minutes(30) + chrono::Duration::seconds(idx),
            ));
        }
        let runs = Arc::new(RwLock::new(runs));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let _ = handle.snapshot().await;

        let outcome = handle.reap(&ReapPolicy::default(), now).await;

        assert_eq!(
            outcome.reaped.len(),
            2,
            "entries outside both the TTL window and last-5 terminal set are reaped"
        );
        let reaped_ids = outcome
            .reaped
            .iter()
            .map(|session| session.summary.id.as_str())
            .collect::<Vec<_>>();
        assert!(reaped_ids.contains(&"old-1"));
        assert!(reaped_ids.contains(&"old-2"));
        assert!(
            handle.reaped_session(outcome.reaped[0].seq).is_some(),
            "compact reaped metadata is retained for /logs fallback"
        );
        let remaining = runs.read().await.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
        assert!(
            remaining.contains(&"recent".to_string()),
            "10-minute window keeps recent terminal runs"
        );
        assert_eq!(
            remaining.len(),
            5,
            "recent plus the four newest old terminal runs remain visible"
        );
        assert!(!remaining.contains(&"old-1".to_string()));
        assert!(!remaining.contains(&"old-2".to_string()));
    }

    #[tokio::test]
    async fn shutdown_all_aborts_agent_and_kills_shell() {
        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let abort_handle = task.abort_handle();
        let mut run = make_run("live-agent", "agent", SubAgentStatus::Running);
        run.abort_handle = Some(abort_handle);
        let runs = Arc::new(RwLock::new(vec![run]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));

        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("sleep 30", &sec, &sink).expect("test: spawn shell");
        handle.add_shell(shell.clone());

        let report = handle.shutdown_all("test-shutdown").await;

        assert_eq!(report.aborted_agents, 1);
        assert_eq!(report.killed_shells, 1);
        assert!(task.is_finished(), "agent task was aborted");
        assert!(runs.read().await.is_empty(), "agent registry cleared");
        assert!(handle.shell_registry().lock().is_empty(), "shell registry cleared");
        assert!(shell.is_terminal(), "shell process group was killed");
    }

    #[tokio::test]
    async fn idle_warning_marks_shell_without_killing_it() {
        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(runs);
        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("sleep 30", &sec, &sink).expect("test: spawn shell");
        let seq = handle.add_shell(shell.clone());

        let warnings = handle
            .idle_warning_seqs(
                &ReapPolicy::default(),
                shell.started_at + chrono::Duration::minutes(11),
                &HashMap::new(),
            )
            .await;

        assert!(warnings.contains(&seq), "long-running detached shell is warning-marked");
        assert!(
            !shell.is_terminal(),
            "idle warning must not auto-kill during active chat"
        );
        shell.kill().await.expect("test: cleanup shell");
    }

    #[tokio::test]
    async fn idle_warning_uses_ring_last_activity_for_running_agent() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-06T12:00:00Z")
            .expect("test timestamp")
            .with_timezone(&Utc);
        let mut run = make_run("agent-active", "long task", SubAgentStatus::Running);
        run.started_at = now - chrono::Duration::minutes(20);
        let id = SessionId::from_run_id(&run.id);
        let runs = Arc::new(RwLock::new(vec![run]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let seq = handle.seq_for(&id);
        let mut rings = HashMap::new();
        let mut ring = super::super::event::SessionRing::with_capacity(16);
        ring.push_at("still working".to_string(), now - chrono::Duration::minutes(1));
        rings.insert(id.clone(), ring);

        let warnings = handle.idle_warning_seqs(&ReapPolicy::default(), now, &rings).await;
        assert!(
            !warnings.contains(&seq),
            "recent output should prevent idle warning even when started_at is old"
        );

        rings
            .get_mut(&id)
            .expect("ring")
            .push_at("old output".to_string(), now - chrono::Duration::minutes(10));
        let warnings = handle.idle_warning_seqs(&ReapPolicy::default(), now, &rings).await;
        assert!(
            warnings.contains(&seq),
            "no output for idle_warn_after should mark the session idle"
        );
    }

    #[cfg(all(unix, feature = "terminal-tui"))]
    #[tokio::test]
    async fn shutdown_all_kills_pty_fixture() {
        use super::super::pty::PtyShellSession;
        use portable_pty::PtySize;

        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(runs);
        let sec = permissive_security();
        let pty = PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn PTY");
        handle.add_pty(pty.clone());

        let report = handle.shutdown_all("test-pty-shutdown").await;

        assert_eq!(report.killed_ptys, 1);
        assert!(handle.pty_registry().lock().is_empty(), "PTY registry cleared");
        for _ in 0..50 {
            if pty.has_exited() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(pty.has_exited(), "PTY child terminated during shutdown_all");
    }

    #[tokio::test]
    async fn snapshot_merges_agents_and_shells_in_one_seq_space() {
        let runs = Arc::new(RwLock::new(vec![make_run("a", "agent a", SubAgentStatus::Running)]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));

        // Agent gets #1.
        let first = handle.snapshot().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].seq, 1);
        assert_eq!(first[0].kind, ManagedKind::Agent);

        // Add a shell — it must appear in the *same* list with #2.
        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("sleep 30", &sec, &sink).expect("test: spawn shell");
        let shell_seq = handle.add_shell(shell.clone());
        assert_eq!(shell_seq, 2, "shells are numbered after agents");

        let merged = handle.snapshot().await;
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].kind, ManagedKind::Agent);
        assert_eq!(merged[1].kind, ManagedKind::Shell);
        assert_eq!(merged[1].seq, 2);
        assert!(merged[1].title.contains("sleep 30"));

        // kind_for_seq routes correctly.
        assert_eq!(handle.kind_for_seq(1).await.expect("test: #1 kind"), ManagedKind::Agent);
        assert_eq!(handle.kind_for_seq(2).await.expect("test: #2 kind"), ManagedKind::Shell);

        // kill_shell(#2) terminates the shell; kill_shell on the agent #1 errors.
        handle.kill_shell(2).await.expect("test: kill shell #2");
        assert!(shell.is_terminal());
        assert!(handle.kill_shell(1).await.is_err(), "agent seq is not a shell");
    }

    #[tokio::test]
    async fn detach_for_chat_session_switch_clears_registry_and_returns_ignored_ids() {
        let runs = Arc::new(RwLock::new(vec![make_run(
            "old-agent",
            "agent a",
            SubAgentStatus::Running,
        )]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        assert_eq!(handle.snapshot().await.len(), 1);

        let (summaries, ignored_ids) = handle.detach_for_chat_session_switch().await;

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "old-agent");
        assert_eq!(summaries[0].status, super::super::model::STATUS_INTERRUPTED);
        assert_eq!(ignored_ids.len(), 1);
        assert_eq!(ignored_ids[0].as_str(), "old-agent");
        assert!(runs.read().await.is_empty(), "agent registry must be cleared");
        assert!(
            handle.snapshot().await.is_empty(),
            "new chat session must start with an empty child-session registry"
        );
    }

    // ── Interactive PTY sessions in the unified list (v3a) ───────────────────

    #[cfg(all(unix, feature = "terminal-tui"))]
    #[tokio::test]
    async fn snapshot_includes_pty_and_kill_routes_to_it() {
        use super::super::pty::PtyShellSession;
        use portable_pty::PtySize;

        let runs = Arc::new(RwLock::new(vec![make_run("a", "agent a", SubAgentStatus::Running)]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));

        // Agent gets #1.
        assert_eq!(handle.snapshot().await.len(), 1);

        // Add an interactive PTY session — it must appear in the *same* list,
        // numbered after agents and shells, with kind `pty`.
        let sec = permissive_security();
        let pty = PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn PTY");
        let pty_seq = handle.add_pty(pty.clone());
        assert_eq!(pty_seq, 2, "PTY numbered after the single agent");

        let merged = handle.snapshot().await;
        assert_eq!(merged.len(), 2);
        let pty_view = merged.iter().find(|v| v.seq == 2).expect("test: PTY view present");
        assert_eq!(pty_view.kind, ManagedKind::Pty);
        assert!(pty_view.title.contains("sleep 30"));

        // kind_for_seq routes #2 to the PTY backend, #1 to the agent.
        assert_eq!(handle.kind_for_seq(2).await.expect("test: #2 kind"), ManagedKind::Pty);
        assert_eq!(handle.kind_for_seq(1).await.expect("test: #1 kind"), ManagedKind::Agent);

        // kill_pty(#2) terminates the PTY group; kill_pty on the agent #1 errors.
        handle.kill_pty(2).await.expect("test: kill PTY #2");
        for _ in 0..50 {
            if pty.has_exited() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(pty.has_exited(), "PTY child terminated after kill_pty");
        assert!(handle.kill_pty(1).await.is_err(), "agent seq is not a PTY session");
    }

    #[cfg(all(unix, feature = "terminal-tui"))]
    #[tokio::test]
    async fn live_pty_count_caps_and_reaps_dead() {
        // v3b: `live_pty_count` counts only not-yet-exited PTYs (so the `/pty`
        // spawn cap is enforced) and reaps the drain readers of exited ones.
        use super::super::pty::PtyShellSession;
        use portable_pty::PtySize;

        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let sec = permissive_security();

        // Two live PTYs → count is 2.
        let a = PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn live a");
        let b = PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn live b");
        handle.add_pty(a.clone());
        handle.add_pty(b.clone());
        assert_eq!(handle.live_pty_count(), 2, "two live PTYs counted");

        // A third that exits on its own must NOT count once dead, and reaping must
        // tear its drain reader down.
        let dead = PtyShellSession::spawn("exit 0", &sec, PtySize::default()).expect("test: spawn fast-exit");
        handle.add_pty(dead.clone());
        for _ in 0..100 {
            if dead.has_exited() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(dead.has_exited(), "fast-exit PTY reached terminal state");

        // live_pty_count reaps the dead one and counts only the two live PTYs.
        assert_eq!(handle.live_pty_count(), 2, "dead PTY is not counted, only the 2 live");

        a.kill().await.expect("test: cleanup a");
        b.kill().await.expect("test: cleanup b");
    }

    #[tokio::test]
    async fn poll_finished_reports_shell_once_with_kind() {
        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(runs);
        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("exit 0", &sec, &sink).expect("test: spawn shell");
        handle.add_shell(shell.clone());

        // Wait for the shell to finish.
        for _ in 0..50 {
            if shell.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut reported = std::collections::HashSet::new();
        let finished = handle.poll_finished(&mut reported).await;
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].kind, ManagedKind::Shell);
        assert_eq!(finished[0].status, ManagedStatus::Completed);
        // Reported exactly once.
        assert!(handle.poll_finished(&mut reported).await.is_empty());
    }

    #[tokio::test]
    async fn poll_finished_carries_agent_usage_and_never_fabricates_shell_usage() {
        let mut agent = make_run(
            "metered-agent",
            "metered task",
            SubAgentStatus::Completed("done".into()),
        );
        agent.token_usage_records = vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)];
        let runs = Arc::new(RwLock::new(vec![agent]));
        let mut handle = ChatSessionsHandle::new(runs);

        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("exit 0", &sec, &sink).expect("test: spawn shell");
        handle.add_shell(shell.clone());
        for _ in 0..50 {
            if shell.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut reported = std::collections::HashSet::new();
        let finished = handle.poll_finished(&mut reported).await;
        let agent = finished
            .iter()
            .find(|fin| fin.kind == ManagedKind::Agent)
            .expect("test: agent completion present");
        let shell = finished
            .iter()
            .find(|fin| fin.kind == ManagedKind::Shell)
            .expect("test: shell completion present");

        assert_eq!(agent.token_usage_records.len(), 1);
        assert_eq!(agent.token_usage_records[0].total_tokens, 12_300);
        assert!(
            shell.token_usage_records.is_empty(),
            "shell sessions have no LLM provider usage and must not fabricate tokens"
        );
    }

    #[tokio::test]
    async fn tail_returns_empty_for_shell_without_error() {
        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(runs);
        let (sink, _rx) = super::super::event::SessionEventSink::channel();
        let sec = permissive_security();
        let shell = super::super::shell::spawn_shell("sleep 30", &sec, &sink).expect("test: spawn shell");
        let seq = handle.add_shell(shell.clone());
        // Shells have no registry history; tail is empty, not an error.
        let lines = handle.tail(seq, 10).await.expect("test: shell tail empty ok");
        assert!(lines.is_empty());
        // is_terminal_for_seq works for shells too.
        assert!(!handle.is_terminal_for_seq(seq).await.expect("test: shell not terminal"));
        shell.kill().await.expect("test: cleanup kill");
    }

    #[tokio::test]
    async fn snapshot_assigns_stable_seqs() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("a", "task a", SubAgentStatus::Running),
            make_run("b", "task b", SubAgentStatus::Completed("ok".into())),
        ]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));

        let first = handle.snapshot().await;
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].seq, 1);
        assert_eq!(first[1].seq, 2);

        // A second snapshot must keep the same seqs for the same ids.
        let second = handle.snapshot().await;
        assert_eq!(second[0].seq, 1);
        assert_eq!(second[1].seq, 2);
    }

    #[tokio::test]
    async fn resolve_unknown_seq_errors() {
        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let mut handle = ChatSessionsHandle::new(runs);
        let err = handle
            .resolve_run_id(99)
            .await
            .expect_err("test: unknown seq must error");
        assert!(err.to_string().contains("no session #99"));
    }

    #[tokio::test]
    async fn resolve_returns_run_id_for_seq() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("a", "task a", SubAgentStatus::Running),
            make_run("b", "task b", SubAgentStatus::Running),
        ]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        // Establish the seq mapping via /sessions.
        let _ = handle.snapshot().await;
        assert_eq!(handle.resolve_run_id(1).await.expect("test: #1"), "a");
        assert_eq!(handle.resolve_run_id(2).await.expect("test: #2"), "b");
    }

    #[tokio::test]
    async fn resolve_assigns_seq_without_prior_snapshot() {
        // Regression: `/bg` then `/kill 1` must work even though `/sessions` was
        // never called — `resolve_run_id` refreshes the seq map itself.
        let runs = Arc::new(RwLock::new(vec![make_run(
            "fresh",
            "just spawned",
            SubAgentStatus::Running,
        )]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        assert_eq!(handle.resolve_run_id(1).await.expect("test: #1 after bg"), "fresh");
    }

    #[tokio::test]
    async fn is_terminal_distinguishes_running_from_finished() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("a", "task a", SubAgentStatus::Running),
            make_run("b", "task b", SubAgentStatus::Completed("done".into())),
            make_run("c", "task c", SubAgentStatus::Failed("boom".into())),
        ]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let _ = handle.snapshot().await; // assign seqs #1..#3

        assert!(!handle.is_terminal_for_seq(1).await.expect("test: #1 running"));
        assert!(handle.is_terminal_for_seq(2).await.expect("test: #2 completed"));
        assert!(handle.is_terminal_for_seq(3).await.expect("test: #3 failed"));

        let err = handle
            .is_terminal_for_seq(99)
            .await
            .expect_err("test: unknown seq must error");
        assert!(err.to_string().contains("no session #99"));
    }

    fn entry(role: &str, content: &str) -> HistoryEntry {
        HistoryEntry {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn tail_returns_last_n_entries_in_order() {
        let run = make_run("a", "task a", SubAgentStatus::Running);
        {
            let mut h = run.history.write().await;
            h.push(entry("user", "1"));
            h.push(entry("assistant", "2"));
            h.push(entry("assistant", "3"));
        }
        let runs = Arc::new(RwLock::new(vec![run]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let _ = handle.snapshot().await; // assign seq #1

        let last_two = handle.tail(1, 2).await.expect("test: tail #1");
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0].content, "2");
        assert_eq!(last_two[1].content, "3");
        assert_eq!(last_two[1].role, "assistant");
    }

    #[tokio::test]
    async fn tail_clamps_to_available_and_errors_on_unknown_seq() {
        let run = make_run("a", "task a", SubAgentStatus::Running);
        {
            run.history.write().await.push(entry("user", "only"));
        }
        let runs = Arc::new(RwLock::new(vec![run]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        // last_n larger than available -> returns all without panicking.
        let all = handle.tail(1, 100).await.expect("test: tail clamp");
        assert_eq!(all.len(), 1);
        // Unknown seq -> error, never panic.
        let err = handle.tail(42, 5).await.expect_err("test: unknown seq");
        assert!(err.to_string().contains("no session #42"));
    }

    #[tokio::test]
    async fn poll_finished_reports_each_terminal_run_once() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("a", "task a", SubAgentStatus::Completed("done a".into())),
            make_run("b", "task b", SubAgentStatus::Running),
            make_run("c", "task c", SubAgentStatus::Failed("boom".into())),
            make_run("d", "task d", SubAgentStatus::Failed("killed by user".into())),
        ]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        let mut reported = std::collections::HashSet::new();

        let first = handle.poll_finished(&mut reported).await;
        // a (completed), c (failed), d (cancelled) — not b (running).
        assert_eq!(first.len(), 3);
        let statuses: Vec<ManagedStatus> = first.iter().map(|f| f.status).collect();
        assert!(statuses.contains(&ManagedStatus::Completed));
        assert!(statuses.contains(&ManagedStatus::Failed));
        assert!(statuses.contains(&ManagedStatus::Cancelled));
        // Summary carries the status payload.
        let a = first.iter().find(|f| f.run_id == "a").expect("test: run a");
        assert_eq!(a.summary, "done a");

        // A second poll with the same `reported` set surfaces nothing new.
        let second = handle.poll_finished(&mut reported).await;
        assert!(second.is_empty(), "each terminal run reports exactly once");
    }

    #[test]
    fn status_summary_empty_when_no_sessions() {
        assert_eq!(status_summary(&[]), "");
    }

    #[test]
    fn status_summary_counts_by_bucket() {
        let mk = |seq: u64, status: ManagedStatus| ManagedSessionView {
            id: SessionId::from_run_id(&format!("r{seq}")),
            seq,
            kind: super::super::model::ManagedKind::Agent,
            origin: super::super::model::SessionOrigin::User,
            title: "t".to_string(),
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
        };
        let views = vec![
            mk(1, ManagedStatus::Running),
            mk(2, ManagedStatus::Running),
            mk(3, ManagedStatus::Completed),
            mk(4, ManagedStatus::Failed),
            mk(5, ManagedStatus::Cancelled),
        ];
        let s = status_summary(&views);
        assert!(s.starts_with("sessions: "), "got {s}");
        assert!(s.contains("2 running"));
        assert!(s.contains("1 completed"));
        assert!(s.contains("1 failed"));
        assert!(s.contains("1 cancelled"));
    }
}

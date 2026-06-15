//! Thin chat-side handle over the shared `sessions_spawn` registry.
//!
//! [`ChatSessionsHandle`] is **not** a supervisor and does **not** own a second
//! registry. It wraps the single-source `Arc<RwLock<Vec<SubAgentRun>>>` that the
//! chat main loop builds once and shares with the four sessions tools
//! (`sessions_spawn`/`sessions_list`/`session_status`/`sessions_send`). The chat
//! `/sessions` and `/kill` commands read/act through this same Arc.
//!
//! The short display alias `#N` lives only here, in the chat main loop's
//! single-threaded state (`seq_map`); it is never shared across a lock.

use super::id::SessionId;
use super::model::{ManagedSessionView, project_run};
use crate::tools::sessions_spawn::SubAgentRun;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Chat-side handle over the shared sub-agent registry.
pub struct ChatSessionsHandle {
    /// The single-source `active_runs` registry (same `Arc` injected into the
    /// four sessions tools via `SessionsSpawnTool::new_with_registry`).
    runs: Arc<RwLock<Vec<SubAgentRun>>>,
    /// `#N` -> run UUID, assigned in first-seen order. Owned by the main loop
    /// (single-threaded), so a plain `Vec` with no lock is correct.
    seq_map: Vec<(u64, SessionId)>,
    /// Next sequence number to hand out.
    next_seq: u64,
}

impl ChatSessionsHandle {
    /// Build a handle over the supplied single-source registry Arc.
    #[must_use]
    pub const fn new(runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self {
            runs,
            seq_map: Vec::new(),
            next_seq: 1,
        }
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

    /// Refresh the `#N` -> UUID mapping from the live registry, assigning a new
    /// sequence to any run not seen before. Runs in first-seen order so display
    /// numbers stay stable across calls. Takes only a read lock.
    ///
    /// This is the single place seqs are minted: both `snapshot` (for `/sessions`)
    /// and `resolve_run_id` (for `/kill`) call it, so a freshly spawned run gets a
    /// `#N` even when the user kills it via `/bg` -> `/kill <N>` without first
    /// running `/sessions` (otherwise the seq map would be stale).
    async fn refresh_seqs(&mut self) -> Vec<SubAgentRun> {
        let runs: Vec<SubAgentRun> = self.runs.read().await.clone();
        for run in &runs {
            let _ = self.seq_for(&SessionId::from_run_id(&run.id));
        }
        runs
    }

    /// Snapshot all background sessions as chat-side views, assigning/refreshing
    /// display sequence numbers.
    pub async fn snapshot(&mut self) -> Vec<ManagedSessionView> {
        let runs = self.refresh_seqs().await;
        let mut views = Vec::with_capacity(runs.len());
        for run in &runs {
            // `refresh_seqs` already assigned a seq for every present run, so the
            // lookup below cannot fail; `seq_for` is idempotent regardless.
            let seq = self.seq_for(&SessionId::from_run_id(&run.id));
            views.push(project_run(run, seq));
        }
        views
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
            status,
            recipient: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::<HistoryEntry>::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: String::new(),
            spawn_depth: 0,
        }
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
}

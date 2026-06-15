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

    /// Snapshot all background sessions as chat-side views, assigning/refreshing
    /// display sequence numbers. Takes a read lock and projects each run.
    pub async fn snapshot(&mut self) -> Vec<ManagedSessionView> {
        // Collect (id, projected-without-seq inputs) under the lock, then assign
        // seqs outside to keep the lock scope minimal and avoid borrow conflicts.
        let runs: Vec<SubAgentRun> = self.runs.read().await.clone();
        let mut views = Vec::with_capacity(runs.len());
        for run in &runs {
            let id = SessionId::from_run_id(&run.id);
            let seq = self.seq_for(&id);
            views.push(project_run(run, seq));
        }
        views
    }

    /// Abort the session with the given display sequence `#N`.
    ///
    /// Mirrors the `sessions_spawn` kill path: abort the spawned task (if a
    /// handle exists) and mark the run `Failed("killed by user")`, which the UI
    /// projects to `Cancelled`. Returns an error (never panics) if the sequence
    /// or the run is unknown.
    pub async fn kill(&self, seq: u64) -> Result<()> {
        let id = self
            .id_for_seq(seq)
            .ok_or_else(|| anyhow!("no session #{seq}"))?
            .clone();
        let mut guard = self.runs.write().await;
        let run = guard
            .iter_mut()
            .find(|run| run.id == id.as_str())
            .ok_or_else(|| anyhow!("session #{seq} is no longer present"))?;
        if let Some(handle) = run.abort_handle.as_ref() {
            handle.abort();
        }
        run.status = crate::tools::sessions_spawn::SubAgentStatus::Failed("killed by user".into());
        Ok(())
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
    async fn kill_unknown_seq_errors() {
        let runs = Arc::new(RwLock::new(Vec::<SubAgentRun>::new()));
        let handle = ChatSessionsHandle::new(runs);
        let err = handle.kill(99).await.expect_err("test: unknown seq must error");
        assert!(err.to_string().contains("no session #99"));
    }

    #[tokio::test]
    async fn kill_marks_cancelled() {
        let runs = Arc::new(RwLock::new(vec![make_run("z", "long", SubAgentStatus::Running)]));
        let mut handle = ChatSessionsHandle::new(Arc::clone(&runs));
        // Establish the seq mapping.
        let _ = handle.snapshot().await;
        handle.kill(1).await.expect("test: kill #1");
        let guard = runs.read().await;
        match &guard[0].status {
            SubAgentStatus::Failed(msg) => assert_eq!(msg, "killed by user"),
            other => panic!("test: expected Failed, got {other:?}"),
        }
    }
}

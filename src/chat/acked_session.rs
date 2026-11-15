//! The single authoritative chat session: the reducer's acknowledged state.
//!
//! The reducer owns the session. After the effect executor has persisted (or,
//! for a non-persistent load, accepted) a reducer snapshot it publishes an
//! [`AckedSession`] on a `tokio::sync::watch` owned by
//! [`crate::chat::dispatcher::ChatDispatcher`]. Every reader in the run loop
//! goes through a [`SessionReader`], which accepts only values of the load it
//! expects, so a delayed publication of an earlier load is never read.

use crate::chat::session::ChatSession;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// Upper bound for one awaited acknowledgement (same bound as the ordered
/// provider-turn commit).
pub(crate) const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// A reducer snapshot acknowledged by the effect executor.
#[derive(Debug, Clone)]
pub struct AckedSession {
    pub session_id: String,
    pub load_epoch: u64,
    pub revision: u64,
    /// The canonical (unsanitized) runtime form of the session.
    pub session: Arc<ChatSession>,
}

impl AckedSession {
    /// The placeholder value of a dispatcher that has not been seeded.
    #[must_use]
    pub fn unseeded() -> Self {
        Self {
            session_id: String::new(),
            load_epoch: 0,
            revision: 0,
            session: Arc::new(ChatSession::new("", "")),
        }
    }

    fn same_load(&self, session_id: &str, load_epoch: u64) -> bool {
        self.load_epoch == load_epoch && self.session_id == session_id
    }
}

/// A reducer session snapshot carried by `Effect::SaveSession` and
/// `Effect::PublishSession`, stamped with the load and revision it belongs to.
#[derive(Debug, Clone)]
pub struct SessionCommit {
    pub session: ChatSession,
    pub load_epoch: u64,
    pub revision: u64,
}

impl SessionCommit {
    #[must_use]
    pub fn acked(&self) -> AckedSession {
        AckedSession {
            session_id: self.session.id.clone(),
            load_epoch: self.load_epoch,
            revision: self.revision,
            session: Arc::new(self.session.clone()),
        }
    }
}

#[cfg(test)]
impl From<ChatSession> for SessionCommit {
    fn from(session: ChatSession) -> Self {
        Self {
            session,
            load_epoch: 1,
            revision: 1,
        }
    }
}

impl std::ops::Deref for SessionCommit {
    type Target = ChatSession;

    fn deref(&self) -> &ChatSession {
        &self.session
    }
}

/// Answer to [`crate::chat::dispatcher::ChatDispatcher::dispatch_awaited`],
/// sent after every effect of the action has run.
#[derive(Debug, Clone)]
pub enum Ack {
    /// The action produced a new acknowledged revision.
    Published(AckedSession),
    /// The action was accepted and changed nothing durable.
    Unchanged(AckedSession),
    /// The reducer refused the action; nothing changed.
    Rejected(String),
}

/// Failure of an awaited acknowledgement. Every variant stops the chat as an
/// unconfirmed history commit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AckError {
    #[error("the session dispatcher is closed")]
    ChannelClosed,
    #[error("timed out waiting for the session acknowledgement")]
    Timeout,
    #[error("session persistence failed: {0}")]
    PersistenceFailed(String),
}

impl AckError {
    /// The failure class for the log. `PersistenceFailed` carries the store's
    /// error text, which is shown to the user but never logged.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ChannelClosed => "channel_closed",
            Self::Timeout => "timeout",
            Self::PersistenceFailed(_) => "persistence_failed",
        }
    }
}

type AckReply = Result<Ack, AckError>;

/// One-use reply slot travelling with an awaited action. Cloning an action
/// shares the slot; the first answer wins.
#[derive(Clone)]
pub struct AckTicket(Arc<parking_lot::Mutex<Option<oneshot::Sender<AckReply>>>>);

impl AckTicket {
    pub(crate) fn new() -> (Self, oneshot::Receiver<AckReply>) {
        let (tx, rx) = oneshot::channel();
        (Self(Arc::new(parking_lot::Mutex::new(Some(tx)))), rx)
    }

    pub(crate) fn answer(&self, reply: AckReply) {
        if let Some(tx) = self.0.lock().take()
            && tx.send(reply).is_err()
        {
            tracing::debug!("session acknowledgement receiver dropped before the answer");
        }
    }
}

impl std::fmt::Debug for AckTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AckTicket")
    }
}

/// Publishing side of the acknowledged-session watch.
#[derive(Clone)]
pub struct SessionPublisher(watch::Sender<AckedSession>);

impl SessionPublisher {
    #[must_use]
    pub fn new(seed: AckedSession) -> Self {
        Self(watch::Sender::new(seed))
    }

    /// Publish `acked` unless it belongs to an older load or an older
    /// revision of the current load than the value already published.
    pub(crate) fn publish(&self, acked: AckedSession) -> bool {
        self.0.send_if_modified(|current| {
            let newer_load = acked.load_epoch > current.load_epoch;
            let newer_revision = acked.load_epoch == current.load_epoch && acked.revision >= current.revision;
            if newer_load || newer_revision {
                *current = acked;
                true
            } else {
                false
            }
        })
    }

    #[must_use]
    pub fn current(&self) -> AckedSession {
        self.0.borrow().clone()
    }

    #[must_use]
    pub fn reader(&self) -> SessionReader {
        let current = self.current();
        SessionReader {
            rx: self.0.subscribe(),
            accepted: current,
        }
    }
}

/// Read-only view of the acknowledged session for one load.
pub struct SessionReader {
    rx: watch::Receiver<AckedSession>,
    accepted: AckedSession,
}

impl SessionReader {
    /// The newest acknowledged value of the expected load.
    #[must_use]
    pub fn acked(&self) -> AckedSession {
        let latest = self.rx.borrow();
        if latest.same_load(&self.accepted.session_id, self.accepted.load_epoch)
            && latest.revision >= self.accepted.revision
        {
            latest.clone()
        } else {
            self.accepted.clone()
        }
    }

    /// The newest acknowledged session of the expected load.
    #[must_use]
    pub fn session(&self) -> Arc<ChatSession> {
        self.acked().session
    }

    /// Adopt the acknowledged value of a new load (or a newer revision of the
    /// current one) returned by an awaited acknowledgement.
    pub fn adopt(&mut self, acked: AckedSession) {
        let newer_load = acked.load_epoch > self.accepted.load_epoch;
        let newer_revision = acked.same_load(&self.accepted.session_id, self.accepted.load_epoch)
            && acked.revision >= self.accepted.revision;
        if newer_load || newer_revision {
            self.accepted = acked;
        }
    }
}

/// Test support: a running dispatcher task over a real persistence-only
/// executor (real save, publish and acknowledgement) seeded from `state`.
#[cfg(test)]
pub(crate) struct TestSessionDispatcher {
    pub(crate) dispatcher: crate::chat::dispatcher::ChatDispatcher,
    pub(crate) commits: crate::chat::dispatcher::TurnCompletionSignal,
    shutdown: tokio_util::sync::CancellationToken,
    handle: tokio::task::JoinHandle<crate::chat::dispatcher::DispatcherStats>,
}

#[cfg(test)]
impl TestSessionDispatcher {
    /// Upper bound for joining the dispatcher task after shutdown.
    const STOP_DEADLINE: Duration = Duration::from_secs(10);

    pub(crate) fn spawn(memory: Arc<dyn crate::memory::Memory>, state: crate::chat::state::ChatState) -> Self {
        use crate::chat::dispatcher::{
            ChatDispatcher, EffectExecutor, PersistenceDeps, RuntimeDualWriteGuard, TurnCompletionSignal,
        };
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (dispatcher, action_rx) = ChatDispatcher::new_seeded(state.acked_session_seed());
        let executor = EffectExecutor::new_persistence_only(PersistenceDeps {
            memory,
            action_tx: dispatcher.sender(),
            dual_write_guard: RuntimeDualWriteGuard::new(),
        })
        .with_session_publisher(dispatcher.session_publisher());
        let commits = TurnCompletionSignal::new();
        let handle = crate::chat::dispatcher::spawn_dispatcher_task_with_signal(
            state,
            action_rx,
            shutdown.clone(),
            executor,
            Some(commits.clone()),
        );
        Self {
            dispatcher,
            commits,
            shutdown,
            handle,
        }
    }

    /// A state with `history` as its reducer history, seeded at load epoch 1.
    pub(crate) fn state_with_history(history: Vec<crate::providers::ChatMessage>) -> crate::chat::state::ChatState {
        let mut state = crate::chat::state::ChatState::new(
            Arc::from("provider"),
            Arc::from("model"),
            tokio_util::sync::CancellationToken::new(),
        );
        let _ = state.reduce(crate::chat::action::Action::SessionLoaded {
            session: ChatSession::new("provider", "model"),
            persist: false,
            leading_system_prompt: None,
        });
        state.session.history = history;
        state
    }

    /// Stop the dispatcher task and join it within [`Self::STOP_DEADLINE`];
    /// a task that does not stop in time is aborted and fails the test.
    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        drop(self.dispatcher);
        let mut handle = self.handle;
        match tokio::time::timeout(Self::STOP_DEADLINE, &mut handle).await {
            Ok(Ok(_stats)) => {}
            Ok(Err(error)) => tracing::warn!(error = %error, "test dispatcher join failed"),
            Err(_) => {
                handle.abort();
                panic!("the test dispatcher did not stop within STOP_DEADLINE");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upper bound for one dispatcher test and its cleanup.
    const TEST_DEADLINE: Duration = Duration::from_secs(20);

    fn acked(id: &str, load_epoch: u64, revision: u64, turns: usize) -> AckedSession {
        let mut session = ChatSession::new("p", "m");
        session.id = id.to_owned();
        for index in 0..turns {
            session.add_user_turn(&format!("turn {index}"));
        }
        AckedSession {
            session_id: id.to_owned(),
            load_epoch,
            revision,
            session: Arc::new(session),
        }
    }

    #[test]
    fn reader_ignores_a_publication_of_another_load() {
        let publisher = SessionPublisher::new(acked("a", 1, 0, 0));
        let mut reader = publisher.reader();
        assert!(publisher.publish(acked("b", 2, 0, 3)), "a newer load is published");
        assert_eq!(
            reader.session().id,
            "a",
            "a reader of load `a` ignores the publication of load `b`"
        );
        reader.adopt(acked("b", 2, 0, 3));
        assert_eq!(reader.session().turns.len(), 3, "an adopted load is read from then on");
    }

    #[test]
    fn publisher_refuses_an_older_load_or_revision() {
        let publisher = SessionPublisher::new(acked("a", 2, 5, 0));
        assert!(
            !publisher.publish(acked("old", 1, 9, 1)),
            "an older load is refused whatever its revision"
        );
        assert!(
            !publisher.publish(acked("a", 2, 4, 1)),
            "an older revision of the same load is refused"
        );
        assert!(
            publisher.publish(acked("a", 2, 6, 2)),
            "a newer revision of the same load is published"
        );
        assert_eq!(publisher.current().revision, 6, "the newest revision is current");
        assert_eq!(
            publisher.reader().session().turns.len(),
            2,
            "a new reader reads the newest revision"
        );
    }

    fn sqlite_memory() -> (tempfile::TempDir, Arc<dyn crate::memory::Memory>) {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
        (dir, memory)
    }

    #[tokio::test]
    async fn an_acknowledged_save_publishes_the_unsanitized_runtime_form_and_stores_a_sanitized_row() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let (_dir, memory) = sqlite_memory();
            let secret = "AKIAABCDEFGHIJKLMNOP";
            let mut state = TestSessionDispatcher::state_with_history(Vec::new());
            let _ = state.reduce(crate::chat::action::Action::RecordUserTurn(format!(
                "deploy key {secret}"
            )));
            let session_id = state.session.id.clone();
            let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
            let mut reader = harness.dispatcher.session_reader();

            let ack = harness
                .dispatcher
                .dispatch_awaited(
                    crate::chat::action::Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
                    &harness.commits,
                )
                .await
                .expect("test: mode change acknowledged");
            let Ack::Published(acked) = ack else {
                panic!("a changed mode must publish a saved revision: {ack:?}");
            };
            reader.adopt(acked.clone());
            let unchanged = harness
                .dispatcher
                .dispatch_awaited(
                    crate::chat::action::Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
                    &harness.commits,
                )
                .await
                .expect("test: repeated mode acknowledged");
            harness.stop().await;

            assert_eq!(acked.load_epoch, 1, "the save belongs to the seeded load");
            assert_eq!(acked.revision, 1, "the mode change is the first saved revision");
            assert!(
                matches!(unchanged, Ack::Unchanged(ref same) if same.revision == 1),
                "repeating the same mode saves nothing and answers the current revision: {unchanged:?}"
            );
            let published = reader.session();
            assert_eq!(
                published.mode,
                crate::agent::turn_contracts::ChatMode::Plan,
                "the reader sees the acknowledged mode"
            );
            assert!(
                published.turns.iter().any(|turn| turn.content.contains(secret)),
                "the acknowledged runtime form is the reducer's unsanitized snapshot"
            );
            let key = format!("{}:{session_id}", crate::chat::session::SESSION_MEMORY_PREFIX);
            let stored = memory
                .get(&key)
                .await
                .expect("test: read stored row")
                .expect("test: row stored");
            assert!(!stored.content.contains(secret), "only the persisted copy is sanitized");
        })
        .await
        .expect("an_acknowledged_save_publishes_the_unsanitized_runtime_form_and_stores_a_sanitized_row exceeded TEST_DEADLINE");
    }

    #[tokio::test]
    async fn a_load_while_generating_is_rejected_and_nothing_is_published() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let (_dir, memory) = sqlite_memory();
            let mut state = TestSessionDispatcher::state_with_history(Vec::new());
            state.control.generating = true;
            let harness = TestSessionDispatcher::spawn(memory, state);
            let before = harness.dispatcher.session_publisher().current();
            let mut other = ChatSession::new("provider", "model");
            other.id = "other-session".to_owned();

            let ack = harness
                .dispatcher
                .dispatch_awaited(
                    crate::chat::action::Action::SessionLoaded {
                        session: other,
                        persist: true,
                        leading_system_prompt: None,
                    },
                    &harness.commits,
                )
                .await
                .expect("test: load answered");
            let after = harness.dispatcher.session_publisher().current();
            harness.stop().await;

            assert!(matches!(ack, Ack::Rejected(_)), "{ack:?}");
            assert_eq!(
                after.session_id, before.session_id,
                "a rejected load leaves the published session"
            );
            assert_eq!(
                (after.load_epoch, after.revision),
                (before.load_epoch, before.revision),
                "a rejected load publishes no new epoch or revision"
            );
        })
        .await
        .expect("a_load_while_generating_is_rejected_and_nothing_is_published exceeded TEST_DEADLINE");
    }

    #[tokio::test]
    async fn a_turn_commit_is_saved_once_per_task() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let (_dir, memory) = sqlite_memory();
            let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
            let task_id = scheduler.enqueue("q", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
            let mut state = TestSessionDispatcher::state_with_history(Vec::new());
            let _ = state.reduce(crate::chat::action::Action::RecordUserTurn("q".to_owned()));
            let harness = TestSessionDispatcher::spawn(memory, state);
            let commit = crate::chat::action::Action::TurnCommitRequested {
                task_id,
                draft_id: None,
            };

            let first = harness
                .dispatcher
                .dispatch_awaited(commit.clone(), &harness.commits)
                .await
                .expect("test: first commit answered");
            let second = harness
                .dispatcher
                .dispatch_awaited(commit, &harness.commits)
                .await
                .expect("test: second commit answered");
            harness.stop().await;

            assert!(
                matches!(first, Ack::Published(ref acked) if acked.session.turns.len() == 1),
                "{first:?}"
            );
            assert!(matches!(second, Ack::Rejected(_)), "{second:?}");
        })
        .await
        .expect("a_turn_commit_is_saved_once_per_task exceeded TEST_DEADLINE");
    }

    #[tokio::test]
    async fn the_ordered_commit_marks_its_task_acknowledged_for_the_one_shot() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (_dir, memory) = sqlite_memory();
            let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
            let task_id = scheduler.enqueue("q", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
            let mut state = TestSessionDispatcher::state_with_history(Vec::new());
            let _ = state.reduce(crate::chat::action::Action::RecordUserTurn("q".to_owned()));
            let harness = TestSessionDispatcher::spawn(memory, state);
            let acknowledged_before = harness.commits.history_commit_acknowledged(task_id);

            let committed = crate::chat::provider_turns::dispatch_ordered_provider_turn_commit_and_wait(
                &harness.dispatcher,
                &harness.commits,
                task_id,
                crate::chat::provider_turns::OrderedCommitTerminal::TurnCommitRequested { draft_id: None },
                "q",
                "pong",
                false,
                false,
            )
            .await;
            let acknowledged_after = harness.commits.history_commit_acknowledged(task_id);
            let published = harness.dispatcher.session_publisher().current();
            harness.stop().await;

            assert!(!acknowledged_before, "nothing is acknowledged before the commit");
            assert_eq!(committed, Ok(()), "the ordered commit is acknowledged");
            assert!(acknowledged_after, "the acknowledged commit is the one-shot's success");
            let turns: Vec<&str> = published.session.turns.iter().map(|t| t.content.as_str()).collect();
            assert_eq!(
                turns,
                vec!["q", "pong"],
                "the published session holds the committed turn"
            );
        })
        .await
        .expect("the_ordered_commit_marks_its_task_acknowledged_for_the_one_shot exceeded its deadline");
    }
}

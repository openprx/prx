//! Acknowledged-session reads (R3 acceptance) over a real dispatcher, a real
//! persistence-only executor and real SQLite:
//!
//! - manual and preflight compaction over past turns that persistence
//!   sanitizes (a redacted secret, a truncated oversized turn) are
//!   acknowledged as `Compacted`; the published turns equal the reducer's turns
//!   and only the stored row is sanitized;
//! - reads right after the startup seed, a resume and `/mode` see the new
//!   value with no turn in between;
//! - a patch or save of an earlier load that arrives after a resume, branch or
//!   rewind is never read by the new load.
#![cfg(test)]

use super::*;
use crate::chat::acked_session::{Ack, AckedSession, TestSessionDispatcher};
use crate::chat::action::{Action, CompactReason};
use crate::chat::session::ChatSession;
use crate::chat::state::ChatState;

/// Upper bound for one test.
const DEADLINE: Duration = Duration::from_secs(20);
/// A credential the persistence redactor replaces.
const SECRET: &str = "AKIAABCDEFGHIJKLMNOP";
const SYSTEM: &str = "acked-reads system prompt";
/// Larger than the 10 KiB persistence cap, so the stored copy is truncated.
const OVERSIZED_BYTES: usize = 12 * 1024;

/// A summarizer that always answers with a well-formed safeguard summary.
struct SectionSummaryProvider;

#[async_trait::async_trait]
impl Provider for SectionSummaryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("## Decisions\n- ACKED_READS_SUMMARY\n## Open TODOs\n- continue\n## Critical Context\n- none".to_owned())
    }
}

fn compaction_config() -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 0,
        keep_recent_messages: 2,
        memory_flush: false,
        // Over budget with both oversized replies, within it once the older
        // one is compacted, so the kept tail is never trimmed.
        max_context_tokens: 4_000,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    }
}

fn sqlite_memory() -> (tempfile::TempDir, Arc<dyn Memory>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    (dir, memory)
}

/// The secret-bearing oversized assistant reply.
fn oversized_reply() -> String {
    format!(
        "rotated {SECRET} {}",
        "lorem ipsum dolor ".repeat(OVERSIZED_BYTES / 18 + 1)
    )
}

/// System prompt plus three exchanges. The first exchange (compacted by both
/// cases) and the newest one (kept by both) carry the secret and the
/// oversized reply.
fn state_with_sanitized_content() -> ChatState {
    let mut state = TestSessionDispatcher::state_with_history(Vec::new());
    let actions = [
        Action::SetLeadingSystemPrompt {
            content: SYSTEM.to_owned(),
        },
        Action::RecordUserTurn(format!("deploy key {SECRET}")),
        Action::RecordAssistantTurn {
            task_id: None,
            content: oversized_reply(),
        },
        Action::RecordUserTurn("q2".to_owned()),
        Action::RecordAssistantTurn {
            task_id: None,
            content: "a2".to_owned(),
        },
        Action::RecordUserTurn(format!("rotate {SECRET}")),
        Action::RecordAssistantTurn {
            task_id: None,
            content: oversized_reply(),
        },
    ];
    for action in actions {
        let _ = state.reduce(action);
    }
    state
}

fn turn_pairs(session: &ChatSession) -> Vec<(String, String)> {
    session
        .turns
        .iter()
        .map(|turn| (turn.role.clone(), turn.content.clone()))
        .collect()
}

/// A saved revision first: before the canonical runtime form, this
/// publication was the sanitized copy and the next guard missed the reducer.
async fn publish_saved_revision(harness: &TestSessionDispatcher) -> AckedSession {
    let ack = harness
        .dispatcher
        .dispatch_awaited(
            Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
            &harness.commits,
        )
        .await
        .expect("test: mode change acknowledged");
    let Ack::Published(acked) = ack else {
        panic!("a changed mode publishes a saved revision: {ack:?}");
    };
    acked
}

async fn stored_row(memory: &dyn Memory, session_id: &str) -> String {
    let key = format!("{}:{session_id}", crate::chat::session::SESSION_MEMORY_PREFIX);
    memory
        .get(&key)
        .await
        .expect("test: read stored row")
        .expect("test: row stored")
        .content
}

fn assert_runtime_form_published_and_row_sanitized(published: &ChatSession, row: &str) {
    assert!(
        published.turns.iter().any(|turn| turn.content == oversized_reply()),
        "the published runtime form keeps the oversized secret-bearing reply byte for byte"
    );
    assert!(!row.contains(SECRET), "the stored row redacts the secret");
    assert!(
        row.contains("truncated ("),
        "the stored row truncates the oversized reply"
    );
}

#[tokio::test]
async fn manual_compaction_over_sanitized_past_turns_is_acknowledged_in_the_runtime_form() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let config = compaction_config();
        let state = state_with_sanitized_content();
        let session_id = state.session.id.clone();
        let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
        let saved = publish_saved_revision(&harness).await;
        let mut source = vec![ChatMessage::system(SYSTEM)];
        source.extend(session_turns_to_history(&saved.session));
        let patch = build_chat_compaction_patch(
            &source,
            &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
            &SectionSummaryProvider,
            "summary-model",
            &config,
            None,
            crate::agent::compaction::MANUAL_COMPACT_TRIGGER,
        )
        .await
        .expect("test: the oversized history yields a summary patch");
        let mut provider_view = source.clone();

        let outcome = apply_chat_compaction_patch_and_sync(
            &mut provider_view,
            &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
            None,
            patch.clone(),
            &config,
            CompactReason::Manual,
            &harness.dispatcher,
            &harness.commits,
        )
        .await;
        let published = harness.dispatcher.session_publisher().current();
        harness.stop().await;

        assert!(
            matches!(outcome, ManualCompactOutcome::Compacted { .. }),
            "a guard built from the published runtime form matches the reducer: {outcome:?}"
        );
        assert_eq!(
            published.revision,
            saved.revision + 1,
            "the compaction is one saved revision"
        );
        let mut reference = state_with_sanitized_content();
        let _ = reference.reduce(Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan));
        let _ = reference.reduce(Action::HistoryCompactionPatchApplied {
            reason: CompactReason::Manual,
            patch,
            compaction_config: config.clone(),
        });
        assert_eq!(
            turn_pairs(&published.session),
            turn_pairs(&reference.build_session_snapshot()),
            "the published turns are the reducer's turns byte for byte"
        );
        // The summary's refresh aside lives only in the provider projection.
        let view: Vec<(&str, &str)> = provider_view
            .iter()
            .filter(|m| !crate::agent::turn_contracts::is_loop_written_user_note(&m.content))
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        let reducer_history: Vec<(&str, &str)> = reference
            .session
            .history
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            view, reducer_history,
            "the provider view without its loop notes is the compacted source the reducer holds"
        );
        assert!(
            provider_view.iter().any(|m| m
                .content
                .starts_with(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER)),
            "the provider projection carries the refresh aside"
        );
        assert_runtime_form_published_and_row_sanitized(
            &published.session,
            &stored_row(memory.as_ref(), &session_id).await,
        );
    })
    .await
    .expect("manual compaction test exceeded DEADLINE");
}

/// One summary compaction of the acknowledged turns, as `/compact` builds
/// it: the source is the system prompt plus the acknowledged turns.
async fn summary_compaction_of_acknowledged_turns(
    harness: &TestSessionDispatcher,
    config: &crate::config::AgentCompactionConfig,
    provider_view: &mut Vec<ChatMessage>,
) -> ManualCompactOutcome {
    let acked = harness.dispatcher.session_reader().acked();
    let mut source = vec![ChatMessage::system(SYSTEM)];
    source.extend(session_turns_to_history(&acked.session));
    let patch = build_chat_compaction_patch(
        &source,
        &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
        &SectionSummaryProvider,
        "summary-model",
        config,
        None,
        crate::agent::compaction::MANUAL_COMPACT_TRIGGER,
    )
    .await
    .expect("test: the history yields a summary patch");
    apply_chat_compaction_patch_and_sync(
        provider_view,
        &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
        None,
        patch,
        config,
        CompactReason::Manual,
        &harness.dispatcher,
        &harness.commits,
    )
    .await
}

/// Two summary compactions in a row against the same live reducer: the first
/// one's refresh aside stays in the provider projection, so the second
/// source, rebuilt from the acknowledged turns, describes the reducer history
/// and is compacted too. The stored row never holds the aside.
#[tokio::test]
async fn two_summary_compactions_in_a_row_are_both_compacted() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let config = compaction_config();
        let state = state_with_sanitized_content();
        let session_id = state.session.id.clone();
        let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
        let mut provider_view = vec![ChatMessage::system(SYSTEM)];
        let epoch_before = harness.dispatcher.session_reader().acked().compaction_epoch;

        let first = summary_compaction_of_acknowledged_turns(&harness, &config, &mut provider_view).await;
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        for (question, answer) in [("q4", "a4"), ("q5", "a5"), ("q6", "a6")] {
            let _ = harness
                .dispatcher
                .dispatch_awaited(Action::RecordUserTurn(question.to_owned()), &harness.commits)
                .await
                .expect("test: question recorded");
            let _ = harness
                .dispatcher
                .dispatch_awaited(
                    Action::RecordAssistantTurn {
                        task_id: None,
                        content: answer.to_owned(),
                    },
                    &harness.commits,
                )
                .await
                .expect("test: answer recorded");
            // Each exchange is committed (saved and published) as a turn is.
            let task_id = scheduler.enqueue(question, crate::chat::turn_scheduler::TurnPriority::Normal, 0);
            let _ = harness
                .dispatcher
                .dispatch_awaited(
                    Action::TurnCommitRequested {
                        task_id,
                        draft_id: None,
                    },
                    &harness.commits,
                )
                .await
                .expect("test: exchange committed");
        }
        let second = summary_compaction_of_acknowledged_turns(&harness, &config, &mut provider_view).await;
        let published = harness.dispatcher.session_publisher().current();
        let row = stored_row(memory.as_ref(), &session_id).await;
        let healthy = harness.commits.ensure_history_commits_healthy();
        harness.stop().await;

        assert!(
            matches!(first, ManualCompactOutcome::Compacted { .. }),
            "the first summary compaction is compacted: {first:?}"
        );
        assert!(
            matches!(second, ManualCompactOutcome::Compacted { .. }),
            "the second summary compaction is compacted: {second:?}"
        );
        assert_eq!(
            published.compaction_epoch,
            epoch_before + 2,
            "both compactions were adopted"
        );
        assert!(
            published.session.turns.iter().all(|turn| !turn
                .content
                .starts_with(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER)),
            "the refresh aside is never a session turn"
        );
        assert!(
            !row.contains(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER),
            "the stored row never holds the refresh aside"
        );
        assert!(healthy.is_ok(), "nothing is latched: {healthy:?}");
    })
    .await
    .expect("two_summary_compactions_in_a_row_are_both_compacted exceeded DEADLINE");
}

#[tokio::test]
async fn preflight_compaction_over_sanitized_past_turns_is_acknowledged_with_the_enriched_request_once() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let config = compaction_config();
        let state = state_with_sanitized_content();
        let session_id = state.session.id.clone();
        let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
        let saved = publish_saved_revision(&harness).await;
        let request = "q4";
        let enriched = ChatMessage::user(format!("[Memory context]\n{request}"));
        // The run loop queues the request before the preflight.
        let _ = harness
            .dispatcher
            .dispatch_or_log(Action::RecordUserTurn(request.to_owned()), "test.record_user_turn");
        let source = persisted_history_for_current_turn(&saved.session, SYSTEM, request);
        let mut provider_view = source.messages().to_vec();
        if let Some(last) = provider_view.last_mut() {
            *last = enriched.clone();
        }
        let patch = build_chat_compaction_patch(
            &provider_view,
            &source,
            &SectionSummaryProvider,
            "summary-model",
            &config,
            None,
            "chat_preflight",
        )
        .await
        .expect("test: the oversized history yields a summary patch");

        let outcome = apply_chat_compaction_patch_and_sync(
            &mut provider_view,
            &source,
            Some(enriched.clone()),
            patch.clone(),
            &config,
            CompactReason::ContextOverflow,
            &harness.dispatcher,
            &harness.commits,
        )
        .await;
        let published = harness.dispatcher.session_publisher().current();
        harness.stop().await;

        assert!(
            matches!(outcome, ManualCompactOutcome::Compacted { .. }),
            "the preflight patch over the persisted source is acknowledged: {outcome:?}"
        );
        let mut reference = state_with_sanitized_content();
        let _ = reference.reduce(Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan));
        let _ = reference.reduce(Action::RecordUserTurn(request.to_owned()));
        let _ = reference.reduce(Action::HistoryCompactionPatchApplied {
            reason: CompactReason::ContextOverflow,
            patch,
            compaction_config: config.clone(),
        });
        assert_eq!(
            turn_pairs(&published.session),
            turn_pairs(&reference.build_session_snapshot()),
            "the published turns are the reducer's turns byte for byte"
        );
        assert_eq!(
            provider_view.last().map(|m| m.content.as_str()),
            Some(enriched.content.as_str()),
            "the enriched request is the final provider message"
        );
        let request_count = provider_view
            .iter()
            .filter(|m| m.role == "user" && m.content.ends_with(request))
            .count();
        assert_eq!(request_count, 1, "the request occurs exactly once");
        assert_runtime_form_published_and_row_sanitized(
            &published.session,
            &stored_row(memory.as_ref(), &session_id).await,
        );
    })
    .await
    .expect("preflight compaction test exceeded DEADLINE");
}

#[tokio::test]
async fn reads_right_after_the_seed_a_resume_and_a_mode_change_see_the_new_value() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let mut state = TestSessionDispatcher::state_with_history(Vec::new());
        let _ = state.reduce(Action::RecordUserTurn("seeded turn".to_owned()));
        let seeded_id = state.session.id.clone();
        let harness = TestSessionDispatcher::spawn(memory, state);
        let mut reader = harness.dispatcher.session_reader();

        let seeded = reader.acked();
        let mut resumed = ChatSession::new("provider", "model");
        resumed.id = "resumed-session".to_owned();
        resumed.add_user_turn("resumed turn");
        let resume = harness
            .dispatcher
            .dispatch_awaited(
                Action::SessionLoaded {
                    session: resumed,
                    persist: false,
                    leading_system_prompt: None,
                },
                &harness.commits,
            )
            .await
            .expect("test: resume acknowledged");
        let Ack::Published(resume) = resume else {
            panic!("a resume publishes its load: {resume:?}");
        };
        reader.adopt(resume);
        let after_resume = reader.acked();
        let mode = harness
            .dispatcher
            .dispatch_awaited(
                Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
                &harness.commits,
            )
            .await
            .expect("test: mode change acknowledged");
        let after_mode = reader.session();
        harness.stop().await;

        assert_eq!(
            seeded.session_id, seeded_id,
            "the startup seed publishes the loaded session"
        );
        assert_eq!((seeded.load_epoch, seeded.revision), (1, 0), "the startup seed");
        assert_eq!(
            turn_pairs(&seeded.session),
            vec![("user".to_owned(), "seeded turn".to_owned())],
            "the startup seed carries the loaded turns"
        );
        assert_eq!(
            after_resume.session_id, "resumed-session",
            "the read after the resume sees the resumed session"
        );
        assert_eq!(after_resume.load_epoch, 2, "the resume is a new load");
        assert_eq!(
            turn_pairs(&after_resume.session),
            vec![("user".to_owned(), "resumed turn".to_owned())],
            "the read after the resume sees its turns"
        );
        assert!(matches!(mode, Ack::Published(_)), "{mode:?}");
        assert_eq!(
            after_mode.id, "resumed-session",
            "the read after /mode stays on the resumed session"
        );
        assert_eq!(
            after_mode.mode,
            crate::agent::turn_contracts::ChatMode::Plan,
            "the reader sees the new mode without adopting anything"
        );
    })
    .await
    .expect("immediate reads test exceeded DEADLINE");
}

/// How the new load arrives.
#[derive(Debug, Clone, Copy)]
enum NewLoad {
    Resume,
    Branch,
    Rewind,
}

/// The first session: system prompt plus two exchanges.
fn first_load_state() -> ChatState {
    let mut state = TestSessionDispatcher::state_with_history(Vec::new());
    for action in [
        Action::SetLeadingSystemPrompt {
            content: SYSTEM.to_owned(),
        },
        Action::RecordUserTurn("first q1".to_owned()),
        Action::RecordAssistantTurn {
            task_id: None,
            content: "first a1".to_owned(),
        },
        Action::RecordUserTurn("first q2".to_owned()),
        Action::RecordAssistantTurn {
            task_id: None,
            content: "first a2".to_owned(),
        },
    ] {
        let _ = state.reduce(action);
    }
    state
}

fn new_load(kind: NewLoad, first: &ChatSession) -> (ChatSession, bool) {
    match kind {
        NewLoad::Resume => {
            let mut other = ChatSession::new("provider", "model");
            other.id = "resumed-other".to_owned();
            other.add_user_turn("other q1");
            (other, false)
        }
        NewLoad::Branch => {
            let mut branch = first.clone();
            branch.id = "branch-of-first".to_owned();
            branch.turns.truncate(2);
            (branch, true)
        }
        NewLoad::Rewind => {
            let mut rewound = first.clone();
            rewound.turns.truncate(2);
            (rewound, true)
        }
    }
}

async fn assert_delayed_writes_of_the_first_load_are_never_read(kind: NewLoad) {
    let (_dir, memory) = sqlite_memory();
    let state = first_load_state();
    let first_history = state.session.history.clone();
    let first = state.build_session_snapshot();
    let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
    let mut reader = harness.dispatcher.session_reader();
    let first_acked = reader.acked();
    // A patch the first load's worker built and a commit its executor saved,
    // both arriving after the new load.
    let delayed_patch = crate::agent::turn_contracts::CompactionPatch {
        replacement: vec![ChatMessage::assistant("stale summary")],
        append_after: Vec::new(),
        guard: crate::agent::compaction::compaction_patch_guard_for(&first_history, 1, 3)
            .expect("test: guard over the first load"),
    };
    let mut stale_session = first.clone();
    stale_session.add_user_turn("stale turn of the first load");
    let delayed_commit = crate::chat::acked_session::SessionCommit {
        session: stale_session,
        load_epoch: first_acked.load_epoch,
        revision: first_acked.revision + 10,
        compaction_epoch: first_acked.compaction_epoch,
        open_request: None,
    };
    let (loaded, persist) = new_load(kind, &first);
    let expected = turn_pairs(&loaded);
    let expected_id = loaded.id.clone();

    let load = harness
        .dispatcher
        .dispatch_awaited(
            Action::SessionLoaded {
                session: loaded,
                persist,
                leading_system_prompt: None,
            },
            &harness.commits,
        )
        .await
        .expect("test: new load acknowledged");
    let Ack::Published(load) = load else {
        panic!("{kind:?}: the new load is published: {load:?}");
    };
    reader.adopt(load.clone());
    let patch_ack = harness
        .dispatcher
        .dispatch_awaited(
            Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch: delayed_patch,
                compaction_config: crate::config::AgentCompactionConfig::default(),
            },
            &harness.commits,
        )
        .await
        .expect("test: delayed patch answered");
    let late_executor =
        crate::chat::dispatcher::EffectExecutor::new_persistence_only(crate::chat::dispatcher::PersistenceDeps {
            memory: Arc::clone(&memory),
            action_tx: harness.dispatcher.sender(),
            dual_write_guard: crate::chat::dispatcher::RuntimeDualWriteGuard::new(),
        })
        .with_session_publisher(harness.dispatcher.session_publisher());
    late_executor
        .execute(crate::chat::state::Effect::SaveSession(delayed_commit))
        .await;
    let read = reader.acked();
    let current = harness.dispatcher.session_publisher().current();
    harness.stop().await;

    assert!(matches!(patch_ack, Ack::Rejected(_)), "{kind:?}: {patch_ack:?}");
    assert_eq!(load.load_epoch, first_acked.load_epoch + 1, "{kind:?}: a new load");
    for value in [&read, &current] {
        assert_eq!(value.session_id, expected_id, "{kind:?}");
        assert_eq!(
            (value.load_epoch, value.revision),
            (load.load_epoch, load.revision),
            "{kind:?}: neither the delayed patch nor the delayed save changed the new load"
        );
        assert_eq!(turn_pairs(&value.session), expected, "{kind:?}");
    }
}

#[tokio::test]
async fn delayed_writes_of_the_first_load_are_never_read_after_a_resume() {
    tokio::time::timeout(
        DEADLINE,
        assert_delayed_writes_of_the_first_load_are_never_read(NewLoad::Resume),
    )
    .await
    .expect("resume case exceeded DEADLINE");
}

#[tokio::test]
async fn delayed_writes_of_the_first_load_are_never_read_after_a_branch() {
    tokio::time::timeout(
        DEADLINE,
        assert_delayed_writes_of_the_first_load_are_never_read(NewLoad::Branch),
    )
    .await
    .expect("branch case exceeded DEADLINE");
}

#[tokio::test]
async fn delayed_writes_of_the_first_load_are_never_read_after_a_rewind() {
    tokio::time::timeout(
        DEADLINE,
        assert_delayed_writes_of_the_first_load_are_never_read(NewLoad::Rewind),
    )
    .await
    .expect("rewind case exceeded DEADLINE");
}

/// How the session a `/compact` runs on right afterwards was loaded.
#[derive(Debug, Clone, Copy)]
enum LoadBeforeCompact {
    StartupResume,
    Branch,
    Rewind,
}

/// A saved session with three exchanges, the oldest of which a forced
/// compaction replaces.
fn saved_three_exchanges() -> ChatSession {
    let mut session = ChatSession::new("provider", "model");
    session.id = "saved-three-exchanges".to_owned();
    for index in 1..=3 {
        session.add_user_turn(&format!("saved q{index}"));
        session.add_assistant_turn(&format!("saved a{index}"), Vec::new());
    }
    session
}

/// Load the session the way `kind` does (startup seed, or an awaited
/// `SessionLoaded` from `apply_chat_session_switch`), with the provider
/// view's system prompt, then run `/compact` on it with no turn in between:
/// the source is the provider view's system prompt plus the acknowledged
/// turns, exactly as `run_manual_compact` builds it.
async fn assert_compact_right_after_the_load_is_compacted(kind: LoadBeforeCompact) {
    let (_dir, memory) = sqlite_memory();
    let config = compaction_config();
    let saved = saved_three_exchanges();
    let (state, load) = match kind {
        LoadBeforeCompact::StartupResume => (
            crate::chat::bootstrap::initial_chat_dispatcher_state(
                "provider",
                "model",
                tokio_util::sync::CancellationToken::new(),
                &saved,
                Some(SYSTEM.to_owned()),
            ),
            None,
        ),
        LoadBeforeCompact::Branch => {
            let mut branch = saved.clone();
            branch.id = "branch-of-saved".to_owned();
            (first_load_state(), Some(branch))
        }
        LoadBeforeCompact::Rewind => {
            let mut rewound = saved.clone();
            rewound.turns.truncate(4);
            (first_load_state(), Some(rewound))
        }
    };
    let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
    let acked = match load {
        None => harness.dispatcher.session_publisher().current(),
        Some(session) => {
            let ack = harness
                .dispatcher
                .dispatch_awaited(
                    Action::SessionLoaded {
                        session,
                        persist: true,
                        leading_system_prompt: Some(SYSTEM.to_owned()),
                    },
                    &harness.commits,
                )
                .await
                .expect("test: load acknowledged");
            let Ack::Published(acked) = ack else {
                panic!("{kind:?}: a persisted load publishes its saved revision: {ack:?}");
            };
            acked
        }
    };
    let mut provider_view = crate::chat::session_store::provider_view_for_session(&acked.session, Some(SYSTEM));
    let source = provider_view.clone();
    let patch = build_chat_compaction_patch(
        &provider_view,
        &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
        &SectionSummaryProvider,
        "summary-model",
        &config,
        None,
        crate::agent::compaction::MANUAL_COMPACT_TRIGGER,
    )
    .await
    .expect("test: a forced compaction yields a summary patch");

    let outcome = apply_chat_compaction_patch_and_sync(
        &mut provider_view,
        &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
        None,
        patch,
        &config,
        CompactReason::Manual,
        &harness.dispatcher,
        &harness.commits,
    )
    .await;
    let published = harness.dispatcher.session_publisher().current();
    harness.stop().await;

    assert!(
        matches!(outcome, ManualCompactOutcome::Compacted { .. }),
        "{kind:?}: /compact right after the load describes the reducer history: {outcome:?}"
    );
    assert_eq!(
        published.revision,
        acked.revision + 1,
        "{kind:?}: the compaction is the next saved revision of the load"
    );
    let view_turns: Vec<(&str, &str)> = provider_view
        .iter()
        .skip(1)
        .filter(|message| !crate::agent::turn_contracts::is_loop_written_user_note(&message.content))
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect();
    let published_turns: Vec<(&str, &str)> = published
        .session
        .turns
        .iter()
        .map(|turn| (turn.role.as_str(), turn.content.as_str()))
        .collect();
    assert_eq!(
        view_turns, published_turns,
        "{kind:?}: the provider view holds the acknowledged turns after the compaction"
    );
}

#[tokio::test]
async fn compact_right_after_a_startup_resume_is_compacted() {
    tokio::time::timeout(
        DEADLINE,
        assert_compact_right_after_the_load_is_compacted(LoadBeforeCompact::StartupResume),
    )
    .await
    .expect("startup resume case exceeded DEADLINE");
}

#[tokio::test]
async fn compact_right_after_a_branch_is_compacted() {
    tokio::time::timeout(
        DEADLINE,
        assert_compact_right_after_the_load_is_compacted(LoadBeforeCompact::Branch),
    )
    .await
    .expect("branch case exceeded DEADLINE");
}

#[tokio::test]
async fn compact_right_after_a_rewind_is_compacted() {
    tokio::time::timeout(
        DEADLINE,
        assert_compact_right_after_the_load_is_compacted(LoadBeforeCompact::Rewind),
    )
    .await
    .expect("rewind case exceeded DEADLINE");
}

fn metered_usage() -> crate::llm::route_decision::MeteredTokenUsageRecord {
    crate::llm::route_decision::MeteredTokenUsageRecord {
        root_agent: None,
        settlement_id: None,
        provider: "provider".to_owned(),
        model: "model".to_owned(),
        prompt_tokens: 11,
        completion_tokens: 7,
        total_tokens: 18,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: crate::llm::route_decision::TokenUsageSource::Reported,
        cost_usd: None,
        settlement_class: None,
    }
}

/// Make every later write of the session row fail inside SQLite itself: the
/// real database refuses the statement, nothing in the process is mocked.
fn refuse_session_row_writes(workspace: &std::path::Path) {
    let connection =
        rusqlite::Connection::open(workspace.join("memory").join("brain.db")).expect("test: open the real database");
    connection
        .execute_batch(
            "CREATE TRIGGER test_refuse_insert BEFORE INSERT ON memories \
             BEGIN SELECT RAISE(ABORT, 'test: the database refuses this write'); END;
             CREATE TRIGGER test_refuse_update BEFORE UPDATE ON memories \
             BEGIN SELECT RAISE(ABORT, 'test: the database refuses this write'); END;",
        )
        .expect("test: install the refusing triggers");
}

#[tokio::test]
async fn a_failed_background_save_latches_one_terminal_failure_and_publishes_nothing() {
    tokio::time::timeout(DEADLINE, async {
        let (dir, memory) = sqlite_memory();
        let state = first_load_state();
        let mode = state.session.mode;
        let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);
        let before = harness.dispatcher.session_publisher().current();
        let healthy_before = harness.commits.ensure_history_commits_healthy().is_ok();
        refuse_session_row_writes(dir.path());

        // Not awaited: usage recorded outside any turn commit, as a
        // background summary or a worker compaction is.
        for _ in 0..2 {
            let sent = harness.dispatcher.dispatch_or_log(
                Action::ProviderUsageRecorded {
                    task_id: None,
                    usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                    record: metered_usage(),
                },
                "test.background_usage",
            );
            assert_eq!(
                sent,
                crate::chat::dispatcher::DispatchResult::Sent,
                "the background action reaches the dispatcher"
            );
        }
        // Same-channel FIFO: once this unchanged action is answered, both
        // saves above have run.
        let barrier = harness
            .dispatcher
            .dispatch_awaited(Action::ModeChanged(mode), &harness.commits)
            .await
            .expect("test: barrier answered");
        let after = harness.dispatcher.session_publisher().current();
        let failure = harness.commits.ensure_history_commits_healthy();
        let turns_cancelled = harness.commits.new_turn_cancellation().is_cancelled();
        harness.stop().await;

        assert!(healthy_before, "the chat is healthy before the database refuses writes");
        assert!(
            matches!(barrier, Ack::Unchanged(_)),
            "the barrier changes nothing: {barrier:?}"
        );
        let error = failure.expect_err("a failed background save stops the chat");
        let message = error.to_string();
        assert!(
            message.contains("session persistence failed"),
            "the terminal failure is the failed session save: {message}"
        );
        assert_eq!(
            message
                .matches("Chat stopped after an unconfirmed history commit")
                .count(),
            1,
            "two failed saves still give exactly one terminal failure: {message}"
        );
        assert!(turns_cancelled, "the latch cancels every running turn");
        assert_eq!(
            (after.load_epoch, after.revision),
            (before.load_epoch, before.revision),
            "a failed save publishes no revision"
        );
    })
    .await
    .expect("a_failed_background_save_latches_one_terminal_failure_and_publishes_nothing exceeded DEADLINE");
}

#[tokio::test]
async fn final_usage_that_the_dispatcher_cannot_accept_fails_before_any_commit() {
    tokio::time::timeout(DEADLINE, async {
        let (dispatcher, action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
        drop(action_rx);
        let commits = crate::chat::dispatcher::TurnCompletionSignal::new();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

        let without_usage = crate::chat::provider_turns::dispatch_final_usage(
            &dispatcher,
            &commits,
            &mut scheduler,
            None,
            None,
            "test.no_usage",
        )
        .await;
        let healthy_without_usage = commits.ensure_history_commits_healthy().is_ok();
        let with_usage = crate::chat::provider_turns::dispatch_final_usage(
            &dispatcher,
            &commits,
            &mut scheduler,
            None,
            Some(metered_usage()),
            "test.usage",
        )
        .await;

        assert_eq!(
            without_usage,
            Ok(()),
            "a turn without metered usage has nothing to send"
        );
        assert!(healthy_without_usage, "nothing is latched when there is no usage");
        assert_eq!(
            with_usage,
            Err(crate::chat::acked_session::AckError::ChannelClosed),
            "usage the dispatcher cannot accept is an error the caller must not commit past"
        );
        assert!(
            commits.ensure_history_commits_healthy().is_err(),
            "unaccepted usage latches the history commit as failed"
        );
    })
    .await
    .expect("final_usage_that_the_dispatcher_cannot_accept_fails_before_any_commit exceeded DEADLINE");
}

/// Fill the parked dispatcher's real bounded action channel with unchanged
/// metadata actions until a non-blocking send is refused; returns how many
/// were queued.
fn fill_action_channel(
    parked: &crate::chat::acked_session::ParkedSessionDispatcher,
    mode: crate::agent::turn_contracts::ChatMode,
) -> usize {
    let mut queued = 0;
    while parked.dispatcher.try_dispatch(Action::ModeChanged(mode)) == crate::chat::dispatcher::DispatchResult::Sent {
        queued += 1;
        assert!(
            queued <= crate::chat::dispatcher::ACTION_CHANNEL_CAPACITY,
            "the action channel accepts no more than its capacity"
        );
    }
    queued
}

#[tokio::test]
async fn the_admission_record_waits_for_a_full_channel_and_the_commit_includes_the_request() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("q", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let state = TestSessionDispatcher::state_with_history(Vec::new());
        let mode = state.session.mode;
        let parked = TestSessionDispatcher::parked(memory, state);
        let queued = fill_action_channel(&parked, mode);
        let dispatcher = parked.dispatcher.clone();
        let commits = parked.commits.clone();

        let mut admission = std::pin::pin!(crate::chat::provider_turns::record_user_turn_at_admission(
            &dispatcher,
            &commits,
            "the admitted request",
        ));
        let waits_while_full = futures::poll!(admission.as_mut()).is_pending();
        let harness = parked.start();
        let admitted = admission.await;
        let committed = crate::chat::provider_turns::dispatch_ordered_provider_turn_commit_and_wait(
            &harness.dispatcher,
            &harness.commits,
            task_id,
            crate::chat::provider_turns::OrderedCommitTerminal::TurnCommitRequested { draft_id: None },
            "the admitted request",
            "the reply",
            false,
            false,
        )
        .await;
        let published = harness.dispatcher.session_publisher().current();
        let healthy = harness.commits.ensure_history_commits_healthy();
        harness.stop().await;

        assert_eq!(
            queued,
            crate::chat::dispatcher::ACTION_CHANNEL_CAPACITY,
            "the channel was full before the admission record"
        );
        assert!(
            waits_while_full,
            "the admission record waits for capacity instead of being dropped"
        );
        assert!(
            admitted.is_ok(),
            "the record is accepted once the dispatcher drains: {admitted:?}"
        );
        assert_eq!(
            committed,
            Ok(()),
            "the completion commit (record_user: false) is acknowledged"
        );
        assert!(healthy.is_ok(), "nothing is latched: {healthy:?}");
        let turns: Vec<(&str, &str)> = published
            .session
            .turns
            .iter()
            .map(|turn| (turn.role.as_str(), turn.content.as_str()))
            .collect();
        assert_eq!(
            turns,
            vec![("user", "the admitted request"), ("assistant", "the reply")],
            "the acknowledged commit holds the request recorded under backpressure"
        );
    })
    .await
    .expect("the_admission_record_waits_for_a_full_channel_and_the_commit_includes_the_request exceeded DEADLINE");
}

#[tokio::test(start_paused = true)]
async fn an_admission_record_the_full_channel_never_accepts_fails_the_turn_once() {
    tokio::time::timeout(DEADLINE + crate::chat::acked_session::ACK_TIMEOUT, async {
        // The dispatcher task never runs here, so its executor never writes.
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::NoneMemory::new());
        let state = TestSessionDispatcher::state_with_history(Vec::new());
        let mode = state.session.mode;
        let parked = TestSessionDispatcher::parked(memory, state);
        let queued = fill_action_channel(&parked, mode);

        let admitted = crate::chat::provider_turns::record_user_turn_at_admission(
            &parked.dispatcher,
            &parked.commits,
            "the refused request",
        )
        .await;
        let latched = parked.commits.ensure_history_commits_healthy();
        let turns_cancelled = parked.commits.new_turn_cancellation().is_cancelled();
        // The dispatcher task never started; dropping the parked dispatcher
        // releases the queued actions.
        drop(parked);

        assert_eq!(
            queued,
            crate::chat::dispatcher::ACTION_CHANNEL_CAPACITY,
            "the channel was full before the admission record"
        );
        let message = admitted
            .expect_err("a record the channel never accepts fails the turn")
            .to_string();
        assert_eq!(
            message
                .matches("Chat stopped after an unconfirmed history commit")
                .count(),
            1,
            "the turn fails once, as an unconfirmed history commit: {message}"
        );
        assert!(
            message.contains("timed out"),
            "the failure is the bounded send's timeout: {message}"
        );
        assert!(
            latched.is_err(),
            "the failed admission record latches the history commit"
        );
        assert!(turns_cancelled, "the latch cancels every running turn");
    })
    .await
    .expect("an_admission_record_the_full_channel_never_accepts_fails_the_turn_once exceeded its deadline");
}

/// The request a failed turn recorded at admission.
const ABORTED_REQUEST: &str = "the request whose provider call failed";

/// Admission record, an in-turn compaction that saves the request, then a
/// draftless failure: the awaited settlement removes the request, saves and
/// publishes, so the acknowledged session and the stored row both lose it.
#[tokio::test]
async fn a_draftless_failure_after_an_in_turn_compaction_is_settled_saved_and_published() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let harness = TestSessionDispatcher::spawn(
            Arc::clone(&memory),
            TestSessionDispatcher::state_with_history(vec![ChatMessage::system("sys")]),
        );
        let admission = crate::chat::provider_turns::record_user_turn_at_admission(
            &harness.dispatcher,
            &harness.commits,
            ABORTED_REQUEST,
        )
        .await
        .expect("test: admission recorded");
        // The in-turn compaction keeps the recorded request and saves it.
        let source = vec![
            ChatMessage::system("sys"),
            crate::agent::turn_contracts::inbound_user_message(ABORTED_REQUEST),
        ];
        let guard = crate::agent::compaction::compaction_patch_guard_for(&source, 0, source.len())
            .expect("test: guard over the reducer history");
        let compacted = harness
            .dispatcher
            .dispatch_awaited(
                Action::HistoryCompactionPatchApplied {
                    reason: crate::chat::action::CompactReason::ContextOverflow,
                    patch: crate::agent::turn_contracts::CompactionPatch {
                        replacement: source.clone(),
                        append_after: Vec::new(),
                        guard,
                    },
                    compaction_config: compaction_config(),
                },
                &harness.commits,
            )
            .await
            .expect("test: compaction acknowledged");
        let Ack::Published(after_compaction) = compacted else {
            panic!("the in-turn compaction publishes a saved revision: {compacted:?}");
        };
        let row_after_compaction = stored_row(memory.as_ref(), &after_compaction.session_id).await;

        let settled =
            crate::chat::provider_turns::settle_aborted_turn(&harness.dispatcher, &harness.commits, Some(admission))
                .await;
        let row_after_settlement = stored_row(memory.as_ref(), &after_compaction.session_id).await;
        let healthy = harness.commits.ensure_history_commits_healthy();
        harness.stop().await;

        assert!(
            row_after_compaction.contains(ABORTED_REQUEST),
            "the compaction saved the admitted request before the failure"
        );
        let settled = settled.expect("the settlement is acknowledged");
        assert!(
            settled.revision > after_compaction.revision,
            "the settlement publishes a newer saved revision"
        );
        assert_eq!(
            settled.compaction_epoch, after_compaction.compaction_epoch,
            "the settled revision carries the in-turn compaction"
        );
        assert!(
            settled.session.turns.iter().all(|turn| turn.content != ABORTED_REQUEST),
            "the acknowledged session no longer holds the failed request: {:?}",
            turn_pairs(&settled.session)
        );
        assert!(
            !row_after_settlement.contains(ABORTED_REQUEST),
            "the stored row no longer holds the failed request"
        );
        assert!(healthy.is_ok(), "nothing is latched: {healthy:?}");
    })
    .await
    .expect("a_draftless_failure_after_an_in_turn_compaction_is_settled_saved_and_published exceeded DEADLINE");
}

/// A draft terminal that already rolled the request back (as a cancel that
/// removed the draft never does): the settlement removes nothing more, keeps
/// the earlier exchange, and still saves the corrected session.
#[tokio::test]
async fn a_settlement_after_the_draft_terminal_removes_nothing_more_and_saves() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let harness = TestSessionDispatcher::spawn(
            Arc::clone(&memory),
            TestSessionDispatcher::state_with_history(vec![ChatMessage::system("sys")]),
        );
        let dispatch = |action: Action| harness.dispatcher.dispatch_awaited(action, &harness.commits);
        dispatch(Action::RecordUserTurn("the earlier question".to_owned()))
            .await
            .expect("test: earlier question");
        dispatch(Action::RecordAssistantTurn {
            task_id: None,
            content: "the earlier answer".to_owned(),
        })
        .await
        .expect("test: earlier answer");
        let admission = crate::chat::action::AdmissionId::next();
        dispatch(Action::UserTurnAdmitted {
            admission,
            content: ABORTED_REQUEST.to_owned(),
        })
        .await
        .expect("test: admission");
        dispatch(Action::TurnStarted {
            draft_id: "draft-aborted".to_owned(),
            cancel: tokio_util::sync::CancellationToken::new(),
        })
        .await
        .expect("test: draft started");
        dispatch(Action::StreamFailed {
            draft_id: "draft-aborted".to_owned(),
            err: "refused".to_owned(),
            retryable: false,
        })
        .await
        .expect("test: draft failed");

        let settled =
            crate::chat::provider_turns::settle_aborted_turn(&harness.dispatcher, &harness.commits, Some(admission))
                .await;
        harness.stop().await;

        let settled = settled.expect("the settlement is acknowledged");
        assert_eq!(
            turn_pairs(&settled.session),
            vec![
                ("user".to_owned(), "the earlier question".to_owned()),
                ("assistant".to_owned(), "the earlier answer".to_owned()),
            ],
            "only the failed request is gone; the earlier exchange stays"
        );
        let row = stored_row(memory.as_ref(), &settled.session_id).await;
        assert!(
            !row.contains(ABORTED_REQUEST) && row.contains("the earlier answer"),
            "the stored row is the corrected session"
        );
    })
    .await
    .expect("a_settlement_after_the_draft_terminal_removes_nothing_more_and_saves exceeded DEADLINE");
}

/// Turn text the generic memory filter refuses: a 15-digit run and an SSN
/// shape. The acknowledged turn commit must save both byte for byte.
const IDENTITY_SHAPED_USER_TEXT: &str = "order 123456789012345 and SSN 123-45-6789";
const IDENTITY_SHAPED_REPLY_TEXT: &str = "recorded 123456789012345 for 123-45-6789";

#[tokio::test]
async fn a_turn_with_identity_shapes_is_acknowledged_and_saved_byte_identical() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue(
            IDENTITY_SHAPED_USER_TEXT,
            crate::chat::turn_scheduler::TurnPriority::Normal,
            0,
        );
        let state = TestSessionDispatcher::state_with_history(Vec::new());
        let session_id = state.session.id.clone();
        let harness = TestSessionDispatcher::spawn(Arc::clone(&memory), state);

        let committed = crate::chat::provider_turns::dispatch_ordered_provider_turn_commit_and_wait(
            &harness.dispatcher,
            &harness.commits,
            task_id,
            crate::chat::provider_turns::OrderedCommitTerminal::TurnCommitRequested { draft_id: None },
            IDENTITY_SHAPED_USER_TEXT,
            IDENTITY_SHAPED_REPLY_TEXT,
            false,
            true,
        )
        .await;
        let acknowledged = harness.commits.history_commit_acknowledged(task_id);
        harness.stop().await;

        assert_eq!(
            committed,
            Ok(()),
            "the turn commit with identity-shaped text is acknowledged"
        );
        assert!(acknowledged, "the acknowledged commit marks its task");
        let row = stored_row(memory.as_ref(), &session_id).await;
        let saved = ChatSession::from_json(&row).expect("test: the stored row is a chat session");
        let turns: Vec<(String, String)> = turn_pairs(&saved);
        assert_eq!(
            turns,
            vec![
                ("user".to_owned(), IDENTITY_SHAPED_USER_TEXT.to_owned()),
                ("assistant".to_owned(), IDENTITY_SHAPED_REPLY_TEXT.to_owned()),
            ],
            "the stored session keeps both turns byte for byte"
        );
    })
    .await
    .expect("a_turn_with_identity_shapes_is_acknowledged_and_saved_byte_identical exceeded DEADLINE");
}

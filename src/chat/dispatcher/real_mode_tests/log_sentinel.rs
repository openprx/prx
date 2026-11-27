//! Payload-free logging (R3): a TRACE capture over the TUI executor, the
//! non-TUI persistence-only executor and the test shadow executor running
//! `SaveSession`, `PublishSession`, `AutoTitleSession` and a compaction patch
//! with its `LogTrace`, plus a resume of a titled session, a paste and a
//! sentinel typed key by key, a stream retry and failure whose text quotes
//! the user, and a store error the database raises with the sentinel in it.
//! The sentinel in turn content, title, tool `args_preview`, input, provider
//! errors and store errors never reaches the captured output; the stored row
//! is sanitized. An actual compaction whose audit storage refuses every
//! write with the sentinel in the database error logs no content either.

use super::*;
use crate::chat::acked_session::{SessionCommit, SessionPublisher};
use crate::chat::session::{ChatTurn, ToolCallSummary};
use crate::providers::ChatMessage;
use std::sync::LazyLock;
use tracing_subscriber::fmt::MakeWriter;

const SENTINEL: &str = "SENTINEL-7f3a";
/// A credential the persistence redactor replaces.
const SECRET: &str = "AKIAABCDEFGHIJKLMNOP";
/// Upper bound for one test.
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Registered once and never dropped, interested in nothing: with two live
/// dispatchers `tracing-core` computes callsite interest from all of them, so
/// a sibling test reaching a line first cannot cache "never" for this capture.
static PLURAL_DISPATCHERS: LazyLock<tracing::Dispatch> =
    LazyLock::new(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));

/// A TRACE capture installed as this thread's default (the test runtime is
/// current-thread, so the dispatcher task runs under it too).
fn capture_logs() -> (CapturedLogs, tracing::subscriber::DefaultGuard) {
    LazyLock::force(&PLURAL_DISPATCHERS);
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn sqlite_memory() -> (TempDir, Arc<dyn Memory>) {
    let dir = TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    (dir, memory)
}

fn sentinel_session() -> ChatSession {
    let mut session = ChatSession::new("provider", "model");
    session.title = format!("title {SENTINEL}");
    session.turns = vec![
        ChatTurn {
            role: "user".to_owned(),
            content: format!("turn {SENTINEL} key {SECRET}"),
            timestamp: chrono::Utc::now(),
            tool_calls: vec![ToolCallSummary {
                name: "shell".to_owned(),
                args_preview: format!("{{\"command\":\"echo {SENTINEL}\"}}"),
                success: true,
                task_id: None,
                sequence: None,
            }],
            context_handoff: false,
            tool_evidence: None,
        },
        ChatTurn {
            role: "assistant".to_owned(),
            content: format!("reply {SENTINEL}"),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff: false,
            tool_evidence: None,
        },
        ChatTurn {
            role: "user".to_owned(),
            content: format!("again {SENTINEL}"),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff: false,
            tool_evidence: None,
        },
        ChatTurn {
            role: "assistant".to_owned(),
            content: format!("done {SENTINEL}"),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff: false,
            tool_evidence: None,
        },
    ];
    session
}

/// The reducer effects of a sentinel-bearing compaction patch (its
/// `LogTrace`, `SaveSession` and redraw), then `PublishSession` and
/// `AutoTitleSession`.
fn sentinel_effects() -> Vec<crate::chat::state::Effect> {
    let mut state = crate::chat::acked_session::TestSessionDispatcher::state_with_history(Vec::new());
    let _ = state.reduce(Action::SessionLoaded {
        session: sentinel_session(),
        persist: false,
        leading_system_prompt: None,
    });
    let history = state.session.history.clone();
    let patch = crate::agent::turn_contracts::CompactionPatch {
        replacement: vec![ChatMessage::assistant(format!("summary {SENTINEL}"))],
        append_after: Vec::new(),
        guard: crate::agent::compaction::compaction_patch_guard_for(&history, 0, 2)
            .expect("test: guard over the reducer history"),
    };
    let mut effects = state.reduce(Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::Manual,
        patch,
        compaction_config: crate::config::AgentCompactionConfig::default(),
    });
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, crate::chat::state::Effect::SaveSession(_))),
        "test: the patch applies and saves"
    );
    let commit = SessionCommit {
        session: state.build_session_snapshot(),
        load_epoch: state.session.load_epoch,
        revision: state.session.revision.saturating_add(1),
        compaction_epoch: state.session.compaction_epoch,
        open_request: None,
    };
    effects.push(crate::chat::state::Effect::PublishSession(commit));
    effects.push(crate::chat::state::Effect::AutoTitleSession(format!("auto {SENTINEL}")));
    effects
}

fn publisher() -> SessionPublisher {
    SessionPublisher::new(crate::chat::acked_session::AckedSession::unseeded())
}

async fn run_effects(executor: &EffectExecutor) {
    for effect in sentinel_effects() {
        executor.execute(effect).await;
    }
}

async fn stored_rows(memory: &dyn Memory) -> String {
    memory
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .expect("test: list stored rows")
        .into_iter()
        .map(|entry| entry.content)
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_sentinel(logs: &str, surface: &str) {
    assert!(
        !logs.contains(SENTINEL),
        "{surface}: the sentinel reached a log line:\n{logs}"
    );
    assert!(!logs.contains(SECRET), "{surface}: the secret reached a log line");
}

#[tokio::test]
async fn executors_never_log_session_content() {
    tokio::time::timeout(DEADLINE, async {
        let (logs, _guard) = capture_logs();
        let (_tui_dir, tui_memory) = sqlite_memory();
        let (deps, _action_rx, _hooks, _hook_dir) = build_deps(Arc::clone(&tui_memory), CancellationToken::new());
        let tui = EffectExecutor::new_with_deps(deps).with_session_publisher(publisher());
        run_effects(&tui).await;
        let tui_logs = logs.text();

        let (_plain_dir, plain_memory) = sqlite_memory();
        let (action_tx, _plain_rx) = mpsc::channel::<Action>(64);
        let persistence_only = EffectExecutor::new_persistence_only(PersistenceDeps {
            memory: Arc::clone(&plain_memory),
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
        })
        .with_session_publisher(publisher());
        run_effects(&persistence_only).await;

        run_effects(&EffectExecutor::new_shadow()).await;
        let all_logs = logs.text();

        assert!(
            tui_logs.contains("HistoryCompactionPatchApplied") && tui_logs.contains("AutoTitleSession effect"),
            "the capture records the executor's lines:\n{tui_logs}"
        );
        assert_eq!(
            all_logs.matches("HistoryCompactionPatchApplied").count(),
            3,
            "each executor logged the patch's LogTrace:\n{all_logs}"
        );
        assert_no_sentinel(&all_logs, "executors");
        for (memory, executor) in [(&tui_memory, "TUI"), (&plain_memory, "persistence-only")] {
            let rows = stored_rows(memory.as_ref()).await;
            assert!(rows.contains(SENTINEL), "{executor}: the session row is stored");
            assert!(!rows.contains(SECRET), "{executor}: the stored row is sanitized");
        }
    })
    .await
    .expect("executors_never_log_session_content exceeded DEADLINE");
}

#[tokio::test]
async fn resuming_a_titled_session_logs_no_title() {
    tokio::time::timeout(DEADLINE, async {
        let (logs, _guard) = capture_logs();
        let (dir, memory) = sqlite_memory();
        let session = sentinel_session();
        crate::chat::session_store::save_session(memory.as_ref(), &session)
            .await
            .expect("test: store the titled session");
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), dir.path().to_string_lossy());

        let resumed = crate::chat::run::resume_session(
            Some(&session.id),
            true,
            &memory,
            &fabric,
            &crate::config::Config::default(),
            "provider",
            "model",
        )
        .await
        .expect("test: resume the titled session");
        let text = logs.text();

        assert_eq!(resumed.id, session.id, "the titled session is the one resumed");
        assert!(text.contains("Resumed session"), "the resume line is captured:\n{text}");
        assert!(
            text.contains("title_len"),
            "the resume line carries only the title length:\n{text}"
        );
        assert_no_sentinel(&text, "resume");
    })
    .await
    .expect("resuming_a_titled_session_logs_no_title exceeded DEADLINE");
}

#[tokio::test]
async fn stream_failure_retry_and_store_error_logs_carry_no_free_text() {
    tokio::time::timeout(DEADLINE, async {
        let (logs, _guard) = capture_logs();
        let mut state = crate::chat::acked_session::TestSessionDispatcher::state_with_history(Vec::new());
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-sentinel".to_owned(),
            cancel: CancellationToken::new(),
        });
        let mut effects = state.reduce(Action::StreamRetryAttempt {
            attempt: 1,
            reason: format!("upstream echoed {SENTINEL}"),
        });
        effects.extend(state.reduce(Action::StreamFailed {
            draft_id: "draft-sentinel".to_owned(),
            err: format!("provider error quoting the user: {SENTINEL}"),
            retryable: false,
        }));
        let shadow = EffectExecutor::new_shadow();
        for effect in effects {
            shadow.execute(effect).await;
        }

        // A store error the real database raises with the sentinel in its
        // text: shown to the user through the latch, never logged.
        let (dir, memory) = sqlite_memory();
        let connection = rusqlite::Connection::open(dir.path().join("memory").join("brain.db"))
            .expect("test: open the real database");
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER test_refuse_insert BEFORE INSERT ON memories \
                 BEGIN SELECT RAISE(ABORT, 'refused {SENTINEL}'); END;"
            ))
            .expect("test: install the refusing trigger");
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let persistence_only = EffectExecutor::new_persistence_only(PersistenceDeps {
            memory,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
        })
        .with_session_publisher(publisher());
        persistence_only
            .execute(crate::chat::state::Effect::SaveSession(SessionCommit {
                session: ChatSession::new("provider", "model"),
                load_epoch: 1,
                revision: 1,
                compaction_epoch: 0,
                open_request: None,
            }))
            .await;
        let (persistence_error, publication) = persistence_only.take_action_results();
        let text = logs.text();

        assert!(
            text.contains("stream_retry_attempt") && text.contains("reason_len") && text.contains("reason_sha256"),
            "the retry line is captured with the reason's length and digest:\n{text}"
        );
        assert!(
            text.contains("stream_failed") && text.contains("err_len") && text.contains("err_sha256"),
            "the failure line is captured with the error's length and digest:\n{text}"
        );
        assert!(
            text.contains("SaveSession effect: store failed") && text.contains("error_len"),
            "the store failure line is captured with the error's length:\n{text}"
        );
        assert!(persistence_error.is_some(), "the refused save is a persistence error");
        assert!(publication.is_none(), "a refused save publishes nothing");
        assert_no_sentinel(&text, "failure, retry and store error");
    })
    .await
    .expect("stream_failure_retry_and_store_error_logs_carry_no_free_text exceeded DEADLINE");
}

/// A summarizer whose summary passes the section check.
struct SentinelSummaryProvider;

#[async_trait::async_trait]
impl Provider for SentinelSummaryProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities::default()
    }

    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok("## Decisions\n- d\n## Open TODOs\n- t\n## Constraints\n- c".to_owned())
    }

    async fn chat(
        &self,
        _: crate::providers::traits::ChatRequest<'_>,
        _: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatResponse> {
        Ok(crate::providers::traits::ChatResponse {
            text: Some("unused".to_owned()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An actual compaction (agent summary compaction, then the chat
/// compaction's audit) over a real SQLite store whose triggers refuse every
/// audit write with the sentinel in the database error: the compaction still
/// succeeds, and its failure lines carry the error's length and digest only.
#[tokio::test]
async fn a_compaction_whose_audit_storage_fails_logs_no_stored_content() {
    tokio::time::timeout(DEADLINE, async {
        let (logs, _guard) = capture_logs();
        let (dir, memory) = sqlite_memory();
        let connection = rusqlite::Connection::open(dir.path().join("memory").join("brain.db"))
            .expect("test: open the real database");
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER test_refuse_memories BEFORE INSERT ON memories \
                 BEGIN SELECT RAISE(ABORT, 'refused {SENTINEL}'); END; \
                 CREATE TRIGGER test_refuse_events BEFORE INSERT ON message_events \
                 BEGIN SELECT RAISE(ABORT, 'refused {SENTINEL}'); END;"
            ))
            .expect("test: install the refusing triggers");
        let envelope = crate::runtime::envelope::RuntimeEnvelope::agent("workspace-sentinel", "run-sentinel");
        let audit = crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
        let history = vec![
            ChatMessage::system("sys"),
            crate::agent::turn_contracts::inbound_user_message(&format!("old user {SENTINEL} ").repeat(60)),
            ChatMessage::assistant(format!("old assistant {SENTINEL} ").repeat(60)),
            crate::agent::turn_contracts::inbound_user_message(&format!("pending user {SENTINEL} ").repeat(60)),
            ChatMessage::assistant("recent assistant"),
        ];
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
        };

        let patch = crate::agent::compaction::build_configurable_compaction_patch(
            &history,
            &SentinelSummaryProvider,
            "model",
            &config,
            Some(&audit),
            "sentinel",
        )
        .await
        .expect("a refused audit write does not fail the compaction");
        crate::chat::compaction::persist_legacy_chat_compaction_audit(
            memory.as_ref(),
            &envelope,
            &history,
            &history,
            crate::chat::compaction::LegacyCompactionTokenMetadata {
                provider_token_estimate: 1,
                persisted_token_estimate: 1,
                enrichment_token_delta: 0,
            },
            "sentinel",
        )
        .await;
        let text = logs.text();

        assert!(patch.is_some(), "the summary compaction produced its patch");
        assert!(
            text.contains("failed to persist compaction summary memory") && text.contains("error_sha256"),
            "the agent compaction's refused audit write is logged by length and digest:\n{text}"
        );
        assert!(
            text.contains("failed to persist legacy chat compaction summary memory"),
            "the chat compaction's refused audit write is logged:\n{text}"
        );
        assert_no_sentinel(&text, "compaction audit storage failure");
    })
    .await
    .expect("a_compaction_whose_audit_storage_fails_logs_no_stored_content exceeded DEADLINE");
}

/// A real detached `switch` rollover whose handoff insert fails: a SQLite
/// trigger refuses every `context.switch.created` event with the sentinel in
/// the database error. The rollover degrades to a trim (no patch reaches the
/// session), and its failure line carries the error's length and digest only.
#[tokio::test]
async fn a_switch_rollover_whose_handoff_insert_fails_logs_no_stored_content() {
    tokio::time::timeout(DEADLINE, async {
        let (logs, _guard) = capture_logs();
        let (dir, memory) = sqlite_memory();
        let envelope =
            crate::runtime::envelope::RuntimeEnvelope::agent("workspace-switch-sentinel", "run-switch-sentinel");
        let audit = crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), "workspace-switch-sentinel");
        let source = vec![
            ChatMessage::system("sys"),
            crate::agent::turn_contracts::inbound_user_message(&format!("old user {SENTINEL} ").repeat(60)),
            ChatMessage::assistant(format!("old assistant {SENTINEL} ").repeat(60)),
            crate::agent::turn_contracts::inbound_user_message(&format!("older user {SENTINEL} ").repeat(60)),
            ChatMessage::assistant(format!("older assistant {SENTINEL} ").repeat(60)),
            crate::agent::turn_contracts::inbound_user_message(&format!("pending user {SENTINEL}")),
        ];
        for (index, message) in source.iter().skip(1).enumerate() {
            if message.role == "user" {
                fabric
                    .record_inbound_user_message(
                        envelope.message_scope(),
                        message.content.clone(),
                        Some(format!("switch-sentinel-{index}")),
                        None,
                    )
                    .await
                    .expect("test: record the user event");
            } else {
                fabric
                    .record_assistant_message(envelope.message_scope(), message.content.clone())
                    .await
                    .expect("test: record the assistant event");
            }
        }
        let connection = rusqlite::Connection::open(dir.path().join("memory").join("brain.db"))
            .expect("test: open the real database");
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER test_refuse_switch BEFORE INSERT ON message_events \
                 WHEN NEW.event_type = 'context.switch.created' \
                 BEGIN SELECT RAISE(ABORT, 'refused {SENTINEL}'); END;"
            ))
            .expect("test: install the refusing trigger");
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
        };
        let mut provider_view = source.clone();
        let mut guard_view = crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone());
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);

        let outcome = apply_redux_context_rollover(
            &SentinelSummaryProvider,
            &mut provider_view,
            &mut guard_view,
            "model",
            &config,
            Some(&audit),
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "sentinel_switch",
        )
        .await;
        drop(action_tx);
        let mut reached_session = false;
        while let Some(action) = action_rx.recv().await {
            reached_session |= matches!(action, Action::HistoryCompactionPatchApplied { .. });
        }
        let text = logs.text();

        assert_eq!(
            outcome,
            Ok(ContextRolloverOutcome {
                replacement_len: None,
                degraded: true,
            }),
            "a refused handoff insert degrades the rollover"
        );
        assert!(!reached_session, "no compaction reaches the session");
        assert!(
            text.contains("redux driver context rollover failed") && text.contains("error_sha256"),
            "the refused handoff insert is logged by length and digest:\n{text}"
        );
        assert_no_sentinel(&text, "switch rollover handoff insert failure");
    })
    .await
    .expect("a_switch_rollover_whose_handoff_insert_fails_logs_no_stored_content exceeded DEADLINE");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn typed_and_pasted_input_logs_no_characters() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let (logs, _guard) = capture_logs();
    let mut state = crate::chat::tui::TuiState::new("provider", "model");
    for ch in SENTINEL.chars() {
        let modifiers = if ch.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        let key = KeyEvent::new(KeyCode::Char(ch), modifiers);
        crate::chat::tui_loop::log_tui_input_event(&Event::Key(key));
        let _ = crate::chat::tui::dispatch_global_key(key, &mut state);
    }
    crate::chat::tui_loop::log_tui_input_event(&Event::Paste(format!("pasted {SENTINEL}")));
    let text = logs.text();

    assert_eq!(
        state.input.lines.first().map(String::as_str),
        Some(SENTINEL),
        "test: the keys reached the input"
    );
    assert!(
        text.contains("dispatch_global_key_entry") && text.contains("event_type=\"Key\"") && text.contains("Paste"),
        "the key and paste lines are captured:\n{text}"
    );
    assert_no_sentinel(&text, "input");
    let chars: Vec<char> = SENTINEL.chars().collect();
    for window in chars.windows(3) {
        let sequence: String = window.iter().collect();
        assert!(
            !text.contains(&sequence),
            "a typed character sequence `{sequence}` reached a log line:\n{text}"
        );
    }
    assert!(!text.contains("Char("), "no key event logs its character:\n{text}");
}

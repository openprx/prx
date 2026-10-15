#![cfg(test)]

use super::*;

/// **S3 T3-1 Step 3**: context overflow → automatic compact + retry while making progress → success.
///
/// The provider first sends StreamError::Provider("maximum context length exceeded") and
/// succeeds on the second attempt. The driver must: emit HistoryCompacted{ContextOverflow} → call the
/// stream API again → emit StreamCompleted.
#[tokio::test]
async fn t31_driver_context_overflow_triggers_compact_and_retries() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct OverflowOnceProvider {
        counter: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for OverflowOnceProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            _: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                    "Error: maximum context length exceeded for this model".into(),
                ))])
                .boxed()
            } else {
                stream::iter(vec![
                    Ok(StreamChunk::delta("recovered")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(OverflowOnceProvider {
        counter: Arc::new(AtomicUsize::new(0)),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-overflow", "history turn 29", false)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-overflow".into(),
                    history: (0..30)
                        .map(|index| crate::providers::traits::ChatMessage {
                            role: "user".into(),
                            content: format!("history turn {index}"),
                        })
                        .collect(),
                    compaction_guard_history: None,
                    compaction_config: None,
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut saw_compacted = false;
            let mut saw_completion = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::HistoryCompacted {
                        reason: crate::chat::action::CompactReason::ContextOverflow,
                    } => {
                        saw_compacted = true;
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("recovered"), "want 'recovered' got {final_text:?}");
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should retry on overflow, not fail: {err}");
                    }
                    _ => {}
                }
            }
            assert!(saw_compacted, "must emit HistoryCompacted on overflow");
            assert!(saw_completion, "must complete after compact+retry");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// A `switch` preflight that cannot resolve provenance answers the turn on a
/// lossy trim. It must say so: the previous behaviour ended the turn with a
/// loud error, and replacing that with silence is how a session quietly
/// forgets its own history while looking healthy.
#[tokio::test]
async fn redux_driver_switch_degradation_tells_the_user_context_was_dropped() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct PlainProvider;
    #[async_trait]
    impl Provider for PlainProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok("SUMMARY_MUST_NOT_BE_USED".to_string())
        }
        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            _: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            stream::iter(vec![
                Ok(StreamChunk::delta("answered anyway")),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(PlainProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            // Switch mode with no durable transcript scope: provenance can never
            // resolve, so the rollover produces no patch at all.
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Switch,
                reserve_tokens: 10,
                keep_recent_messages: 2,
                memory_flush: false,
                max_context_tokens: 160,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..24 {
                history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
            }

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-switch-degraded",
                history
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .expect("original user input")
                    .content
                    .as_str(),
                false,
            )
            .await
            .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-switch-degraded".into(),
                    history,
                    compaction_guard_history: None,
                    compaction_config: Some(config),
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut dropped = None;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::HistoryCompactionDegraded { dropped_messages, .. } => dropped = Some(dropped_messages),
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("answered anyway"), "got {final_text:?}");
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("degrading must not end the turn: {err}")
                    }
                    _ => {}
                }
            }
            let dropped = dropped.expect("a lossy switch fallback must announce itself to the user");
            assert!(dropped > 0, "the notice must report the messages the trim removed");

            // The reducer turns that action into exactly one visible line.
            let mut reducer_state =
                crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let effects = reducer_state.reduce(Action::HistoryCompactionDegraded {
                reason: crate::chat::action::CompactReason::ContextOverflow,
                dropped_messages: dropped,
            });
            assert!(
                effects.iter().any(|effect| matches!(
                    effect,
                    crate::chat::state::Effect::SurfaceNotice { text } if text.contains("lossy")
                )),
                "the degradation action must reach the user as a notice"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn redux_driver_preflight_uses_provider_summary_before_first_stream_request() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct BudgetCaptureProvider {
        first_tokens: Arc<AtomicUsize>,
        summary_calls: Arc<AtomicUsize>,
        first_history: Arc<Mutex<Option<Vec<PMsg>>>>,
    }
    #[async_trait]
    impl Provider for BudgetCaptureProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(
                    "PROVIDER_SUMMARY_MARKER\n## Decisions\n- keep decision\n## Open TODOs\n- keep todo\n## Constraints\n- keep rule"
                        .to_string(),
                )
        }
        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            messages: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            self.first_tokens
                .compare_exchange(
                    0,
                    crate::agent::context_budget::measure_history_tokens(messages),
                    AtomicOrdering::SeqCst,
                    AtomicOrdering::SeqCst,
                )
                .ok();
            let mut guard = self.first_history.lock();
            if guard.is_none() {
                *guard = Some(messages.to_vec());
            }
            stream::iter(vec![
                Ok(StreamChunk::delta("budget-ok")),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let first_tokens = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let first_history = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(BudgetCaptureProvider {
        first_tokens: Arc::clone(&first_tokens),
        summary_calls: Arc::clone(&summary_calls),
        first_history: Arc::clone(&first_history),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Safeguard,
                reserve_tokens: 10,
                keep_recent_messages: 2,
                memory_flush: false,
                max_context_tokens: 160,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..24 {
                history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
            }

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-budget-preflight",
                history
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .expect("original user input")
                    .content
                    .as_str(),
                false,
            )
            .await
            .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-budget-preflight".into(),
                    history,
                    compaction_guard_history: None,
                    compaction_config: Some(config),
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut saw_summary_patch = false;
            let mut saw_feedback = false;
            let mut saw_context_update = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::HistoryCompactionPatchApplied { patch, .. } => {
                        saw_summary_patch = true;
                        assert!(
                            patch
                                .replacement
                                .iter()
                                .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
                            "summary patch must carry provider summary marker"
                        );
                    }
                    Action::HistoryCompacted { .. } => {
                        panic!("preflight summary path must not dispatch lossy HistoryCompacted");
                    }
                    Action::ContextWindowUpdated {
                        used_context_tokens: Some(used),
                        max_context_tokens: Some(max),
                    } => {
                        saw_context_update = true;
                        assert_eq!(max, 160);
                        assert!(
                            used <= 150,
                            "context window update must reflect compacted budget: {used}"
                        );
                    }
                    Action::SystemMessageAdded { text } => {
                        saw_feedback = true;
                        assert!(
                            text.contains("Compacted context") || text.contains("Context already compact"),
                            "preflight feedback should describe compaction result: {text}"
                        );
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("budget-ok"));
                        break;
                    }
                    Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                    _ => {}
                }
            }
            assert!(saw_summary_patch, "preflight must dispatch exact summary patch");
            assert!(saw_feedback, "preflight must dispatch user-visible compact feedback");
            assert!(
                saw_context_update,
                "preflight must update context-window UI metadata after compaction"
            );
            assert_eq!(summary_calls.load(AtomicOrdering::SeqCst), 1);
            let captured = first_history.lock().clone().expect("first stream history");
            assert!(
                captured
                    .iter()
                    .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
                "first stream request must contain provider summary marker"
            );
            assert!(
                captured
                    .iter()
                    .all(|message| !message.content.contains("compact-in-place")),
                "first stream request must not contain old lossy compact marker"
            );
            assert!(
                first_tokens.load(AtomicOrdering::SeqCst) <= 150,
                "redux first provider call must be below literal hard limit 150 tokens"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn resumed_compacted_session_does_not_call_summarizer_again() {
    use crate::chat::state::{ChatState, Effect};
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct ResumeProvider {
        summary_calls: Arc<AtomicUsize>,
        streamed_history: Arc<Mutex<Option<Vec<PMsg>>>>,
    }

    #[async_trait]
    impl Provider for ResumeProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok("UNEXPECTED_RESUME_SUMMARY".to_string())
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            messages: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            *self.streamed_history.lock() = Some(messages.to_vec());
            stream::iter(vec![
                Ok(StreamChunk::delta("resume-ok")),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let current_question = "continue from the compacted summary";
    fn durable_test_turns(messages: &[PMsg]) -> Vec<crate::chat::session::ChatTurn> {
        let timestamp = chrono::Utc::now();
        messages
            .iter()
            .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
            .map(|message| crate::chat::session::ChatTurn {
                role: message.role.clone(),
                content: message.content.clone(),
                timestamp,
                tool_calls: Vec::new(),
            })
            .collect()
    }

    let mut state = ChatState::new(
        Arc::from("test-prov"),
        Arc::from("test-model"),
        CancellationToken::new(),
    );
    state.session.history = vec![
        PMsg::system("sys"),
        PMsg::user(format!("old user {}", "long context ".repeat(80))),
        PMsg::assistant(format!("old assistant {}", "long context ".repeat(80))),
    ];
    state.session.turns = durable_test_turns(&state.session.history);
    let _ = state.reduce(Action::RecordUserTurn(current_question.to_string()));
    let guard = crate::agent::compaction::compaction_patch_guard_for(&state.session.history, 1, 3)
        .expect("test: compaction guard");
    let patch = crate::agent::turn_contracts::CompactionPatch {
        range_start: 1,
        range_end: 3,
        replacement: vec![PMsg::assistant(
            "[Context compacted at test. Summary: DURABLE_SUMMARY_MARKER]",
        )],
        append_after: vec![PMsg::user("[Post-compaction context refresh]\nre-read")],
        guard,
    };
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 170,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    let snapshot = state
        .reduce(Action::HistoryCompactionPatchApplied {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            patch,
            compaction_config: config.clone(),
        })
        .into_iter()
        .find_map(|effect| match effect {
            Effect::SaveSession(session) => Some(session),
            _ => None,
        })
        .expect("compaction patch must persist a session snapshot");

    let mut resumed = ChatState::new(
        Arc::from("test-prov"),
        Arc::from("test-model"),
        CancellationToken::new(),
    );
    let _ = resumed.reduce(Action::SessionLoaded(snapshot));
    let resumed_pairs = resumed
        .session
        .history
        .iter()
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        resumed_pairs,
        vec![
            (
                "assistant",
                "[Context compacted at test. Summary: DURABLE_SUMMARY_MARKER]"
            ),
            ("user", current_question),
        ],
        "resume must rebuild the already-compacted durable history, not the old source turns"
    );

    let summary_calls = Arc::new(AtomicUsize::new(0));
    let streamed_history = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(ResumeProvider {
        summary_calls: Arc::clone(&summary_calls),
        streamed_history: Arc::clone(&streamed_history),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-resume-compacted",
                resumed
                    .session
                    .history
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .expect("original resumed user input")
                    .content
                    .as_str(),
                false,
            )
            .await
            .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-resume-compacted".to_string(),
                    history: resumed.session.history.clone(),
                    compaction_guard_history: None,
                    compaction_config: Some(config),
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut saw_completion = false;
            for _ in 0..8 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("driver action should arrive");
                match action {
                    Action::HistoryCompactionPatchApplied { .. } => {
                        panic!("resumed compacted history must not dispatch another summary patch");
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("resume-ok"));
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                    _ => {}
                }
            }

            assert!(
                saw_completion,
                "driver must stream normally from compacted resume history"
            );
            assert_eq!(
                summary_calls.load(AtomicOrdering::SeqCst),
                0,
                "resuming an already-compacted session must not invoke the summarizer again"
            );
            let streamed = streamed_history
                .lock()
                .clone()
                .expect("provider-bound history should be captured");
            assert_eq!(
                streamed
                    .iter()
                    .map(|message| (message.role.as_str(), message.content.as_str()))
                    .collect::<Vec<_>>(),
                resumed_pairs,
                "provider should receive the compacted resume shape"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn redux_driver_preflight_provider_history_keeps_real_user_question_last_after_compaction() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex;

    struct CaptureStreamProvider {
        captured_history: Arc<Mutex<Option<Vec<PMsg>>>>,
    }

    #[async_trait]
    impl Provider for CaptureStreamProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok("## Decisions\n- preserve Redux compacted context\n## Open TODOs\n- answer the latest user question\n## Critical Context\n- ISS-037 Redux capture test".to_string())
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            messages: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let last_user = messages
                .iter()
                .rfind(|message| message.role == "user")
                .map(|message| message.content.clone())
                .unwrap_or_default();
            *self.captured_history.lock() = Some(messages.to_vec());
            stream::iter(vec![
                Ok(StreamChunk::delta(format!("answer bound to: {last_user}"))),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let current_question = "What should ISS-037 answer now?";
    let captured_history = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(CaptureStreamProvider {
        captured_history: Arc::clone(&captured_history),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Safeguard,
                reserve_tokens: 10,
                keep_recent_messages: 1,
                memory_flush: false,
                max_context_tokens: 180,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..24 {
                history.push(PMsg::user(format!("old turn {i} {}", "long context ".repeat(40))));
            }
            history.push(PMsg::user(current_question));

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-iss-037-redux",
                history
                    .iter()
                    .rev()
                    .find(|message| message.role == "user")
                    .expect("original user input")
                    .content
                    .as_str(),
                false,
            )
            .await
            .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-iss-037-redux".to_string(),
                    history,
                    compaction_guard_history: None,
                    compaction_config: Some(config),
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut saw_summary_patch = false;
            let mut saw_completion = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("driver action should arrive");
                match action {
                    Action::HistoryCompactionPatchApplied { .. } => {
                        saw_summary_patch = true;
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(
                            final_text.contains(current_question),
                            "assistant response must be bound to the real current user question"
                        );
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                    _ => {}
                }
            }

            assert!(saw_summary_patch, "preflight must apply a summary compaction patch");
            assert!(saw_completion, "driver must complete");
            let captured = captured_history
                .lock()
                .clone()
                .expect("provider-bound history should be captured");
            let last = captured.last().expect("provider-bound history should not be empty");
            assert_eq!(last.role, "user");
            assert_eq!(last.content, current_question);
            let refresh_index = captured
                .iter()
                .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
                .expect("refresh marker should be present");
            assert!(
                refresh_index + 1 < captured.len(),
                "refresh marker must not be the trailing provider-bound message"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn redux_switch_without_exact_provenance_degrades_instead_of_failing_the_turn() {
    let provider = MockEnvProvider::from_env();
    let mut history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("old user ".repeat(120)),
        crate::providers::ChatMessage::assistant("old assistant ".repeat(120)),
        crate::providers::ChatMessage::user("current user"),
    ];
    let original = history
        .iter()
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect::<Vec<_>>();
    let mut guard_history = history.clone();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let (action_tx, mut action_rx) = mpsc::channel(2);

    let result = apply_redux_context_rollover(
        &provider,
        &mut history,
        &mut guard_history,
        "model",
        &config,
        None,
        &action_tx,
        crate::chat::action::CompactReason::ContextOverflow,
        "test_switch_failure",
    )
    .await;

    assert_eq!(
        result,
        Ok(ContextRolloverOutcome {
            replacement_len: None,
            degraded: true,
        }),
        "a switch without exact provenance must hand the caller its trim fallback (flagged lossy), not end the turn"
    );
    assert_eq!(
        history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        original,
        "a rollover that produced no patch must leave history untouched for the caller to trim"
    );
    assert!(
        action_rx.try_recv().is_err(),
        "degrading must not emit a terminal StreamFailed for the draft"
    );
}

#[tokio::test]
async fn redux_compaction_guard_uses_persisted_source_when_provider_history_is_enriched() {
    use crate::chat::state::{ChatState, Effect};
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct SummaryProvider;

    #[async_trait]
    impl Provider for SummaryProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, prompt: &str, _: &str, _: f64) -> anyhow::Result<String> {
            assert!(
                !prompt.contains("HIDDEN_FILE_SENTINEL"),
                "persisted-source compaction must not summarize hidden enrichment"
            );
            Ok(
                "PERSISTED_SOURCE_SUMMARY\n## Decisions\n- original transcript only\n## Open TODOs\n- continue"
                    .to_string(),
            )
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            _: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let original_history = vec![
        PMsg::system("sys"),
        PMsg::user(format!("old visible user @file.txt {}", "visible ".repeat(60))),
        PMsg::assistant(format!("old assistant {}", "visible ".repeat(60))),
        PMsg::user(format!("another old user {}", "visible ".repeat(60))),
        PMsg::assistant(format!("another old assistant {}", "visible ".repeat(60))),
        PMsg::user("current visible user"),
    ];
    let mut driver_history = original_history.clone();
    let Some(enriched_old_user) = driver_history.get_mut(1) else {
        panic!("test fixture must include old user turn");
    };
    enriched_old_user.content.push_str(&format!(
        "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
        "hidden ".repeat(120)
    ));
    let enriched_before_compaction = driver_history.clone();
    let mut compaction_guard_history = original_history.clone();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 5,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 220,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

    let replacement_len = apply_redux_context_rollover(
        &SummaryProvider,
        &mut driver_history,
        &mut compaction_guard_history,
        "model",
        &config,
        None,
        &action_tx,
        crate::chat::action::CompactReason::ContextOverflow,
        "test_persisted_guard_source",
    )
    .await
    .unwrap()
    .replacement_len
    .expect("summary patch must apply");
    assert_eq!(replacement_len, 1);

    let patch = match action_rx.recv().await.expect("patch action") {
        Action::HistoryCompactionPatchApplied { patch, .. } => patch,
        other => panic!("expected summary patch action, got {other:?}"),
    };
    let reducer_history = original_history
        .get(..original_history.len() - 1)
        .expect("reducer history fixture")
        .to_vec();
    assert!(
        crate::agent::compaction::compaction_patch_guard_matches(&reducer_history, &patch.guard),
        "patch guard must validate against reducer history before ordered user-turn commit"
    );
    assert!(
        !crate::agent::compaction::compaction_patch_guard_matches(&enriched_before_compaction, &patch.guard),
        "old enriched guard source would have mismatched the reducer"
    );

    let mut reducer_state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    reducer_state.session.history = reducer_history;
    let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });
    assert!(
        effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
        "matching persisted guard must take the exact patch path, not plain-trim fallback"
    );
    assert!(
        effects.iter().all(|effect| {
            !matches!(
                effect,
                Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg
                } if msg.contains("guard mismatch")
            )
        }),
        "persisted guard source must not log a guard mismatch"
    );
    let _ = reducer_state.reduce(Action::RecordUserTurn("current visible user".to_string()));
    assert_eq!(
        driver_history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        reducer_state
            .session
            .history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        "GP-6: driver history must match reducer history after persisted-source patch"
    );
    assert!(
        reducer_state
            .session
            .history
            .iter()
            .all(|message| !message.content.contains("HIDDEN_FILE_SENTINEL")),
        "hidden enrichment must not be persisted through the compaction summary"
    );
}

#[tokio::test]
async fn redux_compaction_guard_source_is_inert_when_provider_history_is_not_enriched() {
    use crate::chat::state::{ChatState, Effect};
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct SummaryProvider;

    #[async_trait]
    impl Provider for SummaryProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok("EMPTY_ENRICHMENT_SUMMARY\n## Decisions\n- unchanged".to_string())
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn stream_chat_with_history(
            &self,
            _: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let original_history = vec![
        PMsg::system("sys"),
        PMsg::user(format!("old user {}", "visible ".repeat(60))),
        PMsg::assistant(format!("old assistant {}", "visible ".repeat(60))),
        PMsg::user("current user"),
    ];
    let mut driver_history = original_history.clone();
    let mut compaction_guard_history = original_history.clone();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 5,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

    let replacement_len = apply_redux_context_rollover(
        &SummaryProvider,
        &mut driver_history,
        &mut compaction_guard_history,
        "model",
        &config,
        None,
        &action_tx,
        crate::chat::action::CompactReason::ContextOverflow,
        "test_empty_enrichment_guard_source",
    )
    .await
    .unwrap()
    .replacement_len
    .expect("summary patch must apply");
    assert_eq!(replacement_len, 1);
    let patch = match action_rx.recv().await.expect("patch action") {
        Action::HistoryCompactionPatchApplied { patch, .. } => patch,
        other => panic!("expected summary patch action, got {other:?}"),
    };
    let reducer_history = original_history
        .get(..original_history.len() - 1)
        .expect("reducer history fixture")
        .to_vec();
    assert!(
        crate::agent::compaction::compaction_patch_guard_matches(&reducer_history, &patch.guard),
        "empty-enrichment path should guard reducer history before ordered user-turn commit"
    );

    let mut reducer_state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    reducer_state.session.history = reducer_history;
    let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });
    assert!(effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))));
    let _ = reducer_state.reduce(Action::RecordUserTurn("current user".to_string()));
    assert_eq!(
        driver_history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        reducer_state
            .session
            .history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        "empty-enrichment path should remain byte-for-byte aligned"
    );
}

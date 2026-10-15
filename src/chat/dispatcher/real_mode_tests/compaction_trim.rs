#![cfg(test)]

use super::*;

#[tokio::test]
async fn redux_compaction_keeps_enrichment_only_trim_fallback() {
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
            Ok("ENRICHMENT_ONLY_SUMMARY\n## Decisions\n- fallback remains".to_string())
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
        PMsg::user("small old user"),
        PMsg::assistant("small old assistant"),
        PMsg::user("current @huge.txt"),
    ];
    let mut driver_history = original_history.clone();
    let Some(current_user) = driver_history.get_mut(3) else {
        panic!("test fixture must include current user turn");
    };
    current_user.content.push_str(&format!(
        "\n\n[Attached file context from @path mentions]\nRECENT_ENRICHMENT_ONLY {}\n[End attached file context]",
        "hidden ".repeat(1000)
    ));
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
        "test_enrichment_only_fallback",
    )
    .await
    .unwrap()
    .replacement_len
    .expect("provider-side enrichment pressure should still produce a patch attempt");
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
        "fallback-preserving patch must still be guarded by reducer persisted source"
    );
    assert!(
        !driver_history
            .iter()
            .any(|message| message.content.contains("RECENT_ENRICHMENT_ONLY")),
        "existing provider-side trim fallback must still remove enrichment-only overflow"
    );
}

#[tokio::test]
async fn redux_driver_preserve_trim_floor_preserves_fitting_summary_and_matches_reducer_history() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct OversizedSummaryProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for OversizedSummaryProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if call == 0 {
                Ok(format!("FLUSH_MARKER {}", "flush-context ".repeat(12)))
            } else {
                Ok(format!(
                    "SUMMARY_MARKER\n## Decisions\n- keep\n## Open TODOs\n- keep\n## Critical Context\n- keep\n{}",
                    "summary-context ".repeat(12)
                ))
            }
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
        PMsg::user("old user ".repeat(40)),
        PMsg::assistant("old assistant ".repeat(40)),
        PMsg::user("older user ".repeat(40)),
        PMsg::assistant("older assistant ".repeat(40)),
        PMsg::user("recent bulk ".repeat(40)),
    ];
    let mut driver_history = original_history.clone();
    let mut compaction_guard_history = original_history.clone();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 5,
        keep_recent_messages: 1,
        memory_flush: true,
        max_context_tokens: 300,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let provider = OversizedSummaryProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

    let replacement_len = apply_redux_context_rollover(
        &provider,
        &mut driver_history,
        &mut compaction_guard_history,
        "model",
        &config,
        None,
        &action_tx,
        crate::chat::action::CompactReason::ContextOverflow,
        "test_second_trim",
    )
    .await
    .unwrap()
    .replacement_len
    .expect("summary patch must apply");
    assert_eq!(replacement_len, 2, "memory flush plus summary must both be protected");
    let after_compact = crate::agent::context_budget::plan_context_budget(
        &driver_history,
        &config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    assert!(
        !after_compact.over_hard_limit,
        "preserve-trim floor should fully remediate when the protected replacement fits"
    );
    let _ = trim_redux_driver_context_budget_after_rollover(
        &mut driver_history,
        &mut compaction_guard_history,
        &config,
        Some(replacement_len),
    );

    let patch = match action_rx.recv().await.expect("patch action") {
        Action::HistoryCompactionPatchApplied { patch, .. } => patch,
        other => panic!("expected summary patch action, got {other:?}"),
    };
    let mut reducer_state =
        crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    reducer_state.session.history = original_history
        .get(..original_history.len() - 1)
        .expect("reducer history fixture")
        .to_vec();
    let _ = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::ContextOverflow,
        patch,
        compaction_config: config,
    });
    let pending_user = original_history.last().expect("pending user fixture").content.clone();
    let _ = reducer_state.reduce(Action::RecordUserTurn(pending_user));

    assert!(
        driver_history
            .iter()
            .any(|message| message.content.contains("SUMMARY_MARKER")),
        "second trim must not delete the provider summary"
    );
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
        "GP-6: driver fallback history must exactly match reducer history"
    );
}

#[tokio::test]
async fn redux_driver_preserve_trim_floor_drops_unfit_summary_and_matches_reducer_history() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct HugeSummaryProvider;
    #[async_trait]
    impl Provider for HugeSummaryProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok(format!(
                "SUMMARY_TOO_LARGE\n## Decisions\n- {}",
                "summary-over-budget ".repeat(800)
            ))
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

    let mut original_history = vec![PMsg::system("sys")];
    for idx in 0..8 {
        original_history.push(PMsg::user(format!(
            "trim-candidate-{idx} {}",
            "recent-bulk ".repeat(80)
        )));
    }
    let mut driver_history = original_history.clone();
    let mut compaction_guard_history = original_history.clone();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 4,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

    let replacement_len = apply_redux_context_rollover(
        &HugeSummaryProvider,
        &mut driver_history,
        &mut compaction_guard_history,
        "model",
        &config,
        None,
        &action_tx,
        crate::chat::action::CompactReason::ContextOverflow,
        "test_floor_drops_unfit_summary",
    )
    .await
    .unwrap()
    .replacement_len
    .expect("summary patch must apply");
    assert_eq!(replacement_len, 1);

    let driver_budget = crate::agent::context_budget::plan_context_budget(
        &driver_history,
        &config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    assert!(
        !driver_budget.over_hard_limit,
        "driver history must be under hard limit"
    );
    assert!(
        !driver_history
            .iter()
            .any(|message| message.content.contains("SUMMARY_TOO_LARGE")),
        "floor fallback may drop a summary that cannot fit by itself"
    );

    let patch = match action_rx.recv().await.expect("patch action") {
        Action::HistoryCompactionPatchApplied { patch, .. } => patch,
        other => panic!("expected summary patch action, got {other:?}"),
    };
    let mut reducer_state =
        crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    reducer_state.session.history = original_history
        .get(..original_history.len() - 1)
        .expect("reducer history fixture")
        .to_vec();
    let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::ContextOverflow,
        patch,
        compaction_config: config.clone(),
    });
    assert!(
        effects.iter().all(|effect| {
            !matches!(
                effect,
                crate::chat::state::Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg
                } if msg.contains("guard mismatch")
            )
        }),
        "pending-user timing must not turn an exact rollover into a stale-patch fallback"
    );
    let pending_user = original_history.last().expect("pending user fixture").content.clone();
    let _ = reducer_state.reduce(Action::RecordUserTurn(pending_user.clone()));
    assert!(
        reducer_state
            .session
            .history
            .last()
            .is_some_and(|message| message.role == "user" && message.content == pending_user),
        "ordered commit must preserve the durable user turn even when it cannot fit provider context"
    );
}

#[tokio::test]
async fn redux_driver_off_mode_preflight_trims_without_history_compacted_action() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct BudgetCaptureProvider {
        first_tokens: Arc<AtomicUsize>,
        summary_calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for BudgetCaptureProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(String::new())
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
            stream::iter(vec![
                Ok(StreamChunk::delta("off-budget-ok")),
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
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(BudgetCaptureProvider {
        first_tokens: Arc::clone(&first_tokens),
        summary_calls: Arc::clone(&summary_calls),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Off,
                reserve_tokens: 10,
                keep_recent_messages: 2,
                memory_flush: false,
                max_context_tokens: 120,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..24 {
                history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
            }

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-budget-off-preflight",
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
                    draft_id: "draft-budget-off-preflight".into(),
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

            let mut saw_history_compacted = false;
            let mut saw_summary_patch = false;
            let mut saw_completion = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::HistoryCompacted { .. } => {
                        saw_history_compacted = true;
                    }
                    Action::HistoryCompactionPatchApplied { .. } => {
                        saw_summary_patch = true;
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("off-budget-ok"));
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should trim in Off mode before stream request, not fail: {err}");
                    }
                    _ => {}
                }
            }
            assert!(
                saw_completion,
                "driver must complete after Off-mode trim-only preflight"
            );
            assert!(
                !saw_history_compacted,
                "Off mode must not dispatch HistoryCompacted because reducer compacts lossy"
            );
            assert!(!saw_summary_patch, "Off mode must not dispatch summary patch action");
            assert_eq!(
                summary_calls.load(AtomicOrdering::SeqCst),
                0,
                "Off mode must not call summarizer"
            );
            assert!(
                first_tokens.load(AtomicOrdering::SeqCst) <= 110,
                "Off-mode first provider call must be below literal hard limit 110 tokens"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn redux_driver_summarizer_failure_uses_local_guard_before_stream() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct FailingSummaryProvider {
        first_tokens: Arc<AtomicUsize>,
        summary_calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for FailingSummaryProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Err(anyhow::anyhow!("summary unavailable"))
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
            stream::iter(vec![
                Ok(StreamChunk::delta("trim-fallback-ok")),
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
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(FailingSummaryProvider {
        first_tokens: Arc::clone(&first_tokens),
        summary_calls: Arc::clone(&summary_calls),
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
                max_context_tokens: 120,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..24 {
                history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
            }

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-summary-failure",
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
                    draft_id: "draft-summary-failure".into(),
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

            let mut saw_local_guard_patch = false;
            let mut saw_completion = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::HistoryCompactionPatchApplied { patch, .. } => {
                        saw_local_guard_patch = patch.replacement.iter().any(|message| {
                            message
                                .content
                                .contains("Summary unavailable; compacted conservatively.")
                        });
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("trim-fallback-ok"));
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should fail soft to trim: {err}")
                    }
                    _ => {}
                }
            }

            assert!(saw_completion, "driver must complete after summary failure trim");
            assert!(
                saw_local_guard_patch,
                "summary failure must dispatch the deterministic local guard patch"
            );
            assert_eq!(
                summary_calls.load(AtomicOrdering::SeqCst),
                2,
                "Safeguard mode must stop requesting summaries once compaction cannot progress"
            );
            assert!(
                first_tokens.load(AtomicOrdering::SeqCst) <= 110,
                "summary failure fallback must stream under literal hard limit 110"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn redux_driver_overflow_retry_uses_provider_summary_patch() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct OverflowThenSuccessProvider {
        stream_calls: Arc<AtomicUsize>,
        summary_calls: Arc<AtomicUsize>,
        retry_history: Arc<Mutex<Option<Vec<PMsg>>>>,
    }
    #[async_trait]
    impl Provider for OverflowThenSuccessProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(
                    "OVERFLOW_PROVIDER_SUMMARY\n## Decisions\n- recovered\n## Open TODOs\n- retry\n## Constraints\n- no duplicate tools"
                        .to_string(),
                )
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
            messages: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let call = self.stream_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if call == 0 {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                    "context_length_exceeded".into(),
                ))])
                .boxed()
            } else {
                let mut guard = self.retry_history.lock();
                if guard.is_none() {
                    *guard = Some(messages.to_vec());
                }
                stream::iter(vec![
                    Ok(StreamChunk::delta("summary-retry-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let stream_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let retry_history = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(OverflowThenSuccessProvider {
        stream_calls: Arc::clone(&stream_calls),
        summary_calls: Arc::clone(&summary_calls),
        retry_history: Arc::clone(&retry_history),
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
                max_context_tokens: 2_500,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            let mut history = vec![PMsg::system("sys")];
            for i in 0..12 {
                history.push(PMsg::user(format!("turn {i} {}", "overflow context ".repeat(18))));
            }

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-overflow-summary",
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
                    draft_id: "draft-overflow-summary".into(),
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
                                .any(|message| message.content.contains("OVERFLOW_PROVIDER_SUMMARY")),
                            "overflow retry patch must carry provider summary"
                        );
                    }
                    Action::HistoryCompacted { .. } => {
                        panic!("overflow retry with config must not use lossy HistoryCompacted");
                    }
                    Action::ContextWindowUpdated {
                        used_context_tokens: Some(used),
                        max_context_tokens: Some(max),
                    } => {
                        saw_context_update = true;
                        assert_eq!(max, 2_500);
                        assert!(
                            used <= 2_490,
                            "overflow context window update should use compacted history: {used}"
                        );
                    }
                    Action::SystemMessageAdded { text } => {
                        saw_feedback = true;
                        assert!(
                            text.contains("Compacted context") || text.contains("Context already compact"),
                            "overflow feedback should describe compaction result: {text}"
                        );
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("summary-retry-ok"));
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should recover after summary retry: {err}")
                    }
                    _ => {}
                }
            }

            assert!(saw_summary_patch, "overflow retry must dispatch exact summary patch");
            assert!(
                saw_feedback,
                "overflow retry must dispatch user-visible compact feedback"
            );
            assert!(
                saw_context_update,
                "overflow retry must update context-window UI metadata after mid-turn compaction"
            );
            assert_eq!(summary_calls.load(AtomicOrdering::SeqCst), 1);
            assert_eq!(
                stream_calls.load(AtomicOrdering::SeqCst),
                2,
                "this fixture should recover on its first progress-making retry"
            );
            let retry = retry_history.lock().clone().expect("retry history");
            assert!(
                retry
                    .iter()
                    .any(|message| message.content.contains("OVERFLOW_PROVIDER_SUMMARY")),
                "retry provider request must include summary"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// **S3 T3-1 Step 3**: more than one context overflow retry → StreamFailed.
#[tokio::test]
async fn t31_driver_context_overflow_exhausted_emits_stream_failed() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct AlwaysOverflowProvider;
    #[async_trait]
    impl Provider for AlwaysOverflowProvider {
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
            stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                "context_length_exceeded: please reduce input".into(),
            ))])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(AlwaysOverflowProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-overflow-fail", &"x".repeat(1000), false)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-overflow-fail".into(),
                    history: vec![crate::providers::traits::ChatMessage {
                        role: "user".into(),
                        content: "x".repeat(1000),
                    }],
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

            let mut saw_failed = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("action within 2s")
                    .expect("must arrive");
                if let Action::StreamFailed { err, .. } = &action {
                    assert!(
                        err.contains("context overflow") || err.contains("context_length_exceeded"),
                        "err must mention overflow: {err}"
                    );
                    saw_failed = true;
                    break;
                }
            }
            assert!(saw_failed, "must emit StreamFailed after overflow retries exhausted");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

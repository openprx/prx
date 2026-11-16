#![cfg(test)]

//! Bug #1: the deterministic `/compact` fallback is the reducer's
//! `compact_history_in_place` (the provider view is rebuilt from its
//! acknowledged result). These tests pin the routine the slash command drives so the user-visible turn /
//! token delta is meaningful.
use super::*;

fn long_user(seq: usize) -> ChatMessage {
    // Each turn is well above COMPACT_CONTENT_CHARS so truncation + drop both fire.
    ChatMessage::user(format!("turn-{seq}-{}", "x".repeat(COMPACT_CONTENT_CHARS * 2)))
}

#[test]
fn compact_drops_old_turns_beyond_keep_window() {
    let mut history = vec![ChatMessage::system("sys")];
    for i in 0..(COMPACT_KEEP_MESSAGES + 6) {
        history.push(long_user(i));
    }
    let turns_before = history.len() - 1;
    let tokens_before = estimate_chat_history_tokens(&history);

    crate::chat::state::compact_history_in_place(&mut history);

    let turns_after = history.len() - 1;
    let tokens_after = estimate_chat_history_tokens(&history);

    // System prompt always preserved at index 0.
    assert_eq!(history.first().map(|m| m.role.as_str()), Some("system"));
    // Manual /compact must actually shrink an over-long history.
    assert!(turns_after < turns_before, "compact should drop old turns");
    assert!(tokens_after < tokens_before, "compact should reduce token estimate");
    // Never exceeds the keep window after compaction.
    assert!(turns_after <= COMPACT_KEEP_MESSAGES, "must respect keep window");
}

#[test]
fn compact_is_noop_for_short_history() {
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
    let before = estimate_chat_history_tokens(&history);
    crate::chat::state::compact_history_in_place(&mut history);
    let after = estimate_chat_history_tokens(&history);
    // A 1-turn history is already compact — nothing to drop.
    assert_eq!(history.len(), 2);
    assert_eq!(before, after);
}

/// Plain (`--plain` / piped) chat has no transcript renderer, so the whole
/// signal has to fit in the single line that gets printed.
#[test]
fn embedding_upgrade_notice_prints_as_one_plain_line() {
    let notice = format_embedding_upgrade_notice(crate::config::EMBEDDING_DEFAULT_UPGRADE_NOTICE);
    assert!(!notice.contains('\n'), "the notice must be a single line: {notice:?}");
    assert!(
        notice.contains("embedding_provider"),
        "the notice must name the key to write: {notice}"
    );
    assert!(
        notice.contains("\"none\""),
        "the notice must say how to turn vector recall off: {notice}"
    );
    // Plain mode pipes it as exactly one line.
    let piped = format_fallback_chat_output_for(&notice, false);
    assert_eq!(piped.lines().count(), 1, "plain output must be one line: {piped:?}");
}

#[test]
fn the_chat_notice_appears_only_while_the_default_is_inherited() {
    let mut config = crate::config::Config::default();
    config.memory.embedding_provider = "local".to_string();
    config.memory.embedding_provider_explicit = false;
    let notice = chat_embedding_upgrade_notice(&config).expect("test: an inherited default must be announced");
    assert!(notice.starts_with("note > "), "{notice}");

    config.memory.embedding_provider_explicit = true;
    assert!(
        chat_embedding_upgrade_notice(&config).is_none(),
        "a key the operator wrote must not be nagged about"
    );

    config.memory.embedding_provider_explicit = false;
    config.memory.embedding_provider = "none".to_string();
    assert!(
        chat_embedding_upgrade_notice(&config).is_none(),
        "vector recall that is off has no upgrade to announce"
    );
}

#[test]
fn context_degraded_notice_prints_as_one_plain_line() {
    let notice = format_context_degraded_notice(7);
    assert!(
        notice.contains("lossy"),
        "the notice must not read as a clean compaction"
    );
    assert!(notice.contains('7'), "the notice must say how much was dropped");
    assert!(!notice.contains('\n'), "the notice must be a single line: {notice:?}");

    let piped = format_fallback_chat_output_for(&notice, false);
    assert_eq!(
        piped.lines().count(),
        1,
        "plain mode must print exactly one line: {piped:?}"
    );
    assert!(piped.ends_with('\n'), "plain output must terminate its line");

    assert!(
        format_context_degraded_notice(1).contains("1 older message dropped"),
        "a single dropped message must not read as plural"
    );
}

#[test]
fn compact_command_reports_window_and_reclaim_delta() {
    let text = format_compact_feedback(20, 12, 10_000, 4_000, 1_000_000);

    assert!(text.contains("20 -> 12 turns"), "turn delta missing: {text}");
    assert!(text.contains("~10000 -> ~4000 tokens"), "token delta missing: {text}");
    assert!(text.contains("1M window"), "model window missing: {text}");
    assert!(
        text.contains("reclaimed ~6000 tokens (60%)"),
        "reclaim delta missing: {text}"
    );
}

#[test]
fn compact_command_with_nothing_beyond_the_kept_window_reports_noop() {
    let config = crate::config::AgentCompactionConfig {
        reserve_tokens: 10,
        max_context_tokens: 10_000,
        max_context_tokens_explicit: true,
        keep_recent_messages: 2,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![ChatMessage::system("sys"), ChatMessage::user("short prompt")];

    assert!(
        !manual_compact_has_compactable(&history, &config),
        "a history inside the kept window has nothing to compact"
    );
    let feedback = format_nothing_to_compact_feedback(&history, &config);
    assert!(feedback.contains("Nothing to compact"), "{feedback}");
    assert!(feedback.contains("1 turns"), "{feedback}");
}

#[test]
fn compact_command_below_the_budget_still_compacts_beyond_the_kept_window() {
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 1_000_000,
        max_context_tokens_explicit: true,
        keep_recent_messages: 2,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("one"),
        ChatMessage::assistant("two"),
        ChatMessage::user("three"),
    ];

    assert!(manual_compact_has_compactable(&history, &config));
}

#[test]
fn legacy_preflight_compaction_feedback_emits_system_message() {
    let config = crate::config::AgentCompactionConfig {
        reserve_tokens: 10,
        max_context_tokens: 10_000,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let mut history = vec![ChatMessage::system("sys")];
    for i in 0..(COMPACT_KEEP_MESSAGES + 4) {
        history.push(long_user(i));
    }
    let (turns_before, tokens_before) = crate::chat::compaction::chat_history_turns_and_tokens(&history);
    let messages_before = history.len();
    crate::chat::state::compact_history_in_place(&mut history);
    let outcome = crate::chat::compaction::ManualCompactOutcome::Degraded {
        dropped_messages: messages_before.saturating_sub(history.len()),
    };
    let text = outcome.feedback(turns_before, tokens_before, &config);
    let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();

    surface_session_message(&dispatcher, None, &text);

    let action = action_rx
        .try_recv()
        .expect("legacy preflight feedback should dispatch a system message");
    assert!(
        matches!(action, crate::chat::action::Action::SystemMessageAdded { text: ref actual } if actual == &text),
        "unexpected feedback action: {action:?}"
    );
}

struct SlowSummaryProvider;

#[async_trait::async_trait]
impl Provider for SlowSummaryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok("## Decisions\nslow summary".to_string())
    }
}

struct ImmediateSummaryProvider;

#[async_trait::async_trait]
impl Provider for ImmediateSummaryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(
                "## Decisions\n- PERSISTED_GUARD_SUMMARY\n## Open TODOs\n- continue\n## Critical Context\n- persisted source"
                    .to_string(),
            )
    }
}

#[tokio::test]
async fn slow_summary_compaction_is_awaited_without_wall_clock_timeout() {
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 0,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 16,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("older ".repeat(400)),
        ChatMessage::assistant("middle ".repeat(400)),
        ChatMessage::user("recent ".repeat(40)),
    ];

    let patch = build_chat_compaction_patch(
        &history,
        &history,
        &SlowSummaryProvider,
        "slow-model",
        &config,
        None,
        "test_slow_summary",
    )
    .await;

    assert!(patch.is_some(), "slow summary must be awaited to completion");
}

#[tokio::test]
async fn legacy_compaction_with_tool_message_uses_persisted_guard_for_reducer() {
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 0,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 400,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let system_message = ChatMessage::system("sys");
    let old_user_message = ChatMessage::user(format!("old persisted user {}", "u ".repeat(220)));
    let old_assistant_message = ChatMessage::assistant(format!("old persisted assistant {}", "a ".repeat(220)));
    let latest_user_message = ChatMessage::user("latest visible question");
    let persisted_history = vec![
        system_message.clone(),
        old_user_message.clone(),
        old_assistant_message.clone(),
        latest_user_message,
    ];
    let enriched_history = vec![
        system_message,
        old_user_message,
        old_assistant_message,
        ChatMessage::assistant(format!("[tool:shell]\n{}", "tool output ".repeat(320))),
        ChatMessage::user("[Memory context]\nlatest visible question"),
    ];

    let patch = build_chat_compaction_patch(
        &enriched_history,
        &persisted_history,
        &ImmediateSummaryProvider,
        "summary-model",
        &config,
        None,
        "test_persisted_guard",
    )
    .await
    .expect("budget-overflowing enriched history should produce a persisted-source patch");

    assert!(
        crate::agent::compaction::compaction_patch_guard_matches(&persisted_history, &patch.guard),
        "patch guard must match the persisted reducer mirror"
    );
    assert!(
        !crate::agent::compaction::compaction_patch_guard_matches(&enriched_history, &patch.guard),
        "pre-fix enriched guard source would miss once tool/internal messages exist"
    );

    let mut legacy_history = enriched_history;
    let session_dispatcher = crate::chat::acked_session::TestSessionDispatcher::spawn(
        std::sync::Arc::new(crate::memory::NoneMemory::new()),
        crate::chat::acked_session::TestSessionDispatcher::state_with_history(persisted_history.clone()),
    );
    let outcome = apply_chat_compaction_patch_and_sync(
        &mut legacy_history,
        &persisted_history,
        None,
        patch.clone(),
        &config,
        crate::chat::action::CompactReason::ContextOverflow,
        &session_dispatcher.dispatcher,
        &session_dispatcher.commits,
    )
    .await;
    session_dispatcher.stop().await;
    assert!(
        matches!(outcome, crate::chat::compaction::ManualCompactOutcome::Compacted { .. }),
        "the persisted-source patch must apply to its own source and be acknowledged: {outcome:?}"
    );

    let mut reducer_state = crate::chat::acked_session::TestSessionDispatcher::state_with_history(persisted_history);
    let _ = reducer_state.reduce(crate::chat::action::Action::HistoryCompactionPatchApplied {
        reason: crate::chat::action::CompactReason::ContextOverflow,
        patch,
        compaction_config: config.clone(),
    });

    let legacy_pairs: Vec<_> = legacy_history
        .iter()
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect();
    let reducer_pairs: Vec<_> = reducer_state
        .session
        .history
        .iter()
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect();
    assert_eq!(
        legacy_pairs, reducer_pairs,
        "legacy driver and reducer mirror must converge on the persisted-source compaction patch"
    );
    assert!(
        legacy_history
            .iter()
            .any(|message| message.content.contains("PERSISTED_GUARD_SUMMARY")),
        "provider summary should survive the reducer guard"
    );
    assert!(
        legacy_history
            .iter()
            .all(|message| !message.content.contains("[tool:shell]")),
        "persisted-source compaction must not keep enriched-only tool/internal messages"
    );
}

#[tokio::test]
async fn chat_compaction_helper_refuses_a_patch_for_another_source_without_any_change() {
    tokio::time::timeout(ACK_TEST_DEADLINE, async {
        let config = crate::config::AgentCompactionConfig::default();
        let source = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
        ];
        let mut other_source = source.clone();
        other_source.insert(1, ChatMessage::user("inserted"));
        let patch = crate::agent::turn_contracts::CompactionPatch {
            replacement: vec![ChatMessage::assistant("summary")],
            append_after: Vec::new(),
            guard: crate::agent::compaction::compaction_patch_guard_for(&other_source, 1, 3)
                .expect("test: guard over the other source"),
        };
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("q2 enriched")];
        let history_before = history.clone();
        let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
        let commits = dispatcher::TurnCompletionSignal::new();

        let outcome = apply_chat_compaction_patch_and_sync(
            &mut history,
            &source,
            Some(ChatMessage::user("q2 enriched")),
            patch,
            &config,
            crate::chat::action::CompactReason::Manual,
            &dispatcher,
            &commits,
        )
        .await;

        assert!(
            matches!(outcome, crate::chat::compaction::ManualCompactOutcome::Refused { .. }),
            "a patch whose guard describes another source must be refused: {outcome:?}"
        );
        assert_eq!(
            history.iter().map(|m| (&m.role, &m.content)).collect::<Vec<_>>(),
            history_before.iter().map(|m| (&m.role, &m.content)).collect::<Vec<_>>(),
            "a refused patch must leave the provider view unchanged"
        );
        assert!(
            action_rx.try_recv().is_err(),
            "a refused patch must not reach the reducer"
        );
    })
    .await
    .expect("the test finishes within ACK_TEST_DEADLINE");
}

#[tokio::test]
async fn chat_compaction_helper_replaces_the_plain_request_with_the_enriched_one() {
    tokio::time::timeout(ACK_TEST_DEADLINE, async {
        let config = crate::config::AgentCompactionConfig::default();
        let source = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
        ];
        let patch = crate::agent::turn_contracts::CompactionPatch {
            replacement: vec![ChatMessage::assistant("summary")],
            append_after: Vec::new(),
            guard: crate::agent::compaction::compaction_patch_guard_for(&source, 1, 3)
                .expect("test: guard over source"),
        };
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("q2 ENRICHED")];
        let session_dispatcher = crate::chat::acked_session::TestSessionDispatcher::spawn(
            std::sync::Arc::new(crate::memory::NoneMemory::new()),
            crate::chat::acked_session::TestSessionDispatcher::state_with_history(source.clone()),
        );

        let outcome = apply_chat_compaction_patch_and_sync(
            &mut history,
            &source,
            Some(ChatMessage::user("q2 ENRICHED")),
            patch,
            &config,
            crate::chat::action::CompactReason::ContextOverflow,
            &session_dispatcher.dispatcher,
            &session_dispatcher.commits,
        )
        .await;
        session_dispatcher.stop().await;

        assert!(
            matches!(outcome, crate::chat::compaction::ManualCompactOutcome::Compacted { .. }),
            "the source patch must apply: {outcome:?}"
        );
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["sys", "summary", "q2 ENRICHED"],
            "the provider view is the compacted source with the enriched request replacing the plain one"
        );
    })
    .await
    .expect("the test finishes within ACK_TEST_DEADLINE");
}

/// Upper bound for one awaited-acknowledgement test.
const ACK_TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

fn sqlite_test_memory() -> (tempfile::TempDir, std::sync::Arc<dyn crate::memory::Memory>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let memory: std::sync::Arc<dyn crate::memory::Memory> =
        std::sync::Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    (dir, memory)
}

#[tokio::test]
async fn deterministic_trim_publishes_a_saved_revision_that_later_reads_keep() {
    tokio::time::timeout(ACK_TEST_DEADLINE, async {
        let (_dir, memory) = sqlite_test_memory();
        let mut history = vec![ChatMessage::system("sys")];
        history.extend((0..20).map(long_user));
        let state = crate::chat::acked_session::TestSessionDispatcher::state_with_history(history.clone());
        let session_id = state.session.id.clone();
        let harness = crate::chat::acked_session::TestSessionDispatcher::spawn(std::sync::Arc::clone(&memory), state);
        let before = harness.dispatcher.session_publisher().current();
        let mut provider_view = history.clone();

        let outcome = crate::chat::compaction::apply_deterministic_trim_and_sync(
            &mut provider_view,
            None,
            crate::chat::action::CompactReason::Manual,
            &harness.dispatcher,
            &harness.commits,
        )
        .await;
        let after_trim = harness.dispatcher.session_publisher().current();
        let later = harness
            .dispatcher
            .dispatch_awaited(
                crate::chat::action::Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
                &harness.commits,
            )
            .await
            .expect("test: later save acknowledged");
        harness.stop().await;

        let crate::chat::compaction::ManualCompactOutcome::Degraded { dropped_messages } = outcome else {
            panic!("a deterministic trim reports the loss: {outcome:?}");
        };
        assert_eq!(
            dropped_messages,
            history.len() - provider_view.len(),
            "the reported loss is the provider view's dropped messages"
        );
        assert!(dropped_messages > 0, "twenty oversized turns must lose some");
        assert_eq!(
            after_trim.revision,
            before.revision + 1,
            "the trim is one saved revision"
        );
        let trimmed: Vec<&str> = provider_view.iter().skip(1).map(|m| m.content.as_str()).collect();
        let published: Vec<&str> = after_trim.session.turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(published, trimmed, "the acknowledged turns are the trimmed history");
        let crate::chat::acked_session::Ack::Published(later) = later else {
            panic!("a changed mode publishes a saved revision: {later:?}");
        };
        let kept: Vec<&str> = later.session.turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(kept, trimmed, "a later revision keeps the trimmed turns");
        let key = format!("{}:{session_id}", crate::chat::session::SESSION_MEMORY_PREFIX);
        let stored = memory
            .get(&key)
            .await
            .expect("test: read stored row")
            .expect("test: row stored");
        assert!(
            !stored.content.contains("turn-0-"),
            "the stored row omits the dropped turns"
        );
        assert!(
            stored.content.contains("turn-19-"),
            "the stored row keeps the newest turn"
        );
    })
    .await
    .expect("deterministic_trim_publishes_a_saved_revision_that_later_reads_keep exceeded ACK_TEST_DEADLINE");
}

#[tokio::test]
async fn a_guard_mismatched_reducer_patch_is_rejected_and_leaves_state_and_revision_unchanged() {
    tokio::time::timeout(ACK_TEST_DEADLINE, async {
        let (_dir, memory) = sqlite_test_memory();
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
        ];
        let mut other = history.clone();
        other.insert(1, ChatMessage::user("inserted"));
        let patch = crate::agent::turn_contracts::CompactionPatch {
            replacement: vec![ChatMessage::assistant("summary")],
            append_after: Vec::new(),
            guard: crate::agent::compaction::compaction_patch_guard_for(&other, 1, 3)
                .expect("test: guard over another history"),
        };
        let harness = crate::chat::acked_session::TestSessionDispatcher::spawn(
            memory,
            crate::chat::acked_session::TestSessionDispatcher::state_with_history(history),
        );
        let before = harness.dispatcher.session_publisher().current();

        let ack = harness
            .dispatcher
            .dispatch_awaited(
                crate::chat::action::Action::HistoryCompactionPatchApplied {
                    reason: crate::chat::action::CompactReason::ContextOverflow,
                    patch,
                    compaction_config: crate::config::AgentCompactionConfig::default(),
                },
                &harness.commits,
            )
            .await
            .expect("test: patch answered");
        let after = harness.dispatcher.session_publisher().current();
        harness.stop().await;

        assert!(matches!(ack, crate::chat::acked_session::Ack::Rejected(_)), "{ack:?}");
        assert_eq!((after.load_epoch, after.revision), (before.load_epoch, before.revision), "a rejected patch publishes no new revision");
        assert_eq!(after.session.turns.len(), before.session.turns.len(), "a rejected patch leaves the turns");
    })
    .await
    .expect("a_guard_mismatched_reducer_patch_is_rejected_and_leaves_state_and_revision_unchanged exceeded ACK_TEST_DEADLINE");
}

#[tokio::test]
async fn a_deterministic_trim_over_enriched_past_requests_keeps_the_view_equal_to_later_acknowledged_reads() {
    tokio::time::timeout(ACK_TEST_DEADLINE, async {
        let (_dir, memory) = sqlite_test_memory();
        let reply = |seq: usize| format!("answer-{seq} {}", "detail ".repeat(42));
        let enriched = |text: &str| format!("[Memory context]\n{}\n{text}", "recalled ".repeat(40));
        let mut reducer_history = vec![ChatMessage::system("sys")];
        // The TUI provider view keeps every past request enriched; the
        // reducer stores them plain.
        let mut provider_view = vec![ChatMessage::system("sys")];
        for seq in 0..COMPACT_KEEP_MESSAGES {
            let question = format!("question-{seq}");
            reducer_history.push(ChatMessage::user(question.clone()));
            provider_view.push(ChatMessage::user(enriched(&question)));
            reducer_history.push(ChatMessage::assistant(reply(seq)));
            provider_view.push(ChatMessage::assistant(reply(seq)));
        }
        let current = "current question";
        reducer_history.push(ChatMessage::user(current));
        let enriched_request = ChatMessage::user(enriched(current));
        provider_view.push(enriched_request.clone());
        let harness = crate::chat::acked_session::TestSessionDispatcher::spawn(
            memory,
            crate::chat::acked_session::TestSessionDispatcher::state_with_history(reducer_history),
        );

        let outcome = crate::chat::compaction::apply_deterministic_trim_and_sync(
            &mut provider_view,
            Some(enriched_request.clone()),
            crate::chat::action::CompactReason::ContextOverflow,
            &harness.dispatcher,
            &harness.commits,
        )
        .await;
        let later = harness
            .dispatcher
            .dispatch_awaited(
                crate::chat::action::Action::ModeChanged(crate::agent::turn_contracts::ChatMode::Plan),
                &harness.commits,
            )
            .await
            .expect("test: later save acknowledged");
        harness.stop().await;

        assert!(
            matches!(outcome, crate::chat::compaction::ManualCompactOutcome::Degraded { .. }),
            "a deterministic trim reports the loss: {outcome:?}"
        );
        let crate::chat::acked_session::Ack::Published(later) = later else {
            panic!("a changed mode publishes a saved revision: {later:?}");
        };
        let (view_request, view_past) = provider_view
            .split_last()
            .expect("test: the trimmed view keeps the current request");
        assert_eq!(
            view_request.content, enriched_request.content,
            "only the current request keeps its enrichment"
        );
        let view_turns: Vec<(&str, &str)> = view_past
            .iter()
            .skip(1)
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect();
        let (acked_request, acked_past) = later
            .session
            .turns
            .split_last()
            .expect("test: the acknowledged turns keep the current request");
        assert_eq!(
            acked_request.content, current,
            "the acknowledged current request is the stored plain one"
        );
        let acked_turns: Vec<(&str, &str)> = acked_past
            .iter()
            .map(|turn| (turn.role.as_str(), turn.content.as_str()))
            .collect();
        assert_eq!(
            view_turns, acked_turns,
            "a later acknowledged read brings back no turn the trimmed provider view dropped"
        );
        assert!(
            view_past.iter().all(|message| !message.content.contains("recalled")),
            "past requests carry no enrichment after the trim"
        );
    })
    .await
    .expect("a_deterministic_trim_over_enriched_past_requests_keeps_the_view_equal_to_later_acknowledged_reads exceeded ACK_TEST_DEADLINE");
}

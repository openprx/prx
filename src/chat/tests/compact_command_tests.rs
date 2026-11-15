#![cfg(test)]

//! Bug #1: `/compact` manual compaction reuses `compact_chat_history`. These
//! tests pin the routine the slash command drives so the user-visible turn /
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

    compact_chat_history(&mut history);

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
    compact_chat_history(&mut history);
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
    compact_chat_history(&mut history);
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
    let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
    let outcome = apply_chat_compaction_patch_and_sync(
        &mut legacy_history,
        &persisted_history,
        None,
        patch,
        &config,
        crate::chat::action::CompactReason::ContextOverflow,
        &dispatcher,
    );
    assert!(
        matches!(outcome, crate::chat::compaction::ManualCompactOutcome::Compacted { .. }),
        "the persisted-source patch must apply to its own source: {outcome:?}"
    );
    let action = action_rx
        .try_recv()
        .expect("compaction helper should dispatch reducer patch");

    let mut reducer_state = crate::chat::state::ChatState::new(
        std::sync::Arc::from("provider"),
        std::sync::Arc::from("model"),
        tokio_util::sync::CancellationToken::new(),
    );
    reducer_state.session.history = persisted_history;
    let _ = reducer_state.reduce(action);

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

#[test]
fn chat_compaction_helper_refuses_a_patch_for_another_source_without_any_change() {
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

    let outcome = apply_chat_compaction_patch_and_sync(
        &mut history,
        &source,
        Some(ChatMessage::user("q2 enriched")),
        patch,
        &config,
        crate::chat::action::CompactReason::Manual,
        &dispatcher,
    );

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
}

#[test]
fn chat_compaction_helper_replaces_the_plain_request_with_the_enriched_one() {
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
        guard: crate::agent::compaction::compaction_patch_guard_for(&source, 1, 3).expect("test: guard over source"),
    };
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("q2 ENRICHED")];
    let (dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();

    let outcome = apply_chat_compaction_patch_and_sync(
        &mut history,
        &source,
        Some(ChatMessage::user("q2 ENRICHED")),
        patch,
        &config,
        crate::chat::action::CompactReason::ContextOverflow,
        &dispatcher,
    );

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
}

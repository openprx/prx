#![cfg(test)]

//! `/compact` and preflight feedback. When no handoff or summary can be
//! built, nothing is changed in the session: a lossy trim is a per-request
//! projection only.
use super::*;

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
fn compact_command_below_the_budget_has_nothing_to_compact_beyond_the_kept_window() {
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

    assert!(
        !manual_compact_has_compactable(&history, &config),
        "non-system messages within the kept-recent token budget have nothing to compact"
    );
}

#[test]
fn legacy_preflight_compaction_feedback_emits_system_message() {
    let config = crate::config::AgentCompactionConfig {
        reserve_tokens: 10,
        max_context_tokens: 10_000,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("q1"),
        ChatMessage::assistant("a1"),
    ];
    let (turns_before, tokens_before) = crate::chat::compaction::chat_history_turns_and_tokens(&history);
    let outcome = crate::chat::compaction::ManualCompactOutcome::NoDurableCompaction;
    let text = outcome.feedback(turns_before, tokens_before, &config);
    assert_eq!(
        text,
        crate::chat::compaction::NO_DURABLE_COMPACTION_FEEDBACK,
        "without a durable compaction the feedback says nothing was changed and requests are trimmed per request"
    );
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
            &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
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
            &crate::agent::turn_history::CanonicalHistory::from_test_messages(source.clone()),
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

#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::agent::compaction::{CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE, apply_configurable_compaction};
use crate::agent::context_preflight::preflight_context_budget_before_provider_call;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_tests::{
    ScriptedProvider, SystemSummaryProvider, WideSchemaTool, context_budget_fixture_config,
    context_budget_fixture_history, first_orphan_tool_index, tool_roundtrip_history_fixture, tool_specs_of,
};
use crate::memory::{Memory, SqliteMemory};
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::Tool;
use std::sync::Arc;
use tempfile::TempDir;

/// A degraded session must not open one write transaction per tool-loop
/// iteration. The pre-provider budget check runs on every iteration, and the
/// overflow retry adds four more call sites, so "write every time" meant a
/// synchronous SQLite write per iteration for the rest of the session.
///
/// MUTATION GUARD: drop the `degradation_recorded` claim and the
/// four-iteration count goes red; restore the per-call `Uuid` idempotency
/// key and the second-runtime count goes red.
#[tokio::test]
async fn context_switch_degradation_writes_one_event_per_turn() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-degrade-count", "run-degrade-count")
        .with_source_message_event_id("inbound-msg-1");
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 400,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    async fn degrade_once(
        memory: &Arc<dyn Memory>,
        envelope: &RuntimeEnvelope,
        provider: &ScriptedProvider,
        config: &crate::config::AgentCompactionConfig,
        audit: &DocumentIngestRuntime,
    ) {
        // Nothing in this history is persisted, so provenance is never
        // resolvable and every call degrades.
        let mut history = tool_roundtrip_history_fixture(10);
        let before = plan_context_budget(&history, config, PRE_TURN_FLUSH_THRESHOLD);
        assert!(before.over_hard_limit, "the fixture must actually trigger a rollover");
        let _ = memory;
        let _ = envelope;
        preflight_context_budget_before_provider_call(
            &mut history,
            provider,
            "model",
            config,
            crate::agent::turn_context::RequestToolMeasurement::NONE,
            Some(audit),
            "pre_provider_call",
        )
        .await
        .expect("degradation must not fail the turn");
    }

    async fn degraded_event_count(memory: &Arc<dyn Memory>, envelope: &RuntimeEnvelope) -> usize {
        memory
            .list_message_events_recent(&envelope.memory_principal(), 256)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count()
    }

    // Four iterations of one turn, through the runtime the loop holds.
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    for _ in 0..4 {
        degrade_once(&memory, &envelope, &provider, &config, &audit).await;
    }
    assert_eq!(
        degraded_event_count(&memory, &envelope).await,
        1,
        "four iterations of one turn must record exactly one degradation"
    );

    // A second runtime built for the same turn (several entry paths build
    // one) has its own in-process claim, so only the idempotency key can
    // stop the duplicate.
    let second = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    degrade_once(&memory, &envelope, &provider, &config, &second).await;
    assert_eq!(
        degraded_event_count(&memory, &envelope).await,
        1,
        "a second runtime for the same turn must be deduplicated by the idempotency key"
    );

    // Fixture proof: the suppression is scoped to the turn, not global.
    let next_turn = RuntimeEnvelope::agent("workspace-degrade-count", "run-degrade-count")
        .with_source_message_event_id("inbound-msg-2");
    let next_audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &next_turn);
    degrade_once(&memory, &next_turn, &provider, &config, &next_audit).await;
    assert_eq!(
        degraded_event_count(&memory, &envelope).await,
        2,
        "the next turn must still be able to report its own degradation"
    );
}

#[test]
fn token_aware_trim_never_strands_a_tool_result() {
    let fixture = tool_roundtrip_history_fixture(10);
    // Sweep the whole budget range so at least some limits cut exactly on a
    // tool result; the lossy fallback must be safe at every one of them.
    for max_context_tokens in (60..=1400).step_by(20) {
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens,
            max_context_tokens_explicit: true,
            os_paging: crate::config::OsPagingConfig::default(),
        };
        let mut history = fixture.clone();
        trim_history_to_context_budget(&mut history, &config);
        assert_eq!(
            first_orphan_tool_index(&history),
            None,
            "token-aware trim stranded a tool result at max_context_tokens={max_context_tokens}"
        );
        assert_eq!(
            history.first().map(|message| message.role.as_str()),
            Some("system"),
            "the leading system prompt must survive every trim"
        );
    }
}

#[tokio::test]
async fn legacy_preflight_preserving_trim_falls_back_when_summary_alone_exceeds_hard_limit() {
    let provider = SystemSummaryProvider {
        response: format!(
            "## Decisions\n- {}\n## Critical Context\n- SUMMARY_TOO_LARGE",
            "summary-over-budget ".repeat(800)
        ),
    };
    let mut history = vec![ChatMessage::system("sys")];
    for idx in 0..8 {
        history.push(ChatMessage::user(format!(
            "trim-candidate-{idx} {}",
            "recent-bulk ".repeat(80)
        )));
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 4,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let budget = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .unwrap();
    let final_budget = plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD);

    assert!(
        final_budget.over_hard_limit,
        "the floor is the request, so the history left is over the limit"
    );
    assert!(
        budget.over_hard_limit,
        "the returned snapshot must report the over-limit history it actually left"
    );
    assert!(
        !history.iter().any(|msg| msg.content.contains("SUMMARY_TOO_LARGE")),
        "plain trim floor may delete an unfit summary instead of sending an oversized prompt"
    );
    // Changed with the current-request floor: the floor fallback still
    // deletes the unfit summary and every older turn, but it stops at the
    // turn's own request, so the result is a request over the limit rather
    // than a prompt with no question in it.
    assert!(
        history.iter().any(|msg| msg.content.contains("trim-candidate-7")),
        "the current turn's request survives the floor fallback"
    );
    assert!(
        !history.iter().any(|msg| msg.content.contains("trim-candidate-6")),
        "everything the floor is allowed to free is still freed"
    );
}

#[tokio::test]
async fn configurable_compaction_1m_window_does_not_compact_above_128k() {
    let provider = ScriptedProvider::from_text_responses(vec!["should not be called"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("wide-context-token ".repeat(150_000)),
        ChatMessage::assistant("ack"),
    ];
    let token_estimate = estimate_history_tokens(&history);
    assert!(
        token_estimate > 128_000,
        "fixture must exceed the old 128K posture, got {token_estimate}"
    );
    assert!(
        token_estimate < 1_000_000usize.saturating_sub(4_096),
        "fixture must remain below the 1M reserve-adjusted trigger, got {token_estimate}"
    );

    let before_roles: Vec<String> = history.iter().map(|msg| msg.role.clone()).collect();
    let before_contents: Vec<String> = history.iter().map(|msg| msg.content.clone()).collect();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 4_096,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 1_000_000,
        max_context_tokens_explicit: false,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config, None, "test")
        .await
        .unwrap();
    assert!(!compacted);
    assert_eq!(
        history.iter().map(|msg| msg.role.clone()).collect::<Vec<_>>(),
        before_roles
    );
    assert_eq!(
        history.iter().map(|msg| msg.content.clone()).collect::<Vec<_>>(),
        before_contents
    );
}

#[test]
fn count_tokens_empty_is_zero() {
    assert_eq!(count_tokens(""), 0);
}

#[test]
fn count_tokens_nonempty_is_positive() {
    assert!(count_tokens("hello world") > 0);
}

#[cfg(feature = "llm-router")]
#[test]
fn count_tokens_matches_known_cl100k_count() {
    // Under cl100k_base, "hello world" tokenizes to exactly 2 tokens, while
    // the crude chars/4 heuristic would yield 11/4 = 2 by floor but 3 by
    // div_ceil. Asserting the real tokenizer value proves tiktoken is wired
    // in (not silently falling back to the char heuristic).
    assert_eq!(count_tokens("hello world"), 2);
}

#[cfg(feature = "llm-router")]
#[test]
fn count_tokens_subword_split_beats_char_heuristic() {
    // "tiktoken" is one 8-char word, so the crude chars/4 heuristic yields
    // 8/4 = 2. cl100k_base BPE splits it into 3 subword tokens. Asserting 3
    // simultaneously pins real BPE subword behaviour AND proves the result
    // diverges from the chars/4 heuristic (which no fallback could produce).
    assert_eq!(count_tokens("tiktoken"), 3);
    assert_ne!(count_tokens("tiktoken"), "tiktoken".chars().count() / 4);
}

#[cfg(feature = "llm-router")]
#[test]
fn plan_context_budget_uses_cl100k_proxy_for_hard_budget() {
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 1,
        max_context_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![ChatMessage::user("tiktoken")];

    let budget = plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD);

    assert_eq!(budget.measurement_source, TokenMeasurementSource::Cl100kProxy);
    assert_eq!(count_tokens("tiktoken"), 3);
    assert!(
        budget.measured_tokens >= 7,
        "role + content + framing must be tokenized"
    );
    assert_eq!(budget.available_input_tokens, 9);
}

#[test]
fn estimate_history_tokens_scales_with_content() {
    let small = vec![ChatMessage::user("hi")];
    let large = vec![ChatMessage::user(
        "this is a substantially longer user message with many more tokens than the small one",
    )];
    assert!(estimate_history_tokens(&large) > estimate_history_tokens(&small));
}

/// The total the compiled-request event reports and the total the context
/// preflight spends are one number from one function, not two estimates
/// that happen to agree today.
#[test]
fn the_request_estimate_and_the_context_preflight_share_one_estimator() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(WideSchemaTool::with_fields("wide_schema_probe", 1))];
    let tool_specs = tool_specs_of(&tools);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);
    let tool_tokens = budgeted_request_tool_tokens(tool_surface);

    let history = context_budget_fixture_history(tool_tokens.saturating_mul(4));
    let config = context_budget_fixture_config(&history, tool_tokens / 2);
    let usable_window = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    let measurement = estimate_history_tokens_with_source(&history);
    let total = estimate_budgeted_request_tokens(measurement, tool_surface);

    assert!(
        budgeted_tokens(tool_surface.instructions) > 0,
        "the appended prompt sections must be part of the total, or the next assertion is blind to them"
    );
    assert_eq!(
        total,
        budgeted_tokens(measurement).saturating_add(tool_tokens),
        "the request total is the message estimate plus everything the tool surface adds"
    );
    assert!(
        total > usable_window,
        "the fixture must be over budget, or the next assertion is vacuous"
    );
    let reserved = reserve_request_tool_surface(&config, tool_surface);
    assert_eq!(
        plan_context_budget(&history, &reserved, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        total > usable_window,
        "the preflight decides on exactly the total the event reports"
    );
    assert_eq!(
        reserve_request_tool_surface(&config, crate::agent::turn_context::RequestToolMeasurement::NONE).reserve_tokens,
        config.reserve_tokens,
        "a request with no published tool is budgeted exactly as before"
    );
}

/// A prompt-guided provider has no `role = "tool"` envelope: the loop feeds
/// a tool round back as a `[Tool results]` **user** message. From the second
/// iteration on, the newest user message is therefore the loop's own
/// bookkeeping and not the question, and a floor that preserves "the last
/// user message" preserves the bookkeeping while the request is deleted.
#[tokio::test]
async fn the_preflight_floor_preserves_a_prompt_guided_request_not_the_tool_result_block() {
    const QUESTION_MARKER: &str = "prompt-guided-request-marker";

    let provider = SystemSummaryProvider {
        response: "unused: compaction is off in this fixture".to_string(),
    };
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("{QUESTION_MARKER} {}", "question-bulk ".repeat(80))),
        ChatMessage::assistant("Checking that now."),
        ChatMessage::user(format!(
            "{}\n<tool_result name=\"probe\">\nok\n</tool_result>",
            crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
        )),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture must reach the trim, or nothing is preserved"
    );

    let budget = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .expect("test: the preflight keeps the turn on the provider-side fallback");

    assert!(
        history
            .iter()
            .any(|message| message.role == "user" && message.content.contains(QUESTION_MARKER)),
        "the prompt-guided turn's question is the floor of the trim, not its [Tool results] block"
    );
    assert_eq!(
        history
            .iter()
            .filter(|message| message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the request may be restored once, never duplicated"
    );
    assert!(
        budget.over_hard_limit,
        "a preserved request larger than the window is reported as such"
    );
}

/// Summary compaction can remove the current request on its own: the
/// compacted range ends at the first message that may head the hot window,
/// and in a prompt-guided tool round that message is the assistant envelope
/// *after* the request. When the summary then fits the window no trim runs,
/// so a restore that only guards the trim never fires.
#[tokio::test]
async fn the_preflight_restores_a_request_a_compaction_summarized_away() {
    const QUESTION_MARKER: &str = "compaction-only-request-marker";

    let provider = SystemSummaryProvider {
        response:
            "## Decisions\n- keep going\n## Open TODOs\n- answer the question\n## Critical Context\n- SHORT_SUMMARY"
                .to_string(),
    };
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("{QUESTION_MARKER} {}", "question-bulk ".repeat(120))),
        ChatMessage::assistant("Checking that now."),
        ChatMessage::user(format!(
            "{}\n<tool_result name=\"probe\">\nok\n</tool_result>",
            crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
        )),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 260,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };

    let budget = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .expect("test: the preflight completes on the compaction path");

    assert!(
        history
            .iter()
            .any(|message| message.content.starts_with("[Context compacted at")),
        "the fixture must reach summary compaction, or it proves nothing about it"
    );
    assert!(
        history
            .iter()
            .any(|message| message.role == "user" && message.content.contains(QUESTION_MARKER)),
        "a compaction that summarized the request away must put it back"
    );
    assert_eq!(
        history
            .iter()
            .filter(|message| message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the request may be restored once, never duplicated"
    );
    assert!(
        history
            .last()
            .is_some_and(|message| message.content.contains(QUESTION_MARKER)),
        "the request returns at the tail, after the summary and the refresh aside"
    );
    let compacted_only = history
        .get(..history.len().saturating_sub(1))
        .expect("test: the restored request is the last message");
    assert!(
        !plan_context_budget(compacted_only, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "compaction alone brought this fixture under the window, so no trim ran and only the restore can explain the request"
    );
    assert_eq!(
        budget.used_tokens,
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).used_tokens,
        "the returned snapshot must describe the history the restore left behind"
    );
}

/// [`latest_user_query_index`] has no "not found" branch — it falls through
/// to the next-older candidate — and nothing ahead of
/// `ChatMessage::user(&msg.content)` in the channel inbound path guards
/// against empty content. A media-only inbound with no synthesized text
/// therefore makes the floor select the *previous* turn's question and
/// re-ask it over the budget: the model is told to answer something it was
/// not asked this turn, which is worse than not restoring at all.
#[tokio::test]
async fn an_empty_inbound_turn_is_not_handed_the_previous_turns_question() {
    const PREVIOUS_QUESTION_MARKER: &str = "previous-turn-question-marker";

    let provider = SystemSummaryProvider {
        response: "unused: compaction is off in this fixture".to_string(),
    };
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("{PREVIOUS_QUESTION_MARKER} {}", "question-bulk ".repeat(80))),
        ChatMessage::assistant("Answered that one."),
        ChatMessage::user(""),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture must reach the trim, or nothing is removed and nothing can be restored"
    );

    preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .expect("test: the preflight completes on the trim path");

    assert!(
        !history
            .iter()
            .any(|message| message.content.contains(PREVIOUS_QUESTION_MARKER)),
        "the floor protects this turn's request or nothing: an empty inbound message must not \
             resurrect the previous turn's question and have the model answer that instead"
    );
}

/// The same fall-through, reached by content rather than by emptiness: the
/// predicate screens out any `user` message opening with `[Tool`, so a user
/// whose own text starts that way is read as the loop's bookkeeping and the
/// floor selects the previous turn's question instead. The guard is that a
/// real `user` turn below the candidate means the candidate is not this
/// turn's request, whatever it was screened out for.
#[tokio::test]
async fn a_user_turn_opening_with_a_tool_marker_is_not_read_as_bookkeeping() {
    const PREVIOUS_QUESTION_MARKER: &str = "previous-turn-question-marker";
    const CURRENT_TURN_TEXT: &str = "[Toolbox] which of these should I use?";

    let provider = SystemSummaryProvider {
        response: "unused: compaction is off in this fixture".to_string(),
    };
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("{PREVIOUS_QUESTION_MARKER} {}", "question-bulk ".repeat(80))),
        ChatMessage::assistant("Answered that one."),
        ChatMessage::user(CURRENT_TURN_TEXT),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture must reach the trim, or nothing is removed and nothing can be restored"
    );

    preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .expect("test: the preflight completes on the trim path");

    assert!(
        !history
            .iter()
            .any(|message| message.content.contains(PREVIOUS_QUESTION_MARKER)),
        "a turn whose own text opens with `[Tool` must not be answered with the previous turn's \
             question in its place"
    );
    assert!(
        history.iter().any(|message| message.content == CURRENT_TURN_TEXT),
        "premise: the turn's own message is still in history, so the floor had something to \
             protect and chose to protect nothing rather than the wrong thing"
    );
}

/// The same fall-through through the refresh aside's marker. The predicate
/// screens out any `user` message opening with `[Post-compaction`, so the
/// floor's "every `user` message below the candidate is the loop's own"
/// check has to match the marker compaction actually writes, in full: a
/// user whose own text merely opens with the same word must still block
/// the stale capture.
#[tokio::test]
async fn a_user_turn_opening_like_the_refresh_aside_is_not_read_as_bookkeeping() {
    const PREVIOUS_QUESTION_MARKER: &str = "previous-turn-question-marker";
    const CURRENT_TURN_TEXT: &str = "[Post-compaction notes] which should I keep?";

    let provider = SystemSummaryProvider {
        response: "unused: compaction is off in this fixture".to_string(),
    };
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("{PREVIOUS_QUESTION_MARKER} {}", "question-bulk ".repeat(80))),
        ChatMessage::assistant("Answered that one."),
        ChatMessage::user(CURRENT_TURN_TEXT),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 90,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture must reach the trim, or nothing is removed and nothing can be restored"
    );

    preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    .expect("test: the preflight completes on the trim path");

    assert!(
        !history
            .iter()
            .any(|message| message.content.contains(PREVIOUS_QUESTION_MARKER)),
        "a turn whose own text opens with `[Post-compaction` must not be answered with the \
             previous turn's question in its place"
    );
    assert!(
        history.iter().any(|message| message.content == CURRENT_TURN_TEXT),
        "premise: the turn's own message is still in history, so the floor had something to \
             protect and chose to protect nothing rather than the wrong thing"
    );
}

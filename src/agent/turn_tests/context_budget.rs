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
use crate::agent::turn_tests::{budget_exhausted_of, window_for_protected_floor};
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
use crate::tokens::count::count_tokens;
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
    let mut fitted_windows = 0usize;
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
        let limit = config.max_context_tokens.saturating_sub(config.reserve_tokens);
        match crate::agent::context_budget::fit_plain_window(&mut history, limit, "test_fixture") {
            Ok(_) => fitted_windows += 1,
            // The protected floor is larger than this window: a typed exhaustion.
            // The reduced view is still checked below: it must never strand a result.
            Err(crate::agent::turn_contracts::ContextCapacityError::BudgetExhausted { .. }) => {}
            Err(other) => panic!("unexpected fit error at max_context_tokens={max_context_tokens}: {other}"),
        }
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
    assert!(fitted_windows > 0, "the sweep must cover windows whose floor fits");
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

    let Err(error) = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    else {
        panic!("the request alone is over the window, so the preflight must report exhaustion");
    };
    let (needed, limit) = budget_exhausted_of(&error).expect("test: a typed BudgetExhausted");
    assert_eq!(limit, 80, "the window is max_context_tokens minus the reserve");
    assert!(needed > limit, "the floor is the request, which does not fit");
    let final_budget = plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD);

    assert!(
        final_budget.over_hard_limit,
        "the floor is the request, so the history left is over the limit"
    );
    // The unfit summary is not protected: the fit deletes it before it gives up.
    assert!(
        !history.iter().any(|msg| msg.content.contains("SUMMARY_TOO_LARGE")),
        "plain trim floor may delete an unfit summary instead of sending an oversized prompt"
    );
    // The protected floor is the request: it is never trimmed, so exhaustion
    // reports the request instead of a prompt with no question in it.
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

#[test]
fn count_tokens_matches_known_cl100k_count() {
    // Under cl100k_base, "hello world" tokenizes to exactly 2 tokens, while
    // the crude chars/4 heuristic would yield 11/4 = 2 by floor but 3 by
    // div_ceil. Asserting the real tokenizer value proves tiktoken is wired
    // in (not silently falling back to the char heuristic).
    assert_eq!(count_tokens("hello world"), 2);
}

#[test]
fn count_tokens_subword_split_beats_char_heuristic() {
    // "tiktoken" is one 8-char word, so the crude chars/4 heuristic yields
    // 8/4 = 2. cl100k_base BPE splits it into 3 subword tokens. Asserting 3
    // simultaneously pins real BPE subword behaviour AND proves the result
    // diverges from the chars/4 heuristic (which no fallback could produce).
    assert_eq!(count_tokens("tiktoken"), 3);
    assert_ne!(count_tokens("tiktoken"), "tiktoken".chars().count() / 4);
}

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

    let Err(error) = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        None,
        "test_preflight",
    )
    .await
    else {
        panic!("the question alone is over the window: the preflight must report exhaustion");
    };
    let (needed, limit) = budget_exhausted_of(&error).expect("test: a typed BudgetExhausted");
    assert!(needed > limit, "a preserved request larger than the window is reported as such");

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
        ChatMessage::user(format!("older-front-bulk {}", "front-bulk ".repeat(120))),
        ChatMessage::user(format!("{QUESTION_MARKER} {}", "question-bulk ".repeat(120))),
        ChatMessage::assistant("Checking that now."),
        ChatMessage::user(format!(
            "{}\n<tool_result name=\"probe\">\nok\n</tool_result>",
            crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
        )),
    ];
    // The request fits with headroom for a summary; the older front range is
    // what the compaction writes over, so the window is over budget.
    let window = window_for_protected_floor(&[history[0].clone(), history[2].clone()], 10, 150);
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: window,
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

/// The fit fixtures (T-C): each one is sized so exactly one step makes the
/// window fit, which proves that step and nothing before it changed the view.
mod fit_steps {
    use crate::agent::compaction::format_context_handoff_message;
    use crate::agent::context_budget::{FitDemand, FitReport, budgeted_history_tokens, fit_request_window};
    use crate::agent::history::build_native_assistant_history;
    use crate::agent::turn_contracts::ContextCapacityError;
    use crate::agent::turn_history::{RoundTag, ViewSlot};
    use crate::providers::ChatMessage;
    use crate::providers::ToolCall;

    const OUTPUT: &str = "tool-output-line ";

    /// One complete native round of two messages (the envelope, its result).
    fn native_round(batch: u64, complete: bool, output: &str) -> [(ChatMessage, ViewSlot); 2] {
        let call = ToolCall {
            id: format!("call-{batch}"),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
        };
        let envelope = ChatMessage::assistant(build_native_assistant_history("", std::slice::from_ref(&call), None));
        let result = ChatMessage::tool(
            serde_json::json!({"tool_call_id": format!("call-{batch}"), "content": output}).to_string(),
        );
        let tag = |envelope| RoundTag {
            batch,
            envelope,
            complete,
            size: 2,
        };
        [
            (envelope, ViewSlot::Round(tag(true))),
            (result, ViewSlot::Round(tag(false))),
        ]
    }

    fn window(parts: Vec<(ChatMessage, ViewSlot)>) -> (Vec<ChatMessage>, Vec<ViewSlot>) {
        parts.into_iter().unzip()
    }

    fn fit(messages: &mut Vec<ChatMessage>, slots: &mut Vec<ViewSlot>, limit: usize) -> Result<FitReport, ContextCapacityError> {
        fit_request_window(messages, slots, limit, FitDemand::Budget, "fit_steps_test")
    }

    fn total(messages: &[ChatMessage]) -> usize {
        budgeted_history_tokens(messages)
    }

    /// Step A: the window's older turns go first, and nothing else is touched.
    #[test]
    fn step_a_removes_only_the_turns_older_than_the_request() {
        let old = [
            (ChatMessage::user("old question"), ViewSlot::Acknowledged),
            (ChatMessage::assistant("old answer"), ViewSlot::Acknowledged),
        ];
        let rest = [
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        let fitted_view: Vec<ChatMessage> = vec![ChatMessage::system("sys"), ChatMessage::user("current request")];
        let limit = total(&fitted_view);
        let mut all = vec![rest[0].clone()];
        all.extend(old);
        all.push(rest[1].clone());
        let (mut messages, mut slots) = window(all);
        let report = fit(&mut messages, &mut slots, limit).expect("step A fits");
        assert_eq!(report.folded_rounds, 0, "step A folds nothing");
        assert_eq!(report.removed_messages, 0, "step A does not count as C or D");
        assert_eq!(messages.len(), 2, "exactly the older turns went: {messages:?}");
        assert_eq!(messages[1].content, "current request");
    }

    /// Step B: an in-flight round's results are folded, oldest first, and the
    /// round's envelope, its tool_call_id and the newest rounds are left alone.
    #[test]
    fn step_b_folds_the_oldest_eligible_round_and_keeps_the_envelopes() {
        let big = OUTPUT.repeat(40);
        let mut parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        parts.extend(native_round(0, true, &big));
        parts.extend(native_round(1, true, &big));
        parts.extend(native_round(2, true, &big));
        let (mut messages, mut slots) = window(parts);
        let before = total(&messages);
        let report = fit(&mut messages, &mut slots, before - 1).expect("folding the oldest round fits");
        assert_eq!(report.folded_rounds, 1, "one round folds: {report:?}");
        assert_eq!(report.removed_messages, 0);
        assert_eq!(report.removed_rounds, 0);
        assert_eq!(messages.len(), 8, "folding keeps every message");
        let folded: serde_json::Value =
            serde_json::from_str(&messages[3].content).expect("the folded result stays a JSON object");
        assert_eq!(folded["tool_call_id"], "call-0", "tool_call_id is untouched");
        assert!(
            folded["content"]
                .as_str()
                .is_some_and(|content| content.starts_with("[tool result folded to fit the context:")),
            "the content is the fold marker: {folded}"
        );
        assert!(messages[3].content.contains("bytes dropped"), "the marker names the dropped bytes");
        assert!(messages[5].content.contains(OUTPUT), "the newer round keeps its output");
    }

    /// Step C: a whole acknowledged message between the handoff and the request
    /// goes, oldest first; the handoff and tool-shaped messages stay.
    #[test]
    fn step_c_removes_acknowledged_turns_between_the_handoff_and_the_request() {
        let handoff = format_context_handoff_message("{}");
        let parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::assistant(handoff.clone()), ViewSlot::Acknowledged),
            (ChatMessage::user("acked question ".repeat(20)), ViewSlot::Acknowledged),
            (ChatMessage::assistant("acked answer"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        let (mut messages, mut slots) = window(parts);
        let limit = total(&messages) - 1;
        let report = fit(&mut messages, &mut slots, limit).expect("step C fits");
        assert_eq!(report.removed_messages, 1, "one acknowledged turn goes: {report:?}");
        assert_eq!(report.folded_rounds, 0);
        assert!(messages.iter().any(|message| message.content == handoff), "the handoff stays");
        assert!(messages.iter().all(|message| message.content != "acked question ".repeat(20)));
        assert_eq!(messages.last().map(|message| message.content.as_str()), Some("current request"));
    }

    /// Step D: an unprotected round whose fold cannot reduce it is removed
    /// whole, its envelope and every result; the newest two rounds stay.
    #[test]
    fn step_d_removes_a_whole_unprotected_round_when_folding_cannot_reduce_it() {
        let mut parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        // A one-byte result: the fold marker is longer, so folding cannot reduce it.
        parts.extend(native_round(0, true, "x"));
        parts.extend(native_round(1, true, &OUTPUT.repeat(40)));
        parts.extend(native_round(2, true, &OUTPUT.repeat(40)));
        let (mut messages, mut slots) = window(parts);
        let limit = total(&messages) - 1;
        let report = fit(&mut messages, &mut slots, limit).expect("step D fits");
        assert_eq!(report.removed_rounds, 1, "the round goes whole: {report:?}");
        assert_eq!(report.removed_messages, 2, "its envelope and its result both go");
        assert!(messages.iter().all(|message| !message.content.contains("call-0")), "no part of batch 0 remains");
        assert_eq!(messages.len(), 6, "the two newest rounds and the request remain");
    }

    /// Step D2: when the floor does not fit with the unprotected rounds gone,
    /// the protected completed rounds are folded, the older first, and nothing
    /// is removed. A floor that still does not fit after both folds is exhausted.
    #[test]
    fn step_d2_folds_the_protected_rounds_older_first_and_removes_nothing() {
        let build = || {
            let mut parts = vec![
                (ChatMessage::system("sys"), ViewSlot::Acknowledged),
                (ChatMessage::user("current request"), ViewSlot::Acknowledged),
            ];
            parts.extend(native_round(0, true, &OUTPUT.repeat(40)));
            parts.extend(native_round(1, true, &OUTPUT.repeat(40)));
            window(parts)
        };
        // Only the older protected round has to fold for this limit.
        let (mut messages, mut slots) = build();
        let limit = total(&messages) - 1;
        let report = fit(&mut messages, &mut slots, limit).expect("folding the older protected round fits");
        assert_eq!(report.folded_rounds, 1, "only the older protected round folds: {report:?}");
        assert_eq!(report.removed_messages, 0, "D2 removes nothing");
        assert_eq!(messages.len(), 6, "every protected message stays");
        assert!(messages[3].content.contains("bytes dropped"), "the older result is folded");
        assert!(messages[5].content.contains(OUTPUT), "the newest result keeps its output");

        // A limit no fold can reach: both protected results fold, then exhaustion.
        let (mut messages, mut slots) = build();
        let error = fit(&mut messages, &mut slots, 1).expect_err("the folded floor still exceeds one token");
        assert!(matches!(error, ContextCapacityError::BudgetExhausted { limit: 1, needed } if needed > 1));
        assert_eq!(messages.len(), 6, "exhaustion removes nothing");
        assert!(messages[3].content.contains("bytes dropped"), "the older protected result folded");
        assert!(messages[5].content.contains("bytes dropped"), "the newest protected result folded too");
        assert!(messages[2].content.contains("call-0"), "the envelope and its tool_call_id stay");
    }

    /// A round the fit may not touch: an incomplete round is never folded or
    /// removed, so a window that needs it removed is the typed error.
    #[test]
    fn an_incomplete_round_is_never_touched_and_its_floor_is_reported() {
        let mut parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        parts.extend(native_round(0, false, &OUTPUT.repeat(40)));
        let (mut messages, mut slots) = window(parts);
        let before = messages.clone();
        let error = fit(&mut messages, &mut slots, 1).expect_err("the incomplete round is protected");
        assert!(matches!(error, ContextCapacityError::BudgetExhausted { limit: 1, .. }));
        assert_eq!(messages.len(), before.len(), "the incomplete round stays");
        assert_eq!(messages[2].content, before[2].content);
    }

    /// Fewer than two completed rounds: every completed round is protected.
    #[test]
    fn with_fewer_than_two_completed_rounds_every_round_is_protected() {
        let mut parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        parts.extend(native_round(0, true, &OUTPUT.repeat(40)));
        let (mut messages, mut slots) = window(parts);
        let error = fit(&mut messages, &mut slots, 1).expect_err("the only round is protected");
        assert!(matches!(error, ContextCapacityError::BudgetExhausted { .. }));
        assert_eq!(messages.len(), 4, "nothing was folded or removed");
    }

    /// The no-handoff case: step A runs front first up to the request.
    #[test]
    fn without_a_handoff_step_a_runs_front_first_up_to_the_request() {
        let parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::user("first question"), ViewSlot::Acknowledged),
            (ChatMessage::assistant("first answer"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        let (mut messages, mut slots) = window(parts);
        let limit = total(&messages) - 1;
        let report = fit(&mut messages, &mut slots, limit).expect("the oldest turn goes");
        assert!(report.reduced);
        assert_eq!(messages[1].content, "first answer", "the oldest turn went first");
        assert_eq!(messages.last().map(|message| message.content.as_str()), Some("current request"));
    }

    /// A summary the safeguard mode wrote is not protected: it is dropped like
    /// any other older turn.
    #[test]
    fn a_safeguard_summary_is_dropped_like_any_older_turn() {
        let parts = vec![
            (ChatMessage::system("sys"), ViewSlot::Acknowledged),
            (ChatMessage::assistant("[Compaction summary]\nkept facts"), ViewSlot::Acknowledged),
            (ChatMessage::user("current request"), ViewSlot::Acknowledged),
        ];
        let (mut messages, mut slots) = window(parts);
        let limit = total(&messages) - 1;
        fit(&mut messages, &mut slots, limit).expect("the summary goes");
        assert!(messages.iter().all(|message| !message.content.contains("kept facts")));
    }
}

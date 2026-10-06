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
#[cfg(test)]
use crate::agent::compaction::post_compaction_refresh_aside;
use crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER;
use crate::agent::turn_tests::{SystemSummaryProvider, first_orphan_tool_index, tool_roundtrip_history_fixture};
use crate::providers::ChatMessage;

#[tokio::test]
async fn safeguard_compaction_over_tool_roundtrips_keeps_pairs_and_shrinks() {
    let provider = SystemSummaryProvider {
        response:
            "## Decisions\n- keep tool pairing\n## Open TODOs\n- none\n## Critical Context\n- TOOL_SAFEGUARD_SUMMARY"
                .to_string(),
    };
    let mut history = tool_roundtrip_history_fixture(10);
    let before_len = history.len();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 400,
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
        "test_tool_safeguard",
    )
    .await
    .expect("safeguard must not regress on tool-bearing history");

    assert!(!budget.over_hard_limit);
    assert!(history.len() < before_len);
    assert_eq!(
        first_orphan_tool_index(&history),
        None,
        "safeguard must not leave a tool result without its requesting assistant message"
    );
}

#[tokio::test]
async fn legacy_preflight_preserving_trim_keeps_compaction_summary_when_recent_still_over_limit() {
    let provider = SystemSummaryProvider {
        response: "## Decisions\n- preserve summary marker\n## Open TODOs\n- trim recent bulk\n## Critical Context\n- SUMMARY_SURVIVES"
            .to_string(),
    };
    let mut history = vec![ChatMessage::system("sys")];
    for idx in 0..10 {
        history.push(ChatMessage::user(format!(
            "old-or-recent-{idx} {}",
            "bulk-token ".repeat(80)
        )));
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 8,
        memory_flush: false,
        max_context_tokens: 120,
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

    assert!(
        history
            .iter()
            .any(|msg| msg.content.contains("[Context compacted at") && msg.content.contains("SUMMARY_SURVIVES")),
        "preserving fallback must keep the just-created compaction summary"
    );
    assert!(
        !history.iter().any(|msg| msg.content.contains("old-or-recent-8")),
        "recent bulk should be trimmed before deleting the summary"
    );
    // Changed with the current-request floor: the preserving trim still
    // frees every other recent message, but the turn's own request is
    // restored instead of being deleted with them, and the returned
    // snapshot reports that honestly rather than describing a history the
    // provider will not be given.
    assert!(
        history.iter().any(|msg| msg.content.contains("old-or-recent-9")),
        "the current turn's request is the floor of every trim"
    );
    assert!(
        budget.over_hard_limit,
        "a request larger than the literal hard limit 110 is reported as such, not trimmed away"
    );
}

/// The exception, and the only one: a summary compaction that *replaced* a
/// front range does not merely delete, it writes. The summary it leaves
/// covers the conversation the request came after, so the request must land
/// after the summary — the one case where the tail is right and the front
/// would put the question ahead of the recap of what led to it.
#[tokio::test]
async fn a_compaction_that_replaced_a_front_range_returns_the_request_after_the_summary() {
    const QUESTION_MARKER: &str = "compaction-replacement-position-marker";

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
    .expect("test: the preflight completes on the compaction path");

    let summary_index = history
        .iter()
        .position(|message| message.content.starts_with("[Context compacted at"))
        .expect("test: the fixture must reach summary compaction, or it pins nothing about it");
    let request_index = history
        .iter()
        .position(|message| message.content.contains(QUESTION_MARKER))
        .expect("test: a compaction that summarized the request away must put it back");
    assert_eq!(
        history
            .iter()
            .filter(|message| message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the request may be restored once, never duplicated"
    );
    assert!(
        summary_index < request_index,
        "the summary recaps the conversation the request came after, so the request must follow \
             it — restoring above everything left standing would put the question ahead of the recap \
             of what led to it"
    );
    assert_eq!(
        request_index,
        history.len().saturating_sub(1),
        "after a replacing compaction the request returns at the tail, behind the summary and \
             the refresh aside the compaction wrote"
    );
}

/// The refresh aside compaction writes is recognised by the one marker
/// both sides share, so the producer and the matcher cannot drift apart.
#[test]
fn the_refresh_aside_compaction_writes_is_loop_written() {
    let aside = post_compaction_refresh_aside();
    assert!(aside.content.starts_with(POST_COMPACTION_REFRESH_MARKER));
    assert!(is_loop_written_user_note(&aside));
    assert!(!is_loop_written_user_note(&ChatMessage::user(
        "[Post-compaction notes] which should I keep?"
    )));
}

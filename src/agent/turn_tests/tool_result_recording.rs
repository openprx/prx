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
use crate::agent::compaction::extract_document_ingest_refs;
use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, plan_context_budget};
use crate::agent::diagnostics::TOOL_PARSE_LOG_PREVIEW_CHARS;
use crate::providers::ChatMessage;

#[test]
fn extract_document_ingest_refs_parses_large_output_markers() {
    // Size off the constant so it exceeds MAX_TOOL_RESULT_INLINE_CHARS after
    // Behavior-limits Phase 1 raised it.
    let segment = "document reference body ";
    let large = segment.repeat(MAX_TOOL_RESULT_INLINE_CHARS / segment.len() + 100);
    let marker = compact_tool_result_for_history_with_status(
        "shell",
        &large,
        MAX_TOOL_RESULT_INLINE_CHARS,
        "persisted_document_backend",
    );
    let refs = extract_document_ingest_refs(&[ChatMessage::assistant(marker)]);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["source"].as_str(), Some("tool_output"));
    assert_eq!(refs[0]["tool"].as_str(), Some("shell"));
    assert_eq!(refs[0]["status"].as_str(), Some("persisted_document_backend"));
    assert!(
        refs[0]["document_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("tool-output:shell:")
    );
    assert!(
        refs[0]["content_sha256"]
            .as_str()
            .is_some_and(|value| value.len() == 64)
    );
    assert!(refs[0]["chunks"].as_u64().unwrap_or_default() >= 1);
}

#[test]
fn compact_tool_result_for_history_emits_document_reference_for_large_output() {
    let content = format!(
        "{}\n{}",
        "# command output",
        "x".repeat(MAX_TOOL_RESULT_INLINE_CHARS + 512)
    );
    let compacted = compact_tool_result_for_history_with_status(
        "shell",
        &content,
        MAX_TOOL_RESULT_INLINE_CHARS,
        "pending_document_backend",
    );

    assert!(compacted.len() < content.len() + 512);
    assert!(compacted.contains("[document_ingest_ref]"));
    assert!(compacted.contains("document_id: tool-output:shell:"));
    assert!(compacted.contains("source: tool_output"));
    assert!(compacted.contains("vector_anchor: vector://tool-output:shell:"));
    assert!(compacted.contains("status: pending_document_backend"));
    assert!(compacted.contains(&format!("inline_preview_bytes: {MAX_TOOL_RESULT_INLINE_CHARS}")));
}

#[test]
fn compact_tool_result_for_history_preserves_small_output() {
    let content = "small command output";
    assert_eq!(
        compact_tool_result_for_history_with_status(
            "shell",
            content,
            MAX_TOOL_RESULT_INLINE_CHARS,
            "pending_document_backend"
        ),
        content
    );
}

#[test]
fn mcp_tool_results_use_a_smaller_inline_history_budget() {
    let history = vec![ChatMessage::system("sys")];
    assert_eq!(
        tool_result_inline_budget_for_history("mcp_call", &history, None),
        MAX_MCP_TOOL_RESULT_INLINE_CHARS
    );
    assert_eq!(
        tool_result_inline_budget_for_history("mcp__playwright__browser_snapshot", &history, None),
        MAX_MCP_TOOL_RESULT_INLINE_CHARS
    );
    assert_eq!(
        tool_result_inline_budget_for_history("shell", &history, None),
        MAX_TOOL_RESULT_INLINE_CHARS
    );
}

/// The inline cap for a tool result is what is left of the window. Once the
/// request itself is over budget — the state the current-request floor
/// deliberately produces — nothing is left, and a cap of zero turns every
/// tool result of that round into a reference stub with an empty preview.
#[test]
fn the_inline_tool_result_budget_keeps_a_preview_when_the_window_is_spent() {
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("spent-window ".repeat(200)),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 40,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert!(
        plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture must leave no room, or it does not test the saturated cap"
    );

    let cap = tool_result_inline_budget_for_history("shell", &history, Some(&config));
    assert!(
        cap >= TOOL_PARSE_LOG_PREVIEW_CHARS,
        "a spent window still leaves a tool result its smallest useful preview, got {cap}"
    );
    assert!(
        cap <= MAX_TOOL_RESULT_INLINE_CHARS,
        "the absolute resident-memory guard still bounds the cap"
    );
    let compacted = compact_tool_result_for_history_with_status(
        "shell",
        &"tool-output-byte ".repeat(400),
        cap,
        "pending_document_backend",
    );
    assert!(
        compacted.starts_with("tool-output-byte "),
        "the stub must carry a preview of the result, not only a reference to it"
    );
}

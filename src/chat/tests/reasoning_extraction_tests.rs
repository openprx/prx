#![cfg(test)]

//! Tests for `collect_reasoning_from_history_slice` — the P2-12 helper that
//! pulls reasoning content from the agent loop's history JSON and feeds it
//! to the TUI's foldable reasoning card.
use super::*;

fn assistant_json(content: Option<&str>, reasoning: Option<&str>) -> ChatMessage {
    let mut obj = serde_json::json!({"content": content, "tool_calls": []});
    if let Some(rc) = reasoning {
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(rc.to_string()),
            );
        }
    }
    ChatMessage::assistant(obj.to_string())
}

#[test]
fn empty_slice_returns_empty_string() {
    assert_eq!(collect_reasoning_from_history_slice(&[]), "");
}

#[test]
fn plain_assistant_text_is_skipped() {
    let history = vec![ChatMessage::assistant("Just a plain answer.")];
    assert_eq!(collect_reasoning_from_history_slice(&history), "");
}

#[test]
fn extracts_single_reasoning_block() {
    let history = vec![assistant_json(Some("ok"), Some("Step 1: think.\nStep 2: act."))];
    let agg = collect_reasoning_from_history_slice(&history);
    assert_eq!(agg, "Step 1: think.\nStep 2: act.");
}

#[test]
fn aggregates_multiple_reasoning_blocks_with_blank_line_separator() {
    let history = vec![
        assistant_json(Some("a"), Some("first thought")),
        ChatMessage::user("user follow-up"),
        assistant_json(Some("b"), Some("second thought")),
    ];
    let agg = collect_reasoning_from_history_slice(&history);
    assert_eq!(agg, "first thought\n\nsecond thought");
}

#[test]
fn whitespace_only_reasoning_dropped() {
    let history = vec![
        assistant_json(Some("a"), Some("   \n\t  ")),
        assistant_json(Some("b"), Some("real reasoning")),
    ];
    let agg = collect_reasoning_from_history_slice(&history);
    assert_eq!(agg, "real reasoning");
}

#[test]
fn malformed_json_is_safely_skipped() {
    // Pre-filter passes (contains "reasoning_content") but JSON parse fails.
    let history = vec![ChatMessage::assistant(
        "broken json with reasoning_content but not valid".to_string(),
    )];
    // Must not panic and must not surface anything.
    assert_eq!(collect_reasoning_from_history_slice(&history), "");
}

#[test]
fn non_assistant_role_ignored_even_with_reasoning_content() {
    // A user message that happens to contain the literal "reasoning_content"
    // must never leak into the reasoning card.
    let history = vec![ChatMessage::user(
        "{\"reasoning_content\":\"shouldn't appear\"}".to_string(),
    )];
    assert_eq!(collect_reasoning_from_history_slice(&history), "");
}

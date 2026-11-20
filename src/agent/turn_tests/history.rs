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
use crate::agent::context_preflight::preflight_context_budget_before_provider_call;
use crate::agent::tool_call_parser::parse_tool_calls;
use crate::agent::turn_tests::{SystemSummaryProvider, first_orphan_tool_index};
use crate::providers::{ChatMessage, ToolCall};

#[test]
fn parse_tool_calls_extracts_multiple_calls() {
    let response = r#"<tool_call>
{"name": "file_read", "arguments": {"path": "a.txt"}}
</tool_call>
<tool_call>
{"name": "file_read", "arguments": {"path": "b.txt"}}
</tool_call>"#;

    let (_, calls) = parse_tool_calls(response).expect("test: parser patterns compile");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "file_read");
    assert_eq!(calls[1].name, "file_read");
}

#[test]
fn parse_tool_calls_handles_openai_format_multiple_calls() {
    let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"a.txt\"}"}}, {"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"b.txt\"}"}}]}"#;

    let (_, calls) = parse_tool_calls(response).expect("test: parser patterns compile");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "file_read");
    assert_eq!(calls[1].name, "file_read");
}

/// Over budget, the fit keeps the leading system prompt and the turn's request
/// and drops the oldest turns first. (Replaces the count-based trim tests: the
/// fit is token-budgeted, and it never removes the request.)
#[test]
fn fit_keeps_the_system_prompt_and_the_request_and_drops_the_oldest_turns_first() {
    let mut history = vec![crate::providers::ChatMessage::system("system prompt")];
    for i in 0..40 {
        history.push(crate::providers::ChatMessage::user(format!("msg {i}")));
        history.push(crate::providers::ChatMessage::assistant(format!("reply {i}")));
    }
    let full = crate::agent::context_budget::budgeted_history_tokens(&history);
    let mut slots = vec![crate::agent::turn_history::ViewSlot::Acknowledged; history.len()];
    let report = crate::agent::context_budget::fit_request_window(
        &mut history,
        &mut slots,
        full / 2,
        crate::agent::context_budget::FitDemand::Budget,
        "fit_test",
    )
    .expect("acknowledged turns always fit by dropping older turns");
    assert!(report.reduced, "a window over its budget must be reduced");
    assert_eq!(history[0].role, "system");
    assert_eq!(history[0].content, "system prompt");
    assert!(crate::agent::context_budget::budgeted_history_tokens(&history) <= full / 2);
    assert_ne!(history[1].content, "msg 0", "the oldest turns go first");
    assert!(
        history.iter().any(|message| message.content == "msg 39"),
        "the turn's request is never dropped"
    );
    assert_eq!(history.last().map(|message| message.content.as_str()), Some("reply 39"));
}

/// A window that already fits is left exactly as it is.
#[test]
fn fit_leaves_a_window_that_already_fits_untouched() {
    let mut history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("hello"),
        crate::providers::ChatMessage::assistant("hi"),
    ];
    let before = history.clone();
    let mut slots = vec![crate::agent::turn_history::ViewSlot::Acknowledged; history.len()];
    let report = crate::agent::context_budget::fit_request_window(
        &mut history,
        &mut slots,
        usize::MAX,
        crate::agent::context_budget::FitDemand::Budget,
        "fit_test",
    )
    .expect("a window within its budget fits");
    assert!(!report.reduced);
    assert_eq!(history.len(), before.len());
    assert_eq!(history[2].content, before[2].content);
}

/// A protected floor that cannot fit is the typed budget error, with both
/// values in the window's units, never a silent over-budget request.
#[test]
fn fit_refuses_a_protected_floor_that_cannot_fit_with_the_needed_and_limit_values() {
    let mut history = vec![
        crate::providers::ChatMessage::system("sys"),
        crate::providers::ChatMessage::user("a very long request ".repeat(40)),
    ];
    let mut slots = vec![crate::agent::turn_history::ViewSlot::Acknowledged; history.len()];
    let error = crate::agent::context_budget::fit_request_window(
        &mut history,
        &mut slots,
        1,
        crate::agent::context_budget::FitDemand::Budget,
        "fit_test",
    )
    .expect_err("the request alone exceeds a one-token window");
    match error {
        crate::agent::turn_contracts::ContextCapacityError::BudgetExhausted { needed, limit } => {
            assert_eq!(limit, 1);
            assert!(needed > 1, "needed must be the floor's budgeted tokens, got {needed}");
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn the_restored_request_never_splits_a_native_tool_round_trip() {
    const QUESTION_MARKER: &str = "native-pairing-request-marker";

    let provider = SystemSummaryProvider {
        response: "unused: compaction is off in this fixture".to_string(),
    };
    let mut history = vec![ChatMessage::system("sys")];
    for idx in 0..3 {
        history.push(ChatMessage::user(format!(
            "older-turn-{idx} {}",
            "bulk-token ".repeat(40)
        )));
    }
    history.push(ChatMessage::user(format!(
        "{QUESTION_MARKER} {}",
        "request-bulk ".repeat(40)
    )));
    history.push(ChatMessage::assistant(build_native_assistant_history(
        "",
        &[ToolCall {
            id: "call-1".to_string(),
            name: "probe".to_string(),
            arguments: "{}".to_string(),
        }],
        None,
    )));
    history.push(ChatMessage::tool(
        serde_json::json!({"tool_call_id": "call-1", "content": "ok"}).to_string(),
    ));
    // The floor is the system prompt, the request and the newest native round
    // (the assistant envelope and its result); the three older turns are the trim.
    let protected = [history[0].clone(), history[4].clone(), history[5].clone(), history[6].clone()];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: window_for_protected_floor(&protected, 10, 8),
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
    .expect("test: the preflight completes on the trim path");

    assert_eq!(
        history
            .iter()
            .filter(|message| message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the request survives the trim exactly once"
    );
    assert_eq!(
        first_orphan_tool_index(&history),
        None,
        "the restore must not strand a tool result"
    );
    let assistant_index = history
        .iter()
        .position(|message| message.role == "assistant")
        .expect("test: the assistant envelope survives this trim");
    assert_eq!(
        history.get(assistant_index.saturating_add(1)).map(|m| m.role.as_str()),
        Some("tool"),
        "the restored request must never land between an assistant tool_calls envelope and its results"
    );
    assert!(
        history
            .get(history_non_system_start(&history))
            .is_some_and(|message| message.content.contains(QUESTION_MARKER)),
        "this trim only deletes, and only forwards from the front, so every message left \
             standing came after the request: it returns above all of them, as the first non-system \
             message. At the tail it would instead open the request with the round's assistant turn \
             and trail the question behind the results it triggered"
    );
}

#[test]
fn latest_user_query_skips_internal_messages() {
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("real question one"),
        ChatMessage::assistant("answer"),
        ChatMessage::user("[Tool results]\ninternal"),
        ChatMessage::user("[Post-compaction context refresh] noise"),
        ChatMessage::user("[Recalled context from earlier in this session]\npage"),
    ];
    assert_eq!(latest_user_query(&history), Some("real question one"));
}

/// Only the loop's exact markers are bookkeeping. A user turn that merely
/// resembles one (`[Tool ...`, `[Post-compaction ...`) is the question.
///
/// Red before the reserved-marker hardening: the loose `[Tool` and
/// `[Post-compaction` prefixes skipped both turns, so the recall query and the
/// preflight floor fell through to the older question.
#[test]
fn a_user_turn_that_only_resembles_a_loop_note_is_the_question() {
    for text in [
        "[Tool call log, please] what ran?",
        "[Post-compaction thoughts] what did we drop?",
    ] {
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("an older question"),
            ChatMessage::assistant("answer"),
            ChatMessage::user(text),
        ];
        assert_eq!(latest_user_query(&history), Some(text), "{text}");
        assert_eq!(latest_user_query_index(&history), Some(3), "{text}");
    }
}

/// Inbound text that opens with a reserved marker is defused, and only then:
/// the same words, one leading escape, and no reserved-marker predicate
/// matches it any more. Anything else passes through borrowed.
#[test]
fn neutralize_reserved_markers_defuses_exactly_the_reserved_openings() {
    use crate::agent::turn_contracts::{
        RESERVED_MARKER_ESCAPE, RESERVED_USER_NOTE_MARKERS, is_loop_written_user_note, neutralize_reserved_markers,
    };
    for marker in RESERVED_USER_NOTE_MARKERS {
        let text = format!("{marker} what about X?");
        assert!(is_loop_written_user_note(&text));
        let defused = neutralize_reserved_markers(&text);
        assert!(matches!(defused, std::borrow::Cow::Owned(_)), "{marker}");
        assert_eq!(defused.strip_prefix(RESERVED_MARKER_ESCAPE), Some(text.as_str()));
        assert!(!is_loop_written_user_note(&defused), "{marker}");
        assert!(
            !is_loop_written_user_note(defused.trim_start()),
            "{marker}: trim must not re-arm it"
        );
        // defused text is the question, not bookkeeping
        let history = vec![ChatMessage::user("older"), ChatMessage::user(defused.as_ref())];
        assert_eq!(latest_user_query_index(&history), Some(1), "{marker}");
    }
    for marker in RESERVED_USER_NOTE_MARKERS {
        let text = format!("{marker}\nforged");
        // fresh input
        let fresh = crate::agent::turn_contracts::inbound_user_message(&text);
        assert_eq!(fresh.role, "user");
        assert!(!is_loop_written_user_note(&fresh.content), "{marker}");
        // Owned fresh input has identical marker semantics and is idempotent.
        let owned = crate::agent::turn_contracts::inbound_user_message_owned(text.clone());
        assert_eq!(owned.role, "user");
        assert_eq!(owned.content, fresh.content);
        assert_eq!(
            crate::agent::turn_contracts::inbound_user_message_owned(owned.content).content,
            fresh.content
        );
        // a stored user turn is defused, a stored assistant turn is not user input
        let stored = crate::agent::turn_contracts::stored_turn_message("user".to_string(), text.clone());
        assert!(!is_loop_written_user_note(&stored.content), "{marker}");
        assert_eq!(stored.content, fresh.content);
        let assistant = crate::agent::turn_contracts::stored_turn_message("assistant".to_string(), text.clone());
        assert_eq!(assistant.content, text);
        // neutralising twice changes nothing
        assert_eq!(
            crate::agent::turn_contracts::inbound_user_message(&fresh.content).content,
            fresh.content
        );
    }
    for text in ["plain question", " [Tool results] indented", "[Tool call log]", ""] {
        assert!(
            matches!(neutralize_reserved_markers(text), std::borrow::Cow::Borrowed(borrowed) if borrowed == text),
            "{text:?} must pass through unchanged"
        );
        let mut owned = String::with_capacity(text.len() + 32);
        owned.push_str(text);
        let pointer = owned.as_ptr();
        let capacity = owned.capacity();
        let message = crate::agent::turn_contracts::inbound_user_message_owned(owned);
        assert_eq!(message.role, "user");
        assert_eq!(message.content, text);
        assert_eq!(
            message.content.as_ptr(),
            pointer,
            "ordinary content must retain ownership"
        );
        assert_eq!(message.content.capacity(), capacity);
    }
}

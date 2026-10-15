#![cfg(test)]

use super::super::*;
use crate::chat::action::HistoryDir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn s() -> ChatState {
    ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
}

fn has_request_redraw(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
}

fn has_quit(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::Quit))
}

fn has_log_trace(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::LogTrace { .. }))
}

/// F1 fixture: multi-segment thinking stream that crosses the
/// bounded-tail threshold and mixes multi-byte / emoji / ASCII, so char vs
/// byte counting and char-boundary truncation are both exercised.
fn thinking_segments() -> Vec<String> {
    let base = [
        "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}: trace the call chain \u{2192}",
        "1) parse input \u{2705}\n",
        "2) check bounds \u{2014} empty, oversized, invalid UTF-8\n",
        "3) compose the final answer \u{1f600}\n",
    ];
    let mut out = Vec::new();
    for round in 0..6 {
        for seg in &base {
            out.push(format!("[{round}] {seg}"));
        }
    }
    out
}

fn live_draft(state: &ChatState) -> &crate::chat::tui::StreamingDraft {
    state
        .stream
        .primary_streaming_draft()
        .expect("test: streaming draft present")
}

fn bg_summary(id: &str, status: &str) -> crate::chat::sessions::PersistedSessionSummary {
    crate::chat::sessions::PersistedSessionSummary {
        id: id.to_string(),
        seq: 1,
        kind: "agent".to_string(),
        origin: "user".to_string(),
        status: status.to_string(),
        title: "task".to_string(),
        summary: String::new(),
        token_usage_records: Vec::new(),
        created_at: chrono::Utc::now(),
    }
}

fn messages_as_pairs(messages: &[crate::providers::ChatMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect()
}

mod input;
mod session;
mod streaming;

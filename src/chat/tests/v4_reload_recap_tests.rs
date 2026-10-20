#![cfg(test)]

use super::format_reloaded_background_sessions;
use crate::chat::sessions::PersistedSessionSummary;
use crate::llm::route_decision::{MeteredTokenUsageRecord, TokenUsageSource};

fn summary(id: &str, status: &str, title: &str, body: &str) -> PersistedSessionSummary {
    PersistedSessionSummary {
        id: id.to_string(),
        seq: 2,
        kind: "agent".to_string(),
        origin: "user".to_string(),
        status: status.to_string(),
        title: title.to_string(),
        summary: body.to_string(),
        token_usage_records: Vec::new(),
        created_at: chrono::Utc::now(),
    }
}

fn usage_record() -> MeteredTokenUsageRecord {
    MeteredTokenUsageRecord {
        settlement_id: None,
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: TokenUsageSource::Reported,
        cost_usd: Some(0.0004),
    }
}

#[test]
fn recap_has_not_resumed_header_and_one_line_per_session() {
    let sessions = vec![
        summary("a", "completed", "build report", "report ready"),
        summary("b", "interrupted", "long crawl", ""),
    ];
    let out = format_reloaded_background_sessions(&sessions);
    assert!(out.contains("not resumed"), "header must signal nothing was revived");
    assert!(out.contains("completed"));
    assert!(out.contains("build report"));
    assert!(out.contains("report ready"));
    // Interrupted (was-running) session shows as terminal, not running.
    assert!(out.contains("interrupted"));
    assert!(!out.contains("running"));
    // One header line + two session lines.
    assert_eq!(out.lines().count(), 3);
}

#[test]
fn recap_omits_empty_summary_body() {
    let sessions = vec![summary("a", "cancelled", "task", "")];
    let out = format_reloaded_background_sessions(&sessions);
    // No trailing ": " when there is no summary body.
    assert!(out.contains("cancelled — task"));
    assert!(!out.contains("task: "));
    assert!(!out.contains("0 tok"), "missing usage must not render as zero: {out}");
}

#[test]
fn recap_displays_persisted_usage_when_present() {
    let mut session = summary("a", "completed", "metered task", "done");
    session.token_usage_records = vec![usage_record()];

    let out = format_reloaded_background_sessions(&[session]);

    assert!(
        out.contains("completed 150 tok | $0.0004 — metered task: done"),
        "{out}"
    );
}

#[test]
fn recap_tags_model_origin_and_leaves_user_untagged() {
    // Bug-V5-2: the persisted origin must be visible on reload so the
    // operator can tell which sessions the model started for itself.
    let mut user = summary("a", "completed", "user task", "done");
    user.origin = "user".to_string();
    user.seq = 1;
    let mut model = summary("b", "completed", "model task", "done");
    model.origin = "model".to_string();
    model.seq = 2;
    let out = format_reloaded_background_sessions(&[user, model]);
    assert!(
        out.contains("#1 completed — user task"),
        "user line stays untagged: {out}"
    );
    assert!(
        out.contains("#2 [model] completed — model task"),
        "model line is tagged: {out}"
    );
}

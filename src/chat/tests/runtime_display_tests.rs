#![cfg(test)]

use super::*;
use crate::chat::sessions::id::SessionId;
use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};
use crate::chat::sessions::runtime::FinishedSession;

fn ts(value: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .expect("test timestamp")
        .with_timezone(&chrono::Utc)
}

fn usage_record(
    source: crate::llm::route_decision::TokenUsageSource,
) -> crate::llm::route_decision::MeteredTokenUsageRecord {
    crate::llm::route_decision::MeteredTokenUsageRecord {
        settlement_id: None,
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        prompt_tokens: 8_000,
        completion_tokens: 4_300,
        total_tokens: 12_300,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source,
        cost_usd: Some(0.0042),
    }
}

#[cfg(feature = "terminal-tui")]
fn switcher_entry(seq: u64) -> crate::chat::sessions::SwitcherEntry {
    crate::chat::sessions::SwitcherEntry {
        seq,
        kind: ManagedKind::Agent.as_str(),
        origin: SessionOrigin::User.as_str(),
        status: ManagedStatus::Running.as_str(),
        title: format!("session {seq}"),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

#[cfg(feature = "terminal-tui")]
#[test]
fn sessions_tick_helper_refreshes_session_cache() {
    let mut mirror = tui::TuiState::new("provider", "model");

    refresh_sessions_cache(&mut mirror, vec![switcher_entry(1), switcher_entry(2)]);

    assert_eq!(mirror.sessions_cache.len(), 2);
    assert_eq!(mirror.sessions_cache.first().map(|entry| entry.seq), Some(1));
    assert_eq!(mirror.sessions_cache.get(1).map(|entry| entry.seq), Some(2));
}

#[test]
fn sessions_list_rows_include_elapsed() {
    let view = ManagedSessionView {
        id: SessionId::from_run_id("run-elapsed"),
        seq: 9,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::Model,
        title: "index workspace".to_string(),
        status: ManagedStatus::Running,
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:03Z"),
        token_usage_records: Vec::new(),
    };

    let out = format_managed_sessions_list(&[view]);

    assert!(out.contains("#9 agent model running 3s index workspace"), "{out}");
}

#[test]
fn started_session_announcement_promotes_model_agent_to_child_session() {
    let view = ManagedSessionView {
        id: SessionId::from_run_id("run-started"),
        seq: 2,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::Model,
        title: "audit queued input".to_string(),
        status: ManagedStatus::Running,
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:03Z"),
        token_usage_records: Vec::new(),
    };

    assert_eq!(
        format_observed_session_announcement(&view),
        "[agent #2 model running 3s] started: audit queued input"
    );
}

#[test]
fn observed_session_announcement_covers_fast_completed_model_agent() {
    let view = ManagedSessionView {
        id: SessionId::from_run_id("run-fast"),
        seq: 3,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::Model,
        title: "fast reply".to_string(),
        status: ManagedStatus::Completed,
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:01Z"),
        token_usage_records: Vec::new(),
    };

    assert_eq!(
        format_observed_session_announcement(&view),
        "[agent #3 model completed 1s] observed: fast reply"
    );
}

#[test]
fn sessions_list_rows_include_reported_usage() {
    let view = ManagedSessionView {
        id: SessionId::from_run_id("run-metered"),
        seq: 9,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::Model,
        title: "index workspace".to_string(),
        status: ManagedStatus::Completed,
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:03Z"),
        token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
    };

    let out = format_managed_sessions_list(&[view]);

    assert!(
        out.contains("#9 agent model completed 3s 12.3k tok | $0.0042 index workspace"),
        "{out}"
    );
}

#[test]
fn sessions_list_marks_estimated_usage() {
    let mut view = ManagedSessionView {
        id: SessionId::from_run_id("run-estimated"),
        seq: 10,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        title: "summarize".to_string(),
        status: ManagedStatus::Completed,
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:03Z"),
        token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Estimated)],
    };
    if let Some(record) = view.token_usage_records.first_mut() {
        record.cost_usd = None;
    }

    let out = format_managed_sessions_list(&[view]);

    assert!(
        out.contains("#10 agent user completed 3s ~12.3k tok | cost unknown summarize"),
        "{out}"
    );
}

/// A user turn that opens with one of the loop's reserved markers enters the
/// provider history as the user's text, never as a loop note: both the
/// current turn and a stored earlier turn come out defused (no longer
/// opening with the marker) and with the user's words intact.
///
/// Red before the reserved-marker hardening: both kept the marker at the
/// start, so the recall query and the preflight floor skipped them.
#[test]
fn a_user_turn_opening_with_a_reserved_marker_enters_history_defused() {
    for marker in [
        "[Tool results]",
        "[Post-compaction context refresh]",
        "[Recalled context from earlier in this session]",
    ] {
        let earlier = format!("{marker} an earlier question");
        let current = format!("{marker} what about X?");
        let mut session = session::ChatSession::new("provider", "model");
        session.add_user_turn(&earlier);
        session.add_assistant_turn("an answer", Vec::new());
        let history = persisted_history_for_current_turn(&session, "system", &current);
        let users: Vec<&str> = history
            .iter()
            .filter(|message| message.role == "user")
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(users.len(), 2, "{marker}: {users:?}");
        for (content, text) in users.iter().zip([&earlier, &current]) {
            assert!(
                !content.starts_with(marker),
                "{marker}: `{content}` still reads as a loop note"
            );
            assert!(
                content.ends_with(text.as_str()),
                "{marker}: `{content}` lost the user's words"
            );
        }
    }
}

#[test]
fn copy_command_selects_latest_and_numbered_assistant_turns() {
    let mut session = session::ChatSession::new("provider", "model");
    session.add_user_turn("question");
    session.add_assistant_turn("first **markdown**", Vec::new());
    session.add_user_turn("again");
    session.add_assistant_turn("second `raw`", Vec::new());

    let latest = select_copy_content(&session, "/copy").expect("latest copy");
    assert_eq!(latest.content, "second `raw`");
    assert_eq!(latest.ordinal, 1);
    assert!(!latest.truncated);

    let latest_named = select_copy_content(&session, "/copy latest").expect("named latest copy");
    assert_eq!(latest_named.content, "second `raw`");
    assert_eq!(latest_named.ordinal, 1);

    let previous = select_copy_content(&session, "/copy 2").expect("previous copy");
    assert_eq!(previous.content, "first **markdown**");
    assert_eq!(previous.ordinal, 2);
}

#[test]
fn copy_command_validates_args_and_truncates_for_osc52() {
    let mut session = session::ChatSession::new("provider", "model");
    assert_eq!(
        select_copy_content(&session, "/copy").expect_err("no assistant"),
        "No assistant response to copy."
    );
    assert_eq!(
        select_copy_content(&session, "/copy nope").expect_err("usage"),
        "Usage: /copy [latest|N]"
    );

    // `\u{ac00}` is a 3-byte, East Asian Wide char: truncation must stop before it.
    let oversized = format!("{}\u{ac00}", "x".repeat(COPY_OSC52_MAX_BYTES));
    session.add_assistant_turn(&oversized, Vec::new());
    let selected = select_copy_content(&session, "/copy").expect("copy oversized");
    assert!(selected.truncated);
    assert!(selected.content.len() <= COPY_OSC52_MAX_BYTES);
    assert!(selected.content.is_char_boundary(selected.content.len()));
    assert!(
        !selected.content.ends_with('\u{ac00}'),
        "truncate at UTF-8 boundary before wide char"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn mouse_capture_disable_env_parser_is_explicit() {
    for enabled in ["1", "true", "TRUE", "yes", "on", " on "] {
        assert!(mouse_capture_disabled_by_env(Some(enabled)), "{enabled:?}");
    }
    for disabled in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("no")] {
        assert!(!mouse_capture_disabled_by_env(disabled), "{disabled:?}");
    }
    assert!(mouse_capture_enabled_by_env(None));
    assert!(mouse_capture_enabled_by_env(Some("0")));
    assert!(!mouse_capture_enabled_by_env(Some("true")));
}

#[test]
fn completion_announcement_includes_final_elapsed() {
    let fin = FinishedSession {
        seq: 4,
        run_id: "run-finished".to_string(),
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        status: ManagedStatus::Completed,
        summary: "done".to_string(),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:01:03Z"),
        token_usage_records: Vec::new(),
    };

    assert_eq!(format_finished_session_announcement(&fin), "[agent #4 ✓ 1m03s] done");
}

#[test]
fn completion_announcement_includes_elapsed_and_tokens() {
    let fin = FinishedSession {
        seq: 4,
        run_id: "run-finished".to_string(),
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        status: ManagedStatus::Completed,
        summary: "done".to_string(),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:01:03Z"),
        token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
    };

    assert_eq!(
        format_finished_session_announcement(&fin),
        "[agent #4 ✓ 1m03s · 12.3k tok | $0.0042] done"
    );
}

#[test]
fn agent_completion_announcement_uses_only_first_useful_line() {
    let fin = FinishedSession {
        seq: 4,
        run_id: "run-finished".to_string(),
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        status: ManagedStatus::Completed,
        summary: "\n```text\nfirst useful line\nfull output remains in child session".to_string(),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:01:03Z"),
        token_usage_records: Vec::new(),
    };

    assert_eq!(
        format_finished_session_announcement(&fin),
        "[agent #4 ✓ 1m03s] first useful line"
    );
    assert!(fin.summary.contains("full output remains in child session"));
}

#[test]
fn agent_completion_announcement_clamps_summary_to_120_chars() {
    let fin = FinishedSession {
        seq: 4,
        run_id: "run-finished".to_string(),
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        status: ManagedStatus::Completed,
        summary: "x".repeat(CHILD_COMPLETION_SUMMARY_MAX_CHARS + 20),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:01:03Z"),
        token_usage_records: Vec::new(),
    };

    let out = format_finished_session_announcement(&fin);
    let summary = out.split_once("] ").map_or("", |(_, summary)| summary);

    assert_eq!(summary.chars().count(), CHILD_COMPLETION_SUMMARY_MAX_CHARS);
    assert!(summary.ends_with('…'), "{out}");
}

#[test]
fn failed_completion_announcement_uses_cross_and_compact_reason() {
    let fin = FinishedSession {
        seq: 4,
        run_id: "run-finished".to_string(),
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        status: ManagedStatus::Failed,
        summary: "provider failed\nstack trace should stay in child logs".to_string(),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:01:03Z"),
        token_usage_records: Vec::new(),
    };

    assert_eq!(
        format_finished_session_announcement(&fin),
        "[agent #4 ✗ 1m03s] provider failed"
    );
}

#[test]
fn shell_completion_announcement_stays_compact_without_tokens() {
    let fin = FinishedSession {
        seq: 5,
        run_id: "run-shell".to_string(),
        kind: ManagedKind::Shell,
        origin: SessionOrigin::User,
        status: ManagedStatus::Completed,
        summary: "exit 0\nstdout body".to_string(),
        created_at: ts("2026-07-04T12:00:00Z"),
        updated_at: ts("2026-07-04T12:00:03Z"),
        token_usage_records: vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)],
    };

    let out = format_finished_session_announcement(&fin);

    assert_eq!(out, "[shell #5 ✓ 3s] exit 0");
    assert!(!out.contains("tok"), "{out}");
}

#[test]
fn session_logs_render_full_retained_output() {
    let lines = vec![
        "first retained line".to_string(),
        "second retained line\nthird retained line".to_string(),
    ];

    let out = format_session_logs(4, &lines, true);

    assert!(out.contains("Session #4 logs (last 2 lines):"));
    assert!(out.contains("[output truncated]"));
    assert!(out.contains("first retained line"));
    assert!(out.contains("second retained line"));
    assert!(out.contains("third retained line"));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn surface_turn_elapsed_message_stays_out_of_transcript_and_redraw() {
    let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
    let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

    surface_turn_elapsed_message(
        &dispatcher,
        Some(&redraw_tx),
        "completed",
        ts("2026-07-04T12:00:00Z"),
        ts("2026-07-04T12:00:03Z"),
    );

    assert!(
        redraw_rx.try_recv().is_err(),
        "completion timing should not request redraw"
    );
    assert!(
        action_rx.try_recv().is_err(),
        "completion timing should not enter transcript state"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_active_turn_message_uses_reducer_redraw_path() {
    let (dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
    let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

    surface_active_turn_message(&dispatcher, Some(&redraw_tx), "Main input queue: 0 queued, 0 priority.");

    assert!(
        redraw_rx.try_recv().is_ok(),
        "active-turn local output should request redraw"
    );
    let action = action_rx
        .try_recv()
        .expect("active-turn local output should dispatch through reducer");
    assert!(
        matches!(
            action,
            crate::chat::action::Action::SystemMessageAdded { ref text }
                if text == "Main input queue: 0 queued, 0 priority."
        ),
        "unexpected action: {action:?}"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_resume_saved_session_is_deferred_while_provider_turn_pending() {
    let mut deferred = std::collections::VecDeque::new();

    assert!(defer_resume_saved_session_if_provider_turn_pending(
        1,
        &mut deferred,
        "session-a".to_string()
    ));
    assert_eq!(deferred.pop_front().as_deref(), Some("session-a"));

    assert!(!defer_resume_saved_session_if_provider_turn_pending(
        0,
        &mut deferred,
        "session-b".to_string()
    ));
    assert!(
        deferred.is_empty(),
        "no pending turn means resume should be handled immediately"
    );
}

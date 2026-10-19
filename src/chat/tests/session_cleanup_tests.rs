#![cfg(test)]

use super::*;
use crate::chat::sessions::id::SessionId;
use crate::chat::sessions::model::ManagedKind;
use crate::chat::sessions::runtime::{ReapPolicy, ReapedSession};

fn reaped(seq: u64, id: &SessionId, now: chrono::DateTime<chrono::Utc>) -> ReapedSession {
    ReapedSession {
        id: id.clone(),
        seq,
        kind: ManagedKind::Agent,
        summary: crate::chat::sessions::PersistedSessionSummary {
            id: id.as_str().to_string(),
            seq,
            kind: "agent".to_string(),
            origin: "user".to_string(),
            status: "completed".to_string(),
            title: "archived task".to_string(),
            summary: "persisted compact summary".to_string(),
            token_usage_records: Vec::new(),
            created_at: now - chrono::Duration::minutes(20),
        },
        terminal_at: now - chrono::Duration::minutes(20),
        reaped_at: now,
    }
}

#[test]
fn logs_archive_returns_full_in_window_then_reaped_notice() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:00:00Z")
        .expect("test timestamp")
        .with_timezone(&chrono::Utc);
    let id = SessionId::from_run_id("archived-run");
    let mut ring = crate::chat::sessions::SessionRing::with_capacity(10);
    ring.push("full retained output".to_string());
    let mut rings = std::collections::HashMap::from([(id.clone(), ring)]);
    let mut policy = ReapPolicy::default();
    policy.archive_keep_last = 0;
    let mut archive = ReapedSessionLogArchive::default();

    archive.archive_reaped(&[reaped(7, &id, now)], &mut rings, &policy, now);

    let live_window = archive
        .logs_message(7, &policy, now + chrono::Duration::minutes(5))
        .expect("archive hit in window");
    assert!(live_window.contains("Session #7 logs"));
    assert!(live_window.contains("full retained output"));

    let expired = archive
        .logs_message(7, &policy, now + chrono::Duration::minutes(11))
        .expect("compact reaped notice after archive expiry");
    assert!(expired.contains("was reaped"));
    assert!(expired.contains("persisted summary"));
    assert!(expired.contains("persisted compact summary"));
    assert!(
        !expired.contains("full retained output"),
        "expired archive should not dump old full logs"
    );
}

#[test]
fn quit_and_exit_are_shutdown_commands() {
    assert!(is_chat_quit_command("/quit"));
    assert!(is_chat_quit_command("/exit"));
    assert!(!is_chat_quit_command("/quit now"));
    assert!(!is_chat_quit_command(" /quit"));
}

#[tokio::test]
async fn exit_shutdown_helper_calls_shutdown_all_and_records_interrupted_summary() {
    use futures_util::FutureExt as _;
    let task = crate::tools::sessions_spawn::TaskRunControl::observe_fixture_task(tokio::spawn(async {
        std::future::pending::<()>().await;
    }));
    let outcome = std::panic::AssertUnwindSafe(async {
        let run = crate::tools::sessions_spawn::SubAgentRun {
            progress: crate::agent::idle::child_beat(),
            batch_id: None,
            id: "quit-live-agent".to_string(),
            task: "still running".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            status: crate::tools::sessions_spawn::SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: Some(task.abort_handle()),
            process_control: None,
            task_control: Some(task.control()),
            history: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: String::new(),
            spawn_depth: 0,
            delegation: None,
            token_usage_records: Vec::new(),
        };
        let runs = std::sync::Arc::new(tokio::sync::RwLock::new(vec![run]));
        let mut chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::clone(&runs));
        let mut chat_session = session::ChatSession::new("provider", "model");
        let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();

        let report = shutdown_child_sessions_for_exit(&mut chat_sessions, &mut chat_session, &dispatcher).await;

        assert_eq!(report.summaries.len(), 1);
        assert!(
            runs.read().await.is_empty(),
            "/quit exit helper must clear live agent registry"
        );
        assert_eq!(chat_session.background_sessions.len(), 1);
        assert_eq!(
            chat_session
                .background_sessions
                .first()
                .map(|summary| summary.status.as_str()),
            Some(crate::chat::sessions::model::STATUS_INTERRUPTED)
        );
    })
    .catch_unwind()
    .await;
    task.finish(outcome).await;
}

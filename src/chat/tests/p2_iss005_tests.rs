#![cfg(test)]

use super::*;
use crate::chat::sessions::id::SessionId;
use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};

fn session_view(seq: u64) -> ManagedSessionView {
    ManagedSessionView {
        id: SessionId::from_run_id(&format!("run-{seq}")),
        seq,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        title: "inspect build output".to_string(),
        status: ManagedStatus::Running,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
    }
}

#[test]
fn attach_projection_keeps_replay_content_out_of_main_history() {
    let tail_line = "[assistant] historical answer that must stay in viewport".to_string();
    let ring_line = "live delta that must stay in viewport".to_string();
    let meta = session_view(7);

    let projection =
        build_active_session_attach_projection(7, Some(&meta), vec![tail_line.clone()], vec![ring_line.clone()], true);

    assert_eq!(projection.view.seq, 7);
    assert_eq!(projection.view.kind, "agent");
    assert_eq!(projection.view.title, "inspect build output");
    assert!(projection.view.lines.contains(&tail_line));
    assert!(projection.view.lines.contains(&ring_line));
    assert!(projection.view.truncated);

    assert_eq!(
        projection.breadcrumb.lines().count(),
        1,
        "attach writes exactly one main-history breadcrumb"
    );
    assert!(projection.breadcrumb.contains("Attached session #7"));
    assert!(
        !projection.breadcrumb.contains("historical answer") && !projection.breadcrumb.contains("live delta"),
        "main-history breadcrumb must not replay child output: {}",
        projection.breadcrumb
    );
}

#[test]
fn active_session_live_refresh_preserves_nonzero_scroll_offset() {
    let mut ring = crate::chat::sessions::SessionRing::with_capacity(16);
    for i in 0..12 {
        ring.push(format!("line {i}"));
    }
    let current = crate::chat::sessions::ActiveSessionView {
        seq: 3,
        kind: "shell".to_string(),
        title: "watch logs".to_string(),
        lines: vec!["old line".to_string()],
        truncated: false,
        scroll_offset: 2,
    };

    ring.push("new live line".to_string());
    let refreshed = active_session_view_from_ring(current, &ring);

    assert_eq!(
        refreshed.scroll_offset, 2,
        "reviewing older child output must not be yanked back to follow-tail"
    );
    assert!(refreshed.lines.iter().any(|line| line == "new live line"));
}

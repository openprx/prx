#![cfg(test)]

use super::*;
use crate::chat::action::Action;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn session_switch_fail_closes_pending_approvals_and_resets_mirror() {
    let (chat_dispatcher, mut action_rx) = dispatcher::ChatDispatcher::new();
    let approval_router = Arc::new(dispatcher::ApprovalRouter::new());
    let (router_tx, mut router_rx) = tokio::sync::oneshot::channel::<bool>();
    assert!(approval_router.register("tool-live".to_string(), router_tx));

    let (_rewind_tx, rewind_rx) = tokio::sync::oneshot::channel::<bool>();
    let (_apply_tx, apply_rx) = tokio::sync::oneshot::channel::<bool>();
    let mut pending_chat_rewind = Some(PendingChatRewind {
        tool_id: "rewind-live".to_string(),
        target_session: session::ChatSession::new("p", "m"),
        approval_rx: rewind_rx,
    });
    let plan = diff_apply::parse_unified_diff(
        "diff --git a/foo.txt b/foo.txt\n--- a/foo.txt\n+++ b/foo.txt\n@@ -1 +1 @@\n-old\n+new\n",
    )
    .expect("valid diff fixture");
    let mut pending_diff_apply = Some(PendingDiffApply {
        tool_id: "diff-live".to_string(),
        plan,
        approval_rx: apply_rx,
    });

    let mut chat_session = session::ChatSession::new("p", "m");
    chat_session.id = "current-session".to_string();
    chat_session.add_user_turn("current");
    let mut loaded_session = session::ChatSession::new("p", "m");
    loaded_session.id = "loaded-session".to_string();
    loaded_session.title = "Loaded".to_string();
    loaded_session.add_user_turn("hello");
    loaded_session.add_assistant_turn("hi", vec![]);
    let mut chat_session_key = "chat:current-session".to_string();
    let mut fabric_turn_seq = 9;
    let mut history = vec![ChatMessage::user("old history")];
    let active_runs = Arc::new(RwLock::new(Vec::<crate::tools::sessions_spawn::SubAgentRun>::new()));
    let mut chat_sessions = crate::chat::sessions::ChatSessionsHandle::new(active_runs);
    let mut ignored_session_events = std::collections::HashSet::new();
    let mut session_rings = std::collections::HashMap::new();
    let mut reported_sessions = std::collections::HashSet::new();
    let mut announced_started_sessions = std::collections::HashSet::from(["stale-started".to_string()]);
    let mut last_sessions_summary = "stale summary".to_string();
    let mut last_sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
        seq: 1,
        kind: "agent",
        origin: "model",
        status: "running",
        title: "stale".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    let mut attached_follow = None;
    let mut attached_follow_seq = Some(7);
    let config = Config::default();
    let skills: Vec<crate::skills::Skill> = Vec::new();
    let chat_mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
    {
        let mut mirror = chat_mirror.lock();
        mirror.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            task_id: None,
            tool_id: "tool-live".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
            selected_approval: false,
        });
        mirror.context_used_tokens = Some(2_500);
        mirror.context_window_tokens = Some(10_000_000);
        mirror.external_editor_prefix_armed = true;
        mirror.input.set_text("draft");
        assert!(mirror.input.begin_or_cycle_reverse_search());
        mirror.focus = crate::chat::sessions::FocusTarget::Approval;
    }

    apply_chat_session_switch(
        ChatSwitchCtx {
            chat_session: &mut chat_session,
            chat_session_key: &mut chat_session_key,
            fabric_turn_seq: &mut fabric_turn_seq,
            history: &mut history,
            approval_router: Some(&approval_router),
            pending_chat_rewind: &mut pending_chat_rewind,
            pending_diff_apply: &mut pending_diff_apply,
            chat_sessions: &mut chat_sessions,
            ignored_session_events: &mut ignored_session_events,
            session_rings: &mut session_rings,
            reported_sessions: &mut reported_sessions,
            announced_started_sessions: &mut announced_started_sessions,
            last_sessions_summary: &mut last_sessions_summary,
            last_sessions_entries: &mut last_sessions_entries,
            attached_follow: &mut attached_follow,
            attached_follow_seq: &mut attached_follow_seq,
            chat_dispatcher: &chat_dispatcher,
            redraw_handle: None,
            config: &config,
            provider_name: "p",
            model_name: "m",
            skills: &skills,
            native_tools: false,
            chat_mirror: &chat_mirror,
        },
        loaded_session,
    )
    .await;

    assert_eq!(router_rx.try_recv(), Ok(false), "router approval must be denied");
    assert!(pending_chat_rewind.is_none());
    assert!(pending_diff_apply.is_none());
    assert!(!approval_router.has_pending());
    assert_eq!(chat_session.id, "loaded-session");
    assert_eq!(chat_session_key, "chat:loaded-session");
    assert_eq!(fabric_turn_seq, 1);
    assert!(announced_started_sessions.is_empty());
    assert!(last_sessions_summary.is_empty(), "expected an empty value");
    assert!(last_sessions_entries.is_empty(), "expected an empty value");
    assert!(attached_follow.is_none());
    assert!(attached_follow_seq.is_none());
    {
        let mirror = chat_mirror.lock();
        assert!(mirror.pending_tool_approval.is_none());
        assert_eq!(mirror.context_used_tokens, None);
        assert_eq!(mirror.context_window_tokens, None);
        assert!(!mirror.external_editor_prefix_armed);
        assert!(!mirror.input.is_reverse_search_active());
        assert_eq!(mirror.focus, crate::chat::sessions::FocusTarget::Main);
        assert_eq!(mirror.turn_count, 2);
    }

    let mut received = Vec::new();
    let mut saw_clear = false;
    while let Ok(action) = action_rx.try_recv() {
        match action {
            Action::ToolApprovalReceived { tool_id, approved } => received.push((tool_id, approved)),
            Action::ToolApprovalCleared => saw_clear = true,
            _ => {}
        }
    }
    assert!(received.contains(&("tool-live".to_string(), false)));
    assert!(received.contains(&("rewind-live".to_string(), false)));
    assert!(received.contains(&("diff-live".to_string(), false)));
    assert!(saw_clear, "switch must clear reducer approval display");
}

#[test]
fn approval_in_progress_covers_router_rewind_and_diff_apply() {
    let router = Arc::new(dispatcher::ApprovalRouter::new());
    let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
    assert!(router.register("tool-live".to_string(), tx));
    assert!(approval_in_progress(Some(&router), &None, &None));

    let (_tx, rewind_rx) = tokio::sync::oneshot::channel::<bool>();
    let pending_rewind = Some(PendingChatRewind {
        tool_id: "rewind-live".to_string(),
        target_session: session::ChatSession::new("p", "m"),
        approval_rx: rewind_rx,
    });
    assert!(approval_in_progress(None, &pending_rewind, &None));
}

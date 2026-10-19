//! Chat session switching: resume, new session and approval clearing.

use super::session_store::{
    bind_session_to_runtime_provider_model, history_for_session_with_system, load_session_by_id_with_message_events,
    save_session,
};
#[cfg(feature = "terminal-tui")]
use super::tui;
use super::{commands, diff_apply, dispatcher, session};
use crate::config::Config;
use crate::memory::Memory;
use crate::providers::ChatMessage;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) struct ChatSwitchCtx<'a> {
    pub(super) chat_session: &'a mut session::ChatSession,
    pub(super) chat_session_key: &'a mut String,
    pub(super) fabric_turn_seq: &'a mut u64,
    pub(super) history: &'a mut Vec<ChatMessage>,
    pub(super) approval_router: Option<&'a Arc<dispatcher::ApprovalRouter>>,
    pub(super) pending_chat_rewind: &'a mut Option<PendingChatRewind>,
    pub(super) pending_diff_apply: &'a mut Option<PendingDiffApply>,
    pub(super) chat_sessions: &'a mut crate::chat::sessions::ChatSessionsHandle,
    pub(super) ignored_session_events: &'a mut std::collections::HashSet<crate::chat::sessions::id::SessionId>,
    pub(super) session_rings:
        &'a mut std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    pub(super) reported_sessions: &'a mut std::collections::HashSet<String>,
    pub(super) announced_started_sessions: &'a mut std::collections::HashSet<String>,
    pub(super) last_sessions_summary: &'a mut String,
    pub(super) last_sessions_entries: &'a mut Vec<crate::chat::sessions::SwitcherEntry>,
    pub(super) attached_follow: &'a mut Option<crate::chat::sessions::id::SessionId>,
    pub(super) attached_follow_seq: &'a mut Option<u64>,
    pub(super) chat_dispatcher: &'a dispatcher::ChatDispatcher,
    pub(super) redraw_handle: Option<&'a mpsc::Sender<()>>,
    pub(super) config: &'a Config,
    pub(super) provider_name: &'a str,
    pub(super) model_name: &'a str,
    pub(super) skills: &'a [crate::skills::Skill],
    pub(super) native_tools: bool,
    #[cfg(feature = "terminal-tui")]
    pub(super) chat_mirror: &'a Arc<parking_lot::Mutex<tui::TuiState>>,
}

pub(super) struct PendingChatRewind {
    pub(super) tool_id: String,
    pub(super) target_session: session::ChatSession,
    pub(super) approval_rx: tokio::sync::oneshot::Receiver<bool>,
}

pub(super) enum RewindApprovalOutcome {
    Apply,
    Cancelled(String),
}

pub(super) fn resolve_rewind_approval(
    tool_id: &str,
    approval: std::result::Result<bool, tokio::sync::oneshot::error::RecvError>,
) -> RewindApprovalOutcome {
    match approval {
        Ok(true) => RewindApprovalOutcome::Apply,
        Ok(false) => RewindApprovalOutcome::Cancelled("Rewind cancelled; current session unchanged.".to_string()),
        Err(_) => RewindApprovalOutcome::Cancelled(format!(
            "Rewind cancelled; approval channel closed for {tool_id} and current session is unchanged."
        )),
    }
}

pub(super) struct PendingDiffApply {
    pub(super) tool_id: String,
    pub(super) plan: diff_apply::DiffApplyPlan,
    pub(super) approval_rx: tokio::sync::oneshot::Receiver<bool>,
}

pub(super) fn approval_in_progress(
    approval_router: Option<&Arc<dispatcher::ApprovalRouter>>,
    pending_chat_rewind: &Option<PendingChatRewind>,
    pending_diff_apply: &Option<PendingDiffApply>,
) -> bool {
    pending_chat_rewind.is_some()
        || pending_diff_apply.is_some()
        || approval_router.is_some_and(|router| router.has_pending())
}

pub(super) const fn approval_already_pending_message() -> &'static str {
    "Another approval is already pending; approve or cancel it first."
}

fn clear_pending_approvals_for_session_switch(ctx: &mut ChatSwitchCtx<'_>) -> usize {
    let mut cleared_ids = Vec::new();
    if let Some(router) = ctx.approval_router {
        for tool_id in router.resolve_all(false) {
            cleared_ids.push(tool_id);
        }
    }
    if let Some(pending) = ctx.pending_chat_rewind.take() {
        if !cleared_ids.iter().any(|existing| existing == &pending.tool_id) {
            cleared_ids.push(pending.tool_id);
        }
    }
    if let Some(pending) = ctx.pending_diff_apply.take() {
        if !cleared_ids.iter().any(|existing| existing == &pending.tool_id) {
            cleared_ids.push(pending.tool_id);
        }
    }
    for tool_id in &cleared_ids {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::ToolApprovalReceived {
                tool_id: tool_id.clone(),
                approved: false,
            },
            "chat.session_switch_fail_closed_approval",
        );
    }
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ToolApprovalCleared,
        "chat.session_switch_clear_approval_view",
    );
    #[cfg(feature = "terminal-tui")]
    {
        let mut mirror = ctx.chat_mirror.lock();
        mirror.pending_tool_approval = None;
        if matches!(mirror.focus, crate::chat::sessions::FocusTarget::Approval) {
            mirror.focus = crate::chat::sessions::FocusTarget::Main;
        }
    }
    cleared_ids.len()
}

#[cfg(feature = "terminal-tui")]
pub(super) fn request_diff_apply_approval(
    plan: diff_apply::DiffApplyPlan,
    interactive_tui: bool,
    approval_router: Option<&Arc<dispatcher::ApprovalRouter>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) -> Result<PendingDiffApply, String> {
    if !interactive_tui {
        return Err(
            "Diff apply requires interactive TUI approval; unavailable in this mode. Workspace unchanged.".to_string(),
        );
    }
    let Some(router) = approval_router else {
        return Err(
            "Diff apply requires interactive TUI approval; approval router unavailable. Workspace unchanged."
                .to_string(),
        );
    };
    let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<bool>();
    let tool_id = format!("diff_apply:{}", uuid::Uuid::new_v4());
    if !router.register(tool_id.clone(), approval_tx) {
        return Err(approval_already_pending_message().to_string());
    }
    let args = plan.approval_args_json();
    let dispatch_result = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ToolApprovalRequested {
            task_id: None,
            tool_id: tool_id.clone(),
            name: "apply_fenced_diff".to_string(),
            args,
        },
        "chat.diff_apply_approval_requested",
    );
    if dispatch_result != crate::chat::dispatcher::DispatchResult::Sent {
        let _ = router.resolve(&tool_id, false);
        return Err("Diff apply approval could not be shown; workspace unchanged.".to_string());
    }
    Ok(PendingDiffApply {
        tool_id,
        plan,
        approval_rx,
    })
}

/// Out-of-band control requests from the TUI key handler to the chat loop.
/// Only the TUI sends them, so without `terminal-tui` the channel carries
/// nothing and the enum has no variants.
#[derive(Debug)]
pub(super) enum ChatControlEvent {
    #[cfg(feature = "terminal-tui")]
    ResumeSavedSession { id: String },
}

pub(super) async fn resume_saved_session_by_id(
    mem: &dyn Memory,
    target_id: &str,
    ctx: ChatSwitchCtx<'_>,
) -> Result<String> {
    let current_child_summaries = ctx
        .chat_sessions
        .snapshot()
        .await
        .iter()
        .map(|view| crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new()))
        .collect::<Vec<_>>();
    let mut current_to_save = ctx.chat_session.clone();
    for summary in &current_child_summaries {
        current_to_save.record_background_session(summary.clone());
    }

    if let Err(e) = save_session(mem, &current_to_save).await {
        anyhow::bail!("Resume aborted: failed to save current session before switching: {e}");
    }
    for summary in &current_child_summaries {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded {
                summary: summary.clone(),
            },
            "chat.resume_record_child_summary_before_switch",
        );
    }

    let workspace_id = ctx.config.workspace_dir.to_string_lossy();
    let loaded_session =
        match load_session_by_id_with_message_events(mem, workspace_id.as_ref(), target_id, &ctx.config.cost).await {
            Ok(Some(session)) => session,
            Ok(None) => anyhow::bail!("Saved chat session '{target_id}' not found."),
            Err(e) => {
                anyhow::bail!("Resume aborted: failed to load saved chat session '{target_id}': {e}")
            }
        };
    let loaded_id = loaded_session.id.clone();
    let loaded_turns = loaded_session.turn_count();
    let loaded_title = if loaded_session.title.is_empty() {
        "(untitled)".to_string()
    } else {
        loaded_session.title.clone()
    };

    apply_chat_session_switch(ctx, loaded_session).await;
    Ok(format!(
        "Resumed saved chat session {loaded_id} ({loaded_title}, {loaded_turns} turns)."
    ))
}

pub(super) async fn start_new_chat_session(mem: &dyn Memory, ctx: ChatSwitchCtx<'_>) -> Result<String> {
    let current_child_summaries = ctx
        .chat_sessions
        .snapshot()
        .await
        .iter()
        .map(|view| crate::chat::sessions::PersistedSessionSummary::from_view(view, String::new()))
        .collect::<Vec<_>>();
    let mut current_to_save = ctx.chat_session.clone();
    for summary in &current_child_summaries {
        current_to_save.record_background_session(summary.clone());
    }

    if let Err(e) = save_session(mem, &current_to_save).await {
        anyhow::bail!("New session aborted: failed to save current session: {e}");
    }
    for summary in &current_child_summaries {
        let _ = ctx.chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::BackgroundSessionRecorded {
                summary: summary.clone(),
            },
            "chat.new_record_child_summary_before_switch",
        );
    }

    #[cfg(feature = "terminal-tui")]
    let current_mode = ctx.chat_mirror.lock().chat_mode;
    #[cfg(not(feature = "terminal-tui"))]
    let current_mode = ctx.chat_session.mode;

    let previous_id = current_to_save.id.clone();
    let new_session = fresh_chat_session(ctx.provider_name, ctx.model_name, current_mode);
    let new_id = new_session.id.clone();
    if let Err(e) = save_session(mem, &new_session).await {
        anyhow::bail!("New session aborted: failed to persist new session: {e}");
    }

    apply_chat_session_switch(ctx, new_session).await;
    Ok(format!(
        "Started new chat session {new_id}. Previous session {previous_id} was saved."
    ))
}

pub(super) fn fresh_chat_session(
    provider_name: &str,
    model_name: &str,
    mode: commands::ChatMode,
) -> session::ChatSession {
    let mut session = session::ChatSession::new(provider_name, model_name);
    session.set_mode(mode);
    session
}

pub(super) async fn apply_chat_session_switch(mut ctx: ChatSwitchCtx<'_>, mut loaded_session: session::ChatSession) {
    let cleared_approvals = clear_pending_approvals_for_session_switch(&mut ctx);
    if cleared_approvals > 0 {
        tracing::warn!(
            cleared_approvals,
            "session switch resolved pending approvals fail-closed before swapping state"
        );
    }
    let (_detached_summaries, ignored_ids) = ctx.chat_sessions.detach_for_chat_session_switch().await;
    ctx.ignored_session_events.extend(ignored_ids);
    ctx.session_rings.clear();
    ctx.reported_sessions.clear();
    ctx.announced_started_sessions.clear();
    ctx.last_sessions_summary.clear();
    ctx.last_sessions_entries.clear();
    *ctx.attached_follow = None;
    *ctx.attached_follow_seq = None;

    bind_session_to_runtime_provider_model(&mut loaded_session, ctx.provider_name, ctx.model_name);
    *ctx.chat_session = loaded_session;
    *ctx.chat_session_key = format!("chat:{}", ctx.chat_session.id);
    *ctx.fabric_turn_seq = ctx
        .chat_session
        .turns
        .iter()
        .filter(|turn| turn.role == "user")
        .fold(0_u64, |acc, _| acc.saturating_add(1));
    *ctx.history = history_for_session_with_system(
        ctx.chat_session,
        ctx.config,
        ctx.model_name,
        ctx.skills,
        ctx.native_tools,
    );

    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionLoaded(ctx.chat_session.clone()),
        "chat.session_switch_loaded",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionFocusChanged {
            focus: crate::chat::sessions::FocusTarget::Main,
        },
        "chat.session_switch_focus_main",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
        "chat.session_switch_clear_active_session_view",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionsStatusUpdated { summary: String::new() },
        "chat.session_switch_clear_sessions_status",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SessionsEntriesUpdated { entries: Vec::new() },
        "chat.session_switch_clear_sessions_entries",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SwitcherClosed,
        "chat.session_switch_switcher_closed",
    );
    let _ = ctx.chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::SavedSessionPickerClosed,
        "chat.session_switch_saved_session_picker_closed",
    );

    #[cfg(feature = "terminal-tui")]
    {
        let mut mirror = ctx.chat_mirror.lock();
        mirror.session_title = ctx.chat_session.title.clone();
        mirror.turn_count = ctx.chat_session.turn_count();
        mirror.chat_mode = ctx.chat_session.mode;
        mirror.autonomy_level = ctx.config.autonomy.level;
        mirror.conversation_lines = conversation_lines_for_resumed_session(ctx.chat_session);
        mirror.streaming = None;
        mirror.sessions_status.clear();
        mirror.sessions_cache.clear();
        mirror.active_session_view = None;
        mirror.pending_tool_approval = None;
        mirror.context_used_tokens = None;
        mirror.context_window_tokens = None;
        mirror.token_usage_summary = ctx.chat_session.token_usage_summary();
        mirror.external_editor_prefix_armed = false;
        mirror.input.clear_navigation_state();
        mirror.focus = crate::chat::sessions::FocusTarget::Main;
        mirror.switcher = None;
        mirror.saved_session_picker = None;
    }
    if let Some(tx) = ctx.redraw_handle {
        let _ = tx.try_send(());
    }
}

pub(super) fn should_ignore_session_event_after_chat_resume(
    ignored_session_events: &std::collections::HashSet<crate::chat::sessions::id::SessionId>,
    event: &crate::chat::sessions::SessionEvent,
) -> bool {
    ignored_session_events.contains(event.session_id())
}

#[cfg(feature = "terminal-tui")]
fn conversation_lines_for_resumed_session(session: &session::ChatSession) -> Vec<tui::ConversationLine> {
    session
        .turns
        .iter()
        .filter_map(|turn| match turn.role.as_str() {
            "user" => Some(tui::ConversationLine::User {
                content: turn.content.clone(),
            }),
            "assistant" => Some(tui::ConversationLine::Assistant {
                content: turn.content.clone(),
            }),
            "system" => Some(tui::ConversationLine::System {
                content: turn.content.clone(),
            }),
            _ => None,
        })
        .collect()
}

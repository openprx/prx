#![cfg(test)]

use super::super::*;
use crate::chat::action::Action;
use crate::providers::ChatMessage;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

/// S2-C-2: redux_history_append_order_user_assistant_stable
///
/// After dispatching SetLeadingSystemPrompt + RecordUserTurn + RecordAssistantTurn, session.history
/// must stay ordered as [system, user, assistant] and be byte-identical to the same legacy
/// `history.push` sequence. It checks the reducer neither reorders nor skips a push.
#[test]
fn redux_history_append_order_user_assistant_stable() {
    let mut state = s();
    assert!(state.session.history.is_empty());

    // SetLeadingSystemPrompt on an empty history must push (equivalent to legacy
    // `if history.is_empty() { push }`)
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "system-rules".to_string(),
    });
    assert_eq!(state.session.history.len(), 1);
    let h0 = state.session.history.first().expect("test: history[0] after push");
    assert_eq!(h0.role, "system");

    // A second SetLeadingSystemPrompt (typical: it runs every turn) must replace the front, not append
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "system-rules-v2".to_string(),
    });
    assert_eq!(
        state.session.history.len(),
        1,
        "a second SetLeadingSystemPrompt call must upsert the front, never append"
    );
    let h0v2 = state.session.history.first().expect("test: history[0] after upsert");
    assert_eq!(h0v2.content, "system-rules-v2");

    // RecordUserTurn → append user
    let _ = state.reduce(Action::RecordUserTurn("user-q1".to_string()));
    assert_eq!(state.session.history.len(), 2);
    let h1 = state.session.history.get(1).expect("test: history[1] = user");
    assert_eq!(h1.role, "user");
    assert_eq!(h1.content, "user-q1");
    assert_eq!(state.session.turns.len(), 1, "session.turns must grow too (user)");

    // RecordAssistantTurn → append assistant
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "assistant-r1".to_string(),
    });
    assert_eq!(state.session.history.len(), 3);
    let h2 = state.session.history.get(2).expect("test: history[2] = assistant");
    assert_eq!(h2.role, "assistant");
    assert_eq!(h2.content, "assistant-r1");
    assert_eq!(state.session.turns.len(), 2, "session.turns +1 (assistant)");

    // one more round — the order must still be system, user, assistant, user, assistant
    let _ = state.reduce(Action::RecordUserTurn("user-q2".to_string()));
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "assistant-r2".to_string(),
    });
    assert_eq!(state.session.history.len(), 5);
    let roles: Vec<&str> = state.session.history.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "user", "assistant"],
        "the order must stay stable"
    );
    // session.turns holds no system (only user/assistant are turns)
    assert_eq!(state.session.turns.len(), 4);
    let turn_roles: Vec<&str> = state.session.turns.iter().map(|t| t.role.as_str()).collect();
    assert_eq!(turn_roles, vec!["user", "assistant", "user", "assistant"]);
}

/// S2-C-3: redux_system_message_mirror_and_state_consistent
///
/// SystemMessageAdded must only touch the UI mirror (ui.conversation_lines) and never pollute
/// session.history; RecordSystemMessage must only touch session.history and never pollute
/// ui.conversation_lines. The two paths are orthogonal.
#[cfg(feature = "terminal-tui")]
#[test]
fn redux_system_message_mirror_and_state_consistent() {
    use crate::chat::tui::ConversationLine;
    let mut state = s();
    assert_eq!(state.ui.conversation_lines.len(), 0);
    assert_eq!(state.session.history.len(), 0);

    // SystemMessageAdded → only touches the UI mirror
    let effects = state.reduce(Action::SystemMessageAdded {
        text: "Banner v1".to_string(),
    });
    assert_eq!(state.ui.conversation_lines.len(), 1, "the ui mirror must grow");
    let first_line = state
        .ui
        .conversation_lines
        .first()
        .expect("test: conversation_lines[0] after SystemMessageAdded");
    assert!(
        matches!(
            first_line,
            ConversationLine::System { content } if content == "Banner v1"
        ),
        "it must be the ConversationLine::System variant"
    );
    assert_eq!(state.session.history.len(), 0, "session.history must stay untouched");
    assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

    // RecordSystemMessage → only touches session.history
    let _ = state.reduce(Action::RecordSystemMessage {
        content: "ctx-system-1".to_string(),
    });
    assert_eq!(state.session.history.len(), 1, "session.history must grow");
    let first_hist = state.session.history.first().expect("test: history[0] = system");
    assert_eq!(first_hist.role, "system");
    assert_eq!(first_hist.content, "ctx-system-1");
    assert_eq!(
        state.ui.conversation_lines.len(),
        1,
        "the ui mirror must stay untouched (still 1 banner line)"
    );

    // send a few more SystemMessageAdded — the UI mirror grows, history stays unchanged
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "Slash output 1".to_string(),
    });
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "Slash output 2".to_string(),
    });
    assert_eq!(state.ui.conversation_lines.len(), 3);
    assert_eq!(state.session.history.len(), 1, "session.history is still 1");
}

/// S2-C-bonus (Codex suggestion): SetLeadingSystemPrompt on a non-empty history must replace the
/// front entry and never append — a regression guard for the 1336 semantics.
#[test]
fn set_leading_system_prompt_replaces_first_instead_of_append() {
    let mut state = s();
    // preload history: [system-old, user1, assistant1]
    state.session.history.push(ChatMessage::system("system-old"));
    state.session.history.push(ChatMessage::user("user1"));
    state.session.history.push(ChatMessage::assistant("assistant1"));
    assert_eq!(state.session.history.len(), 3);

    // SetLeadingSystemPrompt must replace the leading system, never append
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "system-new".to_string(),
    });
    assert_eq!(
        state.session.history.len(),
        3,
        "SetLeadingSystemPrompt must not change the history length (replace, not append)"
    );
    let h0 = state.session.history.first().expect("test: history[0] = system-new");
    assert_eq!(h0.role, "system");
    assert_eq!(h0.content, "system-new");
    // the user / assistant order is unchanged
    let h1 = state.session.history.get(1).expect("test: history[1] = user1");
    assert_eq!(h1.role, "user");
    assert_eq!(h1.content, "user1");
    let h2 = state.session.history.get(2).expect("test: history[2] = assistant1");
    assert_eq!(h2.role, "assistant");
}

#[test]
fn set_leading_system_prompt_preserves_resumed_history_without_system() {
    let mut state = s();
    state.session.history.push(ChatMessage::user("resumed-user"));
    state.session.history.push(ChatMessage::assistant("resumed-assistant"));

    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "system-new".to_string(),
    });

    assert_eq!(state.session.history.len(), 3);
    let h0 = state.session.history.first().expect("test: inserted system prompt");
    assert_eq!(h0.role, "system");
    assert_eq!(h0.content, "system-new");
    let h1 = state.session.history.get(1).expect("test: resumed user preserved");
    assert_eq!(h1.role, "user");
    assert_eq!(h1.content, "resumed-user");
    let h2 = state.session.history.get(2).expect("test: resumed assistant preserved");
    assert_eq!(h2.role, "assistant");
    assert_eq!(h2.content, "resumed-assistant");
}

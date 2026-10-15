#![cfg(test)]

use super::super::*;
use crate::chat::action::{Action, CompactReason};
use crate::providers::ChatMessage;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

/// S2-B-1: redux_cancel_requested_clears_control_and_emits_cancel_effect
///
/// During a single Ctrl+C the reducer must emit `Effect::CancelToken(token)` to really trigger
/// the underlying cancel (replacing the old manual `token.cancel()`), and clear
/// generating/draft/active_cancel. This closes the S2-B Codex risk window.
#[test]
fn redux_cancel_requested_clears_control_and_emits_cancel_effect() {
    let mut state = s();
    let tok = CancellationToken::new();
    // start a turn → control.active_cancel=Some(tok), generating=true
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-s2b-1".to_string(),
        cancel: tok,
    });
    assert!(state.control.generating);
    assert!(state.control.active_cancel.is_some());

    let effects = state.reduce(Action::CancelRequested);

    // the control state must be fully cleared
    assert!(!state.control.generating, "generating=false after CancelRequested");
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft is cleared after CancelRequested"
    );
    assert!(
        state.control.active_cancel.is_none(),
        "active_cancel is cleared after CancelRequested (the token went to Effect::CancelToken)"
    );

    // The Effect list must contain CancelToken (the key one) + CancelDraft + LogTrace + RequestRedraw
    let has_cancel_token = effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
    assert!(
        has_cancel_token,
        "CancelRequested must emit Effect::CancelToken — this is the key S2-B difference"
    );
    let has_cancel_draft = effects
        .iter()
        .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d-s2b-1"));
    assert!(has_cancel_draft, "must contain CancelDraft(draft-id)");
    // CancelToken must come before CancelDraft (really cancel the backend first, then clear the UI)
    let pos_token = effects
        .iter()
        .position(|e| matches!(e, Effect::CancelToken(_)))
        .expect("CancelToken present");
    let pos_draft = effects
        .iter()
        .position(|e| matches!(e, Effect::CancelDraft(_)))
        .expect("CancelDraft present");
    assert!(pos_token < pos_draft, "CancelToken must come before CancelDraft");
}

/// S2-B-2: redux_mode_changed_matches_legacy_chat_session_mode
///
/// During dual-write the reducer's `state.session.mode` must stay in sync with legacy
/// `chat_session.mode`. This test runs the same sequence (set_mode + ModeChanged) against both
/// ChatSession and ChatState and checks the final mode matches.
#[test]
fn redux_mode_changed_matches_legacy_chat_session_mode() {
    use crate::chat::session::ChatSession;
    let mut state = s();
    let mut legacy = ChatSession::new("openai", "gpt-4o-mini");

    // Plan
    let _ = state.reduce(Action::ModeChanged(ChatMode::Plan));
    legacy.set_mode(ChatMode::Plan);
    assert_eq!(state.session.mode, legacy.mode, "the Plan mode must match");
    assert_eq!(state.ui.chat_mode, ChatMode::Plan, "Plan mode must reach the UI status");

    // Auto
    let _ = state.reduce(Action::ModeChanged(ChatMode::Auto));
    legacy.set_mode(ChatMode::Auto);
    assert_eq!(state.session.mode, legacy.mode, "the Auto mode must match");
    assert_eq!(state.ui.chat_mode, ChatMode::Auto, "Auto mode must reach the UI status");

    // Edit (default)
    let _ = state.reduce(Action::ModeChanged(ChatMode::Edit));
    legacy.set_mode(ChatMode::Edit);
    assert_eq!(state.session.mode, legacy.mode, "the Edit mode must match");
    assert_eq!(state.ui.chat_mode, ChatMode::Edit, "Edit mode must reach the UI status");
}

#[test]
fn p8_mode_changed_does_not_escalate_autonomy_or_policy() {
    use crate::config::AutonomyConfig;
    use crate::security::policy::ToolDecision;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    let mut state = s();
    let autonomy = AutonomyConfig {
        level: AutonomyLevel::ReadOnly,
        ..AutonomyConfig::default()
    };
    let autonomy_before = autonomy.clone();
    let policy_before = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
    state.ui.autonomy_level = autonomy.level;

    for mode in [ChatMode::Plan, ChatMode::Edit, ChatMode::Auto, ChatMode::Plan] {
        let _ = state.reduce(Action::ModeChanged(mode));
        assert_eq!(state.ui.autonomy_level, AutonomyLevel::ReadOnly);
    }

    let policy_after = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

    assert_eq!(autonomy.level, autonomy_before.level);
    assert_eq!(autonomy.workspace_only, autonomy_before.workspace_only);
    assert_eq!(autonomy.scopes.rules.len(), autonomy_before.scopes.rules.len());
    assert_eq!(policy_after.autonomy, policy_before.autonomy);
    assert_eq!(state.session.mode, ChatMode::Plan);
    assert_eq!(
        policy_after.decide("file_write", "user", "terminal", "chat"),
        ToolDecision::Deny,
        "ChatMode::Auto cannot widen read_only autonomy because decide() is ChatMode-free"
    );
}

/// BUG-07: the `ModelChanged` reducer updates `session.model` so the status bar immediately
/// reflects the new model, and the new value reaches the UI snapshot (snapshot.model reads session.model).
#[test]
fn redux_model_changed_updates_session_and_snapshot() {
    let mut state = s();
    assert_eq!(&*state.session.model, "gpt-4o-mini", "the initial model");

    let effects = state.reduce(Action::ModelChanged {
        model: "anthropic/claude-sonnet-4".to_string(),
    });
    assert_eq!(
        &*state.session.model, "anthropic/claude-sonnet-4",
        "the model was switched"
    );
    assert!(
        effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
        "ModelChanged must request a redraw to refresh the status bar"
    );

    // snapshot.model comes from session.model; build_ui_snapshot only exists under the
    // terminal-tui feature, so the snapshot assertion is gated on that feature.
    #[cfg(feature = "terminal-tui")]
    {
        let snap = state.build_ui_snapshot(1);
        assert_eq!(
            &*snap.model, "anthropic/claude-sonnet-4",
            "snapshot.model reflects the new model"
        );
    }
}

/// Bug #3: the `ProviderChanged` reducer updates `session.provider` so the status bar
/// `state.provider()` (read from snapshot.provider ← session.provider) reflects the new provider
/// at once. With `model: None`, session.model is left alone.
#[test]
fn redux_provider_changed_updates_session_provider_only() {
    let mut state = s();
    assert_eq!(&*state.session.provider, "openai", "the initial provider");
    assert_eq!(&*state.session.model, "gpt-4o-mini", "the initial model");

    let effects = state.reduce(Action::ProviderChanged {
        provider: "openrouter".to_string(),
        model: None,
    });
    assert_eq!(&*state.session.provider, "openrouter", "the provider was switched");
    assert_eq!(
        &*state.session.model, "gpt-4o-mini",
        "with model: None the model is unchanged"
    );
    assert!(
        effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
        "ProviderChanged must request a redraw to refresh the status bar"
    );

    #[cfg(feature = "terminal-tui")]
    {
        let snap = state.build_ui_snapshot(1);
        assert_eq!(
            &*snap.provider, "openrouter",
            "snapshot.provider reflects the new provider"
        );
    }
}

/// Bug #3: when `ProviderChanged` carries `model: Some(..)` it also syncs session.model
/// (the case where switching provider explicitly passes a compatible model argument).
#[test]
fn redux_provider_changed_with_model_updates_both() {
    let mut state = s();
    let effects = state.reduce(Action::ProviderChanged {
        provider: "anthropic".to_string(),
        model: Some("claude-sonnet-4".to_string()),
    });
    assert_eq!(&*state.session.provider, "anthropic", "the provider was switched");
    assert_eq!(
        &*state.session.model, "claude-sonnet-4",
        "the model was switched along with it"
    );
    assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

    #[cfg(feature = "terminal-tui")]
    {
        let snap = state.build_ui_snapshot(1);
        assert_eq!(&*snap.provider, "anthropic");
        assert_eq!(&*snap.model, "claude-sonnet-4");
    }
}

/// T3-3-d-byte-parity: the reducer's `session.history` and legacy `ChatSession.turns` must
/// reconcile byte for byte in Both mode (the same user/assistant content on both sides).
///
/// This turns the Both-mode "dual-write reconciliation" into an in-process unit test, avoiding
/// PTY comparison noise. Key assertions:
///   1. session.turns.len() == legacy.turns.len() (entry counts line up)
///   2. the role sequence matches exactly
///   3. content is byte-identical (when there is no sanitization difference)
#[test]
fn t3_3d_both_mode_history_byte_level_parity() {
    use crate::chat::session::ChatSession;
    let mut state = s();
    let mut legacy = ChatSession::new("test-prov", "test-model");

    let inputs = [
        ("user", "hi there"),
        ("assistant", "hello!"),
        ("user", "explain monads in 1 sentence"),
        ("assistant", "a monad is a monoid in the category of endofunctors"),
        ("user", ""), // empty string boundary
        (
            "assistant",
            "\u{1f680} unicode \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} mixed content",
        ),
    ];

    for (role, text) in inputs {
        if role == "user" {
            let _ = state.reduce(Action::RecordUserTurn(text.to_string()));
            legacy.add_user_turn(text);
        } else {
            // every role in the test input is "user" / "assistant", so the else branch is assistant
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: text.to_string(),
            });
            legacy.add_assistant_turn(text, Vec::new());
        }
    }

    assert_eq!(
        state.session.turns.len(),
        legacy.turns.len(),
        "Both mode: reducer.session.turns.len() must equal legacy.turns.len()"
    );
    for (i, (lhs, rhs)) in state.session.turns.iter().zip(legacy.turns.iter()).enumerate() {
        assert_eq!(lhs.role, rhs.role, "turn {i} role differs");
        assert_eq!(
            lhs.content.as_bytes(),
            rhs.content.as_bytes(),
            "turn {i} content differs at byte level (reducer vs legacy)"
        );
    }
    // history grows in step with turns
    assert_eq!(
        state.session.history.len(),
        state.session.turns.len(),
        "reducer.session.history.len() must stay in sync with turns.len()"
    );
}

/// T3-3-fixB C1: byte-level parity between the reducer's `session.history` and a legacy manual
/// history on the system dimension. t3_3d_both_mode_history_byte_level_parity only covers
/// user/assistant; this adds SetLeadingSystemPrompt (upsert) + RecordSystemMessage (append).
///
/// The legacy side mirrors a `Vec<ChatMessage>` by hand (ChatSession has no add_system_turn;
/// the chat::run main loop edits the history slice directly), keeping the two byte-aligned.
///
/// Note: the tool_calls parity gap remains (the reducer's RecordAssistantTurn ignores the
/// tool_calls argument and session.turns[i].tool_calls is always Vec::new()); tracked under S2.5.
#[test]
fn t3_3_fix_b_both_parity_system_history() {
    use crate::providers::ChatMessage;
    let mut state = s();
    let mut legacy: Vec<ChatMessage> = Vec::new();

    // 1) SetLeadingSystemPrompt on an empty history → push system v1
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "rules v1".to_string(),
    });
    legacy.push(ChatMessage::system("rules v1"));

    // 2) RecordUserTurn → append user
    let _ = state.reduce(Action::RecordUserTurn("u1".to_string()));
    legacy.push(ChatMessage::user("u1"));

    // 3) SetLeadingSystemPrompt on a non-empty history → replace history[0]
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "rules v2".to_string(),
    });
    if let Some(first) = legacy.first_mut() {
        *first = ChatMessage::system("rules v2");
    }

    // 4) RecordAssistantTurn → append assistant
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "a1".to_string(),
    });
    legacy.push(ChatMessage::assistant("a1"));

    // 5) RecordSystemMessage → append system at the end (the post-/clear case)
    let _ = state.reduce(Action::RecordSystemMessage {
        content: "context note".to_string(),
    });
    legacy.push(ChatMessage::system("context note"));

    // ── byte-level parity ──
    assert_eq!(
        state.session.history.len(),
        legacy.len(),
        "history.len() must match the legacy manual mirror"
    );
    for (i, (lhs, rhs)) in state.session.history.iter().zip(legacy.iter()).enumerate() {
        assert_eq!(lhs.role, rhs.role, "history[{i}] role differs");
        assert_eq!(
            lhs.content.as_bytes(),
            rhs.content.as_bytes(),
            "history[{i}] content differs at byte level"
        );
    }
}

/// S2-B-3: redux_record_turns_single_write_no_duplicate_session_turns
///
/// After dispatching `RecordUserTurn` + `RecordAssistantTurn` once each, `state.session.turns`
/// must grow by exactly +2 and never produce duplicate entries (the earlier 1197+2055 double
/// dispatch has been merged into a single enriched dispatch at one point).
#[test]
fn redux_record_turns_single_write_no_duplicate_session_turns() {
    let mut state = s();
    assert_eq!(state.session.turns.len(), 0);

    let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
    assert_eq!(state.session.turns.len(), 1);
    assert_eq!(
        state.session.history.len(),
        1,
        "history grows in step too (reducer is sole writer)"
    );

    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "hi back".to_string(),
    });
    assert_eq!(
        state.session.turns.len(),
        2,
        "user + assistant, two entries, no duplicates"
    );
    assert_eq!(state.session.history.len(), 2, "history must also hold user+assistant");

    // Key regression guard: dispatch the same content again; turns must grow to 4, not be deduped to 2
    // (the reducer is not idempotent — dedup is the caller's job; this confirms it is append-only)
    let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
    assert_eq!(state.session.turns.len(), 3, "a second dispatch must append one more");
}

/// S2-B-4: redux_compaction_action_preserves_system_and_budget
///
/// HistoryCompacted must keep the system prompt and hold the total char count <= COMPACT_TOTAL_CHARS.
/// This is the core contract of the chat::mod main loop's context-overflow retry path.
#[test]
fn redux_compaction_action_preserves_system_and_budget() {
    let mut state = s();
    // system + 20 long user/assistant messages
    state
        .session
        .history
        .push(ChatMessage::system("system rules — must survive compaction"));
    for i in 0..20 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        state.session.history.push(ChatMessage {
            role: role.to_string(),
            content: format!("turn-{i} {}", "y".repeat(400)),
        });
    }

    let effects = state.reduce(Action::HistoryCompacted {
        reason: CompactReason::ContextOverflow,
    });

    // The system prompt must stay in first position
    assert_eq!(
        state.session.history.first().map(|m| m.role.as_str()),
        Some("system"),
        "system must still be first after compaction"
    );
    assert!(
        state
            .session
            .history
            .first()
            .is_some_and(|m| m.content.contains("must survive")),
        "the system content must be kept in full (never truncated)"
    );
    // the non-system part must stay within COMPACT_TOTAL_CHARS
    let non_system_chars: usize = state
        .session
        .history
        .iter()
        .skip(1)
        .map(|m| m.content.chars().count())
        .sum();
    assert!(
        non_system_chars <= super::COMPACT_TOTAL_CHARS,
        "non-system total chars {non_system_chars} must be <= {}",
        super::COMPACT_TOTAL_CHARS
    );
    // at least one LogTrace must be emitted
    assert!(
        effects.iter().any(|e| matches!(e, Effect::LogTrace { .. })),
        "HistoryCompacted must emit LogTrace"
    );
}

/// S2-B-5: redux_stream_cancelled_cooperates_with_s2a_terminal_actions
///
/// The user presses Ctrl+C while streaming → the reducer emits CancelToken to cancel the backend
/// and clears state. Right after, the chat::run main loop dispatches `StreamCancelled` as the
/// turn terminal — the reducer must be a no-op (draft already cleared), emitting no duplicate hook.
#[test]
fn redux_stream_cancelled_cooperates_with_s2a_terminal_actions() {
    let mut state = s();
    let tok = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-coop".to_string(),
        cancel: tok.clone(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d-coop".to_string(),
        delta: "partial".to_string(),
        version: 1,
    });

    // 1. user presses Ctrl+C → CancelRequested
    let cancel_effects = state.reduce(Action::CancelRequested);
    // really cancel the token (checked through the effect — the reducer already took it out)
    let cancel_token_effect = cancel_effects.iter().find_map(|e| match e {
        Effect::CancelToken(t) => Some(t.clone()),
        _ => None,
    });
    let token_from_effect = cancel_token_effect.expect("the CancelToken effect must exist");
    // EffectExecutor really calls cancel — simulated here
    token_from_effect.cancel();
    assert!(
        tok.is_cancelled(),
        "the original token must be cancelled by the token in the effect (shared cancellation)"
    );
    // control is cleared
    assert!(!state.control.generating);
    assert!(state.stream.primary_streaming_draft().is_none());

    // 2. the chat::run main loop notices the cancellation → dispatches the StreamCancelled terminal
    let terminal_effects = state.reduce(Action::StreamCancelled {
        draft_id: "d-coop".to_string(),
    });
    // the draft was already cleared by CancelRequested, so StreamCancelled must be a no-op (no hook)
    let has_notify = terminal_effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }));
    assert!(
        !has_notify,
        "StreamCancelled (draft already cleared) must not emit NotifyHook again — avoids double firing"
    );
    // and no CancelToken either (the token was already emitted and cancelled)
    let has_cancel_token = terminal_effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
    assert!(!has_cancel_token, "StreamCancelled must not emit CancelToken again");
}

/// S2-B-6 (Codex blocker): cancel_shutdown_race_single_terminal
///
/// When the user triggers `CancelRequested` + `ShutdownRequested` **almost simultaneously**
/// inside a streaming turn (typically holding Ctrl+C then immediately Ctrl+D / SIGTERM):
/// - the first CancelRequested takes active_cancel → emits `Effect::CancelToken`
/// - the second ShutdownRequested sees `generating == false` → must **not** emit another
///   `Effect::CancelToken` (otherwise it would take an Option already taken, or worse emit a
///   None token for EffectExecutor to dereference)
///
/// Two contract points are checked:
/// 1. the whole sequence emits exactly **one** `Effect::CancelToken` (terminal cancel is single)
/// 2. the second ShutdownRequested does not panic / does not cancel twice / still emits `Effect::Quit`
#[test]
fn test_s2b_cancel_shutdown_race_single_terminal() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-race".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d-race".to_string(),
        delta: "streaming...".to_string(),
        version: 1,
    });
    assert!(state.control.generating);
    assert!(state.control.active_cancel.is_some());

    // 1. CancelRequested — takes active_cancel + emits CancelToken
    let cancel_effects = state.reduce(Action::CancelRequested);
    let cancel_token_count = cancel_effects
        .iter()
        .filter(|e| matches!(e, Effect::CancelToken(_)))
        .count();
    assert_eq!(
        cancel_token_count, 1,
        "the CancelRequested stage must emit exactly 1 CancelToken"
    );
    assert!(!state.control.generating, "generating=false after CancelRequested");
    assert!(
        state.control.active_cancel.is_none(),
        "active_cancel has already been taken"
    );

    // 2. ShutdownRequested — generating=false and active_cancel=None by now.
    //    The reducer must **not** emit CancelToken again (avoids a double cancel and taking a None)
    let shutdown_effects = state.reduce(Action::ShutdownRequested);
    let shutdown_cancel_token_count = shutdown_effects
        .iter()
        .filter(|e| matches!(e, Effect::CancelToken(_)))
        .count();
    assert_eq!(
        shutdown_cancel_token_count, 0,
        "ShutdownRequested must not emit CancelToken once active_cancel has been taken"
    );
    // it must not emit CancelDraft either (the draft was already cleared by CancelRequested)
    let shutdown_cancel_draft_count = shutdown_effects
        .iter()
        .filter(|e| matches!(e, Effect::CancelDraft(_)))
        .count();
    assert_eq!(
        shutdown_cancel_draft_count, 0,
        "ShutdownRequested must not emit CancelDraft once the draft is cleared"
    );
    // but it must emit Effect::Quit
    assert!(
        shutdown_effects.iter().any(|e| matches!(e, Effect::Quit)),
        "ShutdownRequested must emit Effect::Quit"
    );

    // the whole sequence emits only 1 CancelToken (the terminal effect is unique)
    let total_cancel_tokens = cancel_effects
        .iter()
        .chain(shutdown_effects.iter())
        .filter(|e| matches!(e, Effect::CancelToken(_)))
        .count();
    assert_eq!(
        total_cancel_tokens, 1,
        "the whole race sequence may emit only 1 Effect::CancelToken (terminal cancel is single)"
    );

    // terminal state: generating=false, active_cancel=None, draft=None — nothing left over
    assert!(!state.control.generating);
    assert!(state.control.active_cancel.is_none());
    assert!(state.stream.primary_streaming_draft().is_none());

    // Reverse race: build another state, shutdown first and cancel after —
    // again only 1 CancelToken may be emitted (the first takes the token, the later Cancel is a
    // no-op with generating=false).
    let mut state2 = s();
    let tok2 = CancellationToken::new();
    let _ = state2.reduce(Action::TurnStarted {
        draft_id: "d-race-2".to_string(),
        cancel: tok2,
    });
    let first_effects = state2.reduce(Action::ShutdownRequested);
    let second_effects = state2.reduce(Action::CancelRequested);
    let cancel_token_total = first_effects
        .iter()
        .chain(second_effects.iter())
        .filter(|e| matches!(e, Effect::CancelToken(_)))
        .count();
    assert_eq!(
        cancel_token_total, 1,
        "the reverse race (Shutdown→Cancel) must also emit only 1 CancelToken"
    );
    // the second CancelRequested must be a no-op (generating=false)
    assert!(
        second_effects.is_empty(),
        "after Shutdown generating=false, so CancelRequested must be a no-op, got: {second_effects:?}"
    );
}

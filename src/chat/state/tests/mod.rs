#![cfg(test)]

use super::*;

fn make_state() -> ChatState {
    let shutdown = CancellationToken::new();
    ChatState::new(Arc::from("test-provider"), Arc::from("test-model"), shutdown)
}

/// Check that the SessionState defaults are sane
#[test]
fn test_chatstate_new_default_session() {
    let state = make_state();
    assert!(uuid::Uuid::parse_str(&state.session.id).is_ok());
    assert!(state.session.title.is_empty(), "expected an empty value");
    assert_eq!(&*state.session.provider, "test-provider");
    assert_eq!(&*state.session.model, "test-model");
    assert!(state.session.turns.is_empty());
    assert!(state.session.history.is_empty());
}

/// Check that the UiState defaults are sane
#[test]
fn test_chatstate_new_default_ui() {
    let state = make_state();
    assert!(state.ui.conversation_lines.is_empty(), "expected an empty value");
    assert!(state.ui.input.is_empty(), "expected an empty value");
    assert_eq!(state.ui.turn_count, 0);
    assert!(!state.ui.ascii_fallback);
    assert_eq!(state.ui.last_ctrlc_ms, 0);
}

/// Check that the StreamState defaults are sane
#[test]
fn test_chatstate_new_default_stream() {
    let state = make_state();
    assert!(state.stream.primary_streaming_draft().is_none());
    assert!(state.control.tool_buffers.is_empty());
}

/// v1b: SessionsStatusUpdated writes ui.sessions_status and the snapshot reflects it; same content is a no-op.
#[cfg(feature = "terminal-tui")]
#[test]
fn sessions_status_updated_writes_and_dedups() {
    let mut state = make_state();
    assert!(state.ui.sessions_status.is_empty(), "expected an empty value");

    let effects = state.reduce(Action::SessionsStatusUpdated {
        summary: "sessions: 1 running".to_string(),
    });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.sessions_status, "sessions: 1 running");
    let snap = state.build_ui_snapshot(1);
    assert_eq!(&*snap.sessions_status, "sessions: 1 running");

    // Identical write is a no-op (no redraw effect).
    let effects = state.reduce(Action::SessionsStatusUpdated {
        summary: "sessions: 1 running".to_string(),
    });
    assert!(effects.is_empty(), "identical status must not emit an effect");

    // Clearing hides the row (empty string flows through).
    let effects = state.reduce(Action::SessionsStatusUpdated { summary: String::new() });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(state.ui.sessions_status.is_empty(), "expected an empty value");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn main_queue_status_updated_writes_snapshot_and_dedups() {
    let mut state = make_state();
    let status = MainQueueStatus { queued: 3, priority: 1 };

    let effects = state.reduce(Action::MainQueueStatusUpdated { status });

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.main_queue_status, status);
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.main_queue_status, status);

    let effects = state.reduce(Action::MainQueueStatusUpdated { status });
    assert!(effects.is_empty(), "identical queue status must not emit an effect");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn provider_worker_status_updated_writes_snapshot_and_dedups() {
    let mut state = make_state();
    let status = ProviderWorkerStatus {
        running: 1,
        cancelling: 1,
        awaiting_commit: 2,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: None,
        rows: Vec::new(),
    };

    let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status: status.clone() });

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.provider_worker_status, status);
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.provider_worker_status, status);

    let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });
    assert!(
        effects.is_empty(),
        "identical provider worker status must not emit an effect"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn provider_worker_status_update_refreshes_open_worker_view_with_io() {
    use crate::chat::tui::{ConversationLine, ToolStatus};

    let mut state = make_state();
    state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 2 };
    state.ui.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "echo P6Z".to_string(),
        args_full: "{\"command\":\"echo P6Z\"}".to_string(),
        result: Some("P6Z\n".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(12),
        folded: true,
    });
    let status = ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis()),
        rows: vec![crate::chat::action::ProviderWorkerStatusRow {
            task_id: 7,
            sequence: 2,
            kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
            state: crate::chat::action::ProviderWorkerRowState::Running,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    };

    let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    let view = state.ui.active_session_view.as_ref().expect("worker view refreshed");
    assert_eq!(view.kind, crate::chat::action::PROVIDER_WORKER_VIEW_KIND);
    assert!(view.lines.iter().any(|line| line == "task: 7"));
    assert!(
        !view.lines.iter().any(|line| line == "io: recent provider turn"),
        "non-streaming worker views must not replay transcript history: {:?}",
        view.lines
    );
    assert!(
        !view.lines.iter().any(|line| line.starts_with("run shell done:")),
        "completed tool cards without a matching streaming draft stay out of worker IO: {:?}",
        view.lines
    );
    assert!(
        !view.lines.iter().any(|line| line == "output: P6Z"),
        "completed tool output without a matching streaming draft stays out of worker IO: {:?}",
        view.lines
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn provider_worker_status_update_enriches_running_rows_with_recent_tool_summary() {
    use crate::chat::tui::{ConversationLine, ToolStatus};

    let mut state = make_state();
    state.ui.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "ls /tmp/demo".to_string(),
        args_full: "{\"command\":\"ls /tmp/demo\"}".to_string(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: true,
    });
    let status = ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(0),
        rows: vec![crate::chat::action::ProviderWorkerStatusRow {
            task_id: 7,
            sequence: 2,
            kind: crate::chat::action::ProviderWorkerRowKind::Detached,
            state: crate::chat::action::ProviderWorkerRowState::Running,
            started_at_ms: 0,
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    };

    let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    let summary = state
        .ui
        .provider_worker_status
        .rows
        .first()
        .and_then(|row| row.recent_tool_call.as_deref())
        .expect("running row should receive latest tool summary");
    assert!(
        summary.contains("run shell running: ls /tmp/demo"),
        "summary should use the compact tool-call wording: {summary}"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn slash_menu_sources_match_legacy_and_redux_for_same_keys() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let catalog = vec![crate::chat::tui::SlashProviderModelCatalog {
        provider: "test-provider".to_string(),
        models: vec![crate::chat::tui::SlashModelCandidate {
            name: "gpt-parity".to_string(),
            description: "Parity model".to_string(),
        }],
    }];
    let mut legacy = crate::chat::tui::TuiState::new("test-provider", "test-model");
    legacy.provider_model_catalog = catalog.clone();
    let mut redux = make_state();
    let _ = redux.reduce(Action::SlashMenuSourcesUpdated {
        saved_sessions: Vec::new(),
        provider_model_catalog: catalog,
    });

    for ch in "/model ".chars() {
        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        let _ = crate::chat::tui::dispatch_global_key(key, &mut legacy);
        let _ = redux.reduce_with_now(Action::KeyPressed(key), 1_000);
    }

    let legacy_labels = legacy
        .slash_menu
        .as_ref()
        .expect("legacy model menu")
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect::<Vec<_>>();
    let redux_labels = redux
        .ui
        .slash_menu
        .as_ref()
        .expect("redux model menu")
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(legacy_labels, redux_labels);
    assert_eq!(redux_labels, vec!["gpt-parity"]);
}

#[cfg(feature = "terminal-tui")]
#[test]
fn at_path_candidates_action_opens_redux_menu_and_tab_inserts() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    let _ = state.reduce(Action::InputReplaced("inspect @ca".to_string()));
    assert!(
        state.ui.slash_menu.is_none(),
        "input alone has no candidates and must not render a stale menu"
    );

    let effects = state.reduce(Action::AtPathCandidatesUpdated {
        candidates: vec![AtPathCandidate {
            path: "Cargo.toml".to_string(),
            is_dir: false,
        }],
    });

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(
        state
            .ui
            .slash_menu
            .as_ref()
            .expect("@path menu")
            .entries
            .first()
            .map(|entry| entry.label.as_str()),
        Some("Cargo.toml")
    );
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(state.ui.input.text(), "inspect @Cargo.toml ");
}

/// P1: SessionsEntriesUpdated writes structured strip entries and dedups
/// identical snapshots so the 1s poll does not churn redraws.
#[cfg(feature = "terminal-tui")]
#[test]
fn sessions_entries_updated_writes_snapshot_and_dedups() {
    use crate::chat::sessions::SwitcherEntry;
    let mut state = make_state();
    assert!(state.ui.sessions_entries.is_empty(), "expected an empty value");

    let entries = vec![SwitcherEntry {
        seq: 1,
        kind: "agent",
        origin: "user",
        status: "running",
        title: "task".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    let expected = entries.clone();
    let effects = state.reduce(Action::SessionsEntriesUpdated { entries });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.sessions_entries, expected);
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.sessions_entries.as_slice(), expected.as_slice());

    let effects = state.reduce(Action::SessionsEntriesUpdated { entries: expected });
    assert!(effects.is_empty(), "identical entries must not redraw");

    let effects = state.reduce(Action::SessionsEntriesUpdated { entries: Vec::new() });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(state.ui.sessions_entries.is_empty(), "expected an empty value");
}

/// P2: ActiveSessionViewUpdated writes the focused child viewport snapshot,
/// flows it to UiSnapshot, and dedups identical writes.
#[cfg(feature = "terminal-tui")]
#[test]
fn active_session_view_updated_writes_snapshot_and_dedups() {
    let mut state = make_state();
    assert!(state.ui.active_session_view.is_none());

    let view = crate::chat::sessions::ActiveSessionView {
        seq: 4,
        kind: "shell".to_string(),
        title: "tail -f app.log".to_string(),
        lines: vec!["a".to_string(), "b".to_string()],
        truncated: false,
        scroll_offset: 1,
    };
    let effects = state.reduce(Action::ActiveSessionViewUpdated {
        view: Some(view.clone()),
    });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.active_session_view, Some(view.clone()));
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.active_session_view, Some(view));

    let duplicate = state.ui.active_session_view.clone();
    let effects = state.reduce(Action::ActiveSessionViewUpdated { view: duplicate });
    assert!(effects.is_empty(), "identical active view must not redraw");

    let effects = state.reduce(Action::ActiveSessionViewUpdated { view: None });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(state.ui.active_session_view.is_none());
}

/// P4c: ContextWindowUpdated writes status-bar window metadata to both the
/// live UI state and UiSnapshot, with identical writes deduped.
#[cfg(feature = "terminal-tui")]
#[test]
fn context_window_updated_writes_snapshot_and_dedups() {
    let mut state = make_state();
    assert_eq!(state.ui.context_used_tokens, None);
    assert_eq!(state.ui.context_window_tokens, None);

    let effects = state.reduce(Action::ContextWindowUpdated {
        used_context_tokens: Some(2_500),
        max_context_tokens: Some(10_000_000),
    });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.context_used_tokens, Some(2_500));
    assert_eq!(state.ui.context_window_tokens, Some(10_000_000));
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.context_used_tokens, Some(2_500));
    assert_eq!(snap.context_window_tokens, Some(10_000_000));

    let effects = state.reduce(Action::ContextWindowUpdated {
        used_context_tokens: Some(2_500),
        max_context_tokens: Some(10_000_000),
    });
    assert!(effects.is_empty(), "identical context budget must not redraw");

    let effects = state.reduce(Action::ContextWindowUpdated {
        used_context_tokens: None,
        max_context_tokens: None,
    });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.context_used_tokens, None);
    assert_eq!(state.ui.context_window_tokens, None);
}

/// v1.1b: SessionFocusChanged writes ui.focus + flows to the snapshot; an
/// identical focus is a no-op.
#[cfg(feature = "terminal-tui")]
#[test]
fn session_focus_changed_writes_and_dedups() {
    use crate::chat::sessions::FocusTarget;
    let mut state = make_state();
    assert_eq!(state.ui.focus, FocusTarget::Main);

    let focus = FocusTarget::Session { seq: 2 };
    let effects = state.reduce(Action::SessionFocusChanged { focus });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.focus, focus);
    let snap = state.build_ui_snapshot(1);
    assert_eq!(snap.focus, focus);

    // Identical focus → no-op.
    let effects = state.reduce(Action::SessionFocusChanged { focus });
    assert!(effects.is_empty(), "identical focus must not emit an effect");

    // Back to main.
    let effects = state.reduce(Action::SessionFocusChanged {
        focus: FocusTarget::Main,
    });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.focus, FocusTarget::Main);
}

/// v1.1b: switcher open/move/close lifecycle through the reducer.
#[cfg(feature = "terminal-tui")]
#[test]
fn switcher_open_move_close_lifecycle() {
    use crate::chat::sessions::SwitcherEntry;
    let mut state = make_state();
    assert!(state.ui.switcher.is_none());

    let entries = vec![
        SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "a".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        },
        SwitcherEntry {
            seq: 2,
            kind: "agent",
            origin: "model",
            status: "completed",
            title: "b".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        },
    ];
    let effects = state.reduce(Action::SwitcherOpened { entries });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    let sw = state.ui.switcher.as_ref().expect("test: switcher open");
    assert_eq!(sw.len(), 2);
    assert_eq!(sw.selected, 0);
    // Snapshot carries it.
    assert!(state.build_ui_snapshot(1).switcher.is_some());

    // Move to row 1.
    let effects = state.reduce(Action::SwitcherMoved { selected: 1 });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);
    // Out-of-range selection is clamped to the last row, not a panic.
    let _ = state.reduce(Action::SwitcherMoved { selected: 99 });
    assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);

    // Close.
    let effects = state.reduce(Action::SwitcherClosed);
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(state.ui.switcher.is_none());
    // Closing again is a no-op.
    let effects = state.reduce(Action::SwitcherClosed);
    assert!(effects.is_empty());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn alt_enter_no_longer_surfaces_session_gone() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
        seq: 1,
        kind: "agent",
        origin: "user",
        status: "running",
        title: "task".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    state.ui.input.set_text("draft");

    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(
        state.ui.input.text(),
        "draft\n",
        "Alt+Enter is only an input newline chord"
    );
    assert!(state.ui.conversation_lines.is_empty());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn alt_enter_with_sessions_no_longer_uses_attach_branch() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
        seq: 2,
        kind: "shell",
        origin: "user",
        status: "running",
        title: "task".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    state.ui.input.set_text("draft");

    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(
        state.ui.input.text(),
        "draft\n",
        "matching Alt+Enter no longer attaches strip selection"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn alt_enter_falls_through_to_newline_insert() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    state.ui.input.set_text("a");

    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::NONE,
    )));

    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.input.text(), "a\nb");
    assert!(
        state.ui.conversation_lines.is_empty(),
        "no strip selection means Alt+Enter falls through to input, not session gone"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn slash_menu_captures_alt_arrows_before_bottom_navigation() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    state.ui.sessions_entries = vec![
        crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "one".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        },
        crate::chat::sessions::SwitcherEntry {
            seq: 2,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "two".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        },
    ];
    state.ui.input.set_text("/mo");
    state.ui.slash_menu = Some(SlashMenuState::new("mo"));

    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));

    assert!(
        effects.iter().all(|effect| matches!(effect, Effect::RequestRedraw)),
        "slash menu may redraw, but must not leak Alt+Up into strip navigation: {effects:?}"
    );
    assert!(state.ui.slash_menu.is_some(), "slash menu remains open");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn saved_session_picker_captures_alt_enter_before_input_newline() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = make_state();
    state.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
        crate::chat::session::SavedSessionPickerEntry {
            id: "saved-a".to_string(),
            title: "saved a".to_string(),
            turn_count: 1,
            updated_at: chrono::Utc::now(),
            provider: "p".to_string(),
            model: "m".to_string(),
            is_current: false,
        },
    ]));
    state.ui.input.set_text("draft");

    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

    assert!(
        effects.is_empty(),
        "saved picker consumes Alt+Enter without reducer side effects"
    );
    assert!(
        state.ui.saved_session_picker.is_some(),
        "Alt+Enter is consumed, not treated as picker Enter"
    );
    assert_eq!(state.ui.input.text(), "draft");
    assert!(
        !matches!(
            state.ui.conversation_lines.last(),
            Some(crate::chat::tui::ConversationLine::System { content }) if content == "session gone"
        ),
        "stale strip selection must not receive Alt+Enter while picker is open"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn saved_session_picker_open_move_close_lifecycle() {
    let mut state = make_state();
    state.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![
        crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "model",
            status: "running",
            title: "child".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        },
    ]));
    let entries = vec![
        crate::chat::session::SavedSessionPickerEntry {
            id: "saved-a".to_string(),
            title: "saved a".to_string(),
            turn_count: 2,
            updated_at: chrono::Utc::now(),
            provider: "p".to_string(),
            model: "m".to_string(),
            is_current: true,
        },
        crate::chat::session::SavedSessionPickerEntry {
            id: "saved-b".to_string(),
            title: "saved b".to_string(),
            turn_count: 4,
            updated_at: chrono::Utc::now(),
            provider: "p".to_string(),
            model: "m".to_string(),
            is_current: false,
        },
    ];

    let effects = state.reduce(Action::SavedSessionPickerOpened { entries });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(
        state.ui.switcher.is_none(),
        "saved picker and child switcher are mutually exclusive"
    );
    let picker = state.ui.saved_session_picker.as_ref().expect("picker open");
    assert_eq!(picker.len(), 2);
    assert_eq!(picker.selected, 0);
    assert_eq!(
        state
            .build_ui_snapshot(1)
            .saved_session_picker
            .as_ref()
            .expect("snapshot picker")
            .entries
            .len(),
        2
    );

    let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 99 });
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert_eq!(state.ui.saved_session_picker.as_ref().expect("picker").selected, 1);
    let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 1 });
    assert!(effects.is_empty(), "same clamped selection is no-op");

    let effects = state.reduce(Action::SavedSessionPickerClosed);
    assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
    assert!(state.ui.saved_session_picker.is_none());
    assert!(state.reduce(Action::SavedSessionPickerClosed).is_empty());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn tool_approval_requested_opens_approval_child_view() {
    let mut state = make_state();
    let effects = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-approve".to_string(),
        name: "shell".to_string(),
        args: r#"{"cmd":"printf secure"}"#.to_string(),
    });
    assert!(state.ui.pending_tool_approval.is_some());
    assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Approval);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::RequestApproval { .. }))
    );

    let snap = state.build_ui_snapshot(1);
    let pending = snap
        .pending_tool_approval
        .as_ref()
        .expect("pending approval in snapshot");
    assert_eq!(pending.tool_id, "call-approve");
    assert_eq!(pending.name, "shell");
    assert!(pending.args.contains("printf secure"));
    assert!(
        !pending.selected_approval,
        "approval selection defaults to the safe deny choice"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn redux_approval_arrows_select_and_enter_resolves() {
    let mut state = make_state();
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-arrow-approve".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });

    let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::NONE,
    )));
    assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    assert!(
        state
            .ui
            .pending_tool_approval
            .as_ref()
            .expect("approval remains pending after arrow")
            .selected_approval,
        "Right selects approve"
    );

    let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    )));
    assert!(state.ui.pending_tool_approval.is_none());
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::ResolveApproval { tool_id, approved: true } if tool_id == "call-arrow-approve"
        )
    }));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn tool_approval_received_closes_approval_child_view() {
    let mut state = make_state();
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-deny".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });
    let effects = state.reduce(Action::ToolApprovalReceived {
        tool_id: "call-deny".to_string(),
        approved: false,
    });
    assert!(state.ui.pending_tool_approval.is_none());
    assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
    assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn redux_esc_approval_generating_denies_without_cancelling_turn() {
    let mut state = make_state();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-approval".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-esc-deny".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });

    let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    )));

    assert!(
        state.control.generating,
        "Esc in approval must not cancel the active turn"
    );
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "streaming draft stays active"
    );
    assert!(state.ui.pending_tool_approval.is_none());
    assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-esc-deny"
        )
    }));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
        "approval Esc must not emit turn-cancel effects"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn redux_esc_generating_slash_menu_closes_menu_without_cancelling_turn() {
    let mut state = make_state();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-slash".to_string(),
        cancel: CancellationToken::new(),
    });
    state.ui.input.set_text("/mo");
    state.ui.slash_menu = Some(SlashMenuState::new("mo"));

    let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    )));

    assert!(state.control.generating, "slash Esc must not cancel the active turn");
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "streaming draft stays active"
    );
    assert!(state.ui.slash_menu.is_none(), "Esc closes only the slash menu");
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
        "slash Esc must not emit turn-cancel effects"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn cancel_requested_clears_pending_approval_and_resolves_false() {
    let mut state = make_state();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-cancel".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-cancel-deny".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });

    let effects = state.reduce(Action::CancelRequested);

    assert!(state.ui.pending_tool_approval.is_none());
    assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
    assert!(effects.iter().any(|effect| matches!(effect, Effect::CancelToken(_))));
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-cancel-deny"
        )
    }));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::CancelDraft(draft_id) if draft_id == "draft-cancel"))
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn tool_approval_focus_paste_does_not_edit_input() {
    let mut state = make_state();
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-paste".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });
    let effects = state.reduce(Action::PasteReceived("must not enter input".to_string()));
    assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    assert!(state.ui.input.is_empty());
    assert!(state.ui.pending_tool_approval.is_some());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn session_loaded_resets_transient_holder_set() {
    let mut state = make_state();
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: None,
        tool_id: "call-stale".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-stale".to_string(),
        cancel: CancellationToken::new(),
    });
    state.ui.context_window_tokens = Some(10_000_000);
    state.ui.context_used_tokens = Some(2_500);
    state.ui.input.set_text("draft text");
    assert!(state.ui.input.begin_or_cycle_reverse_search());
    state.control.generating = false;

    let mut loaded = ChatSession::new("prov-new", "model-new");
    loaded.id = "sess-new".to_string();
    loaded.add_user_turn("hello");
    loaded.add_assistant_turn("hi", vec![]);
    let effects = state.reduce(Action::SessionLoaded(loaded));

    assert!(state.ui.pending_tool_approval.is_none());
    assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
    assert_eq!(state.ui.context_window_tokens, None);
    assert_eq!(state.ui.context_used_tokens, None);
    assert!(!state.ui.input.is_reverse_search_active());
    assert_eq!(state.ui.turn_count, 2);
    assert!(state.stream.primary_streaming_draft().is_none());
    assert!(!state.control.generating);
    assert!(state.control.active_cancel.is_none());
    assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn loading_fresh_session_clears_visible_title_turns_and_token_usage() {
    let mut state = make_state();
    state.session.id = "sess-old".to_string();
    state.session.title = "stale title".to_string();
    state.session.turns.push(ChatTurn {
        role: "user".to_string(),
        content: "old turn".to_string(),
        timestamp: chrono::Utc::now(),
        tool_calls: Vec::new(),
    });
    state.ui.turn_count = 1;
    state.ui.token_usage_summary = MainSessionTokenUsageSummary {
        total_tokens: 68_700,
        prompt_tokens: 68_000,
        completion_tokens: 700,
        request_count: 1,
        unknown_cost_requests: 1,
        ..MainSessionTokenUsageSummary::default()
    };

    let mut fresh = ChatSession::new("prov-new", "model-new");
    fresh.id = "sess-fresh".to_string();
    let _ = state.reduce(Action::SessionLoaded(fresh));

    assert_eq!(state.session.id, "sess-fresh");
    assert!(state.session.title.is_empty(), "expected an empty value");
    assert!(state.session.turns.is_empty());
    assert!(state.session.token_usage_records.is_empty(), "expected an empty value");
    assert_eq!(state.ui.turn_count, 0);
    assert_eq!(state.ui.token_usage_summary, MainSessionTokenUsageSummary::default());
    assert!(state.ui.conversation_lines.is_empty());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn session_loaded_is_rejected_while_generating_without_clearing_active_turn() {
    let mut state = make_state();
    state.session.id = "sess-old".to_string();
    let cancel = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-active".to_string(),
        cancel: cancel.clone(),
    });
    assert!(state.control.generating);
    assert!(state.stream.primary_streaming_draft().is_some());
    assert!(state.control.active_cancel.is_some());

    let mut loaded = ChatSession::new("prov-new", "model-new");
    loaded.id = "sess-new".to_string();
    loaded.add_user_turn("should-not-load");
    let effects = state.reduce(Action::SessionLoaded(loaded));

    assert_eq!(state.session.id, "sess-old");
    assert!(state.control.generating);
    assert!(state.stream.primary_streaming_draft().is_some());
    assert!(state.control.active_cancel.is_some());
    assert!(!cancel.is_cancelled());
    assert!(!effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg,
            } if msg.contains("SessionLoaded rejected while generating: sess-new")
        )
    }));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn reducer_tab_mid_edit_inserts_tab_instead_of_folding() {
    let mut state = make_state();
    state.ui.input.set_text("alpha");

    let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    )));

    assert_eq!(state.ui.input.text(), "alpha\t");
    assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
}

/// reduce does not panic (robustness baseline; keeps the Step 1 name for easy grepping)
#[test]
fn test_reduce_key_pressed_returns_empty_step1() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = make_state();
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    let _effects = state.reduce(Action::KeyPressed(key));
    // Since Step 2, KeyPressed('a') writes into the input buffer -> RequestRedraw
    // here we only assert that it does not panic
}

/// Actions that still return empty after Step 4 (only SlashCommandIssued remains)
#[test]
fn test_reduce_unfilled_actions_return_empty() {
    let mut state = make_state();
    // Filled in by Step 4: HistoryCleared/SessionLoaded/SessionSaved/SessionSwitched/
    //   RecordUserTurn/RecordAssistantTurn/CancelRequested/ShutdownRequested
    // The following are still left for Step 5 (they return an empty vec):
    let unfilled = [Action::SlashCommandIssued {
        cmd: "clear".to_string(),
        args: String::new(),
    }];
    for action in unfilled {
        let effects = state.reduce(action);
        assert!(effects.is_empty(), "unfilled Action must return vec![]");
    }
}

/// Added in Step 3: StreamChunkReceived returns vec![] when there is no draft (stale)
#[test]
fn test_reduce_stream_chunk_no_draft_returns_empty() {
    let mut state = make_state();
    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "x".to_string(),
        version: 1,
    });
    assert!(effects.is_empty(), "chunk must be dropped when there is no draft");
}

/// reduce must not panic for any Action variant (coverage contract)
#[test]
fn test_reduce_does_not_panic_for_all_actions() {
    use crate::chat::action::HistoryDir;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("cancel target", crate::chat::turn_scheduler::TurnPriority::Normal, 0);

    let actions: Vec<Action> = vec![
        Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::PasteReceived("paste text".to_string()),
        Action::TerminalResized { w: 80, h: 24 },
        Action::InputSubmitted("hello".to_string()),
        Action::HistoryNavigated(HistoryDir::Up),
        Action::HistoryNavigated(HistoryDir::Down),
        Action::InputCancelled,
        Action::ToolCardFoldToggled,
        Action::ReasoningFoldToggled,
        Action::RedrawRequested,
        Action::CancelProviderTurn { task_id },
        Action::ForceQuit,
    ];

    for action in actions {
        let mut state = make_state();
        let _effects = state.reduce(action);
    }
}

/// Action must be Send + Sync (compile-time assertion so it can cross tasks over a channel)
#[test]
fn test_action_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Action>();
}

/// Effect must be Send + Sync (compile-time assertion so it can be handed to the executor)
#[test]
fn test_effect_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Effect>();
}

/// EmitChannelMessage carries a SendMessage (P1 check: not a bare String)
#[test]
fn test_effect_emit_channel_message_has_send_message_type() {
    use crate::channels::traits::SendMessage;
    // Being able to construct Effect::EmitChannelMessage(SendMessage) proves the type is wired in
    let msg = SendMessage::new("hello", "bob");
    let effect = Effect::EmitChannelMessage(msg);
    // Check that the Debug implementation exists
    let debug_str = format!("{:?}", effect);
    assert!(
        debug_str.contains("EmitChannelMessage"),
        "EmitChannelMessage Debug output is wrong"
    );
}

/// ControlState default value check
#[test]
fn test_chatstate_new_default_control() {
    let state = make_state();
    assert!(state.control.active_cancel.is_none());
    assert!(!state.control.generating);
}

/// Repeated reduce calls must not panic (robustness)
#[test]
fn test_reduce_multiple_calls_no_panic() {
    let mut state = make_state();
    for _ in 0..10 {
        let _effects = state.reduce(Action::RedrawRequested);
    }
}

// ─── Step 2 unit tests (input path) ───────────────────────────────────────
//
// Most input-path tests rely on the real TuiInput / ConversationLine provided by the
// terminal-tui feature. Without the TUI feature the reducer takes the placeholder branch and
// degrades to "return RequestRedraw without mutating the buffer", so Step 2 assertions are cfg-gated.

#[cfg(feature = "terminal-tui")]
mod step2;

// ─── Step 5a-3 Phase A + F tests ──────────────────────────────────────────
//
// Phase A: StartLLMTurn drives the real path — the reducer initialises the draft and also emits
// Effect::StartTurn. Phase F: StreamFailed emits NotifyHook(Error); StreamCancelled emits neither
// a hook nor SaveSession.

#[cfg(test)]
mod phase_a_f;

// ─── S2-B integration tests (5 new tests) ─────────────────────────────────
//
// These five tests cover the contract after S2-B wired the chat session/cancel paths into Redux:
//   1. CancelRequested really emits a CancelToken effect and clears the control state
//   2. ModeChanged leaves state.session.mode equal to legacy chat_session.set_mode
//   3. a single RecordUserTurn write produces no duplicate session.turns
//   4. HistoryCompacted keeps system and caps the total budget
//   5. StreamCancelled matches the S2-A terminal behaviour (cancel and token cancel do not clash)

#[cfg(test)]
mod s2b;

// ─── S2-C integration tests (3 new tests) ─────────────────────────────────
//
// These three tests cover the contract after S2-C wired the chat module's mirror / history paths
// into Redux dispatch:
//   1. on the /clear path the reducer keeps system and the UI mirror has a system message line
//   2. after a user/assistant double dispatch, session.turns + session.history stay ordered
//   3. SystemMessageAdded and RecordSystemMessage do not interfere (UI and history are two axes)
//
// Key design decisions (from the Codex P0 audit):
//   - no legacy_mirror_enabled / legacy_history_enabled guards are introduced — the legacy
//     history is still the real LLM context source, the reducer is an observing ledger.
//   - SetLeadingSystemPrompt differs from RecordSystemMessage: the former upserts the front
//     entry (it runs every turn, covering skill-list changes), the latter appends after /clear.
#[cfg(test)]
mod s2c;

// ─── S2.5 P1-B: tool_calls parity via backfill inside the reducer ─────────
//
// Option C buffers them in ControlState.current_turn_tool_calls inside the reducer:
//   ToolStarted/Finished accumulate, RecordAssistantTurn uses mem::take to backfill
//   session.turns.last_mut().tool_calls, and the stream terminal + InputSubmitted clear it.
// This closes the original FIXME(S2.5) at state.rs:1171 with no Action signature or callsite change.
#[cfg(test)]
mod p1_b_tool_calls_parity;

#[cfg(test)]
mod p3a_task_aware_tool_buffers;

#[cfg(test)]
mod p3b_task_aware_cancel_tokens;

#[cfg(test)]
mod p3c_task_aware_usage;

// ─── S4-A Commit 1: UiSnapshot + reduce_tracked unit tests ────────────────

#[cfg(feature = "terminal-tui")]
mod s4_a_1;

// ─── S4-A Commit 6: dual-path parity (mirror vs reducer) ───────────────

#[cfg(feature = "terminal-tui")]
mod s4_a_6;

/// S5 invariant test suite: the reducer's core invariants (ordering / idempotency / cancel)
#[cfg(feature = "terminal-tui")]
mod s5_invariants;

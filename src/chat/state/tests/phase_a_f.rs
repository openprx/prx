#![cfg(test)]

use super::super::*;
use crate::chat::action::Action;
use crate::providers::ChatMessage;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

fn has_start_turn(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::StartTurn { .. }))
}
fn has_notify_hook(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }))
}
fn has_save_session(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::SaveSession(_)))
}
fn has_request_redraw(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
}

/// Phase A-1: StartLLMTurn initialises the draft + emits Effect::StartTurn (carrying history)
#[test]
fn test_phase_a_start_llm_turn_emits_effect_start_turn() {
    let mut state = s();
    let cancel = CancellationToken::new();
    let history = vec![ChatMessage::system("you are helpful"), ChatMessage::user("hi")];

    let effects = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: None,
        draft_id: "draft-1".to_string(),
        history,
        compaction_guard_history: None,
        compaction_config: None,
        cancel,
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });

    // state changes: draft + active_cancel + generating
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "stream.draft must be set"
    );
    assert!(
        state.control.active_cancel.is_some(),
        "active_cancel must be registered"
    );
    assert!(state.control.generating, "generating must be set to true");

    // Effect checks
    assert!(has_start_turn(&effects), "Effect::StartTurn must be emitted");
    assert!(has_request_redraw(&effects), "Effect::RequestRedraw must be emitted");

    // history must be carried through into Effect::StartTurn
    let history_in_effect = effects.iter().find_map(|e| match e {
        Effect::StartTurn { history, draft_id, .. } => Some((draft_id.clone(), history.clone())),
        _ => None,
    });
    let (draft_id, hist) = history_in_effect.expect("the StartTurn effect must exist");
    assert_eq!(draft_id, "draft-1");
    assert_eq!(hist.len(), 2);
    let h0 = hist.first().expect("history[0] must exist");
    let h1 = hist.get(1).expect("history[1] must exist");
    assert_eq!(h0.role, "system");
    assert_eq!(h1.role, "user");
}

/// Phase A-2: the cancel registered by StartLLMTurn is the same token as the one in Effect::StartTurn
#[test]
fn test_phase_a_start_llm_turn_cancel_propagates() {
    let mut state = s();
    let cancel = CancellationToken::new();
    let effects = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: None,
        draft_id: "d2".to_string(),
        history: vec![ChatMessage::user("x")],
        compaction_guard_history: None,
        compaction_config: None,
        cancel: cancel.clone(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    // cancel the original token and check the token inside the Effect is cancelled too (shared)
    cancel.cancel();
    let cancel_in_effect = effects.iter().find_map(|e| match e {
        Effect::StartTurn { cancel, .. } => Some(cancel.clone()),
        _ => None,
    });
    let tok = cancel_in_effect.expect("StartTurn must carry a cancel token");
    assert!(
        tok.is_cancelled(),
        "the cancel in StartTurn must be shared with the original token"
    );
}

#[test]
fn start_llm_turn_carries_compaction_config_to_effect() {
    let mut state = s();
    let compaction_config = crate::config::AgentCompactionConfig {
        max_context_tokens: 120,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        memory_flush: false,
        ..crate::config::AgentCompactionConfig::default()
    };

    let effects = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: None,
        draft_id: "d-budget".to_string(),
        history: vec![ChatMessage::user("x")],
        compaction_guard_history: None,
        compaction_config: Some(compaction_config),
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });

    let carried = effects.iter().find_map(|effect| match effect {
        Effect::StartTurn {
            provider_turn_task_id: None,
            compaction_config: Some(config),
            ..
        } => Some(config),
        _ => None,
    });
    let config = carried.expect("StartTurn effect must carry compaction config");
    assert_eq!(config.max_context_tokens, 120);
    assert_eq!(config.reserve_tokens, 10);
}

#[test]
fn start_llm_turn_carries_provider_turn_task_id_to_effect() {
    let mut state = s();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "provider identity",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        0,
    );

    let effects = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: Some(task_id),
        provider_turn_sequence: None,
        draft_id: "d-worker".to_string(),
        history: vec![ChatMessage::user("x")],
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });

    let carried = effects.iter().find_map(|effect| match effect {
        Effect::StartTurn {
            provider_turn_task_id, ..
        } => Some(*provider_turn_task_id),
        _ => None,
    });
    assert_eq!(carried, Some(Some(task_id)));
}

/// Phase A-3: TurnStarted (the old Action) keeps its behaviour — it emits no Effect::StartTurn
#[test]
fn test_phase_a_legacy_turn_started_no_start_turn_effect() {
    let mut state = s();
    let effects = state.reduce(Action::TurnStarted {
        draft_id: "legacy".to_string(),
        cancel: CancellationToken::new(),
    });
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "TurnStarted also initialises the draft"
    );
    assert!(
        !has_start_turn(&effects),
        "TurnStarted must not emit Effect::StartTurn (the old path is still driven by chat::run)"
    );
}

/// Phase F-1: StreamFailed emits NotifyHook(Error) — matching the old hooks.emit(HookEvent::Error)
#[test]
fn test_phase_f_stream_failed_emits_notify_hook() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d3".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamFailed {
        draft_id: "d3".to_string(),
        err: "boom".to_string(),
        retryable: false,
    });
    assert!(has_notify_hook(&effects), "StreamFailed must emit NotifyHook(Error)");
    let hook_evt = effects.iter().find_map(|e| match e {
        Effect::NotifyHook { event, payload } => Some((*event, payload.clone())),
        _ => None,
    });
    let (evt, payload) = hook_evt.expect("NotifyHook must exist");
    assert!(matches!(evt, HookEvent::Error));
    assert_eq!(payload.get("component").and_then(|v| v.as_str()), Some("chat-turn"));
    assert_eq!(payload.get("message").and_then(|v| v.as_str()), Some("boom"));
    assert_eq!(
        payload.get("retryable").and_then(serde_json::Value::as_bool),
        Some(false)
    );
}

/// Phase F-2: StreamCancelled emits neither NotifyHook nor SaveSession (an interrupted turn
/// is not persisted and fires no hook)
#[test]
fn test_phase_f_stream_cancelled_no_save_no_hook() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d4".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "d4".to_string(),
    });
    assert!(!has_notify_hook(&effects), "StreamCancelled must not emit NotifyHook");
    assert!(!has_save_session(&effects), "StreamCancelled must not SaveSession");
    assert!(
        has_request_redraw(&effects),
        "StreamCancelled still needs RequestRedraw"
    );
}

/// Phase F-3: StreamFailed with a mismatched draft_id → no-op, no NotifyHook (avoids stale reports)
#[test]
fn test_phase_f_stream_failed_wrong_id_no_hook() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "right".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamFailed {
        draft_id: "wrong".to_string(),
        err: "stale".to_string(),
        retryable: true,
    });
    assert!(effects.is_empty(), "a stale draft_id must be a no-op");
    assert!(!has_notify_hook(&effects));
}

/// Phase A-4: cancelling right after StartLLMTurn — the state is cleaned up correctly
/// (generating=true, and the cancel token is ready for the executor)
#[test]
fn test_phase_a_start_llm_turn_then_cancel_request() {
    let mut state = s();
    let cancel = CancellationToken::new();
    let _ = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: None,
        draft_id: "d5".to_string(),
        history: vec![ChatMessage::user("hi")],
        compaction_guard_history: None,
        compaction_config: None,
        cancel,
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    assert!(state.control.generating);

    let effects = state.reduce(Action::CancelRequested);
    // generating=true → the reducer emits CancelDraft
    assert!(
        effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
        "CancelRequested(generating=true) must emit CancelDraft"
    );
    assert!(!state.control.generating, "generating must be reset after cancel");
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleaned up after cancel"
    );
}

fn start_phase1_draft(state: &mut ChatState, draft_id: &str, sequence: u64, prompt: &str) {
    let effects = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: Some(sequence),
        draft_id: draft_id.to_string(),
        history: vec![ChatMessage::user(prompt)],
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    assert!(has_start_turn(&effects), "phase1 draft start must still emit StartTurn");
}

fn visible_draft_ids(state: &ChatState) -> Vec<&str> {
    state
        .stream
        .visible_drafts
        .iter()
        .map(|turn| turn.draft.draft_id.as_str())
        .collect()
}

fn draft_text(state: &ChatState, draft_id: &str) -> Option<String> {
    state
        .stream
        .visible_drafts
        .iter()
        .find(|turn| turn.draft.draft_id == draft_id)
        .map(|turn| turn.draft.accumulated.clone())
}

#[cfg(feature = "terminal-tui")]
fn provider_worker_status(sequences: &[u64]) -> ProviderWorkerStatus {
    ProviderWorkerStatus {
        running: sequences.len(),
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(0),
        rows: sequences
            .iter()
            .map(|sequence| crate::chat::action::ProviderWorkerStatusRow {
                task_id: *sequence,
                sequence: *sequence,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: 0,
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            })
            .collect(),
    }
}

#[cfg(feature = "terminal-tui")]
fn active_worker_view_text(state: &ChatState) -> String {
    state
        .ui
        .active_session_view
        .as_ref()
        .map(|view| view.lines.join("\n"))
        .unwrap_or_default()
}

#[cfg(feature = "terminal-tui")]
#[test]
fn phase2_snapshot_exposes_worker_drafts_and_keeps_primary_streaming() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "A live".to_string(),
        version: 1,
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: "B live".to_string(),
        version: 1,
    });

    let snapshot = state.build_ui_snapshot(42);

    assert_eq!(
        snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
        Some("draft-a")
    );
    assert_eq!(
        snapshot
            .streaming_draft_for_worker(20)
            .map(|draft| draft.accumulated.as_str()),
        Some("B live")
    );
    assert!(snapshot.streaming_draft_for_worker(30).is_none());
    assert_eq!(
        snapshot
            .visible_streaming_drafts
            .iter()
            .map(|draft| draft.sequence)
            .collect::<Vec<_>>(),
        vec![10, 20]
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn phase2_worker_pane_focus_uses_matching_draft_not_primary() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "A live".to_string(),
        version: 1,
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: "B live".to_string(),
        version: 1,
    });

    state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 10 };
    let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
        status: provider_worker_status(&[10, 20]),
    });
    let view_a = active_worker_view_text(&state);
    assert!(view_a.contains("assistant streaming: A live"), "{view_a}");
    assert!(!view_a.contains("B live"), "{view_a}");

    state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 20 };
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: " B2".to_string(),
        version: 2,
    });
    let view_b = active_worker_view_text(&state);
    assert!(view_b.contains("assistant streaming: B live B2"), "{view_b}");
    assert!(!view_b.contains("A live"), "{view_b}");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn phase2_worker_pane_missing_draft_uses_empty_io_not_history_or_primary() {
    let mut state = s();
    state.ui.conversation_lines.push(ConversationLine::User {
        content: "history user".to_string(),
    });
    state.ui.conversation_lines.push(ConversationLine::Assistant {
        content: "history assistant must not leak".to_string(),
    });
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "primary live must not leak".to_string(),
        version: 1,
    });

    state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 30 };
    let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
        status: provider_worker_status(&[30]),
    });
    let view = active_worker_view_text(&state);

    assert!(!view.contains("io: recent provider turn"), "{view}");
    assert!(!view.contains("history assistant must not leak"), "{view}");
    assert!(!view.contains("primary live must not leak"), "{view}");
}

#[cfg(feature = "terminal-tui")]
#[test]
fn phase2_main_transcript_primary_streaming_path_is_unchanged() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-b", 20, "second");
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    let snapshot = state.build_ui_snapshot(1);

    assert_eq!(
        snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
        Some("draft-a")
    );
    assert_eq!(
        state
            .stream
            .primary_streaming_draft()
            .map(|draft| draft.draft_id.as_str()),
        Some("draft-a")
    );
}

#[test]
fn phase1_two_visible_drafts_start_without_overwriting() {
    let mut state = s();

    start_phase1_draft(&mut state, "draft-b", 20, "second prompt");
    start_phase1_draft(&mut state, "draft-a", 10, "first prompt");

    assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
    assert_eq!(
        state
            .stream
            .primary_draft()
            .map(|turn| (turn.sequence, turn.prompt_preview.as_str())),
        Some((10, "first prompt"))
    );
    assert!(state.control.generating);
}

#[test]
fn phase1_stream_chunks_route_by_draft_id() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");

    let b_effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: "B1".to_string(),
        version: 1,
    });
    let a_effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "A1".to_string(),
        version: 1,
    });

    assert!(has_request_redraw(&b_effects));
    assert!(has_request_redraw(&a_effects));
    assert_eq!(draft_text(&state, "draft-a"), Some("A1".to_string()));
    assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
    assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
}

#[test]
fn phase1_stream_completed_removes_only_matching_draft() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");

    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "draft-a".to_string(),
        final_text: "answer a".to_string(),
        reasoning: String::new(),
    });

    assert!(has_save_session(&effects));
    assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
    assert!(
        state.control.generating,
        "remaining draft keeps structural generating state"
    );
    assert_eq!(
        state
            .stream
            .primary_streaming_draft()
            .map(|draft| draft.draft_id.as_str()),
        Some("draft-b")
    );
}

#[test]
fn phase1_stale_chunk_for_completed_draft_is_ignored() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "draft-a".to_string(),
        final_text: "answer a".to_string(),
        reasoning: String::new(),
    });

    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "late".to_string(),
        version: 1,
    });

    assert!(effects.is_empty(), "completed draft must reject late chunks");
    assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
    assert_eq!(draft_text(&state, "draft-b"), Some(String::new()));
}

#[test]
fn phase1_stream_cancelled_removes_only_matching_draft() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");

    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "draft-a".to_string(),
    });

    assert!(has_request_redraw(&effects));
    assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
    assert!(
        state.control.generating,
        "cancelling one structural draft must not stop the other"
    );
}

#[test]
fn phase1_stream_failed_removes_only_matching_draft() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");

    let effects = state.reduce(Action::StreamFailed {
        draft_id: "draft-a".to_string(),
        err: "failed a".to_string(),
        retryable: false,
    });

    assert!(has_notify_hook(&effects));
    assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
    assert!(
        state.control.generating,
        "failing one structural draft must not stop the other"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn phase1_snapshot_dirty_changes_when_non_primary_draft_version_changes() {
    let mut state = s();
    start_phase1_draft(&mut state, "draft-a", 10, "first");
    start_phase1_draft(&mut state, "draft-b", 20, "second");
    let before = state.snapshot_dirty_fields();

    let effects = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: "B1".to_string(),
        version: 1,
    });
    let after = state.snapshot_dirty_fields();

    assert!(has_request_redraw(&effects));
    assert_ne!(
        before, after,
        "non-primary draft version must affect snapshot dirty fingerprint"
    );
    assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
    assert_eq!(
        state
            .stream
            .primary_streaming_draft()
            .map(|draft| draft.draft_id.as_str()),
        Some("draft-a"),
        "non-primary chunk must not change primary selection"
    );
}

// ─── S2-A: chat::run stream-path → Redux dispatch wiring tests ─────
//
// These four tests cover the contract after the chat::mod streaming path was wired to Redux:
//   1. dual-write consistency (the M2 acceptance point): for the same delta sequence, the
//      `accumulated` text the old path `update_draft` hands to the terminal ==
//      reducer `stream.draft.accumulated`. The reducer accumulates via StreamChunkReceived, the
//      old path via push_str; the two must be byte-identical.
//   2. the StreamCompleted Effect sequence: contains NotifyHook(TurnComplete) + RequestRedraw.
//   3. the StreamFailed Effect sequence: LogTrace(WARN) + NotifyHook(Error) + RequestRedraw.
//   4. the StreamCancelled Effect sequence: only RequestRedraw (no hook), and cancel is decided
//      **before** failure classification (so no bogus Failed is emitted).

/// S2-A test 1: draft_text_consistency_legacy_vs_redux
///
/// Reproduces the delta accumulation semantics of the chat::mod main loop's `draft_updater` task:
///   - old path: `accumulated.push_str(&delta); update_draft(accumulated)` — passes the running total
///   - new path: `coalescer.try_send_chunk(draft_id, delta, version)` → the reducer's
///     `reduce_stream_chunk_received` accumulates via `draft.accumulated.push_str(delta)`
///
/// On the fast path (no coalescer backpressure) the two must be byte-identical.
#[test]
fn test_s2a_draft_text_consistency_legacy_vs_redux() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-consistency".to_string(),
        cancel: CancellationToken::new(),
    });

    // simulate SSE streaming deltas (with emoji / multi-byte chars to check byte-level equality)
    let deltas: [&str; 6] = [
        "Hel",
        "lo, ",
        "wo",
        "rld",
        " \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}",
        " \u{1f30d}",
    ];
    let mut legacy_accumulated = String::new();
    let mut version: u64 = 0;
    for delta in &deltas {
        // old-path accumulation semantics: accumulated.push_str + update_draft(accumulated)
        legacy_accumulated.push_str(delta);
        // new path: dispatch the incremental delta (not the running total)
        version = version.saturating_add(1);
        let _ = state.reduce(Action::StreamChunkReceived {
            draft_id: "draft-consistency".to_string(),
            delta: (*delta).to_string(),
            version,
        });
    }

    // core acceptance point: the old path's accumulated == the reducer's internal accumulated
    let redux_accumulated = state
        .stream
        .primary_streaming_draft()
        .map(|d| d.accumulated.clone())
        .expect("test: stream.draft must exist after StreamChunkReceived");
    assert_eq!(
        redux_accumulated, legacy_accumulated,
        "S2-A M2: the reducer accumulated must equal the running total the old update_draft received"
    );
    assert_eq!(
        state.stream.primary_streaming_draft().map(|d| d.version),
        Some(version),
        "the reducer version must equal the final counter value inside draft_updater"
    );
}

/// S2-A test 2: stream_completed_effect_sequence
///
/// Success path: after the S2-A change the chat::mod main loop dispatches
///   `Action::StreamCompleted { draft_id, final_text, reasoning: "" }`
/// and the reducer is expected to emit `[NotifyHook(TurnComplete), RequestRedraw]` and clear the draft.
#[test]
fn test_s2a_stream_completed_effect_sequence() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-completed".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-completed".to_string(),
        delta: "the answer".to_string(),
        version: 1,
    });

    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "draft-completed".to_string(),
        final_text: "the answer".to_string(),
        reasoning: String::new(),
    });

    // terminal state cleanup
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared after completion"
    );
    assert!(!state.control.generating, "generating=false after completion");
    assert!(state.control.active_cancel.is_none(), "active_cancel is reset");

    // Effect sequence: NotifyHook(TurnComplete) + RequestRedraw
    let notify_turn_complete = effects.iter().any(|e| {
        matches!(
            e,
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                ..
            }
        )
    });
    assert!(
        notify_turn_complete,
        "StreamCompleted must emit NotifyHook(TurnComplete)"
    );
    assert!(has_request_redraw(&effects), "StreamCompleted must emit RequestRedraw");
}

/// T3-3-c-1: `StreamCompleted` must emit `Effect::SaveSession` (the reducer is the single writer).
///
/// It also checks the Effect sequence contract (order: NotifyHook → SaveSession → RequestRedraw).
#[test]
fn test_t3_3c_stream_completed_emits_save_session() {
    let mut state = s();
    state.session.id = "sess-T3-3c".to_string();
    // record a user turn first so session.turns is non-empty and the snapshot really carries turns
    let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-T3-3c".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "answer".to_string(),
    });
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "draft-T3-3c".to_string(),
        final_text: "answer".to_string(),
        reasoning: String::new(),
    });

    // check that SaveSession exists and the snapshot content is correct
    let save_effect = effects.iter().find(|e| matches!(e, Effect::SaveSession(_)));
    assert!(
        save_effect.is_some(),
        "T3-3-c: StreamCompleted must emit Effect::SaveSession"
    );
    if let Some(Effect::SaveSession(snapshot)) = save_effect {
        assert_eq!(snapshot.id, "sess-T3-3c", "the snapshot id must equal session.id");
        assert_eq!(
            snapshot.turns.len(),
            2,
            "the snapshot must contain the user+assistant turns"
        );
        assert_eq!(snapshot.turns.first().map(|t| t.role.as_str()), Some("user"));
        let assistant = snapshot.turns.get(1).expect("test: turns[1] must exist");
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content, "answer");
        assert_eq!(
            snapshot.title, "question",
            "the auto-title must come from the first user turn"
        );
        // T3-3-fixA P0-1: explicitly assert that the last snapshot.turns entry is this turn's
        // assistant, pinning the dispatch order (RecordAssistantTurn → StreamCompleted) invariant
        let last = snapshot
            .turns
            .last()
            .expect("test: snapshot.turns must have a last entry");
        assert_eq!(last.role, "assistant", "snapshot.turns.last() must be assistant");
        assert_eq!(
            last.content, "answer",
            "the last content must be this turn's assistant text"
        );
    }

    // Effect order contract: NotifyHook first, SaveSession in the middle, RequestRedraw last
    let positions: Vec<&'static str> = effects
        .iter()
        .map(|e| match e {
            Effect::NotifyHook { .. } => "notify",
            Effect::SaveSession(_) => "save",
            Effect::RequestRedraw => "redraw",
            _ => "other",
        })
        .collect();
    let notify_pos = positions.iter().position(|s| *s == "notify");
    let save_pos = positions.iter().position(|s| *s == "save");
    let redraw_pos = positions.iter().position(|s| *s == "redraw");
    assert!(
        notify_pos < save_pos && save_pos < redraw_pos,
        "the Effect order must be NotifyHook < SaveSession < RequestRedraw, got: {positions:?}"
    );
}

/// T3-3-c-2: a repeated `StreamCompleted` must not trigger a second SaveSession (draft cleared → no-op)
#[test]
fn test_t3_3c_duplicate_stream_completed_no_save() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-dup".to_string(),
        cancel: CancellationToken::new(),
    });
    // first time — must contain SaveSession
    let first = state.reduce(Action::StreamCompleted {
        draft_id: "d-dup".to_string(),
        final_text: "ans".to_string(),
        reasoning: String::new(),
    });
    assert!(first.iter().any(|e| matches!(e, Effect::SaveSession(_))));
    // second time — the draft is already None → empty vec, no duplicate SaveSession
    let second = state.reduce(Action::StreamCompleted {
        draft_id: "d-dup".to_string(),
        final_text: "ans-dup".to_string(),
        reasoning: String::new(),
    });
    assert!(second.is_empty(), "a repeated StreamCompleted must be a no-op");
}

/// T3-3-fixA P0-1: two-way regression guard — the dispatch order decides snapshot completeness.
///
/// Forward (RecordAssistantTurn → StreamCompleted): snapshot.turns ends with the assistant turn.
/// Reverse (StreamCompleted → RecordAssistantTurn): snapshot.turns does **not** contain the
/// assistant, because the SaveSession snapshot is built synchronously inside
/// reduce_stream_completed, before RecordAssistantTurn has pushed this turn into session.turns.
///
/// Any future change that reverts the chat::run main loop dispatch order breaks this test,
/// pinning the P0-1 decision into the reducer-level contract.
#[test]
fn t3_3_fix_a_dispatch_order_snapshot_contract() {
    // ── forward: RecordAssistantTurn → StreamCompleted ──
    let mut state_a = s();
    state_a.session.id = "sess-fwd".to_string();
    let _ = state_a.reduce(Action::RecordUserTurn("q".to_string()));
    let _ = state_a.reduce(Action::TurnStarted {
        draft_id: "d-fwd".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state_a.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "a-fwd".to_string(),
    });
    let fwd_effects = state_a.reduce(Action::StreamCompleted {
        draft_id: "d-fwd".to_string(),
        final_text: "a-fwd".to_string(),
        reasoning: String::new(),
    });
    let fwd_snap = fwd_effects
        .iter()
        .find_map(|e| match e {
            Effect::SaveSession(s) => Some(s),
            _ => None,
        })
        .expect("forward: SaveSession must be emitted");
    let last = fwd_snap
        .turns
        .last()
        .expect("forward: snapshot.turns must be non-empty");
    assert_eq!(last.role, "assistant", "forward: the last role must be assistant");
    assert_eq!(
        last.content, "a-fwd",
        "forward: the last content must be this turn's assistant"
    );

    // ── reverse: StreamCompleted → RecordAssistantTurn ──
    let mut state_b = s();
    state_b.session.id = "sess-rev".to_string();
    let _ = state_b.reduce(Action::RecordUserTurn("q".to_string()));
    let _ = state_b.reduce(Action::TurnStarted {
        draft_id: "d-rev".to_string(),
        cancel: CancellationToken::new(),
    });
    let rev_effects = state_b.reduce(Action::StreamCompleted {
        draft_id: "d-rev".to_string(),
        final_text: "a-rev".to_string(),
        reasoning: String::new(),
    });
    let _ = state_b.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "a-rev".to_string(),
    });
    let rev_snap = rev_effects
        .iter()
        .find_map(|e| match e {
            Effect::SaveSession(s) => Some(s),
            _ => None,
        })
        .expect("reverse: SaveSession must be emitted");
    assert!(
        !rev_snap.turns.iter().any(|t| t.role == "assistant"),
        "reverse: snapshot.turns must not contain assistant — this is the bug before the P0-1 fix"
    );
    assert_eq!(
        rev_snap.turns.len(),
        1,
        "reverse: snapshot.turns must hold only the earlier user turn"
    );
}

/// T3-3-fixA P0-2: StreamFailed emits no SaveSession (the error path does not persist).
///
/// Pins the Error row of appendix B's decision table: reduce_stream_failed emits
/// [LogTrace, NotifyHook(Error), RequestRedraw] and no SaveSession. Regression guard: if anyone
/// later wants to "save failures too", this test breaks at once, forcing an appendix B update.
#[test]
fn t3_3_fix_a_stream_error_no_save() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-err".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamFailed {
        draft_id: "d-err".to_string(),
        err: "boom".to_string(),
        retryable: false,
    });
    assert!(
        !has_save_session(&effects),
        "StreamFailed must not emit SaveSession (T3-3-fixA appendix B, Error row)"
    );
}

/// T3-3-fixA P0-2: StreamCancelled emits no SaveSession (a user cancel does not persist).
///
/// Pins the Cancelled row of the appendix B decision table: reduce_stream_cancelled emits only
/// [RequestRedraw]. phase_f_stream_cancelled_no_save_no_hook already covers this; this test keeps
/// the fixA name so regressions can be located by appendix B decision point.
#[test]
fn t3_3_fix_a_stream_cancelled_no_save() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-cancel".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "d-cancel".to_string(),
    });
    assert!(
        !has_save_session(&effects),
        "StreamCancelled must not emit SaveSession (T3-3-fixA appendix B, Cancelled row)"
    );
}

/// S2-A test 3: stream_failed_effect_sequence
///
/// Failure path (timeout / context-overflow / other errors): after the S2-A change the chat::mod
/// main loop dispatches `Action::StreamFailed { draft_id, err, retryable }`.
/// The reducer is expected to emit `[LogTrace(WARN), NotifyHook(Error), RequestRedraw]`.
#[test]
fn test_s2a_stream_failed_effect_sequence() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-failed".to_string(),
        cancel: CancellationToken::new(),
    });
    // simulate receiving a partial chunk earlier in the stream
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-failed".to_string(),
        delta: "partial...".to_string(),
        version: 1,
    });

    let effects = state.reduce(Action::StreamFailed {
        draft_id: "draft-failed".to_string(),
        err: "timeout".to_string(),
        retryable: false,
    });

    // terminal state cleanup
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared after failure"
    );
    assert!(!state.control.generating);

    // Effect sequence assertions (in the order reduce_stream_failed emits them)
    let has_warn_log = effects
        .iter()
        .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
    assert!(has_warn_log, "StreamFailed must emit LogTrace(WARN)");

    let notify_error = effects.iter().any(|e| {
        matches!(
            e,
            Effect::NotifyHook {
                event: HookEvent::Error,
                ..
            }
        )
    });
    assert!(notify_error, "StreamFailed must emit NotifyHook(Error)");
    assert!(has_request_redraw(&effects), "StreamFailed must emit RequestRedraw");

    // retryable=false must reach the hook payload (field mapping check)
    let retryable_in_payload = effects.iter().any(|e| {
        if let Effect::NotifyHook {
            event: HookEvent::Error,
            payload,
        } = e
        {
            payload.get("retryable").and_then(serde_json::Value::as_bool) == Some(false)
        } else {
            false
        }
    });
    assert!(
        retryable_in_payload,
        "retryable=false must reach the NotifyHook payload"
    );
}

/// S2-A test 4: stream_cancelled_effect_sequence
///
/// Cancel path (Ctrl+C / is_tool_loop_cancelled): after the S2-A change the chat::mod main loop
/// decides cancellation **first** and classifies failures **after** — a cancel must dispatch
/// `Action::StreamCancelled { draft_id }` and not `StreamFailed`.
/// The reducer is expected to emit only `[RequestRedraw]` (no hook, no SaveSession).
#[test]
fn test_s2a_stream_cancelled_effect_sequence() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-cancelled".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "draft-cancelled".to_string(),
        delta: "interrupted".to_string(),
        version: 1,
    });

    let effects = state.reduce(Action::StreamCancelled {
        draft_id: "draft-cancelled".to_string(),
    });

    // terminal state cleanup
    assert!(
        state.stream.primary_streaming_draft().is_none(),
        "the draft must be cleared after cancellation"
    );
    assert!(!state.control.generating);
    assert!(state.control.active_cancel.is_none());

    // Effect sequence: only RequestRedraw — no NotifyHook, no LogTrace(WARN), no SaveSession
    assert!(has_request_redraw(&effects), "StreamCancelled must emit RequestRedraw");
    assert!(
        !has_notify_hook(&effects),
        "StreamCancelled must not emit NotifyHook (a cancel is not an error)"
    );
    assert!(
        !has_save_session(&effects),
        "StreamCancelled must not SaveSession (same as Failed, avoiding persisting an interrupted state)"
    );
    let has_warn_log = effects
        .iter()
        .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
    assert!(
        !has_warn_log,
        "StreamCancelled must not emit a WARN LogTrace (a cancel is not a fault)"
    );

    // Protocol contract: a cancel must be decided **before** failure classification — if a cancel
    // were mistaken for a failure and dispatched as StreamFailed, it would emit the
    // NotifyHook(Error) asserted in stream_failed_effect_sequence above, contradicting the
    // !has_notify_hook here and failing. By the absence of NotifyHook(Error) this test indirectly
    // verifies the chat::mod order contract "check is_tool_loop_cancelled first, classify after".
}

/// S2-A test 5 (Codex blocker): tool_call_chunk_interleave_consistency
///
/// Checks that when tool events (`ToolStarted` / `ToolFinished`) and `StreamChunkReceived` are
/// **interleaved** within one turn, the reducer still keeps consistent state on "independent axes":
/// - stream.draft.accumulated is only fed by stream chunks, tool events do not pollute it
/// - tool events only affect ui.conversation_lines / pending_tool_cards, never the draft
/// - the resulting state equals the "all stream chunks first, tools afterwards" sequence
///
/// This is the key invariant for the chat::run tool-call loop running alongside the streaming
/// path — if the reducer wrongly cleared or edited the draft on the ToolStarted/ToolFinished
/// path, users would see the streaming text "suddenly jump backwards".
#[test]
fn test_s2a_tool_call_chunk_interleave_consistency() {
    // ── Scenario A: interleaved sequence — stream / tool / stream / tool ──
    let mut state_a = s();
    let _ = state_a.reduce(Action::TurnStarted {
        draft_id: "draft-interleave".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state_a.reduce(Action::StreamChunkReceived {
        draft_id: "draft-interleave".to_string(),
        delta: "hello ".to_string(),
        version: 1,
    });
    let _ = state_a.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        args: "{\"q\":\"openprx\"}".to_string(),
    });
    let _ = state_a.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        success: true,
        duration_ms: 42,
        result: Some("found 3 results".to_string()),
    });
    let _ = state_a.reduce(Action::StreamChunkReceived {
        draft_id: "draft-interleave".to_string(),
        delta: "world".to_string(),
        version: 2,
    });

    // ── Scenario B: the equivalent "pure streaming, tools last" sequence ──
    let mut state_b = s();
    let _ = state_b.reduce(Action::TurnStarted {
        draft_id: "draft-interleave".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state_b.reduce(Action::StreamChunkReceived {
        draft_id: "draft-interleave".to_string(),
        delta: "hello ".to_string(),
        version: 1,
    });
    let _ = state_b.reduce(Action::StreamChunkReceived {
        draft_id: "draft-interleave".to_string(),
        delta: "world".to_string(),
        version: 2,
    });
    let _ = state_b.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        args: "{\"q\":\"openprx\"}".to_string(),
    });
    let _ = state_b.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        success: true,
        duration_ms: 42,
        result: Some("found 3 results".to_string()),
    });

    // Core invariant: draft.accumulated is identical (tool events do not pollute streaming text)
    let acc_a = state_a
        .stream
        .primary_streaming_draft()
        .map(|d| d.accumulated.clone())
        .expect("test: scenario A draft must exist");
    let acc_b = state_b
        .stream
        .primary_streaming_draft()
        .map(|d| d.accumulated.clone())
        .expect("test: scenario B draft must exist");
    assert_eq!(
        acc_a, "hello world",
        "in the interleaved sequence draft.accumulated is fed only by stream chunks"
    );
    assert_eq!(
        acc_a, acc_b,
        "draft.accumulated must be byte-identical between interleaved and tools-last sequences"
    );

    // the version must match too (tool events do not touch version)
    assert_eq!(
        state_a.stream.primary_streaming_draft().map(|d| d.version),
        state_b.stream.primary_streaming_draft().map(|d| d.version),
        "tool events must not advance stream.version"
    );
    assert_eq!(state_a.stream.primary_streaming_draft().map(|d| d.version), Some(2));

    // the tool cards landed on both sides and were removed from pending after ToolFinished
    assert_eq!(
        state_a.control.pending_tool_card_count(ToolTaskKey::Primary),
        state_b.control.pending_tool_card_count(ToolTaskKey::Primary),
        "both sequences must have the same pending_tool_cards count"
    );
    assert!(
        state_a.control.pending_tool_card_count(ToolTaskKey::Primary) == 0,
        "pending_tool_cards must be empty after ToolFinished"
    );

    // control state matches: still generating, the cancel token unchanged
    assert!(state_a.control.generating);
    assert!(state_b.control.generating);
    assert!(state_a.control.active_cancel.is_some());
    assert!(state_b.control.active_cancel.is_some());
}

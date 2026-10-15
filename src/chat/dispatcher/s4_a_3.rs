#![cfg(test)]

use super::*;
use crate::chat::action::Action;
use crate::chat::state::{ChatState, UiSnapshot};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn make_state() -> ChatState {
    ChatState::new(Arc::from("p-rx"), Arc::from("m-rx"), CancellationToken::new())
}

#[tokio::test]
async fn s4_a_3_dispatcher_pushes_snapshot_on_ui_action() {
    // a UI-affecting Action (SystemMessageAdded) must trigger a snapshot push.
    let state = make_state();
    let initial = Arc::new(UiSnapshot::initial(
        Arc::clone(&state.session.provider),
        Arc::clone(&state.session.model),
    ));
    let (snap_tx, mut snap_rx) = watch::channel(initial);
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let shutdown = CancellationToken::new();
    let _handle = spawn_dispatcher_task_full(
        state,
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
        Some(snap_tx),
    );

    let _ = dispatcher
        .dispatch(Action::SystemMessageAdded { text: "banner".into() })
        .await;
    // wait for watch update
    tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
        .await
        .expect("snap_rx should receive update within 300ms")
        .expect("watch send_if_modified should have fired");
    let snap = snap_rx.borrow();
    assert!(
        snap.revision >= 1,
        "revision should advance to >=1, got {}",
        snap.revision
    );
    assert!(
        !snap.conversation_lines.is_empty(),
        "the snapshot must contain the conversation line written by SystemMessageAdded"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn s4_a_3_dispatched_keypress_updates_snapshot_input() {
    let state = make_state();
    let initial = Arc::new(UiSnapshot::initial(
        Arc::clone(&state.session.provider),
        Arc::clone(&state.session.model),
    ));
    let (snap_tx, mut snap_rx) = watch::channel(initial);
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let shutdown = CancellationToken::new();
    let _handle = spawn_dispatcher_task_full(
        state,
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
        Some(snap_tx),
    );

    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    );
    let _ = dispatcher.dispatch(Action::KeyPressed(key)).await;
    tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
        .await
        .expect("snap_rx should receive input update within 300ms")
        .expect("watch send_if_modified should have fired");

    let snap = snap_rx.borrow();
    assert_eq!(snap.input.text(), "x");

    shutdown.cancel();
}

#[tokio::test]
async fn s4_a_3_dispatcher_skips_unrelated_action() {
    // StreamRetryAttempt is statically judged dirty=false and writes no ui field → no snapshot push.
    let state = make_state();
    let initial = Arc::new(UiSnapshot::initial(
        Arc::clone(&state.session.provider),
        Arc::clone(&state.session.model),
    ));
    let initial_rev = initial.revision;
    let (snap_tx, mut snap_rx) = watch::channel(initial);
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let shutdown = CancellationToken::new();
    let _handle = spawn_dispatcher_task_full(
        state,
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
        Some(snap_tx),
    );

    let _ = dispatcher
        .dispatch(Action::StreamRetryAttempt {
            attempt: 1,
            reason: "transient".to_string(),
        })
        .await;
    // no changed signal must appear within 200ms.
    let result = tokio::time::timeout(Duration::from_millis(200), snap_rx.changed()).await;
    assert!(
        result.is_err(),
        "StreamRetryAttempt must not trigger a snapshot push (changed returned={:?})",
        result.map(|r| r.is_ok())
    );
    assert_eq!(
        snap_rx.borrow().revision,
        initial_rev,
        "the revision must stay unchanged"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn s4_a_3_revision_strict_monotonic_in_pure() {
    // after several UI Actions the revision must increase strictly monotonically.
    let state = make_state();
    let initial = Arc::new(UiSnapshot::initial(
        Arc::clone(&state.session.provider),
        Arc::clone(&state.session.model),
    ));
    let (snap_tx, mut snap_rx) = watch::channel(initial);
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let shutdown = CancellationToken::new();
    let _handle = spawn_dispatcher_task_full(
        state,
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
        Some(snap_tx),
    );

    let mut prev_rev = snap_rx.borrow().revision;
    for i in 0..3 {
        let _ = dispatcher
            .dispatch(Action::SystemMessageAdded {
                text: format!("msg-{i}"),
            })
            .await;
        tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
            .await
            .expect("changed within 300ms")
            .expect("watch send");
        let cur = snap_rx.borrow().revision;
        assert!(
            cur > prev_rev,
            "the revision must increase strictly: prev={prev_rev}, cur={cur}"
        );
        prev_rev = cur;
    }

    shutdown.cancel();
}

#[tokio::test]
async fn s4_a_3_off_mode_no_snapshot_push() {
    // when snapshot_tx=None (Off/Both/Redux), no snapshot is built even if ui_dirty
    // — verifying the zero-overhead contract.
    let state = make_state();
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let shutdown = CancellationToken::new();
    let handle = spawn_dispatcher_task_full(
        state,
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
        None, // key point: snapshot_tx=None
    );

    // push several UI Actions; the dispatcher must not panic or hang.
    for i in 0..5 {
        let _ = dispatcher
            .dispatch(Action::SystemMessageAdded { text: format!("m{i}") })
            .await;
    }
    // give the dispatcher time to process.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.cancel();
    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("handle should complete within 2s after shutdown")
        .expect("task join");
    assert!(
        stats.actions_seen >= 5,
        "must process at least 5 actions, got {}",
        stats.actions_seen
    );
}

// ── BUG-09 / BUG-05 driver-path coverage ──────────────────────────────

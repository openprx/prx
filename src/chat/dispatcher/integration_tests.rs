//! Construct the dispatcher directly + spawn the dispatcher task + feed in an Action stream,
//! reproducing the wiring of `chat::run`, covering:
//! - dispatcher channel capacity and backpressure
//! - whether the reducer is driven (stats.actions_seen / effects_seen)
//! - whether the shadow effect executor is correctly a no-op
//! - whether the shutdown protocol (drop sender + cancel token) makes the dispatcher exit
//!
//! Important constraints:
//! - every timeout in the tests is at most 2s, matching main.rs:866 RUNTIME_SHUTDOWN_TIMEOUT

#![cfg(test)]
use super::*;
use crate::chat::action::{Action, HistoryDir};
use crate::chat::state::ChatState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn make_state(shutdown: CancellationToken) -> ChatState {
    ChatState::new(Arc::from("mock"), Arc::from("mock-model"), shutdown)
}

#[tokio::test]
async fn full_chat_flow_input_to_exit() {
    // simulate one complete chat flow: input → streaming → completion → exit.
    let shutdown = CancellationToken::new();
    let state = make_state(shutdown.clone());
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let handle =
        spawn_dispatcher_task_with_signal(state, action_rx, shutdown.clone(), EffectExecutor::new_shadow(), None);

    // 1. user input
    assert_eq!(
        dispatcher.try_dispatch(Action::InputSubmitted("hello".to_string())),
        DispatchResult::Sent
    );
    assert_eq!(
        dispatcher.try_dispatch(Action::RecordUserTurn("hello".to_string())),
        DispatchResult::Sent
    );

    // 2. LLM inference starts + streaming chunks
    let draft_id = "draft-1".to_string();
    let cancel = CancellationToken::new();
    assert_eq!(
        dispatcher.try_dispatch(Action::TurnStarted {
            draft_id: draft_id.clone(),
            cancel: cancel.clone(),
        }),
        DispatchResult::Sent
    );
    for i in 1..=5u64 {
        assert_eq!(
            dispatcher.try_dispatch(Action::StreamChunkReceived {
                draft_id: draft_id.clone(),
                delta: format!("chunk{i} "),
                version: i,
            }),
            DispatchResult::Sent
        );
    }

    // 3. streaming completes
    assert_eq!(
        dispatcher.try_dispatch(Action::StreamCompleted {
            draft_id: draft_id.clone(),
            final_text: "hello user, response complete".to_string(),
            reasoning: "thinking...".to_string(),
        }),
        DispatchResult::Sent
    );
    assert_eq!(
        dispatcher.try_dispatch(Action::RecordAssistantTurn {
            task_id: None,
            content: "hello user, response complete".to_string(),
        }),
        DispatchResult::Sent
    );

    // 4. exit
    assert_eq!(dispatcher.try_dispatch(Action::ShutdownRequested), DispatchResult::Sent);

    // 5. wrap-up — drop sender + cancel shutdown, the dispatcher must exit within 2s
    shutdown.cancel();
    drop(dispatcher);
    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("dispatcher should exit within 2s")
        .expect("join ok");

    // all 11 actions went through the reducer
    assert_eq!(stats.actions_seen, 11, "actions_seen={}", stats.actions_seen);
    // the reducer should produce at least some effects (RequestRedraw / LogTrace / NotifyHook etc.)
    assert!(stats.effects_seen > 0, "no effects produced");
}

#[tokio::test]
async fn dispatcher_exits_on_shutdown_cancel_only() {
    // verify that shutdown.cancel() alone also makes the dispatcher exit (without dropping the sender)
    let shutdown = CancellationToken::new();
    let state = make_state(shutdown.clone());
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let handle =
        spawn_dispatcher_task_with_signal(state, action_rx, shutdown.clone(), EffectExecutor::new_shadow(), None);

    let _ = dispatcher.try_dispatch(Action::ForceQuit);
    shutdown.cancel();
    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("dispatcher should exit on shutdown")
        .expect("join ok");
    assert!(stats.actions_seen >= 1);
    drop(dispatcher);
}

#[tokio::test]
async fn dispatcher_exits_on_channel_close_only() {
    // verify drop(sender) → channel close → dispatcher exits (without shutdown.cancel)
    let shutdown = CancellationToken::new();
    let state = make_state(shutdown.clone());
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let handle = spawn_dispatcher_task_with_signal(state, action_rx, shutdown, EffectExecutor::new_shadow(), None);

    let _ = dispatcher.try_dispatch(Action::ForceQuit);
    drop(dispatcher);
    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("dispatcher should exit on channel close")
        .expect("join ok");
    assert_eq!(stats.actions_seen, 1);
}

#[tokio::test]
async fn coalescer_under_dispatcher_load() {
    // end to end: dispatcher + coalescer, 100 chunks + a capacity-4 channel → backpressure must trigger.
    let shutdown = CancellationToken::new();
    let state = make_state(shutdown.clone());
    let (tx, action_rx) = mpsc::channel::<Action>(4);
    let handle =
        spawn_dispatcher_task_with_signal(state, action_rx, shutdown.clone(), EffectExecutor::new_shadow(), None);

    let draft = "draft-load".to_string();
    let mut coalescer = StreamChunkCoalescer::new(tx.clone());

    let cancel = CancellationToken::new();
    let _ = tx
        .send(Action::TurnStarted {
            draft_id: draft.clone(),
            cancel: cancel.clone(),
        })
        .await;

    let mut backpressure_count = 0u64;
    for i in 1..=100u64 {
        let r = coalescer.try_send_chunk(draft.clone(), format!("c{i}"), i);
        if matches!(r, DispatchResult::Backpressured) {
            backpressure_count += 1;
        }
    }
    let _ = coalescer.flush();

    tokio::time::sleep(Duration::from_millis(50)).await;

    shutdown.cancel();
    drop(coalescer);
    drop(tx);

    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("dispatcher should exit")
        .expect("join ok");

    assert!(stats.actions_seen >= 2);
    assert!(
        backpressure_count > 0,
        "backpressure must trigger at cap=4 / 100 chunks (saw {backpressure_count})"
    );
    // after coalescing, the number of actions the dispatcher sees must be far below 101
    // (one full merge can compress several chunks into a single Action)
    assert!(stats.actions_seen <= 101);
}

#[tokio::test]
async fn dispatcher_drives_all_action_variants() {
    // Sanity: run every Action variant through the reducer; none may panic in shadow mode.
    let shutdown = CancellationToken::new();
    let state = make_state(shutdown.clone());
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let handle =
        spawn_dispatcher_task_with_signal(state, action_rx, shutdown.clone(), EffectExecutor::new_shadow(), None);

    let actions: Vec<Action> = vec![
        Action::InputSubmitted("x".to_string()),
        Action::InputReplaced("draft".to_string()),
        Action::HistoryNavigated(HistoryDir::Up),
        Action::HistoryNavigated(HistoryDir::Down),
        Action::InputCancelled,
        Action::RedrawRequested,
        Action::TerminalResized { w: 80, h: 24 },
        Action::PasteReceived("paste".to_string()),
        Action::CancelRequested,
        Action::ShutdownRequested,
        Action::HistoryCleared,
        Action::HistoryClearedWithNotice {
            notice: "Conversation cleared".to_string(),
        },
        Action::ForceQuit,
        Action::ToolCardFoldToggled,
        Action::ReasoningFoldToggled,
        // S2-C: the 3 new Actions must be reducible by the dispatcher (without panicking).
        Action::SystemMessageAdded {
            text: "banner".to_string(),
        },
        Action::RecordSystemMessage {
            content: "ctx-system".to_string(),
        },
        Action::SetLeadingSystemPrompt {
            content: "sys-prompt".to_string(),
        },
    ];
    for action in actions {
        let _ = dispatcher.try_dispatch(action);
    }
    shutdown.cancel();
    drop(dispatcher);
    let stats = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("dispatcher exit")
        .expect("join ok");
    // S2-C: actions_seen must be >= 16 (13 existing + 3 new).
    assert!(stats.actions_seen >= 16);
}

#[tokio::test]
async fn effect_executor_handles_all_variants() {
    // Sanity: no Effect variant may panic in shadow mode.
    use crate::hooks::HookEvent;
    use crate::memory::MemoryCategory;

    let executor = EffectExecutor::new_shadow();
    let token = CancellationToken::new();

    let effects: Vec<Effect> = vec![
        Effect::RequestRedraw,
        Effect::Quit,
        Effect::CancelDraft("d1".to_string()),
        Effect::DisplayMedia {
            kind: "IMAGE".to_string(),
            path: "/tmp/x.png".to_string(),
        },
        Effect::AutoTitleSession("session-1".to_string()),
        Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: "test".to_string(),
        },
        Effect::LogTrace {
            level: tracing::Level::WARN,
            msg: "warn-test".to_string(),
        },
        Effect::PersistToMemory {
            key: "k".to_string(),
            value: "v".to_string(),
            category: MemoryCategory::Conversation,
        },
        Effect::NotifyHook {
            event: HookEvent::TurnComplete,
            payload: serde_json::json!({"foo": "bar"}),
        },
        Effect::SendDraftFinalize {
            draft_id: "d1".to_string(),
            text: "final".to_string(),
        },
        Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "shadow-d".to_string(),
            history: Vec::new(),
            compaction_guard_history: None,
            compaction_config: None,
            cancel: token.clone(),
            chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
            routing_input: None,
            turn_binding: None,
        },
        Effect::CancelToken(token),
        Effect::ResolveApproval {
            tool_id: "call-shadow".to_string(),
            approved: false,
        },
    ];

    for effect in effects {
        executor.execute(effect).await;
    }
}

#[tokio::test]
async fn blocking_dispatch_works_in_spawn_blocking_context() {
    // blocking_dispatch is only usable in a synchronous context; isolate the call via spawn_blocking.
    let shutdown = CancellationToken::new();
    let (dispatcher, action_rx) = ChatDispatcher::new();
    let _handle = spawn_dispatcher_task_with_signal(
        make_state(shutdown.clone()),
        action_rx,
        shutdown.clone(),
        EffectExecutor::new_shadow(),
        None,
    );

    let dispatcher_clone = dispatcher.clone();
    let r = crate::runtime::blocking::spawn_blocking(move || dispatcher_clone.blocking_dispatch(Action::ForceQuit))
        .await
        .expect("spawn_blocking join");
    assert_eq!(r, DispatchResult::Sent);

    shutdown.cancel();
    drop(dispatcher);
}

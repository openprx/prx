use super::*;
use crate::chat::sessions::id::SessionId;
use crate::chat::sessions::model::{ManagedKind, ManagedSessionView, ManagedStatus, SessionOrigin};

fn session_view(seq: u64) -> ManagedSessionView {
    ManagedSessionView {
        id: SessionId::from_run_id(&format!("p3-run-{seq}")),
        seq,
        kind: ManagedKind::Agent,
        origin: SessionOrigin::User,
        title: format!("session {seq}"),
        status: ManagedStatus::Running,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
    }
}

#[test]
fn directional_switch_focus_and_synthetic_attach_seq_match() {
    let seq = 42;
    let focus =
        crate::chat::sessions::focus::optimistic_focus(crate::chat::sessions::focus::RoutingIntent::Attach { seq });

    assert_eq!(focus, crate::chat::sessions::FocusTarget::Session { seq });
    assert_eq!(attach_command_for_seq(seq), "/attach 42");
}

#[test]
fn synthetic_ui_command_is_identified_for_hidden_echo_path() {
    let mut msg = crate::channels::traits::ChannelMessage {
        id: "synthetic".to_string(),
        sender: SYNTHETIC_UI_COMMAND_SENDER.to_string(),
        reply_target: "user".to_string(),
        content: "/attach 42".to_string(),
        channel: "terminal".to_string(),
        timestamp: 0,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    };

    assert!(is_synthetic_ui_command(&msg));

    msg.sender = "user".to_string();
    assert!(!is_synthetic_ui_command(&msg));
}

fn input_msg(content: &str) -> crate::channels::traits::ChannelMessage {
    crate::channels::traits::ChannelMessage {
        id: uuid::Uuid::new_v4().to_string(),
        sender: "user".to_string(),
        reply_target: "user".to_string(),
        content: content.to_string(),
        channel: "terminal".to_string(),
        timestamp: 0,
        thread_ts: None,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        mentioned_uuids: vec![],
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

#[test]
fn priority_input_prefix_is_stripped_for_execution() {
    let (priority, content) = input_priority_from_text("  /now   inspect queue  ");
    assert_eq!(priority, InputQueuePriority::Priority);
    assert_eq!(content, "inspect queue");

    let (priority, content) = input_priority_from_text("!! urgent");
    assert_eq!(priority, InputQueuePriority::Priority);
    assert_eq!(content, "urgent");
}

#[test]
fn active_turn_enqueue_reports_priority_and_strips_prefix() {
    let mut backlog = std::collections::VecDeque::new();

    let priority = enqueue_input_message_and_return_priority(&mut backlog, input_msg("/now inspect status"));

    assert_eq!(priority, InputQueuePriority::Priority);
    let msg = pop_next_queued_input_message(&mut backlog)
        .map(|queued| queued.msg)
        .expect("queued priority message");
    assert_eq!(msg.content, "inspect status");
}

#[test]
fn input_backlog_status_counts_priority_messages() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 3);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 3);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal three"), 3);

    // the production status source: the scheduler the queue mirrors into
    let status = scheduler.status().main_queue_status();

    assert_eq!(status.queued, 3);
    assert_eq!(status.priority, 1);
}

#[test]
fn scheduler_mirror_projects_same_queue_status_as_backlog() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 5);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 5);

    assert_eq!(backlog.len(), scheduler.status().main_queue_status().queued);
    assert_eq!(scheduler.status().main_queue_status().queued, 2);
    assert_eq!(scheduler.status().main_queue_status().priority, 1);
}

#[test]
fn scheduler_mirror_does_not_change_priority_pop_order() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 7);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 7);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal three"), 7);

    let first = pop_next_input_task_with_scheduler(&mut backlog, &mut scheduler)
        .map(|task| task.msg)
        .expect("priority first");
    let second = pop_next_input_task_with_scheduler(&mut backlog, &mut scheduler)
        .map(|task| task.msg)
        .expect("normal one second");
    let third = pop_next_input_task_with_scheduler(&mut backlog, &mut scheduler)
        .map(|task| task.msg)
        .expect("normal three third");

    assert_eq!(first.content, "urgent two");
    assert_eq!(second.content, "normal one");
    assert_eq!(third.content, "normal three");
    assert_eq!(
        scheduler.status().main_queue_status(),
        crate::chat::action::MainQueueStatus::default()
    );
}

#[test]
fn dequeued_input_task_id_is_reused_for_provider_start() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();

    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal provider turn"), 7);
    let expected_id = backlog
        .front()
        .and_then(|queued| queued.turn_task_id)
        .expect("queued task id");

    let dequeued = pop_next_input_task_with_scheduler(&mut backlog, &mut scheduler).expect("dequeued input");

    assert_eq!(dequeued.turn_task_id, Some(expected_id));
    assert_eq!(
        scheduler.task(expected_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Dispatched
    );
    assert_eq!(
        scheduler.status().main_queue_status(),
        crate::chat::action::MainQueueStatus::default()
    );

    let provider_id = start_provider_turn_task(&mut scheduler, dequeued.turn_task_id, &dequeued.msg.content, 7)
        .expect("provider task id");

    assert_eq!(provider_id, expected_id);
    assert_eq!(
        scheduler.task(provider_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Running
    );
}

#[test]
fn queue_report_summarizes_backlog_with_preview() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 3);
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("/priority urgent two"), 3);
    let running = scheduler.enqueue("running turn", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(running).expect("running turn starts");

    let report = format_input_backlog_report(&backlog, &scheduler, 8);

    assert!(
        report.contains("Main queue: 2 queued (1 priority), 1 running."),
        "{report}"
    );
    assert!(report.contains("1. [normal] normal one"), "{report}");
    assert!(report.contains("2. [priority] urgent two"), "{report}");
}

#[tokio::test]
async fn active_turn_queue_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal one"), 3);
    let running = scheduler.enqueue("active turn", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(running).expect("active turn starts");
    let msg = input_msg("/now /queue");
    let chat_session = session::ChatSession::new("p", "m");
    let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    let output = active_turn_local_command_output(
        &msg,
        &backlog,
        &scheduler,
        &chat_session,
        &provider_turn_workers,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await
    .expect("queue command output");

    assert!(
        output.contains("Main queue: 1 queued (0 priority), 1 running."),
        "{output}"
    );
    assert_eq!(backlog.len(), 1, "read-only queue command must not mutate backlog");
}

#[tokio::test]
async fn active_turn_cost_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let msg = input_msg("/now /cost");
    let chat_session = session::ChatSession::new("p", "m");
    let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    let output = active_turn_local_command_output(
        &msg,
        &backlog,
        &scheduler,
        &chat_session,
        &provider_turn_workers,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await
    .expect("cost command output");

    assert!(output.contains("Session cost:"), "{output}");
    assert!(output.contains("Turns:"), "{output}");
    assert_eq!(backlog.len(), 1, "read-only cost command must not mutate backlog");
}

#[tokio::test]
async fn active_turn_workers_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    let msg = input_msg("/now /workers");
    let chat_session = session::ChatSession::new("p", "m");
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("long worker", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(task_id).expect("task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("worker starts");
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    let output = active_turn_local_command_output(
        &msg,
        &backlog,
        &scheduler,
        &chat_session,
        &provider_turn_workers,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await
    .expect("workers command output");

    assert!(output.contains("Main provider workers: 1 running"), "{output}");
    assert!(output.contains("w#1 task="), "{output}");
    assert!(output.contains("kind=foreground_awaited"), "{output}");
    assert!(output.contains("state=running"), "{output}");
    assert_eq!(backlog.len(), 1, "read-only workers command must not mutate backlog");
}

#[test]
fn workers_cancel_command_parser_accepts_worker_sequence_forms() {
    assert_eq!(
        parse_workers_cancel_command("/workers cancel w#7"),
        ProviderWorkerCancelCommand::Cancel { sequence: 7 }
    );
    assert_eq!(
        parse_workers_cancel_command("/workers stop #8"),
        ProviderWorkerCancelCommand::Cancel { sequence: 8 }
    );
    assert_eq!(
        parse_workers_cancel_command("/workers cancel 9"),
        ProviderWorkerCancelCommand::Cancel { sequence: 9 }
    );
    assert!(matches!(
        parse_workers_cancel_command("/workers cancel nope"),
        ProviderWorkerCancelCommand::Invalid(_)
    ));
    assert!(matches!(
        parse_workers_cancel_command("/workers status"),
        ProviderWorkerCancelCommand::NotCancel
    ));
}

#[tokio::test]
async fn active_turn_workers_cancel_command_marks_current_worker_cancelling_without_enqueue() {
    let msg = input_msg("/now /workers cancel w#1");
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("long worker", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(task_id).expect("task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("worker starts");

    let (output, signal_cancel) =
        active_turn_workers_cancel_output(&msg, &mut scheduler, &mut provider_turn_workers, Some(task_id))
            .expect("workers cancel output");

    assert_eq!(
        signal_cancel,
        Some(ProviderWorkerCancelSignal::CancelRequested),
        "current foreground worker should signal the active turn"
    );
    assert!(
        output.contains("Requested cancellation for provider worker w#1"),
        "{output}"
    );
    assert!(output.contains("state=cancelling"), "{output}");
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelling
    );
    assert_eq!(
        provider_turn_workers.worker(task_id).unwrap().state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
    );
}

#[tokio::test]
async fn workers_cancel_command_targets_detached_worker_without_cancelling_peer() {
    let msg = input_msg("/workers cancel w#2");
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    let second = scheduler.enqueue("second detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(first).expect("first task starts");
    scheduler.start_task(second).expect("second task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(first).expect("first task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("first worker starts");
    provider_turn_workers
        .start_from_task(
            scheduler.task(second).expect("second task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("second worker starts");

    let (output, signal_cancel) =
        active_turn_workers_cancel_output(&msg, &mut scheduler, &mut provider_turn_workers, Some(first))
            .expect("workers cancel output");

    assert!(
        output.contains("Requested cancellation for provider worker w#2"),
        "{output}"
    );
    assert!(output.contains("kind=detached"), "{output}");
    assert_eq!(
        signal_cancel,
        Some(ProviderWorkerCancelSignal::CancelProviderTurn { task_id: second }),
        "detached worker cancellation must target the requested task id"
    );
    assert_eq!(
        scheduler.task(first).expect("first task").state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "peer detached task must stay running"
    );
    assert_eq!(
        scheduler.task(second).expect("second task").state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelling,
        "requested detached task should enter cancelling"
    );
    assert_eq!(
        provider_turn_workers.worker(first).expect("first worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Running,
        "peer detached worker must stay running"
    );
    assert_eq!(
        provider_turn_workers.worker(second).expect("second worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling,
        "requested detached worker should enter cancelling"
    );
}

#[test]
fn workers_cancel_command_targets_detached_worker_from_outer_loop() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    let second = scheduler.enqueue("second detached", crate::chat::turn_scheduler::TurnPriority::Normal, 3);
    scheduler.start_task(first).expect("first task starts");
    scheduler.start_task(second).expect("second task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(first).expect("first task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("first worker starts");
    provider_turn_workers
        .start_from_task(
            scheduler.task(second).expect("second task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("second worker starts");

    let (output, signal_cancel) = provider_workers_cancel_output_for_input(
        &input_msg("/workers cancel w#2"),
        "/workers cancel w#2",
        &mut scheduler,
        &mut provider_turn_workers,
        None,
    )
    .expect("workers cancel output");

    assert!(
        output.contains("Requested cancellation for provider worker w#2"),
        "{output}"
    );
    assert_eq!(
        signal_cancel,
        Some(ProviderWorkerCancelSignal::CancelProviderTurn { task_id: second }),
        "outer-loop worker cancel must target the requested detached task"
    );
    assert_eq!(
        scheduler.task(first).expect("first task").state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "peer detached task must stay running"
    );
    assert_eq!(
        scheduler.task(second).expect("second task").state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelling,
        "requested detached task should enter cancelling"
    );
}

#[tokio::test]
async fn p4a_active_turn_event_pump_preserves_cancel_local_and_enqueue_order() {
    let first_msg = input_msg("/workers cancel w#1");
    let (tx, mut input_rx) = mpsc::channel(4);
    tx.send(input_msg("/queue")).await.expect("send queue command");
    tx.send(input_msg("follow up turn")).await.expect("send follow-up turn");
    drop(tx);
    let mut outputs = Vec::<String>::new();
    let mut emit = |text: &str| outputs.push(text.to_string());
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "active provider turn",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        3,
    );
    scheduler.start_task(task_id).expect("task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("worker starts");
    let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
    let chat_session = session::ChatSession::new("p", "m");
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    process_active_turn_input_batch(
        first_msg,
        &mut emit,
        &mut input_rx,
        &mut backlog,
        &mut scheduler,
        &mut provider_turn_workers,
        Some(task_id),
        &chat_dispatcher,
        &chat_session,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await;

    assert_eq!(outputs.len(), 2, "cancel and /queue should emit local output");
    assert!(
        outputs[0].contains("Requested cancellation for provider worker w#1"),
        "{}",
        outputs[0]
    );
    assert!(
        outputs[1].contains("Main queue: 0 queued (0 priority), 0 running."),
        "{}",
        outputs[1]
    );
    assert_eq!(backlog.len(), 1, "ordinary input after local commands remains queued");
    assert_eq!(backlog.front().expect("queued follow-up").msg.content, "follow up turn");
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelling
    );
    assert_eq!(scheduler.status().main_queue_status().queued, 1);
}

/// Build the input message a daemon assignment arrives as.
fn assigned_msg(id: &str, task: &str, disposition: &str) -> crate::channels::traits::ChannelMessage {
    let pulled = crate::runtime::tasks_cli::PulledAssignment {
        assignment_id: id.to_string(),
        session_id: "sess".to_string(),
        task: task.to_string(),
        disposition: disposition.to_string(),
        deliveries: 1,
        created_at_unix_ms: 0,
        work_id: "w1".to_string(),
        origin_channel: "wacli".to_string(),
        origin_ref: Some("3d1a4f5b".to_string()),
    };
    crate::chat::sessions::assignment::assignment_message(
        &pulled,
        crate::chat::sessions::assignment::Disposition::from_wire(disposition),
    )
}

/// Feed one assignment into a chat with a turn in flight, and report what
/// the backlog and the running task look like afterwards.
async fn dispatch_assignment_during_a_turn(
    disposition: &str,
) -> (
    Vec<(InputQueuePriority, String)>,
    crate::chat::turn_scheduler::TurnTaskState,
) {
    let msg = assigned_msg("assign-1", "count the crates", disposition);
    let (tx, mut input_rx) = mpsc::channel::<crate::channels::traits::ChannelMessage>(4);
    drop(tx);
    // The output sink is read back: an assignment must never emit a local
    // command answer, which is what it would mean for it to be handled as
    // one instead of run as a turn.
    let mut outputs = Vec::<String>::new();
    let mut emit = |text: &str| outputs.push(text.to_string());
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "active provider turn",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        3,
    );
    scheduler.start_task(task_id).expect("task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("worker starts");
    let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
    let chat_session = session::ChatSession::new("p", "m");
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    process_active_turn_input_batch(
        msg,
        &mut emit,
        &mut input_rx,
        &mut backlog,
        &mut scheduler,
        &mut provider_turn_workers,
        Some(task_id),
        &chat_dispatcher,
        &chat_session,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await;

    assert!(
        outputs.is_empty(),
        "an assignment must be run as a turn, not answered as a local command: {outputs:?}"
    );
    let queued = backlog
        .iter()
        .map(|entry| (entry.priority, entry.msg.content.clone()))
        .collect::<Vec<_>>();
    let state = scheduler.task(task_id).expect("task snapshot").state;
    (queued, state)
}

/// The three dispositions differ in exactly two observable ways: where the
/// work lands in the queue, and whether the turn in flight survives.
#[tokio::test]
async fn each_assignment_disposition_reaches_the_path_it_names() {
    let (queued, state) = dispatch_assignment_during_a_turn("queue").await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, InputQueuePriority::Normal, "queue must not jump the queue");
    assert_eq!(
        queued[0].1, "count the crates",
        "the model must see the task, not a prefix"
    );
    assert_eq!(
        state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "queue must leave the turn in flight alone"
    );

    let (queued, state) = dispatch_assignment_during_a_turn("steer").await;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].0,
        InputQueuePriority::Priority,
        "steer must go in front of the backlog"
    );
    assert_eq!(queued[0].1, "count the crates");
    assert_eq!(
        state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "steer inserts into the turn in flight; it must not destroy it"
    );

    let (queued, state) = dispatch_assignment_during_a_turn("interrupt").await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, InputQueuePriority::Priority);
    assert_eq!(queued[0].1, "count the crates");
    assert_eq!(
        state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelling,
        "interrupt must end the turn in flight before the assignment runs"
    );
}

/// A word this chat cannot read is never permission to destroy a turn.
#[tokio::test]
async fn an_unknown_disposition_never_ends_the_turn_in_flight() {
    let (queued, state) = dispatch_assignment_during_a_turn("obliterate").await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, InputQueuePriority::Normal);
    assert_eq!(
        state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "an unrecognised disposition degrades to queue, never to interrupt"
    );
}

/// Ordinary typed input must not acquire assignment behaviour.
#[tokio::test]
async fn ordinary_input_during_a_turn_still_only_queues() {
    let msg = input_msg("just a message");
    assert!(crate::chat::sessions::assignment::delivered(&msg).is_none());
}

#[tokio::test]
async fn p4a_active_turn_quit_is_queued_for_delayed_exit() {
    let mut outputs = Vec::<String>::new();
    let mut emit = |text: &str| outputs.push(text.to_string());
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "active provider turn",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        3,
    );
    scheduler.start_task(task_id).expect("task starts");
    let mut provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    provider_turn_workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("worker starts");
    let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
    let chat_session = session::ChatSession::new("p", "m");
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let daemon_config = Config::default();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    process_active_turn_input_message(
        input_msg("/exit"),
        &mut emit,
        &mut backlog,
        &mut scheduler,
        &mut provider_turn_workers,
        Some(task_id),
        &chat_dispatcher,
        &chat_session,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
        daemon_control,
    )
    .await;

    assert!(outputs.is_empty(), "/exit during an active turn is not a local command");
    assert_eq!(backlog.len(), 1, "/exit should wait behind the active turn");
    assert_eq!(backlog.front().expect("queued exit").msg.content, "/exit");
    assert_eq!(scheduler.status().main_queue_status().queued, 1);
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Running,
        "delayed exit must not cancel or complete the active provider turn"
    );
}

#[test]
fn persistence_dependent_commands_cover_terminal_session_contract() {
    for command in [
        "/exit",
        "/quit",
        "/apply",
        "/cost",
        "/export",
        "/new",
        "/clear",
        "/resume",
        "/resume last",
        "/branch 1",
        "/rewind 0",
        "/priority /cost",
    ] {
        assert!(is_persistence_dependent_command(command), "{command}");
    }
    for command in ["hello", "/help", "/workers", "/queue status"] {
        assert!(!is_persistence_dependent_command(command), "{command}");
    }
}

#[test]
fn new_session_has_fresh_identity_and_empty_session_scoped_ledgers() {
    let first = fresh_chat_session("provider-a", "model-a", commands::ChatMode::Plan);
    let second = fresh_chat_session("provider-a", "model-a", commands::ChatMode::Plan);

    assert_ne!(first.id, second.id);
    assert!(first.title.is_empty(), "expected an empty value");
    assert!(first.turns.is_empty());
    assert!(first.token_usage_records.is_empty(), "expected an empty value");
    assert!(first.background_sessions.is_empty(), "expected an empty value");
    assert_eq!(first.provider, "provider-a");
    assert_eq!(first.model, "model-a");
    assert_eq!(first.mode, commands::ChatMode::Plan);
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_input_close_keeps_event_pump_alive_until_pending_turn_finishes() {
    assert!(
        should_continue_event_pump_after_input_closed(1),
        "closed input should not terminate the event pump while a detached Redux turn is pending"
    );
    assert!(
        !should_continue_event_pump_after_input_closed(0),
        "with no pending Redux turn, closed input can end the outer loop"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_deferred_resume_waits_for_queued_visible_input_from_current_session() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    let mut deferred_resume = std::collections::VecDeque::from(["session-b".to_string()]);
    let session_a_history_len = 4;

    enqueue_input_message_with_scheduler(
        &mut backlog,
        &mut scheduler,
        input_msg("queued for session A"),
        session_a_history_len,
    );

    assert!(
        !should_drain_deferred_resume_after_visible_inputs(0, &backlog, &workers),
        "deferred resume must not run before queued visible input is dequeued"
    );
    assert_eq!(
        deferred_resume.front().map(String::as_str),
        Some("session-b"),
        "resume request remains deferred while session-A input is pending"
    );

    let next = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        1,
    )
    .expect("queued input");
    assert_eq!(next.msg.content, "queued for session A");
    let task_id = next.turn_task_id.expect("scheduler task id");
    let task = scheduler.task(task_id).expect("queued task snapshot");
    assert_eq!(
        task.history_base_len, session_a_history_len,
        "queued input keeps the pre-resume session-A history boundary"
    );
    assert_eq!(
        task.state,
        crate::chat::turn_scheduler::TurnTaskState::Dispatched,
        "queued input is dispatched before deferred resume can drain"
    );

    assert!(should_drain_deferred_resume_after_visible_inputs(0, &backlog, &workers));
    assert_eq!(deferred_resume.pop_front().as_deref(), Some("session-b"));
}

#[test]
fn workers_report_lists_only_active_worker_rows() {
    let status = crate::chat::action::ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 1,
        finalized_total_tokens: 1_250,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(1_000)),
        rows: vec![
            crate::chat::action::ProviderWorkerStatusRow {
                task_id: 1,
                sequence: 1,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(1_000),
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            },
            crate::chat::action::ProviderWorkerStatusRow {
                task_id: 2,
                sequence: 2,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: crate::chat::action::ProviderWorkerRowState::Committed,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(9_000),
                finalized_total_tokens: Some(1_250),
                completion_ready: true,
                recent_tool_call: None,
            },
        ],
    };

    let report = format_provider_worker_report(&status);

    assert!(
        report.contains("1 finalized payloads, 1.2k finalized tokens"),
        "report should retain finalized aggregate: {report}"
    );
    assert!(report.contains("- w#1 task=1"), "active row should be listed: {report}");
    assert!(
        report.contains("completion=pending"),
        "active row should expose completion readiness: {report}"
    );
    assert!(
        !report.contains("- w#2 task=2"),
        "completed row should not stay in the active worker list: {report}"
    );
}

/// Backpressure contract for the provider-turn lifecycle channel.
///
/// The channel is bounded, so the guarantee under load is: a producer that
/// finds it full is *slowed down* (its `send` parks until the chat loop
/// drains) rather than dropping events or panicking, and every event
/// eventually arrives in order.
#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn provider_turn_lifecycle_channel_backpressures_instead_of_dropping() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("turn", crate::chat::turn_scheduler::TurnPriority::Normal, 1);

    let (tx, mut rx) =
        mpsc::channel::<dispatcher::ProviderTurnLifecycleEvent>(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);

    // Fill the queue exactly to capacity.
    for lease_id in 0..PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY as u64 {
        tx.try_send(dispatcher::ProviderTurnLifecycleEvent::Started { task_id, lease_id })
            .expect("channel accepts events up to its capacity");
    }
    assert!(
        tx.try_send(dispatcher::ProviderTurnLifecycleEvent::Started {
            task_id,
            lease_id: u64::MAX,
        })
        .is_err(),
        "channel must be bounded at PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY"
    );

    // The overflow send parks; it neither resolves nor discards the event.
    let overflow = tx.send(dispatcher::ProviderTurnLifecycleEvent::Exited {
        task_id,
        lease_id: u64::MAX,
    });
    tokio::pin!(overflow);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut overflow)
            .await
            .is_err(),
        "a full queue must park the producer rather than complete the send"
    );

    // Draining frees a permit, which wakes the parked producer.
    let first = rx.recv().await.expect("queued event is delivered");
    assert!(matches!(
        first,
        dispatcher::ProviderTurnLifecycleEvent::Started { lease_id: 0, .. }
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), overflow)
        .await
        .expect("parked producer resumes once the consumer drains")
        .expect("receiver is still alive");

    // Nothing was lost or reordered: the remaining fillers arrive in order,
    // followed by the event that had been waiting on backpressure.
    for expected_lease in 1..PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY as u64 {
        match rx.recv().await.expect("queued event is delivered") {
            dispatcher::ProviderTurnLifecycleEvent::Started { lease_id, .. } => {
                assert_eq!(lease_id, expected_lease);
            }
            other => panic!("unexpected event while draining: {other:?}"),
        }
    }
    match rx.recv().await.expect("backpressured event is delivered") {
        dispatcher::ProviderTurnLifecycleEvent::Exited { lease_id, .. } => {
            assert_eq!(lease_id, u64::MAX);
        }
        other => panic!("unexpected trailing event: {other:?}"),
    }
}

#[cfg(feature = "terminal-tui")]
#[test]
fn non_current_provider_completion_is_retained_and_marks_ready() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    let second = scheduler.enqueue("second", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(first).expect("first starts");
    scheduler.start_task(second).expect("second starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(first).expect("first task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("first worker starts");
    workers
        .start_from_task(
            scheduler.task(second).expect("second task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("second worker starts");
    workers
        .record_execution_started(second, 42)
        .expect("second worker execution starts");

    let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::channel(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);
    let mut lifecycle_open = true;
    let mut pending = std::collections::HashMap::new();
    let completion = ProviderTurnCompletionEvent {
        task_id: second,
        outcome: Some(dispatcher::TurnOutcomeKind::Completed {
            final_text: "second done".to_string(),
            reasoning: String::new(),
        }),
        usage: crate::llm::route_decision::TokenUsage {
            total_tokens: Some(42),
            ..Default::default()
        },
    };

    let route = route_provider_completion_event(
        &mut lifecycle_rx,
        &mut lifecycle_open,
        &mut workers,
        &mut pending,
        Some(first),
        completion,
    );

    assert!(route.is_none(), "a non-current completion is retained as pending");
    assert!(
        pending.contains_key(&second),
        "non-current completion should be retained"
    );
    let rows = workers.snapshot();
    let second_row = rows
        .iter()
        .find(|row| row.task_id == second)
        .expect("second worker snapshot");
    assert!(
        second_row.completion_ready,
        "retained completion should still mark the worker completion-ready"
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.task_id == first)
            .expect("first worker snapshot")
            .completion_ready,
        "uncompleted worker should remain pending"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn current_provider_completion_is_routed_without_retaining_pending() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("current", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(task_id).expect("task starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("worker starts");
    workers
        .record_execution_started(task_id, 43)
        .expect("worker execution starts");

    let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::channel(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);
    let mut lifecycle_open = true;
    let mut pending = std::collections::HashMap::new();
    let completion = ProviderTurnCompletionEvent {
        task_id,
        outcome: Some(dispatcher::TurnOutcomeKind::Completed {
            final_text: "current done".to_string(),
            reasoning: String::new(),
        }),
        usage: crate::llm::route_decision::TokenUsage {
            total_tokens: Some(43),
            ..Default::default()
        },
    };

    let route = route_provider_completion_event(
        &mut lifecycle_rx,
        &mut lifecycle_open,
        &mut workers,
        &mut pending,
        Some(task_id),
        completion,
    );

    let completion = route.expect("current completion must not be retained as pending");
    assert_eq!(completion.task_id, task_id);
    assert_eq!(completion.usage.total_tokens, Some(43));
    assert!(pending.is_empty(), "current completion should bypass pending map");
    assert!(
        workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot")
            .completion_ready,
        "current completion should still mark the worker completion-ready"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_outer_completion_route_retains_event_for_pending_redux_finalizer() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("redux pending", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(task_id).expect("task starts");
    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("worker starts");
    workers
        .record_execution_started(task_id, 60)
        .expect("worker execution starts");
    let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::channel(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);
    let mut lifecycle_open = true;
    let mut pending = std::collections::HashMap::new();
    let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();
    let completion = ProviderTurnCompletionEvent {
        task_id,
        outcome: Some(dispatcher::TurnOutcomeKind::Completed {
            final_text: "done".to_string(),
            reasoning: String::new(),
        }),
        usage: crate::llm::route_decision::TokenUsage {
            total_tokens: Some(9),
            ..Default::default()
        },
    };

    let route = route_provider_completion_event_and_publish(
        &mut lifecycle_rx,
        &mut lifecycle_open,
        &mut workers,
        &mut pending,
        None,
        completion,
        &chat_dispatcher,
    );

    assert!(route.is_none(), "a non-current completion is retained as pending");
    let retained = pending.get(&task_id).expect("outer completion retained by task id");
    assert_eq!(retained.usage.total_tokens, Some(9));
    assert!(
        workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("worker snapshot")
            .completion_ready,
        "outer route still marks worker completion-ready before finalizer"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4a_completion_event_and_finalizer_chain_commits_ready_turn() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "redux pending finalizer",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        1,
    );
    scheduler.start_task(task_id).expect("task starts");
    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    coordinator
        .register_task(scheduler.task(task_id).expect("task snapshot"))
        .expect("history task registers");
    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(task_id).expect("task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("worker starts");
    workers
        .record_execution_started(task_id, 64)
        .expect("worker execution starts");
    let (_tx, mut lifecycle_rx) = tokio::sync::mpsc::channel(PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY);
    let mut lifecycle_open = true;
    let mut pending = std::collections::HashMap::new();
    let (chat_dispatcher, _action_rx) = dispatcher::ChatDispatcher::new();

    let route = route_provider_completion_event_and_publish(
        &mut lifecycle_rx,
        &mut lifecycle_open,
        &mut workers,
        &mut pending,
        None,
        ProviderTurnCompletionEvent {
            task_id,
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "ready finalizer".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(64),
                ..Default::default()
            },
        },
        &chat_dispatcher,
    );
    assert!(route.is_none(), "a non-current completion is retained as pending");

    let completion = pending
        .remove(&task_id)
        .expect("ready completion retained for finalizer");
    let tools: Vec<Box<dyn Tool>> = Vec::new();
    let plan = provider_turn_terminal_plan_from_completion(
        ResolvedProviderTurnCompletion {
            outcome: completion.outcome,
            usage: completion.usage,
        },
        1,
        &tools,
    );
    let mut queue = std::collections::VecDeque::new();
    enqueue_provider_turn_finalizer_event(&mut queue, Some(task_id), plan);

    let results = drain_provider_turn_finalizer_events_and_publish(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        &mut queue,
        &chat_dispatcher,
    );

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].terminal_status, "completed");
    assert!(results[0].finalized);
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Completed
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert_eq!(row.finalized_total_tokens, Some(64));
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4b1_ready_signal_is_noop_until_ordered_commit_dispatches_save() {
    use crate::chat::action::Action;
    use crate::chat::state::{ChatState, Effect};
    use std::sync::Arc;

    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("ordered commit", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(task_id).expect("task starts");
    let sequence = scheduler.task(task_id).expect("task snapshot").sequence;
    let mut state = ChatState::new(Arc::from("provider"), Arc::from("model"), CancellationToken::new());
    let _ = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: Some(task_id),
        provider_turn_sequence: Some(sequence),
        draft_id: "draft-p4b1".to_string(),
        history: Vec::new(),
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });

    let ready_effects = state.reduce(Action::ProviderTurnReadyForCommit {
        draft_id: "draft-p4b1".to_string(),
        final_text: "answer".to_string(),
        reasoning: "reason".to_string(),
    });
    assert!(ready_effects.is_empty(), "ready signal must not emit SaveSession");
    assert!(
        state.stream.primary_streaming_draft().is_some(),
        "ready signal must leave reducer draft open until ordered commit"
    );

    let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();
    dispatch_ordered_provider_turn_commit(
        &dispatcher,
        task_id,
        "draft-p4b1",
        "question",
        "answer",
        "reason",
        false,
        true,
        crate::chat::provider_turns::TurnEventIds::default(),
    )
    .expect("ordered history dispatch");

    let user = rx.try_recv().expect("ordered RecordUserTurn action");
    match user {
        Action::RecordUserTurn(content, event_id) => {
            assert_eq!(content, "question");
            let effects = state.reduce(Action::RecordUserTurn(content, event_id));
            assert!(
                !effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                "RecordUserTurn alone must not save before terminal commit"
            );
        }
        other => panic!("expected ordered RecordUserTurn, got {other:?}"),
    }

    let record = rx.try_recv().expect("ordered RecordAssistantTurn action");
    match record {
        Action::RecordAssistantTurn {
            task_id: seen,
            content,
            event_id,
        } => {
            assert_eq!(seen, Some(task_id));
            assert_eq!(content, "answer");
            let effects = state.reduce(Action::RecordAssistantTurn {
                task_id: seen,
                content,
                event_id,
            });
            assert!(
                !effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                "RecordAssistantTurn alone must not save before terminal commit"
            );
        }
        other => panic!("expected ordered RecordAssistantTurn, got {other:?}"),
    }

    let terminal = rx.try_recv().expect("ordered StreamCompleted action");
    match terminal {
        Action::StreamCompleted {
            draft_id,
            final_text,
            reasoning,
        } => {
            assert_eq!(draft_id, "draft-p4b1");
            assert_eq!(final_text, "answer");
            assert_eq!(reasoning, "reason");
            let effects = state.reduce(Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            });
            assert!(
                effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
                "SaveSession must be emitted only by ordered StreamCompleted"
            );
        }
        other => panic!("expected ordered StreamCompleted, got {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "ordered commit helper should emit exactly record + terminal actions"
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn p4b1_failed_and_cancelled_skip_do_not_emit_persistence_actions() {
    use crate::chat::action::Action;

    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let failed = scheduler.enqueue("failed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let cancelled = scheduler.enqueue("cancelled", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(failed).expect("failed task starts");
    scheduler.start_task(cancelled).expect("cancelled task starts");
    scheduler.request_cancel(cancelled).expect("cancel request accepted");
    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    coordinator
        .register_task(scheduler.task(failed).expect("failed task snapshot"))
        .expect("failed task registers");
    coordinator
        .register_task(scheduler.task(cancelled).expect("cancelled task snapshot"))
        .expect("cancelled task registers");
    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    for task_id in [failed, cancelled] {
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers
            .record_execution_started(task_id, 70)
            .expect("worker execution starts");
        workers
            .record_completion_ready(task_id)
            .expect("worker completion ready");
    }
    workers
        .request_cancel(cancelled)
        .expect("worker cancellation requested");
    let mut queue = std::collections::VecDeque::new();
    enqueue_provider_turn_finalizer_event(
        &mut queue,
        Some(failed),
        ProviderTurnTerminalPlan::Failed {
            err: "failed".to_string(),
            history_commit_len: 0,
            summary: "test failed skip".to_string(),
        },
    );
    enqueue_provider_turn_finalizer_event(
        &mut queue,
        Some(cancelled),
        ProviderTurnTerminalPlan::Cancelled {
            summary: "test cancelled skip",
        },
    );
    let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();

    let results = drain_provider_turn_finalizer_events_and_publish(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        &mut queue,
        &dispatcher,
    );

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].terminal_status, "failed");
    assert_eq!(results[1].terminal_status, "cancelled");
    assert!(results.iter().all(|result| result.finalized));
    while let Ok(action) = rx.try_recv() {
        assert!(
            !matches!(
                action,
                Action::RecordAssistantTurn { .. } | Action::StreamCompleted { .. }
            ),
            "skip decisions must not emit assistant/session persistence actions: {action:?}"
        );
    }
}

#[test]
fn provider_turn_visible_admission_blocks_while_provider_worker_active() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let running = scheduler.enqueue("running", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let committed = scheduler.enqueue("committed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(running).expect("running starts");
    scheduler.start_task(committed).expect("committed starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    assert!(
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1,)
            .can_start_visible
    );

    workers
        .start_from_task(
            scheduler.task(running).expect("running task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("running worker starts");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
    assert_eq!(admission.active_workers, 1);
    assert!(!admission.can_start_visible);

    workers
        .record_execution_started(running, 44)
        .expect("running execution starts");
    workers
        .record_completion_ready(running)
        .expect("running completion ready");
    workers
        .record_finalized_payload(
            running,
            crate::chat::turn_worker::ProviderTurnFinalizedPayload {
                history_commit_len: 1,
                final_text_chars: 1,
                recorded_response_chars: 1,
                total_tokens: 1,
                prompt_tokens: 0,
                completion_tokens: 1,
            },
        )
        .expect("running payload finalized");
    workers.record_completed(running).expect("running worker completed");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
    assert_eq!(admission.active_workers, 1);
    assert!(
        !admission.can_start_visible,
        "awaiting commit still owns the visible draft boundary"
    );

    workers
        .start_from_task(
            scheduler.task(committed).expect("committed task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("committed worker starts");
    workers
        .record_execution_started(committed, 45)
        .expect("committed execution starts");
    workers
        .record_completion_ready(committed)
        .expect("committed completion ready");
    workers
        .record_finalized_payload(
            committed,
            crate::chat::turn_worker::ProviderTurnFinalizedPayload {
                history_commit_len: 1,
                final_text_chars: 1,
                recorded_response_chars: 1,
                total_tokens: 1,
                prompt_tokens: 0,
                completion_tokens: 1,
            },
        )
        .expect("committed payload finalized");
    workers.record_completed(committed).expect("committed worker completed");
    let decision = crate::chat::history_commit::HistoryCommitDecision::Commit {
        task_id: running,
        sequence: scheduler.task(running).expect("running task").sequence,
        history_commit_len: 1,
        summary: "running committed".to_string(),
    };
    workers
        .apply_commit_decision(&decision)
        .expect("running commit applies");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
    assert_eq!(admission.active_workers, 1);
    assert!(
        !admission.can_start_visible,
        "second awaiting commit worker still blocks"
    );

    let decision = crate::chat::history_commit::HistoryCommitDecision::Commit {
        task_id: committed,
        sequence: scheduler.task(committed).expect("committed task").sequence,
        history_commit_len: 1,
        summary: "committed committed".to_string(),
    };
    workers
        .apply_commit_decision(&decision)
        .expect("committed commit applies");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 1);
    assert_eq!(admission.active_workers, 0);
    assert!(admission.can_start_visible);
}

#[test]
fn provider_turn_visible_admission_allows_detached_until_configured_limit() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let second = scheduler.enqueue("second", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(first).expect("first starts");
    scheduler.start_task(second).expect("second starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(first).expect("first task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("first worker starts");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
    assert_eq!(admission.detached_active, 1);
    assert!(admission.can_start_visible, "one detached turn leaves one N=2 slot");

    workers
        .start_from_task(
            scheduler.task(second).expect("second task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("second worker starts");
    let admission =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
    assert_eq!(admission.detached_active, 2);
    assert!(
        !admission.can_start_visible,
        "two detached turns exhaust configured N=2 admission"
    );
}

#[test]
fn provider_turn_visible_admission_legacy_foreground_is_exclusive() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let legacy = scheduler.enqueue("legacy", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(legacy).expect("legacy starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(legacy).expect("legacy task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        )
        .expect("legacy worker starts");

    let detached =
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2);
    assert_eq!(detached.foreground_active, 1);
    assert!(!detached.can_start_visible, "legacy active blocks detached turns");

    let foreground = provider_turn_visible_admission(
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
        2,
    );
    assert!(
        !foreground.can_start_visible,
        "legacy active blocks another legacy turn"
    );
}

#[test]
fn visible_input_pop_preserves_queue_while_provider_worker_active() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let active = scheduler.enqueue("active", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(active).expect("active task starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(active).expect("active task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("active worker starts");
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("queued while active"), 7);
    let queued_task = backlog
        .front()
        .and_then(|queued| queued.turn_task_id)
        .expect("queued task id");

    let popped = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        1,
    );

    assert!(
        popped.is_none(),
        "active provider worker must hold visible input admission"
    );
    assert_eq!(backlog.len(), 1, "queued input must remain queued");
    assert_eq!(
        scheduler.task(queued_task).expect("queued task").state,
        crate::chat::turn_scheduler::TurnTaskState::Queued,
        "blocked visible input must not be marked dispatched"
    );
    assert_eq!(scheduler.status().main_queue_status().queued, 1);
}

#[tokio::test]
async fn active_turn_logs_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let msg = input_msg("/logs #99");
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
    .expect("logs command output");

    assert!(output.contains("Logs failed: no session #99"), "{output}");
    assert_eq!(backlog.len(), 1, "read-only logs command must not mutate backlog");
}

#[tokio::test]
async fn active_turn_kill_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let msg = input_msg("/kill #99");
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
    .expect("kill command output");

    assert!(output.contains("Kill failed: no session #99"), "{output}");
    assert_eq!(backlog.len(), 1, "explicit kill command must not mutate backlog");
}

#[test]
fn queued_input_scheduler_prefers_priority_over_older_normal() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    enqueue_input_message(&mut backlog, input_msg("/priority urgent two"));
    enqueue_input_message(&mut backlog, input_msg("normal three"));

    let first = pop_next_queued_input_message(&mut backlog)
        .map(|queued| queued.msg)
        .expect("priority first");
    let second = pop_next_queued_input_message(&mut backlog)
        .map(|queued| queued.msg)
        .expect("normal one second");
    let third = pop_next_queued_input_message(&mut backlog)
        .map(|queued| queued.msg)
        .expect("normal three third");

    assert_eq!(first.content, "urgent two");
    assert_eq!(second.content, "normal one");
    assert_eq!(third.content, "normal three");
}

#[test]
fn cancelled_turn_rolls_back_legacy_history_user_message() {
    let mut history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("previous question"),
        ChatMessage::assistant("previous answer"),
    ];
    let len_before = history.len();
    history.push(ChatMessage::user("cancelled long task"));

    rollback_cancelled_turn_history(&mut history, len_before);

    assert_eq!(history.len(), len_before);
    assert!(
        history.iter().all(|message| message.content != "cancelled long task"),
        "cancelled turn must not leak into the next queued prompt"
    );
}

#[test]
fn p4a_shutdown_cancelled_pending_turn_rolls_back_to_user_boundary() {
    let mut history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("previous question"),
        ChatMessage::assistant("previous answer"),
    ];
    let len_before_user_turn = history.len();
    history.push(ChatMessage::user("pending provider turn"));
    history.push(ChatMessage::assistant("partial assistant text"));

    rollback_cancelled_turn_history(&mut history, len_before_user_turn);

    assert_eq!(history.len(), len_before_user_turn);
    assert!(
        history
            .iter()
            .all(|message| message.content != "pending provider turn" && message.content != "partial assistant text"),
        "shutdown cancellation must remove both the queued user turn and partial assistant output"
    );
}

#[test]
fn queued_input_notice_is_single_line_and_bounded() {
    let notice = format_queued_input_notice("  first line\nsecond\tline  ", InputQueuePriority::Normal);
    assert_eq!(notice, "queued > first line second line");

    let long = format_queued_input_notice(&"x".repeat(220), InputQueuePriority::Normal);
    assert!(long.starts_with("queued > "));
    assert!(long.ends_with('…'));
    assert!(long.chars().count() <= "queued > ".chars().count() + 160);

    let priority = format_queued_input_notice("/now urgent", InputQueuePriority::Priority);
    assert_eq!(priority, "priority queued > urgent");
}

#[test]
fn active_turn_local_notice_strips_priority_prefix() {
    assert!(is_active_turn_local_command("/now /cost"));
    assert!(is_active_turn_local_command("/workers"));
    assert!(is_active_turn_local_command("/workers cancel w#1"));
    assert!(is_active_turn_local_command("/sessions"));
    assert!(is_active_turn_local_command("/now /logs #1"));
    assert!(is_active_turn_local_command("/priority /kill 2"));
    assert!(!is_active_turn_local_command("/bg create child"));
    assert_eq!(format_active_turn_local_notice("/now /cost"), "local > /cost");
    assert_eq!(
        format_active_turn_local_notice("/priority /queue status"),
        "local > /queue status"
    );
    assert_eq!(
        format_active_turn_local_notice("/priority /workers cancel #2"),
        "local > /workers cancel #2"
    );
}

#[test]
fn session_to_session_switch_suppresses_attach_breadcrumb_spam() {
    let meta = session_view(1);
    let projection =
        build_active_session_attach_projection(1, Some(&meta), vec!["tail".into()], vec!["ring".into()], false);

    assert!(
        attach_breadcrumb_for_transition(false, &projection).is_some(),
        "Main->Session entry keeps one breadcrumb"
    );

    let switches = [
        build_active_session_attach_projection(2, Some(&session_view(2)), Vec::new(), Vec::new(), false),
        build_active_session_attach_projection(3, Some(&session_view(3)), Vec::new(), Vec::new(), false),
        build_active_session_attach_projection(1, Some(&session_view(1)), Vec::new(), Vec::new(), false),
    ];
    let emitted = switches
        .iter()
        .filter_map(|projection| attach_breadcrumb_for_transition(true, projection))
        .count();
    assert_eq!(
        emitted, 0,
        "Session->Session directional cycling must not append extra main-history breadcrumbs"
    );
}

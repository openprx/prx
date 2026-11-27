#[test]
fn session_reset_commands_wait_for_active_turn_even_when_concurrency_has_capacity() {
    for command in ["/new", "/clear"] {
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
        enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg(command), 7);
        let queued_task = backlog
            .front()
            .and_then(|queued| queued.turn_task_id)
            .expect("queued command task id");

        let popped = pop_next_visible_input_task_with_scheduler(
            &mut backlog,
            &mut scheduler,
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            2,
        );

        assert!(popped.is_none(), "{command} must wait for the active turn to persist");
        assert_eq!(backlog.len(), 1, "{command} must remain queued");
        assert_eq!(
            scheduler.task(queued_task).expect("queued command task").state,
            crate::chat::turn_scheduler::TurnTaskState::Queued,
            "{command} must not be dispatched across the session boundary"
        );
    }
}

#[test]
fn visible_input_pop_prefers_priority_when_detached_slot_is_available() {
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
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("normal queued first"), 7);
    enqueue_input_message_with_scheduler(
        &mut backlog,
        &mut scheduler,
        input_msg("/priority urgent queued second"),
        7,
    );

    let popped = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        2,
    )
    .expect("second detached slot should be available");

    assert_eq!(
        popped.msg.content, "urgent queued second",
        "priority input must dispatch before older normal input when a visible slot opens"
    );
    let popped_task = popped.turn_task_id.expect("popped task id");
    assert_eq!(
        scheduler.task(popped_task).expect("priority task").priority,
        crate::chat::turn_scheduler::TurnPriority::Priority
    );
    assert_eq!(
        backlog.front().map(|queued| queued.msg.content.as_str()),
        Some("normal queued first"),
        "normal input remains queued after priority dispatch"
    );
}

#[test]
fn post_route_requeue_defers_next_visible_pop_once_under_detached_capacity() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let active = scheduler.enqueue("active detached", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(active).expect("active task starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(active).expect("active task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("active worker starts");
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("legacy fallback"), 7);
    let queued_task = backlog
        .front()
        .and_then(|queued| queued.turn_task_id)
        .expect("queued task id");

    assert!(
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2,)
            .can_start_visible,
        "ordinary detached pop admission would otherwise immediately re-pop"
    );
    let mut defer_visible_input_pop_once = true;

    assert!(
        consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
        "post-route requeue should force one event-pump wait"
    );
    assert_eq!(backlog.len(), 1, "deferred pop leaves requeued input in place");
    assert_eq!(
        scheduler.task(queued_task).expect("queued task").state,
        crate::chat::turn_scheduler::TurnTaskState::Queued,
        "deferred pop must not repeatedly mark the requeued task dispatched"
    );
    assert!(
        !consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
        "defer gate is one-shot"
    );

    let popped = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        2,
    )
    .expect("input can pop after one event-pump wait");
    assert_eq!(popped.turn_task_id, Some(queued_task));
    assert_eq!(
        scheduler.task(queued_task).expect("queued task").state,
        crate::chat::turn_scheduler::TurnTaskState::Dispatched
    );
}

#[test]
fn post_route_admission_rejection_requeues_input_and_defers_next_pop() {
    let mut backlog = std::collections::VecDeque::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let active = scheduler.enqueue("active detached", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(active).expect("active task starts");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(active).expect("active task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("active worker starts");
    enqueue_input_message_with_scheduler(&mut backlog, &mut scheduler, input_msg("legacy after draft failure"), 7);

    let popped = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        2,
    )
    .expect("detached admission allows second visible turn before route is known");
    let task_id = popped.turn_task_id.expect("popped task id");
    assert_eq!(
        scheduler.task(task_id).expect("popped task").state,
        crate::chat::turn_scheduler::TurnTaskState::Dispatched,
        "chat-loop pop marks the task dispatched before post-route admission runs"
    );
    assert!(
        !provider_turn_visible_admission(
            &workers,
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited,
            1
        )
        .can_start_visible,
        "legacy-routed turn is rejected while another detached worker is active"
    );

    let mut defer_visible_input_pop_once = false;
    requeue_post_route_admission_rejected_input(
        &mut backlog,
        &mut scheduler,
        &mut defer_visible_input_pop_once,
        popped,
    );

    assert_eq!(backlog.len(), 1, "post-route rejection must requeue the popped input");
    assert_eq!(
        backlog.front().and_then(|queued| queued.turn_task_id),
        Some(task_id),
        "requeued input keeps the original scheduler task id"
    );
    assert_eq!(
        scheduler.task(task_id).expect("requeued task").state,
        crate::chat::turn_scheduler::TurnTaskState::Queued,
        "requeued input is queued again in the scheduler"
    );
    assert_eq!(
        scheduler.status().queued,
        1,
        "queue status counts the requeued input it lists"
    );
    assert!(
        provider_turn_visible_admission(&workers, crate::chat::turn_worker::ProviderTurnWorkerKind::Detached, 2)
            .can_start_visible,
        "without the defer gate, the next pump pass would immediately re-pop under detached capacity"
    );
    assert!(
        consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
        "post-route rejection must force one event-pump wait before the same input can pop again"
    );
    assert_eq!(
        backlog.len(),
        1,
        "deferred pump pass leaves the requeued input in place"
    );
    assert!(
        !consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once),
        "defer gate is one-shot after the event-pump wait"
    );
    let repopped = pop_next_visible_input_task_with_scheduler(
        &mut backlog,
        &mut scheduler,
        &workers,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        2,
    )
    .expect("the requeued input pops again");
    assert_eq!(repopped.turn_task_id, Some(task_id));
    assert_eq!(
        scheduler.task(task_id).expect("re-dispatched task").state,
        crate::chat::turn_scheduler::TurnTaskState::Dispatched,
        "the second dequeue is a valid queued -> dispatched transition"
    );
}

#[test]
fn provider_turn_completion_context_supplies_stable_history_boundary_once() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("context", crate::chat::turn_scheduler::TurnPriority::Normal, 9);
    let mut contexts = std::collections::HashMap::new();

    record_provider_turn_completion_context(&mut contexts, Some(task_id), 12);

    assert_eq!(
        take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99),
        12
    );
    assert_eq!(
        take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99),
        99,
        "completion context is single-use"
    );
    assert_eq!(
        take_provider_turn_completion_history_len(&mut contexts, None, 7),
        7,
        "legacy turns without task id use the local fallback"
    );
}

#[test]
fn provider_terminal_plan_uses_completion_context_history_boundary() {
    let tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("context plan", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    let mut contexts = std::collections::HashMap::new();
    record_provider_turn_completion_context(&mut contexts, Some(task_id), 4);
    let history_len = take_provider_turn_completion_history_len(&mut contexts, Some(task_id), 99);

    let plan = provider_turn_terminal_plan_from_completion(
        ResolvedProviderTurnCompletion {
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "context done".to_string(),
                reasoning: String::new(),
            }),
            usage: crate::llm::route_decision::TokenUsage::default(),
        },
        history_len,
        &tools,
    );

    match plan {
        ProviderTurnTerminalPlan::Completed { history_commit_len, .. } => assert_eq!(history_commit_len, 5),
        other => panic!("unexpected terminal plan: {other:?}"),
    }
}

#[test]
fn completed_provider_turn_finalization_gate_records_payload_and_commit() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("complete me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
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
    workers.record_execution_started(task_id, 7).expect("execution starts");
    workers.record_completion_ready(task_id).expect("completion ready");
    let usage = crate::llm::route_decision::TokenUsage {
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
        total_tokens: Some(15),
        ..Default::default()
    };

    let decisions = gate_completed_provider_turn_finalization(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        Some(task_id),
        3,
        12,
        10,
        &usage,
        "test completed",
    );

    assert_eq!(decisions.len(), 1, "completion gate should release one ready decision");
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Completed
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert_eq!(row.finalized_total_tokens, Some(15));
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[test]
fn failed_provider_turn_finalization_gate_records_skip_decision() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("fail me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
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
    workers.record_execution_started(task_id, 8).expect("execution starts");
    workers.record_completion_ready(task_id).expect("completion ready");

    let decisions = gate_failed_provider_turn_finalization(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        Some(task_id),
        2,
        "test failed",
    );

    assert_eq!(decisions.len(), 1, "failure gate should release one skip decision");
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Failed
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Failed
    ));
}

#[test]
fn cancelled_provider_turn_finalization_gate_records_skip_decision() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("cancel me", crate::chat::turn_scheduler::TurnPriority::Normal, 2);
    scheduler.start_task(task_id).expect("task starts");
    scheduler.request_cancel(task_id).expect("task cancellation requested");
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
    workers.record_execution_started(task_id, 9).expect("execution starts");
    workers.request_cancel(task_id).expect("worker cancellation requested");
    workers.record_completion_ready(task_id).expect("completion ready");

    let decisions = gate_cancelled_provider_turn_finalization(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        Some(task_id),
        "test cancelled",
    );

    assert_eq!(decisions.len(), 1, "cancellation gate should release one skip decision");
    assert_eq!(
        scheduler.task(task_id).unwrap().state,
        crate::chat::turn_scheduler::TurnTaskState::Cancelled
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
    ));
}

#[test]
fn provider_completion_resolution_prefers_completion_event() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("event", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let signal = dispatcher::TurnCompletionSignal::new();
    signal.register_turn(task_id, "draft-event");
    signal.record_and_notify(dispatcher::TurnOutcomeKind::Cancelled);
    signal.record_usage(crate::llm::route_decision::TokenUsage {
        total_tokens: Some(99),
        ..Default::default()
    });
    let event = ProviderTurnCompletionEvent {
        task_id,
        outcome: Some(dispatcher::TurnOutcomeKind::Completed {
            final_text: "event wins".to_string(),
            reasoning: String::new(),
        }),
        usage: crate::llm::route_decision::TokenUsage {
            total_tokens: Some(12),
            ..Default::default()
        },
    };

    let resolved = resolve_provider_turn_completion(&signal, Some(task_id), Some(event));

    match resolved.outcome {
        Some(dispatcher::TurnOutcomeKind::Completed { final_text, .. }) => {
            assert_eq!(final_text, "event wins");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(resolved.usage.total_tokens, Some(12));
    assert!(signal.notified_for(task_id).is_none(), "task should unregister");
}

#[test]
fn provider_completion_resolution_uses_legacy_usage_when_event_usage_empty() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "event empty usage",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        0,
    );
    let signal = dispatcher::TurnCompletionSignal::new();
    signal.record_usage(crate::llm::route_decision::TokenUsage {
        total_tokens: Some(21),
        ..Default::default()
    });
    let event = ProviderTurnCompletionEvent {
        task_id,
        outcome: Some(dispatcher::TurnOutcomeKind::Cancelled),
        usage: crate::llm::route_decision::TokenUsage::default(),
    };

    let resolved = resolve_provider_turn_completion(&signal, Some(task_id), Some(event));

    assert!(matches!(resolved.outcome, Some(dispatcher::TurnOutcomeKind::Cancelled)));
    assert_eq!(resolved.usage.total_tokens, Some(21));
}

#[test]
fn provider_completion_resolution_uses_keyed_signal_without_event() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("keyed", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let signal = dispatcher::TurnCompletionSignal::new();
    signal.register_turn(task_id, "draft-keyed");
    assert!(signal.record_usage_for_draft(
        "draft-keyed",
        crate::llm::route_decision::TokenUsage {
            total_tokens: Some(34),
            ..Default::default()
        }
    ));
    assert!(signal.record_and_notify_for_draft(
        "draft-keyed",
        dispatcher::TurnOutcomeKind::Failed {
            err: "keyed failed".to_string(),
            retryable: false,
        },
    ));

    let resolved = resolve_provider_turn_completion(&signal, Some(task_id), None);

    match resolved.outcome {
        Some(dispatcher::TurnOutcomeKind::Failed { err, retryable }) => {
            assert_eq!(err, "keyed failed");
            assert!(!retryable);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(resolved.usage.total_tokens, Some(34));
    assert!(signal.notified_for(task_id).is_none(), "task should unregister");
}

#[test]
fn provider_turn_terminal_gate_dispatches_completed_finalization() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("unified gate", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
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
    workers.record_execution_started(task_id, 55).expect("execution starts");
    workers.record_completion_ready(task_id).expect("completion ready");
    let usage = crate::llm::route_decision::TokenUsage {
        total_tokens: Some(55),
        ..Default::default()
    };

    let decisions = gate_provider_turn_terminal_finalization(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        Some(task_id),
        ProviderTurnTerminalGate::Completed {
            history_commit_len: 2,
            final_text_chars: 11,
            recorded_response_chars: 10,
            usage: &usage,
            summary: "test unified completed",
        },
    );

    assert_eq!(
        decisions.len(),
        1,
        "unified terminal gate should finalize completed turn"
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert_eq!(row.finalized_total_tokens, Some(55));
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[test]
fn provider_turn_finalizer_event_commits_completed_plan() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "finalizer completed",
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
    workers.record_execution_started(task_id, 56).expect("execution starts");
    workers.record_completion_ready(task_id).expect("completion ready");

    let result = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(task_id),
            plan: ProviderTurnTerminalPlan::Completed {
                final_text: "done".to_string(),
                reasoning: String::new(),
                recorded_response: "done".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(56),
                    ..Default::default()
                },
                history_commit_len: 2,
                final_text_chars: 4,
                recorded_response_chars: 4,
                summary: "test finalizer completed",
            },
        },
    );

    assert_eq!(
        result,
        vec![ProviderTurnFinalizerResult {
            task_id: Some(task_id),
            terminal_status: "completed",
            finalized: true,
        }]
    );
    let row = workers
        .snapshot()
        .into_iter()
        .find(|row| row.task_id == task_id)
        .expect("worker snapshot retained");
    assert_eq!(row.finalized_total_tokens, Some(56));
    assert!(matches!(
        row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[test]
fn provider_turn_finalizer_event_closes_failed_and_cancelled_plans() {
    let mut failed_scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let failed_id = failed_scheduler.enqueue("finalizer failed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    failed_scheduler.start_task(failed_id).expect("failed task starts");
    let mut failed_coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    failed_coordinator
        .register_task(failed_scheduler.task(failed_id).expect("failed task snapshot"))
        .expect("failed history task registers");
    let mut failed_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    failed_workers
        .start_from_task(
            failed_scheduler.task(failed_id).expect("failed task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("failed worker starts");
    failed_workers
        .record_execution_started(failed_id, 57)
        .expect("failed execution starts");
    failed_workers
        .record_completion_ready(failed_id)
        .expect("failed completion ready");

    let failed_result = finalize_provider_turn_from_event(
        &mut failed_scheduler,
        &mut failed_coordinator,
        &mut failed_workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(failed_id),
            plan: ProviderTurnTerminalPlan::Failed {
                err: "failed".to_string(),
                history_commit_len: 1,
                summary: "test finalizer failed".to_string(),
            },
        },
    );

    assert_eq!(failed_result.len(), 1);
    assert_eq!(failed_result[0].terminal_status, "failed");
    assert!(failed_result[0].finalized);
    assert!(matches!(
        failed_workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == failed_id)
            .expect("failed worker snapshot")
            .state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Failed
    ));

    let mut cancelled_scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let cancelled_id = cancelled_scheduler.enqueue(
        "finalizer cancelled",
        crate::chat::turn_scheduler::TurnPriority::Normal,
        1,
    );
    cancelled_scheduler
        .start_task(cancelled_id)
        .expect("cancelled task starts");
    cancelled_scheduler
        .request_cancel(cancelled_id)
        .expect("cancelled task requested");
    let mut cancelled_coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    cancelled_coordinator
        .register_task(cancelled_scheduler.task(cancelled_id).expect("cancelled task snapshot"))
        .expect("cancelled history task registers");
    let mut cancelled_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    cancelled_workers
        .start_from_task(
            cancelled_scheduler.task(cancelled_id).expect("cancelled task snapshot"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("cancelled worker starts");
    cancelled_workers
        .record_execution_started(cancelled_id, 58)
        .expect("cancelled execution starts");
    cancelled_workers
        .request_cancel(cancelled_id)
        .expect("cancelled worker requested");
    cancelled_workers
        .record_completion_ready(cancelled_id)
        .expect("cancelled completion ready");

    let cancelled_result = finalize_provider_turn_from_event(
        &mut cancelled_scheduler,
        &mut cancelled_coordinator,
        &mut cancelled_workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(cancelled_id),
            plan: ProviderTurnTerminalPlan::Cancelled {
                summary: "test finalizer cancelled",
            },
        },
    );

    assert_eq!(cancelled_result.len(), 1);
    assert_eq!(cancelled_result[0].terminal_status, "cancelled");
    assert!(cancelled_result[0].finalized);
    assert!(matches!(
        cancelled_workers
            .snapshot()
            .into_iter()
            .find(|row| row.task_id == cancelled_id)
            .expect("cancelled worker snapshot")
            .state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
    ));
}

#[test]
fn provider_turn_finalizer_queue_drains_fifo_and_unlocks_commit_order() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first failed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    let second = scheduler.enqueue("second completed", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(first).expect("first starts");
    scheduler.start_task(second).expect("second starts");

    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    coordinator
        .register_task(scheduler.task(first).expect("first task"))
        .expect("first history task registers");
    coordinator
        .register_task(scheduler.task(second).expect("second task"))
        .expect("second history task registers");

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    for (id, lease) in [(first, 61), (second, 62)] {
        workers
            .start_from_task(
                scheduler.task(id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(id, lease).expect("execution starts");
        workers.record_completion_ready(id).expect("completion ready");
    }

    let mut queue = std::collections::VecDeque::new();
    enqueue_provider_turn_finalizer_event(
        &mut queue,
        Some(first),
        ProviderTurnTerminalPlan::Failed {
            err: "first failed".to_string(),
            history_commit_len: 1,
            summary: "test first failed".to_string(),
        },
    );
    enqueue_provider_turn_finalizer_event(
        &mut queue,
        Some(second),
        ProviderTurnTerminalPlan::Completed {
            final_text: "second done".to_string(),
            reasoning: String::new(),
            recorded_response: "second done".to_string(),
            empty_response: false,
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(62),
                ..Default::default()
            },
            history_commit_len: 2,
            final_text_chars: 11,
            recorded_response_chars: 11,
            summary: "test second completed",
        },
    );

    let results = drain_provider_turn_finalizer_events(&mut scheduler, &mut coordinator, &mut workers, &mut queue);

    assert!(queue.is_empty());
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].task_id, Some(first));
    assert_eq!(results[0].terminal_status, "failed");
    assert!(results[0].finalized);
    assert_eq!(results[1].task_id, Some(second));
    assert_eq!(results[1].terminal_status, "completed");
    assert!(results[1].finalized);
    let rows = workers.snapshot();
    assert!(matches!(
        rows.iter().find(|row| row.task_id == first).expect("first row").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Failed
    ));
    let second_row = rows.iter().find(|row| row.task_id == second).expect("second row");
    assert_eq!(second_row.finalized_total_tokens, Some(62));
    assert!(matches!(
        second_row.state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[test]
fn provider_turn_finalizer_delays_later_completed_turn_until_earlier_ready() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first slow", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    let second = scheduler.enqueue("second fast", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(first).expect("first starts");
    scheduler.start_task(second).expect("second starts");
    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    coordinator
        .register_task(scheduler.task(first).expect("first task"))
        .expect("first history task registers");
    coordinator
        .register_task(scheduler.task(second).expect("second task"))
        .expect("second history task registers");
    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    for (id, lease) in [(first, 81), (second, 82)] {
        workers
            .start_from_task(
                scheduler.task(id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers.record_execution_started(id, lease).expect("execution starts");
        workers.record_completion_ready(id).expect("completion ready");
    }

    let second_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(second),
            plan: ProviderTurnTerminalPlan::Completed {
                final_text: "second answer".to_string(),
                reasoning: String::new(),
                recorded_response: "second answer".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(82),
                    ..Default::default()
                },
                history_commit_len: 3,
                final_text_chars: 13,
                recorded_response_chars: 13,
                summary: "test second completed",
            },
        },
    );
    assert_eq!(
        second_results,
        vec![ProviderTurnFinalizerResult {
            task_id: Some(second),
            terminal_status: "unknown",
            finalized: false,
        }],
        "later completion must wait for the earlier sequence"
    );
    assert!(matches!(
        workers.worker(second).expect("second worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
    ));

    let first_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(first),
            plan: ProviderTurnTerminalPlan::Completed {
                final_text: "first answer".to_string(),
                reasoning: String::new(),
                recorded_response: "first answer".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(81),
                    ..Default::default()
                },
                history_commit_len: 3,
                final_text_chars: 12,
                recorded_response_chars: 12,
                summary: "test first completed",
            },
        },
    );

    assert_eq!(
        first_results.iter().map(|result| result.task_id).collect::<Vec<_>>(),
        vec![Some(first), Some(second)],
        "earlier completion should release both ready decisions in sequence"
    );
    assert!(first_results.iter().all(|result| result.finalized));
    assert!(matches!(
        workers.worker(first).expect("first worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
    assert!(matches!(
        workers.worker(second).expect("second worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[test]
fn provider_turn_finalizer_releases_later_completed_turn_when_earlier_cancelled() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("first cancelled", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    let second = scheduler.enqueue("second held", crate::chat::turn_scheduler::TurnPriority::Normal, 1);
    scheduler.start_task(first).expect("first starts");
    scheduler.request_cancel(first).expect("first cancellation requested");
    scheduler.start_task(second).expect("second starts");
    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    coordinator
        .register_task(scheduler.task(first).expect("first task"))
        .expect("first history task registers");
    coordinator
        .register_task(scheduler.task(second).expect("second task"))
        .expect("second history task registers");
    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    workers
        .start_from_task(
            scheduler.task(first).expect("first task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("first worker starts");
    workers
        .record_execution_started(first, 91)
        .expect("first execution starts");
    workers
        .request_cancel(first)
        .expect("first worker cancellation requested");
    workers.record_completion_ready(first).expect("first completion ready");
    workers
        .start_from_task(
            scheduler.task(second).expect("second task"),
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
        )
        .expect("second worker starts");
    workers
        .record_execution_started(second, 92)
        .expect("second execution starts");
    workers
        .record_completion_ready(second)
        .expect("second completion ready");

    let second_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(second),
            plan: ProviderTurnTerminalPlan::Completed {
                final_text: "second after cancel".to_string(),
                reasoning: String::new(),
                recorded_response: "second after cancel".to_string(),
                empty_response: false,
                usage: crate::llm::route_decision::TokenUsage {
                    total_tokens: Some(92),
                    ..Default::default()
                },
                history_commit_len: 3,
                final_text_chars: 19,
                recorded_response_chars: 19,
                summary: "test second completed after cancel",
            },
        },
    );
    assert_eq!(
        second_results,
        vec![ProviderTurnFinalizerResult {
            task_id: Some(second),
            terminal_status: "unknown",
            finalized: false,
        }],
        "later completion must remain held until earlier cancellation is ordered"
    );

    let first_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(first),
            plan: ProviderTurnTerminalPlan::Cancelled {
                summary: "test first cancelled",
            },
        },
    );

    assert_eq!(
        first_results
            .iter()
            .map(|result| (result.task_id, result.terminal_status))
            .collect::<Vec<_>>(),
        vec![(Some(first), "cancelled"), (Some(second), "completed")],
        "earlier cancellation should skip first and then release held later commit"
    );
    assert!(first_results.iter().all(|result| result.finalized));
    assert!(matches!(
        workers.worker(first).expect("first worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
    ));
    assert!(matches!(
        workers.worker(second).expect("second worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed
    ));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn provider_turn_finalizer_n3_out_of_order_releases_ordered_persistence_actions() {
    use crate::chat::action::Action;

    fn completed_plan(final_text: &str, total_tokens: u32, history_commit_len: usize) -> ProviderTurnTerminalPlan {
        ProviderTurnTerminalPlan::Completed {
            final_text: final_text.to_string(),
            reasoning: String::new(),
            recorded_response: final_text.to_string(),
            empty_response: false,
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(total_tokens),
                ..Default::default()
            },
            history_commit_len,
            final_text_chars: final_text.chars().count(),
            recorded_response_chars: final_text.chars().count(),
            summary: "test n3 completed",
        }
    }

    fn pending_commit_for(
        task_id: crate::chat::turn_scheduler::TurnTaskId,
        draft_id: &str,
        user_input: &str,
        plan: ProviderTurnTerminalPlan,
    ) -> PendingOrderedProviderTurnCommit {
        PendingOrderedProviderTurnCommit {
            context: PerTurnContext {
                _mailbox_input: None,
                turn_binding: None,
                task_id,
                draft_id: draft_id.to_string(),
                delta_tx: None,
                tool_event_tx: None,
                draft_updater: None,
                tool_event_forwarder: None,
                user_input: user_input.to_string(),
                user_event_id: None,
                sent: None,
                turn_run_id: format!("test-run-{}", task_id.get()),
                route_scope: crate::memory::MessageEventScope::new("chat", crate::memory::MemoryVisibility::Workspace),
                route_decision: RouteDecision::single_candidate("mock", "mock"),
                provider_started_at: chrono::Utc::now(),
                provider_name: "mock".to_string(),
                model_name: "mock".to_string(),
                history_len_before_user_turn: 0,
                compaction_epoch_at_start: 0,
                assignment: None,
            },
            terminal_plan: plan,
        }
    }

    fn dispatch_ready_for_test(
        results: Vec<ProviderTurnFinalizerResult>,
        pending: &mut std::collections::HashMap<
            crate::chat::turn_scheduler::TurnTaskId,
            PendingOrderedProviderTurnCommit,
        >,
        dispatcher: &dispatcher::ChatDispatcher,
    ) {
        for result in results {
            if !result.finalized || result.terminal_status != "completed" {
                continue;
            }
            let task_id = result.task_id.expect("finalized result has task id");
            let pending_commit = pending.remove(&task_id).expect("pending commit payload");
            let ProviderTurnTerminalPlan::Completed {
                final_text,
                reasoning,
                empty_response,
                ..
            } = pending_commit.terminal_plan
            else {
                panic!("expected completed terminal plan");
            };
            dispatch_ordered_provider_turn_commit(
                dispatcher,
                task_id,
                &pending_commit.context.draft_id,
                &pending_commit.context.user_input,
                &final_text,
                &reasoning,
                empty_response,
                true,
                crate::chat::provider_turns::TurnEventIds::default(),
            )
            .expect("ordered history dispatch");
        }
    }

    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let first = scheduler.enqueue("alpha prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let second = scheduler.enqueue("bravo prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let third = scheduler.enqueue("charlie prompt", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    for task_id in [first, second, third] {
        scheduler.start_task(task_id).expect("task starts");
    }

    let mut coordinator = crate::chat::history_commit::HistoryCommitCoordinator::new();
    for task_id in [first, second, third] {
        coordinator
            .register_task(scheduler.task(task_id).expect("task snapshot"))
            .expect("history task registers");
    }

    let mut workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
    for (task_id, lease) in [(first, 101), (second, 102), (third, 103)] {
        workers
            .start_from_task(
                scheduler.task(task_id).expect("task snapshot"),
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached,
            )
            .expect("worker starts");
        workers
            .record_execution_started(task_id, lease)
            .expect("execution starts");
        workers.record_completion_ready(task_id).expect("completion ready");
    }

    let first_plan = completed_plan("alpha answer", 11, 2);
    let second_plan = completed_plan("bravo answer", 12, 4);
    let third_plan = completed_plan("charlie answer", 13, 6);
    let mut pending_ordered_provider_turn_commits = std::collections::HashMap::from([
        (
            first,
            pending_commit_for(first, "draft-alpha", "alpha prompt", first_plan.clone()),
        ),
        (
            second,
            pending_commit_for(second, "draft-bravo", "bravo prompt", second_plan.clone()),
        ),
        (
            third,
            pending_commit_for(third, "draft-charlie", "charlie prompt", third_plan.clone()),
        ),
    ]);
    let (dispatcher, mut rx) = dispatcher::ChatDispatcher::new();

    let third_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(third),
            plan: third_plan,
        },
    );
    assert_eq!(
        third_results,
        vec![ProviderTurnFinalizerResult {
            task_id: Some(third),
            terminal_status: "unknown",
            finalized: false,
        }],
        "third completion must stay held until first and second are terminal"
    );
    dispatch_ready_for_test(third_results, &mut pending_ordered_provider_turn_commits, &dispatcher);
    assert!(
        rx.try_recv().is_err(),
        "held third turn must emit no persistence actions"
    );
    assert!(matches!(
        workers.worker(third).expect("third worker").state,
        crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
    ));

    let second_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(second),
            plan: second_plan,
        },
    );
    assert_eq!(
        second_results,
        vec![ProviderTurnFinalizerResult {
            task_id: Some(second),
            terminal_status: "unknown",
            finalized: false,
        }],
        "second completion must stay held until first is terminal"
    );
    dispatch_ready_for_test(second_results, &mut pending_ordered_provider_turn_commits, &dispatcher);
    assert!(
        rx.try_recv().is_err(),
        "held second turn must emit no persistence actions"
    );

    let first_results = finalize_provider_turn_from_event(
        &mut scheduler,
        &mut coordinator,
        &mut workers,
        ProviderTurnFinalizerEvent {
            task_id: Some(first),
            plan: first_plan,
        },
    );
    assert_eq!(
        first_results
            .iter()
            .map(|result| (result.task_id, result.terminal_status, result.finalized))
            .collect::<Vec<_>>(),
        vec![
            (Some(first), "completed", true),
            (Some(second), "completed", true),
            (Some(third), "completed", true),
        ],
        "first completion must release all held completed turns in dispatch order"
    );
    dispatch_ready_for_test(first_results, &mut pending_ordered_provider_turn_commits, &dispatcher);

    for (expected_user, expected_assistant, expected_draft, expected_task) in [
        ("alpha prompt", "alpha answer", "draft-alpha", first),
        ("bravo prompt", "bravo answer", "draft-bravo", second),
        ("charlie prompt", "charlie answer", "draft-charlie", third),
    ] {
        match rx.try_recv().expect("ordered user action") {
            Action::RecordUserTurn(content, _) => assert_eq!(content, expected_user),
            other => panic!("expected RecordUserTurn, got {other:?}"),
        }
        match rx.try_recv().expect("ordered assistant action") {
            Action::RecordAssistantTurn { task_id, content, .. } => {
                assert_eq!(task_id, Some(expected_task));
                assert_eq!(content, expected_assistant);
            }
            other => panic!("expected RecordAssistantTurn, got {other:?}"),
        }
        match rx.try_recv().expect("ordered stream completed action") {
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, expected_draft);
                assert_eq!(final_text, expected_assistant);
                assert!(reasoning.is_empty(), "expected an empty value");
            }
            other => panic!("expected StreamCompleted, got {other:?}"),
        }
    }
    assert!(
        rx.try_recv().is_err(),
        "ordered dispatch should emit exactly three action triplets"
    );
    assert!(pending_ordered_provider_turn_commits.is_empty());
    assert_eq!(coordinator.pending_tasks(), 0);
    assert_eq!(coordinator.pending_outcomes(), 0);
    for task_id in [first, second, third] {
        assert!(matches!(
            workers.worker(task_id).expect("worker").state,
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
        ));
    }
}

#[tokio::test]
async fn provider_turn_finalizer_drain_helper_publishes_queue_and_worker_status() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue(
        "publish finalizer",
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
    workers.record_execution_started(task_id, 63).expect("execution starts");
    workers.record_completion_ready(task_id).expect("completion ready");
    let mut queue = std::collections::VecDeque::new();
    enqueue_provider_turn_finalizer_event(
        &mut queue,
        Some(task_id),
        ProviderTurnTerminalPlan::Completed {
            final_text: "published".to_string(),
            reasoning: String::new(),
            recorded_response: "published".to_string(),
            empty_response: false,
            usage: crate::llm::route_decision::TokenUsage {
                total_tokens: Some(63),
                ..Default::default()
            },
            history_commit_len: 2,
            final_text_chars: 9,
            recorded_response_chars: 9,
            summary: "test finalizer published",
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

    assert_eq!(results.len(), 1);
    assert!(results[0].finalized);
    assert!(queue.is_empty());
    let first = rx.recv().await.expect("main queue status action");
    let second = rx.recv().await.expect("provider worker status action");
    assert!(matches!(
        first,
        crate::chat::action::Action::MainQueueStatusUpdated { .. }
    ));
    match second {
        crate::chat::action::Action::ProviderWorkerStatusUpdated { status } => {
            assert_eq!(status.finalized_payloads, 1);
            assert_eq!(status.finalized_total_tokens, 63);
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn provider_terminal_plan_completed_non_empty_builds_gate_fields() {
    let tools: Vec<Box<dyn Tool>> = Vec::new();
    let resolved = ResolvedProviderTurnCompletion {
        outcome: Some(dispatcher::TurnOutcomeKind::Completed {
            final_text: "P7J terminal plan".to_string(),
            reasoning: "kept reasoning".to_string(),
        }),
        usage: crate::llm::route_decision::TokenUsage {
            total_tokens: Some(77),
            ..Default::default()
        },
    };

    let plan = provider_turn_terminal_plan_from_completion(resolved, 4, &tools);

    match plan {
        ProviderTurnTerminalPlan::Completed {
            final_text,
            recorded_response,
            reasoning,
            empty_response,
            usage,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            summary,
        } => {
            assert_eq!(final_text, "P7J terminal plan");
            assert_eq!(recorded_response, "P7J terminal plan");
            assert_eq!(reasoning, "kept reasoning");
            assert!(!empty_response);
            assert_eq!(usage.total_tokens, Some(77));
            assert_eq!(history_commit_len, 5);
            assert_eq!(final_text_chars, "P7J terminal plan".chars().count());
            assert_eq!(recorded_response_chars, "P7J terminal plan".chars().count());
            assert_eq!(summary, "redux driver completed");
        }
        other => panic!("unexpected terminal plan: {other:?}"),
    }
}

#[test]
fn provider_terminal_plan_empty_failed_and_cancelled_keep_boundaries() {
    let tools: Vec<Box<dyn Tool>> = Vec::new();
    let empty = provider_turn_terminal_plan_from_completion(
        ResolvedProviderTurnCompletion {
            outcome: Some(dispatcher::TurnOutcomeKind::Completed {
                final_text: "   ".to_string(),
                reasoning: "hidden reasoning".to_string(),
            }),
            usage: crate::llm::route_decision::TokenUsage::default(),
        },
        6,
        &tools,
    );
    match empty {
        ProviderTurnTerminalPlan::Completed {
            empty_response,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            summary,
            ..
        } => {
            assert!(empty_response);
            assert_eq!(history_commit_len, 6);
            assert_eq!(final_text_chars, 0);
            assert_eq!(recorded_response_chars, 0);
            assert_eq!(summary, "redux driver completed with empty response");
        }
        other => panic!("unexpected empty terminal plan: {other:?}"),
    }

    let failed = provider_turn_terminal_plan_from_completion(
        ResolvedProviderTurnCompletion {
            outcome: Some(dispatcher::TurnOutcomeKind::Failed {
                err: "provider failed".to_string(),
                retryable: false,
            }),
            usage: crate::llm::route_decision::TokenUsage::default(),
        },
        7,
        &tools,
    );
    match failed {
        ProviderTurnTerminalPlan::Failed {
            err,
            history_commit_len,
            summary,
        } => {
            assert_eq!(err, "provider failed");
            assert_eq!(history_commit_len, 7);
            assert_eq!(summary, "redux driver failed: provider failed");
        }
        other => panic!("unexpected failed terminal plan: {other:?}"),
    }

    let cancelled = provider_turn_terminal_plan_from_completion(
        ResolvedProviderTurnCompletion {
            outcome: None,
            usage: crate::llm::route_decision::TokenUsage::default(),
        },
        8,
        &tools,
    );
    match cancelled {
        ProviderTurnTerminalPlan::Cancelled { summary } => {
            assert_eq!(summary, "redux driver cancelled");
        }
        other => panic!("unexpected cancelled terminal plan: {other:?}"),
    }
}

#[tokio::test]
async fn active_turn_sessions_command_is_handled_without_enqueue() {
    let mut backlog = std::collections::VecDeque::new();
    enqueue_input_message(&mut backlog, input_msg("normal one"));
    let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let msg = input_msg("/sessions");
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
    .expect("sessions command output");

    assert_eq!(
        output,
        format!("current chat session: {}\nNo child TUI sessions.", chat_session.id)
    );
    assert_eq!(backlog.len(), 1, "read-only sessions command must not mutate backlog");
}

/// A `--daemon` command exists to look at, or end, work in another
/// process — and the moment an operator reaches for one is precisely while
/// a long turn is running here. Queueing them behind that turn made the
/// feature unavailable exactly when it was wanted, so both entry points
/// have to answer: the whitelist that decides what stays answerable, and
/// the dispatch that actually runs it.
#[tokio::test]
async fn active_turn_daemon_commands_run_instead_of_queueing() {
    let mut daemon_config = Config::default();
    // A closed port: the request fails on its own, promptly, in its task.
    daemon_config.chat.daemon.url = "http://127.0.0.1:1".to_string();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let daemon_control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };

    for (command, expected) in [
        ("/sessions --daemon", "Listing daemon work at http://127.0.0.1:1"),
        (
            "/kill --daemon w42",
            "Asking the daemon at http://127.0.0.1:1 to kill w42",
        ),
        (
            "/steer --daemon w42 change course",
            "Sending a message to daemon task w42 at http://127.0.0.1:1",
        ),
    ] {
        assert!(
            is_active_turn_local_command(command),
            "{command} must stay answerable while a turn is in flight"
        );

        let mut backlog = std::collections::VecDeque::new();
        enqueue_input_message(&mut backlog, input_msg("normal one"));
        let scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let chat_session = session::ChatSession::new("p", "m");
        let provider_turn_workers = crate::chat::turn_worker::ProviderTurnWorkerRegistry::new();
        let mut chat_sessions =
            crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
        let session_rings = std::collections::HashMap::new();
        let mut reaped_log_archive = ReapedSessionLogArchive::default();
        let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

        let output = active_turn_local_command_output(
            &input_msg(command),
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
        .unwrap_or_else(|| panic!("{command} must be answered during a turn, not queued"));

        assert!(output.contains(expected), "{command}: {output}");
        assert_eq!(backlog.len(), 1, "{command} must not be enqueued behind the turn");
    }
}

/// The idle loop runs the same three commands through
/// `start_daemon_session_command`, so the two entry points cannot drift
/// apart again: this is the whole of what the loop's `Daemon*` arm does.
#[tokio::test]
async fn the_idle_loop_entry_point_starts_the_same_three_commands() {
    let mut daemon_config = Config::default();
    daemon_config.chat.daemon.url = "http://127.0.0.1:1".to_string();
    let (daemon_dispatcher, _daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };
    let in_flight = crate::chat::sessions::daemon::new_in_flight_registry();

    for (action, expected) in [
        (
            crate::chat::sessions::SessionCommand::DaemonSessions,
            "Listing daemon work at http://127.0.0.1:1",
        ),
        (
            crate::chat::sessions::SessionCommand::DaemonKill {
                address: "w42".to_string(),
            },
            "Asking the daemon at http://127.0.0.1:1 to kill w42",
        ),
        (
            crate::chat::sessions::SessionCommand::DaemonSteer {
                address: "w42".to_string(),
                message: "change course".to_string(),
            },
            "Sending a message to daemon task w42 at http://127.0.0.1:1",
        ),
    ] {
        let notice = start_daemon_session_command(&action, None, &in_flight, control)
            .unwrap_or_else(|| panic!("{action:?} must start a daemon request"));
        assert!(notice.contains(expected), "{action:?}: {notice}");
    }

    // A local `/sessions` is not daemon-scoped and must fall through to the
    // local handler rather than being swallowed here.
    assert!(
        start_daemon_session_command(
            &crate::chat::sessions::SessionCommand::Sessions,
            None,
            &in_flight,
            control
        )
        .is_none()
    );
}

/// §7-2's own argument, applied to chat's own cross-process call: in a
/// runtime with no wall clock, the fallback for something stuck is being
/// seen and being killed. A daemon that accepts the connection and then
/// answers nothing used to leave a spawned task that was neither.
#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn a_stuck_daemon_request_is_listed_and_can_be_ended() {
    // A daemon that accepts the connection and then never answers. Nothing
    // here expires: the request stays outstanding until it is ended.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind ephemeral port");
    let addr = listener.local_addr().expect("test: local addr");
    let silent_daemon = tokio::spawn(async move {
        let held = listener.accept().await;
        std::future::pending::<()>().await;
        drop(held);
    });

    let mut daemon_config = Config::default();
    daemon_config.chat.daemon.url = format!("http://{addr}");
    let (daemon_dispatcher, mut daemon_action_rx) = dispatcher::ChatDispatcher::new();
    let control = DaemonControl {
        config: &daemon_config,
        dispatcher: &daemon_dispatcher,
        redraw_tx: None,
    };
    let mut chat_sessions =
        crate::chat::sessions::ChatSessionsHandle::new(std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())));
    let in_flight = chat_sessions.daemon_requests();
    let session_rings = std::collections::HashMap::new();
    let mut reaped_log_archive = ReapedSessionLogArchive::default();
    let reap_policy = crate::chat::sessions::runtime::ReapPolicy::default();
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();

    let notice = start_daemon_session_command(
        &crate::chat::sessions::SessionCommand::DaemonSessions,
        None,
        &in_flight,
        control,
    )
    .expect("test: the listing request starts");
    assert!(notice.contains("Listing daemon work at"), "{notice}");

    // Visible: the hung request shows up in this chat's own `/sessions`.
    let listing = handle_local_session_command(
        &crate::chat::sessions::SessionCommand::Sessions,
        &mut chat_sessions,
        &session_rings,
        &mut reaped_log_archive,
        &reap_policy,
        &tools_registry,
    )
    .await
    .expect("test: /sessions renders");
    assert!(listing.contains("Daemon requests in flight (1)"), "{listing}");
    assert!(listing.contains("d1 list"), "{listing}");

    // Killable: ending it by address stops the task, which then reports
    // back instead of hanging on a daemon that will never answer.
    let killed = start_daemon_session_command(
        &crate::chat::sessions::SessionCommand::DaemonKill {
            address: "d1".to_string(),
        },
        None,
        &in_flight,
        control,
    )
    .expect("test: d1 is this chat's own address space");
    assert!(killed.contains("Stopped waiting on daemon request d1"), "{killed}");

    let reported = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match daemon_action_rx.recv().await {
                Some(crate::chat::action::Action::SystemMessageAdded { text }) => return text,
                Some(_) => (),
                None => return String::new(),
            }
        }
    })
    .await
    .expect("test: an ended request must report back rather than leak");
    assert!(
        reported.contains("was ended from this chat before the daemon answered"),
        "{reported}"
    );

    silent_daemon.abort();
}

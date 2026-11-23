loop {
        if let Some(inbox) = message_inbox.as_mut().filter(|inbox|inbox.fenced()) {
            input_backlog.retain(|queued| {
                if super::message_inbox::cause(&queued.msg).is_some() {
                    if let Some(task) = queued.turn_task_id {let _ = turn_scheduler.request_cancel(task);}
                    false
                } else {true}
            });
            inbox.drain();
        }

        if message_inbox.as_ref().is_some_and(|inbox|inbox.session()!=chat_session_key) {
            drop(message_inbox.take());
            message_inbox=super::message_inbox::ChatInbox::open(&config,&chat_session_key)?;
        }
        #[cfg(feature="terminal-tui")]
        let message_idle = per_turn_contexts.is_empty();
        #[cfg(not(feature="terminal-tui"))]
        let message_idle = true;
        if let Some(inbox)=message_inbox.as_mut().filter(|_| message_idle) {
            match inbox.poll() {
                Ok(inputs)=>for input in inputs {
                    if input.auto_start() {
                        let message=super::message_inbox::channel_input(&input)?;
                        enqueue_input_message_with_scheduler(&mut input_backlog,&mut turn_scheduler,message,chat_session.session().turns.len());
                    } else {
                        history.push(crate::agent::turn_contracts::inbound_user_message(&input.text));
                        surface_session_message(&chat_dispatcher,sessions_redraw_handle.as_ref(),&input.text);
                    }
                },
                Err(error)=>surface_session_message(&chat_dispatcher,sessions_redraw_handle.as_ref(),&error.to_string()),
            }
        }

        #[cfg(feature="terminal-tui")]
        if let (Some(target),Some(seq))=(attached_follow.as_ref(),attached_follow_seq) {
            let lines=session_rings.get(target).map_or_else(Vec::new,|ring|ring.recent_lines(crate::chat::sessions::event::DEFAULT_RING_CAPACITY));
            let offset=chat_mirror.lock().active_session_view.as_ref().map_or(0,|view|view.scroll_offset);
            if let Ok(view)=(super::child_focus::ChildFocusContext { memory:&mem,generation:&ctx.config_generation,parent:&chat_session_key,
                security:&ctx.security,service:&turn_service,tools:&tools_registry,sessions:&chat_sessions,approvals:&pending_approvals }).view(target.as_str(),seq,lines,offset).await {
                chat_mirror.lock().active_session_view=Some(view.clone());
                let _=chat_dispatcher.dispatch_or_log(crate::chat::action::Action::ActiveSessionViewUpdated{view:Some(view)},"chat.child_session_view");
            }
        }
        if let Some(seq)=attached_follow_seq {
            if let Ok(run)=chat_sessions.resolve_run_id(seq).await { attached_follow=Some(crate::chat::sessions::id::SessionId::from_run_id(&run)); }
        }
        for notice in child_focus.pump(&super::child_focus::ChildFocusContext { memory:&mem,generation:&ctx.config_generation,parent:&chat_session_key,
                security:&ctx.security,service:&turn_service,tools:&tools_registry,sessions:&chat_sessions,approvals:&pending_approvals }).await {
            surface_session_message(&chat_dispatcher,sessions_redraw_handle.as_ref(),&notice);
        }
        turn_signal.ensure_history_commits_healthy()?;
        #[cfg(feature = "terminal-tui")]
        macro_rules! handle_resume_saved_session_control {
            ($id:expr) => {{
                let provider_name = current_provider_owned.as_str();
                let model_name = current_model_owned.as_str();
                match resume_saved_session_by_id(
                    mem.as_ref(),
                    &$id,
                    ChatSwitchCtx {
                        chat_session: &mut chat_session,
                        commits: &turn_signal,
                        chat_session_key: &mut chat_session_key,
                        fabric_turn_seq: &mut fabric_turn_seq,
                        history: &mut history,
                        approval_router: approval_router.as_ref(),
                        pending_chat_rewind: &mut pending_chat_rewind,
                        pending_diff_apply: &mut pending_diff_apply,
                        chat_sessions: &mut chat_sessions,
                        ignored_session_events: &mut ignored_session_events,
                        session_rings: &mut session_rings,
                        reported_sessions: &mut reported_sessions,
                        announced_started_sessions: &mut announced_started_sessions,
                        last_sessions_summary: &mut last_sessions_summary,
                        last_sessions_entries: &mut last_sessions_entries,
                        attached_follow: &mut attached_follow,
                        attached_follow_seq: &mut attached_follow_seq,
                        chat_dispatcher: &chat_dispatcher,
                        redraw_handle: sessions_redraw_handle.as_ref(),
                        config: &config,
                        provider_name,
                        model_name,
                        skills: &skills,
                        native_tools,
                        #[cfg(feature = "terminal-tui")]
                        chat_mirror: &chat_mirror,
                    },
                )
                .await
                {
                    Ok(message) => surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &message),
                    Err(e) => {
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &e.to_string())
                    }
                }
            }};
        }
        #[cfg(feature = "terminal-tui")]
        if finalize_ready_per_turn_contexts(
            &mut per_turn_contexts,
            &mut pending_ordered_provider_turn_commits,
            &mut pending_provider_completion_events,
            &turn_signal,
            &mut provider_turn_completion_contexts,
            &mut history,
            &tools_registry,
            &mut provider_turn_finalizer_events,
            &mut turn_scheduler,
            &mut history_commit_coordinator,
            &mut provider_turn_workers,
            &chat_dispatcher,
            &terminal,
            &memory_fabric,
            &chat_session_key,
            &config,
            sessions_redraw_handle.as_ref(),
            redraw_tx_for_main.as_ref(),
            plain_mode,
            &mut plain_mode_turn_failed,
        )
        .await
        {
            continue;
        }
        if !input_events_open && input_backlog.is_empty() {
            #[cfg(feature = "terminal-tui")]
            if should_continue_event_pump_after_input_closed(per_turn_contexts.len()) {
                // Keep lifecycle/completion arms alive until the detached Redux turn
                // resolves; this mirrors the old inner wait, which ignored closed
                // input while the active turn was still running.
            } else {
                break None;
            }
            #[cfg(not(feature = "terminal-tui"))]
            break None;
        }
        #[cfg(feature = "terminal-tui")]
        if !provider_turn_finalizer_events.is_empty() {
            let results = drain_provider_turn_finalizer_events_and_publish(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                &mut provider_turn_finalizer_events,
                &chat_dispatcher,
            );
            #[cfg(feature = "terminal-tui")]
            let applied_ordered_commits = apply_ready_ordered_provider_turn_commits(
                results,
                &mut pending_ordered_provider_turn_commits,
                &mut history,
                &mut turn_scheduler,
                &provider_turn_workers,
                &chat_dispatcher,
                &terminal,
                &memory_fabric,
                &chat_session_key,
                &config,
                sessions_redraw_handle.as_ref(),
                redraw_tx_for_main.as_ref(),
                &turn_signal,
                plain_mode,
                &mut plain_mode_turn_failed,
            )
            .await;
            #[cfg(not(feature = "terminal-tui"))]
            let applied_ordered_commits = !results.is_empty();
            if applied_ordered_commits {
                continue;
            }
        }
        if consume_deferred_visible_input_pop(&mut defer_visible_input_pop_once) {
            // A post-route admission failure just requeued the same input at the
            // front. Skip one immediate visible pop so the event pump can consume
            // in-flight completion/lifecycle events that may free stricter Legacy
            // admission.
        } else if let Some(input) = pop_next_visible_input_task_with_scheduler(
            &mut input_backlog,
            &mut turn_scheduler,
            &provider_turn_workers,
            visible_input_admission_kind,
            max_concurrent_visible_turns,
        ) {
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            break Some(input);
        }
        #[cfg(feature = "terminal-tui")]
        if should_drain_deferred_resume_after_visible_inputs(
            per_turn_contexts.len(),
            &input_backlog,
            &provider_turn_workers,
        ) && let Some(id) = deferred_resume_saved_session_ids.pop_front()
        {
            handle_resume_saved_session_control!(id);
            continue;
        }
        tokio::select! {
            msg = input_rx.recv(), if input_events_open => {
                let Some(msg) = msg else {
                    #[cfg(feature = "terminal-tui")]
                    {
                        if !per_turn_contexts.is_empty() {
                            input_events_open = false;
                            continue;
                        }
                    }
                    break None;
                };
                if msg.message.id == "mailbox-wake" && msg.message.sender == "session-message" && msg.message.sender_is_bot {
                    if let Some(inbox) = message_inbox.as_mut() { inbox.wake(); }
                    continue;
                }
                if super::is_local_operator_input(&msg.message) && !msg.message.sender_is_bot {
                    if let Some(inbox) = message_inbox.as_mut() { inbox.wake(); }
                }
                if let Some(target)=attached_follow.as_ref().filter(|_|super::is_local_operator_input(&msg.message) && !msg.message.sender_is_bot) {
                    match child_focus.submit(target.as_str(),msg.message.clone(),&super::child_focus::ChildFocusContext { memory:&mem,generation:&ctx.config_generation,parent:&chat_session_key,
                security:&ctx.security,service:&turn_service,tools:&tools_registry,sessions:&chat_sessions,approvals:&pending_approvals }).await {
                        Ok(Some(output))=>{surface_session_message(&chat_dispatcher,sessions_redraw_handle.as_ref(),&output);continue;},
                        Err(error)=>{surface_session_message(&chat_dispatcher,sessions_redraw_handle.as_ref(),&error.to_string());continue;},
                        Ok(None)=>{}
                    }
                }
                #[cfg(feature = "terminal-tui")]
                let active_task_id = per_turn_contexts.keys().next().copied();
                #[cfg(feature = "terminal-tui")]
                if let Some(active_task_id) = active_task_id {
                    let mut emit_active_turn_output =
                        |text: &str| surface_active_turn_message(&chat_dispatcher, redraw_tx_for_main.as_ref(), text);
                    let daemon_control = DaemonControl {
                        config: &config,
                        dispatcher: &chat_dispatcher,
                        redraw_tx: redraw_tx_for_main.as_ref(),
                    };
                    let queued_from = input_backlog.len();
                    process_active_turn_input_batch(
                        msg,
                        &mut emit_active_turn_output,
                        &mut input_rx,
                        &mut input_backlog,
                        &mut turn_scheduler,
                        &mut provider_turn_workers,
                        Some(active_task_id),
                        &chat_dispatcher,
                        &chat_session.session(),
                        &mut chat_sessions,
                        &session_rings,
                        &mut reaped_log_archive,
                        &reap_policy,
                        &tools_registry,
                        daemon_control,
                    )
                    .await;
                    if plain_typed_queue {
                        let added = lines_queued_since(&input_backlog, queued_from);
                        settle_queued_lines(&mut input_backlog, &mut turn_scheduler, &added);
                    }
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    continue;
                }
                let queued_from = input_backlog.len();
                enqueue_input_message_with_scheduler(
                    &mut input_backlog,
                    &mut turn_scheduler,
                    msg,
                    chat_session.session().turns.len(),
                );
                drain_available_input_messages(
                    &mut input_rx,
                    &mut input_backlog,
                    Some(&mut turn_scheduler),
                    chat_session.session().turns.len(),
                );
                let added = lines_queued_since(&input_backlog, queued_from);
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                let next = pop_next_visible_input_task_with_scheduler(
                    &mut input_backlog,
                    &mut turn_scheduler,
                    &provider_turn_workers,
                    visible_input_admission_kind,
                    max_concurrent_visible_turns,
                );
                if plain_typed_queue {
                    settle_queued_lines(&mut input_backlog, &mut turn_scheduler, &added);
                }
                if let Some(next) = next {
                    // The enqueue snapshot above reports this item as queued.
                    // Publish again after dequeue so fast slash commands, which
                    // never enter the provider-turn path, cannot leave a stale
                    // `queue:1` badge behind in the TUI.
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    break Some(next);
                }
                continue;
            },
            // EOF on the line front end's stdin. Lines read before it are
            // still queued; the arm waits until the input arm has taken them,
            // then closes the input like a closed channel would. The guard is
            // evaluated before the select polls: the reader may send its last
            // lines and fire EOF while the loop waits, making both arms ready
            // at once. Re-check here so lines sent before EOF are never dropped.
            () = stdin_closed.cancelled(), if input_events_open && input_rx.is_empty() => {
                if !input_rx.is_empty() {
                    continue;
                }
                input_events_open = false;
                continue;
            },
            lifecycle = provider_turn_lifecycle_rx.recv(), if provider_turn_lifecycle_events_open => {
                let Some(lifecycle) = lifecycle else {
                    provider_turn_lifecycle_events_open = false;
                    continue;
                };
                record_provider_turn_lifecycle_event(
                    &mut provider_turn_workers,
                    lifecycle,
                );
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            },
            completion = provider_completion_rx.recv() => {
                let Some(completion) = completion else {
                    continue;
                };
                let _ = route_provider_completion_event_and_publish(
                    &mut provider_turn_lifecycle_rx,
                    &mut provider_turn_lifecycle_events_open,
                    &mut provider_turn_workers,
                    &mut pending_provider_completion_events,
                    None,
                    completion,
                    &chat_dispatcher,
                );
                #[cfg(feature = "terminal-tui")]
                {
                    let _ = finalize_ready_per_turn_contexts(
                        &mut per_turn_contexts,
                        &mut pending_ordered_provider_turn_commits,
                        &mut pending_provider_completion_events,
                        &turn_signal,
                        &mut provider_turn_completion_contexts,
                        &mut history,
                        &tools_registry,
                        &mut provider_turn_finalizer_events,
                        &mut turn_scheduler,
                        &mut history_commit_coordinator,
                        &mut provider_turn_workers,
                        &chat_dispatcher,
                        &terminal,
                        &memory_fabric,
                        &chat_session_key,
                        &config,
                        sessions_redraw_handle.as_ref(),
                        redraw_tx_for_main.as_ref(),
                        plain_mode,
                        &mut plain_mode_turn_failed,
                    )
                    .await;
                }
                continue;
            },
            _ = shutdown.cancelled() => {
                #[cfg(feature = "terminal-tui")]
                {
                    if !per_turn_contexts.is_empty() {
                        finalize_all_per_turn_contexts_as_cancelled(
                            &mut per_turn_contexts,
                            &mut pending_ordered_provider_turn_commits,
                            &turn_signal,
                            &mut provider_turn_completion_contexts,
                            &mut history,
                            tools_registry.as_ref(),
                            &mut provider_turn_finalizer_events,
                            &mut turn_scheduler,
                            &mut history_commit_coordinator,
                            &mut provider_turn_workers,
                            &chat_dispatcher,
                            &terminal,
                            &memory_fabric,
                            &chat_session_key,
                            &config,
                            sessions_redraw_handle.as_ref(),
                            redraw_tx_for_main.as_ref(),
                            plain_mode,
                            &mut plain_mode_turn_failed,
                        )
                        .await;
                    }
                }
                break None
            },
            maybe_control = control_rx.recv(), if control_events_open => {
                let Some(control) = maybe_control else {
                    control_events_open = false;
                    continue;
                };
                match control {
                    #[cfg(feature = "terminal-tui")]
                    ChatControlEvent::ResumeSavedSession { id } => {
                        #[cfg(feature = "terminal-tui")]
                        if defer_resume_saved_session_if_provider_turn_pending(
                            per_turn_contexts.len(),
                            &mut deferred_resume_saved_session_ids,
                            id.clone(),
                        ) {
                            // Old Redux N=1 behavior waited inside the turn and did not
                            // poll control_rx, so session switching could not mutate
                            // chat_session/history until the active turn was finalized.
                            continue;
                        }
                        handle_resume_saved_session_control!(id);
                        continue;
                    }
                }
            }
            rewind_approval = async {
                match pending_chat_rewind.as_mut() {
                    Some(pending) => (&mut pending.approval_rx).await,
                    None => std::future::pending::<std::result::Result<
                        bool,
                        tokio::sync::oneshot::error::RecvError,
                    >>().await,
                }
            }, if pending_chat_rewind.is_some() => {
                let Some(pending) = pending_chat_rewind.take() else {
                    continue;
                };
                match resolve_rewind_approval(&pending.tool_id, rewind_approval) {
                    RewindApprovalOutcome::Apply => {
                        let target_id = pending.target_session.id.clone();
                        let target_turns = pending.target_session.turn_count();
                        let provider_name = current_provider_owned.as_str();
                        let model_name = current_model_owned.as_str();
                        match apply_chat_session_switch(ChatSwitchCtx {
                            chat_session: &mut chat_session,
                            commits: &turn_signal,
                            chat_session_key: &mut chat_session_key,
                            fabric_turn_seq: &mut fabric_turn_seq,
                            history: &mut history,
                            approval_router: approval_router.as_ref(),
                            pending_chat_rewind: &mut pending_chat_rewind,
                            pending_diff_apply: &mut pending_diff_apply,
                            chat_sessions: &mut chat_sessions,
                            ignored_session_events: &mut ignored_session_events,
                            session_rings: &mut session_rings,
                            reported_sessions: &mut reported_sessions,
                            announced_started_sessions: &mut announced_started_sessions,
                            last_sessions_summary: &mut last_sessions_summary,
                            last_sessions_entries: &mut last_sessions_entries,
                            attached_follow: &mut attached_follow,
                            attached_follow_seq: &mut attached_follow_seq,
                            chat_dispatcher: &chat_dispatcher,
                            redraw_handle: sessions_redraw_handle.as_ref(),
                            config: &config,
                            provider_name,
                            model_name,
                            skills: &skills,
                            native_tools,
                            #[cfg(feature = "terminal-tui")]
                            chat_mirror: &chat_mirror,
                        }, pending.target_session, true).await {
                            Ok(()) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &format!("Rewound chat session {target_id} to {target_turns} turns."),
                            ),
                            Err(e) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &format!("Rewind aborted: failed to save trimmed session: {e}"),
                            ),
                        }
                    }
                    RewindApprovalOutcome::Cancelled(message) => {
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &message);
                    }
                }
                continue;
            }
            apply_approval = async {
                match pending_diff_apply.as_mut() {
                    Some(pending) => (&mut pending.approval_rx).await,
                    None => std::future::pending::<std::result::Result<
                        bool,
                        tokio::sync::oneshot::error::RecvError,
                    >>().await,
                }
            }, if pending_diff_apply.is_some() => {
                let Some(pending) = pending_diff_apply.take() else {
                    continue;
                };
                match apply_approval {
                    Ok(true) => {
                        let security = crate::runtime::bootstrap::build_security_policy(&config);
                        match diff_apply::execute_plan(&pending.plan, security.as_ref()).await {
                            Ok(message) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &message,
                            ),
                            Err(error) => surface_session_message(
                                &chat_dispatcher,
                                sessions_redraw_handle.as_ref(),
                                &format!("Diff apply aborted: {error}. Workspace unchanged."),
                            ),
                        }
                    }
                    Ok(false) => {
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            "Diff apply cancelled; workspace unchanged.",
                        );
                    }
                    Err(_) => {
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &format!(
                                "Diff apply cancelled; approval channel closed for {} and workspace is unchanged.",
                                pending.tool_id
                            ),
                        );
                    }
                }
                continue;
            }
            _ = sessions_tick.tick() => {
                // A dedicated mTLS listener may run in another process. Its durable
                // mailbox writes cannot signal this process's in-memory inbox.
                if config.a2a.messaging_enabled && let Some(inbox) = message_inbox.as_mut() {
                    inbox.wake();
                }
                // 1) Summary reflow: surface each newly-finished session once,
                //    carrying its `#N` + status (plan §v1b). No auto-focus.
                let finished = chat_sessions.poll_finished(&mut reported_sessions).await;
                // 2) Persistent status line: recompute and dispatch only on change.
                let views = chat_sessions.snapshot().await;
                for fin in &finished {
                    let line = format_finished_session_announcement(fin);
                    let compact_summary = compact_child_completion_summary(&fin.summary);
                    surface_session_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        &line,
                    );
                    // v4: persist a summary of this finished child session
                    // into the chat session so a reload can show what it
                    // produced. Title / created_at come from the live view (the
                    // finished record itself only carries seq/status/summary);
                    // if the view is already gone we still record with the
                    // finished record's own fields so nothing is lost.
                    let persisted = views
                        .iter()
                        .find(|v| v.id.as_str() == fin.run_id)
                        .map_or_else(
                            || crate::chat::sessions::PersistedSessionSummary {
                                id: fin.run_id.clone(),
                                seq: fin.seq,
                                kind: fin.kind.as_str().to_string(),
                                origin: fin.origin.as_str().to_string(),
                                status: fin.status.as_str().to_string(),
                                title: String::new(),
                                summary: compact_summary.clone(),
                                token_usage_records: fin.token_usage_records.clone(),
                                created_at: fin.created_at,
                            },
                            |view| {
                                crate::chat::sessions::PersistedSessionSummary::from_view(
                                    view,
                                    compact_summary.clone(),
                                )
                            },
                        );
                    // The reducer records and saves the summary (both builds).
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::BackgroundSessionRecorded { summary: persisted },
                        "chat.bg_session_recorded",
                    );
                }
                let now = chrono::Utc::now();
                let reaped = chat_sessions.reap(&reap_policy, now).await;
                if !reaped.reaped.is_empty() {
                    let reaped_ids = reaped
                        .reaped
                        .iter()
                        .map(|session| session.id.clone())
                        .collect::<std::collections::HashSet<_>>();
                    for session in &reaped.reaped {
                        if chat_session
                            .session()
                            .background_sessions
                            .iter()
                            .any(|summary| summary.id == session.summary.id)
                        {
                            continue;
                        }
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::BackgroundSessionRecorded {
                                summary: session.summary.clone(),
                            },
                            "chat.bg_session_recorded_reap",
                        );
                    }
                    reaped_log_archive.archive_reaped(&reaped.reaped, &mut session_rings, &reap_policy, now);
                    if attached_follow.as_ref().is_some_and(|id| reaped_ids.contains(id)) {
                        attached_follow = None;
                        attached_follow_seq = None;
                        let focus = crate::chat::sessions::FocusTarget::Main;
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SessionFocusChanged { focus },
                            "chat.session_focus_reaped",
                        );
                        #[cfg(feature = "terminal-tui")]
                        {
                            {
                                let mut mirror = chat_mirror.lock();
                                mirror.focus = focus;
                                mirror.active_session_view = None;
                            }
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                "chat.active_session_view_reaped",
                            );
                        }
                    }
                }
                let views = chat_sessions.snapshot().await;
                for view in &views {
                    if view.kind != crate::chat::sessions::model::ManagedKind::Agent {
                        continue;
                    }
                    if announced_started_sessions.insert(view.id.as_str().to_string()) {
                        let line = format_observed_session_announcement(view);
                        surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &line);
                    }
                }
                // v1.1b: refresh the switcher cache the key thread reads on Ctrl+G
                // (it cannot run async registry queries itself). Display staleness
                // is harmless: switcher Enter re-resolves the seq via /attach.
                #[cfg(feature = "terminal-tui")]
                {
                    let idle_warnings = chat_sessions
                        .idle_warning_seqs(&reap_policy, chrono::Utc::now(), &session_rings)
                        .await;
                    let mut entries = crate::chat::sessions::focus::switcher_entries(&views);
                    for entry in &mut entries {
                        entry.idle_warning = idle_warnings.contains(&entry.seq);
                    }
                    {
                        let mut mirror = chat_mirror.lock();
                        refresh_sessions_cache(&mut mirror, entries.clone());
                    }
                    if entries != last_sessions_entries {
                        last_sessions_entries = entries.clone();
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::SessionsEntriesUpdated { entries },
                            "chat.sessions_entries",
                        );
                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                }
                let new_summary = crate::chat::sessions::status_summary(&views);
                if new_summary != last_sessions_summary {
                    last_sessions_summary = new_summary.clone();
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::SessionsStatusUpdated { summary: new_summary },
                        "chat.sessions_status",
                    );
                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                        let _ = tx.try_send(());
                    }
                }
                // Keep waiting (do not break the inner loop / produce a message).
                continue;
            }
            maybe_event = session_event_rx.recv(), if session_events_open => {
                // Drain one background-session event: append it to that session's
                // ring (single-consumer write, no lock) and, if focused, refresh
                // the child viewport. P2 keeps child output out of main scrollback.
                let Some(event) = maybe_event else {
                    // Sender side closed (chat shutting down). Disable this arm so
                    // a closed channel does not busy-spin; other arms drive exit.
                    session_events_open = false;
                    continue;
                };
                let sid = event.session_id().clone();
                if should_ignore_session_event_after_chat_resume(&ignored_session_events, &event) {
                    continue;
                }
                let ring = session_rings
                    .entry(sid.clone())
                    .or_insert_with(|| crate::chat::sessions::SessionRing::with_capacity(
                        crate::chat::sessions::event::DEFAULT_RING_CAPACITY,
                    ));
                // A `Truncated` marker carries no output line; it only flags the
                // ring so `/attach` shows `[output truncated]` for events the
                // drainer had to drop on a full channel (P1 fix). Delta/ToolCall
                // append their text as a new ring line.
                let line = match &event {
                    crate::chat::sessions::SessionEvent::Delta { text, .. } => Some(text.clone()),
                    crate::chat::sessions::SessionEvent::ToolCall { summary, .. } => Some(summary.clone()),
                    crate::chat::sessions::SessionEvent::Truncated { .. } => {
                        ring.mark_truncated();
                        None
                    }
                    // NeedsInput: a background sub-agent suspended awaiting an
                    // operator approval decision. Record a ring line for `/attach`
                    // visibility; the non-intrusive `/approve` hint is surfaced
                    // below (after the seq is resolved). Status flips to
                    // `❓ needs-input` via the registry (already set by the
                    // resolver) on the next status refresh.
                    crate::chat::sessions::SessionEvent::NeedsInput { prompt, .. } => {
                        Some(format!("[needs approval] {prompt}"))
                    }
                    crate::chat::sessions::SessionEvent::Resumed { .. } => {
                        Some("[approval resolved — resuming]".to_string())
                    }
                };
                if let Some(line) = line {
                    ring.push(line);
                }
                // NeedsInput / Resumed are control signals (not stream output):
                // surface a non-intrusive operator hint with the session's `#N`
                // and refresh the status line / switcher so the `❓` glyph and
                // `needs-input` counter appear immediately, regardless of attach.
                match &event {
                    crate::chat::sessions::SessionEvent::NeedsInput { prompt, .. } => {
                        let seq = chat_sessions.seq_for_id(&sid).await;
                        let label = seq
                            .map(|n| format!("#{n}"))
                            .unwrap_or_else(|| sid.as_str().to_string());
                        surface_session_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            &format!(
                                "session {label} awaiting approval: {prompt} — /approve {} or /deny {} (/attach {} to inspect)",
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                                seq.map(|n| n.to_string()).unwrap_or_else(|| label.clone()),
                            ),
                        );
                    }
                    crate::chat::sessions::SessionEvent::Resumed { .. } => {
                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                            let _ = tx.try_send(());
                        }
                    }
                    _ => {}
                }
                #[cfg(feature = "terminal-tui")]
                if attached_follow.as_ref() == Some(&sid) {
                    refresh_attached_session_view_from_ring(
                        &chat_mirror,
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        &sid,
                        ring,
                    );
                }
                continue;
            }
        }
    }

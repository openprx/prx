{
        // This is the concrete TerminalChannel: send_draft always returns
        // Some(id), including when its UI receiver has closed. Capacity can
        // therefore be selected before creating the durable source. The local
        // worker registry is inserted into only by this main loop; awaits below
        // cannot consume another slot.
        #[cfg(feature = "terminal-tui")]
        let reducer_driver_turn_active = true;
        #[cfg(feature = "terminal-tui")]
        let provider_worker_kind = crate::chat::turn_worker::ProviderTurnWorkerKind::Detached;
        #[cfg(not(feature = "terminal-tui"))]
        let provider_worker_kind = crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited;
        let provider_admission_max = match provider_worker_kind {
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => 1,
            crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => max_concurrent_visible_turns,
        };
        let provider_admission =
            provider_turn_visible_admission(&provider_turn_workers, provider_worker_kind, provider_admission_max);
        let catalog_changed = turn_service.check_live_parity().is_err();
        if !provider_admission.can_start_visible || (catalog_changed && provider_admission.active_workers > 0) {
            tracing::warn!(
                active_workers = provider_admission.active_workers,
                foreground_active = provider_admission.foreground_active,
                detached_active = provider_admission.detached_active,
                effective_max_visible_turns = provider_admission.effective_max_visible_turns,
                target_kind = ?provider_worker_kind,
                "visible provider turn capacity unavailable before source admission; queueing input"
            );
            // No durable input or decision exists yet. Keep the original
            // assignment and queue identity for its first actual admission.
            if let Some(guard) = assignment_guard.as_mut() {
                guard.disarm();
            }
            requeue_post_route_admission_rejected_input(
                &mut input_backlog,
                &mut turn_scheduler,
                &mut defer_visible_input_pop_once,
                DequeuedInputMessage {
                    priority: input_priority,
                    turn_task_id: provider_queue_task_id,
                    msg,
                    terminal_origin: terminal_origin.clone(),
                },
            );
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            continue;
        }
        if catalog_changed {
            // Publish only at a new source boundary after existing model turns
            // have finished. Their sealed catalog/grants are never rewritten.
            // Keep publication's large builder future off the chat task's
            // stack, including unoptimized binaries used by PTY regression.
            let replacement = Box::pin(ctx.publish_turn_runtimes(
                Arc::clone(&tools_registry),
                crate::tools::capability::turn_state::TurnSurfaceKind::Interactive,
            )).await?;
            child_cleanup.retain_service(Arc::clone(&replacement));
            let retired = std::mem::replace(&mut turn_service, replacement);
            let retirement = child_cleanup.retire(&retired)?;
            let drained = retired.drain_writes(std::time::Instant::now() + Duration::from_secs(2)).await;
            retirement.finish(drained);
            anyhow::ensure!(drained, "previous chat catalog writes were not confirmed");
        }
        fabric_turn_seq += 1;
        // D8-2: one run_id per turn, generated at the turn entry and reused by
        // every run_id consumer within this loop iteration (user event, route
        // scope, assistant event). No parent_run_id is set (turns are not a spawn
        // lineage; session relation is via chat_session_key).
        let turn_run_id = uuid::Uuid::new_v4().to_string();
        let current_provider_binding = provider_binding.as_ref()
            .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))?;
        current_provider_binding.verify(&ctx.config_generation, &provider, provider_name, model_name)?;
        let cancellation = turn_signal.new_turn_cancellation();
        let mut input_scope = chat_message_event_scope(
            memory_fabric.workspace_id(), &chat_session_key, &turn_run_id, provider_name, model_name,
        );
        input_scope.causation_event_id=super::message_inbox::cause(&msg).map(str::to_owned);
        input_scope.config_generation_id = Some(ctx.config_generation.id.0);
        input_scope.config_source_revision = Some(ctx.config_generation.source_revision.fingerprint_sha256.clone());
        // The TUI mirror is updated synchronously by /mode; its legacy session
        // deliberately stops tracking mode when the reducer owns UI state.
        #[cfg(feature = "terminal-tui")]
        let admitted_chat_mode = chat_mirror.lock().chat_mode;
        #[cfg(not(feature = "terminal-tui"))]
        let admitted_chat_mode = chat_session.session().mode;
        let has_conversation_history = history.iter().any(|message| message.role != "system");
        let (chat_user_event, admitted_binding, mut admission_guard) = current_provider_binding.record_guarded_with_origin(
            memory_fabric.clone(), (input_scope, &chat_session_key, binding::ChatAdmissionMode::Model(admitted_chat_mode), has_conversation_history), &user_input,
            format!("chat:{}:{turn_run_id}:{fabric_turn_seq}:user", chat_session.session().id),
            child_cleanup.register(cancellation.clone())?, entry::prepare_origin(terminal_origin,&msg,&user_input)?,
        ).await?;
        let admission = admitted_binding
            .issue_decision(&turn_service, &cancellation)
            .await;
        if let Err(error) = admission {
            if let Err(record_error) = admission_guard.finish_failure(
                cancellation.is_cancelled(), &format!("decision admission failed: {error}"),
            ).await {
                return Err(error.context(format!("Chat admission failure persistence is unconfirmed: {record_error}")));
            }
            return Err(error);
        }
        admission_guard.handoff_terminal()?;
        let chat_turn_binding = Some(Arc::clone(&admitted_binding));
        if one_shot.is_some() {
            one_shot_binding = Some(Arc::clone(&admitted_binding));
        }

        let file_mention_enrichment = enrich_file_mentions_for_prompt(&user_input, tools_registry.as_ref()).await;
        if let Some(note) = file_mention_enrichment.visible_note.as_deref() {
            emit_chat_output(note);
        }
        let user_input_for_prompt = file_mention_enrichment.prompt;

        // Auto-save user message to memory
        if config.memory.should_auto_promote_user_message(&user_input) {
            let safe_user_input = sanitize_chat_semantic_memory_content(&user_input);
            let _ = crate::agent::entry::save_current_input(&memory_fabric,
                admitted_binding.envelope().run_id.as_deref().context("chat turn run id missing")?,
                &admitted_binding.envelope().session_key,
                &safe_user_input, chat_user_event.as_ref().map(|event| event.event_id.as_str())).await;
        }

        // Inject memory context
        let runtime_envelope = admitted_binding.envelope().clone();
        let memory_runtime = crate::agent::turn_request::ToolLoopMemory::new(
            &mem,
            &config.workspace_dir,
            Some(
                DocumentIngestRuntime::from_envelope(mem.clone(), &runtime_envelope)
                    .with_source_message_event_id(chat_user_event.as_ref().map(|event| event.event_id.clone())),
            ),
        )
        .with_event_fabric(memory_fabric.clone())
        .with_request_event_scope(
            runtime_envelope.message_scope().with_recipient(format!("{provider_name}/{model_name}")),
        )
        .with_tool_surface_reserve(tool_surface_reserve.clone());
        // The TUI handoff retains the original source input, before enrichment.
        #[cfg(feature = "terminal-tui")]
        let routing_input = admitted_binding.routing_input().to_owned();
        let mem_context = admitted_binding.recalled_context(mem.as_ref()).await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input_for_prompt.clone()
        } else {
            format!("{context}{user_input_for_prompt}")
        };

        // Build system prompt with skill selection
        let selected_skills = admitted_binding.prompt_skills()?;
        // Publish only this turn's selected metadata. Previous turns do not
        // grant continued Skill exposure; the prompt builder orders entries.
        let system_prompt = build_chat_system_prompt(&config, model_name, &selected_skills, native_tools);
        // An ordered, acknowledged read before this turn builds its sources
        // and copies its provider view: every action sent before it (another
        // turn's compaction whose finalization is still pending included) is
        // applied first, so no turn starts from a view the session already
        // compacted, and its start epoch is from before its own preflight.
        let admitted_session = acknowledged_session_barrier(&chat_dispatcher, &turn_signal).await?;
        if admitted_session.compaction_epoch != provider_view_compaction_epoch {
            history = super::compaction::provider_view_from_acked(&history, &admitted_session);
            provider_view_compaction_epoch = admitted_session.compaction_epoch;
        }
        let persisted_history_for_turn = persisted_history_for_current_turn(&chat_session.session(), &system_prompt, &user_input);
        upsert_leading_system_prompt(&mut history, system_prompt.clone());
        // S2-C Step 4: dual-write SetLeadingSystemPrompt to the reducer — byte-for-byte aligned with
        // the legacy `if empty { push } else { first_mut = ... }` (the reducer takes the same
        // branches internally). It runs on every turn, and expressing it as an append would pile up
        // system messages.
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::SetLeadingSystemPrompt {
                content: system_prompt.clone(),
            },
            "chat.system_prompt_per_turn",
        );
        let history_len_before_user_turn = history.len();
        let history_user_message = crate::agent::turn_contracts::inbound_user_message(&enriched);
        // The provider-side copy is read only by the TUI reducer driver.
        // The compaction epoch the provider-side copy reflects; a worker
        // compaction during the turn moves it on (see the finalizer).
        #[cfg(feature = "terminal-tui")]
        let compaction_epoch_at_start = admitted_session.compaction_epoch;
        #[cfg(feature = "terminal-tui")]
        let history_for_provider = {
            let mut history_for_provider = history.clone();
            history_for_provider.push(history_user_message.clone());
            history_for_provider
        };

        // ── Set active recipient/channel on tools (for proactive messaging) ──
        for tool in tools_registry.iter() {
            if tool.name() == "message_send" {
                continue;
            }
            tool.set_active_recipient("user").await;
            tool.set_active_channel(Arc::clone(&terminal) as Arc<dyn Channel>).await;
        }

        // ── Streaming pipeline setup ─────────────────────────────
        //
        // The delta channel carries ONLY visible assistant text — never the
        // model's reasoning/thinking content. Reasoning is separated upstream
        // at the provider parsing layer (see `parse_native_response` /
        // `parse_sse_line` in providers/{anthropic,openai,ollama,compatible}.rs)
        // and travels back via `ProviderChatResponse.reasoning_content`. The
        // tool-call loop persists reasoning into conversation history through
        // `build_native_assistant_history`; the live stream below renders text
        // only, so the user never sees the model's internal monologue.
        let (delta_tx, delta_rx) = mpsc::channel::<String>(DELTA_CHANNEL_CAPACITY);

        // Start a streaming draft on the terminal
        let draft_id = match terminal.send_draft(&SendMessage::new("", "user")).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Failed to start draft");
                None
            }
        };

        #[cfg(feature = "terminal-tui")]
        anyhow::ensure!(
            !reducer_driver_turn_active || draft_id.is_some(),
            "admitted detached chat turn requires its terminal draft; execution was not requeued"
        );

        // Step 5b dual write: announce that a new LLM inference turn starts (only when a draft
        // exists). In shadow mode the reducer sets stream.draft + control.generating=true, with no
        // external side effects (business Effects are no-ops).
        if let Some(ref d_id) = draft_id {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::TurnStarted {
                    draft_id: d_id.clone(),
                    cancel: cancellation.clone(),
                },
                "chat.turn_started",
            );
        }

        // Spawn background task: accumulate deltas → channel.update_draft()
        // Follows the exact same pattern as process_channel_message in channels/mod.rs.
        //
        // P1-6 — Monotonic draft version protocol (Step 3 update). The
        // sender-side counter still stamps each accumulated snapshot with a
        // strictly monotonic `u64` (kept for `update_draft` downstream
        // consumers and the inline-redraw protocol). The receiver-side stale
        // check formerly performed here by `DraftVersionTracker.accept()` has
        // been DELETED — its protection is now owned end-to-end by the
        // Redux-style reducer in `chat::state` via `StreamState::draft.version`
        // (see `ChatState::reduce_stream_chunk_received`).
        //
        // Why this is safe to remove:
        //   1. `delta_rx` is a single-task tokio mpsc; FIFO is guaranteed at
        //      the runtime layer (no parallel accumulators).
        //   2. The counter is incremented atomically inside the same task,
        //      producing a strictly monotonic sequence by construction. The
        //      old tracker call was over-defence ("unreachable here" per the
        //      original comment).
        //   3. The reducer's `StreamChunkReceived` arm now enforces
        //      strict-monotonic version + draft_id matching + finalize-state
        //      check as the single source of truth. Once Step 5 makes the
        //      reducer the renderer source, this task disappears entirely.
        //
        // Terminal TUI now relies on the reducer for the visible stream; the
        // non-TUI fallback still forwards full accumulated drafts to the
        // channel implementation.
        let draft_updater = if let Some(ref d_id) = draft_id {
            #[cfg(not(feature = "terminal-tui"))]
            let channel: Arc<TerminalChannel> = Arc::clone(&terminal);
            #[cfg(not(feature = "terminal-tui"))]
            let reply_target = "user".to_string();
            #[cfg(not(feature = "terminal-tui"))]
            let draft_id_owned = d_id.clone();
            let mut rx = delta_rx;
            let version_counter = Arc::new(DraftVersionCounter::new());
            // Step 5b dual write: send every delta through the coalescer as
            // Action::StreamChunkReceived. When the bounded(2048) action_tx is full the coalescer
            // merges deltas, so nothing grows without bound.
            let coalescer_sender = chat_dispatcher.sender();
            let coalescer_draft_id = d_id.clone();
            Some(tokio::spawn(async move {
                #[cfg(not(feature = "terminal-tui"))]
                let mut accumulated = String::new();
                let mut coalescer = dispatcher::StreamChunkCoalescer::new(coalescer_sender);
                while let Some(delta) = rx.recv().await {
                    // Counter still ticks for downstream consumers (UiActor's
                    // inline-redraw protocol uses it). No tracker.accept() —
                    // see comment block above.
                    let version = version_counter.next();
                    #[cfg(not(feature = "terminal-tui"))]
                    {
                        accumulated.push_str(&delta);
                        if let Err(e) = channel.update_draft(&reply_target, &draft_id_owned, &accumulated).await {
                            tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Draft update failed");
                        }
                    }
                    // Step 5b shadow: forward the **incremental** delta into
                    // the reducer via the coalescer. The reducer accumulates
                    // its own `draft.accumulated` mirror — feeding the full
                    // `accumulated` string here would cause double-accumulation.
                    // Backpressure or close are silently tolerated (legacy
                    // path remains the renderer source; shadow path observes).
                    let _ = coalescer.try_send_chunk(coalescer_draft_id.clone(), delta, version);
                }
                // Stream ended — flush pending coalescer state, counter goes
                // out of scope; reducer-side version state is cleared on
                // StreamCompleted/Failed/Cancelled (dispatched from the chat::run main loop).
                let _ = coalescer.flush();
            }))
        } else {
            // No draft — consume delta_rx so the sender doesn't block
            let mut rx = delta_rx;
            Some(tokio::spawn(async move { while rx.recv().await.is_some() {} }))
        };

        #[cfg(not(feature = "terminal-tui"))]
        {
            *active_cancel.lock() = Some(cancellation.clone());
        }

        // ── Tool event forwarding (visual feedback in terminal) ──
        //
        // P2-7: in addition to the existing notify_tool_* calls (which feed
        // the legacy UiActor renderer in `channels/terminal.rs`), we also
        // mirror every tool event into a `TuiState` instance behind a
        // `parking_lot::Mutex`. The ratatui renderer in `chat/tui.rs` reads
        // from this mirror; full renderer wiring lands in P2-12.
        let (tool_event_tx, mut tool_event_rx) = mpsc::channel::<ToolCallNotification>(TOOL_EVENT_CHANNEL_CAPACITY);
        let terminal_for_tools = Arc::clone(&terminal);
        // P3-3: every producer in this turn now shares the chat-scoped
        // `chat_mirror` (created once at the top of `run`). The previous
        // per-turn `tui_mirror` instance is gone — keeping a per-turn alias
        // here so downstream code that already says `tui_mirror.lock()` keeps
        // compiling, but the underlying `Arc` is the same one the render
        // task and the input task hold.
        #[cfg(feature = "terminal-tui")]
        let tui_mirror: Arc<parking_lot::Mutex<tui::TuiState>> = Arc::clone(&chat_mirror);
        // P0-5 fix: tool start/finish events now have a single mirror path.
        // The UiActor in `channels/terminal.rs::handle_event_tui` is the sole
        // writer of tool cards into `TuiState` (it sanitises name/args via
        // `sanitize_terminal_output` and calls `notify_redraw()` on the same
        // 1-slot channel as `redraw_tx_for_main`). The previous double-mirror
        // here pushed a second card and could reorder Running/Done with the
        // UiActor path under load — the forwarder now just relays the event
        // to the UiActor and lets that path own the mirror mutation.
        let tool_event_forwarder = tokio::spawn(async move {
            while let Some(notif) = tool_event_rx.recv().await {
                match notif {
                    ToolCallNotification::Started { name, args_summary, .. } => {
                        terminal_for_tools.notify_tool_started(&name, &args_summary).await;
                    }
                    ToolCallNotification::Finished {
                        name,
                        success,
                        duration_ms,
                        ..
                    } => {
                        terminal_for_tools
                            .notify_tool_finished(&name, success, duration_ms)
                            .await;
                    }
                    ToolCallNotification::Progress { iteration } => {
                        terminal_for_tools.notify_progress(iteration).await;
                    }
                }
            }
        });
        // Log a trace stat so the mirror is observably wired (also keeps the
        // `tui_mirror` binding from being flagged as unused when the renderer
        // wiring lands in P2-12).
        #[cfg(feature = "terminal-tui")]
        {
            let tracked_tool_cards_count: usize =
                tui_mirror.lock().last_tool_result_index().map_or(0, |index| index + 1);
            tracing::trace!(tracked_tool_cards_count, "tui_mirror initialized");
        }

        // ── Unified tool authorization (permission-model Phase 1) ──
        // Tool access is governed solely by `[autonomy]` via
        // `SecurityPolicy::decide`; the former PolicyPipeline is removed.
        let scope_owner_id = runtime_envelope.resolved_owner_id();
        let scope_ctx = ScopeContext {
            policy: &security,
            sender: "user",
            channel: "terminal",
            chat_type: "private",
            chat_id: "terminal:user",
            session_key: Some(&runtime_envelope.session_key),
            owner_id: Some(&scope_owner_id),
            owner_isolated: runtime_envelope.owner_isolated,
            topic_id: runtime_envelope.topic_id.as_deref(),
            task_id: runtime_envelope.resolved_task_id(),
            source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
            config_generation_id: runtime_envelope.config_generation_id,
            config_source_revision: runtime_envelope.config_source_revision.as_deref(),
        };

        let mut effective_compaction = admitted_binding.effective_compaction()?;
        effective_compaction.config = admitted_binding.main_compaction()
            .ok_or_else(|| anyhow::anyhow!("chat compaction requires an admitted decision"))?.clone();
        crate::providers::context_window::trace_effective_compaction_resolution(&effective_compaction);
        #[cfg(feature = "terminal-tui")]
        refresh_context_budget_for_tui(
            &history_for_provider,
            &effective_compaction.config,
            &tool_surface_reserve,
            redraw_tx_for_main.is_some(),
            &chat_mirror,
            &chat_dispatcher,
        );

        let route_decision = RouteDecision::from_model_routes_for_context(
            provider_name,
            model_name,
            &config.model_routes,
            runtime_envelope.resolved_owner_id(),
            chat_session_key.clone(),
            chat_user_event.as_ref().map(|event| event.event_id.clone()),
            "chat",
            (user_input.chars().count() / 4 + 1).min(u32::MAX as usize) as u32,
            true,
            false,
        );
        let route_scope = route_event_scope(
            "chat",
            Some(runtime_envelope.resolved_owner_id()),
            Some(chat_session_key.clone()),
            Some(turn_run_id.clone()),
            Some("local-user".to_string()),
            Some(format!(
                "{}/{}",
                route_decision.selected.provider, route_decision.selected.model
            )),
        );
        if let Err(e) = record_route_decision_event(&memory_fabric, route_scope.clone(), &route_decision).await {
            tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "Failed to append router.route_decision message event");
        }
        let provider_started_at = chrono::Utc::now();

        // The shared turn engine owns context-overflow recovery. Chat projects
        // its final outcome and does not start a second compaction/replay loop.
        //
        // There is deliberately no wall-clock budget around the turn. A chat turn
        // ends when it finishes, when the user cancels it, or when the stall
        // detector in `crate::agent::idle` finds it has stopped making progress.
        #[cfg(feature = "terminal-tui")]
        let history_len_before_tools;

        // S2-A refinement: split the coarse `Failed` variant so the Redux
        // dispatch path can distinguish user-driven cancellation from real
        // errors. The legacy renderer still treats every non-Success as a
        // failure (continue), but the reducer now sees the correct semantic
        // (`StreamCancelled` vs `StreamFailed { err, retryable }`).
        enum TurnOutcome {
            // FIX-P0-30/31: carry the loop's provider-attribution trace so the
            // success path can record the *real* serving model/attempts instead
            // of the routed `decision.selected.model`.
            Success(String),
            /// User-initiated cancel (Ctrl+C) or `is_tool_loop_cancelled` from
            /// the inner loop. Reducer side maps to `StreamCancelled`.
            Cancelled,
            /// Genuine failure (timeout / context-overflow exhausted / other
            /// error). Carries error string + retryable hint for the reducer's
            /// `StreamFailed` payload (mirrors `dispatcher::TurnOutcomeKind::Failed`).
            FailedWithError {
                err: String,
                retryable: bool,
            },
        }

        // The terminal TUI always routes a turn to the reducer driver; without
        // the TUI the shared agent loop runs it (the driver branch is cfg'd out).
        #[cfg(feature = "terminal-tui")]
        tracing::info!(
            redux_mode = ?top_redux_mode,
            // Registry roots before routing, refresh and alias expansion;
            // the wire count is the compiled-request event's
            // `tool_snapshot.count`.
            registry_root_tools = Vec::len(&tools_registry),
            route = "ReduxDriver",
            "chat::run turn route decision"
        );
        let provider_history_len_before_assistant;
        // The identity of the request recorded at admission: an aborted turn
        // settles by it, so no earlier request with the same text is touched.
        let admission;
        #[cfg(feature = "terminal-tui")]
        {
            if reducer_driver_turn_active {
                admission = None;
                provider_history_len_before_assistant = history_for_provider.len();
            } else {
                admission = Some(record_user_turn_at_admission(&chat_dispatcher, &turn_signal, &user_input).await?);
                history.push(history_user_message.clone());
                provider_history_len_before_assistant = history.len();
            }
        }
        #[cfg(not(feature = "terminal-tui"))]
        {
            admission = Some(record_user_turn_at_admission(&chat_dispatcher, &turn_signal, &user_input).await?);
            history.push(history_user_message.clone());
            provider_history_len_before_assistant = history.len();
        }

        let provider_turn_task_id = start_provider_turn_task(
            &mut turn_scheduler,
            provider_queue_task_id,
            &user_input,
            history_len_before_user_turn,
        );
        anyhow::ensure!(
            !chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) || provider_turn_task_id.is_some(),
            "decision-owned Chat turn could not acquire a scheduler task"
        );
        if one_shot.is_some() {
            one_shot_task_id = provider_turn_task_id;
        }
        register_provider_history_commit_task(&mut history_commit_coordinator, &turn_scheduler, provider_turn_task_id);
        record_provider_turn_completion_context(
            &mut provider_turn_completion_contexts,
            provider_turn_task_id,
            provider_history_len_before_assistant,
        );
        register_provider_turn_worker(
            &mut provider_turn_workers,
            &turn_scheduler,
            provider_turn_task_id,
            provider_worker_kind,
        );
        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);

        // ── Redux driver switched-over path (Step 5a-4) ────────────
        //
        // Entered only when routing selects ReduxDriver. dispatch Action::StartLLMTurn →
        // EffectExecutor::execute_real(Effect::StartTurn) → spawn drive_start_turn_stream
        // streaming driver → post StreamChunkReceived / Completed / Failed / Cancelled back through
        // action_tx → the dispatcher task reduces them and calls turn_signal.record_and_notify →
        // this code awaits the outcome.
        //
        // This branch does **not** call run_turn; the old path does not run at all:
        //   * no duplicate hooks (the old path's hooks.emit does not run; only the reducer's
        //     NotifyHook(Error) writes)
        //   * no duplicate history writes (the reducer writes once through RecordAssistantTurn)
        //   * round 2 hang defence: shutdown.cancelled() as a fallback arm of tokio::select!
        //
        #[cfg(feature = "terminal-tui")]
        if reducer_driver_turn_active && let Some(d_id) = draft_id.clone() {
            // Protocol: register the keyed turn before dispatch, then obtain the
            // waiter. The legacy single-slot signal remains as fallback while
            // P6J migrates toward detached provider workers.
            if let Some(id) = provider_turn_task_id {
                turn_signal.register_turn(id, d_id.clone());
                let _ = turn_signal.consume_turn_outcome(id);
                let _ = turn_signal.consume_turn_usage(id);
            }
            if let Some(id) = provider_turn_task_id {
                spawn_provider_turn_completion_waiter(
                    turn_signal.clone(),
                    id,
                    provider_completion_tx.clone(),
                    shutdown.clone(),
                );
            }
            let mut notify_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> =
                provider_turn_task_id
                    .is_none()
                    .then(|| Box::pin(turn_signal.notified()) as _);
            // Consume any leftover outcome before dispatching, so what we read belongs to this turn.
            let _ = turn_signal.consume_outcome();

            // S2.5 P1-A: handle dispatch_result in explicit branches (a failed StartLLMTurn must
            // fall through, otherwise notify_fut hangs forever); dispatch_or_log already meters and
            // warns, so no extra tracing is needed.
            // D8-4 (redux path real fix): seed the turn-root spawn execution
            // context for this turn and hand it to the driver via StartLLMTurn →
            // Effect::StartTurn. This is the redux mirror of the legacy
            // `SPAWN_EXECUTION_CONTEXT.scope(seed_turn_context(turn_run_id, ..))`
            // wrapper applied below at the legacy tool-loop path's `run_turn` call
            // — the redux path `continue`s before reaching it, so the seed must
            // travel with the effect. Same `turn_run_id` + `chat_session_key`
            // source as the legacy path (single source of truth) so a sub-agent
            // spawned inside this turn inherits `parent_run_id = turn_run_id`.
            let mut redux_turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                turn_run_id.clone(),
                chat_session_key.clone(),
            );
            redux_turn_spawn_ctx.owner_id = Some(runtime_envelope.resolved_owner_id());
            redux_turn_spawn_ctx.topic_id = runtime_envelope.topic_id.clone();
            redux_turn_spawn_ctx.source_message_event_id = runtime_envelope.source_message_event_id.clone();
            let redux_turn_message_send_ctx = crate::tools::message_send::MessageSendExecutionContext::new(
                Some("user".to_string()),
                Arc::clone(&terminal) as Arc<dyn Channel>,
            );
            let provider_turn_sequence =
                provider_turn_task_id.and_then(|id| turn_scheduler.task(id).map(|task| task.sequence));
            let dispatch_result = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::StartLLMTurn {
                    provider_turn_task_id,
                    provider_turn_sequence,
                    draft_id: d_id.clone(),
                    history: history_for_provider.clone(),
                    compaction_guard_history: Some(persisted_history_for_turn.clone()),
                    compaction_config: Some(effective_compaction.config.clone()),
                    cancel: cancellation.clone(),
                    turn_spawn_ctx: Some(Box::new(redux_turn_spawn_ctx)),
                    turn_message_send_ctx: Some(redux_turn_message_send_ctx),
                    // Route on what the user typed. `history_for_provider`
                    // already carries the memory preamble and the
                    // `[Recent shared workspace events]` block, and routing on
                    // that let injected text widen the published tool surface.
                    routing_input: Some(routing_input.clone()),
                    turn_binding: chat_turn_binding.clone(),
                },
                "chat.start_llm_turn",
            );
            // Codex P1: the dispatch may be Backpressured / ChannelClosed. Either failure means the
            // dispatcher task will not produce a turn outcome, so chat::run must immediately treat it
            // as Failed and fall through to cleanup, otherwise notify_fut is never fired.
            if !matches!(dispatch_result, dispatcher::DispatchResult::Sent) {
                tracing::warn!(
                    result = dispatch_result.kind(),
                    "Redux driver: StartLLMTurn dispatch failed; aborting turn"
                );
                // S2-B Step 3: send StreamCancelled synchronously so the reducer clears
                // active_cancel; the old field is only a fallback under Off/Both (symmetric with the
                // guard at the register site).
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled_dispatch_failed",
                    );
                }
                #[cfg(not(feature = "terminal-tui"))]
                {
                    *active_cancel.lock() = None;
                }
                drop(delta_tx);
                drop(tool_event_tx);
                if let Some(handle) = draft_updater {
                    let _ = handle.await;
                }
                let _ = tool_event_forwarder.await;
                if let Some(ref id) = draft_id {
                    let _ = terminal.cancel_draft("user", id).await;
                }
                enqueue_provider_turn_finalizer_event(
                    &mut provider_turn_finalizer_events,
                    provider_turn_task_id,
                    ProviderTurnTerminalPlan::Failed {
                        err: "redux driver dispatch failed".to_string(),
                        history_commit_len: take_provider_turn_completion_history_len(
                            &mut provider_turn_completion_contexts,
                            provider_turn_task_id,
                            history.len(),
                        ),
                        summary: "redux driver dispatch failed".to_string(),
                    },
                );
                continue;
            }

            if let Some(task_id) = provider_turn_task_id {
                if per_turn_contexts
                    .insert(
                        task_id,
                        PerTurnContext {
                            _mailbox_input: mailbox_input.take(),
                            turn_binding: chat_turn_binding.clone(),

                            task_id,
                            draft_id: d_id.clone(),
                            delta_tx: Some(delta_tx),
                            tool_event_tx: Some(tool_event_tx),
                            draft_updater,
                            tool_event_forwarder: Some(tool_event_forwarder),
                            user_input: user_input.clone(),
                            turn_run_id: turn_run_id.clone(),
                            route_scope: route_scope.clone(),
                            route_decision: route_decision.clone(),
                            provider_started_at,
                            provider_name: provider_name.to_string(),
                            model_name: model_name.to_string(),
                            history_len_before_user_turn,
                            compaction_epoch_at_start,
                            assignment: assignment_guard.take(),
                        },
                    )
                    .is_some()
                {
                    tracing::warn!(
                        task_id = crate::chat::turn_scheduler::TurnTaskId::get(task_id),
                        "replaced pending Redux turn context before completion"
                    );
                }
                continue;
            }

            // Shutdown preemption guard against a round 2 hang.
            let mut turn_input_open = true;
            let mut provider_completion_event: Option<ProviderTurnCompletionEvent> =
                provider_turn_task_id.and_then(|id| pending_provider_completion_events.remove(&id));
            if provider_completion_event.is_none() {
                loop {
                    tokio::select! {
                        () = async {
                            if let Some(fut) = notify_fut.as_mut() {
                                fut.await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => break,
                        completion = provider_completion_rx.recv(), if provider_turn_task_id.is_some() => {
                            let Some(completion) = completion else {
                                break;
                            };
                            // the completion of the current turn; `None`: retained as pending
                            if let Some(completion) = route_provider_completion_event_and_publish(
                                &mut provider_turn_lifecycle_rx,
                                &mut provider_turn_lifecycle_events_open,
                                &mut provider_turn_workers,
                                &mut pending_provider_completion_events,
                                provider_turn_task_id,
                                completion,
                                &chat_dispatcher,
                            ) {
                                provider_completion_event = Some(completion);
                                break;
                            }
                        }
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
                        }
                        () = shutdown.cancelled() => {
                            tracing::debug!("Redux driver: shutdown.cancelled before turn complete");
                            break;
                        }
                        msg = input_rx.recv(), if turn_input_open => {
                            let Some(msg) = msg else {
                                turn_input_open = false;
                                continue;
                            };
                            let daemon_control = DaemonControl {
                                config: &config,
                                dispatcher: &chat_dispatcher,
                                redraw_tx: redraw_tx_for_main.as_ref(),
                            };
                            let queued_from = input_backlog.len();
                            process_active_turn_input_batch(
                                msg,
                                &mut emit_chat_output,
                                &mut input_rx,
                                &mut input_backlog,
                                &mut turn_scheduler,
                                &mut provider_turn_workers,
                                provider_turn_task_id,
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
                        }
                    }
                }
            }

            let resolved_completion =
                resolve_provider_turn_completion(&turn_signal, provider_turn_task_id, provider_completion_event);
            let completion_history_len = take_provider_turn_completion_history_len(
                &mut provider_turn_completion_contexts,
                provider_turn_task_id,
                history.len(),
            );
            let terminal_plan = provider_turn_terminal_plan_from_completion(
                resolved_completion,
                completion_history_len,
                &tools_registry,
            );
            enqueue_provider_turn_finalizer_event(
                &mut provider_turn_finalizer_events,
                provider_turn_task_id,
                terminal_plan.clone(),
            );
            let finalizer_result = drain_provider_turn_finalizer_events_and_publish(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                &mut provider_turn_finalizer_events,
                &chat_dispatcher,
            )
            .into_iter()
            .rev()
            .find(|result| result.task_id == provider_turn_task_id)
            .unwrap_or(ProviderTurnFinalizerResult {
                task_id: provider_turn_task_id,
                terminal_status: "unknown",
                finalized: false,
            });

            // Finalize streaming (aligned with the legacy teardown): drop the senders so the
            // background tasks wind down.
            //
            // S2-B Step 3: on the driver path the reducer already cleared
            // `state.control.active_cancel` when it received StreamCompleted/Failed/Cancelled; the
            // legacy Arc is only a fallback under Off/Both.
            #[cfg(not(feature = "terminal-tui"))]
            {
                *active_cancel.lock() = None;
            }
            drop(delta_tx);
            drop(tool_event_tx);
            if let Some(handle) = draft_updater {
                let _ = handle.await;
            }
            let _ = tool_event_forwarder.await;

            match terminal_plan {
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    recorded_response,
                    empty_response,
                    usage: redux_tokens_used,
                    history_commit_len,
                    ..
                } => {
                    if empty_response {
                        let provider_outcome = if let Some(binding) = chat_turn_binding.as_ref() {
                            binding.provider_outcome(&route_decision, provider_started_at)?
                        } else { ProviderExecutionOutcome::success_for_decision_with_usage(
                            &route_decision, provider_started_at, redux_tokens_used.clone(),
                        ) };
                        if !finalizer_result.finalized {
                            if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                                tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Redux driver: cancel empty-response draft failed");
                            }
                            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                            continue;
                        }
                        if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                            tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Redux driver: cancel empty-response draft failed");
                        }
                        let terminal_usage = match finalize_chat_success_turn(
                            chat_turn_binding.as_deref(),
                            &memory_fabric,
                            &chat_session_key,
                            &turn_run_id,
                            provider_name,
                            model_name,
                            route_scope.clone(),
                            provider_outcome.clone(),
                            None,
                            history_commit_len,
                            "redux driver completed with empty response",
                            &config.cost,
                            &config.workspace_dir,
                        )
                        .await
                        {
                            Ok(receipt) => receipt.into_session_usage(),
                            Err(error) => {
                                tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "Chat terminal persistence is unconfirmed");
                                return Err(error);
                            }
                        };
                        if let Err(error) = dispatch_final_usage(
                            &chat_dispatcher,
                            &turn_signal,
                            &mut turn_scheduler,
                            provider_turn_task_id,
                            terminal_usage,
                            "chat.provider_usage_recorded_empty_response",
                        )
                        .await
                        {
                            anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
                        }
                        #[cfg(feature = "terminal-tui")]
                        if let Some(tx) = redraw_tx_for_main.as_ref() {
                            let _ = tx.try_send(());
                        }
                        surface_turn_elapsed_message(
                            &chat_dispatcher,
                            sessions_redraw_handle.as_ref(),
                            "completed",
                            provider_outcome.started_at,
                            provider_outcome.finished_at,
                        );
                        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        continue;
                    }
                    let provider_outcome = if let Some(binding) = chat_turn_binding.as_ref() {
                        binding.provider_outcome(&route_decision, provider_started_at)?
                    } else { ProviderExecutionOutcome::success_for_decision_with_usage(
                        &route_decision, provider_started_at, redux_tokens_used.clone(),
                    ) };
                    if !finalizer_result.finalized {
                        if let Err(e) = terminal.cancel_draft("user", &d_id).await {
                            tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Redux driver: cancel commit-gated draft failed");
                        }
                        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                        continue;
                    }
                    // 1) Write the final text accumulated by the driver's stream back into the LLM
                    //    history (aligned with the legacy tail
                    //    `history.push(ChatMessage::assistant(...))`).
                    history.push(committed_reply_message(&chat_dispatcher.session_reader().acked(), &final_text));
                    // 2) finalize_draft: deliver the text to the terminal channel so the user sees
                    //    it (the driver path does not go through the delta_tx → draft_updater chain,
                    //    it finalizes directly).
                    if let Err(e) = terminal.finalize_draft("user", &d_id, &final_text).await {
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "Redux driver: finalize_draft failed");
                    }
                    let terminal_usage = match finalize_chat_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &chat_session_key,
                        &turn_run_id,
                        provider_name,
                        model_name,
                        route_scope.clone(),
                        provider_outcome.clone(),
                        Some(recorded_response.clone()),
                        history_commit_len,
                        "redux driver completed",
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        Ok(receipt) => receipt.into_session_usage(),
                        Err(error) => {
                            tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "Chat terminal persistence is unconfirmed");
                            return Err(error);
                        }
                    };
                    let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
                    crate::runtime::control_ladder::append_provider_outcome_trace(
                        std::path::Path::new(&config.workspace_dir),
                        &provider_outcome.decision_id,
                        &provider_outcome.final_provider,
                        &provider_outcome.final_model,
                        attempts_count,
                        "success",
                    );
                    // On this driver path the worker's sink already sent
                    // RecordAssistantTurn + StreamCompleted (fixB B5); the
                    // reducer owns the turn, and the slash commands read its
                    // acknowledged snapshot.
                    if let Err(error) = dispatch_final_usage(
                        &chat_dispatcher,
                        &turn_signal,
                        &mut turn_scheduler,
                        provider_turn_task_id,
                        terminal_usage,
                        "chat.provider_usage_recorded",
                    )
                    .await
                    {
                        anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
                    }
                    #[cfg(feature = "terminal-tui")]
                    if let Some(tx) = redraw_tx_for_main.as_ref() {
                        let _ = tx.try_send(());
                    }
                    surface_turn_elapsed_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        "completed",
                        provider_outcome.started_at,
                        provider_outcome.finished_at,
                    );
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                    let _ = final_text;
                }
                ProviderTurnTerminalPlan::Failed { err, .. } => {
                    if let Err(error) = finalize_chat_non_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &turn_run_id,
                        route_scope.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Failed,
                        None,
                        provider_started_at,
                        &format!("redux driver failed: {err}"),
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "Failed to commit shared failed Chat terminal event");
                    }
                    // The worker's compactions and terminal precede this settlement on the
                    // action channel; the reducer never recorded a driver turn's request.
                    let settled = settle_aborted_turn(&chat_dispatcher, &turn_signal, None).await?;
                    provider_view_after_settled_abort(&mut history, &settled, compaction_epoch_at_start, false, None);
                    // The reducer's NotifyHook(Error) has already been sent; do not hooks.emit here
                    // again, to avoid duplicates.
                    #[cfg(feature = "terminal-tui")]
                    let interactive_tui_active = redraw_tx_for_main.is_some();
                    #[cfg(not(feature = "terminal-tui"))]
                    let interactive_tui_active = false;

                    if !interactive_tui_active && let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                    if !interactive_tui_active {
                        super::output::print_turn_failure(plain_mode, &err);
                    }
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    surface_turn_elapsed_message(
                        &chat_dispatcher,
                        sessions_redraw_handle.as_ref(),
                        "failed",
                        provider_started_at,
                        chrono::Utc::now(),
                    );
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                }
                ProviderTurnTerminalPlan::Cancelled { .. } => {
                    if let Err(error) = finalize_chat_non_success_turn(
                        chat_turn_binding.as_deref(),
                        &memory_fabric,
                        &turn_run_id,
                        route_scope.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Cancelled,
                        None,
                        provider_started_at,
                        "redux driver cancelled",
                        &config.cost,
                        &config.workspace_dir,
                    )
                    .await
                    {
                        if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "Failed to commit shared cancelled Chat terminal event");
                    }
                    if let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                    let settled = settle_aborted_turn(&chat_dispatcher, &turn_signal, None).await?;
                    provider_view_after_settled_abort(
                        &mut history,
                        &settled,
                        compaction_epoch_at_start,
                        false,
                        Some(history_len_before_user_turn),
                    );
                    publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                    publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                }
            }

            continue;
        }

        // The compaction epoch before the preflight: a compaction adopted by
        // the preflight or during the turn moves it, and every turn end then
        // rebuilds the provider view from the acknowledged session.
        let inline_compaction_epoch_before_preflight = admitted_session.compaction_epoch;
        let legacy_budget_config = tool_surface_reserve.reserved_config(&effective_compaction.config);
        let preflight_budget = crate::agent::context_budget::plan_context_budget(
            &history,
            &legacy_budget_config,
            crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
        );
        if preflight_budget.over_hard_limit {
            let audit_source_history = original_legacy_chat_compaction_audit_source(persisted_history_for_turn.messages());
            let summary_projection = bounded_legacy_chat_compaction_audit_source(persisted_history_for_turn.messages());
            let token_metadata = legacy_compaction_token_metadata(&history, persisted_history_for_turn.messages());
            let (turns_before, tokens_before) = super::compaction::chat_history_turns_and_tokens(&history);
            // The request is already recorded (`UserTurnAdmitted` above), so the
            // source is the persisted history including it and the reducer gets
            // the same patch. The enriched request replaces the source's plain
            // final message in the provider view.
            let enriched_request = history.last().filter(|message| message.role == "user").cloned();
            let preflight_outcome = if let Some(patch) = build_chat_compaction_patch(
                &history,
                &persisted_history_for_turn,
                provider.as_ref(),
                model_name,
                &legacy_budget_config,
                memory_runtime.ingest(),
                "chat_preflight",
            )
            .await
            {
                apply_chat_compaction_patch_and_sync(
                    &mut history,
                    &persisted_history_for_turn,
                    enriched_request,
                    patch,
                    &legacy_budget_config,
                    crate::chat::action::CompactReason::ContextOverflow,
                    &chat_dispatcher,
                    &turn_signal,
                )
                .await
            } else {
                // No handoff or summary: the session and this view stay as they
                // are; the turn's own preflight trims each request (a projection
                // that is never persisted).
                super::compaction::ManualCompactOutcome::NoDurableCompaction
            };
            persist_legacy_chat_compaction_audit(
                mem.as_ref(),
                &runtime_envelope,
                &audit_source_history,
                &summary_projection,
                token_metadata,
                "chat_preflight",
            )
            .await;
            #[cfg(feature = "terminal-tui")]
            refresh_context_budget_for_tui(
                &history,
                &effective_compaction.config,
                &tool_surface_reserve,
                redraw_tx_for_main.is_some(),
                &chat_mirror,
                &chat_dispatcher,
            );
            let compact_msg = preflight_outcome.feedback(turns_before, tokens_before, &legacy_budget_config);
            surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), &compact_msg);
            tracing::warn!(
                before_used_tokens = preflight_budget.used_tokens,
                hard_limit = preflight_budget.available_input_tokens,
                "legacy chat context budget preflight compacted before provider call"
            );
        }

        // The inline loop hands its in-turn compactions to the reducer instead
        // of keeping them as private mutations of `history`; after the turn the
        // view follows the acknowledged session (`reconcile_inline_turn`). The
        // reducer history it guards against holds the recorded request once:
        // after an adopted preflight the acknowledged snapshot already holds
        // it, identified by this turn's admission, and it keeps its `Request`
        // origin, so an in-turn compaction that keeps it records it once.
        let inline_compaction = {
            let reducer_source = super::session_store::canonical_history_for_admitted_turn(
                &chat_session.acked(),
                &system_prompt,
                admission,
                &user_input,
            );
            super::compaction::InlineCompactionSink::new(chat_dispatcher.clone(), turn_signal.clone(), &user_input, reducer_source)
        };
        // The inline turn's canonical history: the acknowledged turns plus the
        // plain request, never the enriched provider view (r6-2).
        let inline_canonical = inline_compaction.reducer_source();

        // D8-4: seed a turn-root spawn execution context so a sub-agent spawned
        // directly from this chat turn inherits parent_run_id = the per-turn
        // run_id. spawn_depth starts at 0 and is_turn_root keeps the first child's
        // reported depth at 0 (a turn is not itself a spawn). A spawned child records
        // under its own `spawn:<run_id>` key, never under this chat session key.
        let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            turn_run_id.clone(),
            chat_session_key.clone(),
        );
        let turn_message_send_ctx = crate::tools::message_send::MessageSendExecutionContext::new(
            Some("user".to_string()),
            Arc::clone(&terminal) as Arc<dyn Channel>,
        );

        // The view this turn starts from; a turn that changes it other than by
        // appending (a compaction, a projection trim, a middleware rewrite)
        // rebuilds it from the session when it ends.
        let inline_view_before_turn = super::compaction::ViewPrefix::of(&history);
        // Keep the large trace inline outside the small outcome discriminator.
        let (turn_outcome, successful_trace) = {
            #[cfg(feature = "terminal-tui")]
            {
                history_len_before_tools = history.len();
            }

            let result = crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT
                .scope(
                    turn_message_send_ctx.clone(),
                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(turn_spawn_ctx.clone(), async {
                        run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                advice: admitted_binding.decision_advice(),
                                channel_name: "terminal",
                                scope: Some(&scope_ctx),
                                mode: admitted_binding.admitted_model_mode()?,
                                silent: false,
                                expose_stay_silent: false,
                                multimodal: &config.multimodal,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
                                    priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
                                    low_priority_tool_names: config.agent.low_priority_tools.clone(),
                                },
                                compaction: Some(&effective_compaction.config),
                                decision: admitted_binding.decision_routing()
                                    .ok_or_else(|| anyhow::anyhow!("chat runtime turn requires an admitted decision"))?,
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: provider.as_ref(),
                                provider_name,
                                model: model_name,
                                temperature,
                                tools_registry: Arc::clone(&tools_registry),
                                observer: observer.as_ref(),
                                hooks: &hooks,
                                approval: Some(Arc::clone(&approval_manager)),
                                approval_resolver: None,
                                // Supervised calls ask on the terminal (CR-2), as on the reducer path.
                                runtime_adapter: chat_turn_binding.as_ref().map(|binding| crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                                    live_frames: None,
                                    guarded_delivery: true,
                                    provider_trace: binding.retained_provider_trace(),
                                    events: Some(Arc::clone(&inline_compaction) as Arc<dyn crate::agent::turn_contracts::ToolLoopEventSink>),
                                    stream_provider: false,
                                    tool_execution_service: Some(Arc::new(super::line_approval::line_tool_execution_service(
                                        Arc::clone(&tools_registry), &mem, &security,
                                        Arc::clone(&approval_manager), cancellation.clone(),
                                        Arc::new(super::terminal_approval::ControllingTerminal),
                                    ))),
                                    tool_execution_context: super::approval_context::line_mode(binding.envelope().clone())
                                        .with_approval_ledger(binding.h2_approval_ledger()),
                                    allowed_tool_names: None,
                                }),
                                memory: memory_runtime.clone().with_canonical_history(inline_canonical.clone(), false),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: Some(cancellation.clone()),
                                on_delta: Some(delta_tx.clone()),
                                on_tool_call: Some(tool_event_tx.clone()),
                            },
                        )
                        .await
                        .map(|(outcome, trace)| (outcome.into_text(), trace))
                    }),
                )
                .await;

            match result {
                // ── Success ───────────────────────────────────────
                Ok((resp, trace)) => (TurnOutcome::Success(resp), Some(trace)),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Err(ref e) if is_tool_loop_cancelled(e) || cancellation.is_cancelled() => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    // S2-A: user-driven cancel — distinguished from real
                    // failures so the reducer emits `StreamCancelled` (no
                    // Error hook fan-out) instead of `StreamFailed`.
                    (TurnOutcome::Cancelled, None)
                }
                // ── Other errors ──────────────────────────────────
                Err(e) => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    // The one-shot reads this note: a send-admission refusal
                    // exits 2 with its reply, any other failure exits 1.
                    one_shot::note_turn_failure(chat_turn_binding.as_deref(), &e);
                    let err_text = e.to_string();
                    super::output::print_turn_failure(plain_mode, &err_text);
                    if plain_mode {
                        plain_mode_turn_failed = true;
                    }
                    // Phase E (5a-4): same dual_write_guard reasoning as the timeout branch.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", &err_text))
                            .await;
                    }
                    // S2-A: generic provider/loop error — retryable hint is
                    // false (the caller already chose to surface and continue;
                    // retry policy is owned by upstream once tooling lands).
                    (
                        TurnOutcome::FailedWithError {
                            err: err_text,
                            retryable: false,
                        },
                        None,
                    )
                }
            }
        };

        // ── Finalize streaming ────────────────────────────────────
        // Deregister this turn's cancellation token.
        //
        // S2-B Step 3: on the legacy path the reducer also clears `state.control.active_cancel` when
        // Stream{Completed,Cancelled,Failed} is dispatched (lines 1886-1911 below); the legacy Arc is
        // only a fallback in Off/Both mode (the external Ctrl+C handler reads it).
        #[cfg(not(feature = "terminal-tui"))]
        {
            *active_cancel.lock() = None;
        }

        // Drop our channel senders so background tasks receive channel close
        drop(delta_tx);
        drop(tool_event_tx);
        if let Some(handle) = draft_updater {
            let _ = handle.await;
        }
        let _ = tool_event_forwarder.await;

        // Step 5b dual write: dispatch the matching stream-termination Action for the turn outcome.
        // Only when draft_id exists can the reducer match stream.draft and clean it up; otherwise this
        // is a no-op.
        //
        // S2-A: split the previous single-pronged "Failed → StreamCancelled"
        // fallback. Order is **critical**: cancellation is detected up the
        // stack via `is_tool_loop_cancelled` / `cancellation.is_cancelled()`
        // and surfaces as `TurnOutcome::Cancelled`. Real errors (timeout /
        // context overflow / provider error) surface as `FailedWithError` and
        // map to `StreamFailed { err, retryable }` so the reducer emits the
        // `NotifyHook(Error) + LogTrace + RequestRedraw` effect chain.
        //
        // T3-3-fixA P0-1: the StreamCompleted of the Success branch was moved down to be dispatched
        // after RecordAssistantTurn, so that when the reducer builds the SaveSession snapshot
        // session.turns already contains this turn's assistant message. Cancelled / FailedWithError
        // write no assistant turn, so their dispatch position is unchanged.
        match &turn_outcome {
            TurnOutcome::Success(..) => {}
            TurnOutcome::Cancelled => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled",
                    );
                }
                if let Err(error) = finalize_chat_non_success_turn(
                    chat_turn_binding.as_deref(),
                    &memory_fabric,
                    &turn_run_id,
                    route_scope.clone(),
                    crate::agent::terminal::TurnTerminalStatus::Cancelled,
                    None,
                    provider_started_at,
                    "legacy tool loop cancelled",
                    &config.cost,
                    &config.workspace_dir,
                )
                .await
                {
                    if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                    tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "Failed to commit shared cancelled Chat terminal event");
                }
            }
            TurnOutcome::FailedWithError { err, retryable } => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamFailed {
                            draft_id: d_id.clone(),
                            err: err.clone(),
                            retryable: *retryable,
                        },
                        "chat.stream_failed",
                    );
                }
                // The terminal records the loop failure. Decided turns retain
                // only observed provider attempts in their owning trace.
                if let Err(error) = finalize_chat_non_success_turn(
                    chat_turn_binding.as_deref(),
                    &memory_fabric,
                    &turn_run_id,
                    route_scope.clone(),
                    crate::agent::terminal::TurnTerminalStatus::Failed,
                    None,
                    provider_started_at,
                    &format!("legacy tool loop failed: {err}"),
                    &config.cost,
                    &config.workspace_dir,
                )
                .await
                {
                    if chat_turn_binding.as_ref().is_some_and(|binding| binding.is_decided()) { return Err(error); }
                    tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "Failed to commit shared failed Chat terminal event");
                }
                surface_turn_elapsed_message(
                    &chat_dispatcher,
                    sessions_redraw_handle.as_ref(),
                    "failed",
                    provider_started_at,
                    chrono::Utc::now(),
                );
            }
        }

        let inline_view_changed = inline_compaction.compacted() || inline_view_before_turn.changed_in(&history);
        // If the turn failed or was cancelled, skip response processing
        let (response, turn_trace) = match turn_outcome {
            TurnOutcome::Success(resp) => (resp, successful_trace.context("successful Chat outcome has no serving trace")?),
            TurnOutcome::Cancelled => {
                let settled = settle_aborted_turn(&chat_dispatcher, &turn_signal, admission).await?;
                provider_view_after_settled_abort(
                    &mut history,
                    &settled,
                    inline_compaction_epoch_before_preflight,
                    inline_view_changed,
                    Some(history_len_before_user_turn),
                );
                finish_inline_provider_turn(&mut provider_turn_workers, provider_turn_task_id, None);
                gate_cancelled_provider_turn_finalization(
                    &mut turn_scheduler,
                    &mut history_commit_coordinator,
                    &mut provider_turn_workers,
                    provider_turn_task_id,
                    "legacy tool loop cancelled",
                );
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            }
            TurnOutcome::FailedWithError { err, .. } => {
                let settled = settle_aborted_turn(&chat_dispatcher, &turn_signal, admission).await?;
                provider_view_after_settled_abort(
                    &mut history,
                    &settled,
                    inline_compaction_epoch_before_preflight,
                    inline_view_changed,
                    Some(history_len_before_user_turn),
                );
                finish_inline_provider_turn(&mut provider_turn_workers, provider_turn_task_id, None);
                gate_failed_provider_turn_finalization(
                    &mut turn_scheduler,
                    &mut history_commit_coordinator,
                    &mut provider_turn_workers,
                    provider_turn_task_id,
                    history.len(),
                    format!("legacy tool loop failed: {err}"),
                );
                publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                continue;
            }
        };
        if let Some(binding) = chat_turn_binding.as_ref() {
            binding.capture_trace(&turn_trace)?;
        }
        // FIX-P0-30/31: build the provider outcome from the loop's real
        // attribution trace. When the trace carries the actual serving
        // provider/model + attempts (the `ReliableProvider` path), use them so a
        // retry/fallback is recorded as `FallbackSuccess` and `final_model`
        // reflects what truly executed. Fall back to the routed
        // `decision.selected.{provider,model}` only when no trace is available
        // (e.g. a provider whose `chat_traced` default produced a synthetic
        // attribution).
        let provider_outcome = {
            let has_trace = turn_trace.final_model.is_some() && !turn_trace.attempts.is_empty();
            if has_trace {
                let final_provider = turn_trace
                    .final_provider
                    .unwrap_or_else(|| route_decision.selected.provider.clone());
                let final_model = turn_trace
                    .final_model
                    .unwrap_or_else(|| route_decision.selected.model.clone());
                ProviderExecutionOutcome::from_trace_with_usage(
                    &route_decision,
                    turn_trace.attempts,
                    final_provider,
                    final_model,
                    provider_started_at,
                    chrono::Utc::now(),
                    // FIX #2: a fallback on any earlier (tool-call) turn must
                    // surface as FallbackSuccess even when the final turn is clean.
                    turn_trace.any_turn_had_fallback,
                    turn_trace.tokens_used,
                )
            } else {
                ProviderExecutionOutcome::success_for_decision_with_usage(
                    &route_decision,
                    provider_started_at,
                    turn_trace.tokens_used,
                )
            }
        };
        surface_turn_elapsed_message(
            &chat_dispatcher,
            sessions_redraw_handle.as_ref(),
            "completed",
            provider_outcome.started_at,
            provider_outcome.finished_at,
        );
        // d04 §10 G7: emit a control-ladder trace carrying the structured
        // decision_id / final_provider / final_model / attempts_count so a
        // `decision_id` join links the routing decision to the provider that
        // actually served the request. Best-effort (failures logged, not fatal).
        {
            let status_label = match &provider_outcome.status {
                crate::llm::route_decision::ExecutionStatus::Success => "success",
                crate::llm::route_decision::ExecutionStatus::FallbackSuccess => "fallback_success",
                crate::llm::route_decision::ExecutionStatus::AllFailed { .. } => "all_failed",
            };
            let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
            crate::runtime::control_ladder::append_provider_outcome_trace(
                std::path::Path::new(&config.workspace_dir),
                &provider_outcome.decision_id,
                &provider_outcome.final_provider,
                &provider_outcome.final_model,
                attempts_count,
                status_label,
            );
        }

        // ── P2-12: Mirror reasoning content into the TUI as a folded card. ──
        //
        // Reasoning is separated upstream at the provider layer (P0-2) and
        // persisted into the assistant-history JSON via
        // `build_native_assistant_history`. We scan the slice of history
        // produced during this turn for `reasoning_content` fields, aggregate
        // them, and push a single folded `Reasoning` card to the TUI mirror.
        // Empty buffers are skipped by `push_reasoning`. This does NOT touch
        // the visible delta stream — the user-facing assistant text remains
        // the only thing rendered in the streaming draft.
        #[cfg(feature = "terminal-tui")]
        {
            let turn_slice = history.get(history_len_before_tools..).unwrap_or(&[]);
            let aggregated = collect_reasoning_from_history_slice(turn_slice);
            if !aggregated.is_empty() {
                // S4-B: the mirror push_reasoning is deleted; the reducer
                // (reduce_stream_completed) is the single source that pushes the Reasoning card.
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
        }

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

        if crate::agent::response_guard::is_empty_assistant_response(&response, false) {
            let terminal_usage = match finalize_chat_success_turn(
                chat_turn_binding.as_deref(),
                &memory_fabric,
                &chat_session_key,
                &turn_run_id,
                provider_name,
                model_name,
                route_scope.clone(),
                provider_outcome.clone(),
                None,
                history.len(),
                "legacy tool loop completed with empty response",
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                Ok(receipt) => receipt.into_session_usage(),
                Err(error) => {
                    tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "Chat terminal persistence is unconfirmed");
                    return Err(error);
                }
            };
            let inline_payload = inline_provider_turn_payload(history.len(), 0, terminal_usage.as_ref());
            // Usage first (inside the commit's acknowledged revision), then
            // the awaited commit (the user turn was recorded at admission).
            if let Err(error) = dispatch_final_usage(
                &chat_dispatcher,
                &turn_signal,
                &mut turn_scheduler,
                provider_turn_task_id,
                terminal_usage,
                "chat.provider_usage_recorded_empty_response",
            )
            .await
            {
                anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
            }
            let Some(commit_task_id) = provider_turn_task_id else {
                anyhow::bail!("chat turn has no provider task; its history cannot be committed");
            };
            if let Err(error) = dispatch_ordered_provider_turn_commit_and_wait(
                &chat_dispatcher,
                &turn_signal,
                commit_task_id,
                OrderedCommitTerminal::TurnCommitRequested { draft_id: draft_id.as_deref() },
                &user_input,
                "",
                true,
                false,
            )
            .await
            {
                anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
            }
            super::compaction::reconcile_inline_turn(&mut history, &chat_dispatcher);
            if let Some(ref d_id) = draft_id
                && let Err(e) = terminal.cancel_draft("user", d_id).await
            {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&e);
                tracing::debug!(error_len, error_sha256, "cancel empty-response draft failed");
            }
            surface_session_message(
                &chat_dispatcher,
                sessions_redraw_handle.as_ref(),
                crate::agent::response_guard::EMPTY_ASSISTANT_RESPONSE_MESSAGE,
            );
            #[cfg(feature = "terminal-tui")]
            if let Some(tx) = redraw_tx_for_main.as_ref() {
                let _ = tx.try_send(());
            }
            observer.record_event(&ObserverEvent::TurnComplete);
            if !dual_write_guard.is_active() {
                hooks
                    .emit(
                        HookEvent::TurnComplete,
                        serde_json::json!({
                            "mode": "chat",
                            "response_chars": 0,
                        }),
                    )
                    .await;
            }
            finish_inline_provider_turn(&mut provider_turn_workers, provider_turn_task_id, Some(inline_payload));
            mark_provider_turn_completed(
                &mut turn_scheduler,
                &mut history_commit_coordinator,
                &mut provider_turn_workers,
                provider_turn_task_id,
                history.len(),
                "legacy tool loop completed with empty response",
            );
            publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
            publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
            continue;
        }

        // Persist only the user-facing assistant response. Tool results already
        // live in structured history entries; adding a synthetic summary here
        // can teach the model to emit that internal marker as a final answer.
        // The loop already appended its final reply: that slot becomes the
        // committed (sanitized) response, so the next request carries the
        // reply once, as the saved session does.
        match history.last_mut() {
            Some(last) if last.role == "assistant" => *last = ChatMessage::assistant(&response),
            _ => history.push(ChatMessage::assistant(&response)),
        }
        let terminal_usage = match finalize_chat_success_turn(
            chat_turn_binding.as_deref(),
            &memory_fabric,
            &chat_session_key,
            &turn_run_id,
            provider_name,
            model_name,
            route_scope.clone(),
            provider_outcome.clone(),
            Some(response.clone()),
            history.len(),
            "legacy tool loop completed",
            &config.cost,
            &config.workspace_dir,
        )
        .await
        {
            Ok(receipt) => receipt.into_session_usage(),
            Err(error) => {
                tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "Chat terminal persistence is unconfirmed");
                return Err(error);
            }
        };


        // S2-B Step 4: RecordAssistantTurn(response) is dispatched at the same point as the legacy
        // `history.push(ChatMessage::assistant(...))` — so the reducer's session.history is
        // byte-for-byte aligned with the legacy history. The old dispatch at line 2055 below used
        // sanitized_response, whose content differs from history.push — from S2-B Step 4 on it is
        // dispatched here with response instead, and the old dispatch below is deleted.
        // The final usage goes first so the commit's acknowledged revision
        // carries it; the ordered commit then sends RecordAssistantTurn and
        // the awaited TurnCommitRequested (the user turn was recorded at
        // admission).
        let inline_payload =
            inline_provider_turn_payload(history.len(), response.chars().count(), terminal_usage.as_ref());
        if let Err(error) = dispatch_final_usage(
            &chat_dispatcher,
            &turn_signal,
            &mut turn_scheduler,
            provider_turn_task_id,
            terminal_usage,
            "chat.provider_usage_recorded",
        )
        .await
        {
            anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
        }
        let Some(commit_task_id) = provider_turn_task_id else {
            anyhow::bail!("chat turn has no provider task; its history cannot be committed");
        };
        if let Err(error) = dispatch_ordered_provider_turn_commit_and_wait(
            &chat_dispatcher,
            &turn_signal,
            commit_task_id,
            OrderedCommitTerminal::TurnCommitRequested { draft_id: draft_id.as_deref() },
            &user_input,
            &response,
            false,
            false,
        )
        .await
        {
            anyhow::bail!("Chat stopped after an unconfirmed history commit: {error}");
        }
        super::compaction::reconcile_inline_turn(&mut history, &chat_dispatcher);
        #[cfg(feature = "terminal-tui")]
        if let Some(tx) = redraw_tx_for_main.as_ref() {
            let _ = tx.try_send(());
        }

        // ── Extract and display media markers (images, documents, etc.) ──
        let (clean_response, media_items) = extract_outgoing_media(&response)?;
        for (media_type, path) in &media_items {
            if media_type == "IMAGE" && std::path::Path::new(path).exists() {
                if let Err(e) = terminal_proto::display_image(path) {
                    tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "Image display failed");
                    println!("  [image: {path}]");
                }
            } else {
                println!("  [{media_type}: {path}]");
            }
        }
        // Use the cleaned response (media markers removed) for display
        let display_response = if media_items.is_empty() {
            &response
        } else {
            &clean_response
        };

        // Finalize the streaming draft with the full response.
        //
        // Idempotency contract (P1-4):
        //   When a draft exists, the streamed deltas have already painted the
        //   full response on screen via `update_draft`. `finalize_draft` is
        //   only a structural close (locking in the final text for non-TTY
        //   channels such as Telegram). On failure we therefore MUST NOT
        //   re-send the whole message — that would duplicate output that the
        //   user has already seen via the live stream. We log a warning and
        //   carry on; the assistant turn is already in `history`, so the
        //   conversation state remains consistent.
        //
        //   The "no draft" branch is the genuine first-send path (drafts were
        //   never created, e.g. `send_draft` failed earlier and returned
        //   `None`), so a normal `send` is correct and not a duplicate.
        if let Some(ref d_id) = draft_id {
            if let Err(e) = terminal.finalize_draft("user", d_id, display_response).await {
                if should_resend_on_finalize_failure(true) {
                    let rendered = render_response(display_response);
                    let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
                } else {
                    tracing::warn!(
                        error = %crate::agent::diagnostics::error_class(&e),
                        "finalize_draft failed; suppressing resend to preserve idempotency (user already saw streamed content)"
                    );
                }
            }
        } else {
            // No draft was created — send as a complete message with highlighting.
            let rendered = render_response(display_response);
            let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
        }

        // P0-1 fix: in Both/Redux mode the old path is gated by dual_write_guard.
        // The Redux reducer's SaveSession effect already raised the guard inside execute_real, and
        // when the guard is active the old path skips save_session + hooks.emit(TurnComplete), which
        // prevents double hook/webhook firing (hooks/webhooks are not idempotent and really would
        // fire twice).
        // In Off mode the guard is always false and the old path writes alone as usual.
        // The guard is turn-level (not effect-level): once Redux has executed either
        // SaveSession or NotifyHook during a turn, the guard is active and every later write of the
        // old path is suppressed.
        if !dual_write_guard.is_active() {
            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "chat",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;
        } else {
            // Guard active: the Redux path already handled hooks.emit.
            tracing::debug!("P0-1: dual_write_guard active — legacy path skipping hooks.emit(TurnComplete)");
            // observer.record_event is still called (the observer is only a local counter, with no
            // external side effects).
            observer.record_event(&ObserverEvent::TurnComplete);
        }
        finish_inline_provider_turn(&mut provider_turn_workers, provider_turn_task_id, Some(inline_payload));
        mark_provider_turn_completed(
            &mut turn_scheduler,
            &mut history_commit_coordinator,
            &mut provider_turn_workers,
            provider_turn_task_id,
            history.len(),
            "legacy tool loop completed",
        );
        publish_main_queue_status(&chat_dispatcher, &turn_scheduler);
        publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
        // The reply this turn produced *is* the answer to the assignment, so it
        // is what goes back. Reaching here at all is what distinguishes a turn
        // that answered from every other way the body can end.
        if let Some(guard) = assignment_guard.as_mut() {
            guard.completed(&response);
        }}

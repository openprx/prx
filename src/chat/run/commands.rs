{
        // A one-shot message is literal model input (D4): it skips every
        // quit, copy, queue, slash and bang command below.
        if one_shot.is_none() {
        // Handle /quit and /exit immediately
        if is_chat_quit_command(user_input.as_str()) {
            break 'chat;
        }

        if is_copy_command(&user_input) {
            match select_copy_content(&chat_session.session(), &user_input) {
                Ok(selection) if plain_mode => {
                    println!("{}", selection.content);
                    let _ = std::io::stdout().flush();
                }
                Ok(selection) => match terminal_proto::copy_to_clipboard(&selection.content) {
                    Ok(()) => emit_chat_output(&copy_success_message(&selection)),
                    Err(error) => emit_chat_output(&format!("Copy failed: {error}")),
                },
                Err(message) => emit_chat_output(&message),
            }
            continue;
        }

        if is_queue_command(&user_input) {
            emit_chat_output(&format_input_backlog_report(&input_backlog, &turn_scheduler, 8));
            continue;
        }

        if is_workers_command(&user_input) {
            if let Some((output, signal_cancel)) = provider_workers_cancel_output_for_input(
                &msg,
                &user_input,
                &mut turn_scheduler,
                &mut provider_turn_workers,
                None,
            ) {
                emit_chat_output(&output);
                publish_provider_worker_status(&chat_dispatcher, &provider_turn_workers);
                dispatch_provider_worker_cancel_signal(&chat_dispatcher, signal_cancel);
            } else {
                emit_chat_output(&format_provider_worker_report(&provider_worker_status(
                    &provider_turn_workers,
                )));
            }
            continue;
        }

        // BUG-07: `/model <name>` switches the model online (same provider, different model).
        //
        // Intercepted before commands::dispatch, because taking effect requires (a) rewriting the main
        // loop's `current_model_owned` (which affects the system prompt / event records of later
        // turns), (b) writing the hot-swap slot of EffectDeps (which affects the model the dispatcher
        // subtask actually requests on the next turn), and (c) dispatching ModelChanged so the reducer
        // updates session.model → the status bar reflects it immediately. A bare `/model` (no
        // argument) is still handed to dispatch, which shows the current model.
        if user_input == "/model save-default" {
            let saved = super::model_commands::save_default(&config, provider_name, &current_model_owned, &provider_runtime_options).await;
            match saved {
                Ok(()) => emit_chat_output("Saved the current model and level as the default."),
                Err(error) => emit_chat_output(&format!("Cannot save model default: {}", providers::sanitize_api_error(&error.to_string()))),
            }
            continue;
        }
        let model_command = user_input.strip_prefix("/model ");
        let level_command = user_input.strip_prefix("/level ");
        if model_command.is_some() || level_command.is_some() {
            let (new_model, candidate_options) = match super::model_commands::selection(&user_input, &current_model_owned, provider_name, &provider_runtime_options) {
                Ok(selection) => selection,
                Err(error) => { emit_chat_output(&crate::security::redaction::redact_provider_text(&format!("{error:#}"))); continue; }
            };
            let new_model = new_model.as_str();
            let rebuilt = chat_model_switch(&config, provider_name, new_model).and_then(|()| {
                chat_provider_chain(
                    &config,
                    provider_name,
                    configured_provider_name,
                    new_model,
                    &candidate_options,
                )
            });
            match rebuilt {
                Ok((_connection, built)) => {
                    let new_provider_arc: Arc<dyn Provider> = Arc::from(built);
                    let binding = match binding::ChatProviderBinding::capture(
                        &ctx.config_generation,
                        &new_provider_arc,
                        provider_name,
                        new_model,
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            emit_chat_output(&format!(
                                "Cannot bind model selection: {}",
                                providers::sanitize_api_error(&error.to_string())
                            ));
                            continue;
                        }
                    };
                    provider_runtime_options = candidate_options;
                    provider_binding = binding;
                    provider = Arc::clone(&new_provider_arc);
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = provider_slot.as_ref() {
                        slot.set(Arc::clone(&new_provider_arc));
                    }
                    current_model_owned = new_model.to_string();
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = model_slot.as_ref() {
                        slot.set(Arc::from(new_model));
                    }
                    #[cfg(feature = "terminal-tui")]
                    {
                        chat_mirror.lock().model = new_model.to_string();
                    }
                    if let Err(error) = await_metadata_change(
                        &chat_dispatcher,
                        &turn_signal,
                        &mut chat_session,
                        crate::chat::action::Action::ModelChanged {
                            model: new_model.to_string(),
                        },
                    )
                    .await
                    {
                        emit_chat_output(&format!("Model switch was not saved: {error}"));
                        continue;
                    }
                    // emit_chat_output already nudges the renderer (try_send on
                    // the TUI redraw channel) so the status bar repaints.
                    emit_chat_output(&format!(
                        "Switched model to {new_model} (provider {}). Applies from the next turn.",
                        providers::redact_endpoint_userinfo(&provider_name)
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!("Cannot switch to '{new_model}': {e}"));
                }
            }
            continue;
        }

        // Bug #3: `/provider <name> [model]` — hot-switch the provider within a session.
        //
        // Like `/model`, it is intercepted before commands::dispatch, because taking effect requires
        // the main loop to rebuild the provider instance and rewrite several pieces of runtime state:
        //   (a) rebuild the `Arc<dyn Provider>` from the new provider's auth/base/protocol, replacing
        //       the `provider` handle the legacy tool-loop path's `run_turn` call uses;
        //   (b) `set()` it into `provider_slot` (so the Redux driver subtask reads the new provider on
        //       its next turn);
        //   (c) rewrite `current_provider_owned` (which affects the system prompt / events / snapshot
        //       of later turns) and validate that the current model is valid for the new provider (if
        //       not, the command must carry a compatible model: `/provider <name> <model>`).
        // Credential resolution: when switching to a provider that is not the startup primary we pass
        // `api_key=None`/`api_url=None` and let the provider resolve its own credentials from the auth
        // profile / environment (reusing the startup `config.api_key` only makes sense for the
        // original primary); when switching back to the original primary we reuse
        // `config.api_key`/`config.api_url`.
        if let Some(raw) = user_input.strip_prefix("/provider ") {
            let mut parts = raw.split_whitespace();
            // Own the parsed tokens up front so we can freely reassign the runtime
            // provider/model state below without lingering borrows.
            let new_provider = parts.next().unwrap_or_default().to_string();
            let requested_model = parts.next().map(str::to_string);
            if new_provider.is_empty() {
                emit_chat_output("Usage: /provider <name> [model]");
                continue;
            }
            // Decide the model effective after the switch: prefer the one given explicitly on the
            // command, otherwise keep the current model (if compatible).
            let candidate_model = requested_model.unwrap_or_else(|| current_model_owned.clone());
            if let Err(e) = chat_model_switch(&config, &new_provider, &candidate_model) {
                let shown = providers::redact_endpoint_userinfo(&new_provider);
                emit_chat_output(&format!(
                    "Cannot switch to provider '{shown}': model '{candidate_model}' is incompatible ({}). \
Retry with a compatible model: /provider {shown} <model>",
                    providers::redact_endpoint_userinfo(&e.to_string())
                ));
                continue;
            }
            // When switching to a provider other than the configured primary, do not carry over the
            // primary's explicit credentials/URL; let the new provider resolve its own (so provider
            // A's key is never fed to provider B).
            match chat_provider_chain(
                &config,
                &new_provider,
                configured_provider_name,
                &candidate_model,
                &provider_runtime_options,
            ) {
                Ok((_switch_connection, built)) => {
                    let new_provider_arc: Arc<dyn Provider> = Arc::from(built);
                    let next_binding = match binding::ChatProviderBinding::capture(
                        &ctx.config_generation, &new_provider_arc, &new_provider, &candidate_model,
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            emit_chat_output(&format!("Cannot bind provider selection: {}", providers::sanitize_api_error(&error.to_string())));
                            continue;
                        }
                    };
                    provider_binding = next_binding;
                    // (a) the legacy path handle
                    provider = Arc::clone(&new_provider_arc);
                    // (b) Redux driver slot
                    #[cfg(feature = "terminal-tui")]
                    if let Some(slot) = provider_slot.as_ref() {
                        slot.set(Arc::clone(&new_provider_arc));
                    }
                    // (c) the runtime provider / model names
                    let model_changed = candidate_model != current_model_owned;
                    if model_changed {
                        current_model_owned = candidate_model.clone();
                        #[cfg(feature = "terminal-tui")]
                        if let Some(slot) = model_slot.as_ref() {
                            slot.set(Arc::from(candidate_model.as_str()));
                        }
                    }
                    current_provider_owned = new_provider.clone();
                    #[cfg(feature = "terminal-tui")]
                    {
                        let mut mirror = chat_mirror.lock();
                        mirror.provider = new_provider.clone();
                        if model_changed {
                            mirror.model = candidate_model.clone();
                        }
                    }
                    // (d) the session ledger: dispatch ProviderChanged so the reducer updates
                    // session.provider (and session.model when needed), making the status bar / UI
                    // snapshot reflect the new provider immediately. All three places (the legacy
                    // provider handle, the Redux provider_slot and the session ledger) stay
                    // consistent this way. When switching providers also changes the model, both go
                    // into the same action (no separate ModelChanged needed).
                    if let Err(error) = await_metadata_change(
                        &chat_dispatcher,
                        &turn_signal,
                        &mut chat_session,
                        crate::chat::action::Action::ProviderChanged {
                            provider: new_provider.clone(),
                            model: model_changed.then(|| candidate_model.clone()),
                        },
                    )
                    .await
                    {
                        emit_chat_output(&format!("Provider switch was not saved: {error}"));
                        continue;
                    }
                    let model_note = if model_changed {
                        format!(" (model set to {candidate_model})")
                    } else {
                        String::new()
                    };
                    emit_chat_output(&format!(
                        "Switched provider to {}{model_note}. Applies from the next turn.",
                        providers::redact_endpoint_userinfo(&new_provider)
                    ));
                }
                Err(e) => {
                    emit_chat_output(&format!(
                        "Cannot switch to provider '{}': {}",
                        providers::redact_endpoint_userinfo(&new_provider),
                        providers::redact_endpoint_userinfo(&e.to_string())
                    ));
                }
            }
            continue;
        }

        // BUG-07: the model name effective for this iteration (borrowed from the mutable owned
        // value). The `/model` interception above already handled its case and called `continue`, so
        // the shadowed `model_name` here is always the newest value and covers every later use of
        // `model_name` (system prompt / fabric / snapshot).
        let model_name: &str = current_model_owned.as_str();

        // BUG-04: `!cmd` bang mode — run the rest of the line directly as a
        // shell command (matching the footer hint "! for bash") instead of
        // sending it to the LLM. Output is shown inline; the LLM is not
        // involved. A bare `!` is ignored. The shell tool already applies the
        // direct host execution + workspace cwd, so bang commands share the same host FS view
        // as file_write (see BUG-02).
        if let Some(bang_cmd) = user_input.strip_prefix('!') {
            let bang_cmd = bang_cmd.trim();
            if bang_cmd.is_empty() {
                emit_chat_output("Usage: !<shell command>  (runs directly in the workspace)");
                continue;
            }
            let shell_tool = tools_registry.iter().find(|t| t.supports_name("shell"));
            match shell_tool {
                Some(tool) => {
                    let args = serde_json::json!({ "command": bang_cmd });
                    match tool.execute_named("shell", args).await {
                        Ok(result) => {
                            let mut out = String::new();
                            if !result.output.is_empty() {
                                out.push_str(&result.output);
                            }
                            if let Some(err) = result.error.as_ref().filter(|e| !e.is_empty()) {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str(err);
                            }
                            if out.is_empty() {
                                out = if result.success {
                                    "(no output)".to_string()
                                } else {
                                    "(command failed with no output)".to_string()
                                };
                            }
                            emit_chat_output(&out);
                        }
                        Err(e) => emit_chat_output(&format!("Shell error: {e}")),
                    }
                }
                None => emit_chat_output("Shell tool is not available in this session."),
            }
            continue;
        }

        // Both reset commands save the current state and switch every
        // session-scoped UI/model ledger to a fresh identity. Stored memories
        // remain intact, including those owned by other sessions or channels.
        if matches!(user_input.as_str(), "/new" | "/clear") {
            match start_new_chat_session(
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
                Ok(message) => emit_chat_output(&message),
                Err(e) => emit_chat_output(&e.to_string()),
            }
            continue;
        }

        // `/compact` is an explicit request: it compacts whatever lies beyond
        // the kept window, through the same budget the preflight uses.
        if matches!(user_input.as_str(), "/compact") {
            let compact_context = provider_binding
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))?
                .effective_compaction();
            let compact_config = tool_surface_reserve.reserved_config(&compact_context.config);
            let compact_envelope =
                super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), &chat_session_key);
            let (compact_turns_before, compact_tokens_before) =
                super::compaction::chat_history_turns_and_tokens(&history);
            let compact_outcome = super::compaction::run_manual_compact(
                super::compaction::ManualCompact {
                    history: &mut history,
                    chat_session: &chat_session.session(),
                    provider: provider.as_ref(),
                    model: model_name,
                    config: &compact_config,
                    memory: Arc::clone(&mem),
                    envelope: &compact_envelope,
                    chat_dispatcher: &chat_dispatcher,
                    commits: &turn_signal,
                },
                &mut |text: &str| emit_chat_output(text),
            )
            .await;
            emit_chat_output(&compact_outcome.feedback(compact_turns_before, compact_tokens_before, &compact_config));
            #[cfg(feature = "terminal-tui")]
            refresh_context_budget_for_tui(
                &history,
                &compact_context.config,
                &tool_surface_reserve,
                redraw_tx_for_main.is_some(),
                &chat_mirror,
                &chat_dispatcher,
            );
            continue;
        }

        // Dispatch other slash commands
        {
            let cmd_ctx = commands::CommandContext {
                model_name,
                provider_name,
                chat_session: &chat_session.session(),
                tools_registry: &tools_registry,
                mem: mem.as_ref(),
            };
            match commands::dispatch(&user_input, &cmd_ctx).await {
                commands::CommandResult::Handled => continue,
                commands::CommandResult::HandledWithOutput(text) => {
                    emit_chat_output(&text);
                    continue;
                }
                commands::CommandResult::Quit => break 'chat,
                commands::CommandResult::SetMode(mode) => {
                    // The reducer saves a changed mode; success is reported
                    // only after its acknowledgement.
                    if let Err(error) = await_metadata_change(
                        &chat_dispatcher,
                        &turn_signal,
                        &mut chat_session,
                        crate::chat::action::Action::ModeChanged(mode),
                    )
                    .await
                    {
                        emit_chat_output(&format!("Mode unchanged: {error}"));
                        continue;
                    }
                    #[cfg(feature = "terminal-tui")]
                    {
                        chat_mirror.lock().chat_mode = mode;
                    }
                    let msg = match mode {
                        commands::ChatMode::Plan => {
                            "Switched to plan mode (read-only tools only — write/shell/git_commit will be simulated)"
                        }
                        commands::ChatMode::Edit => "Switched to edit mode (default — write tools enabled)",
                        commands::ChatMode::Auto => "Switched to auto mode (does not override [autonomy])",
                    };
                    emit_chat_output(msg);
                    continue;
                }
                commands::CommandResult::ResumeAction(action) => {
                    match action {
                        commands::ResumeCommand::List => match saved_chat_sessions(mem.as_ref()).await {
                            Ok(sessions) => {
                                #[cfg(feature = "terminal-tui")]
                                if sessions_redraw_handle.is_some() {
                                    let entries = sessions
                                        .iter()
                                        .map(|session| {
                                            crate::chat::session::SavedSessionPickerEntry::from_session(
                                                session,
                                                &chat_session.session().id,
                                            )
                                        })
                                        .collect::<Vec<_>>();
                                    {
                                        let mut mirror = chat_mirror.lock();
                                        mirror.saved_sessions_cache = entries.clone();
                                        mirror.saved_session_picker =
                                            Some(crate::chat::session::SavedSessionPickerState::new(entries.clone()));
                                    }
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SavedSessionPickerOpened { entries },
                                        "chat.saved_session_picker_opened_resume",
                                    );
                                    let sources = {
                                        let mirror = chat_mirror.lock();
                                        crate::chat::action::Action::SlashMenuSourcesUpdated {
                                            saved_sessions: mirror.saved_sessions_cache.clone(),
                                            provider_model_catalog: mirror.provider_model_catalog.clone(),
                                        }
                                    };
                                    let _ = chat_dispatcher.dispatch_or_log(sources, "chat.slash_menu_sources_resume");
                                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                                        let _ = tx.try_send(());
                                    }
                                } else {
                                    emit_chat_output(&format_saved_chat_sessions(&sessions));
                                }
                                #[cfg(not(feature = "terminal-tui"))]
                                emit_chat_output(&format_saved_chat_sessions(&sessions));
                            }
                            Err(e) => emit_chat_output(&format!("Failed to list saved chat sessions: {e}")),
                        },
                        commands::ResumeCommand::Id(id) => {
                            match resume_saved_session_by_id(
                                mem.as_ref(),
                                &id,
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
                                Ok(message) => emit_chat_output(&message),
                                Err(e) => emit_chat_output(&e.to_string()),
                            }
                        }
                        commands::ResumeCommand::Last => {
                            if let Err(e) =
                                record_child_summaries_awaited(&mut chat_sessions, &chat_dispatcher, &turn_signal).await
                            {
                                emit_chat_output(&format!(
                                    "Resume aborted: failed to save current session before switching: {e}"
                                ));
                                continue;
                            }

                            let loaded = match load_latest_session_with_message_events(
                                mem.as_ref(),
                                memory_fabric.workspace_id(),
                                &config.cost,
                            )
                            .await
                            {
                                Ok(Some(session)) => Some(session),
                                Ok(None) => {
                                    emit_chat_output("No saved chat sessions to resume.");
                                    None
                                }
                                Err(e) => {
                                    emit_chat_output(&format!(
                                        "Resume aborted: failed to load saved chat session: {e}"
                                    ));
                                    None
                                }
                            };
                            let Some(loaded_session) = loaded else {
                                continue;
                            };

                            if let Err(e) = apply_chat_session_switch(
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
                                loaded_session,
                                false,
                            )
                            .await
                            {
                                emit_chat_output(&format!("Resume aborted: {e}"));
                                continue;
                            }

                            let resumed = chat_session.session();
                            let title = if resumed.title.is_empty() {
                                "(untitled)"
                            } else {
                                resumed.title.as_str()
                            };
                            emit_chat_output(&format!(
                                "Resumed saved chat session {} ({title}, {} turns).",
                                resumed.id,
                                resumed.turn_count()
                            ));
                        }
                    }
                    continue;
                }
                commands::CommandResult::HistoryAction(action) => {
                    match action {
                        commands::HistoryCommand::BranchList => {
                            emit_chat_output(&format_turn_boundaries(&chat_session.session()));
                        }
                        commands::HistoryCommand::Branch(raw) => {
                            let keep_turns = match parse_turn_boundary(&raw, chat_session.session().turn_count(), "branch") {
                                Ok(value) => value,
                                Err(msg) => {
                                    emit_chat_output(&msg);
                                    continue;
                                }
                            };
                            if let Err(e) =
                                record_child_summaries_awaited(&mut chat_sessions, &chat_dispatcher, &turn_signal).await
                            {
                                emit_chat_output(&format!(
                                    "Branch aborted: failed to save current session before forking: {e}"
                                ));
                                continue;
                            }
                            // The branch boundary reads the acknowledged state.
                            let current = chat_session.session();
                            let mut branch = branched_chat_session_from(&current, keep_turns, provider_name, model_name);
                            let branch_id = branch.id.clone();
                            let branch_turns = branch.turn_count();
                            // The branch's own transcript rows (handoff chain and
                            // retained turns) commit in one transaction before the
                            // branch session exists; any failure aborts the branch.
                            let branch_key = crate::runtime::envelope::RuntimeEnvelope::chat_canonical_session_key(&branch.id);
                            super::message_inbox::pin_epoch(&config, &branch_key).await?;
                            if let Err(e) = super::branch_copy::copy_branch_transcript(
                                mem.as_ref(),
                                memory_fabric.workspace_id(),
                                &current.id,
                                &mut branch,
                            )
                            .await
                            {
                                emit_chat_output(&format!("Branch aborted: failed to copy the transcript: {e}"));
                                continue;
                            }
                            if let Err(e) = save_session(mem.as_ref(), &branch).await {
                                emit_chat_output(&format!("Branch aborted: failed to save new branch session: {e}"));
                                continue;
                            }
                            if let Err(e) = apply_chat_session_switch(
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
                                branch,
                                true,
                            )
                            .await
                            {
                                emit_chat_output(&format!("Branch aborted: {e}"));
                                continue;
                            }
                            emit_chat_output(&format!(
                                "Created branch {branch_id} from the first {branch_turns} turns and switched to it."
                            ));
                        }
                        commands::HistoryCommand::Rewind(raw) => {
                            let keep_turns = match parse_turn_boundary(&raw, chat_session.session().turn_count(), "rewind") {
                                Ok(value) => value,
                                Err(msg) => {
                                    emit_chat_output(&msg);
                                    continue;
                                }
                            };
                            if keep_turns == chat_session.session().turn_count() {
                                emit_chat_output(&format!(
                                    "Rewind skipped: session already has exactly {keep_turns} turns."
                                ));
                                continue;
                            }
                            if approval_in_progress(approval_router.as_ref(), &pending_chat_rewind, &pending_diff_apply)
                            {
                                emit_chat_output(approval_already_pending_message());
                                continue;
                            }
                            if sessions_redraw_handle.is_none() {
                                emit_chat_output(
                                    "Rewind requires interactive confirmation; unavailable in this mode. Current session unchanged.",
                                );
                                continue;
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            {
                                emit_chat_output(
                                    "Rewind requires interactive confirmation; unavailable in this mode. Current session unchanged.",
                                );
                                continue;
                            }
                            #[cfg(feature = "terminal-tui")]
                            {
                                let Some(router) = approval_router.as_ref() else {
                                    emit_chat_output(
                                        "Rewind requires interactive confirmation; approval router unavailable. Current session unchanged.",
                                    );
                                    continue;
                                };
                                let target_session = rewound_chat_session_from(&chat_session.session(), keep_turns);
                                let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<bool>();
                                let tool_id = format!("chat_rewind:{}", uuid::Uuid::new_v4());
                                if !router.register(tool_id.clone(), approval_tx) {
                                    emit_chat_output(approval_already_pending_message());
                                    continue;
                                }
                                let args = serde_json::json!({
                                    "session_id": chat_session.session().id,
                                    "from_turns": chat_session.session().turn_count(),
                                    "to_turns": keep_turns,
                                    "drops_child_summaries": keep_turns < chat_session.session().turn_count(),
                                })
                                .to_string();
                                let dispatch_result = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::ToolApprovalRequested {
                                        task_id: None,
                                        tool_id: tool_id.clone(),
                                        name: "rewind_chat_session".to_string(),
                                        args,
                                    },
                                    "chat.rewind_approval_requested",
                                );
                                if dispatch_result != crate::chat::dispatcher::DispatchResult::Sent {
                                    let _ = router.resolve(&tool_id, false);
                                    emit_chat_output("Rewind approval could not be shown; current session unchanged.");
                                    continue;
                                }
                                pending_chat_rewind = Some(PendingChatRewind {
                                    tool_id,
                                    target_session,
                                    approval_rx,
                                });
                                emit_chat_output(&format!(
                                    "Confirm rewind to {keep_turns} turns in the approval prompt."
                                ));
                            }
                        }
                    }
                    continue;
                }
                commands::CommandResult::ApplyAction(action) => {
                    let latest_index = match action {
                        commands::ApplyCommand::Latest => 1,
                        commands::ApplyCommand::Index(index) => index,
                    };
                    if approval_in_progress(approval_router.as_ref(), &pending_chat_rewind, &pending_diff_apply) {
                        emit_chat_output(approval_already_pending_message());
                        continue;
                    }
                    let Some(diff) = diff_apply::latest_fenced_diff(&chat_session.session().turns, latest_index) else {
                        emit_chat_output("No applicable fenced diff block found in this conversation.");
                        continue;
                    };
                    let plan = match diff_apply::parse_unified_diff(&diff, config.autonomy.level) {
                        Ok(plan) => plan,
                        Err(error) => {
                            emit_chat_output(&format!("Diff apply rejected: {error}. Workspace unchanged."));
                            continue;
                        }
                    };
                    #[cfg(not(feature = "terminal-tui"))]
                    let _ = &plan;
                    if sessions_redraw_handle.is_none() {
                        emit_chat_output(
                            "Diff apply requires interactive TUI approval; unavailable in this mode. Workspace unchanged.",
                        );
                        continue;
                    }
                    #[cfg(not(feature = "terminal-tui"))]
                    {
                        emit_chat_output(
                            "Diff apply requires interactive TUI approval; unavailable in this build. Workspace unchanged.",
                        );
                        continue;
                    }
                    #[cfg(feature = "terminal-tui")]
                    {
                        match request_diff_apply_approval(plan, true, approval_router.as_ref(), &chat_dispatcher) {
                            Ok(pending) => {
                                let summary = pending.plan.summary();
                                pending_diff_apply = Some(pending);
                                emit_chat_output(&format!("Confirm diff apply in the approval prompt.\n{summary}"));
                            }
                            Err(message) => emit_chat_output(&message),
                        }
                    }
                    #[cfg(feature = "terminal-tui")]
                    continue;
                }
                commands::CommandResult::SessionAction(action) => {
                    use crate::chat::sessions::SessionCommand;
                    // Shared parsing must not turn mailbox/model input into
                    // local operator authority for a child or approval action.
                    if action.requires_local_operator() && !is_local_operator_input(&msg) {
                        emit_chat_output("This session command requires local terminal operator input.");
                        if let Some(guard) = assignment_guard.as_mut() {
                            guard.failed("Assigned task cannot issue local operator session commands.");
                        }
                        continue;
                    }
                    match action {
                        SessionCommand::Bg { task } => {
                            // Spawn a background agent via sessions_spawn, passing
                            // the *current* provider/model (read from the main-loop
                            // strings, which `/provider` and `/model` keep in sync)
                            // so a hot switch is honoured (plan §C.0 blocker 3).
                            match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                                Some(tool) => {
                                    let command = provider_binding.as_ref()
                                        .ok_or_else(|| anyhow::anyhow!("chat provider binding is missing"))
                                        .and_then(|binding| binding::ChatOperatorSpawn::capture(
                                            &msg, &task, binding, &ctx.config_generation, &provider,
                                            &current_provider_owned, &current_model_owned,
                                        ));
                                    let command = match command {
                                        Ok(command) => command,
                                        Err(error) => {
                                            emit_chat_output(&format!("Cannot admit background command: {error}"));
                                            continue;
                                        }
                                    };
                                    let (mut args, operator_source) = match command.admit(Arc::clone(&mem), &chat_session_key, Arc::clone(&ctx.security), Arc::clone(&turn_service)).await {
                                        Ok(admitted) => admitted,
                                        Err(error) => { emit_chat_output(&format!("Cannot record background command source: {error}")); continue; }
                                    };
                                    // Spawning is a Medium-risk side effect; under
                                    // supervised autonomy the gate requires a grant
                                    // bound to `sessions_spawn:spawn`. The operator
                                    // typed `/bg`, so issue the matching grant here
                                    // (same op name the gate authorizes), mirroring
                                    // `/kill` and `/steer` and how the agent loop
                                    // grants after operator approval.
                                    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
                                        "sessions_spawn",
                                        "sessions_spawn:spawn",
                                        "chat-operator",
                                        None,
                                    );
                                    match serde_json::to_value(&grant) {
                                        Ok(grant_value) => {
                                            if let Some(obj) = args.as_object_mut() {
                                                obj.insert(
                                                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                                                    grant_value,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %crate::agent::diagnostics::error_class(&e),
                                                "Failed to serialize spawn approval grant; proceeding without it"
                                            );
                                        }
                                    }
                                    match operator_source.scope(tool.execute_named("sessions_spawn", args)).await {
                                        Ok(result) => {
                                            let out = if result.output.is_empty() {
                                                result
                                                    .error
                                                    .filter(|e| !e.is_empty())
                                                    .unwrap_or_else(|| "(no output)".to_string())
                                            } else {
                                                result.output
                                            };
                                            emit_chat_output(&out);
                                        }
                                        Err(e) => emit_chat_output(&format!("Failed to start background agent: {e}")),
                                    }
                                }
                                None => emit_chat_output("Background sessions are not available in this session."),
                            }
                            continue;
                        }
                        SessionCommand::Sessions => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&super::session_glue::sessions_listing_with_current(
                                    &chat_session.session().id,
                                    &output,
                                ));
                            }
                            continue;
                        }
                        SessionCommand::Transcript => {
                            attached_follow = None;
                            attached_follow_seq = None;
                            #[cfg(feature = "terminal-tui")]
                            {
                                open_transcript_view(
                                    &chat_mirror,
                                    &chat_dispatcher,
                                    sessions_redraw_handle.as_ref(),
                                    snapshot_rx_for_tui.as_ref(),
                                );
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            emit_chat_output("Transcript viewer is only available in the terminal TUI.");
                            continue;
                        }
                        SessionCommand::Diff { cached } => {
                            attached_follow = None;
                            attached_follow_seq = None;
                            let source = collect_workspace_diff(&config.workspace_dir, cached).await;
                            #[cfg(feature = "terminal-tui")]
                            {
                                if sessions_redraw_handle.is_some() {
                                    open_diff_view(
                                        &chat_mirror,
                                        &chat_dispatcher,
                                        sessions_redraw_handle.as_ref(),
                                        source,
                                    );
                                } else {
                                    emit_chat_output(&source.to_plain_text());
                                }
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            emit_chat_output(&source.to_plain_text());
                            continue;
                        }
                        SessionCommand::Kill { seq: _ } => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&output);
                            }
                            continue;
                        }
                        SessionCommand::Steer { seq, message } => {
                            // v5: steer only applies to agent sessions (it appends
                            // an instruction to a running sub-agent's steer
                            // channel). Shells and PTYs have no steer channel —
                            // resolving their seq would yield a non-agent id that
                            // the sessions_spawn tool can't address, producing a
                            // cryptic "run not found". Guard with a clear message
                            // up front, mirroring `/kill`'s kind dispatch.
                            match chat_sessions.kind_for_seq(seq).await {
                                Ok(kind) => {
                                    if let Some(msg) =
                                        crate::chat::sessions::command::steer_unsupported_message(kind, seq)
                                    {
                                        emit_chat_output(&msg);
                                        continue;
                                    }
                                    // Agent: fall through to the steer delegation.
                                }
                                Err(e) => {
                                    emit_chat_output(&format!("Steer failed: {e}"));
                                    continue;
                                }
                            }
                            // Resolve `#N` -> run UUID, then delegate to the
                            // sessions_spawn tool's `steer` action so the shared
                            // semantics apply uniformly (Low-risk side-effect gate
                            // op `sessions_spawn:steer:<run_id>`, running-status
                            // check, steer_tx delivery). Mirrors `/kill`.
                            let run_id = match chat_sessions.resolve_run_id(seq).await {
                                Ok(id) => id,
                                Err(e) => {
                                    emit_chat_output(&format!("Steer failed: {e}"));
                                    continue;
                                }
                            };
                            match tools_registry.iter().find(|t| t.supports_name("sessions_spawn")) {
                                Some(tool) => {
                                    let operation_name = format!("sessions_spawn:steer:{run_id}");
                                    let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
                                        "sessions_spawn",
                                        &operation_name,
                                        "chat-operator",
                                        None,
                                    );
                                    let mut args = serde_json::json!({
                                        "action": "steer",
                                        "run_id": run_id,
                                        "message": message,
                                    });
                                    match serde_json::to_value(&grant) {
                                        Ok(grant_value) => {
                                            if let Some(obj) = args.as_object_mut() {
                                                obj.insert(
                                                    crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                                                    grant_value,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %crate::agent::diagnostics::error_class(&e),
                                                "Failed to serialize steer approval grant; proceeding without it"
                                            );
                                        }
                                    }
                                    match tool.execute_named("sessions_spawn", args).await {
                                        Ok(result) => {
                                            let out = if result.output.is_empty() {
                                                result
                                                    .error
                                                    .filter(|e| !e.is_empty())
                                                    .unwrap_or_else(|| "(no output)".to_string())
                                            } else {
                                                result.output
                                            };
                                            emit_chat_output(&out);
                                        }
                                        Err(e) => emit_chat_output(&format!("Steer failed: {e}")),
                                    }
                                }
                                None => emit_chat_output("Background sessions are not available in this session."),
                            }
                            continue;
                        }
                        SessionCommand::Attach { seq } => {
                            // P2 `/attach` focuses the child session viewport.
                            // Main history receives only a breadcrumb; retained
                            // and live child output render inside ActiveSessionView.
                            //
                            // v5: PTY sessions are interactive terminal handoffs,
                            // not line-streamed output, so a read-only follow makes
                            // no sense for them. The Ctrl+G switcher routes Enter
                            // through this same `/attach <seq>` path for every kind,
                            // so guard PTYs here with a clear redirect rather than
                            // silently starting an empty follow. (A live PTY can be
                            // re-entered with `/pty`; an exited one is terminal.)
                            #[cfg(feature = "terminal-tui")]
                            if matches!(
                                chat_sessions.kind_for_seq(seq).await,
                                Ok(crate::chat::sessions::model::ManagedKind::Pty)
                            ) {
                                // v3b: a live PTY can be RE-ATTACHED — the detach
                                // path keeps the child running, so `/attach #N`
                                // (and the Ctrl+G switcher's synthetic /attach)
                                // hands the terminal back to it. An exited PTY is
                                // terminal and cannot be attached.
                                let pty = chat_sessions.pty_for_seq_public(seq);
                                match pty {
                                    Some(session) if session.is_attachable() => {
                                        reattach_pty(
                                            &session,
                                            seq,
                                            &pty_handoff,
                                            sessions_redraw_handle.as_ref(),
                                            &emit_chat_output,
                                        )
                                        .await;
                                    }
                                    _ => {
                                        emit_chat_output(&format!(
                                            "Interactive PTY session #{seq} has exited — nothing to attach to. \
                                             Start a new one with /pty <command>."
                                        ));
                                    }
                                }
                                // Restore the prompt/focus the switcher may have set
                                // optimistically (it pointed at this seq before the
                                // handoff), so the chat prompt is not left targeting
                                // the detached PTY for steering.
                                let prev_focus = crate::chat::sessions::focus::rollback_focus(attached_follow_seq);
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::SessionFocusChanged { focus: prev_focus },
                                    "chat.session_focus_attach_pty_done",
                                );
                                {
                                    let mut mirror = chat_mirror.lock();
                                    mirror.focus = prev_focus;
                                    mirror.active_session_view = None;
                                }
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                    "chat.active_session_view_attach_pty_done",
                                );
                                if let Some(tx) = sessions_redraw_handle.as_ref() {
                                    let _ = tx.try_send(());
                                }
                                continue;
                            }
                            const ATTACH_TAIL_LINES: usize = 20;
                            match chat_sessions.resolve_run_id(seq).await {
                                Ok(run_id) => {
                                    let sid = crate::chat::sessions::id::SessionId::from_run_id(&run_id);
                                    let was_following_before_attach = attached_follow.is_some();
                                    let is_terminal = chat_sessions.is_terminal_for_seq(seq).await.unwrap_or(false);
                                    let views = chat_sessions.snapshot().await;
                                    let view_meta = views.iter().find(|view| view.seq == seq);
                                    let tail_lines = match chat_sessions.tail(seq, ATTACH_TAIL_LINES).await {
                                        Ok(lines) => lines
                                            .into_iter()
                                            .map(|line| format!("[{}] {}", line.role, line.content))
                                            .collect::<Vec<_>>(),
                                        Err(e) => {
                                            emit_chat_output(&format!("Attach tail failed: {e}"));
                                            Vec::new()
                                        }
                                    };
                                    let (ring_lines, truncated) = session_rings.get(&sid).map_or_else(
                                        || (Vec::new(), false),
                                        |ring| {
                                            let lines = if is_terminal {
                                                Vec::new()
                                            } else {
                                                ring.recent_lines(crate::chat::sessions::event::DEFAULT_RING_CAPACITY)
                                            };
                                            (lines, ring.is_truncated())
                                        },
                                    );
                                    #[cfg(not(feature = "terminal-tui"))]
                                    let _ = (
                                        &was_following_before_attach,
                                        &view_meta,
                                        &tail_lines,
                                        &ring_lines,
                                        truncated,
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    let active_projection = build_active_session_attach_projection(
                                        seq, view_meta, tail_lines, ring_lines, truncated,
                                    );
                                    attached_follow = Some(sid);
                                    attached_follow_seq = Some(seq);
                                    // v1.1b: route plain input to this session as
                                    // a steer and reflect the target in the prompt
                                    // (colour+glyph). Update both the render
                                    // snapshot (Action) and the key thread's
                                    // mirror (read by `resolve_esc`). When the
                                    // attach was triggered from the switcher the
                                    // key thread already set this optimistically;
                                    // re-affirming it here is idempotent and also
                                    // covers the typed `/attach N` path.
                                    let focus = crate::chat::sessions::FocusTarget::Session { seq };
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SessionFocusChanged { focus },
                                        "chat.session_focus_attach",
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    {
                                        {
                                            let mut mirror = chat_mirror.lock();
                                            mirror.focus = focus;
                                            mirror.active_session_view = Some(active_projection.view.clone());
                                        }
                                        let _ = chat_dispatcher.dispatch_or_log(
                                            crate::chat::action::Action::ActiveSessionViewUpdated {
                                                view: Some(active_projection.view.clone()),
                                            },
                                            "chat.active_session_view_attach",
                                        );
                                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                                            let _ = tx.try_send(());
                                        }
                                    }
                                    #[cfg(feature = "terminal-tui")]
                                    if let Some(breadcrumb) = attach_breadcrumb_for_transition(
                                        was_following_before_attach,
                                        &active_projection,
                                    ) {
                                        emit_chat_output(breadcrumb);
                                    }
                                    #[cfg(not(feature = "terminal-tui"))]
                                    emit_chat_output(&format!(
                                        "Attached session #{seq} (input starts or queues a child turn). Type /detach to stop."
                                    ));
                                }
                                Err(e) => {
                                    // P0 race fix: the switcher key thread may have
                                    // optimistically pointed the prompt + Esc
                                    // judgment at `seq` before this attach ran. The
                                    // attach failed (seq no longer resolves / the
                                    // session is gone), so `attached_follow` is
                                    // unchanged — restore the prompt to the *actual*
                                    // current target so perception cannot diverge
                                    // from routing. (A typed `/attach N` that fails
                                    // has no optimistic set, but restoring the same
                                    // unchanged focus is an idempotent no-op there.)
                                    let prev_focus = crate::chat::sessions::focus::rollback_focus(attached_follow_seq);
                                    let _ = chat_dispatcher.dispatch_or_log(
                                        crate::chat::action::Action::SessionFocusChanged { focus: prev_focus },
                                        "chat.session_focus_attach_rollback",
                                    );
                                    #[cfg(feature = "terminal-tui")]
                                    {
                                        {
                                            let mut mirror = chat_mirror.lock();
                                            mirror.focus = prev_focus;
                                            mirror.active_session_view = None;
                                        }
                                        let _ = chat_dispatcher.dispatch_or_log(
                                            crate::chat::action::Action::ActiveSessionViewUpdated { view: None },
                                            "chat.active_session_view_attach_rollback",
                                        );
                                        if let Some(tx) = sessions_redraw_handle.as_ref() {
                                            let _ = tx.try_send(());
                                        }
                                    }
                                    emit_chat_output(&format!("Attach failed: {e}"));
                                }
                            }
                            continue;
                        }
                        SessionCommand::Detach => {
                            let was_following = attached_follow.take().is_some();
                            attached_follow_seq = None;
                            if was_following {
                                // v1.1b: reset input routing back to main and clear
                                // the prompt target indicator (snapshot + mirror).
                                let focus = crate::chat::sessions::FocusTarget::Main;
                                let _ = chat_dispatcher.dispatch_or_log(
                                    crate::chat::action::Action::SessionFocusChanged { focus },
                                    "chat.session_focus_detach",
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
                                        "chat.active_session_view_detach",
                                    );
                                    if let Some(tx) = sessions_redraw_handle.as_ref() {
                                        let _ = tx.try_send(());
                                    }
                                }
                                emit_chat_output("Detached. Input routes to main chat again.");
                            } else {
                                emit_chat_output("Not currently following any session.");
                            }
                            continue;
                        }
                        SessionCommand::Shell { command } => {
                            // v2: run a non-interactive command in the background.
                            // Runs directly in the configured workspace with the
                            // inherited host environment and the v1.1 event bridge.
                            match crate::chat::sessions::shell::spawn_shell(
                                &command,
                                &config.workspace_dir,
                                &shell_event_sink,
                            ) {
                                Ok(session) => {
                                    let seq = chat_sessions.add_shell(session);
                                    emit_chat_output(&format!("Started background shell #{seq}: {command}"));
                                }
                                Err(e) => {
                                    emit_chat_output(&format!("Failed to start background shell: {e}"));
                                }
                            }
                            continue;
                        }
                        SessionCommand::Logs { seq: _ } => {
                            if let Some(output) = handle_local_session_command(
                                &action,
                                &mut chat_sessions,
                                &session_rings,
                                &mut reaped_log_archive,
                                &reap_policy,
                                &tools_registry,
                            )
                            .await
                            {
                                emit_chat_output(&output);
                            }
                            continue;
                        }
                        SessionCommand::Pty { command } => {
                            // v3a: interactive PTY shell with a full terminal
                            // handoff. The chat ratatui render loop is suspended
                            // and the real terminal is wired straight to the PTY
                            // for the duration; Ctrl-] detaches, Ctrl-C/Ctrl-D
                            // pass through. Restoration is guaranteed by the RAII
                            // `PtyHandoffGuard` regardless of how the passthrough
                            // ends (detach, child exit, error, or panic).
                            #[cfg(feature = "terminal-tui")]
                            {
                                handle_pty_command(
                                    &command,
                                    &config.workspace_dir,
                                    &mut chat_sessions,
                                    &pty_handoff,
                                    sessions_redraw_handle.as_ref(),
                                    &emit_chat_output,
                                )
                                .await;
                            }
                            #[cfg(not(feature = "terminal-tui"))]
                            {
                                let _ = &command;
                                emit_chat_output("Interactive PTY sessions require the terminal UI.");
                            }
                            continue;
                        }
                        SessionCommand::Approve { seq } | SessionCommand::Deny { seq } => {
                            // NeedsInput: deliver an approval decision to a
                            // background sub-agent suspended on the supervised
                            // approval gate. `/approve` injects a runtime grant
                            // (Grant) so the gated tool can pass the gate; `/deny`
                            // reports the tool as denied to the sub-agent.
                            let approve = matches!(action, SessionCommand::Approve { .. });
                            let run_id = match chat_sessions.resolve_run_id(seq).await {
                                Ok(id) => id,
                                Err(e) => {
                                    emit_chat_output(&format!(
                                        "{} failed: {e}",
                                        if approve { "Approve" } else { "Deny" }
                                    ));
                                    continue;
                                }
                            };
                            let decision = if approve {
                                crate::agent::turn_contracts::ApprovalDecision::Grant
                            } else {
                                crate::agent::turn_contracts::ApprovalDecision::Deny
                            };
                            if pending_approvals.resolve(&run_id, decision) {
                                emit_chat_output(&format!(
                                    "{} session #{seq}.",
                                    if approve { "Approved" } else { "Denied" }
                                ));
                            } else {
                                emit_chat_output(&format!(
                                    "Session #{seq} is not awaiting approval (it may have resumed, \
                                     timed out, completed, or was killed)."
                                ));
                            }
                            continue;
                        }
                        SessionCommand::DaemonSessions
                        | SessionCommand::DaemonKill { .. }
                        | SessionCommand::DaemonSteer { .. } => {
                            // a5: the daemon's work registry lives in another
                            // process, so this is a control-API call, not a
                            // registry read. The listing's local fallback is
                            // built here, on the loop where `chat_sessions` is
                            // reachable, and handed to the request to show if
                            // the daemon turns out to be unreachable.
                            let local_fallback = if matches!(action, SessionCommand::DaemonSessions) {
                                handle_local_session_command(
                                    &SessionCommand::Sessions,
                                    &mut chat_sessions,
                                    &session_rings,
                                    &mut reaped_log_archive,
                                    &reap_policy,
                                    &tools_registry,
                                )
                                .await
                            } else {
                                None
                            };
                            if let Some(notice) = start_daemon_session_command(
                                &action,
                                local_fallback,
                                &chat_sessions.daemon_requests(),
                                DaemonControl {
                                    config: &config,
                                    dispatcher: &chat_dispatcher,
                                    redraw_tx: redraw_tx_for_main.as_ref(),
                                },
                            ) {
                                emit_chat_output(&notice);
                            }
                            continue;
                        }
                    }
                }
                commands::CommandResult::NotACommand => {
                    // Focused child input was admitted by child_focus above.

                }
            }
        }

        }

}

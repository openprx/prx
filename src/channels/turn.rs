//! Per-message channel turn execution.

use super::history::{append_sender_turn, merged_history, normalize_cached_channel_turns};
use super::inbound_policy::{
    ConversationKey, authorize_channel_autosave, authorize_channel_inbound, authorize_channel_outbound,
    chat_kind_from_message, conversation_memory_key, group_reply_mode_for, is_bot_mentioned, is_mention_only_enabled,
    should_process_inbound_message, strip_channel_metadata,
};
use super::prompt_context::{
    build_channel_system_prompt, build_system_prompt_with_mode, channel_message_visibility, smart_group_prompt_addendum,
};
use super::response_sanitize::sanitize_channel_response;
use super::runtime_commands::{
    get_route_selection_for_generation, handle_runtime_command_if_needed, message_runtime_snapshot,
    runtime_defaults_for_generation,
};
use super::signal_vision::{
    SignalVisionPreflightOutcome, SignalVisionProvider, SignalVisionScope, is_signal_image_message,
    run_signal_vision_preflight,
};
use super::smart_gate::{
    gather_pre_gate_history, is_bot_sender, is_system_message, record_smart_proactive_reply,
    run_admitted_smart_pre_gate, smart_proactive_within_cooldown,
};
use super::supervisor::{
    channel_progress_line, channel_turn_label, finalize_draft_recording_activity, log_worker_join_result,
    send_recording_activity, spawn_channel_draft_updater, spawn_scoped_typing_task,
};
use super::turn_failure::{classify_turn_failure, turn_error_id, turn_failure_notice};
use super::{
    Arc, CancellationToken, Channel, ChannelRouteSelection, ChannelRuntimeContext, ChatKind, ChatMessage,
    DocumentIngestRuntime, Instant, MemoryFabric, MemoryWriteContext, Result, SIGNAL_IMAGE_UNCERTAINTY_FALLBACK,
    ScopeContext, SendMessage, decision_source, decision_turn, memory, pre_gate, providers, traits,
    truncate_with_ellipsis,
};

/// Handle one inbound channel message as a registered agent turn.
///
/// Nothing ends a turn on a clock any more, so a turn that wedges — in a tool,
/// in a provider call, or anywhere between them — can only be dealt with if an
/// operator can *see* it and end it by hand. That is what this wrapper exists
/// for: the turn is registered before any of its own work starts, and the whole
/// turn runs inside [`crate::runtime::registry::scoped`], so the tool calls,
/// sub-agents and child processes it goes on to start resolve it as their
/// parent and `prx tasks kill --cascade <turn>` reaches every one of them.
///
/// The guard is owned by the awaited future rather than by this scope, so the
/// row disappears with the turn however the turn ends — normal return, panic,
/// or a dropped future.
pub(super) async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    // D8-1: one run_id per channel turn. Minted here rather than deeper inside
    // the turn so the registry row and the turn's stored provenance (every
    // append_sender_turn call, the per-turn context envelope, the terminal
    // record) all carry the same id, and an operator reading `prx tasks list`
    // can correlate a live row with what it later wrote.
    let turn_run_id = uuid::Uuid::new_v4().to_string();
    let guard = crate::runtime::registry::register_turn(
        &channel_turn_label(&msg),
        &turn_run_id,
        Some(cancellation_token.clone()),
    );
    crate::runtime::registry::scoped(guard, boxed_channel_work(ctx, msg, cancellation_token, turn_run_id)).await;
}

// Construct the large channel-work future outside the caller's async poll
// frame. One Box is owned by the unchanged registry scope through completion
// or cancellation; constructing it does not poll the channel work.
fn boxed_channel_work(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
    turn_run_id: String,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
    Box::pin(run_channel_turn(ctx, msg, cancellation_token, turn_run_id))
}

pub(super) async fn run_channel_turn(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
    turn_run_id: String,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    println!(
        "  💬 [{}] from {}: {}",
        msg.channel,
        msg.sender,
        truncate_with_ellipsis(&msg.content, 80)
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    // Pin exactly one process-owned generation at message admission. Snapshot-hot
    // fields are read from this immutable value for the whole message, while
    // rebuild fields are switched by the channel supervisor before publication.
    let config_generation = ctx.config.pin();
    let message_runtime = message_runtime_snapshot(ctx.as_ref(), &config_generation);
    #[cfg(not(test))]
    let security = crate::runtime::bootstrap::build_security_policy(&config_generation.effective);
    #[cfg(test)]
    let security = if Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation) {
        crate::runtime::bootstrap::build_security_policy(&config_generation.effective)
    } else {
        Arc::clone(&ctx.security.load_full().security)
    };
    if handle_runtime_command_if_needed(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref()).await {
        return;
    }
    if !should_process_inbound_message(ctx.as_ref(), &msg) {
        return;
    }

    let history_key = ConversationKey::from_message(&msg);
    let message_visibility = channel_message_visibility(&msg);
    let route = get_route_selection_for_generation(ctx.as_ref(), &history_key, &config_generation);
    let runtime_defaults = runtime_defaults_for_generation(ctx.as_ref(), &config_generation);
    let chat_kind = chat_kind_from_message(&msg);
    let inferred_chat_type = chat_kind.scope_chat_type();
    let is_direct_chat = chat_kind == ChatKind::Dm;
    let is_group = chat_kind.is_group_like();
    println!("  ⏳ Processing message...");

    // Set active recipient on tools that support proactive messaging (message_send, sessions_spawn).
    // Also update the active channel so channel-aware tools route replies back on the correct channel
    // (e.g., wacli for WhatsApp messages, not always Signal).
    for tool in ctx.tools_registry.iter() {
        if tool.name() == "message_send" {
            continue;
        }
        tool.set_active_recipient(&msg.reply_target).await;
        if let Some(ref ch) = target_channel {
            tool.set_active_channel(Arc::clone(ch)).await;
        }
    }

    let started_at = Instant::now();
    let provider_started_at = chrono::Utc::now();

    // ── Inbound side-effect gate (FIX-P0-10/11/12) ──────────────────────────
    // Every channel (telegram/discord/slack/signal/whatsapp/lark/qq/mattermost/
    // irc/dingtalk/matrix/imessage/wacli/…) funnels through this single function,
    // so gating here covers all transports at once. Appending the inbound turn
    // persists conversation history (append_conversation_turn) — a real side
    // effect that must respect autonomy. We use a Low/read-style judgement: under
    // ReadOnly (or when the action budget is exhausted) the gate denies and we
    // abort before the first history mutation; Supervised/Full keep normal
    // traffic flowing untouched (Low risk + grant=None is allowed there).
    // D2-2: use the per-message security generation captured at the top of this
    // function; the policy comes from the generation pinned at admission and is
    // never replaced mid-message. D6: the operation name
    // (`channel:{channel}:inbound:{sender}`) is owned by `InboundGate`.
    if let Err(reason) = authorize_channel_inbound(ctx.as_ref(), security.as_ref(), &msg.channel, &msg.sender) {
        tracing::warn!(
            channel = %msg.channel,
            sender = %msg.sender,
            "channel inbound blocked by InboundGate: {reason}"
        );
        return;
    }

    if let Err(error) = ctx
        .memory
        .upsert_chat_profile_metadata(
            &msg.channel,
            &msg.reply_target,
            inferred_chat_type,
            msg.chat_title.as_deref(),
        )
        .await
    {
        tracing::warn!(
            channel = %msg.channel,
            chat_id = %msg.reply_target,
            "failed to upsert chat profile metadata: {error}"
        );
    }

    // Preserve user turn before the LLM call so interrupted requests keep context.
    let input_owner = match crate::runtime::turn_cleanup::current_tracker() {
        Some(owner) => owner,
        None => {
            tracing::warn!("channel input cleanup owner missing");
            return;
        }
    };
    let input_ticket = match input_owner.register(cancellation_token.clone()) {
        Ok(ticket) => ticket,
        Err(_) => {
            tracing::warn!("channel input admission is closed");
            return;
        }
    };
    let (inbound_event, channel_source, mut admission_guard) =
        match decision_source::ChannelSourceReceipt::record_guarded(
            Arc::clone(&ctx),
            Arc::clone(&config_generation),
            msg.clone(),
            turn_run_id.clone(),
            input_ticket,
        )
        .await
        {
            Ok(input) => input,
            Err(error) if error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() => {
                tracing::info!(
                    outcome = "already_accepted_unconfirmed",
                    "channel input was previously accepted; execution was not repeated"
                );
                if is_direct_chat
                    && authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_ok()
                    && ensure_notice_before_channel_output(
                        ctx.as_ref(),
                        &config_generation,
                        &msg,
                        target_channel.as_ref(),
                    )
                    .await
                {
                    if let Some(channel) = target_channel.as_ref() {
                        let _=send_recording_activity(channel.as_ref(),&SendMessage::new("This input was already accepted. Execution was not repeated; its outcome remains unconfirmed.",&msg.reply_target).in_thread(msg.thread_ts.clone())).await;
                    }
                }
                return;
            }
            Err(_) => {
                tracing::warn!("channel input persistence is unconfirmed");
                return;
            }
        };

    // ── Autosave side-effect gate (FIX-P0-10/11/12) ─────────────────────────
    // Persisting a memory entry is a distinct autonomous side effect, so it gets
    // its own op-id. Low/read-style: denies only under ReadOnly (or exhausted
    // budget); a deny skips the autosave and logs, never aborting the turn.
    // D2-2: reuse the per-message security generation so inbound+autosave for one
    // message see one coherent policy view (no re-poll happens between them).
    let autosave_allowed = authorize_channel_autosave(ctx.as_ref(), security.as_ref(), &msg.channel)
        .map_err(|reason| {
            tracing::warn!(
                channel = %msg.channel,
                sender = %msg.sender,
                "channel autosave blocked by InboundGate: {reason}"
            );
        })
        .is_ok();

    // Only auto-save DM messages; group messages are noise unless explicitly stored by the agent.
    if autosave_allowed && ctx.auto_save_memory && is_direct_chat && memory::should_autosave_content(&msg.content) {
        let autosave_key = conversation_memory_key(&msg);
        let write_ctx = MemoryWriteContext {
            workspace_id: Some(ctx.workspace_dir.as_path().to_string_lossy().to_string()),
            channel: Some(msg.channel.clone()),
            chat_type: Some(inferred_chat_type.to_string()),
            chat_id: Some(msg.reply_target.clone()),
            sender_id: None,
            raw_sender: Some(msg.sender.clone()),
            // V02-R5: a channel turn is never owner-isolated.
            owner_id: None,
            require_owner_match: false,
        };
        let _ = ctx
            .memory
            .store_with_context_and_metadata(
                &autosave_key,
                &msg.content,
                crate::memory::MemoryCategory::Conversation,
                None,
                Some(&write_ctx),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some(ctx.workspace_dir.as_path().to_string_lossy().to_string()),
                    owner_id: inbound_event.as_ref().and_then(|event| event.owner_id.clone()),
                    agent_id: None,
                    persona_id: None,
                    source_event_id: inbound_event.as_ref().map(|event| event.event_id.clone()),
                    source: Some("semantic_promotion".to_string()),
                    topic_id: None,
                    channel: Some(msg.channel.clone()),
                },
            )
            .await;
    }

    // ── Smart group-reply decision context ──────────────────────────────────
    // Effective mode for this channel. Smart-capable channels (Telegram/Discord)
    // tag group-ness via `msg.is_group_hint` since their `reply_target` is a bare
    // chat/channel id; other channels rely on `is_group` above.
    let group_reply_mode = group_reply_mode_for(ctx.as_ref(), &msg.channel);
    // A message is "in a group" for smart purposes if the structured chat kind
    // says so OR the channel layer flagged it.
    let in_group_for_smart = is_group || msg.is_group_hint;
    // 🔴 Invariant #1 (DM never silent): smart suppression is gated on a real
    // group message. DMs (`!in_group_for_smart`) can never be smart, so
    // stay_silent is never exposed and outbound is never suppressed for them.
    // 🔴 Invariant: system/webhook senders (`system:` prefix) bypass smart
    // entirely — they always get a normal reply.
    let smart_group = group_reply_mode.is_smart() && in_group_for_smart && !is_system_message(&msg);

    // Whether the bot is explicitly addressed: central detection OR channel hint.
    let user_text_for_mention = strip_channel_metadata(&msg.content);
    let mentioned = msg.mentioned || is_bot_mentioned(ctx.as_ref(), &msg, &user_text_for_mention);

    if is_group {
        let mention_only = is_mention_only_enabled(ctx.as_ref(), &msg.channel);
        // Non-smart mention_only behavior is byte-identical to before. Smart mode
        // never drops here (it lets the model decide via stay_silent).
        if mention_only && !group_reply_mode.is_smart() && !mentioned {
            println!("  ⏭️ Group message stored but skipped (no mention)");
            admission_guard.finish_without_decision();
            return;
        }
    }

    // Smart proactive guard: when the bot was NOT explicitly addressed in a smart
    // group, apply anti-spam protections BEFORE spending a full LLM loop:
    //  - never proactively react to a bot's own / another bot's message
    //    (prevents bot-to-bot feedback loops). `is_bot_sender` now trusts the
    //    authoritative platform flag (`sender_is_bot`: Telegram `from.is_bot` /
    //    Discord `author.bot`) first, with the sender-name suffix heuristic only
    //    as a fallback — so a bot that does NOT name itself "*bot" is still caught.
    //  - per-group cooldown so the bot does not dominate a busy group.
    //
    // ACCEPTED RISK: an explicit @-mention (`mentioned`) bypasses BOTH guards and
    // always proceeds — "@ always answers" is an intentional product invariant.
    // This means a single peer could @-spam the bot to drive replies; we accept
    // that here because (a) @ is an explicit human summons, (b) the bot-sender
    // guard above still blocks an @-spamming *bot* unless `listen_to_bots` is on,
    // and (c) the per-turn LLM loop and outbound side-effect gates bound cost. If
    // @-flood abuse is observed, add a per-sender token-bucket here (kept out for
    // now to avoid over-engineering the group MVP).
    if smart_group && !mentioned {
        if is_bot_sender(&msg) {
            tracing::debug!(channel = %msg.channel, sender = %msg.sender, "smart: skip proactive reply to bot sender");
            admission_guard.finish_without_decision();
            return;
        }
        if smart_proactive_within_cooldown(ctx.as_ref(), &msg) {
            tracing::debug!(channel = %msg.channel, "smart: skip proactive reply (within cooldown window)");
            admission_guard.finish_without_decision();
            return;
        }
    }

    // Heuristics may prepare the admitted pre-gate path, but never terminate a
    // turn. A possible Skip is confirmed only by the resolver-built auxiliary
    // classifier after source decision admission below.
    let smart_pre_gate_input = (smart_group && !mentioned).then(|| {
        let cfg = &config_generation.effective.smart_group;
        let (recent_context, bot_recently_active) =
            gather_pre_gate_history(ctx.as_ref(), &history_key, cfg.classifier_max_context_messages);
        let heuristic = pre_gate::classify_heuristic(&user_text_for_mention, &ctx.bot_names, bot_recently_active);
        (recent_context, heuristic)
    });

    let mut decision_turn = match decision_turn::ChannelDecisionTurn::admit(
        ctx.as_ref(),
        &channel_source,
        &config_generation,
        &security,
        &route,
        &cancellation_token,
        &mut admission_guard,
    )
    .await
    {
        Ok(turn) => turn,
        Err(error) => {
            if admission_guard
                .finish_failure(cancellation_token.is_cancelled())
                .await
                .is_err()
            {
                tracing::warn!("channel admission failure persistence is unconfirmed");
            }
            tracing::warn!(
                "channel decision admission failed: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return;
        }
    };
    if let Some((recent_context, heuristic)) = smart_pre_gate_input {
        let pre_gate = run_admitted_smart_pre_gate(
            ctx.as_ref(),
            &config_generation,
            &mut decision_turn,
            heuristic,
            &recent_context,
            &user_text_for_mention,
            &cancellation_token,
        )
        .await;
        tracing::debug!(
            channel = %msg.channel,
            path = pre_gate.outcome.path.as_str(),
            enter_loop = pre_gate.outcome.should_enter_loop(),
            "admitted smart pre-gate decision"
        );
        if !pre_gate.outcome.should_enter_loop() {
            let Some(provider_outcome) = pre_gate.provider_outcome else {
                tracing::warn!("smart pre-gate refused silence without a completed classifier outcome");
                return;
            };
            if let Err(error) = decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
                    terminal_id: turn_run_id.clone(),
                    scope: channel_source.envelope().message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Silent,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "smart-group classifier declined a proactive reply".to_owned(),
                        started_at: pre_gate.started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress {
                        reason: "smart-group classifier declined a proactive reply".to_owned(),
                    },
                })
                .await
            {
                tracing::warn!(%error, "smart pre-gate silent terminal failed");
                return;
            }
            println!(
                "  🤫 Smart pre-gate: staying silent (no agent loop) [{}]",
                pre_gate.outcome.path.as_str()
            );
            return;
        }
    }
    match decision_turn.close_reply() {
        Ok(Some(reply)) => {
            if let Err(error) = decision_turn
                .close(ctx.as_ref(), security.as_ref(), &msg, &cancellation_token)
                .await
            {
                tracing::warn!(%error, "channel decision close failed");
                return;
            }
            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&reply),
                message_visibility.clone(),
                None,
                None,
                &turn_run_id,
                false,
            )
            .await;
            if let Some(channel) = target_channel.as_ref() {
                let _ = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(reply, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await;
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "invalid channel decision disposition");
            return;
        }
    }
    let vision_required = is_signal_image_message(&msg);
    if vision_required {
        if authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_err()
            || !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref())
                .await
        {
            return;
        }
    }
    let mut vision_scope = None;
    let decision_turn = if vision_required {
        match decision_turn.vision_accounting() {
            Ok(accounting) => {
                let (turn, observation) = decision_turn.with_vision_observation(&accounting);
                vision_scope = Some(SignalVisionScope {
                    accounting,
                    observation,
                });
                turn
            }
            Err(_) => decision_turn,
        }
    } else {
        decision_turn
    };
    let preflight = if vision_required {
        if let Some(scope) = vision_scope.as_ref() {
            let provider = scope.accounting.provider();
            run_signal_vision_preflight(
                SignalVisionProvider {
                    provider: provider.as_ref(),
                    model: scope.accounting.selected_model().model(),
                    temperature: runtime_defaults.temperature,
                },
                &msg,
                &message_runtime.multimodal,
                ctx.hooks.media_artifacts().as_ref(),
                Some(scope),
                &cancellation_token,
            )
            .await
        } else {
            SignalVisionPreflightOutcome::Fallback {
                reason: "auxiliary vision source or accounting admission refused",
            }
        }
    } else {
        SignalVisionPreflightOutcome::NotRequired
    };
    let signal_vision_context = match preflight {
        SignalVisionPreflightOutcome::NotRequired => None,
        SignalVisionPreflightOutcome::Ready { context } => Some(context),
        SignalVisionPreflightOutcome::Fallback { reason } => {
            // Image-only admission already passed outbound and identity gates.
            let fallback = SIGNAL_IMAGE_UNCERTAINTY_FALLBACK.to_string();
            if let Err(error) = decision_turn
                .fail(&anyhow::anyhow!(reason), cancellation_token.is_cancelled())
                .await
            {
                tracing::warn!(%error, "channel vision fallback terminal failed");
                return;
            }

            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&fallback),
                message_visibility.clone(),
                None,
                None,
                &turn_run_id,
                true,
            )
            .await;
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&fallback, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Err(e) = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(fallback, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
            return;
        }
    };

    let route_display = route.display_provider().into_owned();
    let selected_provider = vision_scope.as_ref().map_or_else(
        || decision_turn.build_provider(),
        |scope| {
            let selected = scope.accounting.selected_model();
            Ok((
                ChannelRouteSelection {
                    provider: selected.provider_label().to_owned(),
                    model: selected.model().to_owned(),
                },
                scope.accounting.provider(),
            ))
        },
    );
    let (route, active_provider) = match selected_provider {
        Ok(selected) => selected,
        Err(err) => {
            if let Err(error) = decision_turn.fail(&err, cancellation_token.is_cancelled()).await {
                tracing::warn!(%error, "channel provider setup terminal failed");
                return;
            }

            if authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel).is_err()
                || !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref())
                    .await
            {
                return;
            }
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route_display
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(message, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await;
            }
            return;
        }
    };

    // Shared events are refreshed each turn so other entrypoints (chat/gateway)
    // can be observed by channel turns without waiting for a session boundary.
    // D8-1: reuse the same per-turn run_id as the message-event envelope so the
    // whole turn (inbound user event, context/recall, outbound assistant event)
    // shares one run_id.
    let runtime_envelope = channel_source.envelope().clone();
    let route_decision = decision_turn.route.clone();
    let memory_context = match decision_turn.memory_context() {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(%error, "channel decision retrieval failed");
            return;
        }
    };
    let chat_profile = match ctx.memory.get_chat_profile(&msg.channel, &msg.reply_target).await {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!(
                channel = %msg.channel,
                chat_id = %msg.reply_target,
                "failed to load chat profile for prompt: {error}"
            );
            None
        }
    };

    let selected_skills = match decision_turn.prompt_skills() {
        Ok(skills) => skills,
        Err(error) => {
            tracing::warn!(%error, "channel decision skill metadata unavailable");
            return;
        }
    };
    let base_system_prompt = build_system_prompt_with_mode(
        &ctx.workspace_dir,
        &route.model,
        &selected_skills,
        Some(&config_generation.effective.identity),
        if config_generation.effective.agent.compact_context {
            Some(6000)
        } else {
            None
        },
        active_provider
            .capabilities_for(
                &route.model,
                crate::providers::traits::ProviderRequestMode::NonStreaming,
            )
            .native_tool_calling,
    );
    let bot_identity = target_channel.as_ref().and_then(|channel| channel.bot_identity());
    let mut system_prompt = build_channel_system_prompt(
        &base_system_prompt,
        &msg,
        chat_profile.as_ref(),
        bot_identity.as_deref(),
    );
    // Smart group-reply: override the default "respond directly / response is
    // automatically sent" guidance so the model knows it is one participant in a
    // group and may decline to speak via `stay_silent`. Only appended for smart
    // group turns (never DMs / non-smart), preserving the default behavior
    // everywhere else.
    if smart_group {
        system_prompt.push_str(smart_group_prompt_addendum(mentioned));
    }
    let rebuild_history = || {
        // Read the canonical ∪ legacy union (FIX-P1-25b read-merge) so pre-cutover
        // turns stored under the legacy key remain visible alongside new canonical
        // turns; `merged_history` concatenates (legacy first) and truncates to the window.
        let prior_turns_raw = merged_history(&ctx.conversation_histories.lock(), &history_key);
        let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

        if let Some(last_turn) = prior_turns.last_mut() {
            if last_turn.role == "user" {
                if !memory_context.is_empty() {
                    last_turn.content = format!("{memory_context}{}", last_turn.content);
                }
                if let Some(context) = signal_vision_context.as_deref() {
                    last_turn.content.push('\n');
                    last_turn.content.push_str(context);
                }
            }
        }

        let mut next_history = vec![ChatMessage::system(system_prompt.clone())];
        next_history.extend(prior_turns);
        next_history
    };

    // ── Outbound side-effect gate (FIX-P0-10/11/12) ─────────────────────────
    // Driving the LLM tool-call loop and sending the reply is the outbound side
    // effect; it gets its own op-id so autonomy can suppress replies
    // independently of inbound persistence. Low/read-style: denies only under
    // ReadOnly (or exhausted budget). A deny skips the LLM call and all reply
    // sends entirely and logs; Supervised/Full proceed normally.
    // D2-2: reuse the per-message security generation. Outbound runs after the
    // agent loop, but deliberately uses the start-of-message snapshot so the whole
    // message stays within a single coherent generation. D6: the operation name
    // (`channel:{channel}:outbound`) is now owned by `InboundGate`.
    if !vision_required {
        if let Err(reason) = authorize_channel_outbound(ctx.as_ref(), security.as_ref(), &msg.channel) {
            tracing::warn!(
                channel = %msg.channel,
                sender = %msg.sender,
                "channel outbound blocked by InboundGate: {reason}"
            );
            return;
        }
        // Article 50 transparency control: after inbound/outbound authorization but
        // before draft creation, streaming, or the final AI response, deliver and
        // minimally acknowledge the versioned AI identity notice. A required notice
        // failure is fail-closed for this turn.
        if !ensure_notice_before_channel_output(ctx.as_ref(), &config_generation, &msg, target_channel.as_ref()).await {
            return;
        }
    }

    let mut history = rebuild_history();
    let use_streaming = target_channel.as_ref().is_some_and(|ch| ch.supports_draft_updates());

    let (delta_tx, delta_rx) = if use_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let draft_message_id = if use_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(&SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()))
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("Failed to send draft on {}: {e}", channel.name());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let draft_has_text = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let draft_updater = if let (Some(rx), Some(draft_id_ref), Some(channel_ref)) =
        (delta_rx, draft_message_id.as_deref(), target_channel.as_ref())
    {
        Some(spawn_channel_draft_updater(
            Arc::clone(channel_ref),
            msg.reply_target.clone(),
            draft_id_ref.to_string(),
            rx,
            Arc::clone(&draft_has_text),
        ))
    } else {
        None
    };

    let typing_cancellation = target_channel.as_ref().map(|_| CancellationToken::new());
    let typing_task = match (target_channel.as_ref(), typing_cancellation.as_ref()) {
        (Some(channel), Some(token)) => Some(spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            token.clone(),
        )),
        _ => None,
    };
    // The typing refresher is a detached task that stops only when its token
    // is cancelled, and a token is not cancelled on drop. When the runtime
    // aborts this turn (a generation swap past its drain, a shutdown), this
    // future is dropped before the cancel below runs; the guard cancels the
    // token then, so the refresher sends its stop and ends (D-072 M-2).
    let _typing_stop_on_abort = typing_cancellation.as_ref().map(|token| token.clone().drop_guard());

    // ── Tool event forwarding: structured logging plus user-visible progress ──
    //
    // Nothing ends a turn on a clock any more, so a turn that legitimately runs
    // for an hour used to look, from the user's side, exactly like one that had
    // died. These notifications already existed and were only logged; feeding
    // them into the draft turns them into the presence signal a long turn needs.
    // This reports, it never terminates: no branch here can end any work.
    let (tool_event_tx, mut tool_event_rx) =
        tokio::sync::mpsc::channel::<crate::agent::turn_contracts::ToolCallNotification>(32);
    let tool_event_channel_name = msg.channel.clone();
    let tool_event_sender_name = msg.sender.clone();
    let progress_draft = match (target_channel.as_ref(), draft_message_id.as_deref()) {
        (Some(channel), Some(draft_id)) => Some((
            Arc::clone(channel),
            msg.reply_target.clone(),
            draft_id.to_string(),
            Arc::clone(&draft_has_text),
        )),
        _ => None,
    };
    let progress_started_at = started_at;
    let tool_event_forwarder = tokio::spawn(async move {
        let mut iteration_seen = 0usize;
        while let Some(notif) = tool_event_rx.recv().await {
            // A progress note only ever lands in a draft that has no streamed
            // text in it yet, so it can never overwrite part of a reply.
            let progress = match &notif {
                crate::agent::turn_contracts::ToolCallNotification::Started { name, .. } => Some(
                    channel_progress_line(name, iteration_seen, progress_started_at.elapsed()),
                ),
                crate::agent::turn_contracts::ToolCallNotification::Progress { iteration, .. } => {
                    iteration_seen = *iteration;
                    Some(channel_progress_line("", *iteration, progress_started_at.elapsed()))
                }
                crate::agent::turn_contracts::ToolCallNotification::Finished { .. } => None,
            };
            if let (Some(line), Some((channel, reply_target, draft_id, has_text))) = (progress, progress_draft.as_ref())
            {
                if !has_text.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(e) = channel.update_draft(reply_target, draft_id, &line).await {
                        tracing::debug!("Draft progress update failed: {e}");
                    }
                }
            }
            match notif {
                crate::agent::turn_contracts::ToolCallNotification::Started { name, args_summary } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        tool = %name,
                        args = %args_summary,
                        "Tool call started"
                    );
                }
                crate::agent::turn_contracts::ToolCallNotification::Finished {
                    name,
                    success,
                    duration_ms,
                } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        tool = %name,
                        success,
                        duration_ms,
                        "Tool call finished"
                    );
                }
                crate::agent::turn_contracts::ToolCallNotification::Progress { iteration } => {
                    tracing::info!(
                        channel = %tool_event_channel_name,
                        sender = %tool_event_sender_name,
                        iteration,
                        "Tool loop progress"
                    );
                }
            }
        }
    });

    type LoopResult = Result<
        (
            crate::agent::turn_contracts::ToolLoopOutcome,
            crate::agent::turn_contracts::ToolLoopTrace,
        ),
        anyhow::Error,
    >;
    enum LlmExecutionResult {
        Completed(Box<LoopResult>),
        Cancelled,
    }

    enum LlmFinalOutcome {
        Cancelled,
        Success {
            response: String,
            trace: crate::agent::turn_contracts::ToolLoopTrace,
        },
        /// Smart group-reply: the model chose to stay silent. No message is sent
        /// and no assistant turn is written to history.
        Silent {
            reason: String,
            trace: crate::agent::turn_contracts::ToolLoopTrace,
        },
        Error(anyhow::Error),
    }

    let scope_owner_id = runtime_envelope.resolved_owner_id();
    // Borrow the policy derived from this message's pinned ConfigGeneration for
    // the entire tool-call loop.
    let scope_ctx = ScopeContext {
        policy: security.as_ref(),
        sender: &msg.sender,
        channel: &msg.channel,
        chat_type: inferred_chat_type,
        chat_id: &msg.reply_target,
        session_key: Some(&runtime_envelope.session_key),
        owner_id: Some(&scope_owner_id),
        owner_isolated: runtime_envelope.owner_isolated,
        topic_id: runtime_envelope.topic_id.as_deref(),
        task_id: runtime_envelope.resolved_task_id(),
        source_message_event_id: runtime_envelope.source_message_event_id.as_deref(),
        config_generation_id: runtime_envelope.config_generation_id,
        config_source_revision: runtime_envelope.config_source_revision.as_deref(),
    };

    // D8-4: seed a turn-root spawn execution context so any sub-agent spawned
    // directly from this channel turn inherits parent_run_id = the per-turn
    // run_id (top-level lineage was previously None). spawn_depth starts at 0 and
    // is_turn_root keeps the first child's reported depth at 0 (a turn is not
    // itself a spawn). The session scope key mirrors the spawn scope convention
    // (channel:chat_id:sender) so children share the turn's session scope.
    let turn_spawn_session_scope_key = format!("{}:{}:{}", msg.channel, msg.reply_target, msg.sender);
    // The channel surface's hidden tool names travel with the context: a
    // sub-agent spawned from this turn answers the same conversation partner, so
    // it runs behind the same boundary instead of the full operator surface.
    let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        turn_run_id.clone(),
        turn_spawn_session_scope_key,
    )
    .with_channel_excluded_tools(message_runtime.tool_tiering.always_exclude.clone());
    let turn_message_send_ctx = target_channel.as_ref().map(|channel| {
        crate::tools::message_send::MessageSendExecutionContext::new(
            Some(msg.reply_target.clone()),
            Arc::clone(channel),
        )
    });

    // No wall clock wraps this turn. It runs for as long as it keeps
    // working; the only automatic end is the stall detector inside
    // `run_turn` (see `crate::agent::idle`), and the only
    // manual one is cancellation — a newer message or `prx tasks kill`.
    // `run_turn` owns provider overflow compaction and replay. This outer scope
    // owns descendant cleanup only, while the terminal guard remains outside
    // it. Forced drop must acknowledge descendant cleanup before the guard may
    // persist the parent outcome.
    let child_owner = crate::runtime::turn_cleanup::current_tracker();
    let (child_cleanup_tx, child_cleanup_rx) = tokio::sync::oneshot::channel();
    let channel_child_cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(child_owner.as_ref(), CancellationToken::new()) {
            Ok(cleanup) => cleanup.with_acknowledgement(Some(child_cleanup_tx)),
            Err(error) => {
                tracing::warn!(%error, "channel child cleanup scope unavailable");
                return;
            }
        };
    let decision_turn = decision_turn.with_child_cleanup(child_cleanup_rx);
    let final_outcome = channel_child_cleanup.run(async { Ok({
        let llm_work = async {
            tokio::select! {
                () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
                result = async {
                        let scoped_tool_loop = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(
                            turn_spawn_ctx.clone(),
                            crate::agent::turn_engine::run_turn(
                                &mut history,
                                crate::agent::turn_request::TurnRequest {
                                    channel_name: msg.channel.as_str(),
                                    scope: Some(&scope_ctx),
                                    mode: crate::agent::turn_contracts::ChatMode::default(),
                                    silent: true,
                                    // expose_stay_silent: ONLY on smart group turns. DMs / non-smart
                                    // never see the tool, so they can never short-circuit to Silent.
                                    expose_stay_silent: smart_group,
                                    multimodal: &message_runtime.multimodal,
                                    schedule: crate::agent::turn_request::TurnSchedule {
                                        read_only_tool_concurrency_window: message_runtime.read_only_tool_concurrency_window,
                                        priority_scheduling_enabled: message_runtime.priority_scheduling_enabled,
                                        low_priority_tool_names: message_runtime.low_priority_tools.clone(),
                                    },
                                    compaction: Some(decision_turn.compaction()),
                                    decision: decision_turn.routing(),
                                },
                                crate::agent::turn_request::TurnServices {
                                    provider: active_provider.as_ref(),
                                    provider_name: route.provider.as_str(),
                                    model: route.model.as_str(),
                                    temperature: runtime_defaults.temperature,
                                    tools_registry: Arc::clone(&ctx.tools_registry),
                                    observer: ctx.observer.as_ref(),
                                    hooks: ctx.hooks.as_ref(),
                                    approval: None,
                                    approval_resolver: None,
                                    runtime_adapter: Some(decision_turn.adapter()),
                                    memory: crate::agent::turn_request::ToolLoopMemory::new(
                                            &ctx.memory,
                                            &ctx.workspace_dir,
                                            Some(DocumentIngestRuntime::from_scope(ctx.memory.clone(), &scope_ctx)),
                                        )
                                        .with_event_fabric(
                                            MemoryFabric::new(
                                                ctx.memory.clone(),
                                                ctx.workspace_dir.as_path().to_string_lossy().to_string(),
                                            )
                                            .with_event_recording(ctx.memory_event_recording),
                                        ),
                                },
                                crate::agent::turn_request::TurnDelivery {
                                    cancellation: Some(cancellation_token.clone()),
                                    on_delta: delta_tx.clone(),
                                    on_tool_call: Some(tool_event_tx.clone()),
                                },
                            ),
                        );
                        match turn_message_send_ctx.clone() {
                            Some(message_ctx) => {
                                crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT
                                    .scope(message_ctx, scoped_tool_loop)
                                    .await
                            }
                            None => scoped_tool_loop.await,
                        }
                    } => LlmExecutionResult::Completed(Box::new(result)),
            }
        };
        let llm_result = if let Some(owner) = crate::runtime::turn_cleanup::current_tracker() {
            let result = match crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), CancellationToken::new()) {
                Ok(cleanup) => cleanup.run(async { Ok(llm_work.await) }).await,
                Err(error) => {
                    drop(llm_work);
                    Err(error.context(crate::runtime::turn_cleanup::CleanupIncomplete))
                }
            };
            result.unwrap_or_else(|error| LlmExecutionResult::Completed(Box::new(Err(error))))
        } else {
            llm_work.await
        };

        match llm_result {
            LlmExecutionResult::Cancelled => LlmFinalOutcome::Cancelled,
            LlmExecutionResult::Completed(result) => match *result {
                Ok((outcome, trace)) => match outcome {
                    crate::agent::turn_contracts::ToolLoopOutcome::Text(response)
                    | crate::agent::turn_contracts::ToolLoopOutcome::AwaitingApproval { reply: response } => {
                        LlmFinalOutcome::Success { response, trace }
                    }
                    crate::agent::turn_contracts::ToolLoopOutcome::Silent { reason } => {
                        LlmFinalOutcome::Silent { reason, trace }
                    }
                },
                Err(e) => {
                    if e.downcast_ref::<crate::runtime::turn_cleanup::CleanupIncomplete>()
                        .is_some()
                    {
                        LlmFinalOutcome::Error(e)
                    } else if crate::agent::turn_contracts::is_tool_loop_cancelled(&e)
                        || cancellation_token.is_cancelled()
                    {
                        LlmFinalOutcome::Cancelled
                    } else {
                        LlmFinalOutcome::Error(e)
                    }
                }
            },
        }
    }) }).await.unwrap_or_else(LlmFinalOutcome::Error);

    drop(delta_tx);
    drop(tool_event_tx);
    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }
    let _ = tool_event_forwarder.await;

    if let Some(token) = typing_cancellation.as_ref() {
        token.cancel();
    }
    if let Some(handle) = typing_task {
        log_worker_join_result(handle.await);
    }

    match final_outcome {
        LlmFinalOutcome::Cancelled => {
            if let Err(error) = decision_turn
                .fail(&anyhow::anyhow!("channel turn cancelled"), true)
                .await
            {
                tracing::warn!(error = %error, "Failed to commit shared cancelled channel terminal event");
            }
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled in-flight channel request due to newer message"
            );
            if let (Some(channel), Some(draft_id)) = (target_channel.as_ref(), draft_message_id.as_deref()) {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                }
            }
        }
        LlmFinalOutcome::Success { response, trace } => {
            let sanitized_response = sanitize_channel_response(&response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty() && !response.trim().is_empty() {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again."
                    .to_string()
            } else {
                sanitized_response
            };

            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            let terminal_committed = match decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope().with_sender("prx"),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: delivered_response.clone(),
                        history_commit_len: history.len().saturating_add(1),
                    }),
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "channel turn completed".to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: msg.reply_target.clone(),
                    },
                })
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to commit shared channel terminal event");
                    false
                }
            };

            if !terminal_committed {
                return;
            }

            let _ = append_sender_turn(
                ctx.as_ref(),
                &config_generation,
                &history_key,
                &msg.channel,
                &msg.sender,
                Some(&msg.reply_target),
                ChatMessage::assistant(&delivered_response),
                message_visibility,
                None,
                None,
                &turn_run_id,
                !terminal_committed,
            )
            .await;
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&delivered_response, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    if let Err(e) = finalize_draft_recording_activity(
                        channel.as_ref(),
                        &msg.reply_target,
                        draft_id,
                        &delivered_response,
                    )
                    .await
                    {
                        tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                        let _ = send_recording_activity(
                            channel.as_ref(),
                            &SendMessage::new(&delivered_response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                    }
                } else if let Err(e) = send_recording_activity(
                    channel.as_ref(),
                    &SendMessage::new(delivered_response, &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
            // Smart group-reply: when the bot just spoke proactively (not @-ed),
            // start the per-group cooldown so it does not immediately speak again.
            if smart_group && !mentioned {
                record_smart_proactive_reply(ctx.as_ref(), &msg);
            }
        }
        LlmFinalOutcome::Silent { reason, trace } => {
            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace);
            if let Err(error) = decision_turn
                .commit(crate::agent::terminal::TurnTerminalDraft {
                    decision: None,
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Silent,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: reason.clone(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Suppress { reason: reason.clone() },
                })
                .await
            {
                tracing::warn!(error = %error, "Failed to commit shared silent channel terminal event");
            }
            // 🔴 Invariant: outbound suppression only happens on smart group
            // turns (`expose_stay_silent` was gated on `smart_group`, so Silent
            // can only originate there). Do NOT send and do NOT write an
            // assistant turn to history — the silent decision leaves no trace.
            // Cancel any in-flight draft so no empty bubble lingers.
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                reason = reason.as_str(),
                "smart group-reply: staying silent (no message sent, no history written)"
            );
            println!(
                "  🤫 Stayed silent ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&reason, 80)
            );
            if let (Some(channel), Some(draft_id)) = (target_channel.as_ref(), draft_message_id.as_deref()) {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!(
                        "Failed to cancel draft after silent decision on {}: {err}",
                        channel.name()
                    );
                }
            }
        }
        LlmFinalOutcome::Error(e) => {
            if let Err(error) = decision_turn.fail(&e, false).await {
                tracing::warn!(error = %error, "Failed to commit shared failed channel terminal event");
                return;
            }
            // What the user is told is chosen from a closed set of fixed
            // sentences keyed on the failure class; the error's own text never
            // reaches the chat. That is what keeps a leaked filesystem path,
            // API key, internal hostname or stack out of an outbound message
            // while still saying something the reader can act on.
            let failure_kind = classify_turn_failure(&e);
            let error_id = turn_error_id(&turn_run_id);
            // The operator-facing side keeps the error verbatim, and now also
            // carries the code the user will quote plus the class that was
            // picked, so a reported id resolves to exactly one turn.
            eprintln!(
                "  ❌ LLM error after {}ms [error_id={error_id} class={}]: {e}",
                started_at.elapsed().as_millis(),
                failure_kind.log_label()
            );
            tracing::error!(
                error_id = %error_id,
                turn_run_id = %turn_run_id,
                failure_class = failure_kind.log_label(),
                channel = %msg.channel,
                error = ?e,
                "channel turn failed"
            );
            if let Some(channel) = target_channel.as_ref() {
                let error_text = turn_failure_notice(failure_kind, &error_id);
                if let Some(ref draft_id) = draft_message_id {
                    let _ =
                        finalize_draft_recording_activity(channel.as_ref(), &msg.reply_target, draft_id, &error_text)
                            .await;
                } else {
                    let _ = send_recording_activity(
                        channel.as_ref(),
                        &SendMessage::new(error_text.as_str(), &msg.reply_target).in_thread(msg.thread_ts.clone()),
                    )
                    .await;
                }
            }
        }
    }
}

pub(super) async fn ensure_notice_before_channel_output(
    ctx: &ChannelRuntimeContext,
    config_generation: &crate::config::ConfigGeneration,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(channel) = target_channel else {
        return true;
    };
    if let Err(error) = crate::compliance::interaction_notice::ensure_interaction_notice(
        &config_generation.effective.compliance.interaction_notice,
        &ctx.memory,
        channel,
        &msg.channel,
        &msg.reply_target,
        msg.thread_ts.clone(),
    )
    .await
    {
        tracing::error!(
            channel = %msg.channel,
            error = %error,
            "required AI interaction notice could not be completed; rejecting turn"
        );
        return false;
    }
    true
}

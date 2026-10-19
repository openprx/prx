//! Inbound message dispatch loop.

use super::prompt_context::interruption_scope_key;
use super::supervisor::log_worker_join_result;
use super::turn::process_channel_message;
use super::{
    Arc, AtomicU64, CancellationToken, ChannelRuntimeContext, HashMap, InFlightSenderTaskState, InFlightTaskCompletion,
    InFlightTaskCompletionGuard, Mutex, Ordering, Result, activity, traits,
};
#[cfg(test)]
use super::{Provider, recording_fixture};

/// Fan every inbound channel message out to its own turn.
///
/// **There is deliberately no concurrency gate here, and nothing may
/// reintroduce one.** This loop used to acquire a semaphore permit — one pool
/// for user messages, a smaller one for `system:` senders — on the dispatch
/// task itself, before it could come back around to `rx.recv()`. That was
/// survivable only while a turn was guaranteed to end on a clock. Now that a
/// turn can legitimately run for hours and nothing expires on its own, a
/// handful of wedged turns would hold every permit forever, the loop would stop
/// receiving, the bounded inbound queue would fill, and every listener's
/// `tx.send(..).await` would park: one stuck conversation deafens the whole
/// process, for every channel and every user, until it is restarted. Spawning
/// unconditionally is what keeps a stuck turn a *local* failure.
///
/// Removing the gate does not remove backpressure, it moves it to where it can
/// be honest. The inbound channel is still bounded and every listener publishes
/// with `send(..).await`, so a producer that outruns the process is slowed down
/// rather than having its messages dropped. What changed is what a full queue
/// *means*: the loop no longer blocks on anything a turn holds, so it can only
/// fall behind if the machine genuinely cannot spawn tasks fast enough. Live
/// turns are the memory that grows, and every one of them is registered,
/// listable and killable — see [`process_channel_message`].
///
/// The old user/system split is not replaced by another mechanism because it
/// never delivered the property it claimed. Both semaphores gated the same
/// task, so an exhausted *system* pool stalled user messages exactly as hard as
/// an exhausted user pool; with no shared permits at all there is nothing left
/// for system traffic to starve user traffic out of. The distinction survives
/// where it does real work — [`is_system_message`] at the smart-group gate, and
/// the `system:` sender prefix that a turn carries into its registry label.
pub(super) async fn run_message_dispatch_loop_with_cleanup(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    shutdown: CancellationToken,
) -> Result<()> {
    let cleanup = crate::runtime::turn_cleanup::current_tracker()
        .unwrap_or_else(|| Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default()));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(Mutex::new(HashMap::<String, InFlightSenderTaskState>::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));
    let mut health_heartbeat = tokio::time::interval(std::time::Duration::from_mins(1));
    health_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // D5/D9 step 5: observe the external shutdown token alongside inbound
        // messages so the dispatch loop stops accepting new work promptly on a
        // root cancel, rather than only when the channel closes.
        let msg = tokio::select! {
            () = shutdown.cancelled() => break,
            _ = health_heartbeat.tick() => {
                crate::health::mark_component_ok("channels");
                continue;
            }
            // Reap finished workers as they land instead of only when the
            // next message arrives, so a burst followed by silence does not
            // leave completed handles sitting in the set. Guarded because
            // `join_next` on an empty set resolves immediately.
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                log_worker_join_result(result);
                continue;
            }
            maybe_msg = rx.recv() => match maybe_msg {
                Some(msg) => {
                    // The single point every channel's inbound traffic passes
                    // through, webhook-delivered messages included.
                    activity::record_inbound(&msg.channel);
                    msg
                }
                None => break,
            },
        };

        // Register interruptible turns in receive order, before spawning. If
        // registration happens inside independently scheduled tasks, an older
        // message can run second and incorrectly cancel the newer one.
        let interrupt_enabled = ctx.interrupt_on_new_message && msg.channel == "telegram";
        let sender_scope_key = interruption_scope_key(&msg);
        let cancellation_token = CancellationToken::new();
        let completion = Arc::new(InFlightTaskCompletion::new());
        let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);
        let previous = if interrupt_enabled {
            in_flight_by_sender.lock().insert(
                sender_scope_key.clone(),
                InFlightSenderTaskState {
                    task_id,
                    cancellation: cancellation_token.clone(),
                    completion: Arc::clone(&completion),
                },
            )
        } else {
            None
        };

        let worker_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(Some(&cleanup), CancellationToken::new())?;
        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        workers.spawn(async move {
            let _completion_guard = InFlightTaskCompletionGuard(Arc::clone(&completion));

            let work = async {
                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    previous.completion.wait().await;
                }

                process_channel_message(worker_ctx, msg, cancellation_token).await;
                Ok(())
            };
            if let Err(error) = worker_cleanup.run(work).await {
                tracing::warn!(%error, "Channel worker cleanup did not complete");
            }

            if interrupt_enabled {
                let mut active = in_flight.lock();
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    // Closing the input stream is graceful; explicit shutdown cancels active
    // scope owners. In neither case may a child drain be mistaken for completion.
    if !shutdown.is_cancelled() {
        while let Some(result) = workers.join_next().await {
            log_worker_join_result(result);
        }
    }
    let drained = cleanup
        .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(15))
        .await;
    if drained.is_err() {
        workers.abort_all();
    }
    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
    drained
}

#[cfg(test)]
pub(super) async fn run_message_dispatch_loop(
    rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    shutdown: CancellationToken,
) {
    run_message_dispatch_loop_with_cleanup(rx, normalize_legacy_channel_fixture(ctx), shutdown)
        .await
        .expect("channel cleanup");
}

#[cfg(test)]
pub(super) fn normalize_legacy_channel_fixture(mut ctx: Arc<ChannelRuntimeContext>) -> Arc<ChannelRuntimeContext> {
    let split_generation = !Arc::ptr_eq(&ctx.config.pin(), &ctx.config_generation);
    let injected_provider = !crate::providers::ProviderId::of(ctx.default_provider.as_str()).is_resolvable();
    if !split_generation && !injected_provider {
        let owned = Arc::make_mut(&mut ctx);
        if !matches!(owned.memory.name(), "sqlite" | "lucid" | "postgres" | "none") {
            recording_fixture::attach_ledger_only(owned);
        }
        return ctx;
    }
    let mut config = ctx.config.load_full().as_ref().clone();
    let fixture_provider_name = ctx.default_provider.as_str().to_owned();
    let provider_cache = ctx.provider_cache.lock().clone();
    let fixture_provider = provider_cache
        .get(&(
            ctx.config_generation.id.0,
            fixture_provider_name.clone(),
            ctx.model.as_str().to_owned(),
        ))
        .cloned()
        .or_else(|| {
            provider_cache
                .iter()
                .find(|((_, provider, _), _)| provider == &fixture_provider_name)
                .map(|(_, provider)| Arc::clone(provider))
        })
        .unwrap_or_else(|| Arc::clone(&ctx.provider));
    let fixture_label_for = |provider: &Arc<dyn Provider>| {
        let identity = Arc::as_ptr(provider).cast::<()>() as usize;
        format!("custom:http://127.0.0.1/prx-channel-fixture-{identity:x}")
    };
    let mut fixture_providers = HashMap::new();
    fixture_providers.insert(
        fixture_provider_name.clone(),
        (fixture_label_for(&fixture_provider), Arc::clone(&fixture_provider)),
    );
    for ((_, name, _), provider) in provider_cache {
        fixture_providers
            .entry(name)
            .or_insert_with(|| (fixture_label_for(&provider), provider));
    }
    for (label, provider) in fixture_providers.values() {
        crate::providers::test_fixture::register(label, Arc::clone(provider));
    }
    let fixture_label = fixture_providers
        .get(&fixture_provider_name)
        .map(|(label, _)| label.clone())
        .expect("legacy channel fixture default provider");
    config.workspace_dir = ctx.workspace_dir.as_ref().clone();
    config.config_path = config.workspace_dir.join("config.toml");
    if split_generation {
        config.default_model = Some(ctx.model.as_str().to_owned());
        config.default_temperature = ctx.temperature;
        config.api_key.clone_from(&ctx.api_key);
        config.api_url.clone_from(&ctx.api_url);
        config.reliability = ctx.reliability.as_ref().clone();
        config.autonomy.level = ctx.security.load_full().security.autonomy;
        config.multimodal = ctx.multimodal.clone();
        config.agent.read_only_tool_concurrency_window = ctx.read_only_tool_concurrency_window;
        config.agent.priority_scheduling_enabled = ctx.priority_scheduling_enabled;
        config.agent.low_priority_tools = ctx.low_priority_tools.clone();
        config.tool_tiering = ctx.tool_tiering.clone();
        config.memory.auto_save = ctx.auto_save_memory;
        config.memory.events.record_user_messages = ctx.memory_event_recording.record_user_messages;
        config.memory.events.record_assistant_messages = ctx.memory_event_recording.record_assistant_messages;
        config.memory.events.record_tool_events = ctx.memory_event_recording.record_tool_events;
    }
    config.default_provider = Some(fixture_label.clone());
    for route in &mut config.model_routes {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    for route in &mut config.router.models {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    for model in &mut config.providers.models {
        if let Some((label, _)) = fixture_providers.get(&model.provider) {
            model.provider.clone_from(label);
        }
    }
    let mut fixture_routes = vec![(
        fixture_label.clone(),
        config
            .default_model
            .clone()
            .unwrap_or_else(|| ctx.model.as_str().to_owned()),
        Arc::clone(&fixture_provider),
    )];
    for route in ctx.route_overrides.lock().values() {
        if let Some((label, provider)) = fixture_providers.get(&route.provider) {
            fixture_routes.push((label.clone(), route.model.clone(), Arc::clone(provider)));
        }
    }
    fixture_routes.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    fixture_routes.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    for (label, model, provider) in fixture_routes {
        if !config.providers.models.iter().any(|entry| {
            entry.provider.eq_ignore_ascii_case(&label)
                && entry
                    .model
                    .as_deref()
                    .is_some_and(|declared| declared.eq_ignore_ascii_case(&model))
        }) {
            config.providers.models.push(
                toml::from_str(&format!(
                    "provider = {label:?}\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = {}\nvision_input = {}\n",
                    !config.tool_tiering.always_include.is_empty()
                        || provider
                            .capabilities_for(&model, crate::providers::traits::ProviderRequestMode::NonStreaming)
                            .native_tool_calling,
                    provider
                        .capabilities_for(&model, crate::providers::traits::ProviderRequestMode::NonStreaming)
                        .vision
                ))
                .expect("legacy channel fixture model declaration"),
            );
        }
        config
            .cost
            .prices
            .entry(format!("{label}/{model}"))
            .or_insert(crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            });
    }
    for route in ctx.route_overrides.lock().values() {
        let provider = fixture_providers
            .get(&route.provider)
            .map_or(route.provider.as_str(), |(label, _)| label.as_str());
        config
            .cost
            .prices
            .entry(format!("{provider}/{}", route.model))
            .or_insert(crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            });
    }
    let owned = Arc::make_mut(&mut ctx);
    for route in owned.route_overrides.lock().values_mut() {
        if let Some((label, _)) = fixture_providers.get(&route.provider) {
            route.provider.clone_from(label);
        }
    }
    owned.default_provider = Arc::new(fixture_label);
    if !matches!(owned.memory.name(), "sqlite" | "lucid" | "postgres" | "none") {
        recording_fixture::attach_ledger_only(owned);
    }
    let shared = crate::config::new_shared(config);
    owned.config_generation = shared.pin();
    owned.config = shared;
    ctx
}

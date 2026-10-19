//! Supervised channel listeners, health publishing and per-turn helpers.

use super::{
    Arc, CHANNEL_CIRCUIT_BREAKER_BACKOFF_MULTIPLIER, CHANNEL_CIRCUIT_BREAKER_FAILURES, CHANNEL_HEALTH_HEARTBEAT_SECS,
    CHANNEL_TYPING_REFRESH_INTERVAL_SECS, CancellationToken, Channel, Duration, Result, SendMessage, activity, traits,
};

/// Outcome of one supervised `listen()` round (D5/D9 step 5).
///
/// Introduced to give the inner `select!` a single break type carrying either
/// the listener's own result (drives the restart logic) or an explicit shutdown
/// request. Without this, a shutdown branch could not break out of a `select!`
/// arm that previously broke a bare `Result`, and a long-blocked `listen()`
/// would leave `handles.await` (in `start_channels`) hung forever.
pub(super) enum ListenerOutcome {
    /// `ch.listen()` returned; carries its result for the restart/backoff logic.
    Listen(Result<()>),
    /// The external shutdown token fired; stop the supervisor without restarting.
    Shutdown,
}

pub(super) fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
        shutdown,
    )
}

pub(super) fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    tokio::spawn(async move {
        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);
        let mut consecutive_failures = 0_u32;

        loop {
            // Declare how this incarnation of the listener proves liveness and
            // reset its activity baseline, so a restarted listener is not judged
            // on the silence of the one it replaced.
            activity::register(ch.name(), ch.liveness_expectation());
            crate::health::mark_component_ok(&component);
            let mut health = tokio::time::interval(health_interval);
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let outcome = {
                let listen_future = ch.listen(tx.clone());
                tokio::pin!(listen_future);

                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => break ListenerOutcome::Shutdown,
                        _ = health.tick() => {
                            publish_listener_health(ch.name(), &component);
                        }
                        result = &mut listen_future => break ListenerOutcome::Listen(result),
                    }
                }
            };

            // Shutdown requested: stop supervising without restarting so the
            // owning `handles.await` in start_channels can complete even when
            // `listen()` would otherwise block indefinitely.
            let result = match outcome {
                ListenerOutcome::Shutdown => break,
                ListenerOutcome::Listen(result) => result,
            };

            if tx.is_closed() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    // The whole chain ("failed to bind …: address in use"), the
                    // log copy redacted (D-072 L-3, L-4).
                    tracing::error!(
                        "Channel {} error: {}; restarting",
                        ch.name(),
                        crate::security::redaction::redact_secrets(&format!("{e:#}"))
                    );
                    crate::health::mark_component_error(&component, format!("{e:#}"));
                    consecutive_failures = consecutive_failures.saturating_add(1);
                }
            }

            crate::health::bump_component_restart(&component);
            let sleep_duration = channel_supervisor_sleep_duration(consecutive_failures, backoff, max_backoff);
            if consecutive_failures >= CHANNEL_CIRCUIT_BREAKER_FAILURES {
                tracing::warn!(
                    channel = %ch.name(),
                    consecutive_failures,
                    sleep_secs = sleep_duration.as_secs(),
                    "Channel supervisor circuit open; diluting restart attempts"
                );
                crate::health::mark_component_error(
                    &component,
                    format!("listener circuit open after {consecutive_failures} consecutive failures"),
                );
            }
            tokio::time::sleep(sleep_duration).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

/// Publish what the listener has actually been observed doing.
///
/// This replaces an unconditional `mark_component_ok` on every heartbeat tick.
/// That older signal only proved the heartbeat timer was still scheduled, so a
/// listener wedged forever inside `listen()` — a long poll that never returns, a
/// socket that never errors — kept publishing perfect health while receiving
/// nothing. Health is now derived from `channels::activity`, which records real
/// traffic and completed upstream round-trips.
///
/// The stall verdict is a *report*, and must stay one. The listener is left
/// running untouched and returns to `Ready` on its own the moment activity
/// resumes. Never wire a restart, an abort, a reconnect or a cancellation to it:
/// prx imposes no execution timeouts, and only an operator can tell a wedged
/// channel from a deliberately quiet one.
pub(super) fn publish_listener_health(channel: &str, component: &str) {
    let Some(status) = activity::status(channel) else {
        // Nothing has ever been recorded for this channel, so there is no
        // evidence to publish. Refresh freshness without claiming readiness
        // rather than asserting a health we cannot back up.
        crate::health::touch_component(component);
        return;
    };

    crate::health::set_component_activity(component, Some(status.to_component_activity()));
    if status.stalled {
        crate::health::mark_component_degraded(component, status.stall_summary());
    } else {
        crate::health::mark_component_ok(component);
    }
}

/// Deliver a reply and record the outbound activity when it succeeds.
///
/// A successful send proves the credentials, the network and the remote API are
/// still working. It is tracked separately from the receive path and is
/// deliberately excluded from the stall verdict in `channels::activity`: a
/// channel can keep answering perfectly while its listener is deaf, and masking
/// that with outbound traffic is exactly the false-health this design removes.
pub(super) async fn send_recording_activity(channel: &dyn Channel, message: &SendMessage) -> anyhow::Result<()> {
    let result = channel.send(message).await;
    if result.is_ok() {
        activity::record_outbound(channel.name());
    }
    result
}

/// Finalize a streamed draft and record the outbound activity when it succeeds.
pub(super) async fn finalize_draft_recording_activity(
    channel: &dyn Channel,
    recipient: &str,
    message_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let result = channel.finalize_draft(recipient, message_id, text).await;
    if result.is_ok() {
        activity::record_outbound(channel.name());
    }
    result
}

pub(super) fn channel_supervisor_sleep_duration(consecutive_failures: u32, backoff: u64, max_backoff: u64) -> Duration {
    if consecutive_failures >= CHANNEL_CIRCUIT_BREAKER_FAILURES {
        return Duration::from_secs(
            max_backoff
                .max(1)
                .saturating_mul(u64::from(CHANNEL_CIRCUIT_BREAKER_BACKOFF_MULTIPLIER)),
        );
    }
    Duration::from_secs(backoff.max(1))
}

pub(super) fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

/// Streamed assistant text written into a channel draft, one draft per turn.
///
/// `has_text` goes true the moment the first delta arrives and is the interlock
/// with [`spawn_channel_progress_reporter`]: progress notes are only ever
/// written into a draft that has no real text in it yet, so a long turn shows
/// what it is doing without a reply ever being overwritten by a status line.
/// Every update rewrites the whole accumulated string, so even a lost race with
/// a progress note repairs itself on the next delta.
pub(super) fn spawn_channel_draft_updater(
    channel: Arc<dyn Channel>,
    reply_target: String,
    draft_id: String,
    mut rx: tokio::sync::mpsc::Receiver<String>,
    has_text: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut accumulated = String::new();
        while let Some(delta) = rx.recv().await {
            accumulated.push_str(&delta);
            has_text.store(true, std::sync::atomic::Ordering::Release);
            if let Err(e) = channel.update_draft(&reply_target, &draft_id, &accumulated).await {
                tracing::debug!("Draft update failed: {e}");
            }
        }
    })
}

/// One line of "still working" for a turn that has not produced text yet.
pub(super) fn channel_progress_line(tool: &str, iteration: usize, elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let elapsed_text = if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    };
    if tool.is_empty() {
        format!("⏳ working… (step {iteration}, {elapsed_text})")
    } else {
        format!("⏳ {tool}… (step {iteration}, {elapsed_text})")
    }
}

pub(super) fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        tracing::debug!("Failed to start typing on {}: {e}", channel.name());
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
        }
    })
}

/// Registry label for one channel turn — enough to tell two live turns apart in
/// `prx tasks list` without spilling message content into an operator listing.
///
/// The correspondent is named by the same fingerprint the outbound path uses
/// ([`crate::security::op_id::ref_for_channel_recipient`]), never in plaintext.
/// A sender on a messaging platform *is* an address — a phone number, a JID, a
/// chat id — and this row is not a private note: it is what
/// `GET /api/runtime/tasks` hands to every caller the pairing check admits, and
/// pairing is off by default. Spelling the sender out here would have made the
/// busiest row in the runtime, minted once per inbound message, the place a
/// correspondent's address leaks, while every send *to* that same person was
/// carefully fingerprinted.
///
/// Fingerprinting through that shared function rather than a local one is what
/// makes the listing still useful: the digest of an inbound turn matches the
/// digest of an outbound send to the same correspondent on the same channel, so
/// "this turn and that send are the same person" survives the redaction, and
/// the channel name — not sensitive, and the only part that says *where* — is
/// left readable.
///
/// MUTATION GUARD: interpolate `msg.sender` or `msg.reply_target` directly and
/// `a_channel_turn_label_fingerprints_the_correspondent` fails.
pub(super) fn channel_turn_label(msg: &traits::ChannelMessage) -> String {
    use std::fmt::Write as _;
    let mut label = format!(
        "channel:{}:{}",
        msg.channel,
        crate::security::op_id::ref_for_channel_recipient(&msg.channel, &msg.sender)
    );
    if !msg.reply_target.is_empty() && msg.reply_target != msg.sender {
        // Writing into a String is infallible; the Result is discarded rather
        // than unwrapped so this stays panic-free by construction.
        let _ = write!(
            label,
            " in {}",
            crate::security::op_id::ref_for_channel_recipient(&msg.channel, &msg.reply_target)
        );
    }
    label
}

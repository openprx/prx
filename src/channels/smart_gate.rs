//! Smart group pre-gate admission and proactive-reply cooldown.

use super::history::merged_history;
use super::inbound_policy::{ConversationKey, extract_group_identifier, strip_channel_metadata};
use super::{
    Arc, CancellationToken, ChannelRuntimeContext, decision_turn, pre_gate, providers, traits, truncate_with_ellipsis,
};

/// Classify whether a channel message originated from a system source
/// (e.g. webhook `system:*` senders, internal cron ticks) vs. a real user.
pub(super) fn is_system_message(msg: &traits::ChannelMessage) -> bool {
    msg.sender.starts_with("system:")
}

/// Cooldown window between proactive (non-@) smart group replies in the same
/// group, to keep the bot from dominating a busy conversation. Explicit
/// @-mentions bypass this entirely.
pub(super) const SMART_PROACTIVE_COOLDOWN_SECS: u64 = 45;

/// Best-effort detection that a message originated from a bot (this bot or
/// another bot), used to suppress proactive smart replies and break potential
/// bot-to-bot feedback loops. Channel layers also filter bots upstream (e.g.
/// Discord `listen_to_bots`); this is a defense-in-depth central guard keyed on
/// the bot's own configured identities plus a common `bot`-suffix heuristic.
pub(super) fn is_bot_sender(msg: &traits::ChannelMessage) -> bool {
    // Primary signal: the authoritative platform flag (Telegram `from.is_bot`,
    // Discord `author.bot`). When the channel knows the sender is a bot, trust it.
    if msg.sender_is_bot {
        return true;
    }
    // Fallback heuristic for channels that do not supply `sender_is_bot`: common
    // bot account naming conventions across platforms.
    let sender = msg.sender.trim();
    if sender.is_empty() {
        return false;
    }
    let lower = sender.to_lowercase();
    lower.ends_with("bot") || lower.ends_with("-bot") || lower.contains(":bot:")
}

/// Group key for proactive smart-reply cooldown tracking.
pub(super) fn smart_cooldown_key(msg: &traits::ChannelMessage) -> String {
    let group = extract_group_identifier(msg).unwrap_or_else(|| msg.reply_target.clone());
    format!("{}:{}", msg.channel, group)
}

/// Whether a proactive smart reply for this group is still within the cooldown
/// window since the last proactive reply. Read-only (does not record).
pub(super) fn smart_proactive_within_cooldown(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) -> bool {
    let key = smart_cooldown_key(msg);
    let window = std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS);
    let now = std::time::Instant::now();
    let guard = ctx.smart_reply_cooldown.lock();
    guard.get(&key).is_some_and(|last| now.duration_since(*last) < window)
}

/// Record that a proactive smart reply was just sent for this group, starting a
/// fresh cooldown window. Also prunes stale entries to bound memory.
pub(super) fn record_smart_proactive_reply(ctx: &ChannelRuntimeContext, msg: &traits::ChannelMessage) {
    let key = smart_cooldown_key(msg);
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_secs(SMART_PROACTIVE_COOLDOWN_SECS);
    let mut guard = ctx.smart_reply_cooldown.lock();
    guard.retain(|_, last| now.duration_since(*last) < window);
    guard.insert(key, now);
}

/// Number of recent turns scanned to decide whether the bot was "recently
/// active" in this group (topic-continuation signal for the heuristic).
const SMART_RECENT_ACTIVITY_SCAN: usize = 6;

/// Gather the recent-history signals the pre-gate needs:
///   - `recent_context`: up to `max_context` recent turns rendered as short
///     `role: text` lines for the Tier-2 classifier prompt,
///   - `bot_recently_active`: whether any of the last few turns was an assistant
///     turn (the bot participated → topic-continuation bias toward replying).
///
/// Reads the canonical ∪ legacy union exactly like the loop's history rebuild.
pub(super) fn gather_pre_gate_history(
    ctx: &ChannelRuntimeContext,
    history_key: &ConversationKey,
    max_context: usize,
) -> (Vec<String>, bool) {
    let turns = merged_history(&ctx.conversation_histories.lock(), history_key);
    let bot_recently_active = turns
        .iter()
        .rev()
        .take(SMART_RECENT_ACTIVITY_SCAN)
        .any(|turn| turn.role == "assistant" && !turn.content.trim().is_empty());

    let recent_context: Vec<String> = if max_context == 0 {
        Vec::new()
    } else {
        turns
            .iter()
            .rev()
            .take(max_context)
            .rev()
            .map(|turn| {
                let role = if turn.role == "assistant" { "assistant" } else { "user" };
                let body = strip_channel_metadata(&turn.content);
                format!("{role}: {}", truncate_with_ellipsis(body.trim(), 200))
            })
            .collect()
    };
    (recent_context, bot_recently_active)
}

pub(super) struct AdmittedSmartPreGate {
    pub(super) outcome: pre_gate::PreGateOutcome,
    pub(super) provider_outcome: Option<crate::llm::route_decision::ProviderExecutionOutcome>,
    pub(super) started_at: chrono::DateTime<chrono::Utc>,
}

impl AdmittedSmartPreGate {
    /// The completed classifier outcome that authorizes silence, or `None`
    /// when the message must enter the loop (fail-open): either the gate
    /// chose to enter, or it skipped without a settled classifier receipt.
    pub(super) const fn silence_provider_outcome(
        &mut self,
    ) -> Option<crate::llm::route_decision::ProviderExecutionOutcome> {
        if self.outcome.should_enter_loop() {
            return None;
        }
        self.provider_outcome.take()
    }
}

/// Run the smart-group classifier only after the source decision exists. A
/// heuristic may enter the main loop directly, but neither heuristic nor a
/// caller-owned provider can authorize silence.
pub(super) async fn run_admitted_smart_pre_gate(
    ctx: &ChannelRuntimeContext,
    config_generation: &Arc<crate::config::ConfigGeneration>,
    decision_turn: &mut decision_turn::ChannelDecisionTurn,
    heuristic: pre_gate::Heuristic,
    recent_context: &[String],
    user_text: &str,
    cancellation: &CancellationToken,
) -> AdmittedSmartPreGate {
    let started_at = chrono::Utc::now();
    if heuristic == pre_gate::Heuristic::EnterLoop {
        return AdmittedSmartPreGate {
            outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::HeuristicEnter),
            provider_outcome: None,
            started_at,
        };
    }
    let classifier = match decision_turn.smart_group_classifier() {
        Ok(classifier) => classifier,
        Err(error) => {
            tracing::warn!(
                "admitted smart-group classifier unavailable; failing open: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return AdmittedSmartPreGate {
                outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                provider_outcome: None,
                started_at,
            };
        }
    };
    let pending = match classifier.begin() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(
                "admitted smart-group classifier observation refused; failing open: {}",
                providers::sanitize_api_error(&error.to_string())
            );
            return AdmittedSmartPreGate {
                outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                provider_outcome: None,
                started_at,
            };
        }
    };
    let provider = classifier.provider();
    let model = classifier.model().to_owned();
    let accounting = classifier.accounting();
    let cfg = &config_generation.effective.smart_group;
    let result = pre_gate::classify_with_model(
        provider.as_ref(),
        &model,
        cfg,
        recent_context,
        user_text,
        &ctx.bot_names,
        accounting,
        cancellation,
    )
    .await;
    match result.call {
        pre_gate::ClassifierCall::Completed(trace) => {
            if let Err(error) = classifier.complete(pending, &trace) {
                tracing::warn!(
                    "admitted smart-group classifier settlement failed; failing open: {}",
                    providers::sanitize_api_error(&error.to_string())
                );
                return AdmittedSmartPreGate {
                    outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                    provider_outcome: None,
                    started_at,
                };
            }
            let provider_outcome = classifier.provider_outcome(&trace, started_at);
            if !result.outcome.should_enter_loop()
                && let Err(error) = classifier.authorize_silence()
            {
                tracing::warn!(
                    "admitted smart-group classifier silence authorization failed; failing open: {}",
                    providers::sanitize_api_error(&error.to_string())
                );
                return AdmittedSmartPreGate {
                    outcome: pre_gate::PreGateOutcome::enter(pre_gate::PreGatePath::ClassifierFailOpen),
                    provider_outcome: None,
                    started_at,
                };
            }
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: Some(provider_outcome),
                started_at,
            }
        }
        pre_gate::ClassifierCall::Failed => {
            classifier.fail(pending, false);
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: None,
                started_at,
            }
        }
        pre_gate::ClassifierCall::Cancelled => {
            classifier.fail(pending, true);
            AdmittedSmartPreGate {
                outcome: result.outcome,
                provider_outcome: None,
                started_at,
            }
        }
    }
}

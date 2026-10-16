use super::{HistoryEntry, SpawnScope, SubAgentStatus};
use crate::channels::traits::{Channel, SendMessage};
use crate::security::SecurityPolicy;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Trusted provenance of the turn that launched a run, kept for the moment that
/// run finally announces.
///
/// A sub-agent announcement is a real outbound message whose `recipient` comes
/// straight from the model's tool call, but it leaves the process long after the
/// launching turn ended — there is no turn context left to authorize it against.
/// The only trustworthy identity is the one captured atomically from the
/// launching message's `_zc_scope`, in the very same snapshot `recipient` and
/// `channel_name` come from. Nothing may stand in for it: a synthesised
/// "system" sender would make the outbound ACL *look* enforced while letting
/// every announcement through, which is worse than not gating the path at all.
#[derive(Debug, Clone)]
pub(crate) struct AnnounceOrigin {
    pub(super) sender: String,
    pub(super) chat_type: String,
}

impl AnnounceOrigin {
    /// Build an origin from an identity another module already established as
    /// trusted.
    ///
    /// The one caller is [`crate::tools::chat_assignment`], which relays a chat
    /// session's result to the correspondent who asked for it and takes that
    /// correspondent from the origin the mailbox *recorded*, not from anything
    /// the model or the result payload said. Same rule as `from_scope`: an
    /// identity that cannot be established is `None` and degrades to `"unknown"`,
    /// never to a privileged one.
    pub(crate) fn from_parts(sender: &str, chat_type: &str) -> Self {
        Self {
            sender: sender.to_string(),
            chat_type: chat_type.to_string(),
        }
    }

    /// Capture a turn's identity from its trusted scope.
    ///
    /// Two callers with the same shape and different timing: a spawn snapshots
    /// the *launching* turn here because its announcement fires after that turn
    /// is gone, while `action='kill'` reads the *killing* turn's scope off its
    /// own arguments, since that turn is still running when its receipt goes out.
    ///
    /// `None` — an absent or untrusted `_zc_scope`, i.e. the CLI and
    /// single-channel paths — degrades to the same `"unknown"` identity
    /// `message_send` falls back to in exactly that situation (see
    /// `trusted_outbound_identity` there), never to a privileged one.
    pub(super) fn from_scope(scope: Option<&SpawnScope>) -> Option<Self> {
        scope.map(|scope| Self {
            sender: scope.sender.clone(),
            chat_type: scope.chat_type.clone(),
        })
    }
}

/// Outbound recipient authorization for one message this tool emits by itself —
/// a run's result announcement or the receipt for a kill.
///
/// The counterpart of `MessageSendTool::authorize_outbound`: both funnel into
/// [`SecurityPolicy::is_outbound_allowed`], so a `send_allow` / `send_deny` rule
/// an operator writes governs the announcement path as well as the tool path.
/// Without this the model-supplied `recipient` on a `sessions_spawn` call was a
/// complete bypass of that ACL — spawn a throwaway task, name any recipient, and
/// both its completion notice *and* the receipt for killing it are delivered
/// with zero authorization.
///
/// Source and destination channel are deliberately the same value, and that is
/// load-bearing rather than lazy. Neither notice has a channel argument: both
/// always leave on the channel the run resolved for itself, so that channel *is*
/// both the anchor and the destination — there is no cross-channel capability on
/// this path to defend. Passing the captured scope channel instead
/// would diverge exactly when [`SessionsSpawnTool::resolve_announce_channel`]
/// falls back to the shared active channel (a name absent from the registry —
/// every single-channel deployment), source and destination would then differ,
/// the channel default would flip to deny, and every announcement in a
/// deployment with no rules configured at all would vanish.
///
/// MUTATION GUARD: pass the origin's own channel as `src_channel` here and
/// `announce_is_not_denied_by_a_channel_registry_fallback` goes red. That test is
/// the zero-regression tripwire for this decision, not a style preference.
pub(crate) fn announce_is_authorized(
    security: &SecurityPolicy,
    origin: Option<&AnnounceOrigin>,
    dst_channel: &str,
    recipient: &str,
) -> bool {
    let (sender, chat_type) = origin.map_or(("unknown", "unknown"), |origin| {
        (origin.sender.as_str(), origin.chat_type.as_str())
    });
    security.is_outbound_allowed(sender, dst_channel, chat_type, dst_channel, recipient)
}

/// Which of this tool's self-initiated messages is being delivered.
///
/// Three call sites — the task-mode result, the process-mode result and the
/// receipt for `action='kill'` — share one delivery path, so the only thing that
/// legitimately differs between them (operator-facing wording) lives here. The
/// authorization itself deliberately has no per-site variant: that is what makes
/// it impossible to fix the gate on one notice and leave another ungated, which
/// is precisely how the kill receipt stayed a bypass after the announcement was
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutboundNotice {
    /// A finished task-mode run reporting its own result.
    RunResult,
    /// A finished process-mode run reporting its own result.
    ProcessRunResult,
    /// The receipt for an operator's kill of a run.
    KillNotice,
}

impl OutboundNotice {
    /// How the withholding record names this notice to the operator.
    const fn withheld_label(self) -> &'static str {
        match self {
            Self::RunResult | Self::ProcessRunResult => "Announcement",
            Self::KillNotice => "Kill notice",
        }
    }

    /// Log context when the channel itself rejects the send (transport failure,
    /// not authorization).
    const fn send_failure_context(self) -> &'static str {
        match self {
            Self::RunResult => "Failed to announce sub-agent result",
            Self::ProcessRunResult => "Failed to announce sub-agent process result",
            Self::KillNotice => "Failed to announce sub-agent kill",
        }
    }
}

/// Record a notice the outbound ACL refused.
///
/// A refusal must never be a silent drop. It lands in the log *and* on the run's
/// own history, where `sessions_spawn action='history'` and `session_status`
/// surface it to the operator wondering why no result ever arrived. Both carry
/// only the destination's stable audit fingerprint, never the plaintext
/// recipient — the same rule `message_send`'s rejection text follows.
async fn record_withheld_notice(
    history: &Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: &str,
    dst_channel: &str,
    recipient: &str,
    notice: OutboundNotice,
) {
    let recipient_ref = crate::security::op_id::ref_for_channel_recipient(dst_channel, recipient);
    let label = notice.withheld_label();
    tracing::warn!(
        run_id = %run_id,
        channel = %dst_channel,
        recipient = %recipient_ref,
        notice = %label,
        "Sub-agent outbound notice withheld: the configured scope rules do not permit this recipient"
    );
    history.write().await.push(HistoryEntry {
        role: "system".to_string(),
        content: crate::security::redaction::redact_secrets(&format!(
            "{label} withheld: outbound messaging to recipient {recipient_ref} on channel \
             '{dst_channel}' is not permitted by the configured scope rules"
        )),
        timestamp: Utc::now(),
    });
}

/// The one path by which this tool puts a message on a channel by its own
/// initiative — result announcement or kill receipt alike.
///
/// Folding the three call sites into a single function is the substance of this
/// gate, not tidiness. While each site inlined its own copy, "the process-mode
/// block is a verbatim mirror of the task-mode one" was an argument, not
/// evidence, and the kill receipt — carrying the *same* model-chosen
/// `recipient` — simply never grew a copy at all. With one implementation there
/// is one thing to test and nothing left to forget.
///
/// MUTATION GUARD: delete the [`announce_is_authorized`] branch here and four
/// tests go red at once —
/// `announce_to_a_denied_recipient_is_withheld_and_recorded_on_the_run`,
/// `a_model_supplied_recipient_cannot_bypass_send_deny`,
/// `a_kill_notice_to_a_denied_recipient_is_withheld_and_recorded_on_the_run` and
/// `a_model_supplied_recipient_cannot_bypass_send_deny_through_the_kill_notice`.
pub(super) async fn deliver_or_withhold_notice(
    security: &SecurityPolicy,
    origin: Option<&AnnounceOrigin>,
    channel: &dyn Channel,
    history: &Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: &str,
    recipient: &str,
    text: &str,
    notice: OutboundNotice,
) {
    if !announce_is_authorized(security, origin, channel.name(), recipient) {
        record_withheld_notice(history, run_id, channel.name(), recipient, notice).await;
        return;
    }
    let message = SendMessage::new(text, recipient);
    if let Err(error) = channel.send(&message).await {
        tracing::error!(run_id = %run_id, "{}: {error}", notice.send_failure_context());
    }
}

pub(super) fn format_announce_message(rid: &str, status: &SubAgentStatus, result_text: &str) -> String {
    let summary = compact_announce_summary(result_text);
    match status {
        SubAgentStatus::Completed(_) if summary.is_empty() => format!("🤖 Sub-agent `{rid}` completed"),
        SubAgentStatus::Completed(_) => format!("🤖 Sub-agent `{rid}` completed: {summary}"),
        _ if summary.is_empty() => format!("🤖 Sub-agent `{rid}` FAILED with an error"),
        _ => format!("🤖 Sub-agent `{rid}` FAILED with error: {summary}"),
    }
}

const ANNOUNCE_SUMMARY_MAX_CHARS: usize = 120;

fn compact_announce_summary(text: &str) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| is_useful_announce_line(line)) else {
        return String::new();
    };
    clamp_announce_chars(line, ANNOUNCE_SUMMARY_MAX_CHARS)
}

fn is_useful_announce_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~")
}

fn clamp_announce_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        match chars.next() {
            Some(ch) => out.push(ch),
            None => return out,
        }
    }
    if chars.next().is_some() && max_chars > 0 {
        out.pop();
        out.push('…');
    }
    out
}

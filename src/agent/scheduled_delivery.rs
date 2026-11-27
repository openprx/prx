//! Delivery of a successful scheduled agent reply into the session that
//! created its job (`deliver = message`).
//!
//! The reply goes out as a mailbox message from the scheduled session to the
//! creating message's session, which wakes a turn there. Every case without a
//! resolvable creating session, and every failed send, reports a non-delivery,
//! and the caller keeps the result as a note.
use crate::agent::diagnostics::error_class;
use crate::agent::scheduled_source::ScheduledResultMeta;
use crate::config::Config;
use crate::memory::Memory;
use crate::messaging::runtime::MessagingRuntime;
use crate::runtime::envelope::RuntimeEnvelope;
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// Characters of the reply carried by the message, after redaction.
const MESSAGE_CHARS: usize = 16_000;

/// The outcome of one delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// The message was accepted into the creating session.
    Sent,
    /// No creating session can be resolved for this run.
    NoTarget,
    /// The send was refused or the messaging backend is unavailable.
    Failed,
}

impl Delivered {
    pub(crate) const fn is_sent(self) -> bool {
        matches!(self, Self::Sent)
    }
}

/// Sends the reply of a successful scheduled run into the session that created
/// its job. Only the outcome class is logged, never the reply or the ids.
pub(crate) async fn deliver_message(
    memory: &Arc<dyn Memory>,
    config: &Config,
    envelope: &RuntimeEnvelope,
    meta: &ScheduledResultMeta,
    reply: &str,
) -> Delivered {
    // An owner-less task (a local operator's CLI job) has no creating session.
    if !meta.has_owner {
        return Delivered::NoTarget;
    }
    let (Some(source_event_id), Some(run_id)) = (meta.source_event_id.as_deref(), envelope.run_id.as_deref()) else {
        return Delivered::NoTarget;
    };
    let owner = envelope.resolved_owner_id();
    let event = match memory
        .find_owned_message_event(&envelope.workspace_id, &owner, source_event_id)
        .await
    {
        Ok(Some(event)) => event,
        Ok(None) => return Delivered::NoTarget,
        Err(error) => {
            tracing::warn!(error = %error_class(&error), "Scheduled result creator lookup failed");
            return Delivered::NoTarget;
        }
    };
    let Some(session) = event.session_key.as_deref() else {
        return Delivered::NoTarget;
    };
    let runtime = match MessagingRuntime::for_config(config) {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(error = %error_class(&error), "Scheduled result messaging unavailable");
            return Delivered::Failed;
        }
    };
    let source = runtime.address(
        &owner,
        &envelope.session_key,
        envelope.agent_id.as_deref().unwrap_or("main"),
    );
    let target = runtime.address(&owner, session, event.agent_id.as_deref().unwrap_or("main"));
    let text = message_text(meta, envelope, reply);
    let sender = match runtime.bind(source, None) {
        Ok(sender) => sender,
        Err(error) => {
            tracing::warn!(error = %error_class(&error), "Scheduled result sender refused");
            return Delivered::Failed;
        }
    };
    match sender
        .send_with_id(
            target,
            text,
            "text/plain".to_owned(),
            delivery_id(&meta.task_id, run_id),
        )
        .await
    {
        Ok(_) => Delivered::Sent,
        Err(error) => {
            tracing::warn!(error = %error_class(&error), "Scheduled result message refused");
            Delivered::Failed
        }
    }
}

/// The message body: a header naming the job and its finish time, the bounded
/// reply, and the transcript session the run wrote.
fn message_text(meta: &ScheduledResultMeta, envelope: &RuntimeEnvelope, reply: &str) -> String {
    let label = meta.name.as_deref().unwrap_or(meta.task_id.as_str());
    format!(
        "[Scheduled result] {label} finished at {}:\n\n{}\n\n(transcript {})",
        chrono::Utc::now().to_rfc3339(),
        bounded_reply(reply),
        envelope.session_key,
    )
}

/// The reply after the persisted-text redaction, cut at `MESSAGE_CHARS`.
fn bounded_reply(reply: &str) -> String {
    let redacted = crate::agent::terminal::redact_persisted_text(reply);
    if redacted.chars().count() <= MESSAGE_CHARS {
        return redacted;
    }
    let mut cut: String = redacted.chars().take(MESSAGE_CHARS).collect();
    cut.push_str(" [truncated]");
    cut
}

/// A message id derived from the job and the run, so a run recorded twice
/// (a retry of the record path) sends one message.
fn delivery_id(task_id: &str, run_id: &str) -> Uuid {
    let digest = Sha256::digest(format!("prx-scheduled-result:{task_id}:{run_id}").as_bytes());
    let mut bytes = [0_u8; 16];
    for (slot, byte) in bytes.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    Uuid::from_bytes(bytes)
}

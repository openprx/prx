//! `prx chat --message`: one turn through the plain chat turn path, the reply
//! alone on stdout (AR-R1..R3).
//!
//! The turn runs inside `run_internal` exactly like a `--plain` turn
//! (Interactive profile, chat tool set, memory recall, terminal commit, cost
//! outbox); this module only supplies the one literal input, reads how the
//! turn ended, and maps that to the process status: 0 answered, 2 refused by
//! policy (reply still on stdout), 1 everything else.

use std::io::Write as _;

use tokio_util::sync::CancellationToken;

use crate::chat::binding::{ChatTurnBinding, TurnFailureNote};
use crate::chat::input_queue::{InputQueuePriority, QueuedInputMessage};
use crate::config::Config;
use crate::providers::ChatMessage;

/// Process status of a refused one-shot turn; `1` stays every other failure.
pub const ONE_SHOT_REFUSAL_EXIT_CODE: u8 = 2;

/// A one-shot turn refused by policy. Its reply is already on stdout, so the
/// CLI maps this to [`ONE_SHOT_REFUSAL_EXIT_CODE`] without printing anything.
#[derive(Debug, thiserror::Error)]
#[error("chat turn refused")]
pub struct OneShotRefused {
    /// The refusal reply written to stdout.
    pub reply: String,
}

/// Map a one-shot result to a process status: refusal 2, other errors 1.
#[must_use]
pub fn one_shot_exit_code(error: Option<&anyhow::Error>) -> u8 {
    match error {
        None => 0,
        Some(error) if error.is::<OneShotRefused>() => ONE_SHOT_REFUSAL_EXIT_CODE,
        Some(_) => 1,
    }
}

/// What `prx chat --message` asked for.
#[derive(Debug, Clone)]
pub struct ChatMessageRequest {
    /// Literal model input; slash text is never a command (D4).
    pub message: String,
    /// `--save`: persist the turn as a resumable chat session.
    pub save: bool,
    /// `--session <id>`, or `last` for `--continue` (D3). Implies `save`; an
    /// unknown session is an error, never a fresh session (D2).
    pub session: Option<String>,
}

/// The one input a one-shot `run_internal` runs.
pub(super) struct OneShotTurn {
    message: String,
    persist: bool,
}

impl OneShotTurn {
    /// Whether the turn is saved as a chat session row.
    pub(super) const fn persists(&self) -> bool {
        self.persist
    }

    /// Queue the message as an ordinary local operator input, without the
    /// priority-prefix rewrite typed input gets.
    pub(super) fn enqueue(
        &self,
        backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
        scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
        history_base_len: usize,
    ) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let msg = crate::channels::traits::ChannelMessage {
            id: uuid::Uuid::new_v4().to_string(),
            sender: "user".to_owned(),
            reply_target: "user".to_owned(),
            content: self.message.clone(),
            channel: "terminal".to_owned(),
            timestamp,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: Vec::new(),
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        let turn_task_id = scheduler.enqueue(
            msg.content.clone(),
            crate::chat::turn_scheduler::TurnPriority::Normal,
            history_base_len,
        );
        backlog.push_back(QueuedInputMessage {
            priority: InputQueuePriority::Normal,
            turn_task_id: Some(turn_task_id),
            msg,
            terminal_origin: None,
        });
    }
}

/// How the one-shot turn ended when it produced a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OneShotReply {
    /// The committed reply (empty when the model answered with no text).
    Answered(String),
    /// A policy refusal; the user-facing refusal text.
    Refused(String),
}

/// Record why the line-mode turn failed on its binding: a send-admission
/// refusal anywhere in the error chain is noted as a refusal (the one-shot
/// exits 2 with its reply), any other error as a failure (exit 1).
pub(super) fn note_turn_failure(binding: Option<&ChatTurnBinding>, error: &anyhow::Error) {
    if let Some(binding) = binding {
        binding.note_failure(TurnFailureNote::from_error(error));
    }
}

/// Read the turn's end: the driver's refusal note first, then a failure, then
/// a committed answer; a turn that committed nothing and did not fail was
/// cancelled (Ctrl-C), which is a failure for the process.
pub(super) fn outcome(
    binding: Option<&ChatTurnBinding>,
    turn_failed: bool,
    answered: bool,
    history: &[ChatMessage],
) -> anyhow::Result<OneShotReply> {
    match binding.and_then(ChatTurnBinding::failure_note) {
        Some(TurnFailureNote::Refused { reply }) => return Ok(OneShotReply::Refused(reply.clone())),
        Some(TurnFailureNote::Failed { message }) if turn_failed => anyhow::bail!("{message}"),
        _ => {}
    }
    anyhow::ensure!(!turn_failed, "the chat turn failed");
    anyhow::ensure!(answered, "the chat turn was cancelled");
    let reply = history
        .last()
        .filter(|message| message.role == "assistant")
        .map(|message| message.content.clone())
        .unwrap_or_default();
    Ok(OneShotReply::Answered(reply))
}

/// Run `prx chat --message`: one turn, the reply on stdout, then exit.
///
/// # Errors
///
/// [`OneShotRefused`] for a policy refusal (after its reply is printed); any
/// other error for an empty message, an unknown session, a failed or
/// cancelled turn.
pub async fn run_message(
    config: Config,
    provider: Option<String>,
    model: Option<String>,
    temperature: f64,
    request: ChatMessageRequest,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(!request.message.trim().is_empty(), "the message is empty");
    crate::channels::terminal::reserve_stdout_for_reply();
    let turn = OneShotTurn {
        persist: request.save || request.session.is_some(),
        message: request.message,
    };
    let result = super::run_internal(
        config,
        provider,
        model,
        temperature,
        true,
        request.session,
        false,
        shutdown,
        None,
        Some(turn),
    )
    .await;
    let reply = match result {
        Ok(Some(reply)) => reply,
        Ok(None) => anyhow::bail!("the chat turn produced no outcome"),
        // A refusal raised before the driver (at decision admission) is the
        // same policy outcome as one raised at send admission.
        Err(error) => match TurnFailureNote::from_error(&error) {
            TurnFailureNote::Refused { reply } => OneShotReply::Refused(reply),
            TurnFailureNote::Failed { .. } => return Err(error),
        },
    };
    match reply {
        OneShotReply::Answered(text) => write_reply(&text),
        OneShotReply::Refused(reply) => {
            write_reply(&reply)?;
            Err(OneShotRefused { reply }.into())
        }
    }
}

/// The reply and one newline: the whole of stdout.
fn write_reply(text: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_maps_to_exit_two_and_other_errors_to_one() {
        let refused = anyhow::Error::new(OneShotRefused { reply: "no".to_owned() });
        assert_eq!(one_shot_exit_code(None), 0);
        assert_eq!(one_shot_exit_code(Some(&refused)), ONE_SHOT_REFUSAL_EXIT_CODE);
        assert_eq!(one_shot_exit_code(Some(&anyhow::anyhow!("boom"))), 1);
    }

    /// The line-mode loop's failure note decides the one-shot status: a
    /// send-admission refusal (even wrapped in context) is read back as the
    /// refusal reply and exit 2, any other failure as its message and exit 1.
    #[tokio::test]
    async fn line_mode_failure_note_classifies_refusal_and_failure() {
        let refused = crate::agent::send_admission::ProviderSendRefused::budget_for_test("prx test refusal");
        let expected_reply = refused.reply().into_owned();
        let refusal = anyhow::Error::new(refused).context("provider round failed");
        let (_dir, _fabric, binding) =
            crate::chat::binding::tests::fixture_with_advisory(false, Default::default()).await;
        note_turn_failure(Some(&binding), &refusal);
        assert_eq!(
            binding.failure_note(),
            Some(&TurnFailureNote::Refused {
                reply: expected_reply.clone()
            }),
            "a wrapped send-admission refusal must be noted as a refusal with its reply"
        );
        let reply = outcome(Some(&binding), true, false, &[]);
        assert_eq!(
            reply.as_ref().ok(),
            Some(&OneShotReply::Refused(expected_reply.clone())),
            "the one-shot must read the noted refusal back as its reply: {reply:?}"
        );
        let refused_exit = anyhow::Error::new(OneShotRefused { reply: expected_reply });
        assert_eq!(
            one_shot_exit_code(Some(&refused_exit)),
            ONE_SHOT_REFUSAL_EXIT_CODE,
            "a refused one-shot exits 2"
        );

        let (_dir, _fabric, binding) =
            crate::chat::binding::tests::fixture_with_advisory(false, Default::default()).await;
        note_turn_failure(Some(&binding), &anyhow::anyhow!("prx provider transport broke"));
        assert_eq!(
            binding.failure_note(),
            Some(&TurnFailureNote::Failed {
                message: "prx provider transport broke".to_owned()
            }),
            "any other error must be noted as a failure with its message"
        );
        let failed = outcome(Some(&binding), true, false, &[]);
        let message = failed.as_ref().err().map(ToString::to_string);
        assert_eq!(
            message.as_deref(),
            Some("prx provider transport broke"),
            "the one-shot must fail with the noted message, not the generic text: {failed:?}"
        );
        let failed_exit = failed.err();
        assert_eq!(one_shot_exit_code(failed_exit.as_ref()), 1, "a failed one-shot exits 1");
    }

    #[test]
    fn outcome_reads_answer_failure_and_cancellation() {
        let history = vec![ChatMessage::user("q"), ChatMessage::assistant("pong")];
        assert_eq!(
            outcome(None, false, true, &history).ok(),
            Some(OneShotReply::Answered("pong".to_owned()))
        );
        let failed = outcome(None, true, false, &history)
            .err()
            .map(|error| error.to_string());
        assert_eq!(failed.as_deref(), Some("the chat turn failed"));
        let cancelled = outcome(None, false, false, &history)
            .err()
            .map(|error| error.to_string());
        assert_eq!(cancelled.as_deref(), Some("the chat turn was cancelled"));
    }
}

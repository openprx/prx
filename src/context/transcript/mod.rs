//! The transcript: the one way to append to and read a session's log.
//!
//! Writers live in [`tool_rows`] (tool calls, tool results and the attachment
//! of a user message). Readers live in [`replay`] and are reached through
//! [`Transcript`], a handle for one session. Every row written here has role
//! `tool` and an event type other than `message.created`, so the sync stream,
//! the history loaders and the handoff provenance match never see it.
//!
//! `conversation_turns` stays a display projection; it is only an input to
//! [`Transcript::expand_turns`] until the loader reads the log itself.

mod handoff;
mod replay;
mod sources;
mod tool_rows;

pub(crate) use replay::{Attachment, History, ReplayLog, TurnReplay, stub_replayed_results};
pub(crate) use sources::AttachmentSources;
pub(crate) use tool_rows::{RoundLog, append_attachment, call_id_for};

use crate::memory::session_transcript::SessionTranscriptUnsupported;
use crate::memory::{ConversationTurn, Memory, MemoryPrincipal, MessageEvent};

/// Read handle for the log of one session.
pub(crate) struct Transcript<'a> {
    memory: &'a dyn Memory,
    principal: MemoryPrincipal,
}

impl<'a> Transcript<'a> {
    /// `principal` names the session and the visibility the reads run with.
    pub(crate) fn new(memory: &'a dyn Memory, principal: MemoryPrincipal) -> Self {
        Self { memory, principal }
    }

    /// The user and assistant turns of `session_key`, oldest first, at most the
    /// newest `limit`, read from the log (`message_events`) through the
    /// session's own rows: owner, session and agent partition only. A mailbox
    /// adoption row is present, a continuation input is not, and a deleted turn
    /// is not. A backend without that read answers from the display projection
    /// (`list_console_conversation_turns`), which is the same selection.
    pub(crate) async fn load_turns(&self, session_key: &str, limit: usize) -> anyhow::Result<Vec<ConversationTurn>> {
        match self
            .memory
            .session_transcript_rows(&self.principal, session_key, limit)
            .await
        {
            Ok(rows) => Ok(rows.into_iter().map(turn_of_event).collect()),
            Err(error) if error.is::<SessionTranscriptUnsupported>() => {
                self.memory
                    .list_console_conversation_turns(&self.principal, session_key, limit, 0)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// The turns a model history of `session_key` starts from: [`Self::load_turns`],
    /// except that the newest valid context handoff of the session's log, when
    /// there is one, comes first, with its stored bytes, followed by the loaded
    /// turns after its row range. A failed handoff read leaves the turns as
    /// [`Self::load_turns`] returns them.
    pub(crate) async fn load_model_turns(
        &self,
        session_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let turns = self.load_turns(session_key, limit).await?;
        let found = match handoff::newest_valid_handoff(self.memory, &self.principal, session_key).await {
            Ok(found) => found,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(
                    error_len,
                    error_sha256,
                    "transcript could not read the session's context handoff; loading all turns"
                );
                None
            }
        };
        let Some((event, handoff)) = found else {
            return Ok(turns);
        };
        let covered_through = handoff.last_row_id;
        let mut model = Vec::with_capacity(turns.len() + 1);
        model.push(turn_of_event(event));
        model.extend(turns.into_iter().filter(|turn| turn.id > covered_through));
        Ok(model)
    }

    /// The tool, attachment and message rows of the session. A failed read
    /// leaves the log empty, so history is then replayed without them.
    pub(crate) async fn replay_log(&self) -> ReplayLog {
        ReplayLog::load(self.memory, &self.principal).await
    }

    /// As [`Self::replay_log`] without the user and assistant rows: enough to
    /// replay the tool rounds of one run.
    pub(crate) async fn tool_log(&self) -> ReplayLog {
        ReplayLog::load_tool_rows(self.memory, &self.principal).await
    }

    /// The stored `turns` as model-facing history: each user message with its
    /// stored attachment, each reply preceded by the tool rounds of its run.
    pub(crate) async fn expand_turns(&self, turns: Vec<ConversationTurn>) -> History {
        self.replay_log().await.expand_turns(turns)
    }
}

/// A stored message row as the turn the history expansion reads.
fn turn_of_event(event: MessageEvent) -> ConversationTurn {
    ConversationTurn {
        id: event.id,
        session_key: event.session_key.unwrap_or_default(),
        role: event.role,
        content: event.content,
        timestamp: event.created_at,
        message_id: Some(event.event_id.clone()),
        message_event_id: Some(event.event_id),
    }
}

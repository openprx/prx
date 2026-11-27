//! The transcript: the one way to append to and read a session's log.
//!
//! Writers live in [`tool_rows`] (tool calls, tool results and the attachment
//! of a user message) and [`skill_rows`] (successful skill instruction loads).
//! Readers live in [`replay`] and are reached through [`Transcript`], a handle
//! for one session. Every row written here has role
//! `tool` and an event type other than `message.created`, so the sync stream,
//! the history loaders and the handoff provenance match never see it.
//!
//! `conversation_turns` stays a display projection; it is only an input to
//! [`Transcript::expand_turns`] until the loader reads the log itself.

mod handoff;
mod hook_rows;
mod prefix_rows;
mod replay;
mod schedule_rows;
mod skill_rows;
mod sources;
mod tool_rows;

pub(crate) use hook_rows::append_hook_contributions;
pub(crate) use prefix_rows::{
    HOOK_SOURCE_PREFIX, MEMORY_INDEX_SOURCE, PrefixSection, append_session_prefix, newest_session_prefix,
    render_prefix,
};
pub(crate) use replay::{Attachment, History, ReplayLog, TurnReplay, stub_replayed_results};
pub(crate) use schedule_rows::{SCHEDULE_SEEN_EVENT_TYPE, mark_results_seen, unseen_results_note};
pub(crate) use skill_rows::{SkillLoad, skill_load_of, skills_in_use};
pub(crate) use sources::AttachmentSources;
pub(crate) use tool_rows::{RoundLog, append_attachment, call_id_for};

use crate::memory::session_transcript::SessionTranscriptUnsupported;
use crate::memory::{ConversationTurn, Memory, MemoryPrincipal, MessageEvent};

/// Read handle for the log of one session.
pub(crate) struct Transcript<'a> {
    memory: &'a dyn Memory,
    principal: MemoryPrincipal,
}

/// The turns of one session as loaded, with whether they came from the log
/// (`message_events`) or from the display projection, whose ids are not
/// `message_events` ids.
pub(crate) struct SessionTurns {
    pub(crate) turns: Vec<ConversationTurn>,
    from_log: bool,
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
    /// The user and assistant turns of `session_key`, oldest first, with the
    /// fact that they came from the log (`session_transcript_rows`) rather than
    /// from the display projection. The log read is the same selection as the
    /// Console history (owner, session and agent partition only; a mailbox
    /// adoption row is present, a continuation input and a deleted turn are not).
    pub(crate) async fn load_session_turns(&self, session_key: &str, limit: usize) -> anyhow::Result<SessionTurns> {
        match self
            .memory
            .session_transcript_rows(&self.principal, session_key, limit)
            .await
        {
            Ok(rows) => Ok(SessionTurns {
                turns: rows.into_iter().map(turn_of_event).collect(),
                from_log: true,
            }),
            Err(error) if error.is::<SessionTranscriptUnsupported>() => Ok(SessionTurns {
                turns: self
                    .memory
                    .list_console_conversation_turns(&self.principal, session_key, limit, 0)
                    .await?,
                from_log: false,
            }),
            Err(error) => Err(error),
        }
    }

    /// The model's view of a session loaded by [`Self::load_session_turns`]: its
    /// turns, except that the newest valid context handoff of the session's log,
    /// when there is one, comes first, with its stored bytes, followed by the
    /// turns after its row range. Turns from the display projection are returned
    /// unchanged, since their ids are not `message_events` ids. A failed handoff
    /// read leaves the turns as loaded.
    pub(crate) async fn model_turns_from(&self, session_key: &str, loaded: SessionTurns) -> Vec<ConversationTurn> {
        if !loaded.from_log {
            return loaded.turns;
        }
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
            return loaded.turns;
        };
        let covered_through = handoff.last_row_id;
        let mut model = Vec::with_capacity(loaded.turns.len() + 1);
        model.push(turn_of_event(event));
        model.extend(loaded.turns.into_iter().filter(|turn| turn.id > covered_through));
        model
    }

    /// [`Self::load_session_turns`] followed by [`Self::model_turns_from`].
    pub(crate) async fn load_model_turns(
        &self,
        session_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let loaded = self.load_session_turns(session_key, limit).await?;
        Ok(self.model_turns_from(session_key, loaded).await)
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

    /// The document ids that the tool results of `run_id` in this session's
    /// log point at, in call order. Read from the tool rows only; a failed read
    /// gives none.
    pub(crate) async fn run_document_ids(&self, run_id: &str) -> Vec<String> {
        self.tool_log().await.document_ids_of_run(run_id)
    }

    /// The stored `turns` as model-facing history: each user message with its
    /// stored attachment, each reply preceded by the tool rounds of its run.
    pub(crate) async fn expand_turns(&self, turns: Vec<ConversationTurn>) -> History {
        self.replay_log().await.expand_turns(turns)
    }

    /// As [`Self::expand_turns`], with whether a read window came back full
    /// (older tool rows may then be missing). For displays only.
    pub(crate) async fn expand_turns_noted(&self, turns: Vec<ConversationTurn>) -> (History, bool) {
        let log = self.replay_log().await;
        let full = log.window_full();
        (log.expand_turns(turns), full)
    }

    /// The stored `turns` as history without any tool rows.
    pub(crate) fn expand_turns_plain(turns: Vec<ConversationTurn>) -> History {
        ReplayLog::default().expand_turns(turns)
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

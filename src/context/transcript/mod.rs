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

mod replay;
mod tool_rows;

pub(crate) use replay::{Attachment, History, ReplayLog, TurnReplay, stub_replayed_results};
pub(crate) use tool_rows::{RoundLog, append_attachment, call_id_for};

use crate::memory::{ConversationTurn, Memory, MemoryPrincipal};

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

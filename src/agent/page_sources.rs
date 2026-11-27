//! The message event ids a context page was built from.
//!
//! A canonical message may derive from stored `message_events` rows (a stored
//! turn, the request) or from this turn's own output, whose events are not yet
//! written. [`MessageSources`] records that per message.

use std::collections::BTreeSet;

/// Where one canonical message came from, as event ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MessageSources {
    /// Stored `message_events.event_id` values the message derives from.
    event_ids: Vec<String>,
    /// The message was appended by the running turn, whose events are written
    /// after it (the turn's input event is the page's turn event).
    turn_round: bool,
}

impl MessageSources {
    /// A stored turn (or the request) with its event id, when it has one.
    pub(crate) fn stored(event_id: Option<&str>) -> Self {
        Self {
            event_ids: event_id.map(str::to_owned).into_iter().collect(),
            turn_round: false,
        }
    }

    /// A message the running turn appended.
    pub(crate) fn turn_round() -> Self {
        Self {
            event_ids: Vec::new(),
            turn_round: true,
        }
    }

    /// Whether the message derives from at least one stored event.
    pub(crate) fn has_stored_event(&self) -> bool {
        !self.event_ids.is_empty()
    }

    /// The stored event ids the message derives from (empty when none).
    pub(crate) fn event_ids(&self) -> &[String] {
        &self.event_ids
    }

    /// The union of `sources`: a message derived from all of them.
    pub(crate) fn union(sources: &[Self]) -> Self {
        let event_ids: BTreeSet<&String> = sources.iter().flat_map(|source| &source.event_ids).collect();
        Self {
            event_ids: event_ids.into_iter().cloned().collect(),
            turn_round: sources.iter().any(|source| source.turn_round),
        }
    }
}

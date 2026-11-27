//! The message event ids a context page was built from.
//!
//! A canonical message may derive from stored `message_events` rows (a stored
//! turn, the request) or from this turn's own output, whose events are not yet
//! written. [`MessageSources`] records that per message.

/// Where one canonical message came from, as event ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MessageSources {
    /// Stored `message_events.event_id` values the message derives from.
    event_ids: Vec<String>,
}

impl MessageSources {
    /// A stored turn (or the request) with its event id, when it has one.
    pub(crate) fn stored(event_id: Option<&str>) -> Self {
        Self {
            event_ids: event_id.map(str::to_owned).into_iter().collect(),
        }
    }

    /// A message the running turn appended: its events are written after it,
    /// so it has no stored event id yet.
    pub(crate) fn turn_round() -> Self {
        Self::default()
    }

    /// Whether the message derives from at least one stored event.
    pub(crate) fn has_stored_event(&self) -> bool {
        !self.event_ids.is_empty()
    }

    /// The stored event ids the message derives from (empty when none).
    pub(crate) fn event_ids(&self) -> &[String] {
        &self.event_ids
    }
}

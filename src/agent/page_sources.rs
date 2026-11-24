//! The message event ids a context page was built from.
//!
//! A canonical message may derive from stored `message_events` rows (a stored
//! turn, the request) or from this turn's own output, whose events are not yet
//! written. [`MessageSources`] records that per message; an OS-paging eviction
//! folds the sources of the evicted messages into the two arrays stored in the
//! page's `metadata_json`, so a message delete removes exactly the pages that
//! hold the deleted event (see `rf_forget::erase_context_pages`).

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

    /// The union of `sources`: a message derived from all of them.
    pub(crate) fn union(sources: &[Self]) -> Self {
        let event_ids: BTreeSet<&String> = sources.iter().flat_map(|source| &source.event_ids).collect();
        Self {
            event_ids: event_ids.into_iter().cloned().collect(),
            turn_round: sources.iter().any(|source| source.turn_round),
        }
    }
}

/// The two id arrays a page records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PageEventIds {
    /// Sorted, deduplicated union of the evicted messages' event ids.
    pub(crate) message_event_ids: Vec<String>,
    /// The running turn's input event id when an evicted message is this
    /// turn's own output, otherwise empty.
    pub(crate) turn_event_ids: Vec<String>,
}

impl PageEventIds {
    /// Fold the evicted messages' `sources` into the page arrays.
    /// `turn_event_id` is the running turn's input event id, if it has one.
    pub(crate) fn from_sources(sources: &[MessageSources], turn_event_id: Option<&str>) -> Self {
        let union = MessageSources::union(sources);
        Self {
            message_event_ids: union.event_ids,
            turn_event_ids: turn_event_id
                .filter(|_| union.turn_round)
                .map(str::to_owned)
                .into_iter()
                .collect(),
        }
    }

    /// Bytes mixed into the page's content-addressed id, so identical text
    /// from different messages never shares a page.
    pub(crate) fn hash_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for id in &self.message_event_ids {
            bytes.extend_from_slice(id.as_bytes());
            bytes.push(b'\n');
        }
        bytes.push(0);
        for id in &self.turn_event_ids {
            bytes.extend_from_slice(id.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }
}

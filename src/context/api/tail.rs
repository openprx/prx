//! The turn tail: text placed after a user message on this turn, before the
//! date line. The tail is stored with the message and replayed with it. Empty
//! until the context hooks (K-H2) and the scheduled-result notes (K-R2) fill it.

use crate::runtime::envelope::RuntimeEnvelope;

/// The text placed after the user's message on this turn. Its inputs are the
/// turn's own, for the lanes that fill it; nothing is read yet.
pub(crate) fn turn_tail(_envelope: &RuntimeEnvelope, _visible_message: &str) -> String {
    String::new()
}

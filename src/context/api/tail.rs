//! The turn tail: text placed after a user message on this turn, before the
//! date line. The tail is stored with the message and replayed with it. It
//! carries the turn-open context hook contributions (K-H2); the scheduled-result
//! notes (K-R2) join it. Replay sends the stored bytes, so a contribution is
//! never recomputed.

use std::sync::Arc;

use crate::config::MemoryConfig;
use crate::context::transcript::{append_hook_contributions, mark_results_seen, unseen_results_note};
use crate::hooks::context::ContextPoint;
use crate::hooks::{HookIdentity, HookManager};
use crate::memory::{Memory, MemoryFabric, MemoryVisibility};
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};

/// Whether this turn is a channel turn of a group-like (session-shared) chat.
/// Its memory index and scheduled-results note stay out of the prompt, for
/// privacy; its hooks still run. Other surfaces that use the session-shared
/// visibility are unaffected.
pub(crate) fn is_group_shared_channel(envelope: &RuntimeEnvelope) -> bool {
    envelope.source == RuntimeSource::Channel && envelope.visibility == MemoryVisibility::Session
}

/// What the turn tail reads, borrowed from the turn's state.
pub(crate) struct TurnTailInput<'a> {
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) envelope: &'a RuntimeEnvelope,
    pub(crate) memory_config: &'a MemoryConfig,
    pub(crate) visible_message: &'a str,
    /// The hook manager of the surface. `None` for a turn with no context
    /// hooks (child turns): nothing runs and nothing is stored.
    pub(crate) hooks: Option<&'a HookManager>,
}

/// The turn tail and what its scheduled-results note was built from.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnTail {
    /// The whole tail: hook blocks, then the note.
    pub(crate) text: String,
    /// The note appended to `text` (empty when none).
    pub(crate) note: String,
    /// Event ids of the result rows the note shows, in note order.
    pub(crate) results: Vec<String>,
}

impl TurnTail {
    /// The note and result ids to store with a tail that was redacted after
    /// the note was built: kept only while the stored tail still contains the
    /// note verbatim, since the later cut of the note matches that text.
    /// Otherwise empty (one log line, digest only).
    pub(crate) fn stored_note<'a>(&'a self, stored_tail: &str) -> (&'a str, &'a [String]) {
        if self.note.is_empty() || stored_tail.contains(&self.note) {
            return (&self.note, &self.results);
        }
        tracing::warn!(
            note = %crate::agent::diagnostics::text_class(&self.note),
            "scheduled-results note changed by redaction; not stored"
        );
        ("", &[])
    }
}

/// The text placed after the user's message on this turn: the `turn_opened`
/// hook blocks, then, on an interactive turn (Console, Chat or Channel, but not
/// a group-shared channel chat), the unseen scheduled-results note. The note's
/// rows are marked seen in the same step. A turn with no source message event
/// id gets no note: the tail is stored with that message only when it has an
/// id, so a note that cannot be stored is not sent either, and stays unseen.
pub(crate) async fn turn_tail(input: TurnTailInput<'_>) -> TurnTail {
    let memory = input.memory;
    let envelope = input.envelope;
    let memory_config = input.memory_config;
    let mut tail = TurnTail {
        text: hook_tail(input).await,
        ..TurnTail::default()
    };
    if !matches!(
        envelope.source,
        RuntimeSource::Console | RuntimeSource::Chat | RuntimeSource::Channel
    ) || is_group_shared_channel(envelope)
        || envelope.source_message_event_id.is_none()
    {
        return tail;
    }
    let principal = envelope.memory_principal();
    let Some(unseen) = unseen_results_note(memory.as_ref(), &principal, chrono::Utc::now()).await else {
        return tail;
    };
    tail.text.push_str(&unseen.text);
    tail.note = unseen.text;
    tail.results = unseen.results;
    let Some(mark) = unseen.mark else {
        return tail;
    };
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    mark_results_seen(&fabric, envelope.message_scope(), mark).await;
    tail
}

/// The `turn_opened` contributions as `[Hook context: <hook>]` blocks, in the
/// order the hook manager returns them. Each contribution gets one audit row.
async fn hook_tail(input: TurnTailInput<'_>) -> String {
    let TurnTailInput {
        memory,
        envelope,
        memory_config,
        visible_message,
        hooks,
    } = input;
    let Some(hooks) = hooks else {
        return String::new();
    };
    let identity = HookIdentity::for_session(envelope);
    let data = serde_json::json!({
        "input_chars": visible_message.chars().count(),
        "surface": HookIdentity::origin_of(envelope.source),
    });
    let contributions = crate::hooks::identity_scope(identity, hooks.contribute(ContextPoint::TurnOpened, data)).await;
    if contributions.is_empty() {
        return String::new();
    }
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    append_hook_contributions(&fabric, &envelope.message_scope(), &contributions).await;
    contributions
        .iter()
        .map(|contribution| format!("\n\n[Hook context: {}]\n{}", contribution.hook, contribution.text))
        .collect()
}

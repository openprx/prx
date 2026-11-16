//! The untrimmed canonical history of one turn and the provider view projected
//! from it (controller rulings r4/r5).
//!
//! The canonical history is the history the turn started from (the
//! acknowledged session, or the untrimmed source a caller projected its view
//! from) plus every message the turn appended. It never holds a lossy trim or
//! a request-local rewrite. Every durable in-turn compaction (a handoff, a
//! summary or an evicted page) is built from it and guarded by it, so a trim
//! that ran earlier in the same turn can never make a durable replacement
//! lose the turns that trim left out.
//!
//! The provider view is a projection of the canonical history: the recalled
//! context page, the skill-metadata suppression and the inbound middleware
//! rewrite of this turn, then the per-request trims. It is re-derived from the
//! canonical history after every durable compaction, and the turn's own
//! per-provider preflight trims it again before each provider request.
//!
//! The canonical copy is taken lazily: until the first step that reshapes the
//! view other than by appending, the view itself is the canonical history and
//! nothing is cloned.

use crate::agent::context_paging::inject_recalled_context;
use crate::providers::ChatMessage;

/// Whether two message lists carry the same roles and bytes in the same order.
pub(crate) fn same_messages(left: &[ChatMessage], right: &[ChatMessage]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.role == right.role && left.content == right.content)
}

/// One turn's canonical history and the projection state of its provider view.
pub(crate) struct TurnHistory {
    /// `None` while the view has only been appended to: the view is then the
    /// canonical history.
    canonical: Option<Vec<ChatMessage>>,
    /// The view length the canonical history last caught up with. Between two
    /// reshapes the view only appends, so `view[absorbed_view_len..]` is what
    /// the turn appended since.
    absorbed_view_len: usize,
    /// See [`Self::caller_rollover_current`].
    caller_rollover_current: bool,
    recalled_page: Option<ChatMessage>,
    suppress_skill_metadata: bool,
    #[cfg(feature = "wasm-plugins")]
    inbound_middleware_applied: bool,
}

impl TurnHistory {
    /// Start a turn on `view`. `untrimmed` is the history the caller projected
    /// `view` from when it trimmed before the turn (the chat worker's
    /// pre-turn rollover); `None` when the view is the acknowledged history.
    pub(crate) const fn new(view: &[ChatMessage], untrimmed: Option<Vec<ChatMessage>>) -> Self {
        Self {
            caller_rollover_current: untrimmed.is_some(),
            canonical: untrimmed,
            absorbed_view_len: view.len(),
            recalled_page: None,
            suppress_skill_metadata: false,
            #[cfg(feature = "wasm-plugins")]
            inbound_middleware_applied: false,
        }
    }

    /// Whether the caller's pre-turn rollover over the untrimmed history it
    /// supplied is still current: the turn has neither appended to `view` nor
    /// durably compacted that history since. While it is, no durable
    /// compaction of this turn can improve on it; it would only re-derive a
    /// larger view than the one the caller trimmed.
    pub(crate) const fn caller_rollover_current(&self, view: &[ChatMessage]) -> bool {
        self.caller_rollover_current && view.len() == self.absorbed_view_len
    }

    /// Bring the canonical history up to `view`: the first call takes the
    /// canonical copy, later ones add the messages the view appended since the
    /// previous call.
    fn catch_up(&mut self, view: &[ChatMessage]) {
        match self.canonical.as_mut() {
            None => self.canonical = Some(view.to_vec()),
            Some(canonical) => {
                if let Some(appended) = view.get(self.absorbed_view_len..)
                    && !appended.is_empty()
                {
                    canonical.extend_from_slice(appended);
                    self.caller_rollover_current = false;
                }
            }
        }
        self.absorbed_view_len = view.len();
    }

    /// Run one step that reshapes the provider view other than by appending
    /// (a trim, a rewrite, a re-derived projection). What the view appended
    /// before the step reaches the canonical history first; nothing the step
    /// leaves in the view does.
    pub(crate) fn reshape<R>(
        &mut self,
        view: &mut Vec<ChatMessage>,
        step: impl FnOnce(&mut Vec<ChatMessage>) -> R,
    ) -> R {
        self.catch_up(view);
        let result = step(view);
        self.absorbed_view_len = view.len();
        result
    }

    /// The canonical history, caught up with `view`, for a durable compaction
    /// to be built from and applied to. The caller re-derives the view with
    /// [`Self::project`] when the compaction changed it.
    pub(crate) fn canonical_mut(&mut self, view: &[ChatMessage]) -> &mut Vec<ChatMessage> {
        self.catch_up(view);
        self.canonical.get_or_insert_with(Vec::new)
    }

    /// The canonical history as of the last catch-up, or `view` when the view
    /// was never reshaped.
    pub(crate) fn canonical<'a>(&'a self, view: &'a [ChatMessage]) -> &'a [ChatMessage] {
        self.canonical.as_deref().unwrap_or(view)
    }

    /// The recalled context page this turn shows the model; it is a projection
    /// and never part of the canonical history.
    pub(crate) fn record_recalled_page(&mut self, page: ChatMessage) {
        self.recalled_page = Some(page);
    }

    /// Whether this request strips the skill metadata from the system prompt.
    pub(crate) const fn set_skill_metadata_suppressed(&mut self, suppressed: bool) {
        self.suppress_skill_metadata = suppressed;
    }

    /// The turn's inbound middleware pass ran; a re-derived view runs it again.
    #[cfg(feature = "wasm-plugins")]
    pub(crate) const fn mark_inbound_middleware_applied(&mut self) {
        self.inbound_middleware_applied = true;
    }

    /// Whether a re-derived view must pass through the inbound middleware.
    #[cfg(feature = "wasm-plugins")]
    pub(crate) const fn inbound_middleware_applied(&self) -> bool {
        self.inbound_middleware_applied
    }

    /// How many messages the projection inserts ahead of a compaction's
    /// replacement (the recalled page sits right after the system prompt), so a
    /// trim of the view preserves the same replacement.
    pub(crate) fn replacement_offset(&self) -> usize {
        usize::from(self.recalled_page.is_some())
    }

    /// The provider view of the canonical history before any per-request trim
    /// and before the inbound middleware (which is async and runs in the turn
    /// engine): the recalled page and the skill-metadata suppression applied.
    pub(crate) fn project(&mut self, view: &[ChatMessage]) -> Vec<ChatMessage> {
        self.catch_up(view);
        // Only a durable compaction re-derives the view.
        self.caller_rollover_current = false;
        let mut projected = self.canonical(view).to_vec();
        if let Some(page) = &self.recalled_page {
            inject_recalled_context(&mut projected, page.clone());
        }
        if self.suppress_skill_metadata {
            crate::agent::runtime_prompt::suppress_unreadable_skill_metadata(&mut projected);
        }
        projected
    }
}

#[cfg(test)]
#[path = "turn_tests/turn_history.rs"]
mod tests;

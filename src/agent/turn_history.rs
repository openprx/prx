//! The canonical history of one turn and the provider view projected from it
//! (I-CANON; controller rulings r4/r5).
//!
//! Two types keep the two histories apart, so the compiler rejects a view
//! where a canonical history is meant:
//! - [`CanonicalHistory`]: the acknowledged session plus the raw messages
//!   this turn appended. Guards, durable compaction patches, reducer sources,
//!   sink initialisation, turn seeding and persistence take only this type.
//! - [`ProviderView`]: the canonical history with this turn's projection
//!   ([`ProjectionInputs`]: request enrichment, the refresh aside, the recalled
//!   page, the skill-metadata suppression). [`project`] is the one conversion;
//!   nothing converts a view back. The inbound middleware rewrite and the
//!   per-request trims are applied to the projected messages afterwards and
//!   never reach the canonical history either.
//!
//! [`TurnHistory`] carries one turn's canonical history next to its provider
//! view. Every durable in-turn compaction (a handoff, a summary or an evicted
//! page) is built from and guarded by the canonical history, so a trim that
//! ran earlier in the same turn can never make a durable replacement lose the
//! turns that trim left out. The view is re-derived from the canonical
//! history after every durable compaction, and the turn's per-provider
//! preflight trims it again before each provider request.
//!
//! A chat turn supplies its canonical history (acknowledged turns plus the
//! plain request). A caller without an acknowledged session (a channel, the
//! agent entry) reports nothing to a session authority; its history is its
//! canonical history, copied lazily at the first step that reshapes the view.

use crate::agent::context_paging::inject_recalled_context;
use crate::agent::turn_contracts::{inbound_user_message, stored_turn_message};
use crate::providers::ChatMessage;

/// Whether two message lists carry the same roles and bytes in the same order.
pub(crate) fn same_messages(left: &[ChatMessage], right: &[ChatMessage]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.role == right.role && left.content == right.content)
}

/// The canonical history (I-CANON): the acknowledged session, as the reducer
/// holds it (an optional leading system prompt, then every stored user and
/// assistant turn through [`stored_turn_message`]), plus the raw messages this
/// turn appended (the plain request, assistant replies, tool calls and
/// results).
///
/// It is the only history guards are built over, durable compactions (a
/// handoff, a summary, an evicted page) are applied to, reducer patches are
/// sourced from and the session is persisted from. It never holds a
/// projection: no request enrichment, refresh note, recalled page,
/// skill-metadata suppression, middleware rewrite or lossy trim. A
/// `Vec<ChatMessage>` cannot become one: it is built from acknowledged turns
/// ([`Self::from_acknowledged`]), and changed only by appending this turn's
/// messages or by a durable compaction of itself.
#[derive(Debug, Clone)]
pub(crate) struct CanonicalHistory {
    messages: Vec<ChatMessage>,
}

impl PartialEq for CanonicalHistory {
    fn eq(&self, other: &Self) -> bool {
        same_messages(&self.messages, &other.messages)
    }
}

impl CanonicalHistory {
    /// The acknowledged session: `leading_system` (if any), then each stored
    /// `(role, content)` turn whose role is `user` or `assistant`, rebuilt
    /// with [`stored_turn_message`]. This is the form the reducer rebuilds its
    /// history in after a compaction and the form a published snapshot loads
    /// with, so a guard over it describes the reducer's history.
    pub(crate) fn from_acknowledged<'a, I>(leading_system: Option<&str>, turns: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut messages: Vec<ChatMessage> = leading_system
            .map(|prompt| ChatMessage::system(prompt.to_owned()))
            .into_iter()
            .collect();
        messages.extend(
            turns
                .into_iter()
                .filter(|(role, _)| matches!(*role, "user" | "assistant"))
                .map(|(role, content)| stored_turn_message(role.to_owned(), content.to_owned())),
        );
        Self { messages }
    }

    /// `self` with the turn's plain request appended as inbound user text.
    #[must_use]
    pub(crate) fn with_raw_request(mut self, request: &str) -> Self {
        self.messages.push(inbound_user_message(request));
        self
    }

    /// A caller with no acknowledged session store owns its history as given:
    /// that history is its canonical history. [`TurnHistory`] builds one for
    /// a turn that reports to no session authority (`run_turn` refuses a turn
    /// with an event sink and no canonical history); the chat effect executor
    /// for a direct `Effect::StartTurn` without a guard source (the reducer
    /// always supplies one).
    pub(crate) fn of_caller_without_session(messages: &[ChatMessage]) -> Self {
        Self {
            messages: messages.to_vec(),
        }
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) const fn len(&self) -> usize {
        self.messages.len()
    }

    /// The history a durable compaction (a handoff, a summary, an evicted
    /// page, and the request it restores at the tail) mutates in place. Only
    /// durable compaction steps take it; a lossy trim runs on the provider view.
    pub(crate) const fn durable_compaction_target(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    /// Absorb the raw messages this turn appended to its provider view.
    fn append_turn_messages(&mut self, appended: &[ChatMessage]) {
        self.messages.extend_from_slice(appended);
    }

    /// The guard of a patch that replaces `self[range_start..]`.
    pub(crate) fn guard_from(&self, range_start: usize) -> Option<crate::agent::turn_contracts::CompactionPatchGuard> {
        crate::agent::compaction::compaction_patch_guard_for(&self.messages, range_start, self.messages.len())
    }

    /// The guard of a patch that replaces the whole history.
    pub(crate) fn guard(&self) -> Option<crate::agent::turn_contracts::CompactionPatchGuard> {
        self.guard_from(0)
    }

    /// Apply a durable compaction `patch` built over `self`: its replacement
    /// only. The loop notes it appends after the replacement (a summary's
    /// refresh aside) are a projection and are returned with the index they
    /// follow, for [`ProjectionInputs::set_refresh_notes`]. On error `self`
    /// is unchanged.
    pub(crate) fn apply_patch(
        &mut self,
        patch: &crate::agent::turn_contracts::CompactionPatch,
    ) -> Result<RefreshNotes, crate::agent::turn_contracts::CompactionPatchError> {
        let durable = crate::agent::turn_contracts::CompactionPatch {
            replacement: patch.replacement.clone(),
            append_after: Vec::new(),
            guard: patch.guard.clone(),
        };
        crate::agent::compaction::apply_compaction_patch_exact(&mut self.messages, &durable)?;
        Ok(RefreshNotes {
            at: patch.range_start().saturating_add(patch.replacement.len()),
            notes: patch.append_after.clone(),
        })
    }

    /// Move every refresh aside a durable compaction step wrote into the
    /// history out of it: the asides are a projection.
    fn lift_refresh_notes(&mut self) -> Vec<(usize, ChatMessage)> {
        let mut lifted = Vec::new();
        let mut kept = Vec::with_capacity(self.messages.len());
        for message in self.messages.drain(..) {
            if is_refresh_aside(&message) {
                lifted.push((kept.len(), message));
            } else {
                kept.push(message);
            }
        }
        self.messages = kept;
        lifted
    }

    /// The history the reducer holds for this canonical history once the
    /// turn's pending request is left out: the leading system prompt, then
    /// every durable user and assistant message (no tool result, no
    /// tool-call message, no later system message, no loop note) rebuilt with
    /// [`Self::from_acknowledged`], exactly as the reducer rebuilds its own
    /// history after it adopts a patch. A worker's reducer source and every
    /// reducer patch replacement are this form, so a guard over one describes
    /// the other.
    #[must_use]
    pub(crate) fn reducer_form(&self) -> Self {
        let pending = crate::agent::history::latest_user_query_index(&self.messages);
        let leading_system = self
            .messages
            .first()
            .filter(|message| message.role == "system")
            .map(|message| message.content.as_str());
        let turns = self.messages.iter().enumerate().filter_map(|(index, message)| {
            let durable = Some(index) != pending
                && matches!(message.role.as_str(), "user" | "assistant")
                && !crate::agent::turn_contracts::is_loop_written_user_note(&message.content)
                && !(message.role == "assistant" && is_tool_call_message(&message.content));
            durable.then_some((message.role.as_str(), message.content.as_str()))
        });
        Self::from_acknowledged(leading_system, turns)
    }

    /// A canonical history a test states message by message.
    #[cfg(test)]
    pub(crate) const fn from_test_messages(messages: Vec<ChatMessage>) -> Self {
        Self { messages }
    }
}

fn is_refresh_aside(message: &ChatMessage) -> bool {
    message.role == "user"
        && message
            .content
            .starts_with(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER)
}

fn is_tool_call_message(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content).is_ok_and(|value| {
        value
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
    })
}

/// The refresh aside a durable compaction appends after its replacement, and
/// the canonical index it follows.
#[derive(Debug, Clone, Default)]
pub(crate) struct RefreshNotes {
    at: usize,
    notes: Vec<ChatMessage>,
}

/// Everything the provider view adds to, or takes from, the canonical history
/// for one request: the current request's enrichment, the refresh aside of
/// the last durable compaction, the recalled context page and the
/// skill-metadata suppression. The inbound middleware rewrite (async) and the
/// per-request trims are applied to the projected view by the turn engine.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectionInputs {
    /// The canonical plain request and the enriched form that replaces it.
    enriched_request: Option<(ChatMessage, ChatMessage)>,
    refresh_notes: Vec<(usize, ChatMessage)>,
    recalled_page: Option<ChatMessage>,
    suppress_skill_metadata: bool,
}

impl ProjectionInputs {
    /// The current request as the provider sees it (memory and file
    /// enrichment): `enriched` replaces the newest canonical message with the
    /// bytes of `plain`. Without such a message nothing is replaced.
    pub(crate) fn with_enriched_request(mut self, plain: Option<ChatMessage>, enriched: Option<ChatMessage>) -> Self {
        self.enriched_request = plain.zip(enriched);
        self
    }

    /// The refresh aside of the latest durable compaction (replaces any
    /// earlier one: an older aside describes a summary that is gone).
    pub(crate) fn set_refresh_notes(&mut self, notes: RefreshNotes) {
        self.refresh_notes = notes.notes.into_iter().map(|note| (notes.at, note)).collect();
    }
}

/// The provider projection of a canonical history. Built only by
/// [`project`]; it never converts back.
#[derive(Debug, Clone)]
pub(crate) struct ProviderView {
    messages: Vec<ChatMessage>,
}

impl ProviderView {
    pub(crate) fn into_messages(self) -> Vec<ChatMessage> {
        self.messages
    }
}

/// The one conversion between the two: the provider view of `canonical`
/// under `inputs`. The enriched request replaces the canonical plain request,
/// the refresh aside follows the replacement it
/// was written after, the recalled page follows the system prompt, and the
/// skill metadata is stripped when suppressed.
pub(crate) fn project(canonical: &CanonicalHistory, inputs: &ProjectionInputs) -> ProviderView {
    let mut messages = canonical.messages.clone();
    if let Some((plain, enriched)) = &inputs.enriched_request
        && let Some(request) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == plain.role && message.content == plain.content)
    {
        *request = enriched.clone();
    }
    for (offset, (at, note)) in inputs.refresh_notes.iter().enumerate() {
        let index = at.saturating_add(offset).min(messages.len());
        messages.insert(index, note.clone());
    }
    if let Some(page) = &inputs.recalled_page {
        inject_recalled_context(&mut messages, page.clone());
    }
    if inputs.suppress_skill_metadata {
        crate::agent::runtime_prompt::suppress_unreadable_skill_metadata(&mut messages);
    }
    ProviderView { messages }
}

/// One turn's canonical history and the projection state of its provider view.
pub(crate) struct TurnHistory {
    /// `None` while an untracked caller's view has only been appended to: the
    /// view is then the canonical history.
    canonical: Option<CanonicalHistory>,
    /// The view length the canonical history last caught up with. Between two
    /// reshapes the view only appends, so `view[absorbed_view_len..]` is what
    /// the turn appended since.
    absorbed_view_len: usize,
    /// See [`Self::caller_rollover_current`].
    caller_rollover_current: bool,
    projection: ProjectionInputs,
    #[cfg(feature = "wasm-plugins")]
    inbound_middleware_applied: bool,
}

impl TurnHistory {
    /// Start a turn on `view`. `canonical` is the turn's canonical history
    /// (acknowledged turns plus the plain request) when the caller has one;
    /// the view's current request is then the enriched request the projection
    /// shows in place of the plain one. `caller_rollover` says the caller
    /// compacted or trimmed before the turn, so `view` is a projection of
    /// `canonical` that already fits.
    pub(crate) fn new(view: &[ChatMessage], canonical: Option<CanonicalHistory>, caller_rollover: bool) -> Self {
        let newest_request = |messages: &[ChatMessage]| {
            crate::agent::history::latest_user_query_index(messages).and_then(|index| messages.get(index).cloned())
        };
        let plain_request = canonical
            .as_ref()
            .and_then(|canonical| newest_request(canonical.messages()));
        let enriched_request = canonical.as_ref().and_then(|_| newest_request(view));
        Self {
            caller_rollover_current: caller_rollover && canonical.is_some(),
            canonical,
            absorbed_view_len: view.len(),
            projection: ProjectionInputs::default().with_enriched_request(plain_request, enriched_request),
            #[cfg(feature = "wasm-plugins")]
            inbound_middleware_applied: false,
        }
    }

    /// Whether the caller's pre-turn rollover over the canonical history it
    /// supplied is still current: the turn has neither appended to `view` nor
    /// durably compacted that history since. While it is, no durable
    /// compaction of this turn can improve on it; it would only re-derive a
    /// larger view than the one the caller trimmed.
    pub(crate) const fn caller_rollover_current(&self, view: &[ChatMessage]) -> bool {
        self.caller_rollover_current && view.len() == self.absorbed_view_len
    }

    /// Bring the canonical history up to `view`: an untracked caller's first
    /// call takes the canonical copy, later ones add the messages the view
    /// appended since the previous call.
    fn catch_up(&mut self, view: &[ChatMessage]) -> &mut CanonicalHistory {
        let appended = view.get(self.absorbed_view_len..).unwrap_or_default();
        let absorbed_any = !appended.is_empty();
        let canonical = self.canonical.take().map_or_else(
            || CanonicalHistory::of_caller_without_session(view),
            |mut canonical| {
                canonical.append_turn_messages(appended);
                canonical
            },
        );
        if absorbed_any {
            self.caller_rollover_current = false;
        }
        self.absorbed_view_len = view.len();
        self.canonical.insert(canonical)
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
    /// to be built from and applied to. The caller reports it through
    /// [`Self::finish_durable_compaction`] and re-derives the view with
    /// [`Self::project`] when the compaction changed it.
    pub(crate) fn canonical_mut(&mut self, view: &[ChatMessage]) -> &mut CanonicalHistory {
        self.catch_up(view)
    }

    /// After a durable compaction of [`Self::canonical_mut`]: the refresh
    /// aside it wrote moves into the projection (it replaces any earlier one),
    /// and the compacted canonical history is returned for the report.
    pub(crate) fn finish_durable_compaction(&mut self, view: &[ChatMessage]) -> &CanonicalHistory {
        let canonical = self.catch_up(view);
        let lifted = canonical.lift_refresh_notes();
        self.projection.refresh_notes = lifted;
        self.catch_up(view)
    }

    /// The canonical messages as of the last catch-up, or `view` when an
    /// untracked caller's view was never reshaped.
    pub(crate) fn canonical<'a>(&'a self, view: &'a [ChatMessage]) -> &'a [ChatMessage] {
        self.canonical.as_ref().map_or(view, CanonicalHistory::messages)
    }

    /// The recalled context page this turn shows the model; it is a projection
    /// and never part of the canonical history.
    pub(crate) fn record_recalled_page(&mut self, page: ChatMessage) {
        self.projection.recalled_page = Some(page);
    }

    /// Whether this request strips the skill metadata from the system prompt.
    /// The caller derives it from the canonical history's metadata and the
    /// current tool admission, never from an already suppressed view.
    pub(crate) const fn set_skill_metadata_suppressed(&mut self, suppressed: bool) {
        self.projection.suppress_skill_metadata = suppressed;
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
        usize::from(self.projection.recalled_page.is_some())
    }

    /// The provider view of the canonical history ([`project`]) before any
    /// per-request trim and before the inbound middleware (which is async and
    /// runs in the turn engine).
    pub(crate) fn project(&mut self, view: &[ChatMessage]) -> Vec<ChatMessage> {
        // Only a durable compaction re-derives the view.
        self.caller_rollover_current = false;
        self.catch_up(view);
        self.canonical.as_ref().map_or_else(
            || view.to_vec(),
            |canonical| project(canonical, &self.projection).into_messages(),
        )
    }
}

#[cfg(test)]
#[path = "turn_tests/turn_history.rs"]
mod tests;

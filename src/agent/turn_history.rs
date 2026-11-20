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

use crate::agent::context_budget::{FitDemand, FitReport, fit_request_window};
use crate::agent::context_paging::inject_recalled_context;
use crate::agent::turn_contracts::ContextCapacityError;
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

/// Where one canonical message came from. Provenance is recorded when a
/// message enters the canonical history and carried through every durable
/// compaction; nothing is ever classified by its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageOrigin {
    /// An acknowledged session turn (or the leading system prompt), or a
    /// message a durable compaction wrote in place of turns (a handoff note, a
    /// summary, a memory flush, a page breadcrumb).
    Acknowledged,
    /// The turn's raw request: canonical, but pending until the ordered commit.
    Request,
    /// Appended by this turn's tool loop: tool-call envelopes, tool results,
    /// the loop's result blocks. Transient until the turn commits. `Some` when
    /// the engine recorded the round as one batch ([`TurnHistory::record_tool_batch`]).
    TurnRound(Option<RoundTag>),
    /// The refresh aside a summary appends after its replacement. A
    /// projection: [`TurnHistory::finish_durable_compaction`] lifts it out.
    RefreshAside,
    /// History of a caller with no acknowledged session store, which arrived
    /// without provenance.
    CallerSupplied,
}

/// One tool round of this turn: a batch of the assistant's tool calls and the
/// results that answer them, recorded when the engine appended it (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoundTag {
    /// The batch's identity; the batches of one turn are numbered in order.
    pub(crate) batch: u64,
    /// Whether this is the batch's first message: the assistant envelope
    /// (native) or response (prompt-guided).
    pub(crate) envelope: bool,
    /// Whether every result of the batch was appended.
    pub(crate) complete: bool,
    /// How many messages the batch holds when it was recorded.
    pub(crate) size: usize,
}

/// What a message of the provider view is, for the context fit (§3.3, §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewSlot {
    /// An acknowledged turn, a durable replacement, the caller's own message,
    /// or a message of this turn that was never recorded as a round.
    Acknowledged,
    /// One message of a recorded tool round.
    Round(RoundTag),
    /// A projection the canonical history does not hold: the refresh aside
    /// and the recalled page.
    Other,
}

impl MessageOrigin {
    fn view_slot(self) -> ViewSlot {
        match self {
            Self::Acknowledged | Self::Request | Self::CallerSupplied | Self::TurnRound(None) => {
                ViewSlot::Acknowledged
            }
            Self::TurnRound(Some(tag)) => ViewSlot::Round(tag),
            Self::RefreshAside => ViewSlot::Other,
        }
    }
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
///
/// Every message carries its [`MessageOrigin`]: the reducer form and the
/// compaction's tool round-trip folding tell an acknowledged turn from this
/// turn's transient tool traffic by that provenance, never by the message's
/// text (an acknowledged answer may itself read like a tool-call envelope).
#[derive(Debug, Clone)]
pub(crate) struct CanonicalHistory {
    messages: Vec<ChatMessage>,
    /// One origin per message, index for index.
    origins: Vec<MessageOrigin>,
}

impl PartialEq for CanonicalHistory {
    fn eq(&self, other: &Self) -> bool {
        same_messages(&self.messages, &other.messages)
    }
}

impl CanonicalHistory {
    fn with_origin(messages: Vec<ChatMessage>, origin: MessageOrigin) -> Self {
        let origins = vec![origin; messages.len()];
        Self { messages, origins }
    }

    /// The acknowledged session: `leading_system` (if any), then each stored
    /// `(role, content)` turn whose role is `user` or `assistant`, rebuilt
    /// with [`stored_turn_message`]. This is the form the reducer rebuilds its
    /// history in after a compaction and the form a published snapshot loads
    /// with, so a guard over it describes the reducer's history.
    pub(crate) fn from_acknowledged<'a, I>(leading_system: Option<&str>, turns: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        Self::from_stored_turns(leading_system, turns, None)
    }

    /// A snapshot that holds the turn's admitted request (a compaction saved
    /// it while its admission was open): as [`Self::from_acknowledged`], but
    /// the stored turn at `request_turn` (an index into `turns`, given by the
    /// admission's recorded position, never found by its text) keeps its
    /// [`MessageOrigin::Request`] origin. The reducer form leaves it out, so
    /// a later durable compaction that keeps it in its hot tail records it
    /// once, exactly as for a request appended by [`Self::with_raw_request`].
    /// The message bytes are the stored turn's, which the reducer's own
    /// history holds, so a guard over this history describes the reducer's.
    pub(crate) fn from_acknowledged_with_request<'a, I>(
        leading_system: Option<&str>,
        turns: I,
        request_turn: usize,
    ) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        Self::from_stored_turns(leading_system, turns, Some(request_turn))
    }

    fn from_stored_turns<'a, I>(leading_system: Option<&str>, turns: I, request_turn: Option<usize>) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut messages: Vec<ChatMessage> = leading_system
            .map(|prompt| ChatMessage::system(prompt.to_owned()))
            .into_iter()
            .collect();
        let mut origins = vec![MessageOrigin::Acknowledged; messages.len()];
        for (index, (role, content)) in turns.into_iter().enumerate() {
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            let origin = if Some(index) == request_turn && role == "user" {
                MessageOrigin::Request
            } else {
                MessageOrigin::Acknowledged
            };
            messages.push(stored_turn_message(role.to_owned(), content.to_owned()));
            origins.push(origin);
        }
        Self { messages, origins }
    }

    /// `self` with the turn's plain request appended as inbound user text.
    #[must_use]
    pub(crate) fn with_raw_request(mut self, request: &str) -> Self {
        self.messages.push(inbound_user_message(request));
        self.origins.push(MessageOrigin::Request);
        self
    }

    /// A caller with no acknowledged session store owns its history as given:
    /// that history is its canonical history. [`TurnHistory`] builds one for
    /// a turn that reports to no session authority (`run_turn` refuses a turn
    /// with an event sink and no canonical history); the chat effect executor
    /// for a direct `Effect::StartTurn` without a guard source (the reducer
    /// always supplies one).
    pub(crate) fn of_caller_without_session(messages: &[ChatMessage]) -> Self {
        Self::with_origin(messages.to_vec(), MessageOrigin::CallerSupplied)
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) const fn len(&self) -> usize {
        self.messages.len()
    }

    /// Absorb the raw messages this turn appended to its provider view.
    fn append_turn_messages(&mut self, appended: &[ChatMessage]) {
        self.messages.extend_from_slice(appended);
        self.origins
            .extend(std::iter::repeat_n(MessageOrigin::TurnRound(None), appended.len()));
    }

    /// Replace `self[start..end]` with `replacement`, every new message of
    /// `origin`. `false` (and `self` unchanged) when the range is not inside
    /// the history.
    fn splice(&mut self, start: usize, end: usize, replacement: &[ChatMessage], origin: MessageOrigin) -> bool {
        if start > end || end > self.messages.len() || self.origins.len() != self.messages.len() {
            return false;
        }
        self.messages.splice(start..end, replacement.iter().cloned());
        self.origins
            .splice(start..end, std::iter::repeat_n(origin, replacement.len()));
        true
    }

    /// The guard of a patch that replaces `self[range_start..]`.
    pub(crate) fn guard_from(&self, range_start: usize) -> Option<crate::agent::turn_contracts::CompactionPatchGuard> {
        crate::agent::compaction::compaction_patch_guard_for(&self.messages, range_start, self.messages.len())
    }

    /// The guard of a patch that replaces the whole history.
    pub(crate) fn guard(&self) -> Option<crate::agent::turn_contracts::CompactionPatchGuard> {
        self.guard_from(0)
    }

    /// Check `patch` against `self` without changing it: its range lies
    /// inside the history and its guard describes it.
    fn check_patch(
        &self,
        patch: &crate::agent::turn_contracts::CompactionPatch,
    ) -> Result<(), crate::agent::turn_contracts::CompactionPatchError> {
        let (start, end, len) = (patch.range_start(), patch.range_end(), self.messages.len());
        if start > end || end > len {
            return Err(crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds { start, end, len });
        }
        if !crate::agent::compaction::compaction_patch_guard_matches(&self.messages, &patch.guard) {
            return Err(crate::agent::turn_contracts::CompactionPatchError::GuardMismatch {
                expected_len: patch.guard.history_len,
                actual_len: len,
            });
        }
        Ok(())
    }

    /// Apply a durable compaction `patch` built over `self`: its replacement
    /// only. The loop notes it appends after the replacement (a summary's
    /// refresh aside) are a projection and are returned with the index they
    /// follow, for [`ProjectionInputs::set_refresh_notes`]. The replacement is
    /// the compaction's own messages; every message outside the replaced range
    /// keeps its provenance. On error `self` is unchanged.
    pub(crate) fn apply_patch(
        &mut self,
        patch: &crate::agent::turn_contracts::CompactionPatch,
    ) -> Result<RefreshNotes, crate::agent::turn_contracts::CompactionPatchError> {
        self.check_patch(patch)?;
        let (start, end) = (patch.range_start(), patch.range_end());
        if !self.splice(start, end, &patch.replacement, MessageOrigin::Acknowledged) {
            return Err(crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds {
                start,
                end,
                len: self.messages.len(),
            });
        }
        Ok(RefreshNotes {
            at: start.saturating_add(patch.replacement.len()),
            notes: patch.append_after.clone(),
        })
    }

    /// Apply a durable compaction `patch` built over `self` in place, its
    /// refresh aside included (marked as such, so
    /// [`TurnHistory::finish_durable_compaction`] lifts it into the
    /// projection). On error `self` is unchanged.
    pub(crate) fn apply_patch_with_refresh_aside(
        &mut self,
        patch: &crate::agent::turn_contracts::CompactionPatch,
    ) -> Result<(), crate::agent::turn_contracts::CompactionPatchError> {
        let notes = self.apply_patch(patch)?;
        if !self.splice(notes.at, notes.at, &notes.notes, MessageOrigin::RefreshAside) {
            return Err(crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds {
                start: notes.at,
                end: notes.at,
                len: self.messages.len(),
            });
        }
        Ok(())
    }

    /// Put the turn's request back at the tail when a durable compaction
    /// folded it away ([`restore_current_turn_request`]). Whether it was restored.
    ///
    /// [`restore_current_turn_request`]: crate::agent::context_preflight::restore_current_turn_request
    pub(crate) fn restore_request_at_tail(
        &mut self,
        captured: Option<crate::agent::context_preflight::CapturedTurnRequest>,
        trigger: &'static str,
    ) -> bool {
        let restored = crate::agent::context_preflight::restore_current_turn_request(
            &mut self.messages,
            captured,
            trigger,
        );
        if restored {
            self.origins.push(MessageOrigin::Request);
        }
        restored
    }

    /// Apply an OS-paging eviction planned over `self`: the breadcrumb takes
    /// the evicted range, and a `role="tool"` result left directly behind it
    /// is dropped. Whether it was applied.
    pub(super) fn apply_page_eviction(&mut self, eviction: &crate::agent::context_paging::OsPagingEviction) -> bool {
        if !self.splice(
            eviction.start,
            eviction.end,
            std::slice::from_ref(&eviction.breadcrumb),
            MessageOrigin::Acknowledged,
        ) {
            return false;
        }
        let behind = eviction.start.saturating_add(1);
        while self.messages.get(behind).is_some_and(|message| message.role == "tool") {
            self.messages.remove(behind);
            if behind < self.origins.len() {
                self.origins.remove(behind);
            }
        }
        true
    }

    /// Move every refresh aside a durable compaction step wrote into the
    /// history out of it: the asides are a projection.
    fn lift_refresh_notes(&mut self) -> Vec<(usize, ChatMessage)> {
        let mut lifted = Vec::new();
        let mut kept = Vec::with_capacity(self.messages.len());
        let mut kept_origins = Vec::with_capacity(self.origins.len());
        for (message, origin) in self.messages.drain(..).zip(self.origins.drain(..)) {
            if origin == MessageOrigin::RefreshAside {
                lifted.push((kept.len(), message));
            } else {
                kept.push(message);
                kept_origins.push(origin);
            }
        }
        self.messages = kept;
        self.origins = kept_origins;
        lifted
    }

    /// Per message, whether a compaction folds it into the assistant turn of
    /// a tool round-trip instead of resolving it to a transcript event of its
    /// own ([`crate::agent::compaction::RoundCompanions`]). Only this turn's
    /// own round traffic can be a companion (an acknowledged turn, the
    /// request and a compaction's own message never are); within that
    /// traffic, the round-trip shape says which part it is. A caller without
    /// an acknowledged session supplies no provenance, so its history is read
    /// by shape alone.
    pub(crate) fn tool_round_companions(&self) -> Vec<bool> {
        self.messages
            .iter()
            .zip(&self.origins)
            .map(|(message, origin)| match origin {
                MessageOrigin::TurnRound(_) | MessageOrigin::CallerSupplied => {
                    crate::agent::compaction::is_tool_roundtrip_companion(message)
                }
                MessageOrigin::Acknowledged | MessageOrigin::Request | MessageOrigin::RefreshAside => false,
            })
            .collect()
    }

    /// The history the reducer holds for this canonical history: the leading
    /// system prompt, then every acknowledged message that is a durable turn
    /// ([`is_durable_turn_message`], the rule the reducer rebuilds its own
    /// history with after it adopts a patch), rebuilt with
    /// [`Self::from_acknowledged`]. Left out by provenance only: the pending
    /// request and everything this turn's tool loop appended. An acknowledged
    /// turn is kept whatever its text. A worker's reducer source and every
    /// reducer patch replacement are this form, so a guard over one describes
    /// the other.
    ///
    /// A caller-supplied history (no acknowledged session) has no recorded
    /// request; its newest user query is the pending request.
    ///
    /// [`is_durable_turn_message`]: crate::agent::turn_contracts::is_durable_turn_message
    #[must_use]
    pub(crate) fn reducer_form(&self) -> Self {
        let caller_pending = crate::agent::history::latest_user_query_index(&self.messages)
            .filter(|index| self.origins.get(*index) == Some(&MessageOrigin::CallerSupplied));
        let leading_system = self
            .messages
            .first()
            .filter(|message| message.role == "system")
            .map(|message| message.content.as_str());
        let turns = self
            .messages
            .iter()
            .zip(&self.origins)
            .enumerate()
            .filter_map(|(index, (message, origin))| {
                let acknowledged = match origin {
                    MessageOrigin::Acknowledged => true,
                    MessageOrigin::CallerSupplied => Some(index) != caller_pending,
                    MessageOrigin::Request | MessageOrigin::TurnRound(_) | MessageOrigin::RefreshAside => false,
                };
                (acknowledged && crate::agent::turn_contracts::is_durable_turn_message(message))
                    .then_some((message.role.as_str(), message.content.as_str()))
            });
        Self::from_acknowledged(leading_system, turns)
    }

    /// A canonical history a test states message by message (no recorded
    /// provenance, as a caller without a session).
    #[cfg(test)]
    pub(crate) fn from_test_messages(messages: Vec<ChatMessage>) -> Self {
        Self::with_origin(messages, MessageOrigin::CallerSupplied)
    }

    /// A canonical history a test builds as a chat turn does: acknowledged
    /// `turns`, the raw `request`, then `round` as this turn's tool traffic.
    #[cfg(test)]
    pub(crate) fn from_test_turn(
        leading_system: Option<&str>,
        turns: &[(&str, &str)],
        request: &str,
        round: &[ChatMessage],
    ) -> Self {
        let mut canonical = Self::from_acknowledged(leading_system, turns.iter().copied()).with_raw_request(request);
        canonical.append_turn_messages(round);
        canonical
    }
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
/// [`project`]; it never converts back. `slots` is index for index with
/// `messages`.
#[derive(Debug, Clone)]
pub(crate) struct ProviderView {
    messages: Vec<ChatMessage>,
    slots: Vec<ViewSlot>,
}

impl ProviderView {
    fn from_parts(messages: Vec<ChatMessage>, slots: Vec<ViewSlot>) -> Self {
        if slots.len() == messages.len() {
            Self { messages, slots }
        } else {
            Self::untracked(&messages)
        }
    }

    /// A view whose slots are not known: every message is acknowledged.
    pub(crate) fn untracked(messages: &[ChatMessage]) -> Self {
        Self {
            messages: messages.to_vec(),
            slots: vec![ViewSlot::Acknowledged; messages.len()],
        }
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// The same slots over `messages` when the count still matches; otherwise
    /// the slots are unknown and every message is acknowledged.
    pub(crate) fn with_messages(self, messages: Vec<ChatMessage>) -> Self {
        if messages.len() == self.slots.len() {
            Self {
                messages,
                slots: self.slots,
            }
        } else {
            Self::untracked(&messages)
        }
    }

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
    let mut slots: Vec<ViewSlot> = canonical.origins.iter().map(|origin| origin.view_slot()).collect();
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
        slots.insert(index.min(slots.len()), ViewSlot::Other);
    }
    if let Some(page) = &inputs.recalled_page {
        let index = recalled_page_index(&messages);
        inject_recalled_context(&mut messages, page.clone());
        slots.insert(index.min(slots.len()), ViewSlot::Other);
    }
    if inputs.suppress_skill_metadata {
        crate::agent::runtime_prompt::suppress_unreadable_skill_metadata(&mut messages);
    }
    ProviderView::from_parts(messages, slots)
}

/// Where a recalled page is inserted: directly after the leading system prompt.
fn recalled_page_index(messages: &[ChatMessage]) -> usize {
    usize::from(messages.first().is_some_and(|message| message.role == "system"))
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
    /// The slot of every message of the provider view, index for index.
    view_slots: Vec<ViewSlot>,
    /// The number the next recorded tool round takes.
    next_batch: u64,
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
            view_slots: vec![ViewSlot::Acknowledged; view.len()],
            next_batch: 0,
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
        self.view_slots.resize(view.len(), ViewSlot::Acknowledged);
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
        if view.len() != self.view_slots.len() {
            // The step changed the window's shape outside the fit. The slots
            // are unknown from here: no message is a round or a removable turn
            // until a new round is recorded, so nothing unproven is folded.
            self.view_slots = vec![ViewSlot::Acknowledged; view.len()];
        }
        self.absorbed_view_len = view.len();
        result
    }

    /// Insert the recalled context page into the view (a projection, directly
    /// after the system prompt) and record its slot.
    pub(crate) fn insert_recalled_page(&mut self, view: &mut Vec<ChatMessage>, page: ChatMessage) {
        self.catch_up(view);
        let index = recalled_page_index(view);
        inject_recalled_context(view, page);
        self.view_slots.insert(index.min(self.view_slots.len()), ViewSlot::Other);
        self.absorbed_view_len = view.len();
    }

    /// Record the tool round the engine just appended to the view as one batch.
    /// The batch is the view's appends since the last reshape. Its identity is
    /// written to the canonical history too, so it survives a durable
    /// compaction (§3.4).
    pub(crate) fn record_tool_batch(&mut self, view: &[ChatMessage], complete: bool) {
        let start = self.absorbed_view_len;
        self.catch_up(view);
        let size = view.len().saturating_sub(start);
        if size == 0 {
            return;
        }
        let batch = self.next_batch;
        self.next_batch = self.next_batch.saturating_add(1);
        let Some(canonical) = self.canonical.as_mut() else {
            return;
        };
        let canonical_start = canonical.messages.len().saturating_sub(size);
        for offset in 0..size {
            let tag = RoundTag {
                batch,
                envelope: offset == 0,
                complete,
                size,
            };
            if let Some(origin) = canonical.origins.get_mut(canonical_start.saturating_add(offset)) {
                *origin = MessageOrigin::TurnRound(Some(tag));
            }
            if let Some(slot) = self.view_slots.get_mut(start.saturating_add(offset)) {
                *slot = ViewSlot::Round(tag);
            }
        }
    }

    /// Fit the provider view to `limit` (the one fit of §3.3). The slots travel
    /// with the view, and only the view changes.
    pub(crate) fn fit_view(
        &mut self,
        view: &mut Vec<ChatMessage>,
        limit: usize,
        demand: FitDemand,
        trigger: &'static str,
    ) -> Result<FitReport, ContextCapacityError> {
        self.catch_up(view);
        let report = fit_request_window(view, &mut self.view_slots, limit, demand, trigger);
        self.absorbed_view_len = view.len();
        report
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

    /// The provider view of the canonical history ([`project`]) before any
    /// per-request trim and before the inbound middleware (which is async and
    /// runs in the turn engine).
    pub(crate) fn project(&mut self, view: &[ChatMessage]) -> ProviderView {
        // Only a durable compaction re-derives the view.
        self.caller_rollover_current = false;
        self.catch_up(view);
        self.canonical.as_ref().map_or_else(
            || ProviderView::untracked(view),
            |canonical| project(canonical, &self.projection),
        )
    }

    /// Make `projected` the provider view: its messages and slots replace the view's.
    pub(crate) fn adopt(&mut self, view: &mut Vec<ChatMessage>, projected: ProviderView) {
        *view = projected.messages;
        self.view_slots = projected.slots;
        self.absorbed_view_len = view.len();
    }
}

#[cfg(test)]
#[path = "turn_tests/turn_history.rs"]
mod tests;

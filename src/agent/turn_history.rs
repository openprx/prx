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

use crate::agent::compaction::render_context_handoff_for_provider;
use crate::agent::context_budget::{budgeted_tokens, estimate_history_tokens_with_source};
use crate::agent::context_paging::inject_recalled_context;
use crate::agent::page_sources::MessageSources;
use crate::agent::tool_evidence::{TurnToolEvidence, render_assistant_content, render_user_content};
use crate::agent::turn_contracts::{inbound_user_message, stored_turn_message};
use crate::providers::ChatMessage;
use std::sync::Arc;

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
    /// the loop's result blocks. Transient until the turn commits.
    TurnRound,
    /// The refresh aside a summary appends after its replacement. A
    /// projection: [`TurnHistory::finish_durable_compaction`] lifts it out.
    RefreshAside,
    /// History of a caller with no acknowledged session store, which arrived
    /// without provenance.
    CallerSupplied,
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
///
/// An acknowledged assistant message may carry its turn's tool evidence. The
/// evidence is never part of the message bytes (guards, equality and
/// provenance see the stored text); only [`project`] renders it, so every
/// view built from the same turns holds the same bytes.
#[derive(Debug, Clone)]
pub(crate) struct CanonicalHistory {
    messages: Vec<ChatMessage>,
    /// One origin per message, index for index.
    origins: Vec<MessageOrigin>,
    /// One evidence slot per message, index for index; only an acknowledged
    /// message's slot can be filled.
    evidence: Vec<Option<Arc<TurnToolEvidence>>>,
    /// One set of source event ids per message, index for index; an evicted
    /// context page records the union over its messages.
    sources: Vec<MessageSources>,
}

impl PartialEq for CanonicalHistory {
    fn eq(&self, other: &Self) -> bool {
        same_messages(&self.messages, &other.messages)
    }
}

impl CanonicalHistory {
    fn with_origin(messages: Vec<ChatMessage>, origin: MessageOrigin) -> Self {
        let origins = vec![origin; messages.len()];
        let evidence = vec![None; messages.len()];
        let sources = vec![MessageSources::default(); messages.len()];
        Self {
            messages,
            origins,
            evidence,
            sources,
        }
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
        Self::from_stored_turns(
            leading_system,
            turns.into_iter().map(|(role, content)| (role, content, None)),
            None,
        )
    }

    /// As [`Self::from_acknowledged`], with each stored turn's runtime record
    /// (tool evidence and replayed rounds of a reply, sent form of a request).
    pub(crate) fn from_acknowledged_turns<'a, I>(leading_system: Option<&str>, turns: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str, Option<&'a TurnToolEvidence>)>,
    {
        Self::from_stored_turns(leading_system, shared_evidence(turns), None)
    }

    /// A snapshot that holds the turn's admitted request at `request_turn`
    /// (see `from_acknowledged_with_request`), with each stored turn's
    /// runtime record.
    pub(crate) fn from_acknowledged_turns_with_request<'a, I>(
        leading_system: Option<&str>,
        turns: I,
        request_turn: usize,
    ) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str, Option<&'a TurnToolEvidence>)>,
    {
        Self::from_stored_turns(leading_system, shared_evidence(turns), Some(request_turn))
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
    /// Production callers use [`Self::from_acknowledged_turns_with_request`],
    /// which also carries the turns' tool evidence.
    #[cfg(test)]
    pub(crate) fn from_acknowledged_with_request<'a, I>(
        leading_system: Option<&str>,
        turns: I,
        request_turn: usize,
    ) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        Self::from_stored_turns(
            leading_system,
            turns.into_iter().map(|(role, content)| (role, content, None)),
            Some(request_turn),
        )
    }

    fn from_stored_turns<'a, I>(leading_system: Option<&str>, turns: I, request_turn: Option<usize>) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str, Option<Arc<TurnToolEvidence>>)>,
    {
        let mut messages: Vec<ChatMessage> = leading_system
            .map(|prompt| ChatMessage::system(prompt.to_owned()))
            .into_iter()
            .collect();
        let mut origins = vec![MessageOrigin::Acknowledged; messages.len()];
        let mut evidence = vec![None; messages.len()];
        let mut sources = vec![MessageSources::default(); messages.len()];
        for (index, (role, content, turn_evidence)) in turns.into_iter().enumerate() {
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
            evidence.push(turn_evidence);
            sources.push(if origin == MessageOrigin::Request {
                MessageSources::turn_round()
            } else {
                MessageSources::default()
            });
        }
        Self {
            messages,
            origins,
            evidence,
            sources,
        }
    }

    /// `self` with the turn's plain request appended as inbound user text.
    #[must_use]
    pub(crate) fn with_raw_request(mut self, request: &str) -> Self {
        self.messages.push(inbound_user_message(request));
        self.origins.push(MessageOrigin::Request);
        self.evidence.push(None);
        self.sources.push(MessageSources::turn_round());
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

    /// As [`Self::of_caller_without_session`], for a view the running turn
    /// started with `started_len` messages and then appended to. `event_ids`
    /// gives the stored event of each started message, index for index
    /// (`None` for a message with no stored event); it is used only when it
    /// covers exactly the started messages. The messages after them were
    /// appended by the running turn.
    pub(crate) fn of_caller_with_event_ids(
        messages: &[ChatMessage],
        started_len: usize,
        event_ids: &[Option<String>],
    ) -> Self {
        let mut canonical = Self::of_caller_without_session(messages);
        let known = event_ids.len() == started_len;
        for (index, slot) in canonical.sources.iter_mut().enumerate() {
            if index >= started_len {
                *slot = MessageSources::turn_round();
            } else if known {
                *slot = MessageSources::stored(event_ids.get(index).and_then(Option::as_deref));
            }
        }
        canonical
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) const fn len(&self) -> usize {
        self.messages.len()
    }

    /// Per message, index for index, whether it is an acknowledged message
    /// ([`MessageOrigin::Acknowledged`]): never the pending request, this
    /// turn's tool loop output, a refresh aside or caller-supplied history.
    pub(crate) fn acknowledged_flags(&self) -> Vec<bool> {
        self.origins
            .iter()
            .map(|origin| *origin == MessageOrigin::Acknowledged)
            .collect()
    }

    /// Absorb the raw messages this turn appended to its provider view.
    fn append_turn_messages(&mut self, appended: &[ChatMessage]) {
        self.messages.extend_from_slice(appended);
        self.origins
            .extend(std::iter::repeat_n(MessageOrigin::TurnRound, appended.len()));
        self.evidence.extend(std::iter::repeat_n(None, appended.len()));
        self.sources
            .extend(std::iter::repeat_n(MessageSources::turn_round(), appended.len()));
    }

    /// Replace `self[start..end]` with `replacement`, every new message of
    /// `origin`, without evidence and with `sources`. `false` (and `self`
    /// unchanged) when the range is not inside the history or the per-message
    /// lists disagree in length.
    fn splice(
        &mut self,
        start: usize,
        end: usize,
        replacement: &[ChatMessage],
        origin: MessageOrigin,
        sources: &MessageSources,
    ) -> bool {
        if start > end
            || end > self.messages.len()
            || self.origins.len() != self.messages.len()
            || self.evidence.len() != self.messages.len()
            || self.sources.len() != self.messages.len()
        {
            return false;
        }
        self.messages.splice(start..end, replacement.iter().cloned());
        self.origins
            .splice(start..end, std::iter::repeat_n(origin, replacement.len()));
        self.evidence
            .splice(start..end, std::iter::repeat_n(None, replacement.len()));
        self.sources
            .splice(start..end, std::iter::repeat_n(sources.clone(), replacement.len()));
        true
    }

    /// The union of the sources of `self[start..end]`, which a replacement of
    /// that range derives from.
    fn range_sources(&self, start: usize, end: usize) -> MessageSources {
        MessageSources::union(self.sources.get(start..end).unwrap_or_default())
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
        let derived = self.range_sources(start, end);
        if !self.splice(start, end, &patch.replacement, MessageOrigin::Acknowledged, &derived) {
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
        if !self.splice(
            notes.at,
            notes.at,
            &notes.notes,
            MessageOrigin::RefreshAside,
            &MessageSources::default(),
        ) {
            return Err(crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds {
                start: notes.at,
                end: notes.at,
                len: self.messages.len(),
            });
        }
        Ok(())
    }

    /// Put the turn's request back at the tail when a durable compaction
    /// folded it away ([`restore_current_turn_request`] at
    /// [`RestoredRequestPosition::Tail`]). Whether it was restored.
    ///
    /// [`restore_current_turn_request`]: crate::agent::context_preflight::restore_current_turn_request
    /// [`RestoredRequestPosition::Tail`]: crate::agent::context_preflight::RestoredRequestPosition::Tail
    pub(crate) fn restore_request_at_tail(
        &mut self,
        captured: Option<crate::agent::context_preflight::CapturedTurnRequest>,
        trigger: &'static str,
    ) -> bool {
        let restored = crate::agent::context_preflight::restore_current_turn_request(
            &mut self.messages,
            captured,
            trigger,
            crate::agent::context_preflight::RestoredRequestPosition::Tail,
        );
        if restored {
            self.origins.push(MessageOrigin::Request);
            self.evidence.push(None);
            self.sources.push(MessageSources::turn_round());
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
            &MessageSources::default(),
        ) {
            return false;
        }
        let behind = eviction.start.saturating_add(1);
        while self.messages.get(behind).is_some_and(|message| message.role == "tool") {
            self.messages.remove(behind);
            if behind < self.origins.len() {
                self.origins.remove(behind);
            }
            if behind < self.evidence.len() {
                self.evidence.remove(behind);
            }
            if behind < self.sources.len() {
                self.sources.remove(behind);
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
        let mut kept_evidence = Vec::with_capacity(self.evidence.len());
        let mut kept_sources = Vec::with_capacity(self.sources.len());
        for (((message, origin), evidence), sources) in self
            .messages
            .drain(..)
            .zip(self.origins.drain(..))
            .zip(self.evidence.drain(..))
            .zip(self.sources.drain(..))
        {
            if origin == MessageOrigin::RefreshAside {
                lifted.push((kept.len(), message));
            } else {
                kept.push(message);
                kept_origins.push(origin);
                kept_evidence.push(evidence);
                kept_sources.push(sources);
            }
        }
        self.messages = kept;
        self.origins = kept_origins;
        self.evidence = kept_evidence;
        self.sources = kept_sources;
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
                MessageOrigin::TurnRound | MessageOrigin::CallerSupplied => {
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
                    MessageOrigin::Request | MessageOrigin::TurnRound | MessageOrigin::RefreshAside => false,
                };
                (acknowledged && crate::agent::turn_contracts::is_durable_turn_message(message)).then(|| {
                    (
                        message.role.as_str(),
                        message.content.as_str(),
                        self.evidence.get(index).cloned().flatten(),
                    )
                })
            });
        Self::from_stored_turns(leading_system, turns, None)
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

/// Stored turns with their tool evidence shared, ready for the canonical
/// history (one copy per evidence record; later clones of the history share
/// it).
fn shared_evidence<'a, I>(turns: I) -> impl Iterator<Item = (&'a str, &'a str, Option<Arc<TurnToolEvidence>>)>
where
    I: IntoIterator<Item = (&'a str, &'a str, Option<&'a TurnToolEvidence>)>,
{
    turns
        .into_iter()
        .map(|(role, content, evidence)| (role, content, evidence.map(|evidence| Arc::new(evidence.clone()))))
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
    /// Keep every context handoff as stored instead of rendering it
    /// compactly: set for a caller without an acknowledged session, whose
    /// view is its own canonical history and is kept (and saved) as such.
    keep_stored_handoffs: bool,
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

impl CanonicalHistory {
    /// The budgeted token weight of each message as the provider view sends
    /// it, index for index: its rendered content (the same renderers
    /// [`project`] applies) plus, for an assistant reply, the tool rounds
    /// replayed before it. A stored handoff weighs its compact rendering, as the
    /// terminal and the gateway (which renders stored handoffs) send it.
    pub(crate) fn provider_weights(&self) -> Vec<usize> {
        self.messages
            .iter()
            .zip(&self.evidence)
            .map(|(message, evidence)| {
                let stored = message.content.as_str();
                let handed = render_context_handoff_for_provider(&message.role, stored);
                let rendered = match message.role.as_str() {
                    "assistant" => render_assistant_content(&handed, evidence.as_deref()).into_owned(),
                    "user" => render_user_content(&handed, evidence.as_deref()).into_owned(),
                    _ => handed.into_owned(),
                };
                let mut shown = message.clone();
                shown.content = rendered;
                let mut block = Vec::new();
                if message.role == "assistant"
                    && let Some(replay) = evidence.as_deref().and_then(|evidence| evidence.replay.as_ref())
                {
                    block.extend(replay.messages.iter().cloned());
                }
                block.push(shown);
                budgeted_tokens(estimate_history_tokens_with_source(&block))
            })
            .collect()
    }
}

/// The one conversion between the two: the provider view of `canonical`
/// under `inputs`. The enriched request replaces the canonical plain request,
/// the refresh aside follows the replacement it
/// was written after, the recalled page follows the system prompt, and the
/// skill metadata is stripped when suppressed.
pub(crate) fn project(canonical: &CanonicalHistory, inputs: &ProjectionInputs) -> ProviderView {
    let mut messages = canonical.messages.clone();
    // Each context handoff is shown in its compact form, by the one renderer,
    // before anything is inserted (indices still align).
    if !inputs.keep_stored_handoffs {
        for message in &mut messages {
            if let std::borrow::Cow::Owned(rendered) =
                render_context_handoff_for_provider(&message.role, &message.content)
            {
                message.content = rendered;
            }
        }
    }
    // Each acknowledged reply is shown with its tool evidence and each
    // acknowledged request in the form it was sent, by the one renderer of
    // each, before anything is inserted (indices still align).
    for (message, evidence) in messages.iter_mut().zip(&canonical.evidence) {
        let rendered = match message.role.as_str() {
            "assistant" => render_assistant_content(&message.content, evidence.as_deref()),
            "user" => render_user_content(&message.content, evidence.as_deref()),
            _ => continue,
        };
        if let std::borrow::Cow::Owned(rendered) = rendered {
            message.content = rendered;
        }
    }
    // The tool rounds of an acknowledged reply come right before it. Where
    // each canonical message starts in the expanded view is kept for the
    // refresh notes, which are placed by canonical index.
    let mut starts = Vec::new();
    if canonical
        .evidence
        .iter()
        .any(|evidence| evidence.as_deref().is_some_and(|evidence| evidence.replay.is_some()))
    {
        let mut expanded = Vec::with_capacity(messages.len());
        for (message, evidence) in std::mem::take(&mut messages).into_iter().zip(&canonical.evidence) {
            starts.push(expanded.len());
            if message.role == "assistant"
                && let Some(replay) = evidence.as_deref().and_then(|evidence| evidence.replay.as_ref())
            {
                expanded.extend(replay.messages.iter().cloned());
            }
            expanded.push(message);
        }
        starts.push(expanded.len());
        messages = expanded;
    }
    if let Some((plain, enriched)) = &inputs.enriched_request
        && let Some(request) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == plain.role && message.content == plain.content)
    {
        *request = enriched.clone();
    }
    for (offset, (at, note)) in inputs.refresh_notes.iter().enumerate() {
        let at = starts.get(*at).copied().unwrap_or(*at);
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
    /// The turn's own request as the view held it when the loop opened
    /// ([`Self::anchor_live_request`]).
    live_request: Option<ChatMessage>,
    /// The stored event of each message of the view the turn started on, when
    /// the caller knows them (see [`Self::with_history_event_ids`]).
    history_event_ids: Vec<Option<String>>,
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
        let mut projection = ProjectionInputs::default().with_enriched_request(plain_request, enriched_request);
        // A caller without a canonical history keeps the view as its history:
        // its handoffs stay as stored, so a later turn still resolves them.
        projection.keep_stored_handoffs = canonical.is_none();
        Self {
            caller_rollover_current: caller_rollover && canonical.is_some(),
            live_request: newest_request(view),
            canonical,
            absorbed_view_len: view.len(),
            history_event_ids: Vec::new(),
            projection,
            #[cfg(feature = "wasm-plugins")]
            inbound_middleware_applied: false,
        }
    }

    /// [`Self::new`] for a turn that also gets the stored event of each message
    /// of `view` ([`Self::with_history_event_ids`]). Only when
    /// `rendered_stored_handoffs` is set and the caller has no canonical
    /// history, a view that holds a stored context handoff takes its canonical
    /// copy now, from the stored bytes, and the view then shows that handoff
    /// in its compact form. The view keeps its length.
    pub(crate) fn open(
        view: &mut Vec<ChatMessage>,
        canonical: Option<CanonicalHistory>,
        caller_rollover: bool,
        event_ids: Vec<Option<String>>,
        rendered_stored_handoffs: bool,
    ) -> Self {
        let holds_stored_handoff = rendered_stored_handoffs
            && canonical.is_none()
            && view.iter().any(|message| {
                matches!(
                    render_context_handoff_for_provider(&message.role, &message.content),
                    std::borrow::Cow::Owned(_)
                )
            });
        if !holds_stored_handoff {
            return Self::new(view.as_slice(), canonical, caller_rollover).with_history_event_ids(event_ids);
        }
        let canonical = CanonicalHistory::of_caller_with_event_ids(view.as_slice(), view.len(), &event_ids);
        for message in view.iter_mut() {
            let rendered = render_context_handoff_for_provider(&message.role, &message.content);
            if let std::borrow::Cow::Owned(rendered) = rendered {
                message.content = rendered;
            }
        }
        // A canonical history makes `new` show the stored handoffs as the
        // compact rendering; no caller rollover was current before this turn.
        Self::new(view.as_slice(), Some(canonical), false).with_history_event_ids(event_ids)
    }

    /// Record the request the turn answers as `view` holds it now. The loop
    /// calls this once it opens, after the inbound middleware, so the anchor
    /// is the request exactly as the provider view shows it.
    pub(crate) fn anchor_live_request(&mut self, view: &[ChatMessage]) {
        self.live_request =
            crate::agent::history::latest_user_query_index(view).and_then(|index| view.get(index).cloned());
    }

    /// Where the turn's own request stands in `view`: the newest message equal
    /// to the request recorded at loop open, or the latest real user message
    /// ([`crate::agent::history::latest_user_query_index`]) when that is
    /// older, so a user-role message a later step appended never moves the
    /// start of the turn forward. `None` when the view holds neither.
    pub(crate) fn current_turn_start(&self, view: &[ChatMessage]) -> Option<usize> {
        let by_request = self.live_request.as_ref().and_then(|live| {
            view.iter()
                .rposition(|message| message.role == live.role && message.content == live.content)
        });
        let by_marker = crate::agent::history::latest_user_query_index(view);
        match (by_request, by_marker) {
            (Some(request), Some(marker)) => Some(request.min(marker)),
            (request, marker) => request.or(marker),
        }
    }

    /// Tell the turn the stored event of each message of the view it starts
    /// on, index for index (`None` for a message with no stored event). They
    /// apply only while the view still has that length when the canonical
    /// history is first taken from it.
    #[must_use]
    pub(crate) fn with_history_event_ids(mut self, event_ids: Vec<Option<String>>) -> Self {
        self.history_event_ids = event_ids;
        self
    }

    /// Whether this turn's history holds a context handoff: its canonical
    /// history does (a per-request trim may have left the handoff out of the
    /// view), or the provider view does (stored or rendered, both start with
    /// the marker). The one test both the recovery-tool check and its
    /// publication use.
    pub(crate) fn holds_context_handoff(&self, view: &[ChatMessage]) -> bool {
        crate::agent::compaction::holds_context_handoff(self.canonical(view))
            || crate::agent::compaction::holds_context_handoff(view)
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
        let (started_len, event_ids) = (self.absorbed_view_len, self.history_event_ids.as_slice());
        let canonical = self.canonical.take().map_or_else(
            || CanonicalHistory::of_caller_with_event_ids(view, started_len, event_ids),
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

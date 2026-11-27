//! Transcript provenance of a compaction source: resolving the messages a
//! compaction moves out of the window to the session's own log rows.

use super::handoff_note::{is_context_handoff_message, parse_context_handoff_note};
use super::roundtrips::RoundCompanions;
use super::summary::CompactionAuditSource;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::neutralize_reserved_markers;
use crate::context::transcript::{Attachment, Transcript};
use crate::memory::CompactionSourceEventRange;
use crate::memory::Memory;
use crate::memory::MemoryPrincipal;
use crate::memory::MessageEvent;
use crate::memory::SessionContextQuery;
use crate::providers::ChatMessage;
use anyhow::Result;
use std::collections::HashMap;
use std::collections::HashSet;

/// Provenance status used when every cold-window message is accounted for, but
/// some of them are tool round-trip parts covered by their assistant turn
/// rather than by an event id of their own.
pub(crate) const COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS: &str = "exact_with_tool_roundtrips";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionEventProvenance {
    pub(crate) source_event_ids: Vec<String>,
    pub(crate) covered_range: CompactionSourceEventRange,
    /// Messages in the cold window that belong to a tool round-trip and have no
    /// durable event of their own. They are covered by the assistant turn they
    /// hang off, not by an entry in `source_event_ids`.
    pub(crate) companion_message_count: usize,
}

/// The texts a history message may have been persisted as: the same text,
/// the text redacted (every turn commit since D-054), or the text redacted and
/// capped (chat events written before D-054, which capped them).
pub(crate) fn persisted_forms(content: &str) -> [String; 3] {
    [
        content.to_string(),
        crate::security::redaction::redact_secrets(content),
        crate::chat::sanitize::sanitize_for_persistence(content),
    ]
}

/// Whether a history message is the durable event it was persisted as
/// ([`persisted_forms`]); the provenance pool indexes the same forms.
#[cfg(test)]
pub(crate) fn source_message_matches_event(message: &ChatMessage, event: &MessageEvent) -> bool {
    message.role == event.role && persisted_forms(&message.content).contains(&event.content)
}

/// Rows per page of the provenance pool read (D8; the SQLite query cap,
/// `MAX_CONVERSATION_QUERY_LIMIT`). A shorter page means the session's first
/// row has been read.
pub(crate) const POOL_PAGE_ROWS: usize = 500;

/// Most rows the provenance pool reads (D8: 40 pages). A session with more own
/// user/assistant rows has a pool whose completeness is unproven, so its
/// provenance is never exact (D9).
pub(crate) const POOL_MAX_ROWS: usize = 20_000;

/// Candidate checks one segment search may spend (D7). Running out leaves
/// uniqueness unproven: the result is unavailable, never a guess.
pub(crate) const TURN_MATCH_STEP_BUDGET: usize = 200_000;

/// Why a compaction source has no exact transcript provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvenanceGap {
    /// The principal has no session key (or the source is empty).
    NoSessionKey,
    /// The durable remainder is empty or holds a role other than user/assistant.
    NonTranscriptSource,
    /// A handoff message does not match exactly one unused switch row.
    HandoffRowUnmatched,
    /// A run of durable messages has no admissible assignment of rows.
    TurnUnmatched,
    /// Uniqueness is not proven: two or more admissible assignments, an
    /// exhausted search budget, or a candidate pool that may be truncated.
    TurnAmbiguous,
}

impl ProvenanceGap {
    pub(crate) const ALL: [Self; 5] = [
        Self::NoSessionKey,
        Self::NonTranscriptSource,
        Self::HandoffRowUnmatched,
        Self::TurnUnmatched,
        Self::TurnAmbiguous,
    ];

    /// The provenance status recorded for this gap.
    pub(crate) const fn status(self) -> &'static str {
        match self {
            Self::NoSessionKey => "unavailable:no_session_key",
            Self::NonTranscriptSource => "unavailable:non_transcript_source",
            Self::HandoffRowUnmatched => "unavailable:handoff_row_unmatched",
            Self::TurnUnmatched => "unavailable:turn_unmatched",
            Self::TurnAmbiguous => "unavailable:turn_ambiguous",
        }
    }
}

/// The outcome of resolving a compaction source to its transcript rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProvenanceResolution {
    Exact(CompactionEventProvenance),
    Unavailable(ProvenanceGap),
}

/// Role of a pool row (only user and assistant rows are transcript rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PoolRole {
    User,
    Assistant,
}

impl PoolRole {
    fn of(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// One own `message.created` row of the session whose text matches at least
/// one durable source message. Its content is not kept.
#[derive(Debug, Clone)]
pub(crate) struct PoolRow {
    pub(crate) id: i64,
    pub(crate) event_id: String,
    pub(crate) role: PoolRole,
    /// The row's run id when it is non-empty after trim.
    pub(crate) run_id: Option<String>,
    /// Indices of the durable source messages this row matches
    /// ([`persisted_forms`] of the same role).
    pub(crate) matches: Vec<usize>,
    /// Position among every own user/assistant `message.created` row read, in
    /// id order: the store adjacency the contiguous seed is computed over.
    pub(crate) store_rank: usize,
}

/// One own `context.switch.created` row: the handoff messages it matches and
/// what its note references. Its content is not kept.
#[derive(Debug, Clone)]
pub(crate) struct PoolSwitchRow {
    id: i64,
    event_id: String,
    matches: Vec<usize>,
    lookup_event_ids: Vec<String>,
    parent_handoff_event_id: Option<String>,
}

/// The keys of one own user/assistant row that decide task echoes (the same
/// rule the child history loader applies). Its content is not kept.
#[derive(Debug)]
struct TaskRowKey {
    id: i64,
    workspace_id: String,
    owner_id: String,
    role: PoolRole,
    content_hash: String,
    /// The row's raw run id.
    run_id: Option<String>,
    /// For a `message.created` row: the string `task_run_id` of its payload.
    task_run_id: Option<String>,
}

impl TaskRowKey {
    /// The key of `event`, or `None` when the row cannot take part in an echo
    /// match: it needs an owner and a content hash, and a run id or a task run id.
    fn of(event: &MessageEvent, role: PoolRole) -> Option<Self> {
        let owner_id = event.owner_id.clone()?;
        let content_hash = event.content_hash.clone()?;
        let task_run_id = if event.event_type == "message.created" {
            task_run_id_of(event.raw_payload_json.as_deref())
        } else {
            None
        };
        if event.run_id.is_none() && task_run_id.is_none() {
            return None;
        }
        Some(Self {
            id: event.id,
            workspace_id: event.workspace_id.clone(),
            owner_id,
            role,
            content_hash,
            run_id: event.run_id.clone(),
            task_run_id,
        })
    }
}

/// The string `task_run_id` of a row's raw payload. A payload that does not
/// parse, or whose `task_run_id` is absent or not a string, yields `None`, so
/// its row is never an echo.
fn task_run_id_of(raw_payload_json: Option<&str>) -> Option<String> {
    let raw = raw_payload_json?;
    if !raw.contains("task_run_id") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get("task_run_id")?.as_str().map(str::to_owned)
}

/// The ids of the task echoes among `keys`: a `message.created` row E whose
/// `task_run_id` names the run of an earlier row P with the same workspace,
/// owner, role and content hash. Owner and hash must be present, as NULL never
/// matches in the history loader.
fn task_echo_ids(keys: &[TaskRowKey]) -> HashSet<i64> {
    // The smallest row id per (workspace, owner, role, content hash, run id).
    let mut first: HashMap<(&str, &str, PoolRole, &str, &str), i64> = HashMap::new();
    for key in keys {
        let Some(run) = key.run_id.as_deref() else {
            continue;
        };
        let slot = first
            .entry((
                key.workspace_id.as_str(),
                key.owner_id.as_str(),
                key.role,
                key.content_hash.as_str(),
                run,
            ))
            .or_insert(i64::MAX);
        *slot = (*slot).min(key.id);
    }
    keys.iter()
        .filter_map(|key| {
            let task_run = key.task_run_id.as_deref()?;
            let earliest = first.get(&(
                key.workspace_id.as_str(),
                key.owner_id.as_str(),
                key.role,
                key.content_hash.as_str(),
                task_run,
            ))?;
            (*earliest < key.id).then_some(key.id)
        })
        .collect()
}

/// Every own transcript row of the session that could be a candidate, read to
/// the session's first row (§3.1).
#[derive(Debug, Default)]
pub(crate) struct CandidatePool {
    /// Matching `message.created` rows, sorted by id.
    pub(crate) messages: Vec<PoolRow>,
    pub(crate) switches: Vec<PoolSwitchRow>,
    /// Ids of every own user/assistant `message.created` row read.
    pub(crate) message_ids: Vec<i64>,
    /// Keys of the own user/assistant rows that can match or be a task echo.
    /// Read only until [`CandidatePool::finish`] removes the echoes.
    task_keys: Vec<TaskRowKey>,
}

/// The accepted persisted forms of every durable source message
/// ([`persisted_forms`]: raw, redacted, sanitized), keyed by role
/// and form, to the indices of the messages that have them.
pub(crate) struct SourceForms {
    by_role: HashMap<String, HashMap<String, Vec<usize>>>,
    /// The stored attachment of each user message, by its event id. A user
    /// message is replayed as its text and attachment tail, so that form is
    /// matched as well ([`Self::row_matches`]).
    attachments: HashMap<String, Attachment>,
}

impl SourceForms {
    pub(crate) fn new(messages: &[&ChatMessage]) -> Self {
        let mut by_role: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        for (index, message) in messages.iter().enumerate() {
            let forms = by_role.entry(message.role.clone()).or_default();
            for form in persisted_forms(&message.content) {
                let indices = forms.entry(form).or_default();
                if indices.last() != Some(&index) {
                    indices.push(index);
                }
            }
        }
        Self {
            by_role,
            attachments: HashMap::new(),
        }
    }

    /// These forms with the stored attachments of the session's own rows,
    /// by user event id. An empty map leaves the plain-text match as it was.
    #[must_use]
    pub(crate) fn with_attachments(mut self, attachments: HashMap<String, Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    fn matches(&self, role: &str, content: &str) -> Vec<usize> {
        self.by_role
            .get(role)
            .and_then(|forms| forms.get(content))
            .cloned()
            .unwrap_or_default()
    }

    /// The source messages a stored row may stand for: the union, ascending
    /// and de-duplicated, of its plain text and, for a user row, the text as
    /// it is replayed: its stored attachment's tail after the text
    /// ([`Attachment::replayed`]), neutralized exactly as the replay
    /// neutralizes it. Byte equality only.
    fn row_matches(&self, role: &str, event_id: &str, content: &str) -> Vec<usize> {
        let mut indices = self.matches(role, content);
        if role == "user" {
            indices.extend(self.matches(role, &neutralize_reserved_markers(content)));
            if let Some(attachment) = self.attachments.get(event_id) {
                let replayed = attachment.replayed(content);
                indices.extend(self.matches(role, &neutralize_reserved_markers(&replayed)));
            }
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

/// The stored attachments of the session's own rows, by user event id, read
/// with the principal the pool is read with. A failed read gives none (the
/// reader logs it), so the match is then the plain-text match alone.
pub(super) async fn stored_attachments(mem: &dyn Memory, principal: &MemoryPrincipal) -> HashMap<String, Attachment> {
    Transcript::new(mem, principal.clone())
        .replay_log()
        .await
        .into_attachments()
}

/// The lookup event ids and parent of a stored handoff note.
pub(crate) fn switch_note_references(content: &str) -> (Vec<String>, Option<String>) {
    let Some(note) = parse_context_handoff_note(content) else {
        return (Vec::new(), None);
    };
    let lookup_event_ids = note
        .pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let parent = note
        .get("parent_handoff_event_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    (lookup_event_ids, parent)
}

impl CandidatePool {
    /// Keep a compact record of every own transcript row of `page`; drop
    /// every row's content.
    fn absorb(&mut self, page: Vec<MessageEvent>, session_key: Option<&str>, forms: &SourceForms) {
        for event in page {
            // Defence in depth: only the session's own rows (canonical key).
            if event.session_key.as_deref() != session_key {
                continue;
            }
            let Some(role) = PoolRole::of(&event.role) else {
                continue;
            };
            if let Some(key) = TaskRowKey::of(&event, role) {
                self.task_keys.push(key);
            }
            match event.event_type.as_str() {
                "message.created" => {
                    self.message_ids.push(event.id);
                    let matches = forms.row_matches(&event.role, &event.event_id, &event.content);
                    if !matches.is_empty() {
                        self.messages.push(PoolRow {
                            id: event.id,
                            event_id: event.event_id,
                            role,
                            run_id: event.run_id.filter(|run| !run.trim().is_empty()),
                            matches,
                            store_rank: 0,
                        });
                    }
                }
                "context.switch.created" => {
                    let (lookup_event_ids, parent_handoff_event_id) = switch_note_references(&event.content);
                    self.switches.push(PoolSwitchRow {
                        id: event.id,
                        matches: forms.matches(&event.role, &event.content),
                        event_id: event.event_id,
                        lookup_event_ids,
                        parent_handoff_event_id,
                    });
                }
                _ => {}
            }
        }
    }

    /// Drop the task echoes, sort the rows by id and rank every matching row in
    /// store order.
    fn finish(&mut self) {
        let echoes = task_echo_ids(&self.task_keys);
        self.task_keys = Vec::new();
        if !echoes.is_empty() {
            self.message_ids.retain(|id| !echoes.contains(id));
            self.messages.retain(|row| !echoes.contains(&row.id));
        }
        self.message_ids.sort_unstable();
        self.message_ids.dedup();
        self.messages.sort_by_key(|row| row.id);
        self.messages.dedup_by_key(|row| row.id);
        for row in &mut self.messages {
            row.store_rank = self
                .message_ids
                .binary_search(&row.id)
                .unwrap_or_else(|insertion| insertion);
        }
    }
}

/// Whether a paging `error` leaves the pool unproven rather than failing the
/// read: the fail-fast error of a backend without the method, or a page the
/// backend could not return whole ([`crate::memory::traits::SessionPageIncomplete`]).
pub(crate) fn is_unproven_page_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::memory::erasure::Unsupported>()
        .is_some_and(|unsupported| unsupported.0 == "fabric operation")
        || error
            .downcast_ref::<crate::memory::traits::SessionPageIncomplete>()
            .is_some()
}

/// Every own user/assistant row of the session (§3.1): the newest page, then
/// older pages until a short page proves the session's first row was read.
/// Every page, the newest included, is read through
/// `load_session_context_before`, which reports rows it would have to drop.
/// `Ok(Err(TurnAmbiguous))` when completeness cannot be proven: the rows read
/// reached [`POOL_MAX_ROWS`], the backend cannot page, or a page is incomplete.
pub(crate) async fn load_complete_pool(
    mem: &dyn Memory,
    principal: &MemoryPrincipal,
    forms: &SourceForms,
) -> Result<Result<CandidatePool, ProvenanceGap>> {
    load_pool_within(mem, principal, forms, POOL_MAX_ROWS).await
}

pub(crate) async fn load_pool_within(
    mem: &dyn Memory,
    principal: &MemoryPrincipal,
    forms: &SourceForms,
    max_rows: usize,
) -> Result<Result<CandidatePool, ProvenanceGap>> {
    let query = || SessionContextQuery {
        principal: principal.clone(),
        since_event_id: None,
        limit: POOL_PAGE_ROWS,
        include_roles: vec!["user".to_string(), "assistant".to_string()],
    };
    let mut pool = CandidatePool::default();
    let mut rows_read = 0usize;
    let mut oldest: Option<i64> = None;
    // The newest page is "below" the largest possible id.
    let mut page = match mem.load_session_context_before(query(), i64::MAX).await {
        Ok(page) => page,
        Err(error) if is_unproven_page_error(&error) => return Ok(Err(ProvenanceGap::TurnAmbiguous)),
        Err(error) => return Err(error),
    };
    loop {
        let page_len = page.len();
        rows_read = rows_read.saturating_add(page_len);
        if let Some(page_oldest) = page.iter().map(|event| event.id).min() {
            oldest = Some(oldest.map_or(page_oldest, |seen| seen.min(page_oldest)));
        }
        pool.absorb(page, principal.session_key.as_deref(), forms);
        if page_len < POOL_PAGE_ROWS {
            break;
        }
        if rows_read >= max_rows {
            return Ok(Err(ProvenanceGap::TurnAmbiguous));
        }
        let Some(before_id) = oldest else {
            return Ok(Err(ProvenanceGap::TurnAmbiguous));
        };
        page = match mem.load_session_context_before(query(), before_id).await {
            Ok(page) => page,
            Err(error) if is_unproven_page_error(&error) => return Ok(Err(ProvenanceGap::TurnAmbiguous)),
            Err(error) => return Err(error),
        };
    }
    pool.finish();
    Ok(Ok(pool))
}

/// The group structure of a segment: for each message its role, the user that
/// opened its turn group and the nearest earlier assistant.
pub(crate) struct SegmentShape {
    roles: Vec<PoolRole>,
    group_user: Vec<Option<usize>>,
    previous_assistant: Vec<Option<usize>>,
}

impl SegmentShape {
    fn new(segment: &[&ChatMessage]) -> Option<Self> {
        let mut shape = Self {
            roles: Vec::with_capacity(segment.len()),
            group_user: Vec::with_capacity(segment.len()),
            previous_assistant: Vec::with_capacity(segment.len()),
        };
        let mut current_user = None;
        let mut last_assistant = None;
        for (index, message) in segment.iter().enumerate() {
            let role = PoolRole::of(&message.role)?;
            match role {
                PoolRole::User => {
                    current_user = Some(index);
                    shape.group_user.push(None);
                    shape.previous_assistant.push(None);
                }
                PoolRole::Assistant => {
                    shape.group_user.push(current_user);
                    shape.previous_assistant.push(last_assistant);
                    last_assistant = Some(index);
                }
            }
            shape.roles.push(role);
        }
        Some(shape)
    }

    /// Whether `row` may stand for message `index` given the rows `chosen`
    /// for the messages before it (A1, A3, A4, A5; A2 holds by construction).
    fn admits(&self, index: usize, row_index: usize, pool: &[PoolRow], chosen: &[usize], taken: &[bool]) -> bool {
        let Some(row) = pool.get(row_index) else {
            return false;
        };
        // (A1) one-to-one.
        if taken.get(row_index).copied().unwrap_or(true) {
            return false;
        }
        if self.roles.get(index) != Some(&PoolRole::Assistant) {
            return true;
        }
        let chosen_row = |message: usize| chosen.get(message).and_then(|&chosen_index| pool.get(chosen_index));
        // (A3) commit order among assistant rows.
        if let Some(previous) = self.previous_assistant.get(index).copied().flatten()
            && chosen_row(previous).is_none_or(|previous_row| previous_row.id >= row.id)
        {
            return false;
        }
        if let Some(user) = self.group_user.get(index).copied().flatten() {
            let Some(user_row) = chosen_row(user) else {
                return false;
            };
            // (A4) request before reply.
            if user_row.id >= row.id {
                return false;
            }
            // (A5) run consistency where both rows carry a run.
            if let (Some(user_run), Some(reply_run)) = (user_row.run_id.as_deref(), row.run_id.as_deref())
                && user_run != reply_run
            {
                return false;
            }
        }
        true
    }
}

/// The contiguous store window for the segment (today's rule), as one pool
/// index per message: a search seed only, never a justification.
pub(crate) fn contiguous_window(candidates: &[Vec<usize>], pool: &[PoolRow]) -> Option<Vec<usize>> {
    let first = candidates.first()?;
    'start: for &start in first {
        let start_rank = pool.get(start)?.store_rank;
        let mut window = Vec::with_capacity(candidates.len());
        window.push(start);
        for (offset, list) in candidates.iter().enumerate().skip(1) {
            let rank = start_rank.saturating_add(offset);
            let found = list.binary_search_by(|&row_index| {
                pool.get(row_index)
                    .map_or(std::cmp::Ordering::Less, |row| row.store_rank.cmp(&rank))
            });
            let Ok(position) = found else {
                continue 'start;
            };
            window.push(*list.get(position)?);
        }
        return Some(window);
    }
    None
}

/// The unique admissible rows for `segment` (durable messages between
/// handoffs, in history order; `first_index` is the index of its first message
/// in the durable source). `Err` names why uniqueness is not proven.
pub(crate) fn match_segment<'e>(
    segment: &[&ChatMessage],
    first_index: usize,
    pool: &'e [PoolRow],
    used_event_ids: &HashSet<String>,
) -> Result<Vec<&'e PoolRow>, ProvenanceGap> {
    match_segment_within(segment, first_index, pool, used_event_ids, TURN_MATCH_STEP_BUDGET)
}

/// [`match_segment`] with an explicit step `budget`: counts admissible
/// assignments by an iterative depth-first search, capped at 2.
pub(crate) fn match_segment_within<'e>(
    segment: &[&ChatMessage],
    first_index: usize,
    pool: &'e [PoolRow],
    used_event_ids: &HashSet<String>,
    budget: usize,
) -> Result<Vec<&'e PoolRow>, ProvenanceGap> {
    let message_count = segment.len();
    if message_count > crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS {
        return Err(ProvenanceGap::TurnUnmatched);
    }
    if message_count == 0 {
        return Ok(Vec::new());
    }
    let shape = SegmentShape::new(segment).ok_or(ProvenanceGap::TurnUnmatched)?;
    // (A2) candidates per message, ascending id (the pool is sorted by id).
    let mut candidates = vec![Vec::new(); message_count];
    for (row_index, row) in pool.iter().enumerate() {
        if used_event_ids.contains(&row.event_id) {
            continue;
        }
        for &message in &row.matches {
            let Some(local) = message.checked_sub(first_index) else {
                continue;
            };
            if let (Some(list), Some(role)) = (candidates.get_mut(local), shape.roles.get(local))
                && *role == row.role
            {
                list.push(row_index);
            }
        }
    }
    // Contiguous first: the window's row leads each candidate list.
    if let Some(window) = contiguous_window(&candidates, pool) {
        for (list, seed) in candidates.iter_mut().zip(window) {
            list.retain(|&row_index| row_index != seed);
            list.insert(0, seed);
        }
    }

    let mut next = vec![0usize; message_count];
    let mut chosen: Vec<usize> = Vec::with_capacity(message_count);
    let mut taken = vec![false; pool.len()];
    let mut steps = 0usize;
    let mut found: Option<Vec<usize>> = None;
    loop {
        let depth = chosen.len();
        let (Some(list), Some(position)) = (candidates.get(depth), next.get(depth).copied()) else {
            return Err(ProvenanceGap::TurnUnmatched);
        };
        if let Some(&row_index) = list.get(position) {
            if let Some(slot) = next.get_mut(depth) {
                *slot = position.saturating_add(1);
            }
            steps = steps.saturating_add(1);
            if steps > budget {
                return Err(ProvenanceGap::TurnAmbiguous);
            }
            if !shape.admits(depth, row_index, pool, &chosen, &taken) {
                continue;
            }
            if depth.saturating_add(1) == message_count {
                if found.is_some() {
                    return Err(ProvenanceGap::TurnAmbiguous);
                }
                let mut assignment = chosen.clone();
                assignment.push(row_index);
                found = Some(assignment);
                continue;
            }
            if let Some(flag) = taken.get_mut(row_index) {
                *flag = true;
            }
            chosen.push(row_index);
            if let Some(slot) = next.get_mut(depth.saturating_add(1)) {
                *slot = 0;
            }
            continue;
        }
        // Every candidate of this message was tried: backtrack.
        let Some(previous) = chosen.pop() else {
            break;
        };
        if let Some(flag) = taken.get_mut(previous) {
            *flag = false;
        }
    }
    let assignment = found.ok_or(ProvenanceGap::TurnUnmatched)?;
    let rows = assignment
        .iter()
        .filter_map(|&row_index| pool.get(row_index))
        .collect::<Vec<_>>();
    if rows.len() == message_count {
        Ok(rows)
    } else {
        Err(ProvenanceGap::TurnUnmatched)
    }
}

/// The rows referenced by the adopted handoff chain: each matched switch row's
/// note, followed through `parent_handoff_event_id` to its ancestors in the
/// pool. A missing link claims nothing more.
pub(crate) fn claimed_event_ids(pool: &CandidatePool, adopted: &[&PoolSwitchRow]) -> HashSet<String> {
    let mut claimed = HashSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for switch in adopted {
        let mut current = Some(*switch);
        while let Some(row) = current {
            if !visited.insert(row.event_id.as_str()) {
                break;
            }
            claimed.extend(row.lookup_event_ids.iter().cloned());
            current = row
                .parent_handoff_event_id
                .as_deref()
                .and_then(|parent| pool.switches.iter().find(|candidate| candidate.event_id == parent));
        }
    }
    claimed
}

/// The `(row id, event id)` of every durable message, in history order.
pub(crate) fn resolve_in_pool(
    messages: &[&ChatMessage],
    mut pool: CandidatePool,
) -> Result<Vec<(i64, String)>, ProvenanceGap> {
    // Handoffs first: each matches exactly one unused switch row.
    let mut used_event_ids: HashSet<String> = HashSet::new();
    let mut handoff_rows: Vec<Option<usize>> = vec![None; messages.len()];
    for (index, message) in messages.iter().enumerate() {
        if !is_context_handoff_message(message) {
            continue;
        }
        let candidates = pool
            .switches
            .iter()
            .enumerate()
            .filter(|(_, row)| row.matches.contains(&index) && !used_event_ids.contains(&row.event_id))
            .map(|(switch_index, _)| switch_index)
            .collect::<Vec<_>>();
        let [switch_index] = candidates.as_slice() else {
            return Err(ProvenanceGap::HandoffRowUnmatched);
        };
        let row = pool.switches.get(*switch_index).ok_or(ProvenanceGap::TurnUnmatched)?;
        used_event_ids.insert(row.event_id.clone());
        let slot = handoff_rows.get_mut(index).ok_or(ProvenanceGap::TurnUnmatched)?;
        *slot = Some(*switch_index);
    }
    let adopted = handoff_rows
        .iter()
        .flatten()
        .filter_map(|&switch_index| pool.switches.get(switch_index))
        .collect::<Vec<_>>();
    let claimed = claimed_event_ids(&pool, &adopted);
    pool.messages.retain(|row| !claimed.contains(&row.event_id));

    let mut matched = Vec::new();
    let mut source_index = 0usize;
    while source_index < messages.len() {
        if let Some(switch_index) = handoff_rows.get(source_index).copied().flatten() {
            let row = pool.switches.get(switch_index).ok_or(ProvenanceGap::TurnUnmatched)?;
            matched.push((row.id, row.event_id.clone()));
            source_index = source_index.saturating_add(1);
            continue;
        }
        let remaining = messages.get(source_index..).ok_or(ProvenanceGap::TurnUnmatched)?;
        let segment_end = remaining
            .iter()
            .position(|message| is_context_handoff_message(message))
            .map_or(messages.len(), |offset| source_index.saturating_add(offset));
        let segment = messages
            .get(source_index..segment_end)
            .ok_or(ProvenanceGap::TurnUnmatched)?;
        let rows = match_segment(segment, source_index, &pool.messages, &used_event_ids)?;
        for row in rows {
            used_event_ids.insert(row.event_id.clone());
            matched.push((row.id, row.event_id.clone()));
        }
        source_index = segment_end;
    }
    Ok(matched)
}

pub(crate) async fn resolve_compaction_event_provenance(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> anyhow::Result<Option<CompactionEventProvenance>> {
    Ok(
        match resolve_compaction_event_provenance_detailed(mem, principal, source_messages, companions).await? {
            ProvenanceResolution::Exact(provenance) => Some(provenance),
            ProvenanceResolution::Unavailable(_) => None,
        },
    )
}

/// Resolve `source_messages` to the session's own transcript rows (§3): exact
/// only when every durable message has exactly one admissible row within a
/// pool proven complete; otherwise the precise gap.
pub(crate) async fn resolve_compaction_event_provenance_detailed(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> anyhow::Result<ProvenanceResolution> {
    resolve_provenance_with_kept(mem, principal, source_messages, companions, &[]).await
}

/// [`resolve_compaction_event_provenance_detailed`] for `source_messages`
/// resolved together with `kept`: acknowledged messages after the source,
/// in history order, each of which claims a row of its own, with the stored
/// event ids recorded for it (empty when none). The assignment must be unique
/// over source and `kept` together, and every kept message with a recorded id
/// must claim exactly that row; only the source's rows are returned.
pub(crate) async fn resolve_provenance_with_kept(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
    kept: &[(&ChatMessage, Vec<String>)],
) -> anyhow::Result<ProvenanceResolution> {
    if principal.session_key.is_none() || source_messages.is_empty() {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::NoSessionKey));
    }

    // Tool round-trip messages are never persisted verbatim, so they cannot be
    // matched against transcript events. Fold them into the assistant turn that
    // produced them and resolve provenance over the durable remainder.
    let companion_message_count = (0..source_messages.len())
        .filter(|index| companions.is_companion(*index))
        .count();
    let durable_messages = source_messages
        .iter()
        .enumerate()
        .filter(|(index, _)| !companions.is_companion(*index))
        .map(|(_, message)| message)
        .collect::<Vec<_>>();
    if durable_messages.is_empty()
        || durable_messages
            .iter()
            .any(|message| !matches!(message.role.as_str(), "user" | "assistant"))
    {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::NonTranscriptSource));
    }
    let source_count = durable_messages.len();
    let mut joint_messages = durable_messages;
    joint_messages.extend(kept.iter().map(|(message, _)| *message));

    // Only the session's own rows (canonical key): rows under the legacy key
    // (child or worker traffic) never take part.
    let principal = MemoryPrincipal {
        legacy_session_key: None,
        ..principal
    };
    let attachments = stored_attachments(mem, &principal).await;
    let forms = SourceForms::new(&joint_messages).with_attachments(attachments);
    let pool = match load_complete_pool(mem, &principal, &forms).await? {
        Ok(pool) => pool,
        Err(gap) => return Ok(ProvenanceResolution::Unavailable(gap)),
    };
    let mut matched = match resolve_in_pool(&joint_messages, pool) {
        Ok(matched) => matched,
        Err(gap) => return Ok(ProvenanceResolution::Unavailable(gap)),
    };
    // One row per message in history order: the source's rows lead.
    if matched.len() != joint_messages.len() {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
    }
    // A kept message with a recorded event id must claim that row. Several
    // recorded ids, or a different row, is a doubt: the source is refused.
    for (offset, (_, recorded)) in kept.iter().enumerate() {
        if recorded.is_empty() {
            continue;
        }
        let [expected] = recorded.as_slice() else {
            return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous));
        };
        let Some((_, claimed)) = matched.get(source_count.saturating_add(offset)) else {
            return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
        };
        if claimed != expected {
            return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous));
        }
    }
    matched.truncate(source_count);

    let Some((first_row_id, first_event_id)) = matched.iter().min_by_key(|(id, _)| *id) else {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
    };
    let Some((last_row_id, last_event_id)) = matched.iter().max_by_key(|(id, _)| *id) else {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
    };
    let covered_range = CompactionSourceEventRange {
        first_event_id: first_event_id.clone(),
        last_event_id: last_event_id.clone(),
        first_row_id: *first_row_id,
        last_row_id: *last_row_id,
        source_event_count: matched.len(),
    };
    let source_event_ids = matched.into_iter().map(|(_, event_id)| event_id).collect::<Vec<_>>();
    Ok(ProvenanceResolution::Exact(CompactionEventProvenance {
        source_event_ids,
        covered_range,
        companion_message_count,
    }))
}

pub(crate) async fn attach_compaction_event_provenance(
    audit: &DocumentIngestRuntime,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
    kept_acknowledged: &[(&ChatMessage, Vec<String>)],
    source: &mut CompactionAuditSource,
) {
    let resolution = match resolve_compaction_event_provenance_detailed(
        &*audit.memory,
        audit.memory_principal(),
        source_messages,
        companions,
    )
    .await
    {
        // The cold window alone is ambiguous: resolve it together with the
        // kept acknowledged messages, each of which claims its own row. Only a
        // unique joint assignment is exact; anything else stays ambiguous.
        Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous)) if !kept_acknowledged.is_empty() => {
            match resolve_provenance_with_kept(
                &*audit.memory,
                audit.memory_principal(),
                source_messages,
                companions,
                kept_acknowledged,
            )
            .await
            {
                Ok(ProvenanceResolution::Exact(provenance)) => {
                    tracing::debug!(
                        kept_messages = kept_acknowledged.len(),
                        "cold window provenance resolved jointly with the kept messages"
                    );
                    Ok(ProvenanceResolution::Exact(provenance))
                }
                Ok(ProvenanceResolution::Unavailable(_)) => {
                    Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous))
                }
                Err(error) => {
                    let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                    tracing::debug!(
                        error_len,
                        error_sha256,
                        "failed to resolve compaction provenance jointly with the kept messages"
                    );
                    Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous))
                }
            }
        }
        other => other,
    };
    match resolution {
        Ok(ProvenanceResolution::Exact(provenance)) => {
            source.source_event_ids = provenance.source_event_ids;
            source.source_event_range = Some(provenance.covered_range);
            source.unpersisted_companion_count = provenance.companion_message_count;
            source.source_event_provenance_status = if provenance.companion_message_count == 0 {
                "exact".to_string()
            } else {
                COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS.to_string()
            };
        }
        Ok(ProvenanceResolution::Unavailable(gap)) => {
            source.source_event_provenance_status = gap.status().to_string();
        }
        Err(error) => {
            source.source_event_provenance_status = "read_error".to_string();
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::debug!(
                error_len,
                error_sha256,
                "failed to resolve compaction MessageEvent provenance"
            );
        }
    }
}

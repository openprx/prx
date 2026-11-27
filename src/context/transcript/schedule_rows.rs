//! The scheduled-results note: the owner's unseen `schedule.result.recorded`
//! rows, shown once at the opening of the owner's next interactive turn.
//!
//! The note is built from the rows of the owner and agent partition, across
//! every session. Rows are grouped per scheduled session (the transcript
//! session of the run): one line per group, the newest row shown, older failed
//! attempts dropped as superseded, and older successful runs counted as
//! `+k earlier`. The seen mark is a `schedule.result.seen` row holding the
//! newest row id the note has read; the next turn reads only rows after it.
//! A mark counts only when its turn reached `turn.finalized` (the
//! `turn:<run>:final` row) with a status that kept the note: `completed` or
//! `silent` on any surface, and `failed` or `cancelled` only when the mark's
//! source is Console, the one note-rendering surface whose history replays the
//! aborted user message with its stored tail (see `aborted_input_replayed`).
//! On every other surface a failed or cancelled turn leaves the note unseen
//! and the next turn shows it again. Each new mark also carries the seen point
//! it was computed from (`floor`), so unverified marks never push a verified
//! point out of the read window. A note shown past the line limit ends with a
//! `+n more` line.
//! Rows older than seven days are not shown but are covered by the mark. A
//! group whose newest row was delivered as a message is not shown, unless an
//! older successful run of it was not delivered; its older failed attempts are
//! superseded, and the mark covers every row read.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::agent::diagnostics::error_log_digest;
use crate::agent::scheduled_result::SCHEDULE_RESULT_EVENT_TYPE;
use crate::memory::{Memory, MemoryFabric, MemoryPrincipal, MessageEvent, MessageEventScope, MessageEventSource};

/// The event type of the seen mark.
pub(crate) const SCHEDULE_SEEN_EVENT_TYPE: &str = "schedule.result.seen";
/// Seen-mark rows read per turn, newest first. The seen point is the largest
/// `floor` or counting `through_id` among them, so a late writer cannot move it back.
const SEEN_ROWS_READ: usize = 50;
/// Result rows read per turn, newest first.
const RESULT_ROWS_READ: usize = 50;
/// Lines shown in the note at most; the rest are covered by the mark.
const NOTE_MAX_LINES: usize = 10;
/// Age, from the run's finish, after which a result is not shown.
const NOTE_MAX_AGE_DAYS: i64 = 7;
/// Characters of a reply excerpt shown on one line.
const NOTE_EXCERPT_CHARS: usize = 200;

/// A seen mark to write: the newest result row the note covers, and the seen
/// point (`floor`) the turn computed it against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeenMark {
    pub(crate) through_id: i64,
    pub(crate) floor: Option<i64>,
}

/// The note for the owner's unseen scheduled results, and the mark to write.
/// `None` when there is no result after the seen point or the read fails (logged
/// by digest). The mark is `Some` only when its row id is above the seen point. The text is empty when every row read is too old or
/// superseded; the mark is still written then.
pub(crate) async fn unseen_results_note(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    now: DateTime<Utc>,
) -> Option<(String, Option<SeenMark>)> {
    let seen = read_rows(memory, principal, SCHEDULE_SEEN_EVENT_TYPE, None, SEEN_ROWS_READ).await?;
    let seen_through = seen_point(memory, principal, &seen).await;
    let rows = read_rows(
        memory,
        principal,
        SCHEDULE_RESULT_EVENT_TYPE,
        seen_through,
        RESULT_ROWS_READ,
    )
    .await?;
    let through = rows.iter().map(|row| row.id).max()?;
    let mark = seen_through.map_or(true, |seen| through > seen).then_some(SeenMark {
        through_id: through,
        floor: seen_through,
    });
    let max_age = Duration::days(NOTE_MAX_AGE_DAYS);
    let mut groups: Vec<Group> = Vec::new();
    for row in &rows {
        let Some(payload) = payload_of(row) else {
            continue;
        };
        if let Some(finished) = payload
            .get("finished_at")
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            && now.signed_duration_since(finished.with_timezone(&Utc)) > max_age
        {
            continue;
        }
        let key = payload
            .get("transcript_session")
            .and_then(Value::as_str)
            .or(row.session_key.as_deref())
            .unwrap_or(row.event_id.as_str())
            .to_owned();
        let ok = payload.get("status").and_then(Value::as_str) == Some("ok");
        let undelivered_ok = ok && payload.get("delivered").and_then(Value::as_bool) != Some(true);
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => {
                if ok {
                    group.earlier += 1;
                }
                group.undelivered_ok_earlier |= undelivered_ok;
            }
            None => groups.push(Group {
                key,
                newest: payload,
                earlier: 0,
                undelivered_ok_earlier: undelivered_ok,
            }),
        }
    }
    // A group whose newest row was sent as a message is already in the session,
    // unless an older successful run of it was never delivered.
    let pending: Vec<&Group> = groups.iter().filter(|group| !group.delivered()).collect();
    let mut lines: Vec<String> = pending.iter().take(NOTE_MAX_LINES).map(|group| group.line()).collect();
    if pending.len() > NOTE_MAX_LINES {
        lines.push(format!("+{} more", pending.len() - NOTE_MAX_LINES));
    }
    let text = if lines.is_empty() {
        String::new()
    } else {
        format!("\n\n[Scheduled results]\n{}", lines.join("\n"))
    };
    Some((text, mark))
}

/// Whether the aborted (failed or cancelled) user message of a turn from this
/// source is replayed, with its stored tail, in later context. Only Console
/// history keeps it: the gateway session history reads every user row of the
/// log and attaches the tail by event id. `prx chat` cuts the aborted request
/// from the session, and channel turns replay no tail at all, so for them the
/// note was lost with the turn and must be shown again.
fn aborted_input_replayed(source: &MessageEventSource) -> bool {
    matches!(source, MessageEventSource::Console)
}

/// Whether a mark whose turn ended with `status` covers its note.
fn note_consumed(status: &str, source: &MessageEventSource) -> bool {
    match status {
        "completed" | "silent" => true,
        "failed" | "cancelled" => aborted_input_replayed(source),
        _ => false,
    }
}

/// The seen point: the larger of the highest `floor` stored in the marks read
/// (verified when it was written, trusted without a check) and the highest
/// `through_id` of a mark whose turn ended with a status that counts. The run
/// statuses of all marks above the floor are read in one query. A mark with no
/// run id, no final row or an unreadable status does not count. A failed read
/// is logged by digest and leaves the floor.
async fn seen_point(memory: &dyn Memory, principal: &MemoryPrincipal, seen: &[MessageEvent]) -> Option<i64> {
    let marks: Vec<(i64, Option<i64>, Option<&str>, &MessageEventSource)> = seen
        .iter()
        .filter_map(|row| {
            let payload = payload_of(row)?;
            let through = payload.get("through_id").and_then(Value::as_i64)?;
            let floor = payload.get("floor").and_then(Value::as_i64);
            Some((through, floor, row.run_id.as_deref(), &row.source))
        })
        .collect();
    let floor = marks.iter().filter_map(|mark| mark.1).max();
    let candidates: Vec<(i64, &str, &MessageEventSource)> = marks
        .iter()
        .filter(|mark| floor.is_none_or(|floor| mark.0 > floor))
        .filter_map(|mark| Some((mark.0, mark.2?, mark.3)))
        .collect();
    let mut run_ids: Vec<String> = candidates.iter().map(|mark| mark.1.to_owned()).collect();
    run_ids.sort_unstable();
    run_ids.dedup();
    let Some(owner_id) = principal.owner_id.as_deref() else {
        return floor;
    };
    if run_ids.is_empty() {
        return floor;
    }
    let statuses = match memory
        .turn_final_statuses(&principal.workspace_id, owner_id, &run_ids)
        .await
    {
        Ok(statuses) => statuses,
        Err(error) => {
            let (error_len, error_sha256) = error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                "Failed to check the turns of the scheduled results seen marks"
            );
            return floor;
        }
    };
    let counted = candidates
        .iter()
        .filter(|(_, run_id, source)| {
            statuses
                .get(*run_id)
                .is_some_and(|status| note_consumed(status, source))
        })
        .map(|(through, _, _)| *through)
        .max();
    floor.max(counted)
}

/// Writes the seen mark: one tool-round row in the owner's turn scope, naming
/// the newest result row the note has covered. A failure is logged by digest.
pub(crate) async fn mark_results_seen(fabric: &MemoryFabric, scope: MessageEventScope, mark: SeenMark) {
    let mut payload = serde_json::json!({ "schema_version": 1, "through_id": mark.through_id });
    if let (Some(floor), Some(object)) = (mark.floor, payload.as_object_mut()) {
        object.insert("floor".to_owned(), Value::from(floor));
    }
    if let Err(error) = fabric
        .record_tool_round_event(
            scope,
            SCHEDULE_SEEN_EVENT_TYPE,
            "scheduled results seen",
            Some(payload.to_string()),
        )
        .await
    {
        let (error_len, error_sha256) = error_log_digest(&error);
        tracing::warn!(
            error_len,
            error_sha256,
            "Failed to record the scheduled results seen mark"
        );
    }
}

/// The rows of `event_type` after `after_id`, or `None` on a read error (logged).
async fn read_rows(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    event_type: &str,
    after_id: Option<i64>,
    limit: usize,
) -> Option<Vec<MessageEvent>> {
    match memory
        .newest_owner_events_of_type(principal, event_type, after_id, limit)
        .await
    {
        Ok(rows) => Some(rows),
        Err(error) => {
            let (error_len, error_sha256) = error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                "Failed to read scheduled results for the turn note"
            );
            None
        }
    }
}

fn payload_of(row: &MessageEvent) -> Option<Value> {
    row.raw_payload_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
}

/// One scheduled session: its newest row, the count of older successful rows,
/// and whether an older successful row was not delivered as a message.
struct Group {
    key: String,
    newest: Value,
    earlier: usize,
    undelivered_ok_earlier: bool,
}

impl Group {
    /// Whether the group is already in the session: its newest row was
    /// delivered as a message and no older successful run is still undelivered.
    fn delivered(&self) -> bool {
        self.newest.get("delivered").and_then(Value::as_bool) == Some(true) && !self.undelivered_ok_earlier
    }

    /// `- <finished, to the minute> <name or id> <status>: <excerpt or error class> (transcript <session>)`
    fn line(&self) -> String {
        let payload = &self.newest;
        let field = |name: &str| payload.get(name).and_then(Value::as_str).unwrap_or_default();
        let finished: String = flatten(field("finished_at")).chars().take(16).collect();
        let name = match field("name") {
            "" => flatten(field("id")),
            name => flatten(name),
        };
        let status = flatten(field("status"));
        let detail = if field("status") == "ok" {
            flatten(field("excerpt"))
                .chars()
                .take(NOTE_EXCERPT_CHARS)
                .collect::<String>()
        } else {
            flatten(field("error_class"))
        };
        let earlier = if self.earlier > 0 {
            format!(" (+{} earlier)", self.earlier)
        } else {
            String::new()
        };
        format!(
            "- {finished} {name} {status}: {detail} (transcript {}){earlier}",
            flatten(&self.key)
        )
    }
}

/// Replaces every line break and line or paragraph separator with a space, so
/// a field of a stored row cannot start a new line of the note.
fn flatten(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' | '\u{2028}' | '\u{2029}' | '\u{85}' => ' ',
            other => other,
        })
        .collect()
}

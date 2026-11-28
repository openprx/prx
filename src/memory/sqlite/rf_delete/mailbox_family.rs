//! Mailbox-family selection of an rf delete: the completion notices of erased
//! child runs, the turns they woke, the adopted rows an erased child run sent,
//! and the erase of the mailbox bodies of the adopted rows.

use super::{Child, EventRow, Target, derived_sweep};
use rusqlite::{Connection, params};

/// The completion notices that erased child runs delivered to their parent
/// session through the mailbox: rows of source `prx_mailbox`, type
/// `message.created`, whose sender is the child's session
/// (`memory/sqlite/mailbox/adopt.rs`, `sender = stream.from.session`; the
/// announcing side is `tools/sessions_spawn/mailbox_announce.rs`, which sends
/// from the child's session address). A notice is written after the run's
/// first row. A child session that still holds a row the delete keeps may have
/// announced another run, so only the notices of the erased runs go, found by
/// sender and the fixed notice head that PRX writes around the run id
/// ([`run_notices`]). Every adopted row whose recorded sender run is an erased
/// run of its own session ([`sent_by_erased_runs`]) goes as well, whatever
/// recipient run adoption stamped it with. Read before the tombstone.
pub(super) fn announcement_events(
    tx: &Connection,
    target: &Target<'_>,
    children: &[Child],
) -> anyhow::Result<Vec<String>> {
    let deleted = serde_json::to_string(
        &children
            .iter()
            .flat_map(|child| child.event_ids.iter())
            .collect::<Vec<_>>(),
    )?;
    let mut found: Vec<String> = Vec::new();
    let mut done: Vec<&str> = Vec::new();
    for child in children {
        if done.contains(&child.session_key.as_str()) {
            continue;
        }
        done.push(&child.session_key);
        // The survival rule of `sweep_listings`, with the rows this delete
        // rewrites not counted (the tombstone has not run yet). Rows the delete
        // keeps that are not content do not make a session survive: accounting
        // (`usage.settled`, written per metered run into the run's session) and
        // the session prefix row (`context.prefix.recorded`, a fixed label with
        // no run id, written once per session by `context/transcript/prefix_rows.rs`).
        let survives: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id = ?2 AND session_key = ?1
                           AND event_type NOT IN ('message.deleted', 'turn.deleted', 'usage.settled',
                                                  'context.prefix.recorded')
                           AND event_id NOT IN (SELECT value FROM json_each(?3)))
                 OR EXISTS(SELECT 1 FROM conversation_turns c WHERE c.session_key = ?1 AND c.deleted_at IS NULL
                     AND (c.message_event_id IS NULL OR c.message_event_id NOT IN (SELECT value FROM json_each(?3)))
                     AND (c.ma_scope_id IS NULL OR EXISTS(SELECT 1 FROM agent_scopes a
                         WHERE a.scope_id = c.ma_scope_id AND a.workspace_id = ?2)))",
            params![child.session_key, target.workspace_id, deleted],
            |row| row.get(0),
        )?;
        if survives {
            // A continued child session keeps the notices of its other runs;
            // those of the erased runs are found by the fixed notice head.
            for child in children.iter().filter(|other| other.session_key == child.session_key) {
                found.extend(run_notices(tx, target, child)?);
            }
            continue;
        }
        let first_id = children
            .iter()
            .filter(|other| other.session_key == child.session_key)
            .map(|other| other.first_id)
            .min()
            .unwrap_or(child.first_id);
        let mut statement = tx.prepare(
            "SELECT event_id FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND source = 'prx_mailbox' AND event_type = 'message.created'
               AND sender = ?3 AND id > ?4",
        )?;
        found.extend(
            statement
                .query_map(
                    params![target.workspace_id, target.owner_id, child.session_key, first_id],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    found.extend(sent_by_erased_runs(tx, target, children)?);
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// The adopted rows (source `prx_mailbox`, `message.created`) that an erased
/// run in `children` sent, found by the sender run the sending runtime recorded
/// in the row's payload (`sender_run_id`, written by `mailbox/adopt.rs`) and by
/// the child's session as sender; both must match. The row's text is the erased
/// run's output wherever adoption placed it: it is found whatever run the
/// recipient session stamped it with, before or after a kept run's first row.
/// A row whose recorded run is a kept run of the same child session is not
/// matched, nor is a row of another sender, a remote peer's row or a row
/// written before the field existed (those have no `sender_run_id`). Matched by
/// key only, never by text. Read before the tombstone.
fn sent_by_erased_runs(tx: &Connection, target: &Target<'_>, children: &[Child]) -> anyhow::Result<Vec<String>> {
    let mut statement = tx.prepare(
        "SELECT event_id FROM message_events
         WHERE workspace_id = ?1 AND owner_id = ?2 AND source = 'prx_mailbox' AND event_type = 'message.created'
           AND sender = ?3 AND json_valid(raw_payload_json)
           AND json_extract(raw_payload_json, '$.sender_run_id') = ?4",
    )?;
    let mut found: Vec<String> = Vec::new();
    for child in children {
        found.extend(
            statement
                .query_map(
                    params![target.workspace_id, target.owner_id, child.session_key, child.run_id],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(found)
}

/// The completion notice of one erased run in its (surviving) child session:
/// rows of source `prx_mailbox` sent by the child session after the run's first
/// row whose text, after the `[message from ...]` line the adoption prepends,
/// starts with the fixed notice head of the run id
/// (`messaging::mailbox::completion_notice_head`). A head quoted later in a
/// body does not match. Run ids shorter than `MIN_IDENTIFIER_CHARS` are not matched.
fn run_notices(tx: &Connection, target: &Target<'_>, child: &Child) -> anyhow::Result<Vec<String>> {
    if child.run_id.chars().count() < derived_sweep::MIN_IDENTIFIER_CHARS {
        return Ok(Vec::new());
    }
    let head = crate::messaging::mailbox::completion_notice_head(&child.run_id);
    let mut statement = tx.prepare(
        "SELECT event_id, content FROM message_events
         WHERE workspace_id = ?1 AND owner_id = ?2 AND source = 'prx_mailbox' AND event_type = 'message.created'
           AND sender = ?3 AND id > ?4 AND instr(content, ?5) > 0",
    )?;
    let rows = statement
        .query_map(
            params![
                target.workspace_id,
                target.owner_id,
                child.session_key,
                child.first_id,
                head
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, content)| {
            content
                .split_once('\n')
                .is_some_and(|(_, text)| text.starts_with(head.as_str()))
        })
        .map(|(event_id, _)| event_id)
        .collect())
}

/// The rows of the turns that a deleted completion notice woke: among
/// `notices` (a JSON array of the event ids of the notices of erased runs and
/// of the erased adopted rows an erased spawned run sent),
/// the rows of source `prx_mailbox`; a turn they woke has its input
/// (`message.created`, role `user`) caused by the notice
/// (`causation_event_id`, set from the adopted event by
/// `gateway/api/sessions/mailbox.rs` and `RuntimeEnvelope::message_scope`) and
/// its own run, and every row of that run goes: input, attachment, request
/// and tool rows, reply, decision and terminal. The accounting row stays, as
/// for the deleted run. Rows already among `kept` (every event id this delete
/// removes) are left out. Matched by
/// key only, never by text. Read before the tombstone.
pub(super) fn announcement_turns(
    tx: &Connection,
    target: &Target<'_>,
    notices: &str,
    kept: &str,
) -> anyhow::Result<Vec<EventRow>> {
    let mut statement = tx.prepare(
        "WITH notices(event_id) AS (
             SELECT event_id FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND source = 'prx_mailbox'
               AND event_type = 'message.created' AND event_id IN (SELECT value FROM json_each(?3))),
         woken(run_id) AS (
             SELECT DISTINCT run_id FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND event_type = 'message.created' AND role = 'user'
               AND run_id IS NOT NULL AND causation_event_id IN (SELECT event_id FROM notices))
         SELECT id, event_id, COALESCE(role, ''), COALESCE(content, ''), run_id, session_key, ma_scope_id
         FROM message_events
         WHERE workspace_id = ?1 AND owner_id = ?2 AND run_id IN (SELECT run_id FROM woken)
           AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')
           AND event_id NOT IN (SELECT value FROM json_each(?4))
         ORDER BY id",
    )?;
    Ok(statement
        .query_map(params![target.workspace_id, target.owner_id, notices, kept], |row| {
            Ok(EventRow {
                id: row.get(0)?,
                event_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                run_id: row.get(4)?,
                session_key: row.get(5)?,
                scope: row.get(6)?,
                schedule_result: false,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The event ids of the adopted rows among `rows` (source `prx_mailbox`,
/// `message.created`) that an erased spawned run in `children` sent while it
/// was its session's run: a message such a run sent to another session, such
/// as interim output that is not its completion notice. The turn each one woke
/// reports the erased run's output and is followed like a notice's.
///
/// The adopted row carries the sender's session but not the sender's run
/// (`Common::exec_run` is the recipient's live-run pin), so the run is found
/// by row order (`id`, as the rest of this module orders rows): sender equal to
/// the child's session, after the run's first row, and before the first row of
/// the next run of that session that this delete keeps (no upper end when
/// there is none). A message a kept later run of a continued child session
/// sent is not followed, nor is an adopted row from any other sender; both are
/// still erased when they are in `rows`. Matched by key and order only, never
/// by text.
///
/// The order window is the rule for rows without a recorded sender run (written
/// before `sender_run_id` existed). A row that records its sender run is judged
/// by that run alone ([`sent_by_erased_runs`]): one recorded with a kept run
/// is not followed here even inside the window, and one recorded with an erased
/// run is already among the announcements.
pub(super) fn child_adopted_rows(
    tx: &Connection,
    target: &Target<'_>,
    rows: &[EventRow],
    children: &[Child],
) -> anyhow::Result<Vec<String>> {
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(&rows.iter().map(|row| row.event_id.as_str()).collect::<Vec<_>>())?;
    let erased = serde_json::to_string(&children.iter().map(|child| child.run_id.as_str()).collect::<Vec<_>>())?;
    let mut found: Vec<String> = Vec::new();
    for child in children {
        // The first row of a later run of the child session that the delete
        // keeps: that run, not the erased one, is the session's run from there.
        let next_kept: Option<i64> = tx.query_row(
            "SELECT MIN(id) FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND id > ?4
               AND source <> 'prx_mailbox' AND run_id IS NOT NULL
               AND run_id NOT IN (SELECT value FROM json_each(?5))
               AND event_type NOT IN ('turn.deleted', 'message.deleted')",
            params![
                target.workspace_id,
                target.owner_id,
                child.session_key,
                child.first_id,
                erased
            ],
            |row| row.get(0),
        )?;
        let mut statement = tx.prepare(
            "SELECT event_id FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND source = 'prx_mailbox' AND event_type = 'message.created'
               AND event_id IN (SELECT value FROM json_each(?3)) AND sender = ?4
               AND id > ?5 AND (?6 IS NULL OR id < ?6)
               AND CASE WHEN json_valid(raw_payload_json)
                        THEN json_type(raw_payload_json, '$.sender_run_id') END IS NULL",
        )?;
        found.extend(
            statement
                .query_map(
                    params![
                        target.workspace_id,
                        target.owner_id,
                        ids,
                        child.session_key,
                        child.first_id,
                        next_kept
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// Erase the mailbox bodies of the adopted rows among `event_ids` (a JSON
/// array), in the target's workspace and owner.
pub(super) fn erase_mailbox_bodies(tx: &Connection, target: &Target<'_>, event_ids: &str) -> anyhow::Result<()> {
    let now = chrono::DateTime::parse_from_rfc3339(target.now)
        .map_or_else(|_| chrono::Utc::now().timestamp(), |time| time.timestamp());
    super::super::mailbox::erase_adopted_bodies(tx, target.workspace_id, target.owner_id, event_ids, now)?;
    Ok(())
}

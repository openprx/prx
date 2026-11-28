//! Erase what erased runs sent and no receiver has adopted yet. A pending slot
//! closes as an out-of-order hole above the delivery cursor: `deliver` steps over
//! it only when it reaches it in order, so an earlier kept slot is still delivered
//! first. The pass never calls `terminal_stale_slot` or `nack_slot`.
use super::{
    erase::erase_outbox_body,
    model::{Hole, OutboxMeta, decode, encode},
    receipt, stream,
};
use crate::messaging::mailbox::{MAX_HOLES, MailboxResult};
use rusqlite::{Connection, params};

/// Close `seq` of `stream` as a hole and save the progress. Returns false when the
/// hole table is full after pruning (nothing is saved then). A retired stream, or a
/// slot at or below the consume cursor, needs no hole and returns true.
fn close_pending_slot(tx: &Connection, stream: &mut stream::Stream, seq: u64, now: i64) -> MailboxResult<bool> {
    let progress = &mut stream.progress;
    if stream.retired || seq <= progress.consume_cursor {
        return Ok(true);
    }
    if !progress.seq_skipped.iter().any(|hole| hole.seq == seq) {
        if progress.seq_skipped.len() >= MAX_HOLES {
            // Holes at or below the consume cursor are redundant: `accept` refuses
            // those seqs first and delivery starts above the cursor.
            let consumed = progress.consume_cursor;
            progress.seq_skipped.retain(|hole| hole.seq > consumed);
        }
        if progress.seq_skipped.len() >= MAX_HOLES {
            return Ok(false);
        }
        progress.seq_skipped.push(Hole { seq, skipped_at: now });
        progress.seq_skipped.sort_by_key(|hole| hole.seq);
    }
    while let Some(next) = progress.consume_cursor.checked_add(1) {
        if !progress.seq_skipped.iter().any(|hole| hole.seq == next) {
            break;
        }
        progress.consume_cursor = next;
    }
    progress.deliver_cursor = progress.deliver_cursor.max(progress.consume_cursor);
    if progress.gap_seq == Some(seq) {
        progress.gap_seq = None;
        progress.gap_started_at = None;
    }
    stream::save_progress(tx, stream, now)?;
    Ok(true)
}

/// What one pending-erase pass did. `unreadable` counts rows whose receipt, stream
/// or envelope metadata does not decode, each row once: a receipt or an outbox row
/// with such metadata is kept; an outbox row whose stream does not load is still
/// erased, without reserving its seq.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::memory::sqlite) struct PendingEraseCounts {
    pub receipts: usize,
    pub outboxes: usize,
    pub skipped_full: usize,
    pub unreadable: usize,
}

/// Erase what the runs in `runs_json` (a JSON array of run ids) sent that is not
/// yet adopted: receipts in state 0/1 by `sender_run_id` (receiver body, receipt
/// mark, the sender's outbox body of the same message on the same stream), then the
/// outbox rows by `sender_run_id` whose body is still present. Rows that do not
/// load are counted in `unreadable` (see [`PendingEraseCounts`]); the pass logs them
/// at `warn`.
pub(in crate::memory::sqlite) fn erase_pending_from_runs(
    tx: &Connection,
    workspace_id: &str,
    owner_id: &str,
    runs_json: &str,
    now: i64,
) -> anyhow::Result<PendingEraseCounts> {
    let (mut receipts, mut outboxes, mut full, mut unreadable) = (0usize, 0usize, 0usize, 0usize);
    let mut skipped: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut bad_outboxes = std::collections::HashSet::new();
    let ids = {
        let mut query = tx.prepare(
            "SELECT id FROM prx_mailbox_receipts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND state IN (0, 1)
               AND sender_run_id IN (SELECT value FROM json_each(?3)) ORDER BY stream_id, seq",
        )?;
        query
            .query_map(params![workspace_id, owner_id, runs_json], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in ids {
        let Ok(mut saved) = receipt::load(tx, id) else {
            unreadable += 1;
            continue;
        };
        if saved.meta.body_erased_at.is_some() {
            continue;
        }
        let Ok(mut stream) = stream::load(tx, saved.stream_id) else {
            unreadable += 1;
            continue;
        };
        if !close_pending_slot(tx, &mut stream, saved.seq, now)? {
            full += 1;
            skipped.push((saved.stream_id, saved.seq.to_be_bytes().to_vec()));
            continue;
        }
        tx.execute("DELETE FROM prx_mailbox_messages WHERE id = ?1", [id])?;
        saved.meta.body_erased_at = Some(now);
        saved.meta.updated_at = now;
        tx.execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta = ?1 WHERE id = ?2",
            params![encode(&saved.meta)?, id],
        )?;
        for (outbox_id, json, state) in outbox_rows(
            tx,
            "SELECT id, envelope_meta, state FROM prx_mailbox_outbox
             WHERE workspace_id = ?1 AND owner_id = ?2 AND stream_id = ?3 AND message_id = ?4
               AND body_bytes IS NOT NULL",
            params![
                workspace_id,
                owner_id,
                saved.stream_id,
                saved.message_id.as_bytes().as_slice()
            ],
        )? {
            if decode::<OutboxMeta>(&json).is_ok() {
                erase_outbox_body(tx, outbox_id, &json, state, now)?;
            } else {
                bad_outboxes.insert(outbox_id);
            }
        }
        receipts += 1;
    }
    let rows = {
        let mut query = tx.prepare(
            "SELECT id, envelope_meta, state, stream_id, seq FROM prx_mailbox_outbox
             WHERE workspace_id = ?1 AND owner_id = ?2 AND body_bytes IS NOT NULL
               AND sender_run_id IN (SELECT value FROM json_each(?3)) ORDER BY stream_id, seq",
        )?;
        query
            .query_map(params![workspace_id, owner_id, runs_json], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, json, state, stream_id, seq) in rows {
        if skipped.iter().any(|(s, q)| *s == stream_id && *q == seq) {
            continue;
        }
        if decode::<OutboxMeta>(&json).is_err() {
            bad_outboxes.insert(id);
            continue;
        }
        if state < 2 {
            let covered: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM prx_mailbox_receipts WHERE stream_id = ?1 AND seq = ?2)",
                params![stream_id, seq],
                |row| row.get(0),
            )?;
            // A local stream with no receipt yet (crash between publish and
            // accept): reserve the seq so a retry cannot deliver it later. A
            // remote stream has no local receiver, so its seq is not reserved.
            if !covered {
                match stream::load(tx, stream_id) {
                    Ok(mut stream) if stream.to.instance == stream.from.instance => {
                        let number = super::model::sequence(&seq)?;
                        if !close_pending_slot(tx, &mut stream, number, now)? {
                            full += 1;
                            continue;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => unreadable += 1,
                }
            }
        }
        erase_outbox_body(tx, id, &json, state, now)?;
        outboxes += 1;
    }
    unreadable += bad_outboxes.len();
    if receipts + outboxes > 0 {
        tracing::info!(receipts, outboxes, "pending mailbox messages of erased runs erased");
    }
    if unreadable > 0 {
        tracing::warn!(unreadable, "pending mailbox rows kept: metadata does not decode");
    }
    if full > 0 {
        tracing::warn!(
            skipped_full = full,
            "pending mailbox messages kept: stream hole table full"
        );
    }
    Ok(PendingEraseCounts {
        receipts,
        outboxes,
        skipped_full: full,
        unreadable,
    })
}

fn outbox_rows(tx: &Connection, sql: &str, params: impl rusqlite::Params) -> anyhow::Result<Vec<(i64, String, i64)>> {
    let mut query = tx.prepare(sql)?;
    Ok(query
        .query_map(params, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

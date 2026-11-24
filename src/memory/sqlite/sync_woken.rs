//! Startup cancellation of approvals left open by a message-woken run that
//! died with its process. A woken run is never replayed, so nothing can
//! decide such an approval and the run can never end it.
use super::mailbox_continuation::CONTINUATION_PREDICATE_E;
use super::sync_journal::{append, event_header};
use crate::security::approval_ledger::keys::RunCallAuthority;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::collections::BTreeSet;

/// Close every open approval whose root is a woken run without a terminal
/// row. Returns the number of runs swept. Safe to repeat: a closed approval is
/// skipped and the interrupt header is keyed by run.
pub(super) fn sweep(conn: &Connection) -> anyhow::Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let candidates = {
        let mut query = tx.prepare(&format!(
            "SELECT p.event_id,p.workspace_id,p.owner_id,
                CASE WHEN json_valid(p.raw_payload_json) THEN json_extract(p.raw_payload_json,'$.record.rf.root_run_id') END AS root,
                CASE WHEN json_valid(p.raw_payload_json) THEN json_extract(p.raw_payload_json,'$.record.authority') END AS authority
             FROM message_events p
             WHERE p.event_type='approval.call.pending' AND root IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM message_events f WHERE f.workspace_id=p.workspace_id AND f.owner_id=p.owner_id
                   AND f.idempotency_key IN('turn:'||root||':final','turn:'||root||':admission-failed'))
               AND EXISTS(SELECT 1 FROM message_events e WHERE e.workspace_id=p.workspace_id AND e.owner_id=p.owner_id
                   AND e.run_id=root AND e.parent_run_id IS NULL AND {CONTINUATION_PREDICATE_E})
             ORDER BY p.id"
        ))?;
        query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    // One entry per root run: (workspace, owner, root run), the first open pending row, its open count.
    let mut runs: Vec<((String, String, String), String, usize)> = Vec::new();
    let mut seen = BTreeSet::new();
    for (event, workspace, owner, root, authority) in candidates {
        let Some(authority) = authority
            .and_then(|raw| serde_json::from_str::<RunCallAuthority>(&raw).ok())
        else {
            continue;
        };
        let decided: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key=?3)",
            params![workspace, owner, format!("rf-decision:{}", authority.call_id())],
            |r| r.get(0),
        )?;
        if decided || !seen.insert(authority.call_id()) {
            continue;
        }
        let key = (workspace, owner, root);
        if let Some(entry) = runs.iter_mut().find(|entry| entry.0 == key) {
            entry.2 += 1;
        } else {
            runs.push((key, event, 1));
        }
    }
    let mut swept = 0;
    for ((workspace, _, run), event, open) in runs {
        let Some(mut header) = event_header(&tx, &event, "turn.failed")? else {
            continue;
        };
        header.source_ref = format!("interrupt:{run}");
        header.event_id = format!("interrupt:{run}");
        append(&tx, &header)?;
        super::sync_lifecycle::cancel_approvals(&tx, &header, &workspace, &run, true)?;
        tracing::info!(
            open_approvals = open,
            "startup sweep cancelled a leftover approval of an interrupted message-woken turn"
        );
        swept += 1;
    }
    tx.commit()?;
    Ok(swept)
}

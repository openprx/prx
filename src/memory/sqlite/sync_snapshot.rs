//! Fixed, content-free inventory cuts; mutable erased sources project tombstones.
use super::{
    SqliteMemory,
    sync_journal::{COLUMNS, Header},
};
use crate::memory::sync::{SnapshotPage, SyncError};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_snapshots(
        id TEXT PRIMARY KEY,owner_id TEXT NOT NULL,epoch TEXT NOT NULL,head INTEGER NOT NULL,
        high_water INTEGER NOT NULL,expires INTEGER NOT NULL) STRICT;
        CREATE TABLE IF NOT EXISTS sync_snapshot_rows(
        snapshot_id TEXT NOT NULL REFERENCES sync_snapshots(id) ON DELETE CASCADE,
        ordinal INTEGER NOT NULL,kind TEXT NOT NULL,key TEXT NOT NULL,header TEXT NOT NULL,
        PRIMARY KEY(snapshot_id,ordinal)) STRICT;",
    )?;
    Ok(())
}
fn save(conn: &Connection, id: &str, ordinal: &mut i64, kind: &str, key: &str, h: &Header) -> anyhow::Result<()> {
    *ordinal = ordinal
        .checked_add(1)
        .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
    conn.execute(
        "INSERT INTO sync_snapshot_rows VALUES(?1,?2,?3,?4,?5)",
        params![id, *ordinal, kind, key, serde_json::to_string(h)?],
    )?;
    Ok(())
}

fn inventory(conn: &Connection, id: &str, owner: &str, head: i64, water: i64) -> anyhow::Result<()> {
    let mut ordinal = 0;
    // Sessions are independent of whether a message has been admitted yet.
    let incarnation:String=conn.query_row("SELECT a.incarnation FROM agent_incarnations a JOIN sync_meta m ON m.workspace=a.workspace_id WHERE a.owner_id=?1 AND a.agent_key='m' AND a.state='active'",[owner],|r|r.get(0))?;
    let mut q =
        conn.prepare("SELECT session_key FROM sessions s WHERE owner_id=?1 AND channel='rf' AND NOT EXISTS(SELECT 1 FROM sync_journal j WHERE j.owner_id=s.owner_id AND j.session=s.session_key AND j.kind='session.upsert' AND j.agent!='main' AND j.parent_run_id IS NULL AND (j.session LIKE 'cron:%' OR j.session LIKE 'xin:%')) ORDER BY session_key")?;
    for session in q.query_map([owner], |r| r.get::<_, String>(0))? {
        let session = session?;
        save(
            conn,
            id,
            &mut ordinal,
            "session",
            &session,
            &Header::new(
                owner,
                "session.upsert",
                &session,
                "main",
                &incarnation,
                format!("session:{session}"),
            ),
        )?;
    }
    let mut q=conn.prepare(&format!("SELECT {COLUMNS} FROM sync_journal j WHERE owner_id=?1 AND kind='session.upsert' AND (parent_run_id IS NOT NULL OR session LIKE 'cron:%' OR session LIKE 'xin:%') AND seq<=?2 AND seq=(SELECT MAX(k.seq) FROM sync_journal k WHERE k.owner_id=j.owner_id AND k.session=j.session AND k.kind=j.kind AND k.seq<=?2) ORDER BY session"))?;
    for h in q.query_map(params![owner, head], Header::row)? {
        let h = h?;
        save(conn, id, &mut ordinal, "session", &h.session, &h)?;
    }
    let mut q=conn.prepare("SELECT s.agent,s.incarnation,(SELECT c.id FROM sync_config_sources c WHERE c.owner_id=s.owner_id AND c.agent=s.agent ORDER BY rowid DESC LIMIT 1) FROM sync_agent_state s WHERE owner_id=?1 ORDER BY agent")?;
    for row in q.query_map([owner], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })? {
        let (agent, inc, source) = row?;
        save(
            conn,
            id,
            &mut ordinal,
            "agent",
            &agent,
            &Header::new(
                owner,
                "agent.config_changed",
                "",
                &agent,
                &inc,
                format!("config:{source}"),
            ),
        )?;
    }
    let mut q=conn.prepare("SELECT e.event_id FROM message_events e WHERE e.owner_id=?1 AND e.parent_run_id IS NOT NULL AND e.id<=?2 AND COALESCE(e.run_id,'') NOT LIKE 'child-segment:%' AND COALESCE(e.run_id,'') NOT LIKE 'operator-worker-segment:%' AND e.id=(SELECT x.id FROM message_events x WHERE x.owner_id=e.owner_id AND x.session_key=e.session_key AND x.parent_run_id IS NOT NULL AND x.id<=?2 AND COALESCE(x.run_id,'') NOT LIKE 'child-segment:%' AND COALESCE(x.run_id,'') NOT LIKE 'operator-worker-segment:%' ORDER BY (x.event_type IN('turn.finalized','turn.admission.failed')) DESC,x.id DESC LIMIT 1) ORDER BY e.session_key")?;
    for source in q.query_map(params![owner, water], |r| r.get::<_, String>(0))? {
        let source = source?;
        if let Some(mut h) = super::sync_journal::event_header(conn, &source, "child_session.upsert")? {
            let latest:Option<(String,String)>=conn.query_row("SELECT event_id,created_at FROM memory_events WHERE owner_id=?1 AND run_id=?2 AND event_type IN('task.completed','task.failed','task.cancelled') ORDER BY id DESC LIMIT 1",params![owner,h.run_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((event, time)) = latest {
                h.source_ref = format!("memory:{event}");
                h.timestamp = time;
            }
            save(conn, id, &mut ordinal, "child_session", &h.session, &h)?;
        }
    }
    let mut q = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM sync_journal j WHERE owner_id=?1 AND kind='approval.pending' AND seq<=?2 AND NOT EXISTS(SELECT 1 FROM sync_journal d WHERE d.owner_id=j.owner_id AND d.kind='approval.decided' AND d.seq<=?2 AND d.event_id=replace(j.event_id,'approval-pending:','approval-decided:')) ORDER BY seq"
    ))?;
    for h in q.query_map(params![owner, head], Header::row)? {
        let h = h?;
        save(conn, id, &mut ordinal, "approval", &h.event_id, &h)?;
    }
    // Receipts outlive journal retention and must survive a fresh snapshot.
    let mut q=conn.prepare("SELECT w.erase_id,json_extract(w.work,'$.session'),json_extract(w.work,'$.agent'),json_extract(w.work,'$.incarnation') FROM agent_erase_work w JOIN agent_erase_receipts r ON r.erase_id=w.erase_id WHERE r.owner_id=?1 AND w.receipt IS NOT NULL AND json_extract(w.work,'$.purge')=0 ORDER BY w.erase_id")?;
    for row in q.query_map([owner], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (receipt, session, agent, inc) = row?;
        let h = Header::new(
            owner,
            "history.erased",
            &session,
            &agent,
            &inc,
            format!("erase:{receipt}"),
        );
        save(conn, id, &mut ordinal, "erase_receipt", &receipt, &h)?;
    }
    let mut q=conn.prepare("SELECT p.purge_id,substr(p.agent_key,3),i.value FROM agent_purge_operations p,json_each(p.receipt,'$.incarnations') i WHERE p.owner_id=?1 AND p.receipt IS NOT NULL ORDER BY p.purge_id,i.value")?;
    for row in q.query_map([owner], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })? {
        let (receipt, agent, inc) = row?;
        let h = Header::new(owner, "history.erased", "", &agent, &inc, format!("purge:{receipt}"));
        save(conn, id, &mut ordinal, "erase_receipt", &format!("{receipt}:{inc}"), &h)?;
    }
    // Admission/terminal commits precede their transcript projections. The
    // cut must inventory the primary events, or it could advance past an
    // event that is absent from both snapshot and subsequent replay.
    let continuation_flag = format!(
        "CASE WHEN {} OR {} THEN 1 ELSE 0 END",
        super::mailbox_continuation::CONTINUATION_PREDICATE_E,
        super::mailbox_continuation::TASK_ECHO_PREDICATE_E
    );
    let mut q=conn.prepare(&format!("WITH candidates AS (
        SELECT e.id,e.event_id,e.workspace_id,e.owner_id,e.run_id,e.role,e.event_type,e.idempotency_key,
            {continuation_flag} AS continuation,
            CASE WHEN e.role='assistant' OR e.idempotency_key LIKE 'turn:%:assistant'
            THEN (SELECT f.event_id FROM message_events f WHERE f.owner_id=e.owner_id AND f.workspace_id=e.workspace_id
                AND f.run_id=e.run_id AND f.id<=?2 AND (
                    (f.event_type='turn.finalized' AND json_extract(f.raw_payload_json,'$.assistant_event_id')=e.event_id)
                    OR (e.idempotency_key='turn:'||e.run_id||':assistant' AND f.idempotency_key='turn:'||e.run_id||':final'))
                ORDER BY f.id LIMIT 1) END AS final_marker
        FROM message_events e WHERE e.owner_id=?1 AND e.id<=?2)
        SELECT e.event_id,e.final_marker FROM candidates e WHERE
            (e.role='user' AND e.continuation=0)
            OR (e.event_type='message.deleted' AND (
                EXISTS(SELECT 1 FROM rf_message_attempts a WHERE a.owner_id=e.owner_id AND a.message_event_id=e.event_id)
                OR EXISTS(SELECT 1 FROM conversation_turns c WHERE c.message_event_id=e.event_id AND c.role='user')
                OR EXISTS(SELECT 1 FROM sync_journal j WHERE j.owner_id=e.owner_id AND j.kind='turn.appended' AND j.source_ref='event:'||e.event_id AND j.seq<=?3)))
            OR e.final_marker IS NOT NULL
        ORDER BY e.id"))?;
    for row in q.query_map(params![owner, water, head], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })? {
        let (event, marker) = row?;
        if let Some(mut h) = super::sync_journal::event_header(conn, &event, "turn.appended")? {
            if let Some(marker) = marker {
                h.source_ref = format!("final:{marker}:{event}");
            }
            save(conn, id, &mut ordinal, "turn", &event, &h)?;
        }
    }
    Ok(())
}

pub(super) fn page(
    memory: &SqliteMemory,
    owner: &str,
    token: Option<&str>,
    limit: usize,
) -> anyhow::Result<SnapshotPage> {
    if !(1..=500).contains(&limit) {
        return Err(SyncError::Invalid.into());
    }
    let conn = memory.pool.write();
    let tx: rusqlite::Transaction<'_> = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
    let now = chrono::Utc::now().timestamp();
    tx.execute(
        "DELETE FROM sync_snapshot_rows WHERE snapshot_id IN(SELECT id FROM sync_snapshots WHERE expires<?1)",
        [now],
    )?;
    tx.execute("DELETE FROM sync_snapshots WHERE expires<?1", [now])?;
    let (id, offset) = if let Some(token) = token {
        let (id, offset) = token.split_once(':').ok_or(SyncError::Invalid)?;
        let offset = offset.parse::<i64>().map_err(|_| SyncError::Invalid)?;
        if offset < 0 || id.len() != 32 {
            return Err(SyncError::Invalid.into());
        }
        (id.to_owned(), offset)
    } else {
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM sync_snapshots WHERE owner_id=?1", [owner], |r| {
            r.get(0)
        })?;
        if count >= 2 {
            return Err(SyncError::RateLimited.into());
        }
        let (epoch,head,water):(String,i64,i64)=tx.query_row("SELECT epoch,MAX(floor,COALESCE((SELECT MAX(seq) FROM sync_journal),0)),COALESCE((SELECT MAX(id) FROM message_events),0) FROM sync_meta",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        tx.execute(
            "INSERT INTO sync_snapshots VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, owner, epoch, head, water, now + 600],
        )?;
        inventory(&tx, &id, owner, head, water)?;
        (id, 0)
    };
    let (epoch, head): (String, i64) = tx
        .query_row(
            "SELECT epoch,head FROM sync_snapshots WHERE id=?1 AND owner_id=?2 AND expires>=?3",
            params![id, owner, now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or(SyncError::SnapshotExpired)?;
    tx.execute(
        "UPDATE sync_snapshots SET expires=?2 WHERE id=?1",
        params![id, now + 600],
    )?;
    let mut q=tx.prepare("SELECT ordinal,kind,key,header FROM sync_snapshot_rows WHERE snapshot_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3")?;
    let mut rows = Vec::new();
    let mut last = offset;
    for row in q.query_map(params![id, offset, i64::try_from(limit)?], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (index, kind, key, header) = row?;
        let h: Header = serde_json::from_str(&header)?;
        let event = super::sync_projection::event(&tx, &h)?;
        rows.push(json!({"kind":kind,"key":key,"payload":event.get("payload")}));
        last = index;
    }
    drop(q);
    let more: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_snapshot_rows WHERE snapshot_id=?1 AND ordinal>?2)",
        params![id, last],
        |r| r.get(0),
    )?;
    let next = more.then(|| format!("{id}:{last}"));
    if !more {
        // The final page owns its projected rows; no outstanding page needs
        // this cut or its owner slot after this transaction commits.
        tx.execute("DELETE FROM sync_snapshot_rows WHERE snapshot_id=?1", [&id])?;
        tx.execute("DELETE FROM sync_snapshots WHERE id=?1", [&id])?;
    }
    tx.commit()?;
    Ok(SnapshotPage {
        cursor: format!("{epoch}:{head}"),
        epoch,
        snapshot_id: id,
        rows,
        next,
    })
}

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
        .ok_or_else(|| anyhow::anyhow!("snapshot ordinal exhausted"))?;
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
        conn.prepare("SELECT session_key FROM sessions WHERE owner_id=?1 AND channel='rf' ORDER BY session_key")?;
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
    for (kind, event_kind) in [
        ("child_session", "child_session.upsert"),
        ("erase_receipt", "history.erased"),
    ] {
        let mut q=conn.prepare(&format!("SELECT {COLUMNS} FROM sync_journal j WHERE owner_id=?1 AND kind=?2 AND seq<=?3 AND seq=(SELECT MAX(k.seq) FROM sync_journal k WHERE k.owner_id=j.owner_id AND k.kind=j.kind AND k.session=j.session AND k.agent=j.agent AND k.incarnation=j.incarnation AND k.seq<=?3) ORDER BY seq"))?;
        for h in q.query_map(params![owner, event_kind, head], Header::row)? {
            let h = h?;
            save(conn, id, &mut ordinal, kind, &h.event_id, &h)?;
        }
    }
    let mut q=conn.prepare("SELECT e.event_id,COALESCE(c.message_id,e.event_id) FROM message_events e JOIN conversation_turns c ON c.message_event_id=e.event_id WHERE e.owner_id=?1 AND e.id<=?2 AND c.role IN('user','assistant') ORDER BY e.id")?;
    for row in q.query_map(params![owner, water], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (event, message) = row?;
        if let Some(h) = super::sync_journal::event_header(conn, &event, "turn.appended")? {
            save(conn, id, &mut ordinal, "turn", &message, &h)?;
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
    let tx = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
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
    tx.commit()?;
    Ok(SnapshotPage {
        cursor: format!("{epoch}:{head}"),
        epoch,
        snapshot_id: id,
        rows,
        next,
    })
}

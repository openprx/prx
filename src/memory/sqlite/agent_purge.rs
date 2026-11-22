//! Named-agent purge reuses the history adapters across every incarnation.
//! Permanent receipts outlive the progress cursors and idempotency bindings.
use super::SqliteMemory;
use crate::memory::agent_erasure::{AgentPurge, ErasePrepared, EraseWork, ErasureRefusal};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_purge_operations(
        purge_id TEXT PRIMARY KEY,workspace_id TEXT NOT NULL,owner_id TEXT NOT NULL,
        agent_key TEXT NOT NULL,receipt TEXT
    ) STRICT;
    CREATE UNIQUE INDEX IF NOT EXISTS agent_purge_one_pending ON agent_purge_operations(workspace_id,owner_id,agent_key) WHERE receipt IS NULL;
    CREATE TABLE IF NOT EXISTS agent_purge_keys(
        workspace_id TEXT NOT NULL,owner_id TEXT NOT NULL,idempotency_key TEXT NOT NULL,
        agent_key TEXT NOT NULL,purge_id TEXT NOT NULL REFERENCES agent_purge_operations(purge_id),
        created_at INTEGER NOT NULL,PRIMARY KEY(workspace_id,owner_id,idempotency_key)
    ) STRICT;")?;
    Ok(())
}

pub(super) fn begin(memory: &SqliteMemory, request: &AgentPurge) -> anyhow::Result<ErasePrepared> {
    let writer = memory.pool.write();
    let tx = begin_immediate(&writer)?;
    let key = format!("n:{}", request.agent);
    let history_key: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_erase_keys
        WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key=?3)",
        params![request.workspace, request.owner, request.key],
        |row| row.get(0),
    )?;
    if history_key {
        return Err(ErasureRefusal::IdempotencyConflict.into());
    }
    let bound: Option<(String, String, Option<String>)> = tx
        .query_row(
            "SELECT k.agent_key,k.purge_id,o.receipt
        FROM agent_purge_keys k JOIN agent_purge_operations o ON o.purge_id=k.purge_id
        WHERE k.workspace_id=?1 AND k.owner_id=?2 AND k.idempotency_key=?3",
            params![request.workspace, request.owner, request.key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if bound.as_ref().is_some_and(|(name, _, _)| name != &key) {
        return Err(ErasureRefusal::IdempotencyConflict.into());
    }
    if let Some((_, _, Some(receipt))) = &bound {
        return Ok(ErasePrepared::Replay(serde_json::from_str(receipt)?));
    }
    let incarnations = {
        let mut query=tx.prepare("SELECT incarnation,state FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 ORDER BY incarnation")?;
        query
            .query_map(params![request.workspace, request.owner, key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if incarnations.is_empty() {
        return Err(ErasureRefusal::Unknown.into());
    }
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT purge_id,receipt FROM agent_purge_operations
        WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND receipt IS NOT NULL ORDER BY rowid DESC LIMIT 1",
            params![request.workspace, request.owner, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if incarnations.iter().all(|(_, state)| state == "retired") {
        if let Some((purge_id, receipt)) = previous {
            bind_key(&tx, request, &key, &purge_id)?;
            tx.commit()?;
            return Ok(ErasePrepared::Replay(serde_json::from_str(&receipt)?));
        }
    }
    let pending:Option<String>=tx.query_row("SELECT purge_id FROM agent_purge_operations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND receipt IS NULL",
        params![request.workspace,request.owner,key],|row|row.get(0)).optional()?;
    if let Some(purge_id) = pending {
        bind_key(&tx, request, &key, &purge_id)?;
        let work: String = tx.query_row(
            "SELECT work FROM agent_erase_work WHERE erase_id=?1",
            [&purge_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        return Ok(ErasePrepared::Work(serde_json::from_str(&work)?));
    }
    if request.expected_revision != request.current_revision {
        return Err(ErasureRefusal::RevisionConflict(request.current_revision.clone()).into());
    }
    if incarnations.iter().any(|(_, state)| state == "active") {
        return Err(ErasureRefusal::NotRetired.into());
    }
    let purge_id = uuid::Uuid::new_v4().to_string();
    // Materialize an empty private scope for incarnations that never ran.
    for (incarnation, _) in &incarnations {
        let has_scope:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND incarnation=?4)",
            params![request.workspace,request.owner,key,incarnation],|row|row.get(0))?;
        if !has_scope {
            tx.execute(
                "INSERT INTO agent_scopes VALUES(?1,?2,?3,NULL,?4,?5,1)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    request.workspace,
                    request.owner,
                    key,
                    incarnation
                ],
            )?;
        }
        tx.execute("INSERT INTO agent_purge_progress(workspace_id,owner_id,agent_key,incarnation,purge_id,state) VALUES(?1,?2,?3,?4,?5,'draining')
            ON CONFLICT(workspace_id,owner_id,agent_key,incarnation) DO UPDATE SET purge_id=excluded.purge_id,state='draining',after_scope_id=NULL,current_erase_id=NULL",
            params![request.workspace,request.owner,key,incarnation,purge_id])?;
    }
    let scopes = {
        let mut query = tx.prepare(
            "SELECT scope_id,incarnation,generation,COALESCE(session_key,'') FROM agent_scopes
            WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 ORDER BY scope_id",
        )?;
        query
            .query_map(params![request.workspace, request.owner, key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let first = scopes.first().ok_or(ErasureRefusal::Pending)?;
    let scope_ids: Vec<String> = scopes.iter().map(|(id, _, _, _)| id.clone()).collect();
    let scope_json = serde_json::to_string(&scope_ids)?;
    let runs = {
        let mut query=tx.prepare("SELECT DISTINCT run_id FROM message_events WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND run_id IS NOT NULL ORDER BY run_id")?;
        query
            .query_map([&scope_json], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let jobs = {
        let mut query=tx.prepare("SELECT DISTINCT job_id FROM rf_message_attempts WHERE ma_scope_id IN(SELECT value FROM json_each(?1)) AND job_id IS NOT NULL ORDER BY job_id")?;
        query
            .query_map([&scope_json], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let work = EraseWork {
        erase_id: purge_id.clone(),
        scope_id: first.0.clone(),
        workspace: request.workspace.clone(),
        owner: request.owner.clone(),
        session: first.3.clone(),
        agent: request.agent.clone(),
        incarnation: first.1.clone(),
        generation: first.2,
        forget_writes: false,
        purge: true,
        scopes: scope_ids,
        runs,
        jobs,
    };
    for (scope, _, generation, _) in &scopes {
        let next = generation.checked_add(1).ok_or(ErasureRefusal::Pending)?;
        tx.execute(
            "INSERT OR IGNORE INTO agent_erase_fences VALUES(?1,?2,'draining',?3)",
            params![scope, purge_id, next],
        )?;
    }
    tx.execute(
        "INSERT INTO agent_erase_receipts(workspace_id,owner_id,erase_id,scope_id,incarnation,generation,state)
        VALUES(?1,?2,?3,?4,?5,?6,'pending')",
        params![request.workspace, request.owner, purge_id, first.0, first.1, first.2],
    )?;
    tx.execute(
        "INSERT INTO agent_erase_work(erase_id,work) VALUES(?1,?2)",
        params![purge_id, serde_json::to_string(&work)?],
    )?;
    tx.execute(
        "INSERT INTO agent_purge_operations VALUES(?1,?2,?3,?4,NULL)",
        params![purge_id, request.workspace, request.owner, key],
    )?;
    tx.execute(
        "UPDATE agent_purge_progress SET current_erase_id=?4 WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3",
        params![request.workspace, request.owner, key, purge_id],
    )?;
    bind_key(&tx, request, &key, &purge_id)?;
    tx.commit()?;
    Ok(ErasePrepared::Work(Box::new(work)))
}

fn bind_key(conn: &Connection, request: &AgentPurge, agent: &str, purge_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO agent_purge_keys VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            request.workspace,
            request.owner,
            request.key,
            agent,
            purge_id,
            chrono::Utc::now().timestamp()
        ],
    )?;
    Ok(())
}

/// Called after the shared history adapter and the config transaction finish.
pub(super) fn finish(memory: &SqliteMemory, work: &EraseWork) -> anyhow::Result<serde_json::Value> {
    let external_counts = crate::cron::agent_jobs::purge(work)?;
    let writer = memory.pool.write();
    let tx = begin_immediate(&writer)?;
    let existing: Option<String> = tx.query_row(
        "SELECT receipt FROM agent_purge_operations WHERE purge_id=?1",
        [&work.erase_id],
        |row| row.get(0),
    )?;
    if let Some(receipt) = existing {
        return Ok(serde_json::from_str(&receipt)?);
    }
    let (counts, erased): (String, bool) = tx.query_row(
        "SELECT counts,receipt IS NOT NULL FROM agent_erase_work WHERE erase_id=?1",
        [&work.erase_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if !erased {
        return Err(ErasureRefusal::Pending.into());
    }
    let key = format!("n:{}", work.agent);
    let incarnations = {
        let mut query=tx.prepare("SELECT incarnation FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 ORDER BY incarnation")?;
        query
            .query_map(params![work.workspace, work.owner, key], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut counts = serde_json::from_str::<std::collections::BTreeMap<String, u64>>(&counts)?;
    counts.extend(external_counts);
    let receipt = serde_json::json!({"name":work.agent,"incarnations":incarnations,"counts":counts,
        "completed_at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs,true)});
    tx.execute("UPDATE agent_incarnations SET state='retired' WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND state='retiring'",params![work.workspace,work.owner,key])?;
    tx.execute("UPDATE agent_purge_progress SET state='erased',after_scope_id=(SELECT MAX(a.scope_id) FROM agent_scopes a
        WHERE a.workspace_id=agent_purge_progress.workspace_id AND a.owner_id=agent_purge_progress.owner_id
        AND a.agent_key=agent_purge_progress.agent_key AND a.incarnation=agent_purge_progress.incarnation),current_erase_id=NULL
        WHERE purge_id=?1",[&work.erase_id])?;
    tx.execute(
        "UPDATE agent_purge_operations SET receipt=?2 WHERE purge_id=?1",
        params![work.erase_id, serde_json::to_string(&receipt)?],
    )?;
    for incarnation in &incarnations {
        let mut header = super::sync_journal::Header::new(
            &work.owner,
            "history.erased",
            "",
            &work.agent,
            incarnation,
            format!("purge:{}", work.erase_id),
        );
        header.event_id = format!("purge:{}:{incarnation}", work.erase_id);
        super::sync_journal::append(&tx, &header)?;
    }
    tx.commit()?;
    Ok(receipt)
}

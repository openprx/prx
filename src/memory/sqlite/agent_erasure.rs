//! Scope-fenced erase orchestration. Payload removal and progress commit together;
//! terminal receipts follow independent coverage checks and WAL truncation.
use super::{SqliteMemory, agent_registry::MANIFEST_STORES};
use crate::memory::agent_erasure::{ErasePrepared, EraseWork, ErasureRefusal, HistoryErase};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::time::Duration;

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_erase_work (
            erase_id TEXT PRIMARY KEY REFERENCES agent_erase_receipts(erase_id),
            work TEXT NOT NULL, main_erased INTEGER NOT NULL DEFAULT 0,
            counts TEXT NOT NULL DEFAULT '{}', forgotten TEXT, receipt TEXT
        ) STRICT;
        CREATE TABLE IF NOT EXISTS agent_erase_side_applied(erase_id TEXT NOT NULL,store_path TEXT NOT NULL,PRIMARY KEY(erase_id,store_path)) STRICT;",
    )?;
    Ok(())
}

/// Derive stamped tables from SQLite, including alternate mailbox/log stamps.
/// Control tables keep their fences/receipts and do not carry payload stamps.
pub(super) fn stamped_tables(conn: &Connection) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::new();
    let mut query = conn.prepare(
        "SELECT s.name,p.name FROM sqlite_schema s JOIN pragma_table_info(s.name) p
        WHERE s.type='table' AND (p.name IN ('ma_scope_id','sender_ma_scope_id','recipient_ma_scope_id')
        OR (s.name='memory_write_log' AND p.name='agent_partition')) ORDER BY s.name,p.name",
    )?;
    for row in query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
        let (table, column) = row?;
        result.entry(table).or_insert_with(Vec::new).push(column);
    }
    Ok(result)
}

pub(super) fn check_manifest(conn: &Connection) -> anyhow::Result<()> {
    for table in stamped_tables(conn)?.keys() {
        if !MANIFEST_STORES.contains(&table.as_str()) {
            return Err(ErasureRefusal::UnknownStore(table.clone()).into());
        }
    }
    Ok(())
}

fn agent_key(name: &str) -> String {
    if name == "main" {
        "m".into()
    } else {
        format!("n:{name}")
    }
}

fn request_hash(request: &HistoryErase) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        "DELETE",
        &request.workspace,
        &request.owner,
        &request.session,
        &request.agent,
        &request.incarnation,
        request.forget_writes,
    ))?)))
}

pub(super) fn begin_history(memory: &SqliteMemory, request: &HistoryErase) -> anyhow::Result<ErasePrepared> {
    let conn = memory.pool.write();
    let tx = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
    let hash = request_hash(request)?;
    let saved: Option<(String, String, Option<String>)> = tx
        .query_row(
            "SELECT k.request_hash,w.work,w.receipt FROM agent_erase_keys k
         JOIN agent_erase_work w ON w.erase_id=k.erase_id
         WHERE k.workspace_id=?1 AND k.owner_id=?2 AND k.idempotency_key=?3",
            params![request.workspace, request.owner, request.key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((saved_hash, work, receipt)) = saved {
        if saved_hash != hash {
            return Err(ErasureRefusal::IdempotencyConflict.into());
        }
        return Ok(match receipt {
            Some(receipt) => ErasePrepared::Replay(serde_json::from_str(&receipt)?),
            None => ErasePrepared::Work(serde_json::from_str(&work)?),
        });
    }
    if !request.configured {
        return Err(ErasureRefusal::Unknown.into());
    }
    let key = agent_key(&request.agent);
    let live: Option<(String, String)> = tx
        .query_row(
            "SELECT incarnation,state FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2
         AND agent_key=?3 ORDER BY state='active' DESC,state='retiring' DESC LIMIT 1",
            params![request.workspace, request.owner, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (incarnation, _) = live.ok_or(ErasureRefusal::Unknown)?;
    if incarnation != request.incarnation {
        return Err(ErasureRefusal::IncarnationMismatch(incarnation).into());
    }
    let scope: Option<(String, i64)> = tx
        .query_row(
            "SELECT scope_id,generation FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2
         AND session_key=?3 AND agent_key=?4 AND incarnation=?5 ORDER BY generation DESC LIMIT 1",
            params![request.workspace, request.owner, request.session, key, incarnation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (scope_id, generation) = if let Some(scope) = scope {
        scope
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO agent_scopes VALUES(?1,?2,?3,?4,?5,?6,1)",
            params![id, request.workspace, request.owner, request.session, key, incarnation],
        )?;
        (id, 1)
    };
    let fenced: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_erase_fences WHERE scope_id=?1)",
        [&scope_id],
        |row| row.get(0),
    )?;
    if fenced {
        return Err(ErasureRefusal::Pending.into());
    }
    let erase_id = uuid::Uuid::new_v4().to_string();
    let runs = {
        let mut query = tx.prepare(
            "WITH RECURSIVE family(run) AS (
            SELECT run_id FROM message_events WHERE ma_scope_id=?1 AND run_id IS NOT NULL
            UNION SELECT e.run_id FROM message_events e JOIN family f ON e.parent_run_id=f.run
            WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.run_id IS NOT NULL)
            SELECT DISTINCT run FROM family ORDER BY run",
        )?;
        query
            .query_map(params![scope_id, request.workspace, request.owner], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let run_json = serde_json::to_string(&runs)?;
    let scopes = {
        let mut query=tx.prepare("SELECT DISTINCT a.scope_id FROM agent_scopes a WHERE a.scope_id=?1 OR
            (a.workspace_id=?2 AND a.owner_id=?3 AND a.agent_key=?4 AND a.incarnation=?5
            AND a.scope_id IN(SELECT ma_scope_id FROM message_events WHERE run_id IN(SELECT value FROM json_each(?6)))) ORDER BY a.scope_id")?;
        query
            .query_map(
                params![scope_id, request.workspace, request.owner, key, incarnation, run_json],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let jobs = {
        let mut query = tx.prepare(
            "SELECT DISTINCT job_id FROM rf_message_attempts WHERE workspace_id=?1 AND owner_id=?2
            AND run_id IN(SELECT value FROM json_each(?3)) AND job_id IS NOT NULL ORDER BY job_id",
        )?;
        query
            .query_map(params![request.workspace, request.owner, run_json], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let work = EraseWork {
        erase_id: erase_id.clone(),
        scope_id: scope_id.clone(),
        workspace: request.workspace.clone(),
        owner: request.owner.clone(),
        session: request.session.clone(),
        agent: request.agent.clone(),
        incarnation: incarnation.clone(),
        generation,
        forget_writes: request.forget_writes,
        runs,
        jobs,
        scopes,
    };
    for scope in &work.scopes {
        tx.execute(
            "INSERT INTO agent_erase_fences(scope_id,erase_id,state,next_generation)
            SELECT scope_id,?2,'draining',generation+1 FROM agent_scopes WHERE scope_id=?1",
            params![scope, erase_id],
        )?;
    }
    tx.execute(
        "INSERT INTO agent_erase_receipts(workspace_id,owner_id,erase_id,scope_id,incarnation,generation,state)
        VALUES(?1,?2,?3,?4,?5,?6,'pending')",
        params![
            request.workspace,
            request.owner,
            erase_id,
            scope_id,
            incarnation,
            generation
        ],
    )?;
    tx.execute(
        "INSERT INTO agent_erase_work(erase_id,work) VALUES(?1,?2)",
        params![erase_id, serde_json::to_string(&work)?],
    )?;
    tx.execute("INSERT INTO agent_erase_keys(workspace_id,owner_id,idempotency_key,request_method,canonical_path,scope_id,request_hash,erase_id,created_at)
        VALUES(?1,?2,?3,'DELETE',?4,?5,?6,?7,?8)",params![request.workspace,request.owner,request.key,
        format!("/api/sessions/{}/agents/{}/history",request.session,request.agent),scope_id,hash,erase_id,chrono::Utc::now().timestamp()])?;
    tx.commit()?;
    Ok(ErasePrepared::Work(Box::new(work)))
}

fn predicate(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| format!("\"{column}\" IN(SELECT value FROM json_each(?1))"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn inventory_counts() -> BTreeMap<String, u64> {
    MANIFEST_STORES.iter().map(|name| ((*name).to_owned(), 0)).collect()
}

/// Delete rows in dependency order. The inventory and independent final scan
/// are derived separately; a newly stamped table is a typed refusal.
fn erase_rows(
    conn: &Connection,
    scopes: &str,
    counts: &mut BTreeMap<String, u64>,
    erase_id: Option<&str>,
) -> anyhow::Result<()> {
    check_manifest(conn)?;
    let tables = stamped_tables(conn)?;
    // Defer links while every member of the scoped family is removed.
    conn.execute_batch("PRAGMA defer_foreign_keys=ON")?;
    for (table, columns) in &tables {
        let condition = predicate(columns);
        let amount: i64 = conn.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| row.get(0))?;
        *counts.entry(table.clone()).or_default() = u64::try_from(amount)?;
        if let Some(erase_id) = erase_id {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO agent_erase_manifest(erase_id,store,record_key)
                SELECT ?2,?3,json_array(rowid) FROM \"{table}\" WHERE {condition}"
                ),
                params![scopes, erase_id, table],
            )?;
        }
    }
    if let Some(erase_id) = erase_id {
        conn.execute(
            "DELETE FROM ramflux_erase_index WHERE (table_name,record_rowid) IN
            (SELECT store,json_extract(record_key,'$[0]') FROM agent_erase_manifest WHERE erase_id=?1)",
            [erase_id],
        )?;
    }
    // Follow stable target ids, never a content probe. These same deletion
    // adapters remove each target's derived embeddings, links, trash and topics.
    for (table, id_column) in [("memories", "id"), ("documents", "document_id")] {
        if !tables.contains_key(table) {
            continue;
        }
        let ids = {
            let mut query = conn.prepare(&format!(
                "SELECT {id_column} FROM {table} WHERE ma_scope_id IN(SELECT value FROM json_each(?1))"
            ))?;
            query
                .query_map([scopes], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in ids {
            if table == "memories" {
                super::rf_forget::delete_memory(conn, &id)?;
            } else {
                super::rf_forget::delete_document(conn, &id, false)?;
            }
        }
    }
    if tables.contains_key("prx_remote_trees") {
        conn.execute("DELETE FROM prx_remote_tree_resources WHERE (workspace_id,owner_id,trigger_message_id) IN
            (SELECT workspace_id,owner_id,trigger_message_id FROM prx_remote_trees WHERE ma_scope_id IN(SELECT value FROM json_each(?1)))",[scopes])?;
    }
    // Parent tables may cascade; all counts above include those removals.
    for (table, columns) in &tables {
        if table == "rf_message_attempts" {
            conn.execute(
                "UPDATE rf_message_attempts SET deleted_at=COALESCE(deleted_at,?2),request_hash='',erase_pending=0
                WHERE ma_scope_id IN(SELECT value FROM json_each(?1))",
                params![scopes, chrono::Utc::now().to_rfc3339()],
            )?;
        } else {
            conn.execute(
                &format!("DELETE FROM \"{table}\" WHERE {}", predicate(columns)),
                [scopes],
            )?;
        }
    }
    for table in tables.keys() {
        let after: i64 = conn.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| row.get(0))?;
        let before = counts.get(table).copied().unwrap_or(0);
        counts.insert(
            table.clone(),
            before
                .checked_sub(u64::try_from(after)?)
                .context("erase row count increased")?,
        );
    }
    for fts in ["memories_fts", "document_chunks_fts", "topics_fts"] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
            [fts],
            |row| row.get(0),
        )?;
        if exists {
            conn.execute(&format!("INSERT INTO {fts}({fts}) VALUES('rebuild')"), [])?;
        }
    }
    Ok(())
}

fn verify(conn: &Connection, scopes: &str) -> anyhow::Result<()> {
    check_manifest(conn)?;
    for (table, columns) in stamped_tables(conn)? {
        let payload = if table == "rf_message_attempts" {
            " AND (deleted_at IS NULL OR request_hash<>'' OR erase_pending<>0)"
        } else {
            ""
        };
        let remaining: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM \"{table}\" WHERE ({}){payload}",
                predicate(&columns)
            ),
            [scopes],
            |row| row.get(0),
        )?;
        if remaining != 0 {
            return Err(ErasureRefusal::Pending.into());
        }
    }
    Ok(())
}

pub(super) fn finish_history(memory: &SqliteMemory, work: &EraseWork) -> anyhow::Result<serde_json::Value> {
    // Re-read durable work: a retry may race another finisher. Refresh child
    // attribution after cancellation, before deleting its parent events.
    let mut work: EraseWork = {
        let writer = memory.pool.write();
        let text: String = writer.query_row(
            "SELECT work FROM agent_erase_work WHERE erase_id=?1",
            [&work.erase_id],
            |row| row.get(0),
        )?;
        serde_json::from_str(&text)?
    };
    {
        let writer = memory.pool.write();
        let tx = crate::runtime::sqlite_pool::begin_immediate(&writer)?;
        let runs = serde_json::to_string(&work.runs)?;
        let mut query=tx.prepare("WITH RECURSIVE family(run) AS (
            SELECT value FROM json_each(?1) UNION SELECT e.run_id FROM message_events e JOIN family f ON e.parent_run_id=f.run
            WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.run_id IS NOT NULL)
            SELECT DISTINCT e.ma_scope_id FROM message_events e JOIN family f ON f.run=e.run_id
            JOIN agent_scopes a ON a.scope_id=e.ma_scope_id
            WHERE a.workspace_id=?2 AND a.owner_id=?3 AND a.agent_key=?4 AND a.incarnation=?5")?;
        let extra = query
            .query_map(
                params![
                    runs,
                    work.workspace,
                    work.owner,
                    agent_key(&work.agent),
                    work.incarnation
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        work.scopes.extend(extra);
        work.scopes.sort();
        work.scopes.dedup();
        for scope in &work.scopes {
            tx.execute(
                "INSERT OR IGNORE INTO agent_erase_fences(scope_id,erase_id,state,next_generation)
                SELECT scope_id,?2,'draining',generation+1 FROM agent_scopes WHERE scope_id=?1",
                params![scope, work.erase_id],
            )?;
            let owner: String = tx.query_row(
                "SELECT erase_id FROM agent_erase_fences WHERE scope_id=?1",
                [scope],
                |row| row.get(0),
            )?;
            if owner != work.erase_id {
                return Err(ErasureRefusal::Pending.into());
            }
        }
        tx.execute(
            "UPDATE agent_erase_work SET work=?2 WHERE erase_id=?1",
            params![work.erase_id, serde_json::to_string(&work)?],
        )?;
        tx.commit()?;
    }
    let scopes = serde_json::to_string(&work.scopes)?;
    for scope in &work.scopes {
        let id = uuid::Uuid::parse_str(scope)?;
        if !memory.mailbox_runtime.drain_and_join(id, Duration::from_secs(4)) {
            return Err(ErasureRefusal::Pending.into());
        }
    }
    {
        let conn = memory.pool.write();
        let tx = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
        let (saved, erased): (String, bool) = tx.query_row(
            "SELECT work,main_erased FROM agent_erase_work WHERE erase_id=?1",
            [&work.erase_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(saved == serde_json::to_string(&work)?, "erase work identity changed");
        if !erased {
            let mut counts = inventory_counts();
            let mut forgotten = crate::memory::RfForgetCounts::default();
            if work.forget_writes {
                for run in &work.runs {
                    let found = super::rf_forget::forget_family(
                        &tx,
                        run,
                        super::rf_forget::Partition {
                            scope: Some(&work.scope_id),
                            main: work.agent == "main",
                        },
                        &chrono::Utc::now().to_rfc3339(),
                    )?;
                    forgotten.memories_created += found.memories_created;
                    forgotten.memories_touched += found.memories_touched;
                    forgotten.documents_created += found.documents_created;
                    forgotten.documents_touched += found.documents_touched;
                }
            }
            erase_rows(&tx, &scopes, &mut counts, Some(&work.erase_id))?;
            // Session previews are shared by agents; derive them again from survivors.
            tx.execute("UPDATE sessions SET last_message_preview=COALESCE((SELECT substr(content,1,200) FROM conversation_turns
                WHERE session_key=?1 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1),''),
                message_count=(SELECT count(*) FROM conversation_turns WHERE session_key=?1 AND deleted_at IS NULL)
                WHERE session_key=?1",[&work.session])?;
            verify(&tx, &scopes)?;
            tx.execute(
                "UPDATE agent_erase_work SET main_erased=1,counts=?2,forgotten=?3 WHERE erase_id=?1",
                params![
                    work.erase_id,
                    serde_json::to_string(&counts)?,
                    serde_json::to_string(&forgotten)?
                ],
            )?;
        }
        tx.commit()?;
    }
    erase_side_stores(memory, &work, &scopes)?;
    super::memory_impl::truncate_wal(&memory.pool, Duration::from_secs(4)).map_err(|_| ErasureRefusal::Pending)?;
    let conn = memory.pool.write();
    let tx = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
    verify(&tx, &scopes)?;
    let existing: Option<String> = tx.query_row(
        "SELECT receipt FROM agent_erase_work WHERE erase_id=?1",
        [&work.erase_id],
        |row| row.get(0),
    )?;
    if let Some(existing) = existing {
        return Ok(serde_json::from_str(&existing)?);
    }
    let forgotten: String = tx.query_row(
        "SELECT forgotten FROM agent_erase_work WHERE erase_id=?1",
        [&work.erase_id],
        |row| row.get(0),
    )?;
    let mut receipt = serde_json::json!({"status":"erased","scope":{"session":work.session,"agent":work.agent,"incarnation":work.incarnation},
        "erase_id":work.erase_id,"generation":work.generation,"remaining":0,"incarnation":work.incarnation});
    if work.forget_writes {
        let counts: crate::memory::RfForgetCounts = serde_json::from_str(&forgotten)?;
        receipt.as_object_mut().context("erase receipt must be an object")?.insert("forgotten".to_owned(),serde_json::json!({"created":counts.memories_created+counts.documents_created,"touched":counts.memories_touched+counts.documents_touched}));
    }
    tx.execute(
        "UPDATE agent_erase_receipts SET state='erased',remaining=0,completed_at=?2 WHERE erase_id=?1",
        params![work.erase_id, chrono::Utc::now().timestamp()],
    )?;
    tx.execute(
        "UPDATE agent_erase_keys SET expires_at=?2 WHERE erase_id=?1",
        params![work.erase_id, chrono::Utc::now().timestamp() + 604_800],
    )?;
    tx.execute(
        "UPDATE agent_erase_fences SET state='erased' WHERE erase_id=?1",
        [&work.erase_id],
    )?;
    tx.execute(
        "UPDATE agent_erase_work SET receipt=?2 WHERE erase_id=?1",
        params![work.erase_id, serde_json::to_string(&receipt)?],
    )?;
    let next = work.generation.checked_add(1).context("erase generation exhausted")?;
    tx.execute(
        "INSERT INTO agent_scopes(scope_id,workspace_id,owner_id,session_key,agent_key,incarnation,generation)
        SELECT ?1,workspace_id,owner_id,session_key,agent_key,incarnation,?2 FROM agent_scopes a WHERE scope_id=?3
        AND EXISTS(SELECT 1 FROM agent_incarnations i WHERE i.workspace_id=a.workspace_id AND i.owner_id=a.owner_id
        AND i.agent_key=a.agent_key AND i.incarnation=a.incarnation AND i.state='active')",
        params![uuid::Uuid::new_v4().to_string(), next, work.scope_id],
    )?;
    tx.commit()?;
    Ok(receipt)
}

fn erase_side_stores(memory: &SqliteMemory, work: &EraseWork, scopes: &str) -> anyhow::Result<()> {
    let workspace = std::path::Path::new(&work.workspace);
    for path in [
        workspace.join(crate::decision_engine::observe::TELEMETRY_FILE),
        workspace.join("state/chat-mailbox.db"),
    ] {
        if !path.try_exists()? {
            continue;
        }
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(Duration::from_secs(4))?;
        conn.execute_batch("PRAGMA secure_delete=ON;PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS agent_erase_side_progress(erase_id TEXT PRIMARY KEY,counts TEXT NOT NULL) STRICT;")?;
        let tx = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT counts FROM agent_erase_side_progress WHERE erase_id=?1",
                [&work.erase_id],
                |row| row.get(0),
            )
            .optional()?;
        let counts = if let Some(old) = old {
            serde_json::from_str::<BTreeMap<String, u64>>(&old)?
        } else {
            let mut counts = BTreeMap::new();
            erase_rows(&tx, scopes, &mut counts, None)?;
            verify(&tx, scopes)?;
            tx.execute(
                "INSERT INTO agent_erase_side_progress VALUES(?1,?2)",
                params![work.erase_id, serde_json::to_string(&counts)?],
            )?;
            counts
        };
        tx.commit()?;
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(ErasureRefusal::Pending.into());
        }
        // Each side transaction retained its own counts. Apply them exactly
        // once, with the main progress update, even after a crash between DBs.
        let writer = memory.pool.write();
        let tx = crate::runtime::sqlite_pool::begin_immediate(&writer)?;
        let path_key = path.to_string_lossy();
        let applied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_erase_side_applied WHERE erase_id=?1 AND store_path=?2)",
            params![work.erase_id, path_key],
            |row| row.get(0),
        )?;
        if !applied {
            let old: String = tx.query_row(
                "SELECT counts FROM agent_erase_work WHERE erase_id=?1",
                [&work.erase_id],
                |row| row.get(0),
            )?;
            let mut total: BTreeMap<String, u64> = serde_json::from_str(&old)?;
            for (store, count) in counts {
                let old = total.get(&store).copied().unwrap_or(0);
                total.insert(store, old.checked_add(count).context("erase count overflow")?);
            }
            tx.execute(
                "UPDATE agent_erase_work SET counts=?2 WHERE erase_id=?1",
                params![work.erase_id, serde_json::to_string(&total)?],
            )?;
            tx.execute(
                "INSERT INTO agent_erase_side_applied VALUES(?1,?2)",
                params![work.erase_id, path_key],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

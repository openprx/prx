//! Spawn-session persistence shares the source event transaction and scope.
use super::SqliteMemory;
use crate::agent::compaction::validated_context_handoff;
use crate::memory::branch_copy::CONTEXT_SWITCH_EVENT_TYPE;
use crate::memory::child_sessions::{ChildSessionError, ChildSessionRecord, ChildTurn};
use crate::memory::{AgentPartition, MessageEvent, PartitionKey};
use rusqlite::{OptionalExtension, params};

pub(super) fn source_written(conn: &rusqlite::Transaction<'_>, event: &MessageEvent) -> anyhow::Result<()> {
    if !matches!(
        event.source,
        crate::memory::MessageEventSource::SessionsSpawn | crate::memory::MessageEventSource::SessionWorker
    ) {
        return Ok(());
    }
    let Some(raw) = event.raw_payload_json.as_deref() else {
        return Ok(());
    };
    // Ordinary source events can retain opaque provider payloads. Only the
    // structured launch/terminal metadata participates in child persistence.
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error)
            if matches!(
                event.event_type.as_str(),
                "task.spawned" | "turn.finalized" | "turn.admission.failed"
            ) =>
        {
            return Err(error.into());
        }
        Err(_) => return Ok(()),
    };
    // task.spawned mirrors this metadata for observation; only the admitted
    // user input creates/advances the durable logical child.
    if event.role == "user"
        && event.event_type == "message.created"
        && let Some(value) = value.get("child_session_record")
    {
        let mut record: ChildSessionRecord = serde_json::from_value(value.clone())?;
        let run = event.run_id.as_deref().ok_or(ChildSessionError::InvalidRecord)?;
        anyhow::ensure!(
            event.role == "user"
                && event.source == crate::memory::MessageEventSource::SessionsSpawn
                && record.workspace == event.workspace_id
                && Some(record.owner.as_str()) == event.owner_id.as_deref()
                && Some(record.session_key.as_str()) == event.session_key.as_deref()
                && Some(record.parent_run.as_str()) == event.parent_run_id.as_deref()
                && Some(record.parent_session.as_str()) == event.parent_session_key.as_deref()
                && record.runs.last().is_some_and(|last| last == run)
                && record.session_key.starts_with("spawn:"),
            ChildSessionError::IdentityMismatch
        );
        let existing: Option<String> = conn
            .query_row(
                "SELECT record FROM child_sessions WHERE key=?1 AND workspace_id=?2 AND owner_id=?3",
                params![record.session_key, record.workspace, record.owner],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let previous: ChildSessionRecord = serde_json::from_str(&existing)?;
            anyhow::ensure!(
                previous.parent_session == record.parent_session && previous.agent == record.agent,
                ChildSessionError::IdentityMismatch
            );
            anyhow::ensure!(
                record.runs.len() == previous.runs.len() + 1 && record.runs.starts_with(&previous.runs),
                ChildSessionError::InvalidRecord
            );
            record.created_at = previous.created_at;
            record.context_after = previous.context_after;
            record.context = previous.context;
        }
        let scope: Option<String> = conn.query_row(
            "SELECT ma_scope_id FROM message_events WHERE event_id=?1",
            [&event.event_id],
            |r| r.get(0),
        )?;
        let scope = match scope {
            Some(scope) => scope,
            None => super::agent_migration::resolve_write_scope(
                conn,
                &AgentPartition {
                    workspace_id: record.workspace.clone(),
                    owner_id: record.owner.clone(),
                    key: PartitionKey::Main,
                },
                Some(&record.session_key),
            )?,
        };
        conn.execute(
            "UPDATE message_events SET ma_scope_id=?2 WHERE event_id=?1 AND ma_scope_id IS NULL",
            params![event.event_id, scope],
        )?;
        conn.execute("UPDATE memory_events SET ma_scope_id=?2 WHERE subject_table='message_events' AND subject_id=?1 AND ma_scope_id IS NULL",params![event.event_id,scope])?;
        conn.execute("INSERT INTO child_sessions(key,record,workspace_id,owner_id,instance_id,session_id,agent_key,ma_scope_id,incarnation,generation)
            SELECT ?1,?2,?3,?4,'local',?1,agent_key,scope_id,incarnation,generation FROM agent_scopes WHERE scope_id=?5
            ON CONFLICT(key) DO UPDATE SET record=excluded.record WHERE child_sessions.workspace_id=excluded.workspace_id AND child_sessions.owner_id=excluded.owner_id AND child_sessions.ma_scope_id=excluded.ma_scope_id",
            params![record.session_key,serde_json::to_string(&record)?,record.workspace,record.owner,scope])?;
    }
    if matches!(event.event_type.as_str(), "turn.finalized" | "turn.admission.failed") {
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("failed");
        conn.execute(
            "UPDATE child_sessions SET record=json_set(record,'$.status',?4,'$.ended_at',?5)
            WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3 AND json_extract(record,'$.runs[#-1]')=?6",
            params![
                event.workspace_id,
                event.owner_id,
                event.session_key,
                status,
                event.created_at,
                event.run_id
            ],
        )?;
    }
    Ok(())
}

pub(super) fn sessions(memory: &SqliteMemory, workspace: &str, owner: &str) -> anyhow::Result<Vec<ChildSessionRecord>> {
    let reader = memory.pool.read()?;
    let conn = reader.connection()?;
    let mut query = conn.prepare("SELECT record FROM child_sessions c JOIN agent_scopes s ON s.scope_id=c.ma_scope_id
        WHERE c.workspace_id=?1 AND c.owner_id=?2 AND c.key LIKE 'spawn:%'
        AND EXISTS(SELECT 1 FROM agent_incarnations i WHERE i.workspace_id=s.workspace_id AND i.owner_id=s.owner_id AND i.agent_key=s.agent_key AND i.incarnation=s.incarnation AND i.state='active')
        AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=s.scope_id) ORDER BY c.rowid")?;
    let mut records = Vec::new();
    for row in query.query_map(params![workspace, owner], |r| r.get::<_, String>(0))? {
        records.push(serde_json::from_str(&row?)?);
    }
    Ok(records)
}

/// Switch rows read per history lookup, newest first. A row that does not
/// validate as a handoff is passed over.
const HANDOFF_CANDIDATE_ROWS: i64 = 8;

/// The newest switch row of the child, after its last context reset, that
/// validates as a handoff: the turn a history starts with, and the row id its
/// handoff covers up to.
fn newest_handoff(
    conn: &rusqlite::Connection,
    record: &ChildSessionRecord,
) -> anyhow::Result<Option<(ChildTurn, i64)>> {
    let mut query = conn.prepare("SELECT e.content,e.created_at FROM message_events e
        JOIN child_sessions c ON c.key=e.session_key AND c.ma_scope_id=e.ma_scope_id
        JOIN agent_scopes s ON s.scope_id=c.ma_scope_id
        WHERE c.key=?1 AND c.workspace_id=?2 AND c.owner_id=?3 AND e.workspace_id=?2 AND e.owner_id=?3
        AND EXISTS(SELECT 1 FROM agent_incarnations i WHERE i.workspace_id=s.workspace_id AND i.owner_id=s.owner_id AND i.agent_key=s.agent_key AND i.incarnation=s.incarnation AND i.state='active')
        AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=s.scope_id)
        AND e.id>?4 AND e.event_type=?5
        ORDER BY e.id DESC LIMIT ?6")?;
    let rows = query
        .query_map(
            params![
                record.session_key,
                record.workspace,
                record.owner,
                record.context_after,
                CONTEXT_SWITCH_EVENT_TYPE,
                HANDOFF_CANDIDATE_ROWS
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (content, created_at) in rows {
        let Some(handoff) = validated_context_handoff(&content) else {
            continue;
        };
        let timestamp = chrono::DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&chrono::Utc);
        let turn = ChildTurn {
            role: "assistant".to_owned(),
            content,
            timestamp,
        };
        return Ok(Some((turn, handoff.last_row_id)));
    }
    Ok(None)
}

pub(super) fn history(memory: &SqliteMemory, record: &ChildSessionRecord) -> anyhow::Result<Vec<ChildTurn>> {
    let reader = memory.pool.read()?;
    let conn = reader.connection()?;
    let live:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM child_sessions c JOIN agent_scopes s ON c.ma_scope_id=s.scope_id WHERE c.key=?1 AND c.workspace_id=?2 AND c.owner_id=?3 AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=s.scope_id))",
        params![record.session_key,record.workspace,record.owner],|r|r.get(0))?;
    anyhow::ensure!(live, ChildSessionError::ScopeRefused);
    // A handoff newer than the last context reset starts the history; rows the
    // handoff covers are not loaded again. Without one, the history is exactly
    // the stored context plus the rows after it.
    let (mut result, after) = match newest_handoff(conn, record)? {
        Some((turn, last_row_id)) => (vec![turn], record.context_after.max(last_row_id)),
        None => (record.context.clone(), record.context_after),
    };
    let mut query = conn.prepare("SELECT e.role,e.content,e.created_at FROM message_events e
        JOIN child_sessions c ON c.key=e.session_key AND c.ma_scope_id=e.ma_scope_id
        JOIN agent_scopes s ON s.scope_id=c.ma_scope_id
        WHERE c.key=?1 AND c.workspace_id=?2 AND c.owner_id=?3 AND e.workspace_id=?2 AND e.owner_id=?3
        AND EXISTS(SELECT 1 FROM agent_incarnations i WHERE i.workspace_id=s.workspace_id AND i.owner_id=s.owner_id AND i.agent_key=s.agent_key AND i.incarnation=s.incarnation AND i.state='active')
        AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=s.scope_id)
        AND e.id>?4 AND e.role IN('user','assistant','tool') AND e.event_type='message.created'
        AND NOT EXISTS(SELECT 1 FROM message_events p WHERE p.event_id=e.causation_event_id AND p.workspace_id=e.workspace_id AND p.owner_id=e.owner_id AND p.session_key=e.session_key AND p.source='prx_mailbox' AND e.role='user')
        AND NOT EXISTS(SELECT 1 FROM message_events p WHERE p.workspace_id=e.workspace_id AND p.owner_id=e.owner_id
          AND p.id<e.id AND p.session_key=e.session_key AND p.role=e.role AND p.content_hash=e.content_hash
          AND json_extract(e.raw_payload_json,'$.task_run_id')=p.run_id)
        ORDER BY e.id")?;
    let turns: anyhow::Result<Vec<ChildTurn>> = query
        .query_map(
            params![record.session_key, record.workspace, record.owner, after],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )?
        .map(|row| {
            let (role, content, timestamp) = row?;
            Ok(ChildTurn {
                role,
                content,
                timestamp: chrono::DateTime::parse_from_rfc3339(&timestamp)?.with_timezone(&chrono::Utc),
            })
        })
        .collect();
    result.extend(turns?);
    Ok(result)
}

pub(super) fn replace_context(
    memory: &SqliteMemory,
    record: &ChildSessionRecord,
    turns: &[ChildTurn],
) -> anyhow::Result<()> {
    let writer = memory.pool.write();
    let tx = rusqlite::Transaction::new_unchecked(&writer, rusqlite::TransactionBehavior::Immediate)?;
    let mut value = record.clone();
    value.context_after = tx.query_row(
        "SELECT COALESCE(MAX(id),0) FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND session_key=?3",
        params![record.workspace, record.owner, record.session_key],
        |r| r.get(0),
    )?;
    value.context = turns.to_owned();
    let written=tx.execute("UPDATE child_sessions SET record=?4 WHERE key=?1 AND workspace_id=?2 AND owner_id=?3 AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=child_sessions.ma_scope_id)",
        params![record.session_key,record.workspace,record.owner,serde_json::to_string(&value)?])?;
    anyhow::ensure!(written == 1, ChildSessionError::ScopeRefused);
    tx.commit()?;
    Ok(())
}

//! Child lifecycle sources and restart cancellation headers.
use super::sync_journal::{Header, append, event_header};
use crate::memory::MemoryEvent;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(super) fn memory_written(conn: &Connection, event: &MemoryEvent) -> anyhow::Result<()> {
    if !matches!(
        event.event_type.as_str(),
        "task.spawned" | "task.completed" | "task.failed" | "task.cancelled"
    ) {
        return Ok(());
    }
    let Some(run) = &event.run_id else {
        return Ok(());
    };
    let source:Option<String>=conn.query_row("SELECT event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND run_id=?3 AND parent_run_id IS NOT NULL ORDER BY id LIMIT 1",params![event.workspace_id,event.owner_id,run],|r|r.get(0)).optional()?;
    let Some(source) = source else {
        return Ok(());
    };
    let Some(mut h) = event_header(conn, &source, "child_session.upsert")? else {
        return Ok(());
    };
    if event.event_type == "task.spawned" {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_journal WHERE owner_id=?1 AND run_id=?2 AND kind='child_session.upsert')",
            params![h.owner, run],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(());
        }
    }
    h.source_ref = format!("memory:{}", event.event_id);
    h.event_id = format!("child:{}", event.event_id);
    h.timestamp = event.created_at.clone();
    append(conn, &h)
}

pub(super) fn child_payload(conn: &Connection, h: &Header) -> anyhow::Result<Value> {
    let deleted:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM rf_message_attempts a WHERE a.owner_id=?1 AND a.run_id=?2 AND (a.deleted_at IS NOT NULL OR a.reply_deleted_at IS NOT NULL OR EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=a.ma_scope_id)))",params![h.owner,h.root_run_id.as_ref().or(h.run_id.as_ref())],|r|r.get(0))?;
    if deleted {
        return Ok(json!({"deleted":true,"session_key":h.session}));
    }
    let start:Option<(String,Option<String>)>=conn.query_row("SELECT created_at,json_extract(payload_json,'$.mode') FROM memory_events WHERE owner_id=?1 AND run_id=?2 AND event_type='task.spawned' ORDER BY id LIMIT 1",params![h.owner,h.run_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let start = if start.is_some() {
        start
    } else {
        conn.query_row("SELECT created_at,json_extract(raw_payload_json,'$.mode') FROM message_events WHERE owner_id=?1 AND run_id=?2 ORDER BY id LIMIT 1",params![h.owner,h.run_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
    };
    let (started, mode) = start.unwrap_or_else(|| (h.timestamp.clone(), None));
    let (status, ended) = if let Some(id) = h.source_ref.strip_prefix("memory:") {
        let kind: Option<String> = conn
            .query_row(
                "SELECT event_type FROM memory_events WHERE event_id=?1 AND owner_id=?2",
                params![id, h.owner],
                |r| r.get(0),
            )
            .optional()?;
        match kind.as_deref() {
            Some("task.spawned") => ("running", false),
            Some("task.completed") => ("succeeded", true),
            Some("task.cancelled") => ("cancelled", true),
            Some("task.failed") => ("failed", true),
            _ => return Ok(json!({"deleted":true,"session_key":h.session})),
        }
    } else {
        let id = h
            .source_ref
            .strip_prefix("event:")
            .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
        let row:Option<(String,Option<String>,Option<String>)>=conn.query_row("SELECT event_type,json_extract(raw_payload_json,'$.status'),json_extract(raw_payload_json,'$.mode') FROM message_events WHERE event_id=?1 AND owner_id=?2",params![id,h.owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((kind, status, source_mode)) = row else {
            return Ok(json!({"deleted":true,"session_key":h.session}));
        };
        if !matches!(kind.as_str(), "turn.finalized" | "turn.admission.failed") {
            return Ok(
                json!({"status":"running","mode":source_mode.or(mode).unwrap_or_else(||"task".into()),"started_at":started}),
            );
        }
        (
            match status.as_deref() {
                Some("completed" | "succeeded" | "silent") => "succeeded",
                Some("cancelled") => "cancelled",
                _ => "failed",
            },
            true,
        )
    };
    let mut p = json!({"status":status,"mode":mode.unwrap_or_else(||"task".into()),"started_at":started});
    if ended {
        p.as_object_mut()
            .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?
            .insert("ended_at".into(), json!(h.timestamp));
    }
    Ok(p)
}

pub(super) fn cancel_approvals(conn: &Connection, root: &Header, workspace: &str, run: &str) -> anyhow::Result<()> {
    use crate::security::approval_ledger::keys::{ApprovalAuthority, RunCallAuthority};
    let mut q=conn.prepare("SELECT CASE WHEN length(CAST(raw_payload_json AS BLOB))<=65536 THEN raw_payload_json ELSE NULL END FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND run_id=?3 AND event_type='approval.call.pending'")?;
    for raw in q.query_map(params![workspace, root.owner, run], |r| r.get::<_, Option<String>>(0))? {
        let raw = raw?.ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
        let value: Value = serde_json::from_str(&raw)?;
        let authority: RunCallAuthority = serde_json::from_value(
            value
                .get("record")
                .and_then(|r| r.get("authority"))
                .cloned()
                .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?,
        )?;
        let closed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key IN(?3,?4))",params![workspace,root.owner,authority.issued_key(),authority.resolution_key()],|r|r.get(0))?;
        if closed {
            continue;
        }
        let id = authority.call_id();
        let mut h = root.clone();
        h.kind = "approval.decided".into();
        h.source_ref = format!("approval-cancel:{id}");
        h.event_id = format!("approval-decided:{id}");
        append(conn, &h)?;
    }
    Ok(())
}

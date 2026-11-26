//! Read-time source projections. No journal or snapshot stores plaintext.
use super::{
    SqliteMemory,
    sync_journal::{Header, append},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) const MAX_PAYLOAD: usize = 1024 * 1024 - 8192;

pub(super) fn event(conn: &Connection, h: &Header) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(h)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
    object.remove("owner");
    object.remove("source_ref");
    object.remove("timestamp");
    object.insert("type".into(), json!("event"));
    let mut body = payload(conn, h)?;
    let p = body
        .as_object_mut()
        .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
    p.insert("session_key".into(), json!(h.session));
    p.insert("agent".into(), json!(h.agent));
    p.insert("incarnation".into(), json!(h.incarnation));
    if let Some(root) = &h.root_session {
        p.insert("root_session".into(), json!(root));
        p.insert("root_run_id".into(), json!(h.root_run_id));
        p.insert("parent_session".into(), json!(h.parent_session));
        p.insert("parent_run_id".into(), json!(h.parent_run_id));
    }
    if let Some(run) = &h.run_id {
        p.insert("run_id".into(), json!(run));
    }
    let bytes = serde_json::to_vec(&body)?.len();
    if bytes > MAX_PAYLOAD {
        let mut marker = json!({"message_id":body.get("message_id"),"oversized":true,"bytes":bytes,"session_key":h.session,"agent":h.agent,"incarnation":h.incarnation});
        for key in [
            "run_id",
            "parent_session",
            "parent_run_id",
            "root_session",
            "root_run_id",
        ] {
            if let Some(value) = body.get(key) {
                marker
                    .as_object_mut()
                    .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?
                    .insert(key.into(), value.clone());
            }
        }
        body = marker;
    }
    object.insert("payload".into(), body);
    Ok(value)
}

pub(super) fn payload(conn: &Connection, h: &Header) -> anyhow::Result<Value> {
    if let Some(retained) = super::sync_retained::payload(h)? {
        return Ok(retained);
    }
    if h.kind == "session.upsert" && (h.session.starts_with("cron:") || h.session.starts_with("xin:")) {
        let row: Option<(String,Option<String>)> = conn.query_row("SELECT (SELECT created_at FROM sessions WHERE session_key=?1 AND owner_id=?2),raw_payload_json FROM message_events WHERE session_key=?1 AND owner_id=?2 AND parent_run_id IS NULL AND (role='user' OR event_type='scheduled.skipped') ORDER BY id DESC LIMIT 1", params![h.session,h.owner], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((created_at, raw)) = row else {
            return Ok(json!({"deleted":true}));
        };
        let metadata = scheduled_metadata(raw.as_deref())?;
        return Ok(
            json!({"session_key":h.session,"channel":if h.session.starts_with("cron:") {"cron"} else {"xin"},"created_at":created_at,"metadata":{"job_id":metadata.get("job_id"),"job_version":metadata.get("job_version")}}),
        );
    }
    if h.kind == "turn.started" && (h.session.starts_with("cron:") || h.session.starts_with("xin:")) {
        let id = h
            .source_ref
            .strip_prefix("event:")
            .ok_or(crate::memory::sync::SyncError::InvalidSource)?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT raw_payload_json FROM message_events WHERE event_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        return raw
            .as_deref()
            .map_or_else(|| Ok(json!({"deleted":true})), |raw| scheduled_metadata(Some(raw)));
    }
    if h.kind == "turn.failed" && (h.session.starts_with("cron:") || h.session.starts_with("xin:")) {
        if let Some(id) = h.source_ref.strip_prefix("event:") {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT raw_payload_json FROM message_events WHERE event_id=?1 AND event_type='scheduled.skipped'",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(raw) = raw {
                let p: Value = serde_json::from_str(&raw)?;
                return Ok(json!({"reason":p.get("reason"),"retryable":false}));
            }
        }
    }
    if let Some(reference) = h.source_ref.strip_prefix("final:") {
        let (marker, message) = reference
            .split_once(':')
            .ok_or(crate::memory::sync::SyncError::InvalidSource)?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events WHERE event_id=?1 AND owner_id=?2)",
            params![marker, h.owner],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(json!({"message_id":message,"deleted":true,"timestamp":h.timestamp}));
        }
        let mut source = h.clone();
        source.source_ref = format!("event:{message}");
        return payload(conn, &source);
    }
    if h.kind == "child_session.upsert" || (h.kind == "turn.started" && h.source_ref.starts_with("memory:")) {
        return super::sync_lifecycle::child_payload(conn, h);
    }
    if let Some(id) = h.source_ref.strip_prefix("approval-cancel:") {
        return Ok(json!({"approval_id":id,"decision":"cancelled"}));
    }

    if let Some(reference) = h.source_ref.strip_prefix("deleted:") {
        let (event_id, message_id) = reference
            .split_once(':')
            .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT raw_payload_json FROM message_events WHERE event_id=?1 AND owner_id=?2",
                params![event_id, h.owner],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let mut payload: Value = raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_else(|| json!({"deleted":true}));
        payload
            .as_object_mut()
            .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?
            .insert("message_id".into(), json!(message_id));
        return Ok(payload);
    }
    if let Some(id) = h.source_ref.strip_prefix("session:") {
        return Ok(conn
            .query_row(
                "SELECT channel,created_at FROM sessions WHERE session_key=?1 AND owner_id=?2",
                params![id, h.owner],
                |r| Ok(json!({"session_key":id,"channel":r.get::<_,String>(0)?,"created_at":r.get::<_,String>(1)?})),
            )
            .optional()?
            .unwrap_or_else(|| json!({"session_key":id,"deleted":true})));
    }
    if let Some(id) = h.source_ref.strip_prefix("attempt:") {
        return Ok(conn.query_row("SELECT run_id,job_id,client_message_id,attempt FROM rf_message_attempts WHERE message_event_id=?1 AND owner_id=?2",params![id,h.owner],|r|Ok(json!({"run_id":r.get::<_,Option<String>>(0)?,"job_id":r.get::<_,Option<String>>(1)?,"client_message_id":r.get::<_,String>(2)?,"attempt":r.get::<_,i64>(3)?}))).optional()?.unwrap_or_else(||json!({"deleted":true})));
    }
    if h.source_ref.starts_with("interrupt:") {
        return Ok(json!({"reason":"prx_restart","retryable":true}));
    }
    if let Some(id) = h.source_ref.strip_prefix("config:") {
        return Ok(conn.query_row("SELECT revision,enabled,deleted FROM sync_config_sources WHERE id=?1 AND owner_id=?2",params![id,h.owner],|r|Ok(json!({"revision":r.get::<_,Option<String>>(0)?,"enabled":r.get::<_,bool>(1)?,"deleted":r.get::<_,bool>(2)?}))).optional()?.unwrap_or_else(||json!({"revision":null,"deleted":true})));
    }
    if let Some(id) = h.source_ref.strip_prefix("erase:") {
        return Ok(conn.query_row("SELECT w.receipt,w.counts FROM agent_erase_work w JOIN agent_erase_receipts r ON r.erase_id=w.erase_id WHERE w.erase_id=?1 AND r.owner_id=?2",params![id,h.owner],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,String>(1)?))).optional()?.map(|(receipt,counts)|->anyhow::Result<Value>{
            let mut receipt:Value=receipt.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_else(||json!({"erase_id":id}));
            let obj=receipt.as_object_mut().ok_or_else(||crate::memory::sync::SyncError::InvalidSource)?;
            obj.insert("counts".into(),serde_json::from_str(&counts)?);Ok(receipt)
        }).transpose()?.unwrap_or_else(||json!({"erase_id":id,"deleted":true})));
    }
    if let Some(id) = h.source_ref.strip_prefix("purge:") {
        return Ok(conn
            .query_row(
                "SELECT receipt FROM agent_purge_operations WHERE purge_id=?1 AND owner_id=?2",
                params![id, h.owner],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(|text| -> anyhow::Result<Value> {
                let receipt: Value = serde_json::from_str(&text)?;
                Ok(json!({"erase_id":id,"scope":"agent","counts":receipt.get("counts")}))
            })
            .transpose()?
            .unwrap_or_else(|| json!({"erase_id":id,"scope":"agent","deleted":true})));
    }
    let id = h
        .source_ref
        .strip_prefix("event:")
        .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
    let deleted:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM message_events e WHERE e.event_id=?1 AND e.owner_id=?2 AND (EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=e.ma_scope_id) OR EXISTS(SELECT 1 FROM rf_message_attempts a WHERE a.workspace_id=e.workspace_id AND a.owner_id=e.owner_id AND ((a.deleted_at IS NOT NULL AND (a.message_event_id=e.event_id OR a.run_id=e.run_id)) OR (a.reply_deleted_at IS NOT NULL AND a.run_id=e.run_id AND a.message_event_id<>e.event_id)))))",params![id,h.owner],|r|r.get(0))?;
    if deleted {
        return Ok(json!({"message_id":id,"deleted":true,"timestamp":h.timestamp}));
    }
    let row=conn.query_row("SELECT role,event_type,CASE WHEN length(CAST(content AS BLOB))<=?3 THEN content ELSE '' END,created_at,CASE WHEN length(CAST(raw_payload_json AS BLOB))<=?3 THEN raw_payload_json ELSE NULL END,length(CAST(content AS BLOB)),parent_session_key FROM message_events WHERE event_id=?1 AND owner_id=?2",params![id,h.owner,i64::try_from(MAX_PAYLOAD)?],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,i64>(5)?,r.get::<_,Option<String>>(6)?))).optional()?;
    let Some((role, kind, content, timestamp, raw, bytes, _)) = row else {
        return Ok(json!({"message_id":id,"deleted":true,"timestamp":h.timestamp}));
    };
    if kind == "message.deleted" {
        return Ok(json!({"message_id":id,"deleted":true,"timestamp":timestamp}));
    }
    let raw: Value = raw
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    match h.kind.as_str() {
        "approval.pending" => {
            let record = raw.get("record").ok_or(crate::memory::sync::SyncError::InvalidSource)?;
            let authority: crate::security::approval_ledger::keys::RunCallAuthority = serde_json::from_value(
                record
                    .get("authority")
                    .cloned()
                    .ok_or(crate::memory::sync::SyncError::InvalidSource)?,
            )?;
            Ok(
                json!({"approval_id":authority.call_id(),"tool":record.pointer("/rf/tool"),"summary":record.pointer("/rf/summary"),"expires_at":record.get("expires_at")}),
            )
        }
        "approval.decided" => {
            let mut value = json!({"approval_id":raw.get("approval_id"),"decision":raw.get("decision")});
            if raw.get("decision").and_then(Value::as_str) == Some("denied") {
                if let Some(cause) = raw.get("cause").filter(|v| !v.is_null()) {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("cause".into(), cause.clone());
                    }
                }
            }
            Ok(value)
        }
        "turn.deleted" => Ok(raw),
        "turn.failed" => Ok(
            json!({"reason":if kind=="turn.admission.failed" {"terminal_persistence_failed"} else if raw.get("provider_outcome").is_some_and(|value|!value.is_null()) {"provider_error"}else{"internal"},"retryable":false}),
        ),
        "turn.cancelled" => Ok(json!({})),
        "session.upsert" => Ok(json!({"session_key":h.session,"channel":"spawn","created_at":timestamp})),
        _ => {
            let message_id: Option<String> = conn
                .query_row(
                    "SELECT message_id FROM conversation_turns WHERE message_event_id=?1 LIMIT 1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let message_id = message_id.as_deref().unwrap_or(id);
            if bytes > i64::try_from(MAX_PAYLOAD)? {
                return Ok(json!({"message_id":message_id,"oversized":true,"bytes":bytes}));
            }
            let input:Option<(String,bool)>=conn.query_row("SELECT client_message_id,run_id IS NOT NULL FROM rf_message_attempts WHERE message_event_id=?1 AND owner_id=?2",params![id,h.owner],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let mut value = json!({"message_id":message_id,"role":role,"content":content,"final":role=="assistant","timestamp":timestamp});
            if let Some(provenance) = raw.get("provenance") {
                let object = value
                    .as_object_mut()
                    .ok_or(crate::memory::sync::SyncError::InvalidSource)?;
                object.insert("provenance".into(), provenance.clone());
                object.insert(
                    "from".into(),
                    raw.get("from")
                        .cloned()
                        .ok_or(crate::memory::sync::SyncError::InvalidSource)?,
                );
            }
            if let Some((client, turn)) = input {
                let obj = value
                    .as_object_mut()
                    .ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?;
                obj.insert("client_message_id".into(), json!(client));
                obj.insert("deliver".into(), json!(if turn { "turn" } else { "context" }));
            }
            Ok(value)
        }
    }
}

pub(super) fn refresh_config(
    memory: &SqliteMemory,
    config: &crate::config::Config,
    revision: &str,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    let conn = memory.pool.write();
    let tx: rusqlite::Transaction<'_> = crate::runtime::sqlite_pool::begin_immediate(&conn)?;
    let identity: Option<(String, String)> = tx
        .query_row(
            "SELECT workspace,owner_id FROM sync_meta WHERE owner_id IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((workspace, owner)) = identity else {
        return Ok(());
    };
    let mut agents = config
        .agents
        .iter()
        .map(|(name, agent)| {
            Ok((
                name.clone(),
                crate::config::agent_revision::config_revision(agent)?,
                agent.enabled,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    agents.push((
        "main".into(),
        hex::encode(Sha256::digest(serde_json::to_vec(&(
            config.default_provider.as_deref(),
            config.default_model.as_deref(),
            &config.autonomy,
            config.default_temperature,
            &config.tool_tiering,
        ))?)),
        true,
    ));
    let live: BTreeSet<String> = agents.iter().map(|(name, _, _)| name.clone()).collect();
    let mut q = tx.prepare("SELECT agent,fingerprint FROM sync_agent_state WHERE owner_id=?1 AND deleted=0")?;
    let old = q
        .query_map([&owner], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(q);
    for (name, fingerprint) in old {
        if !live.contains(&name) {
            agents.push((name, fingerprint, false));
        }
    }
    for (name, fingerprint, enabled) in agents {
        let deleted = !live.contains(&name);
        let key = if name == "main" {
            "m".into()
        } else {
            format!("n:{name}")
        };
        let prior: Option<(String, String, bool, bool)> = tx
            .query_row(
                "SELECT incarnation,fingerprint,enabled,deleted FROM sync_agent_state WHERE owner_id=?1 AND agent=?2",
                params![owner, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let incarnation: Option<String> = if deleted {
            prior.as_ref().map(|(inc, _, _, _)| inc.clone())
        } else {
            tx.query_row("SELECT incarnation FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 ORDER BY state='active' DESC LIMIT 1",params![workspace,owner,key],|r|r.get(0)).optional()?
        };
        let incarnation = if let Some(incarnation) = incarnation {
            incarnation
        } else if deleted {
            continue;
        } else {
            let incarnation = if name == "main" {
                super::agent_migration::main_incarnation(&tx, &workspace, &owner)?
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            tx.execute(
                "INSERT OR IGNORE INTO agent_incarnations VALUES(?1,?2,?3,?4,'active',?5)",
                params![workspace, owner, key, incarnation, fingerprint],
            )?;
            incarnation
        };
        if prior.as_ref().is_some_and(|(inc, fp, on, gone)| {
            inc == &incarnation && fp == &fingerprint && *on == enabled && *gone == deleted
        }) {
            continue;
        }
        let source = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO sync_config_sources VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                source,
                owner,
                name,
                incarnation,
                if deleted { None } else { Some(revision) },
                enabled,
                deleted
            ],
        )?;
        tx.execute("INSERT INTO sync_agent_state(owner_id,agent,incarnation,revision,enabled,deleted,fingerprint) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(owner_id,agent) DO UPDATE SET incarnation=excluded.incarnation,revision=excluded.revision,enabled=excluded.enabled,deleted=excluded.deleted,fingerprint=excluded.fingerprint",params![owner,name,incarnation,revision,enabled,deleted,fingerprint])?;
        append(
            &tx,
            &Header::new(
                &owner,
                "agent.config_changed",
                "",
                &name,
                &incarnation,
                format!("config:{source}"),
            ),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Ingress recovery wraps the source payload; project only the job identity.
pub(super) fn scheduled_metadata(raw: Option<&str>) -> anyhow::Result<Value> {
    let mut value: Value = raw.map(serde_json::from_str).transpose()?.unwrap_or_default();
    if let Some(original) = value.get("original_payload_json").and_then(Value::as_str) {
        value = serde_json::from_str(original)?;
    }
    Ok(json!({"job_id":value.get("job_id"),"job_version":value.get("job_version")}))
}

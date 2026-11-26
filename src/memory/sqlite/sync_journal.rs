//! Transactional header append and owner-scoped commit notification.
use super::SqliteMemory;
use crate::memory::sync::{JournalPage, RestartCandidate, SyncStore};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Runtime {
    pub notify: Arc<Notify>,
    shutdown: Mutex<CancellationToken>,
    connections: Mutex<BTreeMap<String, Arc<CancellationToken>>>,
}

pub(super) fn for_database(path: &std::path::Path) -> anyhow::Result<Arc<Runtime>> {
    type Registry = Mutex<BTreeMap<std::path::PathBuf, std::sync::Weak<Runtime>>>;
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    let path = std::fs::canonicalize(path)?;
    let mut entries = REGISTRY.get_or_init(Mutex::default).lock();
    entries.retain(|_, runtime| runtime.strong_count() > 0);
    if let Some(runtime) = entries.get(&path).and_then(std::sync::Weak::upgrade) {
        return Ok(runtime);
    }
    let runtime = Arc::new(Runtime::default());
    entries.insert(path, Arc::downgrade(&runtime));
    Ok(runtime)
}

/// The hook runs under the sole writer guard. Journal readers acquire that
/// same guard, so a notification can never race ahead of the visible commit.
pub(super) fn install_notify(conn: &mut Connection, runtime: &Arc<Runtime>) -> anyhow::Result<()> {
    let notify = Arc::clone(&runtime.notify);
    conn.commit_hook(Some(move || {
        notify.notify_one();
        false
    }))?;
    Ok(())
}

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    let existed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='sync_journal' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS sync_meta(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),epoch TEXT NOT NULL,
        floor INTEGER NOT NULL DEFAULT 0,last_prune INTEGER NOT NULL DEFAULT 0,
        workspace TEXT,owner_id TEXT) STRICT;
        CREATE TABLE IF NOT EXISTS sync_journal(
        seq INTEGER PRIMARY KEY AUTOINCREMENT,event_id TEXT NOT NULL UNIQUE,
        owner_id TEXT NOT NULL,kind TEXT NOT NULL,session TEXT NOT NULL,
        agent TEXT NOT NULL,incarnation TEXT NOT NULL,run_id TEXT,
        parent_session TEXT,parent_run_id TEXT,root_session TEXT,root_run_id TEXT,
        source_ref TEXT NOT NULL,created_at INTEGER NOT NULL,source_created_at TEXT NOT NULL,
        CHECK((parent_session IS NULL AND parent_run_id IS NULL AND root_session IS NULL AND root_run_id IS NULL)
           OR (parent_session IS NOT NULL AND parent_run_id IS NOT NULL AND root_session IS NOT NULL AND root_run_id IS NOT NULL))) STRICT;
        CREATE INDEX IF NOT EXISTS sync_journal_owner ON sync_journal(owner_id,seq);
        CREATE TABLE IF NOT EXISTS sync_agent_state(
        owner_id TEXT NOT NULL,agent TEXT NOT NULL,incarnation TEXT NOT NULL,
        revision TEXT,enabled INTEGER NOT NULL,deleted INTEGER NOT NULL,fingerprint TEXT NOT NULL,
        PRIMARY KEY(owner_id,agent)) STRICT;
        CREATE TABLE IF NOT EXISTS sync_config_sources(
        id TEXT PRIMARY KEY,owner_id TEXT NOT NULL,agent TEXT NOT NULL,
        incarnation TEXT NOT NULL,revision TEXT,enabled INTEGER NOT NULL,deleted INTEGER NOT NULL) STRICT;")?;
    conn.execute(
        "INSERT OR IGNORE INTO sync_meta(singleton,epoch) VALUES(1,?1)",
        [uuid::Uuid::new_v4().simple().to_string()],
    )?;
    super::sync_snapshot::init_schema(conn)?;
    super::sync_retained::init_schema(conn)?;
    if !existed {
        conn.execute(
            "UPDATE sync_meta SET epoch=?1,floor=0,last_prune=0",
            [uuid::Uuid::new_v4().simple().to_string()],
        )?;
        conn.execute("DELETE FROM sync_snapshot_rows", [])?;
        conn.execute("DELETE FROM sync_snapshots", [])?;
        conn.execute("DELETE FROM sync_agent_state", [])?;
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Header {
    pub seq: i64,
    pub event_id: String,
    pub owner: String,
    pub kind: String,
    pub session: String,
    pub agent: String,
    pub incarnation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_run_id: Option<String>,
    pub source_ref: String,
    pub timestamp: String,
}
impl Header {
    pub fn new(owner: &str, kind: &str, session: &str, agent: &str, incarnation: &str, source_ref: String) -> Self {
        Self {
            seq: 0,
            event_id: format!("{kind}:{source_ref}"),
            owner: owner.into(),
            kind: kind.into(),
            session: session.into(),
            agent: agent.into(),
            incarnation: incarnation.into(),
            run_id: None,
            parent_session: None,
            parent_run_id: None,
            root_session: None,
            root_run_id: None,
            source_ref,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }
    pub fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            seq: row.get(0)?,
            event_id: row.get(1)?,
            owner: row.get(2)?,
            kind: row.get(3)?,
            session: row.get(4)?,
            agent: row.get(5)?,
            incarnation: row.get(6)?,
            run_id: row.get(7)?,
            parent_session: row.get(8)?,
            parent_run_id: row.get(9)?,
            root_session: row.get(10)?,
            root_run_id: row.get(11)?,
            source_ref: row.get(12)?,
            timestamp: row.get(13)?,
        })
    }
}
pub(super) const COLUMNS: &str = "seq,event_id,owner_id,kind,session,agent,incarnation,run_id,parent_session,parent_run_id,root_session,root_run_id,source_ref,source_created_at";

pub(super) fn append(conn: &Connection, h: &Header) -> anyhow::Result<()> {
    anyhow::ensure!(!conn.is_autocommit(), crate::memory::sync::SyncError::InvalidSource);
    let written=conn.execute("INSERT INTO sync_journal(event_id,owner_id,kind,session,agent,incarnation,run_id,parent_session,parent_run_id,root_session,root_run_id,source_ref,created_at,source_created_at)
        SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14 WHERE NOT EXISTS(SELECT 1 FROM sync_journal WHERE event_id=?1)",params![h.event_id,h.owner,h.kind,h.session,h.agent,h.incarnation,h.run_id,h.parent_session,h.parent_run_id,h.root_session,h.root_run_id,h.source_ref,chrono::Utc::now().timestamp(),h.timestamp])?;
    if written == 0 {
        let mut stored = conn.query_row(
            &format!("SELECT {COLUMNS} FROM sync_journal WHERE event_id=?1"),
            [&h.event_id],
            Header::row,
        )?;
        stored.seq = h.seq;
        stored.timestamp = h.timestamp.clone();
        anyhow::ensure!(stored == *h, crate::memory::sync::SyncError::InvalidSource);
    }
    Ok(())
}

/// Resolve descendants through durable ancestry to the rf admission, whose
/// partition is the root's purge stamp. Never relabel from current config.
pub(super) fn event_header(conn: &Connection, id: &str, kind: &str) -> anyhow::Result<Option<Header>> {
    let event:Option<(String,String,Option<String>,Option<String>,String,String)>=conn.query_row(
        "SELECT workspace_id,session_key,run_id,parent_run_id,owner_id,created_at FROM message_events WHERE event_id=?1 AND owner_id IS NOT NULL AND session_key IS NOT NULL",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((workspace, session, run, parent, owner, timestamp)) = event else {
        return Ok(None);
    };
    // Prefer the root already recorded by WriteProvenance, then traverse
    // event ancestry before the child's first attributed effect exists.
    let provenance_root:Option<String>=conn.query_row("SELECT root_run_id FROM memory_write_log WHERE run_id=?1 AND agent_partition IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?2 AND owner_id=?3) ORDER BY id LIMIT 1",params![run,workspace,owner],|r|r.get(0)).optional()?;
    let root_run = provenance_root.as_deref().or(parent.as_deref()).or(run.as_deref());
    let root:Option<(String,String,String,Option<String>,Option<String>)>=conn.query_row(
        "WITH RECURSIVE family(run) AS (SELECT ?1 UNION SELECT e.parent_run_id FROM message_events e JOIN family f ON e.run_id=f.run WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.parent_run_id IS NOT NULL)
        SELECT a.session_key,a.agent_key,s.incarnation,a.run_id,s.session_key FROM rf_message_attempts a JOIN agent_scopes s ON s.scope_id=a.ma_scope_id WHERE a.workspace_id=?2 AND a.owner_id=?3 AND (a.run_id IN(SELECT run FROM family) OR a.message_event_id=?4) LIMIT 1",
        params![root_run,workspace,owner,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let root = if root.is_some() {
        root
    } else {
        conn.query_row("WITH RECURSIVE family(run) AS (SELECT ?1 UNION SELECT e.parent_run_id FROM message_events e JOIN family f ON e.run_id=f.run WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.parent_run_id IS NOT NULL) SELECT e.session_key,s.agent_key,s.incarnation,e.run_id,s.session_key FROM message_events e JOIN agent_scopes s ON s.scope_id=e.ma_scope_id WHERE e.workspace_id=?2 AND e.owner_id=?3 AND s.agent_key GLOB 'n:*' AND e.source IN ('cron','xin') AND e.parent_run_id IS NULL AND e.run_id IN(SELECT run FROM family) LIMIT 1", params![root_run, workspace, owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?
    };
    // A message-woken run is its own root in the receiver session: a top-level
    // run whose continuation input names the adoption row of its message.
    let root = if root.is_some() {
        root
    } else {
        conn.query_row(&format!("WITH RECURSIVE family(run) AS (SELECT ?1 UNION SELECT e.parent_run_id FROM message_events e JOIN family f ON e.run_id=f.run WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.parent_run_id IS NOT NULL) SELECT e.session_key,CASE WHEN s.agent_key='m' THEN 'main' ELSE s.agent_key END,s.incarnation,e.run_id,s.session_key FROM message_events e JOIN agent_scopes s ON s.scope_id=e.ma_scope_id WHERE e.workspace_id=?2 AND e.owner_id=?3 AND e.parent_run_id IS NULL AND e.run_id IN(SELECT run FROM family) AND {} LIMIT 1", super::mailbox_continuation::CONTINUATION_PREDICATE_E), params![root_run, workspace, owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?
    };
    let Some((root_session, agent, incarnation, root_id, _)) = root else {
        return Ok(None);
    };
    let agent = agent.strip_prefix("n:").unwrap_or(&agent).to_owned();
    let mut h = Header::new(&owner, kind, &session, &agent, &incarnation, format!("event:{id}"));
    h.run_id = run;
    h.timestamp = timestamp;

    if let Some(parent) = parent {
        let parent_session:Option<String>=conn.query_row("SELECT session_key FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND run_id=?3 AND session_key IS NOT NULL LIMIT 1",params![workspace,owner,parent],|r|r.get(0)).optional()?;
        h.parent_session = Some(parent_session.unwrap_or_else(|| root_session.clone()));
        h.parent_run_id = Some(parent);
        h.root_session = Some(root_session);
        h.root_run_id = Some(root_id.ok_or_else(|| crate::memory::sync::SyncError::InvalidSource)?);
    }
    Ok(Some(h))
}

pub(super) fn event_written(conn: &Connection, event: &crate::memory::MessageEvent) -> anyhow::Result<()> {
    // Tool call and tool result rows are log-only: the stream never carries them.
    if event.role == "tool" {
        return Ok(());
    }
    let Some(mut h) = event_header(conn, &event.event_id, "turn.appended")? else {
        return Ok(());
    };
    if event.event_type == "approval.call.pending" || event.event_type == "tool.approval.resolved" {
        let raw: serde_json::Value = event
            .raw_payload_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let kind =
            if event.event_type == "approval.call.pending" && raw.pointer("/record/rf").is_some_and(|v| !v.is_null()) {
                Some("approval.pending")
            } else if event.event_type == "tool.approval.resolved" && raw.get("approval_id").is_some() {
                Some("approval.decided")
            } else {
                None
            };
        if let Some(kind) = kind {
            h.kind = kind.into();
            if kind == "approval.pending" {
                let authority: crate::security::approval_ledger::keys::RunCallAuthority = serde_json::from_value(
                    raw.pointer("/record/authority")
                        .cloned()
                        .ok_or(crate::memory::sync::SyncError::InvalidSource)?,
                )?;
                h.event_id = format!("approval-pending:{}", authority.call_id());
            }
            if kind == "approval.decided" {
                if let Some(id) = raw.get("approval_id").and_then(serde_json::Value::as_str) {
                    h.event_id = format!("approval-decided:{id}");
                }
            }
            append(conn, &h)?;
        }
        return Ok(());
    }
    let scheduled_root = h.parent_run_id.is_none()
        && matches!(
            event.source,
            crate::memory::MessageEventSource::Cron | crate::memory::MessageEventSource::Xin
        );
    if scheduled_root && (event.role == "user" || event.event_type == "scheduled.skipped") {
        h.kind = "session.upsert".into();
        h.event_id = format!("session:{}", event.event_id);
        append(conn, &h)?;
        if event.role == "user" {
            project_scheduled(conn, &event.event_id)?;
        } else {
            conn.execute("INSERT INTO sessions(session_key,channel,sender,owner_id,created_at,updated_at) VALUES(?1,?2,?5,?3,?4,?4) ON CONFLICT(session_key) DO UPDATE SET updated_at=excluded.updated_at WHERE sessions.owner_id=excluded.owner_id",params![h.session,event.source.as_str(),h.owner,event.created_at,h.agent])?;
        }
        h.kind = if event.event_type == "scheduled.skipped" {
            "turn.failed"
        } else {
            "turn.started"
        }
        .into();
        h.event_id = format!("{}:{}", h.kind, event.event_id);
        append(conn, &h)?;
        h.kind = "turn.appended".into();
        h.event_id = format!("turn.appended:{}", event.event_id);
        if event.role == "user" {
            append(conn, &h)?;
        }
    }
    // An event of a session written on behalf of that same session (the batch
    // membership journal of a root run) carries the run's parent link but is
    // not a child session.
    let own_session = event.parent_session_key.is_some() && event.parent_session_key == event.session_key;
    // Root user rows are appended by admit, after their attempt is inserted.
    if h.parent_run_id.is_some() && !own_session {
        let first: bool = conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sync_journal WHERE owner_id=?1 AND session=?2 AND kind='session.upsert')",
            params![h.owner, h.session],
            |r| r.get(0),
        )?;
        if first {
            h.kind = "session.upsert".into();
            h.event_id = format!("session:{}", event.event_id);
            append(conn, &h)?;
            h.kind = "child_session.upsert".into();
            h.event_id = format!("child-start:{}", event.event_id);
            append(conn, &h)?;
        }
    }
    if event.role == "user"
        && (h.parent_run_id.is_some() || event.source.as_str() == "prx_mailbox")
        && !super::mailbox_continuation::is_continuation(conn, event)?
        && !super::mailbox_continuation::is_task_echo(conn, &event.event_id)?
    {
        h.kind = "turn.appended".into();
        h.event_id = format!("turn.appended:{}", h.source_ref);
        append(conn, &h)?;
    }
    if event.event_type == "turn.finalized" || event.event_type == "turn.admission.failed" {
        let payload: serde_json::Value = event
            .raw_payload_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let status = payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("failed");
        let kind = match status {
            "completed" | "succeeded" | "silent" => None,
            "cancelled" => Some("turn.cancelled"),
            _ => Some("turn.failed"),
        };
        // A turn the parent already closed after its writer died keeps that
        // terminal; the real marker adds none.
        let closed: bool = match &h.run_id {
            Some(run) => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_journal WHERE event_id=?1)",
                [super::sync_lifecycle::open_terminal_id(run)],
                |r| r.get(0),
            )?,
            None => false,
        };
        if let Some(kind) = kind.filter(|_| !closed) {
            h.kind = kind.into();
            h.event_id = format!("terminal:{}", event.event_id);
            append(conn, &h)?;
        }
        if let Some(assistant) = payload.get("assistant_event_id").and_then(serde_json::Value::as_str) {
            if let Some(mut header) = event_header(conn, assistant, "turn.appended")? {
                // The final marker is the publication source, including on
                // paths whose assistant row was prepared in an earlier tx.
                header.source_ref = format!("final:{}:{assistant}", event.event_id);
                append(conn, &header)?;
                if scheduled_root {
                    project_scheduled(conn, assistant)?;
                }
            }
        }
        if let Some(run) = &event.run_id {
            super::sync_lifecycle::cancel_approvals(conn, &h, &event.workspace_id, run, status == "cancelled")?;
        }
        // The end of the child session is its run's terminal. A segment's own
        // terminal ends one turn only, and a run that already has a terminal
        // upsert (from its `task.*` event) keeps that one.
        if let Some(run) = h.run_id.clone().filter(|_| h.parent_run_id.is_some() && !own_session) {
            if !super::sync_lifecycle::is_segment_run(&run)
                && !super::sync_lifecycle::terminal_upsert_exists(conn, &h.owner, &run)?
            {
                h.kind = "child_session.upsert".into();
                h.event_id = format!("child-end:{}", event.event_id);
                append(conn, &h)?;
            }
        }
    }
    Ok(())
}

/// Transcript projection shares the source/journal transaction and its erase stamp.
fn project_scheduled(conn: &Connection, event: &str) -> anyhow::Result<()> {
    conn.execute("INSERT INTO sessions(session_key,channel,sender,owner_id,created_at,updated_at) SELECT session_key,source,(SELECT substr(agent_key,3) FROM agent_scopes WHERE scope_id=ma_scope_id),owner_id,created_at,created_at FROM message_events WHERE event_id=?1 ON CONFLICT(session_key) DO UPDATE SET updated_at=excluded.updated_at WHERE sessions.owner_id=excluded.owner_id AND sessions.channel=excluded.channel",[event])?;
    conn.execute("INSERT INTO conversation_turns(session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,persona_id,visibility,ma_scope_id) SELECT session_key,owner_id,role,content,created_at,event_id,event_id,agent_id,persona_id,visibility,ma_scope_id FROM message_events WHERE event_id=?1 AND NOT EXISTS(SELECT 1 FROM conversation_turns WHERE message_event_id=?1)",[event])?;
    conn.execute("UPDATE sessions SET message_count=(SELECT COUNT(*) FROM conversation_turns t WHERE t.session_key=sessions.session_key AND t.owner_id=sessions.owner_id),last_message_preview=(SELECT substr(content,1,120) FROM message_events WHERE event_id=?1) WHERE session_key=(SELECT session_key FROM message_events WHERE event_id=?1) AND owner_id=(SELECT owner_id FROM message_events WHERE event_id=?1)",[event])?;
    Ok(())
}

pub(super) fn admission(
    conn: &Connection,
    admission: &crate::memory::RfAdmission,
    event: &crate::memory::MessageEvent,
) -> anyhow::Result<()> {
    let k = &admission.key;
    let incarnation:String=conn.query_row("SELECT s.incarnation FROM message_events e JOIN agent_scopes s ON s.scope_id=e.ma_scope_id WHERE e.event_id=?1",[&event.event_id],|r|r.get(0))?;
    let mut h = Header::new(
        &k.owner_id,
        "turn.started",
        &k.session_key,
        &k.agent_key,
        &incarnation,
        format!("attempt:{}", event.event_id),
    );
    h.run_id = admission.run.as_ref().map(|r| r.run_id.clone());
    h.timestamp = event.created_at.clone();
    if h.run_id.is_some() {
        append(conn, &h)?;
    }
    if super::mailbox_continuation::is_continuation(conn, event)? {
        return Ok(());
    }
    h.kind = "turn.appended".into();
    h.source_ref = format!("event:{}", event.event_id);
    h.event_id = format!("turn.appended:{}", h.source_ref);
    append(conn, &h)
}

pub(super) fn session_created(conn: &Connection, session: &str, owner: &str) -> anyhow::Result<()> {
    let identity:Option<(String,String)>=conn.query_row("SELECT m.workspace,a.incarnation FROM sync_meta m JOIN agent_incarnations a ON a.workspace_id=m.workspace AND a.owner_id=?1 AND a.agent_key='m' AND a.state='active' WHERE m.owner_id=?1",[owner],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((_, incarnation)) = identity {
        append(
            conn,
            &Header::new(
                owner,
                "session.upsert",
                session,
                "main",
                &incarnation,
                format!("session:{session}"),
            ),
        )?;
    }
    Ok(())
}

pub(super) fn interrupted(conn: &Connection, workspace: &str, owner: &str, run: &str) -> anyhow::Result<()> {
    let id: Option<String> = conn
        .query_row(
            "SELECT message_event_id FROM rf_message_attempts WHERE workspace_id=?1 AND owner_id=?2 AND run_id=?3",
            params![workspace, owner, run],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        if let Some(mut h) = event_header(conn, &id, "turn.failed")? {
            h.source_ref = format!("interrupt:{run}");
            h.event_id = format!("interrupt:{run}");
            append(conn, &h)?;
            super::sync_lifecycle::cancel_approvals(conn, &h, workspace, run, true)?;
        }
    }
    Ok(())
}

pub(super) fn prune(conn: &Connection) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute("UPDATE sync_meta SET floor=MAX(floor,COALESCE((SELECT MAX(seq) FROM sync_journal WHERE created_at<?1 AND seq<COALESCE((SELECT MIN(seq) FROM sync_journal WHERE created_at>=?1),9223372036854775807)),0)),last_prune=?2 WHERE last_prune<?2-3600",params![now-14*86400,now])?;
    conn.execute("DELETE FROM sync_journal WHERE seq<=(SELECT floor FROM sync_meta)", [])?;
    Ok(())
}

impl SyncStore for SqliteMemory {
    fn session_metadata(&self, owner: &str, session: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let reader = self.pool.read()?;
        let conn = reader.connection()?;
        let raw:Option<String>=conn.query_row("SELECT raw_payload_json FROM message_events WHERE owner_id=?1 AND session_key=?2 AND source IN ('cron','xin') AND parent_run_id IS NULL AND (role='user' OR event_type='scheduled.skipped') ORDER BY id DESC LIMIT 1",params![owner,session],|r|r.get(0)).optional()?.flatten();
        raw.as_deref()
            .map(|raw| super::sync_projection::scheduled_metadata(Some(raw)))
            .transpose()
    }

    fn live_scope(&self, owner: &str, event: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let reader = self.pool.read()?;
        let conn = reader.connection()?;
        let Some(h) = event_header(&conn, event, "stream")? else {
            return Ok(None);
        };
        anyhow::ensure!(h.owner == owner, crate::memory::sync::SyncError::InvalidSource);
        // Descendant headers retain the root's agent and incarnation. Its
        // approval surface belongs to that root even after its turn ended.
        let session = h.root_session.as_ref().unwrap_or(&h.session);
        let run = h.root_run_id.as_ref().or(h.run_id.as_ref());
        Ok(Some(
            serde_json::json!({"session":session,"agent":h.agent,"incarnation":h.incarnation,"run_id":run}),
        ))
    }

    fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.sync_runtime.notify)
    }
    fn connect(&self, owner: &str) -> Arc<CancellationToken> {
        let token = Arc::new(CancellationToken::new());
        let old = self
            .sync_runtime
            .connections
            .lock()
            .insert(owner.into(), Arc::clone(&token));
        if let Some(old) = old {
            old.cancel();
        }
        token
    }
    fn disconnect(&self, owner: &str, token: &Arc<CancellationToken>) {
        let mut slots = self.sync_runtime.connections.lock();
        if slots.get(owner).is_some_and(|old| Arc::ptr_eq(old, token)) {
            slots.remove(owner);
        }
    }
    fn journal(&self, owner: &str, after: i64, limit: usize) -> anyhow::Result<JournalPage> {
        let writer = self.pool.write();
        let last: i64 = writer.query_row("SELECT last_prune FROM sync_meta", [], |r| r.get(0))?;
        if chrono::Utc::now().timestamp() - last >= 3600 {
            let tx = crate::runtime::sqlite_pool::begin_immediate(&writer)?;
            prune(&tx)?;
            tx.commit()?;
        }
        let tx = rusqlite::Transaction::new_unchecked(&writer, rusqlite::TransactionBehavior::Deferred)?;
        let (epoch, floor, head) = tx.query_row(
            "SELECT epoch,floor,MAX(floor,COALESCE((SELECT MAX(seq) FROM sync_journal),0)) FROM sync_meta",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )?;
        let mut query = tx.prepare(&format!(
            "SELECT {COLUMNS} FROM sync_journal WHERE owner_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3"
        ))?;
        let headers = query
            .query_map(params![owner, after, i64::try_from(limit.min(256))?], Header::row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        let events = headers
            .iter()
            .map(|h| super::sync_projection::event(&tx, h))
            .collect::<anyhow::Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(JournalPage {
            epoch,
            head,
            floor,
            events,
        })
    }
    fn snapshot(
        &self,
        owner: &str,
        token: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<crate::memory::sync::SnapshotPage> {
        super::sync_snapshot::page(self, owner, token, limit)
    }
    fn configure(&self, workspace: &str, owner: &str) -> anyhow::Result<()> {
        let writer = self.pool.write();
        let conn: &Connection = &writer;
        conn.execute(
            "UPDATE sync_meta SET workspace=?1,owner_id=?2 WHERE singleton=1",
            params![workspace, owner],
        )?;
        let previous = std::mem::replace(&mut *self.sync_runtime.shutdown.lock(), CancellationToken::new());
        previous.cancel();
        Ok(())
    }
    fn refresh_config(&self, config: &crate::config::Config, revision: &str) -> anyhow::Result<()> {
        super::sync_projection::refresh_config(self, config, revision)
    }
    fn restart_candidates(&self, after: &str) -> anyhow::Result<Vec<RestartCandidate>> {
        let conn = self.pool.write();
        let mut q=conn.prepare("SELECT workspace_id,owner_id,run_id,lifetime FROM rf_message_attempts a WHERE run_id>?1 AND interrupted=0 AND NOT EXISTS(SELECT 1 FROM message_events e WHERE e.workspace_id=a.workspace_id AND e.owner_id=a.owner_id AND e.idempotency_key IN('turn:'||a.run_id||':final','turn:'||a.run_id||':admission-failed')) ORDER BY run_id LIMIT 128")?;
        Ok(q.query_map([after], |r| {
            Ok(RestartCandidate {
                workspace: r.get(0)?,
                owner: r.get(1)?,
                run_id: r.get(2)?,
                lifetime: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn cancel_orphaned_woken_approvals(&self) -> anyhow::Result<usize> {
        let conn = self.pool.write();
        super::sync_woken::sweep(&conn)
    }
    fn shutdown_signal(&self) -> CancellationToken {
        self.sync_runtime.shutdown.lock().clone()
    }
    fn trim_message_log(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        max_batches: u32,
    ) -> anyhow::Result<crate::memory::sync::MessageTrim> {
        super::message_retention::trim(self, cutoff, max_batches)
    }
}

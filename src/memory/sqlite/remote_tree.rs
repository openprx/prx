//! SQL for `prx_remote_trees` / `prx_remote_tree_resources` (C2).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::runtime::sqlite_pool::install_busy_handler;

/// Owner-qualified remote-tree key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteTreeId {
    /// Workspace that owns the tree.
    pub workspace_id: String,
    /// Authenticated owner.
    pub owner_id: String,
    /// `CausalChain.trigger` as text.
    pub trigger_message_id: String,
}

/// Resource charged against a remote tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteTreeResource {
    Spawned,
    Send,
    Scheduled,
    Connection,
}

impl RemoteTreeResource {
    /// Wire/error name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Send => "send",
            Self::Scheduled => "scheduled",
            Self::Connection => "connection",
        }
    }

    const fn occupancy_kind(self) -> Option<&'static str> {
        match self {
            Self::Spawned => Some("descendant"),
            Self::Scheduled => Some("scheduled"),
            Self::Connection => Some("connection"),
            Self::Send => None,
        }
    }
}

impl std::fmt::Display for RemoteTreeResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed remote-tree failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTreeError {
    /// Cap exceeded for this resource.
    Exhausted { resource: RemoteTreeResource },
    /// Tree was closed; resume and further charge are refused.
    Closed,
}

impl std::fmt::Display for RemoteTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted { resource } => write!(f, "remote_tree_resource_exhausted: {resource}"),
            Self::Closed => f.write_str("remote_tree_closed"),
        }
    }
}

impl std::error::Error for RemoteTreeError {}

/// Counters read after rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTreeSnapshot {
    /// Cumulative spawned descendants.
    pub spawned: i64,
    /// Cumulative outbound sends.
    pub sends: i64,
    /// Live scheduled occupancy.
    pub scheduled_live: i64,
    /// Live connection occupancy.
    pub connection_live: i64,
    /// Live descendant occupancy.
    pub descendant_live: i64,
    /// `open` or `closed`.
    pub state: String,
}

/// Idempotent DDL. Called from schema init.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS prx_remote_trees (
            workspace_id TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            trigger_message_id TEXT NOT NULL,
            ma_scope_id TEXT REFERENCES agent_scopes(scope_id),
            spawned INTEGER NOT NULL DEFAULT 0,
            sends INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL CHECK (state IN ('open','closed')),
            created_at INTEGER NOT NULL,
            closed_at INTEGER,
            PRIMARY KEY (workspace_id, owner_id, trigger_message_id)
        ) STRICT;
        CREATE TABLE IF NOT EXISTS prx_remote_tree_resources (
            workspace_id TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            trigger_message_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('scheduled','connection','descendant')),
            resource_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('live','gone')),
            PRIMARY KEY (workspace_id, owner_id, trigger_message_id, kind, resource_id),
            FOREIGN KEY (workspace_id, owner_id, trigger_message_id)
                REFERENCES prx_remote_trees(workspace_id, owner_id, trigger_message_id)
        ) STRICT;",
    )
    .context("create prx_remote_tree tables")?;
    Ok(())
}

/// Caps applied at charge time.
#[derive(Debug, Clone, Copy)]
pub struct RemoteTreeCaps {
    /// Spawned descendants (cumulative).
    pub max_spawned: u32,
    /// Scheduled occupancy.
    pub max_scheduled: u32,
    /// Live connection occupancy.
    pub max_connections: u32,
    /// Outbound sends (cumulative).
    pub max_sends: u32,
}

/// Charge `resource` against `id`.
pub fn charge(
    conn: &Connection,
    id: &RemoteTreeId,
    resource: RemoteTreeResource,
    resource_id: &str,
    ma_scope_id: Option<&str>,
    caps: RemoteTreeCaps,
) -> Result<(), RemoteTreeError> {
    ensure_open_tree(conn, id, ma_scope_id)?;
    match resource {
        RemoteTreeResource::Spawned => {
            bump_cumulative(conn, id, "spawned", i64::from(caps.max_spawned), resource)?;
            insert_live(conn, id, "descendant", resource_id)?;
        }
        RemoteTreeResource::Send => {
            bump_cumulative(conn, id, "sends", i64::from(caps.max_sends), resource)?;
        }
        RemoteTreeResource::Scheduled => {
            charge_occupancy(
                conn,
                id,
                "scheduled",
                resource_id,
                i64::from(caps.max_scheduled),
                resource,
            )?;
        }
        RemoteTreeResource::Connection => {
            charge_occupancy(
                conn,
                id,
                "connection",
                resource_id,
                i64::from(caps.max_connections),
                resource,
            )?;
        }
    }
    Ok(())
}

/// Refund occupancy. Cumulative counters are left unchanged.
pub fn refund(
    conn: &Connection,
    id: &RemoteTreeId,
    resource: RemoteTreeResource,
    resource_id: &str,
) -> Result<(), RemoteTreeError> {
    ensure_tree_not_closed(conn, id)?;
    if let Some(kind) = resource.occupancy_kind() {
        conn.execute(
            "UPDATE prx_remote_tree_resources
             SET state = 'gone'
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3
               AND kind = ?4 AND resource_id = ?5 AND state = 'live'",
            params![id.workspace_id, id.owner_id, id.trigger_message_id, kind, resource_id],
        )
        .map_err(|_| RemoteTreeError::Closed)?;
    }
    Ok(())
}

/// Load counters from durable rows.
pub fn rebuild(conn: &Connection, id: &RemoteTreeId) -> Result<RemoteTreeSnapshot> {
    snapshot(conn, id)
}

/// Close when no live occupancy remains and no resumable turn references the trigger.
pub fn close(conn: &Connection, id: &RemoteTreeId, has_resumable_turn: bool) -> Result<()> {
    let snap = snapshot(conn, id)?;
    if snap.state == "closed" {
        return Ok(());
    }
    if has_resumable_turn || snap.scheduled_live > 0 || snap.connection_live > 0 || snap.descendant_live > 0 {
        bail!("remote tree still has live occupancy or a resumable turn");
    }
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "UPDATE prx_remote_trees
         SET state = 'closed', closed_at = ?4
         WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3",
        params![id.workspace_id, id.owner_id, id.trigger_message_id, now],
    )?;
    Ok(())
}

/// Resume. Closed trees return [`RemoteTreeError::Closed`].
pub fn resume(conn: &Connection, id: &RemoteTreeId) -> Result<(), RemoteTreeError> {
    ensure_tree_not_closed(conn, id)
}

fn ensure_open_tree(conn: &Connection, id: &RemoteTreeId, ma_scope_id: Option<&str>) -> Result<(), RemoteTreeError> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM prx_remote_trees
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3",
            params![id.workspace_id, id.owner_id, id.trigger_message_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| RemoteTreeError::Closed)?;
    match state.as_deref() {
        Some("closed") => Err(RemoteTreeError::Closed),
        Some(_) => Ok(()),
        None => {
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO prx_remote_trees (
                    workspace_id, owner_id, trigger_message_id, ma_scope_id,
                    spawned, sends, state, created_at, closed_at
                 ) VALUES (?1, ?2, ?3, ?4, 0, 0, 'open', ?5, NULL)",
                params![id.workspace_id, id.owner_id, id.trigger_message_id, ma_scope_id, now],
            )
            .map_err(|_| RemoteTreeError::Closed)?;
            Ok(())
        }
    }
}

fn ensure_tree_not_closed(conn: &Connection, id: &RemoteTreeId) -> Result<(), RemoteTreeError> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM prx_remote_trees
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3",
            params![id.workspace_id, id.owner_id, id.trigger_message_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| RemoteTreeError::Closed)?;
    match state.as_deref() {
        Some("closed") => Err(RemoteTreeError::Closed),
        Some(_) => Ok(()),
        None => Err(RemoteTreeError::Closed),
    }
}

fn bump_cumulative(
    conn: &Connection,
    id: &RemoteTreeId,
    column: &str,
    cap: i64,
    resource: RemoteTreeResource,
) -> Result<(), RemoteTreeError> {
    // Column names are a closed set from this module, never caller input.
    let sql = match column {
        "spawned" => {
            "UPDATE prx_remote_trees SET spawned = spawned + 1
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3
               AND spawned < ?4"
        }
        "sends" => {
            "UPDATE prx_remote_trees SET sends = sends + 1
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3
               AND sends < ?4"
        }
        _ => return Err(RemoteTreeError::Closed),
    };
    let updated = conn
        .execute(sql, params![id.workspace_id, id.owner_id, id.trigger_message_id, cap])
        .map_err(|_| RemoteTreeError::Closed)?;
    if updated == 0 {
        return Err(RemoteTreeError::Exhausted { resource });
    }
    Ok(())
}

fn charge_occupancy(
    conn: &Connection,
    id: &RemoteTreeId,
    kind: &str,
    resource_id: &str,
    cap: i64,
    resource: RemoteTreeResource,
) -> Result<(), RemoteTreeError> {
    let live = live_count(conn, id, kind)?;
    if live >= cap {
        return Err(RemoteTreeError::Exhausted { resource });
    }
    insert_live(conn, id, kind, resource_id)
}

fn insert_live(conn: &Connection, id: &RemoteTreeId, kind: &str, resource_id: &str) -> Result<(), RemoteTreeError> {
    conn.execute(
        "INSERT INTO prx_remote_tree_resources (
            workspace_id, owner_id, trigger_message_id, kind, resource_id, state
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'live')
         ON CONFLICT(workspace_id, owner_id, trigger_message_id, kind, resource_id)
         DO UPDATE SET state = 'live'",
        params![id.workspace_id, id.owner_id, id.trigger_message_id, kind, resource_id],
    )
    .map_err(|_| RemoteTreeError::Closed)?;
    Ok(())
}

fn live_count(conn: &Connection, id: &RemoteTreeId, kind: &str) -> Result<i64, RemoteTreeError> {
    conn.query_row(
        "SELECT COUNT(*) FROM prx_remote_tree_resources
         WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3
           AND kind = ?4 AND state = 'live'",
        params![id.workspace_id, id.owner_id, id.trigger_message_id, kind],
        |row| row.get(0),
    )
    .map_err(|_| RemoteTreeError::Closed)
}

fn snapshot(conn: &Connection, id: &RemoteTreeId) -> Result<RemoteTreeSnapshot> {
    let (spawned, sends, state): (i64, i64, String) = conn
        .query_row(
            "SELECT spawned, sends, state FROM prx_remote_trees
             WHERE workspace_id = ?1 AND owner_id = ?2 AND trigger_message_id = ?3",
            params![id.workspace_id, id.owner_id, id.trigger_message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| {
            format!(
                "remote tree {}/{} /{}",
                id.workspace_id, id.owner_id, id.trigger_message_id
            )
        })?;
    Ok(RemoteTreeSnapshot {
        spawned,
        sends,
        scheduled_live: live_count(conn, id, "scheduled").map_err(|error| anyhow::anyhow!("{error}"))?,
        connection_live: live_count(conn, id, "connection").map_err(|error| anyhow::anyhow!("{error}"))?,
        descendant_live: live_count(conn, id, "descendant").map_err(|error| anyhow::anyhow!("{error}"))?,
        state,
    })
}

/// Open a connection with the same busy policy as the memory pool.
pub fn open_connection(path: &std::path::Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    install_busy_handler(&mut conn)?;
    Ok(conn)
}

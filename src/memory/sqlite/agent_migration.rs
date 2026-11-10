//! Legacy console history migration to Main (MA-01b1b round 1). It runs in the
//! schema-init transaction on every normal open and touches only proven
//! console history: NULL-agent rows of console message events, and of turns in
//! sessions whose channel is `console`. Nothing is guessed: a row whose
//! workspace, owner or session cannot be proven stays NULL and is counted.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::SqliteMemory;
use crate::memory::{AgentPartition, AgentScopeRefused, PartitionKey};

/// Main (K=`m`) never depends on config: its revision is this constant's hash.
const MAIN_REVISION_PREIMAGE: &str = "prx.ma.main.v1";

const EVENT_PENDING: &str = "e.agent_id IS NULL AND e.source = 'console' AND e.ma_scope_id IS NULL";

const TURN_PENDING: &str = "c.agent_id IS NULL AND c.ma_scope_id IS NULL AND EXISTS(
    SELECT 1 FROM sessions s WHERE s.session_key = c.session_key AND s.channel = 'console')";

/// A turn's workspace: its session's console message events, which must all
/// agree. NULL when there are none or they disagree.
const TURN_WORKSPACE: &str = "(SELECT CASE WHEN COUNT(DISTINCT m.workspace_id) = 1 THEN MIN(m.workspace_id) END
    FROM message_events m WHERE m.session_key = c.session_key AND m.source = 'console')";

/// Mirrors the `agent_scopes` CHECK bounds, so an out-of-range legacy value
/// stays NULL instead of failing the open.
fn bounded(col: &str, max: usize) -> String {
    format!("length(CAST({col} AS BLOB)) BETWEEN 1 AND {max} AND instr(CAST({col} AS BLOB), x'00') = 0")
}

/// The active Main incarnation of (W,O), minted on first use.
fn main_incarnation(conn: &Connection, workspace: &str, owner: &str) -> anyhow::Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT incarnation FROM agent_incarnations
             WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm' AND state = 'active'",
            params![workspace, owner],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(incarnation) = existing {
        return Ok(incarnation);
    }
    let incarnation = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'm', ?3, 'active', ?4)",
        params![
            workspace,
            owner,
            incarnation,
            SqliteMemory::schema_migration_checksum(MAIN_REVISION_PREIMAGE)
        ],
    )?;
    Ok(incarnation)
}

enum UnfencedScope {
    Ready(String),
    AllFenced,
    Absent,
}

fn inspect_unfenced_scope(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    session: &str,
    agent_key: &str,
    incarnation: &str,
) -> anyhow::Result<UnfencedScope> {
    let scopes = conn
        .prepare(
            "SELECT sc.scope_id, EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = sc.scope_id)
             FROM agent_scopes sc
             WHERE sc.workspace_id = ?1 AND sc.owner_id = ?2 AND sc.session_key = ?3
               AND sc.agent_key = ?4 AND sc.incarnation = ?5
             ORDER BY sc.generation",
        )?
        .query_map(params![workspace, owner, session, agent_key, incarnation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some((scope, _)) = scopes.iter().find(|(_, fenced)| !fenced) {
        return Ok(UnfencedScope::Ready(scope.clone()));
    }
    if scopes.is_empty() {
        Ok(UnfencedScope::Absent)
    } else {
        Ok(UnfencedScope::AllFenced)
    }
}

/// The one unfenced scope for (W,O,S,K,U). G=1 is inserted only when the tuple
/// has no scope at all; if every scope is fenced there is none to stamp.
fn unfenced_scope(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    session: &str,
    agent_key: &str,
    incarnation: &str,
) -> anyhow::Result<Option<String>> {
    match inspect_unfenced_scope(conn, workspace, owner, session, agent_key, incarnation)? {
        UnfencedScope::Ready(scope) => Ok(Some(scope)),
        UnfencedScope::AllFenced => Ok(None),
        UnfencedScope::Absent => {
            let scope = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![scope, workspace, owner, session, agent_key, incarnation],
            )?;
            Ok(Some(scope))
        }
    }
}

fn require_session(session_key: Option<&str>) -> anyhow::Result<&str> {
    session_key
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .ok_or_else(|| AgentScopeRefused.into())
}

/// Read-only lookup of the unfenced write scope. `Ok(None)` means it is safe
/// to mint (Main incarnation or a missing unfenced scope). Refusal is typed.
pub(super) fn lookup_write_scope(
    conn: &Connection,
    partition: &AgentPartition,
    session_key: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let session = require_session(session_key)?;
    let workspace = partition.workspace_id.as_str();
    let owner = partition.owner_id.as_str();
    match &partition.key {
        PartitionKey::Main => {
            let incarnation: Option<String> = conn
                .query_row(
                    "SELECT incarnation FROM agent_incarnations
                     WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = 'm' AND state = 'active'",
                    params![workspace, owner],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(incarnation) = incarnation else {
                return Ok(None);
            };
            match inspect_unfenced_scope(conn, workspace, owner, session, "m", &incarnation)? {
                UnfencedScope::Ready(scope) => Ok(Some(scope)),
                UnfencedScope::AllFenced => Err(AgentScopeRefused.into()),
                UnfencedScope::Absent => Ok(None),
            }
        }
        PartitionKey::Named { name, incarnation } => {
            let agent_key = format!("n:{}", name.as_str());
            let incarnation = incarnation.to_string();
            let state: Option<String> = conn
                .query_row(
                    "SELECT state FROM agent_incarnations
                     WHERE workspace_id = ?1 AND owner_id = ?2 AND agent_key = ?3 AND incarnation = ?4",
                    params![workspace, owner, &agent_key, &incarnation],
                    |row| row.get(0),
                )
                .optional()?;
            if state.as_deref() != Some("active") {
                return Err(AgentScopeRefused.into());
            }
            match inspect_unfenced_scope(conn, workspace, owner, session, &agent_key, &incarnation)? {
                UnfencedScope::Ready(scope) => Ok(Some(scope)),
                UnfencedScope::AllFenced => Err(AgentScopeRefused.into()),
                UnfencedScope::Absent => Ok(None),
            }
        }
    }
}

/// Get or create the unfenced write scope. Main mints the incarnation (CR1).
/// Named requires an active incarnation. Never falls back to NULL.
pub(super) fn resolve_write_scope(
    tx: &Transaction<'_>,
    partition: &AgentPartition,
    session_key: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(scope) = lookup_write_scope(tx, partition, session_key)? {
        return Ok(scope);
    }
    let session = require_session(session_key)?;
    let workspace = partition.workspace_id.as_str();
    let owner = partition.owner_id.as_str();
    let (agent_key, incarnation) = match &partition.key {
        PartitionKey::Main => ("m".to_owned(), main_incarnation(tx, workspace, owner)?),
        PartitionKey::Named { name, incarnation } => (format!("n:{}", name.as_str()), incarnation.to_string()),
    };
    unfenced_scope(tx, workspace, owner, session, &agent_key, &incarnation)?.ok_or_else(|| AgentScopeRefused.into())
}

/// Writer-facing resolve: partition `None` keeps NULL; workspace/owner mismatch
/// is refused.
pub(super) fn resolve_input_scope(
    tx: &Transaction<'_>,
    partition: Option<&AgentPartition>,
    session_key: Option<&str>,
    workspace_id: &str,
    owner_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(partition) = partition else {
        return Ok(None);
    };
    if partition.workspace_id != workspace_id || owner_id != Some(partition.owner_id.as_str()) {
        return Err(AgentScopeRefused.into());
    }
    resolve_write_scope(tx, partition, session_key).map(Some)
}

/// `BEGIN IMMEDIATE` for every stamped writer (appendix B).
pub(super) fn begin_immediate(conn: &Connection) -> anyhow::Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?)
}

/// Mint the write scope in a short committed transaction when lookup misses.
pub(super) fn lookup_or_mint_write_scope(
    conn: &Connection,
    partition: &AgentPartition,
    session_key: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(scope) = lookup_write_scope(conn, partition, session_key)? {
        return Ok(scope);
    }
    let tx = begin_immediate(conn)?;
    let scope = resolve_write_scope(&tx, partition, session_key)?;
    tx.commit()?;
    Ok(scope)
}

/// Guard G (H2): four-clause COALESCE predicate for stamped upserts.
pub(super) fn scope_conflict_guard(table: &str, has_where: bool) -> String {
    let predicate = format!(
        "({table}.ma_scope_id IS excluded.ma_scope_id OR EXISTS(SELECT 1 FROM agent_scopes o JOIN agent_scopes n ON n.scope_id=excluded.ma_scope_id WHERE o.scope_id={table}.ma_scope_id AND o.workspace_id=n.workspace_id AND o.owner_id=n.owner_id AND o.agent_key=n.agent_key AND o.incarnation=n.incarnation) OR ({table}.ma_scope_id IS NULL AND excluded.ma_scope_id IN (SELECT scope_id FROM agent_scopes WHERE agent_key='m')) OR (excluded.ma_scope_id IS NULL AND EXISTS(SELECT 1 FROM agent_scopes o WHERE o.scope_id={table}.ma_scope_id AND o.agent_key='m')))"
    );
    if has_where {
        format!(" AND {predicate}")
    } else {
        format!(" WHERE {predicate}")
    }
}

/// SET fragment that keeps a Main stamp when an unstamped writer overwrites it.
pub(super) fn scope_coalesce_set(table: &str) -> String {
    format!("ma_scope_id = COALESCE(excluded.ma_scope_id, {table}.ma_scope_id)")
}

/// After `INSERT OR IGNORE`, the stored stamp must equal the bound scope.
pub(super) fn refuse_if_stored_scope_differs(
    tx: &Transaction<'_>,
    table: &str,
    id_column: &str,
    id: &str,
    bound: &Option<String>,
) -> anyhow::Result<()> {
    let sql = match (table, id_column) {
        ("memory_events", "event_id") => "SELECT ma_scope_id FROM memory_events WHERE event_id = ?1",
        ("memory_drafts", "draft_id") => "SELECT ma_scope_id FROM memory_drafts WHERE draft_id = ?1",
        ("memory_links", "link_id") => "SELECT ma_scope_id FROM memory_links WHERE link_id = ?1",
        ("documents", "document_id") => "SELECT ma_scope_id FROM documents WHERE document_id = ?1",
        _ => anyhow::bail!("unknown stamped identity ({table}.{id_column})"),
    };
    let stored: Option<String> = tx.query_row(sql, params![id], |row| row.get(0))?;
    if stored != *bound {
        return Err(AgentScopeRefused.into());
    }
    Ok(())
}

/// Same incarnation, Main-over-NULL, or unstamped-over-Main (D4 / M2).
pub(super) fn scopes_compatible(conn: &Connection, stored: Option<&str>, writer: Option<&str>) -> anyhow::Result<bool> {
    match (stored, writer) {
        (None, None) => Ok(true),
        (Some(stored), Some(writer)) if stored == writer => Ok(true),
        (Some(stored), Some(writer)) => {
            let same: bool = conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM agent_scopes o
                     JOIN agent_scopes n ON n.scope_id = ?2
                     WHERE o.scope_id = ?1
                       AND o.workspace_id = n.workspace_id
                       AND o.owner_id = n.owner_id
                       AND o.agent_key = n.agent_key
                       AND o.incarnation = n.incarnation
                 )",
                params![stored, writer],
                |row| row.get(0),
            )?;
            Ok(same)
        }
        (None, Some(writer)) => {
            let is_main: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_scopes WHERE scope_id = ?1 AND agent_key = 'm')",
                params![writer],
                |row| row.get(0),
            )?;
            Ok(is_main)
        }
        (Some(stored), None) => {
            let is_main: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_scopes WHERE scope_id = ?1 AND agent_key = 'm')",
                params![stored],
                |row| row.get(0),
            )?;
            Ok(is_main)
        }
    }
}

/// Stamp legacy console history with its owner's Main scope. Returns how many
/// legacy console rows are still unattributed (unknown owner, workspace or
/// session, ambiguous workspace, or a fenced scope). Idempotent.
pub(super) fn migrate_legacy_console(conn: &Connection) -> anyhow::Result<u64> {
    let tuples = conn
        .prepare(&format!(
            "SELECT w, o, s FROM (
                 SELECT e.workspace_id AS w, e.owner_id AS o, e.session_key AS s
                 FROM message_events e WHERE {EVENT_PENDING}
                 UNION
                 SELECT {TURN_WORKSPACE}, c.owner_id, c.session_key
                 FROM conversation_turns c WHERE {TURN_PENDING}
             ) WHERE {} AND {} AND {}",
            bounded("w", 4096),
            bounded("o", 512),
            bounded("s", 512),
        ))?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (workspace, owner, session) in tuples {
        let incarnation = main_incarnation(conn, &workspace, &owner)?;
        let Some(scope) = unfenced_scope(conn, &workspace, &owner, &session, "m", &incarnation)? else {
            continue;
        };
        let events = conn
            .prepare(&format!(
                "SELECT e.event_id FROM message_events e
                 WHERE {EVENT_PENDING} AND e.workspace_id = ?1 AND e.owner_id = ?2 AND e.session_key = ?3"
            ))?
            .query_map(params![workspace, owner, session], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Receipts are verified against the original bytes before any mutation.
        let binding = super::recovery_authority::verify_main_binding(conn, &events)?;
        let blocked = serde_json::to_string(&binding.blocked)?;
        conn.execute_batch("SAVEPOINT ma_main_attribution")?;
        let stamped = (|| -> anyhow::Result<()> {
            conn.execute(
                &format!(
                    "UPDATE message_events AS e SET ma_scope_id = ?1
                     WHERE {EVENT_PENDING} AND e.workspace_id = ?2 AND e.owner_id = ?3 AND e.session_key = ?4
                       AND e.event_id NOT IN (SELECT value FROM json_each(?5))"
                ),
                params![scope, workspace, owner, session, blocked],
            )?;
            conn.execute(
                &format!(
                    "UPDATE conversation_turns AS c SET ma_scope_id = ?1
                     WHERE {TURN_PENDING} AND {TURN_WORKSPACE} = ?2 AND c.owner_id = ?3 AND c.session_key = ?4"
                ),
                params![scope, workspace, owner, session],
            )?;
            super::recovery_authority::rebind_main_binding(conn, &binding.verified)
        })();
        if let Err(error) = stamped {
            // Never keep a stamp whose receipt could not be rebound and re-verified.
            conn.execute_batch("ROLLBACK TO ma_main_attribution")?;
            tracing::warn!(reason = "recovery_rebind_failed", %error, "legacy console rows left unmigrated");
        }
        conn.execute_batch("RELEASE ma_main_attribution")?;
    }
    let left: i64 = conn.query_row(
        &format!(
            "SELECT (SELECT COUNT(*) FROM message_events e WHERE {EVENT_PENDING})
                  + (SELECT COUNT(*) FROM conversation_turns c WHERE {TURN_PENDING})"
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(left).unwrap_or(0))
}

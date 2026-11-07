//! Legacy console history migration to Main (MA-01b1b round 1). It runs in the
//! schema-init transaction on every normal open and touches only proven
//! console history: NULL-agent rows of console message events, and of turns in
//! sessions whose channel is `console`. Nothing is guessed: a row whose
//! workspace, owner or session cannot be proven stays NULL and is counted.

use rusqlite::{Connection, OptionalExtension, params};

use super::SqliteMemory;

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

/// The one unfenced Main scope for (W,O,S,U). G=1 is inserted only when the
/// tuple has no scope at all; if every scope is fenced there is none to stamp.
fn unfenced_scope(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    session: &str,
    incarnation: &str,
) -> anyhow::Result<Option<String>> {
    let scopes = conn
        .prepare(
            "SELECT sc.scope_id, EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = sc.scope_id)
             FROM agent_scopes sc
             WHERE sc.workspace_id = ?1 AND sc.owner_id = ?2 AND sc.session_key = ?3
               AND sc.agent_key = 'm' AND sc.incarnation = ?4
             ORDER BY sc.generation",
        )?
        .query_map(params![workspace, owner, session, incarnation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some((scope, _)) = scopes.iter().find(|(_, fenced)| !fenced) {
        return Ok(Some(scope.clone()));
    }
    if !scopes.is_empty() {
        return Ok(None);
    }
    let scope = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'm', ?5, 1)",
        params![scope, workspace, owner, session, incarnation],
    )?;
    Ok(Some(scope))
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
        let Some(scope) = unfenced_scope(conn, &workspace, &owner, &session, &incarnation)? else {
            continue;
        };
        conn.execute(
            &format!(
                "UPDATE message_events AS e SET ma_scope_id = ?1
                 WHERE {EVENT_PENDING} AND e.workspace_id = ?2 AND e.owner_id = ?3 AND e.session_key = ?4"
            ),
            params![scope, workspace, owner, session],
        )?;
        conn.execute(
            &format!(
                "UPDATE conversation_turns AS c SET ma_scope_id = ?1
                 WHERE {TURN_PENDING} AND {TURN_WORKSPACE} = ?2 AND c.owner_id = ?3 AND c.session_key = ?4"
            ),
            params![scope, workspace, owner, session],
        )?;
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

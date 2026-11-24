use crate::memory::traits::{
    ChatProfile, ConversationSessionSummary, ConversationTurn, MemoryEvent, MemoryEventInput, MemoryPrincipal,
    MessageEvent, MessageEventInput, PartitionKey, SessionContextQuery, SharedContextQuery,
};
use crate::runtime::sqlite_pool::begin_immediate;
use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::sync::Arc;
use uuid::Uuid;

/// The agent-scope column every partitioned store carries.
pub(crate) const MA_SCOPE_COLUMN: &str = "ma_scope_id";

/// One parenthesised conjunct on `column` after existing predicates, with its
/// bound values in placeholder order. Empty when `partition` is `None`.
///
/// Named: rows of exactly (workspace, owner, `n:<name>`, incarnation). Main:
/// unstamped rows and rows of a Main scope in the workspace. `placeholder`
/// yields the next bind marker (`?N` or `?`); `column` is a constant.
pub(crate) fn partition_conjunct(
    column: &str,
    partition: Option<&crate::memory::AgentPartition>,
    mut placeholder: impl FnMut() -> String,
) -> (String, Vec<String>) {
    let Some(partition) = partition else {
        return (String::new(), Vec::new());
    };
    match &partition.key {
        PartitionKey::Named { name, incarnation } => {
            let (workspace, owner, key, generation) = (placeholder(), placeholder(), placeholder(), placeholder());
            (
                format!(
                    " AND ({column} IN (SELECT scope_id FROM agent_scopes WHERE workspace_id={workspace} AND owner_id={owner} AND agent_key={key} AND incarnation={generation}))"
                ),
                vec![
                    partition.workspace_id.clone(),
                    partition.owner_id.clone(),
                    format!("n:{}", name.as_str()),
                    incarnation.to_string(),
                ],
            )
        }
        PartitionKey::Main => {
            let workspace = placeholder();
            (
                format!(
                    " AND ({column} IS NULL OR {column} IN (SELECT scope_id FROM agent_scopes WHERE workspace_id={workspace} AND agent_key='m'))"
                ),
                vec![partition.workspace_id.clone()],
            )
        }
    }
}

/// Numbered bind markers `?first`, `?first+1`, ...
pub(crate) fn numbered_placeholders(first: usize) -> impl FnMut() -> String {
    let mut next = first;
    move || {
        let marker = format!("?{next}");
        next += 1;
        marker
    }
}

/// [`partition_conjunct`] on `ma_scope_id` for the principal's partition.
pub(crate) fn agent_partition_clause(principal: &MemoryPrincipal, first_placeholder: usize) -> (String, Vec<String>) {
    partition_conjunct(
        MA_SCOPE_COLUMN,
        principal.agent_partition.as_ref(),
        numbered_placeholders(first_placeholder),
    )
}

pub(crate) const fn conversation_turns_ordering(recorded_order: bool) -> &'static str {
    if recorded_order {
        "julianday(timestamp) DESC, COALESCE((SELECT event.id FROM message_events event WHERE event.event_id = conversation_turns.message_event_id), id) DESC, id DESC"
    } else {
        "id DESC"
    }
}

/// The seventh column says whether the row is a deleted tombstone
/// (AC-1b2b). Only a caller-keyed history listing (`tombstones`) reads
/// tombstones; every other reader, model context included, skips them.
pub(crate) fn conversation_turns_select_sql(
    session_fragment: &str,
    partition_sql: &str,
    ordering: &str,
    tombstones: bool,
) -> String {
    let deleted_sql = if tombstones {
        ""
    } else {
        "\n                   AND deleted_at IS NULL"
    };
    format!(
        "SELECT id, session_key, role, content, timestamp, message_id, deleted_at IS NOT NULL, message_event_id
                 FROM conversation_turns
                 WHERE {session_fragment}
                   AND (
                       ?4 = 'system:*'
                       OR
                       owner_id = ?4
                       OR (?5 = 1 AND (owner_id IS NULL OR owner_id = 'legacy:' || session_key))
                   ){partition_sql}{deleted_sql}
                 ORDER BY {ordering}
                 LIMIT ?2 OFFSET ?3"
    )
}

pub(crate) fn message_events_since_select_sql(
    session_fragment: &str,
    owner_fragment: &str,
    owner_required_fragment: &str,
    partition_sql: &str,
) -> String {
    format!(
        "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE id > ?1
                   AND (
                       visibility = 'global'
                       OR (
                           workspace_id = ?2
                           AND (
                               visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?3 IS NOT NULL AND agent_id = ?3)
                                   OR (?4 IS NOT NULL AND persona_id = ?4)
                                   OR (?6 IS NOT NULL AND sender = ?6)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?7)
                           )
                       )
                   ){partition_sql}
                 ORDER BY id ASC
                 LIMIT ?8"
    )
}

pub(crate) struct MessageEventsSinceStatement {
    pub sql: String,
    pub partition_values: Vec<String>,
    pub legacy_session_keys: Vec<String>,
    pub bind_owner: bool,
}

/// Production conversation-turns statement, including the partition conjunct.
/// Returns `(sql, partition_values, legacy_session_keys)`.
#[cfg(test)]
pub(crate) fn conversation_turns_statement(
    principal: &MemoryPrincipal,
    session_key: &str,
    recorded_order: bool,
) -> (String, Vec<String>, Vec<String>) {
    conversation_turns_statement_with(principal, session_key, recorded_order, false)
}

/// [`conversation_turns_statement`], optionally reading deleted tombstones.
fn conversation_turns_statement_with(
    principal: &MemoryPrincipal,
    session_key: &str,
    recorded_order: bool,
    tombstones: bool,
) -> (String, Vec<String>, Vec<String>) {
    let legacy_session_keys: Vec<String> = principal
        .legacy_session_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty() && *key != session_key.trim())
        .map(|key| vec![key.to_string()])
        .unwrap_or_default();
    let mut session_indices = vec![1usize];
    for offset_idx in 0..legacy_session_keys.len() {
        session_indices.push(6 + offset_idx);
    }
    let session_fragment =
        crate::memory::session_predicate::session_key_match_fragment(super::SQLITE_DIALECT, &session_indices);
    let (partition_sql, partition_values) = agent_partition_clause(principal, 6 + legacy_session_keys.len());
    (
        conversation_turns_select_sql(
            &session_fragment.sql,
            &partition_sql,
            conversation_turns_ordering(recorded_order),
            tombstones,
        ),
        partition_values,
        legacy_session_keys,
    )
}

use super::ingress_recovery;
use super::{SQLITE_DIALECT, SqliteMemory};
use crate::memory::ingress_recovery::AtomicIngressMutation;

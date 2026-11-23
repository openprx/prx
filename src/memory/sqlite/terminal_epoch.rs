//! Process-local write epochs pinned by authenticated terminal sessions.
use super::{SqliteMemory, agent_migration};
use crate::memory::{AgentPartition, AgentScopeRefused, PartitionKey, principal::MemoryWriteContext};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::OnceLock};

type Key = (String, String, String);
static EPOCHS: OnceLock<parking_lot::Mutex<HashMap<Key, String>>> = OnceLock::new();
const MAX_EPOCHS: usize = 4096;

pub(crate) fn pin(memory: &SqliteMemory, partition: &AgentPartition, session: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(partition.key, PartitionKey::Main), AgentScopeRefused);
    let writer = memory.pool.write();
    let scope = agent_migration::lookup_or_mint_write_scope(&writer, partition, Some(session))?;
    let key = (
        partition.workspace_id.clone(),
        partition.owner_id.clone(),
        session.to_owned(),
    );
    let mut epochs = EPOCHS.get_or_init(Default::default).lock();
    if let Some(pinned) = epochs.get(&key) {
        anyhow::ensure!(pinned == &scope, AgentScopeRefused);
    } else {
        anyhow::ensure!(epochs.len() < MAX_EPOCHS, AgentScopeRefused);
        epochs.insert(key, scope);
    }
    Ok(())
}

/// Called inside each writing transaction, before resolving a replacement scope.
pub(super) fn check(
    conn: &Connection,
    workspace: &str,
    owner: Option<&str>,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let (Some(epochs), Some(owner), Some(session)) = (EPOCHS.get(), owner, session) else {
        return Ok(());
    };
    let scope = epochs
        .lock()
        .get(&(workspace.into(), owner.into(), session.into()))
        .cloned();
    if let Some(scope) = scope {
        let live: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_scopes s WHERE s.scope_id=?1 AND s.workspace_id=?2 AND s.owner_id=?3 AND s.session_key=?4 AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=s.scope_id))",
            params![scope,workspace,owner,session], |row|row.get(0))?;
        anyhow::ensure!(live, AgentScopeRefused);
    }
    Ok(())
}

/// Snapshot keys are runtime-authored; authority comes only from the pinned map.
pub(super) fn snapshot_context(
    path: &Path,
    key: &str,
    session: Option<&str>,
) -> anyhow::Result<Option<MemoryWriteContext>> {
    let Some(id) = key.strip_prefix("chat_session:") else {
        return Ok(None);
    };
    let Some(workspace) = path.parent().and_then(Path::parent).and_then(Path::to_str) else {
        return Ok(None);
    };
    let canonical = format!("chat:terminal:local-user:{id}");
    let Some(epochs) = EPOCHS.get() else {
        return Ok(None);
    };
    let bound = epochs
        .lock()
        .keys()
        .find(|(w, _, s)| w == workspace && s == &canonical)
        .cloned();
    let Some((workspace, owner, session_key)) = bound else {
        return Ok(None);
    };
    anyhow::ensure!(
        session.is_none_or(|session| session == id || session == session_key),
        AgentScopeRefused
    );
    Ok(Some(MemoryWriteContext {
        workspace_id: Some(workspace.clone()),
        owner_id: Some(owner.clone()),
        session_key: Some(session_key),
        agent_partition: Some(AgentPartition {
            workspace_id: workspace,
            owner_id: owner,
            key: PartitionKey::Main,
        }),
        ..MemoryWriteContext::default()
    }))
}

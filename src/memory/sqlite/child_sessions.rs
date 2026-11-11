//! Child-session blobs never enter the operator's memories/picker namespace.
use super::{SqliteMemory, mailbox};
use crate::memory::child_sessions::{ChildSessionContext, ChildSessionError, ChildSessionStore, ScopedChildRecord};
use crate::memory::{AgentPartition, PartitionKey};
use crate::messaging::mailbox::{MailboxScope, agent_key, bounded, validate_address};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS child_sessions (key TEXT PRIMARY KEY NOT NULL,record TEXT NOT NULL CHECK(json_valid(record))) STRICT;")?;
    Ok(())
}

fn validate_key(key: &str, context: &ChildSessionContext) -> anyhow::Result<()> {
    bounded(&context.workspace_id, 4096)?;
    validate_address(&context.addr)?;
    let expected = format!(
        "child-{}",
        hex::encode(Sha256::digest(serde_json::to_vec(&context.addr)?))
    );
    anyhow::ensure!(key == expected, ChildSessionError::IdentityMismatch);
    Ok(())
}
fn validate_json(record: &str, context: &ChildSessionContext) -> anyhow::Result<()> {
    let json: serde_json::Value = serde_json::from_str(record).map_err(|_| ChildSessionError::InvalidRecord)?;
    let address: crate::messaging::SessionAddr = serde_json::from_value(
        json.get("child_session")
            .and_then(|m| m.get("addr"))
            .cloned()
            .ok_or(ChildSessionError::InvalidRecord)?,
    )
    .map_err(|_| ChildSessionError::InvalidRecord)?;
    anyhow::ensure!(address == context.addr, ChildSessionError::IdentityMismatch);
    Ok(())
}
fn stored_scope(
    conn: &Connection,
    key: &str,
    context: &ChildSessionContext,
    allow_backfill: bool,
) -> anyhow::Result<Option<(String, MailboxScope)>> {
    let row=conn.query_row("SELECT record,workspace_id,owner_id,instance_id,session_id,agent_key,ma_scope_id,incarnation,generation FROM child_sessions WHERE key=?1",[key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<i64>>(8)?))).optional()?;
    let Some((record, workspace, owner, instance, session, agent, id, inc, generation)) = row else {
        return Ok(None);
    };
    let all_null = workspace.is_none()
        && owner.is_none()
        && instance.is_none()
        && session.is_none()
        && agent.is_none()
        && id.is_none()
        && inc.is_none()
        && generation.is_none();
    if all_null && allow_backfill {
        validate_json(&record, context)?;
        // Never mint/rebind a legacy record. Exactly one historical mapping
        // must already exist, in the trusted workspace, and still be live.
        let mut query=conn.prepare("SELECT scope_id,workspace_id,owner_id,incarnation,generation FROM agent_scopes WHERE owner_id=?1 AND session_key=?2 AND agent_key=?3")?;
        let rows = query
            .query_map(params![context.addr.owner, key, agent_key(&context.addr.agent)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(rows.len() == 1, ChildSessionError::UnboundScope);
        let (id, w, o, inc, generation) = rows.into_iter().next().ok_or(ChildSessionError::UnboundScope)?;
        anyhow::ensure!(w == context.workspace_id, ChildSessionError::IdentityMismatch);
        let scope = mailbox::pin::scope_from_row(w, o, id, inc, generation)?;
        mailbox::pin::check_scope(conn, &scope, &context.addr, Some(key), true)?;
        return Ok(Some((record, scope)));
    }
    let (
        Some(workspace),
        Some(owner),
        Some(instance),
        Some(session),
        Some(agent),
        Some(id),
        Some(inc),
        Some(generation),
    ) = (workspace, owner, instance, session, agent, id, inc, generation)
    else {
        return Err(ChildSessionError::UnboundScope.into());
    };
    anyhow::ensure!(
        workspace == context.workspace_id
            && owner == context.addr.owner
            && instance == context.addr.instance
            && session == context.addr.session
            && agent == agent_key(&context.addr.agent)?,
        ChildSessionError::IdentityMismatch
    );
    let scope = mailbox::pin::scope_from_row(workspace, owner, id, inc, generation)?;
    mailbox::pin::check_scope(conn, &scope, &context.addr, Some(key), true)?;
    validate_json(&record, context)?;
    Ok(Some((record, scope)))
}
fn resolve_new_scope(tx: &Transaction<'_>, context: &ChildSessionContext, key: &str) -> anyhow::Result<MailboxScope> {
    let partition = if context.addr.agent == "main" {
        PartitionKey::Main
    } else {
        let name = crate::config::agent_name::AgentName::parse(&context.addr.agent)?;
        let inc:String=tx.query_row("SELECT incarnation FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND state='active'",params![context.workspace_id,context.addr.owner,agent_key(&context.addr.agent)?],|r|r.get(0)).optional()?.ok_or(ChildSessionError::ScopeRefused)?;
        PartitionKey::Named {
            name,
            incarnation: uuid::Uuid::parse_str(&inc)?,
        }
    };
    let partition = AgentPartition {
        workspace_id: context.workspace_id.clone(),
        owner_id: context.addr.owner.clone(),
        key: partition,
    };
    let id = super::agent_migration::resolve_write_scope(tx, &partition, Some(key))?;
    let (inc, generation): (String, i64) = tx.query_row(
        "SELECT incarnation,generation FROM agent_scopes WHERE scope_id=?1",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(mailbox::pin::scope_from_row(
        context.workspace_id.clone(),
        context.addr.owner.clone(),
        id,
        inc,
        generation,
    )?)
}

impl ChildSessionStore for SqliteMemory {
    fn load(&self, key: &str, context: &ChildSessionContext) -> anyhow::Result<Option<ScopedChildRecord>> {
        validate_key(key, context)?;
        let writer = self.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let Some((record, scope)) = stored_scope(&tx, key, context, false)? else {
            return Ok(None);
        };
        let (cursors, mailbox_turns) = mailbox::child_projection::projection(&tx, context, &scope, key)?;
        let lease = Box::new(self.mailbox_runtime.cache_lease(scope.scope_id));
        tx.commit()?;
        Ok(Some(ScopedChildRecord {
            record,
            scope,
            cursors,
            mailbox_turns,
            lease,
        }))
    }
    fn update(
        &self,
        key: &str,
        context: &ChildSessionContext,
        update: &mut dyn FnMut(Option<&str>) -> anyhow::Result<String>,
    ) -> anyhow::Result<ScopedChildRecord> {
        validate_key(key, context)?;
        let writer = self.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let old = stored_scope(&tx, key, context, true)?;
        let scope = if let Some((_, scope)) = &old {
            scope.clone()
        } else {
            resolve_new_scope(&tx, context, key)?
        };
        let record = update(old.as_ref().map(|(record, _)| record.as_str()))?;
        validate_json(&record, context)?;
        tx.execute("INSERT INTO child_sessions(key,record,workspace_id,owner_id,instance_id,session_id,agent_key,ma_scope_id,incarnation,generation) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(key) DO UPDATE SET record=excluded.record,workspace_id=excluded.workspace_id,owner_id=excluded.owner_id,instance_id=excluded.instance_id,session_id=excluded.session_id,agent_key=excluded.agent_key,ma_scope_id=excluded.ma_scope_id,incarnation=excluded.incarnation,generation=excluded.generation",params![key,record,context.workspace_id,context.addr.owner,context.addr.instance,context.addr.session,agent_key(&context.addr.agent)?,scope.scope_id.to_string(),scope.incarnation.to_string(),scope.generation])?;
        let (cursors, mailbox_turns) = mailbox::child_projection::projection(&tx, context, &scope, key)?;
        let lease = Box::new(self.mailbox_runtime.cache_lease(scope.scope_id));
        tx.commit()?;
        Ok(ScopedChildRecord {
            record,
            scope,
            cursors,
            mailbox_turns,
            lease,
        })
    }
}

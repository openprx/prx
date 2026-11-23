//! Trusted runtime resolution of owner-scoped session addresses.
use super::{SqliteMailbox, pin, storage_error};
use crate::memory::{AgentPartition, PartitionKey};
use crate::messaging::mailbox::{MailboxError, MailboxResult, MailboxScope};
use crate::messaging::{CausalChain, SessionAddr};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

impl SqliteMailbox {
    pub(crate) fn partition(
        &self,
        scope: &crate::messaging::mailbox::MailboxScope,
    ) -> crate::messaging::mailbox::MailboxResult<crate::memory::AgentPartition> {
        let writer = self.memory.pool.write();
        let key:String=writer.query_row("SELECT agent_key FROM agent_scopes WHERE scope_id=?1 AND workspace_id=?2 AND owner_id=?3 AND incarnation=?4 AND generation=?5",params![scope.scope_id.to_string(),scope.workspace_id,scope.owner_id,scope.incarnation.to_string(),scope.generation],|row|row.get(0)).map_err(storage_error)?;
        let key = if key == "m" {
            crate::memory::PartitionKey::Main
        } else {
            crate::memory::PartitionKey::Named {
                name: crate::config::agent_name::AgentName::parse(
                    key.strip_prefix("n:")
                        .ok_or(crate::messaging::mailbox::MailboxError::CorruptState)?,
                )
                .map_err(|_| crate::messaging::mailbox::MailboxError::CorruptState)?,
                incarnation: scope.incarnation,
            }
        };
        Ok(crate::memory::AgentPartition {
            workspace_id: scope.workspace_id.clone(),
            owner_id: scope.owner_id.clone(),
            key,
        })
    }
    pub(crate) fn hold_input(
        &self,
        scope: &MailboxScope,
        target: &SessionAddr,
    ) -> MailboxResult<std::sync::Arc<dyn Send + Sync>> {
        let writer = self.memory.pool.write();
        pin::check_scope(&writer, scope, target, None, true)?;
        Ok(std::sync::Arc::new(
            self.memory.mailbox_runtime.cache_lease(scope.scope_id),
        ))
    }
    pub(crate) fn with_live_scope<T>(
        &self,
        scope: &MailboxScope,
        target: &SessionAddr,
        action: impl FnOnce() -> T,
    ) -> MailboxResult<T> {
        let writer = self.memory.pool.write();
        pin::check_scope(&writer, scope, target, None, true)?;
        Ok(action())
    }
    pub(crate) fn validate_input(
        &self,
        workspace: &str,
        target: &SessionAddr,
        event: uuid::Uuid,
        text: &str,
        message: uuid::Uuid,
        receipt: &crate::messaging::mailbox::MailboxReceipt,
    ) -> MailboxResult<()> {
        self.with_input(workspace, target, event, text, message, receipt, || ())
    }
    pub(crate) fn with_input<T>(
        &self,
        workspace: &str,
        target: &SessionAddr,
        event: uuid::Uuid,
        text: &str,
        message: uuid::Uuid,
        receipt: &crate::messaging::mailbox::MailboxReceipt,
        action: impl FnOnce() -> T,
    ) -> MailboxResult<T> {
        let scope = self.session_scope(workspace, target, false)?;
        let writer = self.memory.pool.write();
        pin::check_scope(&writer, &scope, target, None, true)?;
        let raw:Option<String>=writer.query_row("SELECT source_ref_json FROM message_events WHERE event_id=?1 AND workspace_id=?2 AND owner_id=?3 AND session_key=?4 AND source='prx_mailbox' AND role='user' AND content=?5 AND ma_scope_id=?6",params![event.to_string(),scope.workspace_id,target.owner,target.session,text,scope.scope_id.to_string()],|r|r.get(0)).optional().map_err(storage_error)?;
        let reference: super::model::EventRef = super::model::decode(&raw.ok_or(MailboxError::Fenced)?)?;
        if reference.message_id != message
            || receipt.message_id != message
            || reference.common.chain(message) != receipt.chain
            || reference.scope != scope
            || reference.agent != target.agent
        {
            return Err(MailboxError::IdentityImmutable);
        }
        Ok(action())
    }

    pub fn has_pending(&self, scope: &MailboxScope, target: &SessionAddr) -> MailboxResult<bool> {
        self.validate_scope(scope, target)?;
        let writer = self.memory.pool.write();
        writer.query_row("SELECT EXISTS(SELECT 1 FROM prx_mailbox_messages m JOIN prx_mailbox_seq s ON s.stream_id=m.stream_id WHERE m.ma_scope_id=?1 AND s.to_addr=?2 AND m.state IN(0,1))", params![scope.scope_id.to_string(),serde_json::to_string(target).map_err(storage_error)?], |r| r.get(0)).map_err(storage_error)
    }

    pub fn session_scope(
        &self,
        workspace: &str,
        address: &SessionAddr,
        registered: bool,
    ) -> MailboxResult<MailboxScope> {
        crate::messaging::mailbox::validate_address(address)?;
        if address.instance != self.local_instance {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::LocalInstanceMismatch,
            ));
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        let child:Option<(String,String,i64)>=tx.query_row("SELECT ma_scope_id,incarnation,generation FROM child_sessions WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3 AND json_extract(record,'$.agent')=?4",
            params![workspace,address.owner,address.session,address.agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage_error)?;
        if let Some((scope, inc, generation)) = child {
            let scope = pin::scope_from_row(workspace.into(), address.owner.clone(), scope, inc, generation)?;
            pin::check_scope(&tx, &scope, address, Some(&address.session), true)?;
            tx.commit().map_err(storage_error)?;
            return Ok(scope);
        }
        let actual:Option<String>=tx.query_row("SELECT CASE WHEN s.agent_key='m' THEN 'main' ELSE substr(s.agent_key,3) END FROM message_events e JOIN agent_scopes s ON s.scope_id=e.ma_scope_id WHERE e.workspace_id=?1 AND e.owner_id=?2 AND e.session_key=?3 ORDER BY e.id DESC LIMIT 1",params![workspace,address.owner,address.session],|r|r.get(0)).optional().map_err(storage_error)?;
        if actual.as_deref().is_some_and(|agent| agent != address.agent) {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::ReceiverAgentMismatch,
            ));
        }
        let known:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND session_key=?3) OR EXISTS(SELECT 1 FROM sessions WHERE owner_id=?2 AND session_key=?3) OR EXISTS(SELECT 1 FROM child_sessions WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3) OR EXISTS(SELECT 1 FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2 AND session_key=?3 AND agent_key=?4)",params![workspace,address.owner,address.session,crate::messaging::mailbox::agent_key(&address.agent)?],|r|r.get(0)).map_err(storage_error)?;
        if !known && !registered {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::ReceiverUnknown,
            ));
        }
        let key = if address.agent == "main" {
            PartitionKey::Main
        } else {
            let agent = crate::config::agent_name::AgentName::parse(&address.agent)
                .map_err(|_| MailboxError::InvalidAddress)?;
            let incarnation:String=tx.query_row("SELECT incarnation FROM agent_incarnations WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND state='active'",params![workspace,address.owner,format!("n:{}",address.agent)],|r|r.get(0)).optional().map_err(storage_error)?.ok_or_else(|| crate::messaging::mailbox::unauthorized(crate::messaging::mailbox::UnauthorizedReason::ReceiverAgentInactive))?;
            PartitionKey::Named {
                name: agent,
                incarnation: uuid::Uuid::parse_str(&incarnation).map_err(|_| MailboxError::CorruptState)?,
            }
        };
        let scope = super::super::agent_migration::resolve_write_scope(
            &tx,
            &AgentPartition {
                workspace_id: workspace.into(),
                owner_id: address.owner.clone(),
                key,
            },
            Some(&address.session),
        )
        .map_err(storage_error)?;
        let (inc, generation): (String, i64) = tx
            .query_row(
                "SELECT incarnation,generation FROM agent_scopes WHERE scope_id=?1",
                [&scope],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(storage_error)?;
        let scope = pin::scope_from_row(workspace.into(), address.owner.clone(), scope, inc, generation)?;
        pin::check_scope(&tx, &scope, address, Some(&address.session), true)?;
        tx.commit().map_err(storage_error)?;
        Ok(scope)
    }
    pub fn logical_agent(&self, workspace: &str, owner: &str, session: &str) -> MailboxResult<Option<String>> {
        let writer = self.memory.pool.write();
        let child:Option<Option<String>>=writer.query_row("SELECT json_extract(record,'$.agent') FROM child_sessions WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3",params![workspace,owner,session],|r|r.get(0)).optional().map_err(storage_error)?;
        if let Some(agent) = child.flatten() {
            return Ok(Some(agent));
        }
        writer.query_row("SELECT CASE WHEN s.agent_key='m' THEN 'main' ELSE substr(s.agent_key,3) END FROM message_events e JOIN agent_scopes s ON s.scope_id=e.ma_scope_id WHERE e.workspace_id=?1 AND e.owner_id=?2 AND e.session_key=?3 ORDER BY e.id DESC LIMIT 1",params![workspace,owner,session],|r|r.get(0)).optional().map_err(storage_error)
    }

    pub fn parent_session(&self, workspace: &str, owner: &str, session: &str) -> MailboxResult<Option<String>> {
        let writer = self.memory.pool.write();
        writer.query_row("SELECT json_extract(record,'$.parent_session') FROM child_sessions WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3",params![workspace,owner,session],|r|r.get(0)).optional().map_err(storage_error).map(Option::flatten)
    }

    pub fn source_chain(&self, workspace: &str, owner: &str, event: &str) -> MailboxResult<Option<CausalChain>> {
        let writer = self.memory.pool.write();
        let parent: Option<String> = writer
            .query_row(
                "SELECT causation_event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND event_id=?3",
                params![workspace, owner, event],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .flatten();
        let Some(parent) = parent else {
            return Ok(None);
        };
        let raw:Option<String>=writer.query_row("SELECT source_ref_json FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND event_id=?3 AND source='prx_mailbox'",params![workspace,owner,parent],|r|r.get(0)).optional().map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let reference: super::model::EventRef = super::model::decode(&raw)?;
        reference.common.validate(reference.message_id)?;
        Ok(Some(reference.common.chain(reference.message_id)))
    }
}

//! Atomic event append, recovery fencing and child-session persistence.
#[cfg(test)]
use super::events::hold_append_snapshot_for_test;
use super::recovery_authority::{self, FullSync};
use super::recovery_keys::KeyDirectory;
use super::{SqliteMemory, ingress_recovery};
use crate::memory::ingress_recovery::AtomicIngressMutation;
use crate::memory::{AgentScopeRefused, MessageEvent, MessageEventInput, PartitionKey};
use anyhow::Context;
use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::sync::Arc;
use uuid::Uuid;
impl SqliteMemory {
    pub(super) async fn append_event_with_recovery(
        &self,
        input: MessageEventInput,
        mutation: Option<crate::memory::ingress_recovery::AtomicIngressMutation>,
        repair_deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        // The attested append future carries the whole input, the mutation and the
        // blocking closure; keep it on the heap so callers' futures stay small.
        Box::pin(self.append_attested_event(input, mutation, repair_deadline, None))
            .await?
            .context("only an ingress input can hit a tombstone without an error")
    }

    /// The single append transaction; `None` means the stable key of an ingress
    /// input is tombstoned (any other tombstoned input is `StableKeyTombstoned`).
    /// Every stable-key user input runs the tombstone fence inside it. An attested
    /// ingress input also writes its pending attestation and transport index row,
    /// under FULL synchronization, so a crash leaves all of them or none. An rf
    /// admission writes its attempt row in the same transaction (AC-1b2a).
    pub(super) async fn append_attested_event(
        &self,
        input: MessageEventInput,
        mutation: Option<crate::memory::ingress_recovery::AtomicIngressMutation>,
        repair_deadline: Option<std::time::Instant>,
        admission: Option<crate::memory::rf_messages::RfAdmission>,
    ) -> anyhow::Result<Option<(MessageEvent, bool)>> {
        input.validate()?;
        let pool = self.pool.clone();
        let deadline = mutation.as_ref().map(ingress_recovery::deadline).or(repair_deadline);
        let repair_owner = repair_deadline
            .map(|_| self.repair_owner(&input.workspace_id))
            .transpose()?;
        let key_root = self.db_path.parent().map(std::path::Path::to_path_buf);
        #[cfg(test)]
        let db_path = self.db_path.clone();

        Box::pin(crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Option<(MessageEvent, bool)>> {
                // The mutation owns the genuine lease inside this blocking closure,
                // including when its async waiter is cancelled.
                let mut write = match deadline {
                    Some(deadline) => pool.write_until(deadline)?,
                    None => pool.write(),
                };
                let conn = ingress_recovery::DeadlineConn::wrap(&mut write, deadline)?;
                let attested = match &mutation {
                    Some(AtomicIngressMutation::Input {
                        capsule,
                        attestation: Some(attestation),
                        ..
                    }) => Some((capsule, attestation)),
                    _ => None,
                };
                // Synchronization cannot change inside a transaction: set it first.
                let _sync = attested.map(|_| FullSync::new(&conn)).transpose()?;
                let keys = attested
                    .map(|_| KeyDirectory::open(key_root.as_deref().context("missing memory directory")?, false))
                    .transpose()?;
                // BEGIN IMMEDIATE takes the WAL write lock before any read.
                // BEGIN DEFERRED pins a snapshot on the first SELECT (the
                // tombstone fence when it runs); a second opener that commits
                // then makes this INSERT return SQLITE_BUSY_SNAPSHOT /
                // "database is locked" without the busy handler — the
                // spawn_batch child-start failure against another pool on
                // brain.db.
                let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
                if let (Some(owner), Some(deadline)) = (&repair_owner, repair_deadline) {
                    ingress_recovery::validate_owner(&tx, owner, &input.workspace_id, deadline)?;
                }
                if let Some(operation) = &mutation {
                    ingress_recovery::before_append(&tx, operation, &input)?;
                }
                // Every stable-key user input is fenced, with or without a recorder.
                if matches!(
                    recovery_authority::stable_key_fence(&tx, key_root.as_deref(), &input)?,
                    recovery_authority::Fence::Tombstoned
                ) {
                    // Dropping the transaction rolls back; nothing was inserted.
                    return match mutation {
                        Some(AtomicIngressMutation::Input { .. }) => Ok(None),
                        _ => Err(crate::memory::ingress_recovery::StableKeyTombstoned.into()),
                    };
                }
                #[cfg(test)]
                hold_append_snapshot_for_test(&input, &db_path)?;
                let (event, inserted) = Self::append_event_row(&tx, input, deadline.is_some())?;
                if let Some(operation) = &mutation {
                    ingress_recovery::after_append(&tx, operation, &event, inserted)?;
                }
                if let (true, Some((capsule, attestation)), Some(keys)) = (inserted, attested, &keys) {
                    recovery_authority::attest_ingress(&tx, keys, capsule, attestation, &event)?;
                }
                if let Some(admission) = &admission {
                    let lifetime = mutation.as_ref().map(|operation| operation.lease().token());
                    super::rf_messages::admit(&tx, admission, &event, inserted, lifetime)?;
                }
                if let (Some(owner), Some(deadline)) = (&repair_owner, repair_deadline) {
                    ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
                }
                if let Some(deadline) = deadline {
                    crate::memory::ingress_recovery::remaining(deadline)?;
                }
                tx.commit()?;
                if let Some(deadline) = deadline {
                    crate::memory::ingress_recovery::remaining(deadline)?;
                }
                Ok(Some((event, inserted)))
            },
        ))
        .await?
    }

    pub(in crate::memory::sqlite) fn append_event_row(
        tx: &rusqlite::Transaction<'_>,
        input: MessageEventInput,
        bounded_receipt: bool,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        let now = Utc::now().to_rfc3339();
        let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let content_hash = Self::content_hash(&input.content);
        let visibility = input.visibility.as_str().to_string();
        let source = input.source.as_str().to_string();
        let source_ref_json = serde_json::to_string(&input.source)?;
        let subject_ref_json = input.subject.as_ref().map(serde_json::to_string).transpose()?;
        let config_generation_id = input.config_generation_id.map(i64::try_from).transpose()?;

        let mut bound_scope = super::agent_migration::resolve_input_scope(
            tx,
            input.agent_partition.as_ref(),
            input.session_key.as_deref(),
            &input.workspace_id,
            input.owner_id.as_deref(),
        )?;
        // A compaction audit row of a child turn (switch handoff, degradation,
        // compaction summary) carries no partition for a Main child, so it binds
        // the child's own scope the same way the child's message rows do.
        let child_message_source = matches!(
            input.source,
            crate::memory::MessageEventSource::SessionsSpawn | crate::memory::MessageEventSource::SessionWorker
        );
        let child_audit_source = matches!(
            &input.source,
            crate::memory::MessageEventSource::Other(source) if matches!(source.as_str(), "context_switch" | "compaction")
        );
        if bound_scope.is_none() && (child_message_source || child_audit_source) {
            bound_scope = tx
                .query_row(
                    "SELECT ma_scope_id FROM child_sessions WHERE workspace_id=?1 AND owner_id=?2 AND session_id=?3",
                    params![input.workspace_id, input.owner_id, input.session_key],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(scope) = &bound_scope {
                let fenced: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_erase_fences WHERE scope_id=?1)",
                    [scope],
                    |r| r.get(0),
                )?;
                anyhow::ensure!(!fenced, AgentScopeRefused);
            } else if child_message_source
                && input
                    .raw_payload_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .is_some_and(|value| value.get("child_session_record").is_some())
            {
                let owner = input.owner_id.clone().ok_or(AgentScopeRefused)?;
                bound_scope = Some(super::agent_migration::resolve_write_scope(
                    tx,
                    &crate::memory::AgentPartition {
                        workspace_id: input.workspace_id.clone(),
                        owner_id: owner,
                        key: crate::memory::PartitionKey::Main,
                    },
                    input.session_key.as_deref(),
                )?);
            }
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO message_events (
                event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                content, content_hash, raw_payload_json, visibility, created_at, updated_at, ma_scope_id
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                     ?30, ?31, ?32)",
            params![
                event_id,
                input.idempotency_key,
                input.workspace_id,
                input.owner_id,
                source,
                input.channel,
                input.session_key,
                input.parent_session_key,
                input.run_id,
                input.parent_run_id,
                input.agent_id,
                input.persona_id,
                input.sender,
                input.recipient,
                input.role,
                input.event_type,
                source_ref_json,
                subject_ref_json,
                input.goal_id,
                input.causation_event_id,
                input.correlation_id,
                input.attempt_id,
                input.lease_epoch,
                config_generation_id,
                input.config_source_revision,
                input.content,
                content_hash,
                input.raw_payload_json,
                visibility,
                now,
                now,
                bound_scope
            ],
        )?;

        if bounded_receipt {
            ingress_recovery::check_receipt_size(tx, &event_id, input.idempotency_key.as_deref(), &input.workspace_id)?;
        }

        let event = if let Some(ref idempotency_key) = input.idempotency_key {
            tx.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE event_id = ?1
                    OR (workspace_id = ?3 AND idempotency_key = ?2)
                 ORDER BY CASE WHEN event_id = ?1 THEN 0 ELSE 1 END
                 LIMIT 1",
                params![event_id, idempotency_key, input.workspace_id],
                Self::message_event_from_row,
            )?
        } else {
            tx.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE event_id = ?1
                 LIMIT 1",
                params![event_id],
                Self::message_event_from_row,
            )?
        };

        let stored_scope: Option<String> = tx.query_row(
            "SELECT ma_scope_id FROM message_events WHERE event_id = ?1",
            params![event.event_id],
            |row| row.get(0),
        )?;
        if stored_scope != bound_scope {
            return Err(AgentScopeRefused.into());
        }
        if inserted > 0 {
            let mirror_scope = match (input.agent_partition.as_ref(), stored_scope) {
                (_, Some(scope)) => Some(scope),
                (Some(partition), None) => match &partition.key {
                    PartitionKey::Named { .. } => return Err(AgentScopeRefused.into()),
                    PartitionKey::Main => Some(super::agent_migration::resolve_write_scope(
                        tx,
                        partition,
                        input.session_key.as_deref(),
                    )?),
                },
                (None, None) => None,
            };
            tx.execute(
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at, ma_scope_id
                 )
                 VALUES (?1, ?2, ?3, ?4, 'message_events', ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11)",
                params![
                    Uuid::new_v4().to_string(),
                    event.workspace_id,
                    event.owner_id,
                    event.event_type,
                    event.event_id,
                    event.session_key,
                    event.agent_id,
                    event.persona_id,
                    event.visibility.as_str(),
                    Utc::now().to_rfc3339(),
                    mirror_scope
                ],
            )?;
        }
        if inserted > 0 {
            super::child_turns::source_written(tx, &event)?;
            super::sync_journal::event_written(tx, &event)?;
        }
        Ok((event, inserted > 0))
    }

    pub(super) fn repair_owner(
        &self,
        workspace: &str,
    ) -> anyhow::Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.ingress_owners
            .cell(workspace)?
            .cell
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("SQLite ingress repair requires an initialized owner"))
    }
}

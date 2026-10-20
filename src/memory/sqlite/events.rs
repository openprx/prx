//! Conversations, ingress recovery, message events and memory events.

use crate::memory::traits::{
    ChatProfile, ConversationSessionSummary, ConversationTurn, MemoryEvent, MemoryEventInput, MemoryPrincipal,
    MessageEvent, MessageEventInput, SessionContextQuery, SharedContextQuery,
};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;
use uuid::Uuid;

use super::ingress_recovery;
use super::{SQLITE_DIALECT, SqliteMemory};

impl SqliteMemory {
    pub(super) async fn list_conversation_turns_ordered(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
        recorded_order: bool,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let pool = self.pool.clone();
        let ordering = if recorded_order {
            "julianday(timestamp) DESC, COALESCE((SELECT event.id FROM message_events event WHERE event.event_id = conversation_turns.message_event_id), id) DESC, id DESC"
        } else {
            "id DESC"
        };
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let session_key = session_key.to_string();
        let limit = Self::sanitize_conversation_limit(limit);
        let offset = Self::sanitize_conversation_offset(offset);
        let legacy_visible = 1_i64;
        // D4 read-merge: the explicit `session_key` arg is the canonical key
        // (bound at `?1`); the principal may carry a distinct legacy key bound
        // at trailing placeholders starting at `?6`. With no legacy key the
        // predicate degrades to the byte-identical `session_key = ?1`.
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
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<ConversationTurn>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, session_key, role, content, timestamp, message_id
                 FROM conversation_turns
                 WHERE {session_fragment}
                   AND (
                       ?4 = 'system:*'
                       OR
                       owner_id = ?4
                       OR (?5 = 1 AND (owner_id IS NULL OR owner_id = 'legacy:' || session_key))
                   )
                 ORDER BY {ordering}
                 LIMIT ?2 OFFSET ?3",
                session_fragment = session_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> =
                vec![&session_key, &limit, &offset, &owner_id, &legacy_visible];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            let rows = stmt.query_map(bind.as_slice(), |row| {
                Ok(ConversationTurn {
                    id: row.get(0)?,
                    session_key: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    timestamp: row.get(4)?,
                    message_id: row.get(5)?,
                })
            })?;

            let mut turns = Vec::new();
            for row in rows {
                turns.push(row?);
            }
            turns.reverse();
            Ok(turns)
        })
        .await?
    }

    pub(super) async fn append_event_with_recovery(
        &self,
        input: MessageEventInput,
        mutation: Option<crate::memory::ingress_recovery::AtomicIngressMutation>,
        repair_deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<(MessageEvent, bool)> {
        input.validate()?;
        let pool = self.pool.clone();
        let deadline = mutation.as_ref().map(ingress_recovery::deadline).or(repair_deadline);
        let repair_owner = repair_deadline
            .map(|_| self.repair_owner(&input.workspace_id))
            .transpose()?;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<(MessageEvent, bool)> {
            // The mutation owns the genuine lease inside this blocking closure,
            // including when its async waiter is cancelled.
            let conn = match deadline {
                Some(deadline) => pool.write_until(deadline)?,
                None => pool.write(),
            };
            let _budget = deadline
                .map(|deadline| ingress_recovery::ConnectionDeadline::new(&conn, deadline))
                .transpose()?;
            let tx = conn.unchecked_transaction()?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, repair_deadline) {
                ingress_recovery::validate_owner(&tx, owner, &input.workspace_id, deadline)?;
            }
            if let Some(operation) = &mutation {
                ingress_recovery::before_append(&tx, operation, &input)?;
            }
            let (event, inserted) = Self::append_event_row(&tx, input, deadline.is_some())?;
            if let Some(operation) = &mutation {
                ingress_recovery::after_append(&tx, operation, &event, inserted)?;
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
            Ok((event, inserted))
        })
        .await?
    }

    pub(super) fn append_event_row(
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

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO message_events (
                event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                content, content_hash, raw_payload_json, visibility, created_at, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                     ?30, ?31)",
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
                now
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

        if inserted > 0 {
            tx.execute(
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, 'message_events', ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
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
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        Ok((event, inserted > 0))
    }

    fn repair_owner(
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

    pub(super) async fn project_console_message_with_deadline(
        &self,
        event: &MessageEvent,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<()> {
        if let Some(deadline) = deadline {
            crate::memory::ingress_recovery::remaining(deadline)?;
            ingress_recovery::validate_event_bounds(event)?;
        }
        event.validate_console_projection()?;
        let repair_owner = deadline.map(|_| self.repair_owner(&event.workspace_id)).transpose()?;
        let event = event.clone();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = match deadline { Some(deadline) => pool.write_until(deadline)?, None => pool.write() };
            let _budget = deadline.map(|deadline| ingress_recovery::ConnectionDeadline::new(&conn, deadline)).transpose()?;
            let tx = conn.unchecked_transaction()?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
                ingress_recovery::check_receipt_size(&tx, &event.event_id, None, &event.workspace_id)?;
                ingress_recovery::check_console_projection_size(&tx, &event.event_id)?;
            }
            let durable = tx.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events WHERE event_id = ?1 AND workspace_id = ?2 AND owner_id = ?3",
                params![event.event_id, event.workspace_id, event.owner_id], Self::message_event_from_row,
            )?;
            anyhow::ensure!(serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
                "console projection differs from its durable event");
            let existing: Vec<(String, Option<String>, String, String)> = {
                let mut stmt = tx.prepare("SELECT session_key, owner_id, role, content FROM conversation_turns
                    WHERE message_event_id = ?1 OR message_id = ?1 LIMIT 2")?;
                stmt.query_map(params![event.event_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if let Some(row) = existing.first() {
                anyhow::ensure!(existing.len() == 1 && Some(&row.0) == event.session_key.as_ref()
                    && row.1 == event.owner_id && row.2 == event.role && row.3 == event.content,
                    "console message projection conflicts with its durable event");
                if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                    ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
                }
                return Ok(());
            }
            let preview = Self::conversation_preview(&event.content);
            tx.execute("INSERT INTO sessions
                (session_key, channel, sender, owner_id, created_at, updated_at, message_count, last_message_preview)
                VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?6)
                ON CONFLICT(session_key) DO UPDATE SET
                    message_count = COALESCE(sessions.message_count, 0) + 1,
                    channel = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.channel ELSE sessions.channel END,
                    sender = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.sender ELSE sessions.sender END,
                    owner_id = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.owner_id ELSE sessions.owner_id END,
                    last_message_preview = CASE WHEN excluded.updated_at >= sessions.updated_at THEN excluded.last_message_preview ELSE sessions.last_message_preview END,
                    updated_at = MAX(sessions.updated_at, excluded.updated_at)",
                params![event.session_key, event.channel, event.sender, event.owner_id, event.created_at, preview])?;
            tx.execute("INSERT INTO conversation_turns
                (session_key, owner_id, role, content, timestamp, message_id, message_event_id, agent_id, persona_id, visibility)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, 'session')",
                params![event.session_key, event.owner_id, event.role, event.content, event.created_at,
                    event.event_id, event.agent_id, event.persona_id])?;
            if let (Some(owner), Some(deadline)) = (&repair_owner, deadline) {
                ingress_recovery::validate_owner(&tx, owner, &event.workspace_id, deadline)?;
            }
            tx.commit()?;
            if let Some(deadline) = deadline { crate::memory::ingress_recovery::remaining(deadline)?; }
            Ok(())
        }).await?
    }

    pub(super) async fn upsert_chat_profile_metadata_impl(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let title = title.map(str::to_string);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let conn = pool.write();
            conn.execute(
                "INSERT INTO chat_profiles
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, '[]', 'auto', ?6, ?6)
                 ON CONFLICT(channel, chat_id) DO UPDATE SET
                    chat_kind = excluded.chat_kind,
                    title = CASE
                        WHEN excluded.title IS NOT NULL THEN excluded.title
                        ELSE chat_profiles.title
                    END,
                    updated_at = excluded.updated_at",
                params![id, channel, chat_id, chat_kind, title, now],
            )?;
            Ok(())
        })
        .await?
    }

    pub(super) async fn get_chat_profile_impl(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> anyhow::Result<Option<ChatProfile>> {
        let pool = self.pool.clone();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<ChatProfile>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let profile = conn
                .query_row(
                    "SELECT id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at
                     FROM chat_profiles
                     WHERE channel = ?1 AND chat_id = ?2",
                    params![channel, chat_id],
                    Self::chat_profile_from_row,
                )
                .optional()?;
            Ok(profile)
        })
        .await?
    }

    pub(super) async fn list_console_terminal_events_impl(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !session_key.is_empty(),
            "console recovery scope is empty"
        );
        let workspace_id = workspace_id.to_owned();
        let session_key = session_key.to_owned();
        let limit = i64::try_from(limit.clamp(1, 128))?;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare("SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM message_events WHERE workspace_id = ?1 AND session_key = ?2
                AND source = 'console' AND event_type = 'turn.finalized' AND id > ?3
                ORDER BY id ASC LIMIT ?4")?;
            Ok(stmt.query_map(params![workspace_id, session_key, after_id, limit], Self::message_event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }).await?
    }

    pub(super) async fn list_conversation_sessions_impl(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let pool = self.pool.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let offset = Self::sanitize_conversation_offset(offset);
        let channel = channel.map(str::to_string);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<ConversationSessionSummary>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut sessions = Vec::new();

            if let Some(channel_filter) = channel {
                let mut stmt = conn.prepare(
                    "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions
                     WHERE channel = ?1
                     ORDER BY updated_at DESC
                     LIMIT ?2 OFFSET ?3",
                )?;
                let rows = stmt.query_map(params![channel_filter, limit, offset], |row| {
                    let message_count: i64 = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    let message_count = message_count.max(0) as u64;
                    Ok(ConversationSessionSummary {
                        session_key: row.get(0)?,
                        channel: row.get(1)?,
                        sender: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        message_count,
                        last_message_preview: row.get(6)?,
                    })
                })?;
                for row in rows {
                    sessions.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions
                     ORDER BY updated_at DESC
                     LIMIT ?1 OFFSET ?2",
                )?;
                let rows = stmt.query_map(params![limit, offset], |row| {
                    let message_count: i64 = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    let message_count = message_count.max(0) as u64;
                    Ok(ConversationSessionSummary {
                        session_key: row.get(0)?,
                        channel: row.get(1)?,
                        sender: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        message_count,
                        last_message_preview: row.get(6)?,
                    })
                })?;
                for row in rows {
                    sessions.push(row?);
                }
            }

            Ok(sessions)
        })
        .await?
    }

    pub(super) async fn get_conversation_session_impl(
        &self,
        session_key: &str,
    ) -> anyhow::Result<Option<ConversationSessionSummary>> {
        let pool = self.pool.clone();
        let session_key = session_key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<ConversationSessionSummary>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                     FROM sessions WHERE session_key = ?1",
            )?;
            let row = stmt.query_row(params![session_key], |row| {
                let message_count: i64 = row.get(5)?;
                #[allow(clippy::cast_sign_loss)]
                let message_count = message_count.max(0) as u64;
                Ok(ConversationSessionSummary {
                    session_key: row.get(0)?,
                    channel: row.get(1)?,
                    sender: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    message_count,
                    last_message_preview: row.get(6)?,
                })
            });

            match row {
                Ok(summary) => Ok(Some(summary)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    pub(super) async fn load_recent_conversation_histories_impl(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
        let pool = self.pool.clone();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let max_turns_per_session = Self::sanitize_conversation_limit(max_turns_per_session);
        let max_sessions = Self::sanitize_hydrated_sessions_limit(max_sessions);
        let legacy_visible = 1_i64;

        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let mut stmt = conn.prepare(
                    "SELECT id, session_key, role, content, timestamp, message_id
                     FROM (
                         SELECT
                             ct.id,
                             ct.session_key,
                             ct.role,
                             ct.content,
                             ct.timestamp,
                             ct.message_id,
                             ROW_NUMBER() OVER (PARTITION BY ct.session_key ORDER BY ct.id DESC) AS row_num
                         FROM conversation_turns ct
                         INNER JOIN (
                             SELECT session_key
                             FROM sessions
                             WHERE ?3 = 'system:*'
                                OR owner_id = ?3
                                OR (?4 = 1 AND (owner_id IS NULL OR owner_id = 'legacy:' || session_key))
                             ORDER BY updated_at DESC
                             LIMIT ?2
                         ) recent_sessions
                         ON recent_sessions.session_key = ct.session_key
                         WHERE ?3 = 'system:*'
                            OR ct.owner_id = ?3
                            OR (?4 = 1 AND (ct.owner_id IS NULL OR ct.owner_id = 'legacy:' || ct.session_key))
                     )
                     WHERE row_num <= ?1
                     ORDER BY session_key ASC, id ASC",
                )?;
                let rows = stmt.query_map(
                    params![max_turns_per_session, max_sessions, owner_id, legacy_visible],
                    |row| {
                        Ok(ConversationTurn {
                            id: row.get(0)?,
                            session_key: row.get(1)?,
                            role: row.get(2)?,
                            content: row.get(3)?,
                            timestamp: row.get(4)?,
                            message_id: row.get(5)?,
                        })
                    },
                )?;

                let mut histories: std::collections::HashMap<String, Vec<ConversationTurn>> =
                    std::collections::HashMap::new();
                for row in rows {
                    let turn = row?;
                    histories.entry(turn.session_key.clone()).or_default().push(turn);
                }
                Ok(histories)
            },
        )
        .await?
    }

    pub(super) async fn acquire_ingress_lifetime_impl(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        crate::memory::ingress_recovery::remaining(deadline)?;
        let entry = self.ingress_owners.cell(workspace_id)?;
        let lifetime = if let Some(lifetime) = entry.cell.get() {
            lifetime.clone()
        } else {
            // Only queueing for initialization is timed out. Once this caller
            // owns the gate, admission's registered owner task awaits the real
            // backend completion, including a late blocking open/write.
            let _gate = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                entry.init_gate.clone().lock_owned(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("ingress owner initialization queue deadline elapsed"))?;
            crate::memory::ingress_recovery::remaining(deadline)?;
            entry
                .cell
                .get_or_try_init(|| async {
                    ingress_recovery::acquire(
                        self.pool.clone(),
                        self.db_path.clone(),
                        workspace_id.to_owned(),
                        token.to_owned(),
                        deadline,
                        false,
                    )
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("ingress lifetime is already owned"))
                })
                .await?
                .clone()
        };
        // Validation failures retain the original cell: no token replacement or
        // implicit authority renewal after loss of the issued owner.
        lifetime.validate(deadline).await?;
        Ok(lifetime)
    }

    pub(super) async fn append_ingress_input_impl(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        let (event, inserted) = self
            .append_event_with_recovery(
                input,
                Some(crate::memory::ingress_recovery::AtomicIngressMutation::Input {
                    capsule,
                    lifetime,
                    deadline,
                }),
                None,
            )
            .await?;
        Ok(if inserted {
            crate::memory::ingress_recovery::IngressAppendOutcome::Inserted(event)
        } else {
            crate::memory::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(event)
        })
    }

    pub(super) async fn find_message_event_by_idempotency_key_impl(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let workspace_id = workspace_id.to_string();
        let idempotency_key = idempotency_key.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND idempotency_key = ?2
                 LIMIT 1",
                params![workspace_id, idempotency_key],
                Self::message_event_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn list_worker_launch_events_impl(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && after_id >= 0 && (1..=128).contains(&limit),
            "worker launch discovery requires workspace, nonnegative cursor and limit 1..=128"
        );
        let workspace_id = workspace_id.to_owned();
        let limit = i64::try_from(limit)?;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND event_type IN ('worker.recovery.registered', 'worker.ordinary.recovery.registered', 'worker.operator.recovery.registered') AND id > ?2
                 ORDER BY id ASC LIMIT ?3",
            )?;
            stmt.query_map(params![workspace_id, after_id, limit], Self::message_event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn find_owned_message_event_impl(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let workspace_id = workspace_id.to_string();
        if workspace_id.is_empty() || owner_id.is_empty() || event_id.is_empty() {
            return Ok(None);
        }
        let owner_id = owner_id.to_string();
        let event_id = event_id.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND event_id = ?3
                 LIMIT 1",
                params![workspace_id, owner_id, event_id],
                Self::message_event_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn list_message_events_since_impl(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its original `?5` binding; legacy key(s) bind to
        // new trailing placeholders starting at `?9` (after limit at `?8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
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
                   )
                 ORDER BY id ASC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn list_message_events_recent_impl(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = Self::sanitize_conversation_limit(limit);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?4` binding; legacy key(s) bind at trailing
        // placeholders starting at `?8` (after limit at `?7`).
        let session_indices = Self::session_indices(4, 8, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE (
                       visibility = 'global'
                       OR (
                           workspace_id = ?1
                           AND (
                               visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                                   OR (?5 IS NOT NULL AND sender = ?5)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?6)
                           )
                       )
                   )
                 ORDER BY id DESC
                 LIMIT ?7",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn list_message_events_time_range_impl(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let descendant_workspace_root = descendant_workspace_root.map(str::to_string);
        let start = start.to_string();
        let end = end.to_string();
        let limit = i64::try_from(limit.min(100_000)).unwrap_or(100_000);
        let system_allowed = Self::is_system_principal(&principal);
        anyhow::ensure!(
            descendant_workspace_root.is_none() || system_allowed,
            "descendant workspace event queries require a system principal"
        );
        let descendant_pattern = descendant_workspace_root.as_ref().map(|root| {
            let escaped = root.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("{escaped}{}%", std::path::MAIN_SEPARATOR)
        });
        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        let session_indices = Self::session_indices(4, 12, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE created_at >= ?9 AND created_at < ?10
                   AND (
                       visibility = 'global'
                       OR (
                           (
                               workspace_id = ?1
                               OR (?6 AND ?7 IS NOT NULL AND (
                                   workspace_id = ?7 OR workspace_id LIKE ?8 ESCAPE '\\'
                               ))
                           )
                           AND (
                               (?6 AND ?7 IS NOT NULL)
                               OR visibility = 'workspace'{owner_fragment}
                               OR (visibility = 'agent' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                               ))
                               OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                               OR (visibility = 'private' AND (
                                   (?2 IS NOT NULL AND agent_id = ?2)
                                   OR (?3 IS NOT NULL AND persona_id = ?3)
                                   OR (?5 IS NOT NULL AND sender = ?5)
                               ){owner_required_fragment})
                               OR (visibility = 'system' AND ?6)
                           )
                       )
                 )
                 ORDER BY created_at ASC, id ASC
                 LIMIT ?11",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &descendant_workspace_root,
                &descendant_pattern,
                &start,
                &end,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn load_recent_shared_context_impl(
        &self,
        query: SharedContextQuery,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = query.principal;
        let limit = Self::sanitize_conversation_limit(query.limit);
        let after_id = query.since_event_id.unwrap_or(0);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?5` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
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
                               visibility = 'workspace'{owner_required_fragment}
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
                   )
                 ORDER BY id DESC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            if include_roles.is_empty() {
                return Ok(events);
            }
            Ok(events
                .into_iter()
                .filter(|event| include_roles.contains(&event.role.to_ascii_lowercase()))
                .collect())
        })
        .await?
    }

    pub(super) async fn retrieve_decision_session_impl(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionSessionHit>> {
        anyhow::ensure!(
            (1..=5).contains(&limit) && !before_event_id.is_empty() && before_event_id.len() <= 128,
            "invalid decision Session bounds"
        );
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "decision Session deadline expired"
        );
        let session_key = principal
            .session_key
            .as_ref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("selected Session requires a session"))?
            .clone();
        anyhow::ensure!(
            crate::memory::owner_predicate::has_owner_id(principal),
            "selected Session requires an authenticated owner"
        );
        let principal = principal.clone();
        let before_event_id = before_event_id.to_owned();
        let db_path = self.db_path.clone();
        let limit = i64::from(limit);
        let system_allowed = Self::is_system_principal(&principal);
        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?3` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Vec<crate::memory::traits::DecisionSessionHit>> {
                let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
                let before_id: i64 = conn
                    .query_row(
                        "SELECT id FROM message_events WHERE event_id = ?1 AND workspace_id = ?2
                 AND session_key = ?3 AND owner_id = ?4 AND role = 'user' AND event_type = 'message.created'",
                        params![before_event_id, principal.workspace_id, session_key, principal.owner_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("selected Session source event is unavailable"))?;
                let mut stmt = conn.prepare(&format!(
                    "SELECT event_id, role, content, created_at
                 FROM message_events
                 WHERE id < ?1
                   AND workspace_id = ?2
                   AND {session_fragment}
                   AND (
                       {shared_tiers_arm}
                       OR (visibility = 'agent' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                       ))
                       OR (visibility = 'session'{owner_fragment})
                       OR (visibility = 'private' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                           OR (?6 IS NOT NULL AND sender = ?6)
                       ){owner_required_fragment})
                       OR (visibility = 'system' AND ?7)
                   )
                   AND role IN ('user', 'assistant') AND event_type = 'message.created'
                   AND length(CAST(event_id AS BLOB)) <= 128
                   AND length(CAST(content AS BLOB)) <= 4096
                   AND length(CAST(created_at AS BLOB)) <= 64
                 ORDER BY id DESC
                 LIMIT ?8",
                    session_fragment = session_fragment.sql,
                    owner_fragment = owner_fragment.sql,
                    owner_required_fragment = owner_required_fragment.sql,
                ))?;
                let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                    &before_id,
                    &principal.workspace_id,
                    &session_key,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.sender,
                    &system_allowed,
                    &limit,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                let rows = stmt.query_map(bind.as_slice(), |row| {
                    Ok(crate::memory::traits::DecisionSessionHit {
                        event_id: row.get(0)?,
                        role: row.get(1)?,
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })?;
                let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "decision Session deadline expired"
                );
                hits.reverse();
                Ok(hits)
            },
        )
        .await?
    }

    pub(super) async fn load_recent_session_context_impl(
        &self,
        query: SessionContextQuery,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let Some(session_key) = query.principal.session_key.clone() else {
            return Ok(Vec::new());
        };
        let pool = self.pool.clone();
        let principal = query.principal;
        let limit = Self::sanitize_conversation_limit(query.limit);
        let after_id = query.since_event_id.unwrap_or(0);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::BTreeSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `?3` binding; legacy key(s) bind at trailing
        // placeholders starting at `?9` (after limit at `?8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);
        let owner_index = Self::owner_index(9, legacy_session_keys.len());
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        let first_role_index = owner_index + usize::from(owner_required_fragment.bound_owner);
        let role_fragment = if include_roles.is_empty() {
            "1 = 1".to_string()
        } else {
            format!(
                "role IN ({})",
                (first_role_index..first_role_index + include_roles.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                 FROM message_events
                 WHERE id > ?1
                   AND workspace_id = ?2
                   AND {session_fragment}
                   AND {role_fragment}
                   AND (
                       {shared_tiers_arm}
                       OR (visibility = 'agent' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                       ))
                       OR (visibility = 'session'{owner_fragment})
                       OR (visibility = 'private' AND (
                           (?4 IS NOT NULL AND agent_id = ?4)
                           OR (?5 IS NOT NULL AND persona_id = ?5)
                           OR (?6 IS NOT NULL AND sender = ?6)
                       ){owner_required_fragment})
                       OR (visibility = 'system' AND ?7)
                   )
                 ORDER BY id DESC
                 LIMIT ?8",
                session_fragment = session_fragment.sql,
                role_fragment = role_fragment,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &session_key,
                &principal.agent_id,
                &principal.persona_id,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            for role in &include_roles {
                bind.push(role);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;

            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            Ok(events)
        })
        .await?
    }

    pub(super) async fn append_memory_event_impl(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryEvent> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            conn.execute(
                "INSERT OR IGNORE INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    event_id,
                    input.workspace_id,
                    input.owner_id,
                    input.event_type,
                    input.subject_table,
                    input.subject_id,
                    input.session_key,
                    input.run_id,
                    input.parent_run_id,
                    input.agent_id,
                    input.persona_id,
                    input.visibility.as_str(),
                    input.payload_json,
                    now
                ],
            )?;

            let event = conn.query_row(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                 FROM memory_events
                 WHERE event_id = ?1
                 LIMIT 1",
                params![event_id],
                Self::memory_event_from_row,
            )?;
            Ok(event)
        })
        .await?
    }

    pub(super) async fn list_memory_events_since_impl(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

        // C03: the owner parameter binds at the first free trailing slot (`?8`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            8,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            8,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let system_allowed =
                principal.agent_id.as_deref() == Some("system") || principal.persona_id.as_deref() == Some("system");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                   FROM memory_events
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
                                OR (visibility = 'session' AND ?5 IS NOT NULL AND session_key = ?5{owner_fragment})
                                OR (visibility = 'private' AND (
                                    (?3 IS NOT NULL AND agent_id = ?3)
                                    OR (?4 IS NOT NULL AND persona_id = ?4)
                                    OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                                ){owner_required_fragment})
                                OR (visibility = 'system' AND ?6)
                            )
                        )
                    )
                  ORDER BY id ASC
                  LIMIT ?7",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &system_allowed,
                &limit,
            ];
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::memory_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn list_memory_events_recent_impl(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

        // C03: the owner parameter binds at the first free trailing slot (`?7`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            7,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            7,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let system_allowed =
                principal.agent_id.as_deref() == Some("system") || principal.persona_id.as_deref() == Some("system");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                   FROM memory_events
                  WHERE (
                        visibility = 'global'
                        OR (
                            workspace_id = ?1
                            AND (
                                visibility = 'workspace'{owner_fragment}
                                OR (visibility = 'agent' AND (
                                    (?2 IS NOT NULL AND agent_id = ?2)
                                    OR (?3 IS NOT NULL AND persona_id = ?3)
                                ))
                                OR (visibility = 'session' AND ?4 IS NOT NULL AND session_key = ?4{owner_fragment})
                                OR (visibility = 'private' AND (
                                    (?2 IS NOT NULL AND agent_id = ?2)
                                    OR (?3 IS NOT NULL AND persona_id = ?3)
                                    OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                                ){owner_required_fragment})
                                OR (visibility = 'system' AND ?5)
                            )
                        )
                    )
                  ORDER BY id DESC
                  LIMIT ?6",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            ))?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &principal.workspace_id,
                &principal.agent_id,
                &principal.persona_id,
                &principal.session_key,
                &system_allowed,
                &limit,
            ];
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::memory_event_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }
}

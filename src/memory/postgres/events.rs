//! Conversation turns and message-event writes.
use super::{PG_DIALECT, PostgresMemory};
use crate::memory::traits::{ConversationTurn, MemoryPrincipal, MessageEvent, MessageEventInput};
use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

impl PostgresMemory {
    pub(super) async fn list_conversation_turns_ordered(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
        recorded_order: bool,
    ) -> Result<Vec<ConversationTurn>> {
        let client = self.client.clone();
        let turns_table = self.qualified_conversation_turns_table();
        let events_table = self.qualified_message_events_table.clone();
        let ordering = if recorded_order {
            format!(
                "timestamp::timestamptz DESC, COALESCE((SELECT event.id FROM {events_table} event WHERE event.event_id = {turns_table}.message_event_id), id) DESC, id DESC"
            )
        } else {
            "id DESC".to_owned()
        };
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let workspace_id = principal.workspace_id.trim().to_string();
        let session_key = session_key.to_string();
        #[allow(clippy::cast_possible_wrap)]
        let limit = limit.clamp(1, 500) as i64;
        #[allow(clippy::cast_possible_wrap)]
        let offset = offset.min(100_000) as i64;
        let system_allowed = Self::is_system_principal(principal);
        let owner_isolated = principal.require_owner_match;
        // D4 read-merge: the explicit `session_key` arg is the canonical key
        // (bound at `$1`); the principal may carry a distinct legacy key bound
        // at trailing placeholders starting at `$6`.
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
            crate::memory::session_predicate::session_key_match_fragment(PG_DIALECT, &session_indices);

        tokio::task::spawn_blocking(move || -> Result<Vec<ConversationTurn>> {
            let stmt = format!(
                "SELECT id, session_key, role, content, timestamp, message_id
                 FROM {turns_table}
                 WHERE {session_fragment}
                   AND (
                       $4
                       OR owner_id = $5
                       OR owner_id IS NULL
                       OR owner_id = 'legacy:' || session_key
                   )
                 ORDER BY {ordering}
                 LIMIT $2 OFFSET $3",
                session_fragment = session_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    system_allowed,
                    Some(&workspace_id),
                    owner_id.as_deref(),
                    owner_isolated,
                )?;
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> =
                    vec![&session_key, &limit, &offset, &system_allowed, &owner_id];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                let rows = tx.query(&stmt, &bind)?;
                tx.commit()?;
                Ok(rows)
            })?;
            let mut turns: Vec<ConversationTurn> = rows
                .iter()
                .map(|row| ConversationTurn {
                    id: row.get(0),
                    session_key: row.get(1),
                    role: row.get(2),
                    content: row.get(3),
                    timestamp: row.get(4),
                    message_id: row.get(5),
                })
                .collect();
            turns.reverse();
            Ok(turns)
        })
        .await?
    }

    pub(super) async fn append_message_event_ordinary(&self, input: MessageEventInput) -> Result<MessageEvent> {
        input.validate()?;
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MessageEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_hash = Self::content_hash(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let source = input.source.as_str().to_string();
            let source_ref_json = serde_json::to_string(&input.source)?;
            let subject_ref_json = input.subject.as_ref().map(serde_json::to_string).transpose()?;
            let config_generation_id = input.config_generation_id.map(i64::try_from).transpose()?;

            let insert_stmt = format!(
                "
                INSERT INTO {qualified_message_events_table} (
                    event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                    $21, $22, $23, $24, $25, $26, $27, $28, $29, $30,
                    $31
                )
                ON CONFLICT DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE event_id = $1
                   OR ($2::TEXT IS NOT NULL AND workspace_id = $3 AND idempotency_key = $2)
                ORDER BY CASE WHEN event_id = $1 THEN 0 ELSE 1 END
                LIMIT 1
                "
            );
            let outbox_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, 'message_events', $5, $6, $7, $8, $9, NULL, $10)
                ON CONFLICT (event_id) DO NOTHING
                "
            );

            client.with_client(|client| {
                let mut tx = client.transaction()?;
                let inserted = tx.execute(
                    &insert_stmt,
                    &[
                        &event_id,
                        &input.idempotency_key,
                        &input.workspace_id,
                        &input.owner_id,
                        &source,
                        &input.channel,
                        &input.session_key,
                        &input.parent_session_key,
                        &input.run_id,
                        &input.parent_run_id,
                        &input.agent_id,
                        &input.persona_id,
                        &input.sender,
                        &input.recipient,
                        &input.role,
                        &input.event_type,
                        &source_ref_json,
                        &subject_ref_json,
                        &input.goal_id,
                        &input.causation_event_id,
                        &input.correlation_id,
                        &input.attempt_id,
                        &input.lease_epoch,
                        &config_generation_id,
                        &input.config_source_revision,
                        &input.content,
                        &content_hash,
                        &input.raw_payload_json,
                        &visibility,
                        &now,
                        &now,
                    ],
                )?;
                let row = tx.query_one(&select_stmt, &[&event_id, &input.idempotency_key, &input.workspace_id])?;
                let event = Self::row_to_message_event(&row)?;
                if inserted > 0 {
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &event.workspace_id,
                            &event.owner_id,
                            &event.event_type,
                            &event.event_id,
                            &event.session_key,
                            &event.agent_id,
                            &event.persona_id,
                            &event.visibility.as_str(),
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(event)
            })
        })
        .await?
    }
}

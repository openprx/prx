//! The `Memory` trait implementation for `PostgresMemory`.
use super::rows::chat_profile_from_pg_row;
use super::{PG_DIALECT, POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP, PostgresMemory, scope_params_to_strings};
use crate::memory::{
    principal::{ChatType, MemoryWriteContext, classify_memory},
    traits::{
        ChatProfile, CompactionRun, CompactionRunInput, ConversationTurn, DocumentChunkRecord, DocumentIngestInput,
        DocumentRecord, DocumentSearchResult, Memory, MemoryCategory, MemoryDraft, MemoryDraftInput, MemoryEntry,
        MemoryEvent, MemoryEventInput, MemoryLink, MemoryLinkInput, MemoryPrincipal, MemoryReadMode,
        MemoryStoreMetadata, MessageEvent, MessageEventInput, RetrievalTrace, RetrievalTraceInput, SessionContextQuery,
        SharedContextQuery, validate_memory_write_target,
    },
    vector,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

#[async_trait]
impl Memory for PostgresMemory {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> Result<()> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let title = title.map(str::to_string);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let stmt = format!(
                "INSERT INTO {table}
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, NULL, NULL, '[]', 'auto', $6, $6)
                 ON CONFLICT (channel, chat_id) DO UPDATE SET
                    chat_kind = EXCLUDED.chat_kind,
                    title = CASE
                        WHEN EXCLUDED.title IS NOT NULL THEN EXCLUDED.title
                        ELSE {table}.title
                    END,
                    updated_at = EXCLUDED.updated_at"
            );
            client.with_client(|client| {
                client.execute(&stmt, &[&id, &channel, &chat_id, &chat_kind, &title, &now])?;
                Ok(())
            })
        })
        .await?
    }

    async fn update_chat_profile(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        purpose: Option<&str>,
        notes: Option<&str>,
        tags: Option<&[String]>,
        updated_by: &str,
    ) -> Result<ChatProfile> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let chat_kind = chat_kind.to_string();
        let purpose = purpose.map(str::to_string);
        let notes = notes.map(str::to_string);
        let tags_json = tags.map(serde_json::to_string).transpose()?;
        let updated_by = updated_by.to_string();
        tokio::task::spawn_blocking(move || -> Result<ChatProfile> {
            let now = Utc::now().to_rfc3339();
            let id = Uuid::new_v4().to_string();
            let stmt = format!(
                "INSERT INTO {table}
                    (id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, NULL, $5, $6, COALESCE($7, '[]'), $8, $9, $9)
                 ON CONFLICT (channel, chat_id) DO UPDATE SET
                    chat_kind = EXCLUDED.chat_kind,
                    purpose = COALESCE(EXCLUDED.purpose, {table}.purpose),
                    notes = COALESCE(EXCLUDED.notes, {table}.notes),
                    tags = COALESCE($7, {table}.tags),
                    updated_by = EXCLUDED.updated_by,
                    updated_at = EXCLUDED.updated_at
                 RETURNING id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at"
            );
            client.with_client(|client| {
                let row = client.query_one(
                    &stmt,
                    &[&id, &channel, &chat_id, &chat_kind, &purpose, &notes, &tags_json, &updated_by, &now],
                )?;
                Ok(chat_profile_from_pg_row(&row))
            })
        })
        .await?
    }

    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> Result<Option<ChatProfile>> {
        let client = self.client.clone();
        let table = self.qualified_chat_profiles_table();
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<ChatProfile>> {
            let stmt = format!(
                "SELECT id, channel, chat_id, chat_kind, title, purpose, notes, tags, updated_by, created_at, updated_at
                 FROM {table}
                 WHERE channel = $1 AND chat_id = $2"
            );
            client.with_client(|client| {
                let row = client.query_opt(&stmt, &[&channel, &chat_id])?;
                Ok(row.as_ref().map(chat_profile_from_pg_row))
            })
        })
        .await?
    }

    async fn store(&self, key: &str, content: &str, category: MemoryCategory, session_id: Option<&str>) -> Result<()> {
        self.store_with_metadata(key, content, category, session_id, MemoryStoreMetadata::default())
            .await
    }

    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> Result<()> {
        self.store_with_context_and_metadata(
            key,
            content,
            category,
            session_id,
            context,
            MemoryStoreMetadata::default(),
        )
        .await
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> Result<()> {
        validate_memory_write_target(key, session_id)?;
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) = self
            .embedding_metadata_for_category(&category, content, None, None, true)
            .await?;
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let pgvector_available = self.pgvector_available;
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now();
            let stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source,
                        embedding, embedding_provider, embedding_model, embedding_dimensions
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source,
                    embedding = EXCLUDED.embedding,
                    embedding_provider = EXCLUDED.embedding_provider,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_dimensions = EXCLUDED.embedding_dimensions
                "
            );

            let id = Uuid::new_v4().to_string();
            client.with_client(|client| {
                client.execute(
                    &stmt,
                    &[
                        &id,
                        &key,
                        &content,
                        &category,
                        &now,
                        &now,
                        &sid,
                        &metadata.workspace_id,
                        &metadata.owner_id,
                        &metadata.agent_id,
                        &metadata.persona_id,
                        &metadata.source_event_id,
                        &metadata.source,
                        &embedding,
                        &embedding_provider,
                        &embedding_model,
                        &embedding_dimensions,
                    ],
                )?;
                if pgvector_available {
                    let update_vector_stmt =
                        format!("UPDATE {qualified_table} SET embedding_vector = $1::text::vector WHERE key = $2");
                    let clear_vector_stmt =
                        format!("UPDATE {qualified_table} SET embedding_vector = NULL WHERE key = $1");
                    if let Some(embedding_vector) = embedding_vector.as_ref() {
                        client.execute(&update_vector_stmt, &[embedding_vector, &key])?;
                    } else {
                        client.execute(&clear_vector_stmt, &[&key])?;
                    }
                }
                Ok(())
            })?;
            Ok(())
        })
        .await?
    }

    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> Result<()> {
        let Some(context) = context.cloned() else {
            return self
                .store_with_metadata(key, content, category, session_id, metadata)
                .await;
        };

        validate_memory_write_target(key, session_id)?;
        // V02-R9: an owner-isolated write is confined to the caller's own keys,
        // exactly as on SQLite. A blank owner id refuses before any SQL runs,
        // and the isolated row is attributed to the authenticated owner alone.
        let isolated_owner = super::super::owner_predicate::isolated_store_owner(Some(&context))?;
        let mut metadata = metadata;
        if let Some(owner) = isolated_owner.as_ref() {
            metadata.owner_id = Some(owner.clone());
        }
        let isolation_workspace = metadata.workspace_id.clone().or_else(|| context.workspace_id.clone());
        let isolation_owner = metadata.owner_id.clone().or_else(|| {
            context.sender_id.clone().or_else(|| {
                context
                    .channel
                    .as_ref()
                    .zip(context.raw_sender.as_ref())
                    .map(|(channel, sender)| format!("anonymous:{channel}:{sender}"))
            })
        });
        let isolation_system = isolation_owner
            .as_deref()
            .is_some_and(super::super::principal::is_system_principal);
        let (embedding, embedding_provider, embedding_model, embedding_dimensions, embedding_vector) = self
            .embedding_metadata_for_category(
                &category,
                content,
                isolation_workspace.as_deref(),
                isolation_owner.as_deref(),
                isolation_system,
            )
            .await?;
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let pgvector_available = self.pgvector_available;
        let key = key.to_string();
        let content = content.to_string();
        let category = Self::category_to_str(&category);
        let sid = session_id.map(str::to_string);
        let rls_owner_isolated = isolated_owner.is_some();
        let owner_guard =
            super::super::owner_predicate::store_conflict_guard(PG_DIALECT, &qualified_table, isolated_owner.is_some());

        tokio::task::spawn_blocking(move || -> Result<()> {
            let now = Utc::now();
            let principal = Self::principal_from_context(&context);
            let classified = classify_memory(&context, &content, &principal);
            let risk_json = serde_json::to_string(&classified.risk_signals)?;
            let chat_type = context
                .chat_type
                .as_deref()
                .map(ChatType::from_str)
                .map(|chat_type| chat_type.as_str().to_string());
            let sender_id = context.sender_id.clone().or_else(|| {
                if context.channel.is_some() && context.raw_sender.is_some() {
                    Some(principal.user_id.clone())
                } else {
                    None
                }
            });
            let owner_id = metadata.owner_id.clone().or_else(|| sender_id.clone());
            let workspace_id = metadata.workspace_id.clone().or_else(|| context.workspace_id.clone());
            let rls_system = super::super::principal::is_system_principal(&principal.user_id);
            // FIX-P0-23 (#4 F4): persist topic_id instead of hard-coded NULL.
            // Prefer the explicit metadata topic_id; fall back to source_event_id
            // (source_event_id as the fallback) so a write still threads back to its
            // originating event for Project-visibility scope resolution.
            let topic_id = metadata.topic_id.clone().or_else(|| metadata.source_event_id.clone());
            let stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source,
                        embedding, embedding_provider, embedding_model, embedding_dimensions,
                        channel, chat_type, chat_id, sender_id, raw_sender, topic_id,
                        visibility, sensitivity, risk_signals, policy_version
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source,
                    embedding = EXCLUDED.embedding,
                    embedding_provider = EXCLUDED.embedding_provider,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_dimensions = EXCLUDED.embedding_dimensions,
                    channel = EXCLUDED.channel,
                    chat_type = EXCLUDED.chat_type,
                    chat_id = EXCLUDED.chat_id,
                    sender_id = EXCLUDED.sender_id,
                    raw_sender = EXCLUDED.raw_sender,
                    topic_id = EXCLUDED.topic_id,
                    visibility = EXCLUDED.visibility,
                    sensitivity = EXCLUDED.sensitivity,
                    risk_signals = EXCLUDED.risk_signals,
                    policy_version = EXCLUDED.policy_version{owner_guard}
                "
            );

            let id = Uuid::new_v4().to_string();
            let refused = || -> anyhow::Error {
                super::super::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable { key: key.clone() }.into()
            };
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    workspace_id.as_deref(),
                    owner_id.as_deref(),
                    rls_owner_isolated,
                )?;
                let written = tx.execute(
                    &stmt,
                    &[
                        &id,
                        &key,
                        &content,
                        &category,
                        &now,
                        &now,
                        &sid,
                        &workspace_id,
                        &owner_id,
                        &metadata.agent_id,
                        &metadata.persona_id,
                        &metadata.source_event_id,
                        &metadata.source,
                        &embedding,
                        &embedding_provider,
                        &embedding_model,
                        &embedding_dimensions,
                        &context.channel,
                        &chat_type,
                        &context.chat_id,
                        &sender_id,
                        &context.raw_sender,
                        &topic_id,
                        &classified.visibility.as_str(),
                        &classified.sensitivity.as_str(),
                        &risk_json,
                        &classified.policy_version,
                    ],
                );
                // An isolated write that meets another owner's key ends in one
                // of two ways, and both are the same refusal: the conflict
                // guard suppresses the update (zero rows), or the
                // `memories_owner_isolation` policy rejects the existing row
                // outright (`42501`) — which of the two PostgreSQL reports for
                // a non-shared row is pinned by
                // `postgres_owner_isolated_store_refuses_a_cross_owner_upsert`.
                let written = match written {
                    Ok(rows) => rows,
                    Err(error)
                        if isolated_owner.is_some()
                            && error.code() == Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) =>
                    {
                        return Err(refused());
                    }
                    Err(error) => return Err(error.into()),
                };
                if isolated_owner.is_some() && written == 0 {
                    return Err(refused());
                }
                if pgvector_available {
                    let update_vector_stmt = format!(
                        "UPDATE {qualified_table} SET embedding_vector = $1::text::vector \
                         WHERE key = $2 AND workspace_id = $3 AND owner_id = $4"
                    );
                    let clear_vector_stmt = format!(
                        "UPDATE {qualified_table} SET embedding_vector = NULL \
                         WHERE key = $1 AND workspace_id = $2 AND owner_id = $3"
                    );
                    if let Some(embedding_vector) = embedding_vector.as_ref() {
                        tx.execute(&update_vector_stmt, &[embedding_vector, &key, &workspace_id, &owner_id])?;
                    } else {
                        tx.execute(&clear_vector_stmt, &[&key, &workspace_id, &owner_id])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })?;
            Ok(())
        })
        .await?
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);
        let query_embedding = if query.is_empty() {
            None
        } else {
            self.get_or_compute_embedding(&query, None, None, true).await?
        };
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();
        let vector_weight = f64::from(self.vector_weight);
        let keyword_weight = f64::from(self.keyword_weight);
        let pgvector_available = self.pgvector_available;
        let query_vector = query_embedding
            .as_ref()
            .and_then(|embedding| Self::pgvector_literal(embedding));

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       (
                         CASE WHEN key ILIKE '%' || $1 || '%' THEN 2.0 ELSE 0.0 END +
                         CASE WHEN content ILIKE '%' || $1 || '%' THEN 1.0 ELSE 0.0 END
                       ) AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($2::TEXT IS NULL OR session_id = $2)
                  AND ($1 = '' OR key ILIKE '%' || $1 || '%' OR content ILIKE '%' || $1 || '%')
                ORDER BY score DESC, updated_at DESC
                LIMIT $3
                "
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = limit as i64;

            let rows = client.with_client(|client| Ok(client.query(&stmt, &[&query, &sid, &limit_i64])?))?;
            let mut by_id = std::collections::HashMap::<String, MemoryEntry>::new();
            for row in &rows {
                let mut entry = Self::row_to_entry(row)?;
                entry.score = entry.score.map(|score| score * keyword_weight);
                by_id.insert(entry.id.clone(), entry);
            }

            if let Some(query_embedding) = query_embedding {
                if pgvector_available {
                    if let Some(query_vector) = query_vector.as_ref() {
                        let vector_stmt = format!(
                            "
                            SELECT id, key, content, category, created_at, session_id,
                                   (1.0 - (embedding_vector <=> $1::text::vector))::DOUBLE PRECISION AS score,
                                   useful_count
                            FROM {qualified_table}
                            WHERE embedding_vector IS NOT NULL
                              AND embedding_provider = $2
                              AND embedding_model = $3
                              AND embedding_dimensions = $4
                              AND ($5::TEXT IS NULL OR session_id = $5)
                            ORDER BY embedding_vector <=> $1::text::vector
                            LIMIT $6
                            "
                        );
                        let candidate_limit = limit_i64.saturating_mul(4).max(limit_i64);
                        let vector_rows = client.with_client(|client| {
                            Ok(client.query(
                                &vector_stmt,
                                &[
                                    query_vector,
                                    &embedding_provider,
                                    &embedding_model,
                                    &embedding_dimensions_i64,
                                    &sid,
                                    &candidate_limit,
                                ],
                            )?)
                        })?;
                        for row in &vector_rows {
                            let score: f64 = row.try_get(6).unwrap_or(0.0);
                            let score = score * vector_weight;
                            if score <= 0.0 {
                                continue;
                            }
                            let mut entry = Self::row_to_entry(row)?;
                            entry.score = Some(score);
                            by_id
                                .entry(entry.id.clone())
                                .and_modify(|existing| {
                                    let combined = existing.score.unwrap_or(0.0) + score;
                                    existing.score = Some(combined);
                                })
                                .or_insert(entry);
                        }
                    }
                } else {
                    // FIX-P2-03: BYTEA fallback (no pgvector). SQL *cannot* order
                    // by vector distance here, so we fetch a bounded candidate set
                    // and re-rank it in-process by cosine similarity below. The
                    // LIMIT is a pure safety valve against an unbounded full-table
                    // scan (DoS), NOT a relevance filter — it is deliberately set far
                    // larger than the pgvector path's `4 * limit` so the in-process
                    // re-rank still sees enough genuinely-similar rows and we do not
                    // truncate "older but more similar" records on recency. The cap
                    // applies *after* the scope/owner/session WHERE filter. Ordering
                    // by id keeps the (cap-only) truncation deterministic without
                    // masquerading recency as similarity. This is the documented
                    // degradation tradeoff of running without pgvector.
                    let candidate_limit = POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP;
                    let vector_stmt = format!(
                        "
                        SELECT id, key, content, category, created_at, session_id,
                               NULL::DOUBLE PRECISION AS score,
                               useful_count,
                               embedding
                        FROM {qualified_table}
                        WHERE embedding IS NOT NULL
                          AND embedding_provider = $1
                          AND embedding_model = $2
                          AND embedding_dimensions = $3
                          AND ($4::TEXT IS NULL OR session_id = $4)
                        ORDER BY id
                        LIMIT $5
                        "
                    );
                    let vector_rows = client.with_client(|client| {
                        Ok(client.query(
                            &vector_stmt,
                            &[
                                &embedding_provider,
                                &embedding_model,
                                &embedding_dimensions_i64,
                                &sid,
                                &candidate_limit,
                            ],
                        )?)
                    })?;
                    for row in &vector_rows {
                        let embedding_blob: Vec<u8> = row.get(8);
                        let embedding = vector::bytes_to_vec(&embedding_blob);
                        if embedding.len() != embedding_dimensions {
                            tracing::debug!(
                                memory_id = %row.get::<_, String>(0),
                                expected_dimensions = embedding_dimensions,
                                actual_dimensions = embedding.len(),
                                "Skipping stale Postgres memory embedding with mismatched dimensions"
                            );
                            continue;
                        }
                        let score = f64::from(vector::cosine_similarity(&query_embedding, &embedding)) * vector_weight;
                        if score <= 0.0 {
                            continue;
                        }
                        let mut entry = Self::row_to_entry(row)?;
                        entry.score = Some(score);
                        by_id
                            .entry(entry.id.clone())
                            .and_modify(|existing| {
                                let combined = existing.score.unwrap_or(0.0) + score;
                                existing.score = Some(combined);
                            })
                            .or_insert(entry);
                    }
                }
            }

            let mut entries: Vec<MemoryEntry> = by_id.into_values().collect();
            entries.sort_by(|a, b| {
                let left = a.score.unwrap_or(0.0);
                let right = b.score.unwrap_or(0.0);
                right.partial_cmp(&left).unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(limit.max(1));
            Ok(entries)
        })
        .await?
    }

    async fn read_router_history(
        &self,
        evidence: super::super::router_history::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::super::router_history::RouterHistoryRead> {
        self.router_history_read(evidence, deadline).await
    }
    async fn commit_router_history(
        &self,
        evidence: &super::super::router_history::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::super::router_history::RouterHistoryCommitReceipt> {
        self.router_history_commit(evidence, deadline).await
    }
    async fn read_router_learning(
        &self,
        evidence: &super::super::traits::RouterLearningEvidence,
        deadline: Instant,
    ) -> Result<super::super::traits::RouterLearningRead> {
        self.router_learning_transaction(evidence, None, deadline)
            .await
            .map(|(read, _)| read)
    }
    async fn commit_router_learning(
        &self,
        evidence: &super::super::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &super::super::traits::RouterLearningSnapshot,
        deadline: Instant,
    ) -> Result<super::super::traits::RouterLearningCommit> {
        let (read, conflict) = self
            .router_learning_transaction(evidence, Some((expected_version, next.clone())), deadline)
            .await?;
        if conflict {
            Ok(super::super::traits::RouterLearningCommit::Conflict)
        } else {
            Ok(super::super::traits::RouterLearningCommit::Committed(
                read.snapshot.context("Router committed snapshot missing")?,
            ))
        }
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: Instant,
    ) -> Result<u32> {
        self.count_decision_rehearsal(query, limit, context, deadline).await
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> Result<Vec<super::super::traits::DecisionMemoryHit>> {
        self.read_decision_memory(query, limit, context, session_id, deadline)
            .await
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: Instant,
    ) -> Result<Vec<super::super::traits::DecisionDocumentHit>> {
        self.read_decision_documents(query, limit, principal, deadline).await
    }

    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: Instant,
    ) -> Result<Vec<super::super::traits::DecisionSessionHit>> {
        self.read_decision_session(principal, before_event_id, limit, deadline)
            .await
    }

    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> Result<Vec<MemoryEntry>> {
        let Some(context) = context.cloned() else {
            return self.recall(query, limit, session_id).await;
        };
        // C03 / V02-R5 (F1): an owner-isolated caller with no usable owner id
        // reaches nothing owned. Decide that here, before the RLS context is
        // applied, so the answer is the empty set the SQLite adapter returns
        // rather than the "requires a trusted owner context" error
        // `apply_rls_context_raw` would raise for a missing owner.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory recall carries no owner id; returning no rows");
            return Ok(Vec::new());
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();

        // FIX-P0-22 (#3 F3): resolve a full ACL principal (identity_bindings +
        // user_policies) so blocked_patterns and role are populated, then apply
        // the post_filter after the SQL visibility scope (parity with SQLite).
        let principal = self.resolve_principal_from_context(&context).await;

        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);
        // D11: the visibility scope is now derived from the resolved `principal`
        // (shared predicate), so the raw context triple is no longer bound into
        // the recall SQL. `sender_id` is still needed for the RLS owner context.
        let sender_id = context.sender_id.clone().or_else(|| {
            if context.channel.is_some() && context.raw_sender.is_some() {
                Some(format!(
                    "anonymous:{}:{}",
                    context.channel.as_deref().unwrap_or("unknown"),
                    context.raw_sender.as_deref().unwrap_or("unknown")
                ))
            } else {
                None
            }
        });
        // Over-fetch so the post_filter has candidates to remove without
        // starving the requested limit.
        let fetch_limit = limit.saturating_mul(3).max(limit);

        // FIX-P3-04: capture the RLS context so the DB-layer owner policy is set
        // before the read (defense-in-depth behind the SQL visibility scope).
        // FIX-P3-04: the RLS owner mirrors the row `owner_id`, which writes set to
        // the effective `sender_id` (see store_with_context_and_metadata). System
        // principals (resolved via the ACL user_id) get the policy bypass.
        let rls_system = super::super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5: for an owner-isolated caller the RLS owner is the
        // authenticated `owner_id` itself, so the row-level policy and the SQL
        // predicate gate on the same string by construction rather than by the
        // coincidence that these tools also copy the owner into `sender_id`.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            context.owner_id.as_deref()
        } else {
            sender_id.as_deref()
        }
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_string);

        // D11: render the ACL visibility scope from the shared, dialect-agnostic
        // predicate (single source with SQLite). The fixed parameters are
        // $1=query, $2=session_id, $3=limit, so the scope predicate is rendered
        // with `$N` starting at $4 and its values appended after them. This
        // tightens the Postgres scope to the SQLite canonical truth table,
        // removing the previously over-permissive `visibility IS NULL` /
        // `visibility IN ('workspace', ...)` allow paths.
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(4);
        let scope_values = scope_params_to_strings(scope_params);

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       (
                         CASE WHEN key ILIKE '%' || $1 || '%' THEN 2.0 ELSE 0.0 END +
                         CASE WHEN content ILIKE '%' || $1 || '%' THEN 1.0 ELSE 0.0 END
                       ) AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($2::TEXT IS NULL OR session_id = $2)
                  AND ($1 = '' OR key ILIKE '%' || $1 || '%' OR content ILIKE '%' || $1 || '%')
                  AND ({scope_sql})
                ORDER BY score DESC, updated_at DESC
                LIMIT $3
                "
            );

            #[allow(clippy::cast_possible_wrap)]
            let limit_i64 = fetch_limit.max(1) as i64;
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&query, &sid, &limit_i64];
                for value in &scope_values {
                    params.push(value);
                }
                let rows = tx.query(&stmt, &params)?;
                tx.commit()?;
                Ok(rows)
            })?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await??;

        let mut visible = super::super::principal::post_filter(rows, &principal, |entry| entry.content.as_str());
        visible.truncate(limit.max(1));
        Ok(visible)
    }

    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> Result<Vec<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 defence in depth: an owner-isolated context is enforced
        // whatever mode was requested (parity with the SQLite adapter).
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self
            .recall_with_context(query, limit, session_id, Some(&context))
            .await?;
        let unrestricted = self.recall(query, limit, session_id).await?;
        let would_deny = unrestricted
            .iter()
            .any(|entry| !scoped.iter().any(|allowed| allowed.id == entry.id));
        let selected = match mode {
            MemoryReadMode::Enforce => scoped,
            MemoryReadMode::Observe => unrestricted,
        };
        let principal = self.resolve_principal_from_context(&context).await;
        Self::log_access_best_effort(
            Arc::clone(&self.client),
            self.qualified_access_audit_log_table(),
            principal,
            "search",
            Some(query.to_string()),
            None,
            Some(match mode {
                MemoryReadMode::Enforce => "acl_enforced",
                MemoryReadMode::Observe => "observe_mode",
            }),
            if mode == MemoryReadMode::Observe && would_deny {
                "would_deny"
            } else if selected.is_empty() {
                "no_results"
            } else {
                "allowed"
            },
        )
        .await;
        Ok(selected)
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       NULL::DOUBLE PRECISION AS score,
                       useful_count
                FROM {qualified_table}
                WHERE key = $1
                LIMIT 1
                "
            );

            let row = client.with_client(|client| Ok(client.query_opt(&stmt, &[&key])?))?;
            row.as_ref().map(Self::row_to_entry).transpose()
        })
        .await?
    }

    async fn get_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 (F1): see `recall_with_context` — the keyed read answers
        // "no entry", not an error, when an isolated caller has no owner id.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory keyed read carries no owner id; returning no entry");
            return Ok(None);
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();
        let principal = self.resolve_principal_from_context(&context).await;
        let sender_id = context.sender_id.clone().or_else(|| {
            if context.channel.is_some() && context.raw_sender.is_some() {
                Some(format!(
                    "anonymous:{}:{}",
                    context.channel.as_deref().unwrap_or("unknown"),
                    context.raw_sender.as_deref().unwrap_or("unknown")
                ))
            } else {
                None
            }
        });
        let rls_system = super::super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5: see `recall_with_context` — an isolated caller's RLS
        // owner is its authenticated `owner_id`.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            context.owner_id.as_deref()
        } else {
            sender_id.as_deref()
        }
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_string);
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(2);
        let scope_values = scope_params_to_strings(scope_params);
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();
        let principal_for_filter = principal.clone();

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryEntry>> {
            let stmt = format!(
                "SELECT id, key, content, category, created_at, session_id,
                        NULL::DOUBLE PRECISION AS score, useful_count
                   FROM {qualified_table}
                  WHERE key = $1 AND ({scope_sql})
                  LIMIT 1"
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                let row = tx.query_opt(&stmt, &params)?;
                tx.commit()?;
                Ok(row)
            })?;
            let entry = row.as_ref().map(Self::row_to_entry).transpose()?;
            let mut visible =
                super::super::principal::post_filter(entry.into_iter().collect(), &principal_for_filter, |entry| {
                    entry.content.as_str()
                });
            Ok(visible.pop())
        })
        .await?
    }

    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5: isolation outranks the requested mode.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self.get_with_context(key, Some(&context)).await?;
        let unrestricted = self.get(key).await?;
        let would_deny = unrestricted.is_some() && scoped.is_none();
        let selected = match mode {
            MemoryReadMode::Enforce => scoped,
            MemoryReadMode::Observe => unrestricted,
        };
        let principal = self.resolve_principal_from_context(&context).await;
        Self::log_access_best_effort(
            Arc::clone(&self.client),
            self.qualified_access_audit_log_table(),
            principal,
            "get",
            None,
            selected
                .as_ref()
                .map(|entry| entry.id.clone())
                .or_else(|| Some(key.to_string())),
            Some(match mode {
                MemoryReadMode::Enforce => "acl_enforced",
                MemoryReadMode::Observe => "observe_mode",
            }),
            if mode == MemoryReadMode::Observe && would_deny {
                "would_deny"
            } else if selected.is_some() {
                "allowed"
            } else {
                "denied"
            },
        )
        .await;
        Ok(selected)
    }

    async fn list(&self, category: Option<&MemoryCategory>, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let category = category.map(Self::category_to_str);
        let sid = session_id.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEntry>> {
            let stmt = format!(
                "
                SELECT id, key, content, category, created_at, session_id,
                       NULL::DOUBLE PRECISION AS score,
                       useful_count
                FROM {qualified_table}
                WHERE ($1::TEXT IS NULL OR category = $1)
                  AND ($2::TEXT IS NULL OR session_id = $2)
                ORDER BY updated_at DESC
                "
            );

            let category_ref = category.as_deref();
            let session_ref = sid.as_deref();
            let rows = client.with_client(|client| Ok(client.query(&stmt, &[&category_ref, &session_ref])?))?;
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await?
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let category = Self::category_to_str(category);

        tokio::task::spawn_blocking(
            move || -> Result<Option<std::collections::HashMap<String, Option<String>>>> {
                let stmt = format!("SELECT id, owner_id FROM {qualified_table} WHERE category = $1");
                let rows = client.with_client(|client| Ok(client.query(&stmt, &[&category])?))?;
                Ok(Some(
                    rows.iter()
                        .map(|row| (row.get::<_, String>(0), row.get::<_, Option<String>>(1)))
                        .collect(),
                ))
            },
        )
        .await?
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> Result<bool> {
            let stmt = format!("DELETE FROM {qualified_table} WHERE key = $1");
            let deleted = client.with_client(|client| Ok(client.execute(&stmt, &[&key])?))?;
            Ok(deleted > 0)
        })
        .await?
    }

    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> Result<bool> {
        let Some(context) = context.cloned() else {
            return self.forget(key).await;
        };
        // R-B1d D-1: an owner-isolated caller with no usable owner id deletes
        // nothing. Decide that here, before the RLS context is applied — and
        // before any statement runs — so the answer is the "nothing forgotten"
        // the SQLite adapter gives rather than the "requires a trusted owner
        // context" error `apply_rls_context_raw` would raise for a missing
        // owner. Log line: operation only, no key and no content.
        if Self::isolated_read_without_owner(&context) {
            tracing::warn!("owner-isolated memory forget carries no owner id; removing nothing");
            return Ok(false);
        }
        let rls_workspace = context.workspace_id.clone().unwrap_or_default();

        // FIX-P0-22 (#3 F3): resolve the ACL principal and record an access-audit
        // entry for both allowed and denied deletes (parity with SQLite).
        let principal = self.resolve_principal_from_context(&context).await;
        let rls_system = super::super::principal::is_system_principal(&principal.user_id);
        // C03 / V02-R5 / R-B1d D-1: for an owner-isolated caller the RLS owner
        // is the authenticated `owner_id` itself, so the row-level policy and
        // the SQL predicate gate on the same string by construction. The
        // non-isolated expression is the historical one, unchanged.
        let rls_owner_isolated = context.require_owner_match;
        let rls_owner = if rls_owner_isolated {
            Self::forget_owner_anchor(&context)
        } else {
            context.sender_id.clone().or_else(|| {
                context
                    .channel
                    .as_ref()
                    .zip(context.raw_sender.as_ref())
                    .map(|(channel, sender)| format!("anonymous:{channel}:{sender}"))
            })
        };

        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let audit_table = self.qualified_access_audit_log_table();
        let key = key.to_string();

        // D11: derive the delete-visibility scope from the shared predicate
        // (single source with SQLite). The fixed parameter is $1=key, so the
        // scope predicate is rendered with `$N` starting at $2 and its values
        // appended after the key. This tightens the Postgres forget scope to the
        // SQLite canonical truth table (removing the over-permissive
        // `visibility IS NULL` / `visibility IN ('workspace', ...)` allow paths)
        // and folds the secret-exclusion into the shared predicate, so an owner
        // can delete its own secrets exactly as on SQLite.
        let (scope_sql, scope_params) = principal.build_sql_scope_pg(2);
        let scope_values = scope_params_to_strings(scope_params);
        // R-B1d D-1: the delete guard, at the next free placeholder after the
        // key and the scope values. See `Self::forget_owner_guard`.
        let owner_guard = Self::forget_owner_guard(&context, scope_values.len() + 2);
        let owner_value = owner_guard
            .bound_owner
            .then(|| Self::forget_owner_anchor(&context))
            .flatten();
        let owner_guard_sql = owner_guard.sql;

        tokio::task::spawn_blocking(move || -> Result<bool> {
            let stmt = format!(
                "
                DELETE FROM {qualified_table}
                WHERE key = $1
                  AND ({scope_sql}){owner_guard_sql}
                "
            );
            let deleted = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    rls_owner.as_deref(),
                    rls_owner_isolated,
                )?;
                let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&key];
                for value in &scope_values {
                    params.push(value);
                }
                if let Some(owner) = &owner_value {
                    params.push(owner);
                }
                let deleted = tx.execute(&stmt, &params)?;
                tx.commit()?;
                Ok(deleted)
            })?;
            let result = if deleted > 0 { "allowed" } else { "denied" };
            Self::log_access_best_effort_blocking(
                &client,
                &audit_table,
                &principal,
                "forget",
                Some(&key),
                None,
                None,
                result,
            );
            Ok(deleted > 0)
        })
        .await?
    }

    async fn count(&self) -> Result<usize> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();

        tokio::task::spawn_blocking(move || -> Result<usize> {
            let stmt = format!("SELECT COUNT(*) FROM {qualified_table}");
            let count: i64 = client.with_client(|client| Ok(client.query_one(&stmt, &[])?.get(0)))?;
            let count = usize::try_from(count).context("PostgreSQL returned a negative memory count")?;
            Ok(count)
        })
        .await?
    }

    // FIX-P0-19/20 (C5b #2): conversation persistence with owner ACL, mirrored
    // from the SQLite backend so switching to Postgres preserves the same
    // isolation guarantees. Non-system principals only see their own owner_id
    // (legacy NULL / `legacy:<session_key>` rows remain visible during the
    // phased rollout, matching SQLite's `legacy_visible = 1` default).
    #[allow(clippy::too_many_arguments)]
    async fn append_conversation_turn(
        &self,
        session_key: &str,
        channel: &str,
        sender: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        message_id: Option<&str>,
        owner_id: Option<&str>,
    ) -> Result<()> {
        let client = self.client.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        let session_key = session_key.to_string();
        let channel = channel.to_string();
        let sender = sender.to_string();
        let role = role.to_string();
        let content = content.to_string();
        let timestamp = timestamp.map(str::to_string).unwrap_or_else(|| Utc::now().to_rfc3339());
        let message_id = message_id.map(str::to_string);
        let owner_id = owner_id
            .map(str::to_string)
            .or_else(|| Some(format!("legacy:{session_key}")));
        let preview: String = content.chars().take(200).collect();

        tokio::task::spawn_blocking(move || -> Result<()> {
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                let session_sql = format!(
                    "INSERT INTO {sessions_table} (
                         session_key, channel, sender, owner_id,
                         created_at, updated_at, message_count, last_message_preview
                     ) VALUES ($1, $2, $3, $4, $5, $5, 1, $6)
                     ON CONFLICT (session_key) DO UPDATE SET
                         channel = EXCLUDED.channel,
                         sender = EXCLUDED.sender,
                         owner_id = COALESCE(EXCLUDED.owner_id, {sessions_table}.owner_id),
                         updated_at = EXCLUDED.updated_at,
                         message_count = {sessions_table}.message_count + 1,
                         last_message_preview = EXCLUDED.last_message_preview"
                );
                tx.execute(
                    &session_sql,
                    &[&session_key, &channel, &sender, &owner_id, &timestamp, &preview],
                )?;

                let turn_sql = format!(
                    "INSERT INTO {turns_table} (session_key, owner_id, role, content, timestamp, message_id)
                     VALUES ($1, $2, $3, $4, $5, $6)"
                );
                tx.execute(
                    &turn_sql,
                    &[&session_key, &owner_id, &role, &content, &timestamp, &message_id],
                )?;
                tx.commit()?;
                Ok(())
            })
        })
        .await?
    }

    async fn project_console_message(&self, event: &MessageEvent) -> Result<()> {
        event.validate_console_projection()?;
        let event = event.clone();
        let client = self.client.clone();
        let events_table = self.qualified_message_events_table.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The event row lock serializes competing retries of this exact
                // projection across backend instances, before checking history.
                let row = tx.query_one(&format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {events_table} WHERE event_id = $1 AND workspace_id = $2 AND owner_id = $3 FOR UPDATE"),
                    &[&event.event_id, &event.workspace_id, &event.owner_id])?;
                let durable = Self::row_to_message_event(&row)?;
                anyhow::ensure!(serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
                    "console projection differs from its durable event");
                let existing = tx.query(&format!("SELECT session_key, owner_id, role, content FROM {turns_table}
                    WHERE message_event_id = $1 OR message_id = $1 LIMIT 2"), &[&event.event_id])?;
                if let Some(row) = existing.first() {
                    anyhow::ensure!(existing.len() == 1
                        && Some(row.get::<_, String>(0)) == event.session_key
                        && row.get::<_, Option<String>>(1) == event.owner_id
                        && row.get::<_, String>(2) == event.role && row.get::<_, String>(3) == event.content,
                        "console message projection conflicts with its durable event");
                    return Ok(());
                }
                let preview: String = event.content.chars().take(200).collect();
                tx.execute(&format!("INSERT INTO {sessions_table} AS current_session
                    (session_key, channel, sender, owner_id, created_at, updated_at, message_count, last_message_preview)
                    VALUES ($1, $2, $3, $4, $5, $5, 1, $6)
                    ON CONFLICT(session_key) DO UPDATE SET
                        message_count = COALESCE(current_session.message_count, 0) + 1,
                        channel = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.channel ELSE current_session.channel END,
                        sender = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.sender ELSE current_session.sender END,
                        owner_id = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.owner_id ELSE current_session.owner_id END,
                        last_message_preview = CASE WHEN EXCLUDED.updated_at >= current_session.updated_at THEN EXCLUDED.last_message_preview ELSE current_session.last_message_preview END,
                        updated_at = GREATEST(current_session.updated_at, EXCLUDED.updated_at)"),
                    &[&event.session_key, &event.channel, &event.sender, &event.owner_id, &event.created_at, &preview])?;
                tx.execute(&format!("INSERT INTO {turns_table}
                    (session_key, owner_id, role, content, timestamp, message_id, message_event_id, agent_id, persona_id, visibility)
                    VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8, 'session')"),
                    &[&event.session_key, &event.owner_id, &event.role, &event.content, &event.created_at,
                        &event.event_id, &event.agent_id, &event.persona_id])?;
                tx.commit()?;
                Ok(())
            })
        }).await?
    }

    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !session_key.is_empty(),
            "console recovery scope is empty"
        );
        let workspace_id = workspace_id.to_owned();
        let session_key = session_key.to_owned();
        let limit = i64::try_from(limit.clamp(1, 128))?;
        let client = self.client.clone();
        let table = self.qualified_message_events_table.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            client.with_client(|client| {
                let rows = client.query(&format!("SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                    FROM {table} WHERE workspace_id = $1 AND session_key = $2
                    AND source = 'console' AND event_type = 'turn.finalized' AND id > $3
                    ORDER BY id ASC LIMIT $4"), &[&workspace_id, &session_key, &after_id, &limit])?;
                rows.iter().map(Self::row_to_message_event).collect()
            })
        }).await?
    }

    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, false)
            .await
    }

    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, true)
            .await
    }
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
        let client = self.client.clone();
        let sessions_table = self.qualified_sessions_table();
        let turns_table = self.qualified_conversation_turns_table();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        #[allow(clippy::cast_possible_wrap)]
        let max_turns = max_turns_per_session.clamp(1, 500) as i64;
        #[allow(clippy::cast_possible_wrap)]
        let max_sessions = max_sessions.clamp(1, 500) as i64;
        let system_allowed = Self::is_system_principal(principal);

        tokio::task::spawn_blocking(
            move || -> Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
                let stmt = format!(
                    "SELECT id, session_key, role, content, timestamp, message_id
                     FROM (
                         SELECT
                             ct.id, ct.session_key, ct.role, ct.content, ct.timestamp, ct.message_id,
                             ROW_NUMBER() OVER (PARTITION BY ct.session_key ORDER BY ct.id DESC) AS row_num
                         FROM {turns_table} ct
                         INNER JOIN (
                             SELECT session_key
                             FROM {sessions_table}
                             WHERE $3
                                OR owner_id = $4
                                OR owner_id IS NULL
                                OR owner_id = 'legacy:' || session_key
                             ORDER BY updated_at DESC
                             LIMIT $2
                         ) recent_sessions
                         ON recent_sessions.session_key = ct.session_key
                         WHERE $3
                            OR ct.owner_id = $4
                            OR ct.owner_id IS NULL
                            OR ct.owner_id = 'legacy:' || ct.session_key
                     ) sub
                     WHERE row_num <= $1
                     ORDER BY session_key ASC, id ASC"
                );
                let rows = client.with_client(|client| {
                    Ok(client.query(&stmt, &[&max_turns, &max_sessions, &system_allowed, &owner_id])?)
                })?;
                let mut histories: std::collections::HashMap<String, Vec<ConversationTurn>> =
                    std::collections::HashMap::new();
                for row in &rows {
                    let turn = ConversationTurn {
                        id: row.get(0),
                        session_key: row.get(1),
                        role: row.get(2),
                        content: row.get(3),
                        timestamp: row.get(4),
                        message_id: row.get(5),
                    };
                    histories.entry(turn.session_key.clone()).or_default().push(turn);
                }
                Ok(histories)
            },
        )
        .await?
    }

    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: Instant,
    ) -> Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        let entry = self.ingress_owners.cell(workspace_id)?;
        let _gate = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            entry.init_gate.clone().lock_owned(),
        )
        .await
        .context("PostgreSQL ingress initialization deadline elapsed")?;
        let lifetime = if let Some(lifetime) = entry.cell.get() {
            lifetime.clone()
        } else {
            let lifetime = self
                .new_ingress_lifetime(workspace_id, token, deadline, false)
                .await?
                .context("ingress lifetime lock unavailable")?;
            entry
                .cell
                .set(lifetime.clone())
                .map_err(|_| anyhow::anyhow!("ingress lifetime initialized concurrently"))?;
            lifetime
        };
        lifetime.validate(deadline).await?;
        Ok(lifetime)
    }
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: Instant,
    ) -> Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        use crate::memory::ingress_recovery::{AtomicIngressMutation, IngressAppendOutcome};
        let (event, inserted) = self
            .ingress_append_atomic(
                input,
                AtomicIngressMutation::Input {
                    capsule,
                    lifetime,
                    deadline,
                },
            )
            .await?;
        Ok(if inserted {
            IngressAppendOutcome::Inserted(event)
        } else {
            IngressAppendOutcome::AlreadyAccepted(event)
        })
    }
    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        self.ingress_candidates(scope, current, phase, after_id, limit, deadline)
            .await
    }
    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: Instant,
    ) -> Result<Option<MessageEvent>> {
        self.ingress_read(scope, lookup, deadline).await
    }
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: Instant,
    ) -> Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        Ok(self
            .new_ingress_lifetime(workspace_id, token, deadline, true)
            .await?
            .map(|lifetime| crate::memory::ingress_recovery::IngressRecoveryClaim { lifetime }))
    }
    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> Result<MessageEvent> {
        self.ingress_append_atomic(
            commit.evidence.clone(),
            crate::memory::ingress_recovery::AtomicIngressMutation::Recovery(commit),
        )
        .await
        .map(|(event, _)| event)
    }

    async fn append_message_event_until(&self, input: MessageEventInput, deadline: Instant) -> Result<MessageEvent> {
        self.ingress_repair_append(input, deadline).await
    }
    async fn project_console_message_until(&self, event: &MessageEvent, deadline: Instant) -> Result<()> {
        self.ingress_repair_console(event, deadline).await
    }
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.append_message_event_ordinary(input).await
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let workspace_id = workspace_id.to_string();
        let idempotency_key = idempotency_key.to_string();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let stmt = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {qualified_message_events_table}
                     WHERE workspace_id = $1 AND idempotency_key = $2
                     LIMIT 1"
                );
                client
                    .query_opt(&stmt, &[&workspace_id, &idempotency_key])?
                    .map(|row| Self::row_to_message_event(&row))
                    .transpose()
            })
        })
        .await?
    }

    async fn list_worker_launch_events(
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
        let client = self.client.clone();
        let table = self.qualified_message_events_table.clone();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let query = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                        parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                        sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                        goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                        content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {table}
                     WHERE workspace_id = $1 AND event_type IN ('worker.recovery.registered', 'worker.ordinary.recovery.registered', 'worker.operator.recovery.registered') AND id > $2
                     ORDER BY id ASC LIMIT $3"
                );
                client.query(&query, &[&workspace_id, &after_id, &limit])?
                    .iter().map(Self::row_to_message_event).collect()
            })
        })
        .await?
    }

    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !owner_id.is_empty() && !idempotency_key.is_empty(),
            "owned event lookup requires workspace, owner and key"
        );
        // The workspace/key index yields at most one row; filter owner before
        // returning any event content to the caller.
        Ok(self
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await?
            .filter(|event| event.owner_id.as_deref() == Some(owner_id)))
    }

    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> Result<Option<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let workspace_id = workspace_id.to_string();
        if workspace_id.is_empty() || owner_id.is_empty() || event_id.is_empty() {
            return Ok(None);
        }
        let owner_id = owner_id.to_string();
        let event_id = event_id.to_string();
        tokio::task::spawn_blocking(move || {
            client.with_client(|client| {
                let stmt = format!(
                    "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                            parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                            sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                            goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                            content, content_hash, raw_payload_json, visibility, created_at, updated_at
                     FROM {qualified_message_events_table}
                     WHERE workspace_id = $1 AND owner_id = $2 AND event_id = $3
                     LIMIT 1"
                );
                client
                    .query_opt(&stmt, &[&workspace_id, &owner_id, &event_id])?
                    .map(|row| Self::row_to_message_event(&row))
                    .transpose()
            })
        })
        .await?
    }

    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$5` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
        })
        .await?
    }

    async fn list_message_events_recent(&self, principal: &MemoryPrincipal, limit: usize) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$4` binding; legacy key(s) bind at trailing
        // placeholders starting at `$8` (after limit at `$7`).
        let session_indices = Self::session_indices(4, 8, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(8, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE (
                      visibility = 'global'
                      OR (
                          workspace_id = $1
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR ($5::TEXT IS NOT NULL AND sender = $5)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $7
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
        })
        .await?
    }

    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = principal.clone();
        let descendant_workspace_root = descendant_workspace_root.map(str::to_string);
        let start = chrono::DateTime::parse_from_rfc3339(start)?.with_timezone(&chrono::Utc);
        let end = chrono::DateTime::parse_from_rfc3339(end)?.with_timezone(&chrono::Utc);
        let limit_i64 = i64::try_from(limit.clamp(1, 100_000)).unwrap_or(100_000);
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
        let mut session_indices = Self::session_indices(4, 12, &principal, &legacy_session_keys);
        // This query has fixed placeholders after the session key. Keep $4 in
        // the predicate even when it is NULL so PostgreSQL can infer its type.
        if session_indices.is_empty() {
            session_indices.push(4);
        }
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(12, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE created_at >= $9 AND created_at < $10
                  AND (
                      visibility = 'global'
                      OR (
                          (
                              workspace_id = $1
                              OR ($6::BOOLEAN AND $7::TEXT IS NOT NULL AND (
                                  workspace_id = $7 OR workspace_id LIKE $8 ESCAPE '\\'
                              ))
                          )
                          AND (
                              ($6::BOOLEAN AND $7::TEXT IS NOT NULL)
                              OR visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR ($5::TEXT IS NOT NULL AND sender = $5)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                )
                ORDER BY created_at ASC, id ASC
                LIMIT $11
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
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
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter().map(Self::row_to_message_event).collect::<Result<Vec<_>>>()
        })
        .await?
    }

    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> Result<Vec<MessageEvent>> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = query.principal;
        let after_id = query.since_event_id.unwrap_or(0);
        let limit_i64 = i64::try_from(query.limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::HashSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$5` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`).
        let session_indices = Self::session_indices(5, 9, &principal, &legacy_session_keys);
        let session_fragment =
            crate::memory::session_predicate::session_visibility_or_fragment(PG_DIALECT, &session_indices);
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            Self::owner_index(9, legacy_session_keys.len()),
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_required_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND {session_fragment}{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
                ];
                for key in &legacy_session_keys {
                    bind.push(key);
                }
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            let mut events = rows
                .iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()?;
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

    async fn load_recent_session_context(&self, query: SessionContextQuery) -> Result<Vec<MessageEvent>> {
        let Some(session_key) = query.principal.session_key.clone() else {
            return Ok(Vec::new());
        };
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let principal = query.principal;
        let after_id = query.since_event_id.unwrap_or(0);
        let limit_i64 = i64::try_from(query.limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);
        let include_roles = query
            .include_roles
            .into_iter()
            .map(|role| role.trim().to_ascii_lowercase())
            .filter(|role| !role.is_empty())
            .collect::<std::collections::BTreeSet<_>>();

        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        // Canonical key keeps its `$3` binding; legacy key(s) bind at trailing
        // placeholders starting at `$9` (after limit at `$8`). The top-level
        // `session_key` hard filter becomes an `IN (...)` read-merge union.
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(PG_DIALECT, &session_indices);
        let owner_index = Self::owner_index(9, legacy_session_keys.len());
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            owner_index,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            owner_index,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        let first_role_index = owner_index + usize::from(owner_required_fragment.bound_owner);
        let role_fragment = if include_roles.is_empty() {
            "TRUE".to_string()
        } else {
            format!(
                "role IN ({})",
                (first_role_index..first_role_index + include_roles.len())
                    .map(|index| format!("${index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // OV-C: `workspace` is owner-qualified for an owner-isolated reader;
        // every other reader's statement is byte-identical.
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                       goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                       content, content_hash, raw_payload_json, visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND workspace_id = $2
                  AND {session_fragment}
                  AND {role_fragment}
                  AND (
                      {shared_tiers_arm}
                      OR (visibility = 'agent' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                      ))
                      OR (visibility = 'session'{owner_fragment})
                      OR (visibility = 'private' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                          OR ($6::TEXT IS NOT NULL AND sender = $6)
                      ){owner_required_fragment})
                      OR (visibility = 'system' AND $7::BOOLEAN)
                  )
                ORDER BY id DESC
                LIMIT $8
                ",
                session_fragment = session_fragment.sql,
                role_fragment = role_fragment,
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &session_key,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.sender,
                    &system_allowed,
                    &limit_i64,
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
                Ok(client.query(&stmt, &bind)?)
            })?;
            let mut events = rows
                .iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()?;
            events.reverse();
            Ok(events)
        })
        .await?
    }

    async fn append_memory_event(&self, input: MemoryEventInput) -> Result<MemoryEvent> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MemoryEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                ON CONFLICT (event_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE event_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &event_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.event_type,
                        &input.subject_table,
                        &input.subject_id,
                        &input.session_key,
                        &input.run_id,
                        &input.parent_run_id,
                        &input.agent_id,
                        &input.persona_id,
                        &input.visibility.as_str(),
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&event_id])?;
                Self::row_to_memory_event(&row)
            })
        })
        .await?
    }

    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MemoryEvent>> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        // C03: the owner parameter binds at the first free trailing slot (`$8`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            8,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            8,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $7
                ",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &after_id,
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &system_allowed,
                    &limit_i64,
                ];
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_memory_event)
                .collect::<Result<Vec<MemoryEvent>>>()
        })
        .await?
    }

    async fn list_memory_events_recent(&self, principal: &MemoryPrincipal, limit: usize) -> Result<Vec<MemoryEvent>> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let principal = principal.clone();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let system_allowed = Self::is_system_principal(&principal);

        // C03: the owner parameter binds at the first free trailing slot (`$7`).
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            7,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        // OV-E (OQ-1): the `private` arm requires the owner for every reader,
        // isolated or not, and admits nothing to a reader without one. Same
        // placeholder as `owner_fragment`, so one bound value serves both.
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            PG_DIALECT,
            7,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                       session_key, run_id, parent_run_id, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE (
                      visibility = 'global'
                      OR (
                          workspace_id = $1
                          AND (
                              visibility = 'workspace'{owner_fragment}
                              OR (visibility = 'agent' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                              ))
                              OR (visibility = 'session' AND $4::TEXT IS NOT NULL AND session_key = $4{owner_fragment})
                              OR (visibility = 'private' AND (
                                  ($2::TEXT IS NOT NULL AND agent_id = $2)
                                  OR ($3::TEXT IS NOT NULL AND persona_id = $3)
                                  OR (event_type = 'document.ingested' AND agent_id IS NULL AND persona_id IS NULL)
                              ){owner_required_fragment})
                              OR (visibility = 'system' AND $5::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $6
                ",
                owner_fragment = owner_fragment.sql,
                owner_required_fragment = owner_required_fragment.sql,
            );
            let rows = client.with_client(|client| {
                let mut bind: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
                    &principal.workspace_id,
                    &principal.agent_id,
                    &principal.persona_id,
                    &principal.session_key,
                    &system_allowed,
                    &limit_i64,
                ];
                if owner_required_fragment.bound_owner {
                    bind.push(&principal.owner_id);
                }
                Ok(client.query(&stmt, &bind)?)
            })?;
            rows.iter()
                .map(Self::row_to_memory_event)
                .collect::<Result<Vec<MemoryEvent>>>()
        })
        .await?
    }

    async fn create_memory_draft(&self, input: MemoryDraftInput) -> Result<MemoryDraft> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let draft_id = input.draft_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let category = Self::category_to_str(&input.category);
        let visibility = input.visibility.as_str().to_string();

        tokio::task::spawn_blocking(move || -> Result<MemoryDraft> {
            let now = Utc::now();
            let stmt = format!(
                "
                INSERT INTO {qualified_drafts_table}
                    (
                        draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'pending', $14, $15, $16)
                ON CONFLICT (draft_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    worker_run_id = EXCLUDED.worker_run_id,
                    parent_run_id = EXCLUDED.parent_run_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    key = EXCLUDED.key,
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    source_event_id = EXCLUDED.source_event_id,
                    visibility = EXCLUDED.visibility,
                    payload_json = EXCLUDED.payload_json,
                    updated_at = EXCLUDED.updated_at
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: the draft's own owner is recorded in the column, not only
                // inside `payload_json`, so a `session`/`private` row born here
                // is attributed and stays readable by its owner-isolated owner.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'memory.draft.created', 'memory_drafts', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                let row = tx.query_one(
                    &stmt,
                    &[
                        &draft_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.worker_run_id,
                        &input.parent_run_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.key,
                        &input.content,
                        &category,
                        &input.source_event_id,
                        &visibility,
                        &input.payload_json,
                        &now,
                        &now,
                    ],
                )?;
                let draft = Self::row_to_draft(&row)?;
                let payload_json = serde_json::json!({
                    "worker_run_id": draft.worker_run_id,
                    "owner_id": draft.owner_id,
                    "parent_run_id": draft.parent_run_id,
                    "key": draft.key,
                })
                .to_string();
                tx.execute(
                    &outbox_stmt,
                    &[
                        &Uuid::new_v4().to_string(),
                        &draft.workspace_id,
                        &draft.owner_id,
                        &draft.draft_id,
                        &draft.session_key,
                        &draft.agent_id,
                        &draft.persona_id,
                        &draft.visibility.as_str(),
                        &payload_json,
                        &Utc::now(),
                    ],
                )?;
                tx.commit()?;
                Ok(row)
            })?;
            Self::row_to_draft(&row)
        })
        .await?
    }

    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> Result<Vec<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let worker_run_id = worker_run_id.to_string();
        // Owner ACL: only return drafts owned by the principal (or ownerless /
        // system-created drafts). System principals bypass the filter.
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryDraft>> {
            let owner_clause = if owner.is_some() {
                "AND (owner_id = $2 OR owner_id IS NULL)"
            } else {
                ""
            };
            let stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                FROM {qualified_drafts_table}
                WHERE worker_run_id = $1 {owner_clause}
                ORDER BY id
                "
            );
            let rows = client.with_client(|client| {
                if let Some(owner) = owner.as_deref() {
                    Ok(client.query(&stmt, &[&worker_run_id, &owner])?)
                } else {
                    Ok(client.query(&stmt, &[&worker_run_id])?)
                }
            })?;
            rows.iter()
                .map(Self::row_to_draft)
                .collect::<Result<Vec<MemoryDraft>>>()
        })
        .await?
    }

    async fn merge_memory_draft(&self, principal: &MemoryPrincipal, draft_id: &str) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let draft_id = draft_id.to_string();
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let select_stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                FROM {qualified_drafts_table}
                WHERE draft_id = $1
                LIMIT 1
                "
            );
            let Some(row) = client.with_client(|client| Ok(client.query_opt(&select_stmt, &[&draft_id])?))? else {
                return Ok(None);
            };
            let draft = Self::row_to_draft(&row)?;
            // Owner ACL: reject merge attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                if let Some(draft_owner) = draft.owner_id.as_deref() {
                    if draft_owner != owner {
                        anyhow::bail!("memory draft {} is owned by a different principal", draft.draft_id);
                    }
                }
            }
            if draft.status != "pending" && draft.status != "merge_requested" {
                return Ok(Some(draft));
            }

            let category = Self::category_to_str(&draft.category);
            let now = Utc::now();
            let memory_id = Uuid::new_v4().to_string();
            let upsert_stmt = format!(
                "
                INSERT INTO {qualified_table}
                    (
                        id, key, content, category, created_at, updated_at, session_id,
                        workspace_id, owner_id, agent_id, persona_id, source_event_id, source
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'memory_draft')
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source
                "
            );
            let update_stmt = format!(
                "
                UPDATE {qualified_drafts_table}
                SET status = 'merged', updated_at = $2
                WHERE draft_id = $1
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                tx.execute(
                    &upsert_stmt,
                    &[
                        &memory_id,
                        &draft.key,
                        &draft.content,
                        &category,
                        &now,
                        &now,
                        &draft.session_key,
                        &draft.workspace_id,
                        &draft.owner_id,
                        &draft.agent_id,
                        &draft.persona_id,
                        &draft.source_event_id,
                    ],
                )?;
                let row = tx.query_one(&update_stmt, &[&draft_id, &now])?;
                let merged = Self::row_to_draft(&row)?;
                let payload_json = serde_json::json!({
                    "draft_id": merged.draft_id,
                    "owner_id": merged.owner_id,
                    "worker_run_id": merged.worker_run_id,
                    "parent_run_id": merged.parent_run_id,
                    "key": merged.key,
                })
                .to_string();
                for (event_type, subject_table, subject_id) in [
                    ("memory.draft.merged", "memory_drafts", merged.draft_id.as_str()),
                    ("memory.stored", "memories", merged.key.as_str()),
                ] {
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &merged.workspace_id,
                            &merged.owner_id,
                            &event_type,
                            &subject_table,
                            &subject_id,
                            &merged.session_key,
                            &merged.agent_id,
                            &merged.persona_id,
                            &merged.visibility.as_str(),
                            &payload_json,
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(row)
            })?;
            Ok(Some(Self::row_to_draft(&row)?))
        })
        .await?
    }

    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let draft_id = draft_id.to_string();
        let reason = reason.map(str::to_string);
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let now = Utc::now();
            // Owner ACL: reject attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                let select_owner = format!("SELECT owner_id FROM {qualified_drafts_table} WHERE draft_id = $1 LIMIT 1");
                let existing = client.with_client(|client| Ok(client.query_opt(&select_owner, &[&draft_id])?))?;
                if let Some(row) = existing {
                    let draft_owner: Option<String> = row.get(0);
                    if let Some(draft_owner) = draft_owner {
                        if draft_owner != owner {
                            anyhow::bail!("memory draft {draft_id} is owned by a different principal");
                        }
                    }
                }
            }
            let payload = reason
                .as_ref()
                .map(|reason| serde_json::json!({ "reason": reason }).to_string());
            let stmt = format!(
                "
                UPDATE {qualified_drafts_table}
                SET
                    status = 'rejected',
                    payload_json = COALESCE($3, payload_json),
                    updated_at = $2
                WHERE draft_id = $1
                RETURNING
                    id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let outbox_stmt = format!(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'memory.draft.rejected', 'memory_drafts', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let row = client.with_client(|client| {
                let mut tx = client.transaction()?;
                let row = tx.query_opt(&stmt, &[&draft_id, &now, &payload])?;
                if let Some(row) = &row {
                    let draft = Self::row_to_draft(row)?;
                    let payload_json = serde_json::json!({
                        "draft_id": draft.draft_id,
                        "owner_id": draft.owner_id,
                        "worker_run_id": draft.worker_run_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key,
                        "reason": reason.as_deref(),
                    })
                    .to_string();
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &draft.workspace_id,
                            &draft.owner_id,
                            &draft.draft_id,
                            &draft.session_key,
                            &draft.agent_id,
                            &draft.persona_id,
                            &draft.visibility.as_str(),
                            &payload_json,
                            &Utc::now(),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(row)
            })?;
            row.as_ref().map(Self::row_to_draft).transpose()
        })
        .await?
    }

    /// Adds one to `useful_count` for the row with this id.
    ///
    /// Takes no caller context, like `forget` and the SQLite twin, so it runs
    /// in the pool's system session without an owner scope. The only caller
    /// passes ids it read from owner-scoped recall results. The id is a bound
    /// parameter; only the validated table name is formatted into the SQL.
    async fn increment_useful_count(&self, id: &str) -> Result<()> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let id = id.to_string();

        // Same scope as `forget` and `count`, which also take no caller
        // context: the pooled connection is the trusted system session. As in
        // SQLite, an unknown id updates nothing and is not an error.
        tokio::task::spawn_blocking(move || -> Result<()> {
            let stmt = format!(
                "UPDATE {qualified_table}
                 SET useful_count = COALESCE(useful_count, 0) + 1,
                     updated_at = $1
                 WHERE id = $2"
            );
            let now = Utc::now();
            client.with_client(|client| Ok(client.execute(&stmt, &[&now, &id])?))?;
            Ok(())
        })
        .await?
    }

    async fn ingest_document(&self, input: DocumentIngestInput) -> Result<DocumentRecord> {
        let workspace_context = input.workspace_id.trim();
        anyhow::ensure!(
            !workspace_context.is_empty(),
            "PostgreSQL vector isolation requires document workspace_id"
        );
        let owner_context = input
            .owner_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("PostgreSQL vector isolation requires document owner_id")?;
        let workspace_context = workspace_context.to_string();
        let owner_context = owner_context.to_string();
        let client = self.client.clone();
        let qualified_documents_table = self.qualified_documents_table.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedding_dimensions_i64();
        let raw_chunks: Vec<(usize, Option<String>, String)> =
            super::super::chunker::chunk_markdown(&input.content, 1_000)
                .into_iter()
                .map(|chunk| {
                    (
                        chunk.index,
                        chunk.heading.as_ref().map(|heading| heading.to_string()),
                        chunk.content,
                    )
                })
                .collect();
        let mut prepared_chunks = Vec::with_capacity(raw_chunks.len());
        for (chunk_index, heading, content) in raw_chunks {
            let embedding = self
                .get_or_compute_embedding(&content, Some(&workspace_context), Some(&owner_context), false)
                .await?;
            let embedding_vector = embedding.as_ref().and_then(|embedding| {
                self.pgvector_available
                    .then(|| Self::pgvector_literal(embedding))
                    .flatten()
            });
            let embedding_bytes = embedding.map(|embedding| vector::vec_to_bytes(&embedding));
            prepared_chunks.push((chunk_index, heading, content, embedding_bytes, embedding_vector));
        }
        let pgvector_available = self.pgvector_available;

        tokio::task::spawn_blocking(move || -> Result<DocumentRecord> {
            let now = Utc::now();
            let document_id = input.document_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_sha256 = Self::content_sha256_hex(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let chunk_count = i64::try_from(prepared_chunks.len()).unwrap_or(i64::MAX);
            let insert_document_stmt = format!(
                "
                INSERT INTO {qualified_documents_table} (
                    document_id, workspace_id, owner_id, topic_id, task_id, source_message_event_id,
                    source_kind, source_uri, title, content_sha256, mime_type, visibility,
                    metadata_json, chunk_count, created_at, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                ON CONFLICT (document_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    topic_id = EXCLUDED.topic_id,
                    task_id = EXCLUDED.task_id,
                    source_message_event_id = EXCLUDED.source_message_event_id,
                    source_kind = EXCLUDED.source_kind,
                    source_uri = EXCLUDED.source_uri,
                    title = EXCLUDED.title,
                    content_sha256 = EXCLUDED.content_sha256,
                    mime_type = EXCLUDED.mime_type,
                    visibility = EXCLUDED.visibility,
                    metadata_json = EXCLUDED.metadata_json,
                    chunk_count = EXCLUDED.chunk_count,
                    updated_at = EXCLUDED.updated_at
                "
            );
            let delete_chunks_stmt = format!(
                "DELETE FROM {qualified_document_chunks_table} \
                 WHERE document_id = $1 AND workspace_id = $2 AND owner_id = $3"
            );
            let insert_chunk_stmt = format!(
                "
                INSERT INTO {qualified_document_chunks_table} (
                    chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                    chunk_index, heading, content, content_sha256, embedding,
                    embedding_provider, embedding_model, embedding_dimensions,
                    visibility, source_anchor, token_estimate, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                "
            );
            let insert_event_stmt = format!(
                // C03: the ingested document's own owner is recorded in the
                // column, so a `session`/`private` ingest event is attributed.
                // V02-R6: the turn's scoping keys are bound too, so the event is
                // reachable through the `session`/`agent`/`private` arms of the
                // read predicate instead of carrying unmatchable NULLs.
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'document.ingested', 'documents', $4, $5, $6, $7, $8, $9, $10)
                "
            );
            let select_document_stmt = format!(
                "
                SELECT id, document_id, workspace_id, owner_id, topic_id, task_id,
                       source_message_event_id, source_kind, source_uri, title,
                       content_sha256, mime_type, visibility, metadata_json,
                       chunk_count, created_at, updated_at
                FROM {qualified_documents_table}
                WHERE document_id = $1 AND workspace_id = $2 AND owner_id = $3
                LIMIT 1
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // Every ingest statement is exact on the document's own owner, so the
                // narrowest read scope costs nothing and admits no other owner.
                Self::apply_rls_context_raw(&mut tx, false, Some(&workspace_context), Some(&owner_context), true)?;
                tx.execute(
                    &insert_document_stmt,
                    &[
                        &document_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.topic_id,
                        &input.task_id,
                        &input.source_message_event_id,
                        &input.source_kind,
                        &input.source_uri,
                        &input.title,
                        &content_sha256,
                        &input.mime_type,
                        &visibility,
                        &input.metadata_json,
                        &chunk_count,
                        &now,
                        &now,
                    ],
                )?;
                tx.execute(&delete_chunks_stmt, &[&document_id, &workspace_context, &owner_context])?;
                let update_chunk_vector_stmt = if pgvector_available {
                    Some(format!(
                        "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::text::vector \
                         WHERE chunk_id = $2 AND workspace_id = $3 AND owner_id = $4"
                    ))
                } else {
                    None
                };
                for (chunk_index, heading, content, embedding_bytes, embedding_vector) in prepared_chunks {
                    let chunk_id = format!("{document_id}:chunk:{chunk_index}");
                    let source_anchor = format!("{document_id}#chunk-{chunk_index}");
                    let token_estimate = i64::try_from(content.chars().count().div_ceil(4)).unwrap_or(i64::MAX);
                    let chunk_index = i64::try_from(chunk_index).unwrap_or(i64::MAX);
                    let chunk_hash = Self::content_sha256_hex(&content);
                    let chunk_embedding_provider = embedding_bytes.as_ref().map(|_| embedding_provider.clone());
                    let chunk_embedding_model = embedding_bytes.as_ref().map(|_| embedding_model.clone());
                    let chunk_embedding_dimensions = embedding_bytes.as_ref().map(|_| embedding_dimensions);
                    tx.execute(
                        &insert_chunk_stmt,
                        &[
                            &chunk_id,
                            &document_id,
                            &input.workspace_id,
                            &input.owner_id,
                            &input.topic_id,
                            &input.task_id,
                            &chunk_index,
                            &heading,
                            &content,
                            &chunk_hash,
                            &embedding_bytes,
                            &chunk_embedding_provider,
                            &chunk_embedding_model,
                            &chunk_embedding_dimensions,
                            &visibility,
                            &source_anchor,
                            &token_estimate,
                            &now,
                        ],
                    )?;
                    if let (Some(stmt), Some(embedding_vector)) =
                        (update_chunk_vector_stmt.as_ref(), embedding_vector.as_ref())
                    {
                        tx.execute(stmt, &[embedding_vector, &chunk_id, &workspace_context, &owner_context])?;
                    }
                }
                let payload_json = serde_json::json!({
                    "owner_id": input.owner_id,
                    "topic_id": input.topic_id,
                    "task_id": input.task_id,
                    "source_message_event_id": input.source_message_event_id,
                    "chunk_count": chunk_count,
                    "content_sha256": content_sha256
                })
                .to_string();
                tx.execute(
                    &insert_event_stmt,
                    &[
                        &Uuid::new_v4().to_string(),
                        &input.workspace_id,
                        &input.owner_id,
                        &document_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &visibility,
                        &payload_json,
                        &Utc::now(),
                    ],
                )?;
                let row = tx.query_one(
                    &select_document_stmt,
                    &[&document_id, &workspace_context, &owner_context],
                )?;
                let document = Self::row_to_document(&row)?;
                tx.commit()?;
                Ok(document)
            })
        })
        .await?
    }

    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DocumentSearchResult>> {
        let client = self.client.clone();
        let qualified_documents_table = self.qualified_documents_table.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let principal = principal.clone();
        let query = query.trim().to_string();
        let owner_id = if principal.require_owner_match {
            // V02-R8: a blank `owner_id` is not an owner. Taking it verbatim
            // would bind whitespace as the caller's identity, here and in the
            // RLS context below; `document_owner_for_principal` applies the
            // shared blank rule and yields `None`, which the statements read as
            // "admit nothing".
            Self::document_owner_for_principal(&principal)
        } else {
            principal
                .owner_id
                .clone()
                .or_else(|| Self::document_owner_for_principal(&principal))
        };
        // V02-R8: empty for every non-isolated principal, so all three
        // statements below stay byte-identical for them.
        let visibility_guard = Self::document_visibility_guard(&principal);
        // The decision `isolated_read_without_owner` takes for the keyed reads,
        // applied to the same blank rule here: an owner-isolated caller with no
        // usable owner id answers empty rather than erroring.
        // `apply_rls_context_raw` below rejects a non-system read that carries
        // no owner, so without this the chunk search would return an error
        // where the SQLite adapter returns no rows. A non-isolated principal is
        // never short-circuited.
        if principal.require_owner_match && owner_id.is_none() {
            tracing::warn!(
                operation = "search_document_chunks",
                "owner-isolated document search carries no owner id; returning no rows"
            );
            return Ok(Vec::new());
        }
        let rls_system = Self::is_system_principal(&principal);
        let rls_owner_isolated = principal.require_owner_match;
        let rls_workspace = principal.workspace_id.trim().to_string();
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let query_embedding = self
            .get_or_compute_embedding(&query, Some(&rls_workspace), owner_id.as_deref(), rls_system)
            .await?;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();
        let vector_weight = self.vector_weight;
        let keyword_weight = self.keyword_weight;
        let pgvector_available = self.pgvector_available;
        let query_vector = query_embedding
            .as_ref()
            .and_then(|embedding| Self::pgvector_literal(embedding));

        tokio::task::spawn_blocking(move || -> Result<Vec<DocumentSearchResult>> {
            let stmt = format!(
                "
                SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                       c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                       c.token_estimate, c.created_at,
                       ts_rank_cd(to_tsvector('simple', c.content), plainto_tsquery('simple', $1)) AS score,
                       d.source_kind AS source_kind
                FROM {qualified_document_chunks_table} c
                JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                WHERE c.workspace_id = $2
                  AND (
                      d.visibility IN ('global', 'workspace'){visibility_guard}
                      OR ($3::TEXT IS NOT NULL AND c.owner_id = $3)
                  )
                  AND (
                      to_tsvector('simple', c.content) @@ plainto_tsquery('simple', $1)
                      OR c.content ILIKE '%' || $1 || '%'
                      OR c.heading ILIKE '%' || $1 || '%'
                  )
                ORDER BY score DESC, c.id ASC
                LIMIT $4
                "
            );
            let rows = client.with_client(|client| {
                let mut tx = client.transaction()?;
                Self::apply_rls_context_raw(
                    &mut tx,
                    rls_system,
                    Some(&rls_workspace),
                    owner_id.as_deref(),
                    rls_owner_isolated,
                )?;
                let rows = tx.query(&stmt, &[&query, &principal.workspace_id, &owner_id, &limit_i64])?;
                tx.commit()?;
                Ok(rows)
            })?;
            let mut by_chunk = std::collections::HashMap::<String, DocumentSearchResult>::new();
            for row in &rows {
                let mut result = {
                    let chunk = Self::row_to_document_chunk(row)?;
                    let score: f32 = row.try_get::<_, f32>(14).unwrap_or(0.0);
                    let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                    DocumentSearchResult {
                        chunk,
                        score,
                        source_kind,
                    }
                };
                result.score *= keyword_weight;
                by_chunk.insert(result.chunk.chunk_id.clone(), result);
            }

            if let Some(query_embedding) = query_embedding {
                if pgvector_available {
                    if let Some(query_vector) = query_vector.as_ref() {
                        let vector_stmt = format!(
                            "
                            SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                                   c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                                   c.token_estimate, c.created_at,
                                   (1.0 - (c.embedding_vector <=> $1::text::vector))::REAL AS score,
                                   d.source_kind AS source_kind
                            FROM {qualified_document_chunks_table} c
                            JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                            WHERE c.embedding_vector IS NOT NULL
                              AND c.embedding_provider = $2
                              AND c.embedding_model = $3
                              AND c.embedding_dimensions = $4
                              AND c.workspace_id = $5
                              AND (
                                  d.visibility IN ('global', 'workspace'){visibility_guard}
                                  OR ($6::TEXT IS NOT NULL AND c.owner_id = $6)
                              )
                            ORDER BY c.embedding_vector <=> $1::text::vector
                            LIMIT $7
                            "
                        );
                        let candidate_limit = limit_i64.saturating_mul(4).max(limit_i64);
                        let rows = client.with_client(|client| {
                            let mut tx = client.transaction()?;
                            Self::apply_rls_context_raw(
                                &mut tx,
                                rls_system,
                                Some(&rls_workspace),
                                owner_id.as_deref(),
                                rls_owner_isolated,
                            )?;
                            let rows = tx.query(
                                &vector_stmt,
                                &[
                                    query_vector,
                                    &embedding_provider,
                                    &embedding_model,
                                    &embedding_dimensions_i64,
                                    &principal.workspace_id,
                                    &owner_id,
                                    &candidate_limit,
                                ],
                            )?;
                            tx.commit()?;
                            Ok(rows)
                        })?;
                        for row in &rows {
                            let score = row.try_get::<_, f32>(14).unwrap_or(0.0) * vector_weight;
                            if score <= 0.0 {
                                continue;
                            }
                            let chunk = Self::row_to_document_chunk(row)?;
                            let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                            by_chunk
                                .entry(chunk.chunk_id.clone())
                                .and_modify(|existing| {
                                    existing.score += score;
                                })
                                .or_insert(DocumentSearchResult {
                                    chunk,
                                    score,
                                    source_kind,
                                });
                        }
                    }
                } else {
                    // FIX-P2-03: BYTEA document-chunk fallback (no pgvector). As with
                    // the memory-entry fallback above, SQL cannot order by vector
                    // distance, so candidates are bounded by a pure anti-DoS safety
                    // cap (after the workspace/visibility/owner WHERE filter) and
                    // re-ranked in-process by cosine similarity. The cap is not a
                    // relevance filter; `ORDER BY c.id` keeps the (cap-only)
                    // truncation deterministic without faking similarity. Known
                    // degradation tradeoff of running without pgvector.
                    let candidate_limit = POSTGRES_BYTEA_FALLBACK_CANDIDATE_CAP;
                    let vector_stmt = format!(
                        "
                        SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                               c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                               c.token_estimate, c.created_at, c.embedding, d.source_kind AS source_kind
                        FROM {qualified_document_chunks_table} c
                        JOIN {qualified_documents_table} d ON d.document_id = c.document_id
                        WHERE c.embedding IS NOT NULL
                          AND c.embedding_provider = $1
                          AND c.embedding_model = $2
                          AND c.embedding_dimensions = $3
                          AND c.workspace_id = $4
                          AND (
                              d.visibility IN ('global', 'workspace'){visibility_guard}
                              OR ($5::TEXT IS NOT NULL AND c.owner_id = $5)
                          )
                        ORDER BY c.id
                        LIMIT $6
                        "
                    );
                    let rows = client.with_client(|client| {
                        let mut tx = client.transaction()?;
                        Self::apply_rls_context_raw(
                            &mut tx,
                            rls_system,
                            Some(&rls_workspace),
                            owner_id.as_deref(),
                            rls_owner_isolated,
                        )?;
                        let rows = tx.query(
                            &vector_stmt,
                            &[
                                &embedding_provider,
                                &embedding_model,
                                &embedding_dimensions_i64,
                                &principal.workspace_id,
                                &owner_id,
                                &candidate_limit,
                            ],
                        )?;
                        tx.commit()?;
                        Ok(rows)
                    })?;
                    for row in &rows {
                        let embedding_blob: Vec<u8> = row.get(14);
                        let embedding = vector::bytes_to_vec(&embedding_blob);
                        if embedding.len() != embedding_dimensions {
                            tracing::debug!(
                                chunk_id = %row.get::<_, String>(1),
                                expected_dimensions = embedding_dimensions,
                                actual_dimensions = embedding.len(),
                                "Skipping stale Postgres document chunk embedding with mismatched dimensions"
                            );
                            continue;
                        }
                        let score = vector::cosine_similarity(&query_embedding, &embedding) * vector_weight;
                        if score <= 0.0 {
                            continue;
                        }
                        let chunk = Self::row_to_document_chunk(row)?;
                        let source_kind: Option<String> = row.try_get::<_, Option<String>>(15).unwrap_or(None);
                        by_chunk
                            .entry(chunk.chunk_id.clone())
                            .and_modify(|existing| {
                                existing.score += score;
                            })
                            .or_insert(DocumentSearchResult {
                                chunk,
                                score,
                                source_kind,
                            });
                    }
                }
            }

            let mut results: Vec<DocumentSearchResult> = by_chunk.into_values().collect();
            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(limit.max(1));
            Ok(results)
        })
        .await?
    }

    async fn get_document_chunk(&self, chunk_id: &str) -> Result<Option<DocumentChunkRecord>> {
        let client = self.client.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let chunk_id = chunk_id.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<DocumentChunkRecord>> {
            let stmt = format!(
                "
                SELECT id, chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                       chunk_index, heading, content, content_sha256, source_anchor,
                       token_estimate, created_at
                FROM {qualified_document_chunks_table}
                WHERE chunk_id = $1
                LIMIT 1
                "
            );
            let row = client.with_client(|client| Ok(client.query_opt(&stmt, &[&chunk_id])?))?;
            row.as_ref().map(Self::row_to_document_chunk).transpose()
        })
        .await?
    }

    async fn link_memory_source(&self, input: MemoryLinkInput) -> Result<MemoryLink> {
        let client = self.client.clone();
        let qualified_memory_links_table = self.qualified_memory_links_table.clone();
        let link_id = input.link_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<MemoryLink> {
            let now = Utc::now();
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_memory_links_table} (
                    link_id, workspace_id, owner_id, memory_key, memory_event_id,
                    message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (link_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, link_id, workspace_id, owner_id, memory_key, memory_event_id,
                       message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                FROM {qualified_memory_links_table}
                WHERE link_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &link_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.memory_key,
                        &input.memory_event_id,
                        &input.message_event_id,
                        &input.document_id,
                        &input.chunk_id,
                        &input.link_type,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&link_id])?;
                Self::row_to_memory_link(&row)
            })
        })
        .await?
    }

    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> Result<RetrievalTrace> {
        let client = self.client.clone();
        let qualified_retrieval_traces_table = self.qualified_retrieval_traces_table.clone();
        let trace_id = input.trace_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<RetrievalTrace> {
            let now = Utc::now();
            let candidate_count = i64::try_from(input.candidate_count).unwrap_or(i64::MAX);
            let selected_count = i64::try_from(input.selected_count).unwrap_or(i64::MAX);
            let dropped_count = i64::try_from(input.dropped_count).unwrap_or(i64::MAX);
            let budget_tokens = input
                .budget_tokens
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_retrieval_traces_table} (
                    trace_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    source, query, candidate_count, selected_count, dropped_count,
                    budget_tokens, selected_json, dropped_json, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                ON CONFLICT (trace_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source = EXCLUDED.source,
                    query = EXCLUDED.query,
                    candidate_count = EXCLUDED.candidate_count,
                    selected_count = EXCLUDED.selected_count,
                    dropped_count = EXCLUDED.dropped_count,
                    budget_tokens = EXCLUDED.budget_tokens,
                    selected_json = EXCLUDED.selected_json,
                    dropped_json = EXCLUDED.dropped_json,
                    payload_json = EXCLUDED.payload_json
                "
            );
            let select_stmt = format!(
                "
                SELECT id, trace_id, workspace_id, owner_id, session_key, agent_id,
                       persona_id, source, query, candidate_count, selected_count,
                       dropped_count, budget_tokens, selected_json, dropped_json,
                       payload_json, created_at
                FROM {qualified_retrieval_traces_table}
                WHERE trace_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &trace_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.source,
                        &input.query,
                        &candidate_count,
                        &selected_count,
                        &dropped_count,
                        &budget_tokens,
                        &input.selected_json,
                        &input.dropped_json,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&trace_id])?;
                Self::row_to_retrieval_trace(&row)
            })
        })
        .await?
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> Result<CompactionRun> {
        input.validate_source_event_provenance()?;
        let client = self.client.clone();
        let qualified_compaction_runs_table = self.qualified_compaction_runs_table.clone();
        let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        tokio::task::spawn_blocking(move || -> Result<CompactionRun> {
            let now = Utc::now();
            let source_message_count = i64::try_from(input.source_message_count).unwrap_or(i64::MAX);
            let source_token_estimate = i64::try_from(input.source_token_estimate).unwrap_or(i64::MAX);
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_compaction_runs_table} (
                    run_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    trigger, mode, source_message_count, source_token_estimate,
                    summary, summary_memory_key, source_event_ids_json,
                    source_event_range_json, source_document_refs_json,
                    fidelity_status, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                ON CONFLICT (run_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
                    owner_id = EXCLUDED.owner_id,
                    session_key = EXCLUDED.session_key,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    trigger = EXCLUDED.trigger,
                    mode = EXCLUDED.mode,
                    source_message_count = EXCLUDED.source_message_count,
                    source_token_estimate = EXCLUDED.source_token_estimate,
                    summary = EXCLUDED.summary,
                    summary_memory_key = EXCLUDED.summary_memory_key,
                    source_event_ids_json = EXCLUDED.source_event_ids_json,
                    source_event_range_json = EXCLUDED.source_event_range_json,
                    source_document_refs_json = EXCLUDED.source_document_refs_json,
                    fidelity_status = EXCLUDED.fidelity_status,
                    payload_json = EXCLUDED.payload_json
                "
            );
            let select_stmt = format!(
                "
                SELECT id, run_id, workspace_id, owner_id, session_key, agent_id,
                       persona_id, trigger, mode, source_message_count,
                       source_token_estimate, summary, summary_memory_key,
                       source_event_ids_json, source_event_range_json,
                       source_document_refs_json, fidelity_status, payload_json,
                       created_at
                FROM {qualified_compaction_runs_table}
                WHERE run_id = $1
                LIMIT 1
                "
            );
            client.with_client(|client| {
                client.execute(
                    &insert_stmt,
                    &[
                        &run_id,
                        &input.workspace_id,
                        &input.owner_id,
                        &input.session_key,
                        &input.agent_id,
                        &input.persona_id,
                        &input.trigger,
                        &input.mode,
                        &source_message_count,
                        &source_token_estimate,
                        &input.summary,
                        &input.summary_memory_key,
                        &input.source_event_ids_json,
                        &input.source_event_range_json,
                        &input.source_document_refs_json,
                        &input.fidelity_status,
                        &input.payload_json,
                        &now,
                    ],
                )?;
                let row = client.query_one(&select_stmt, &[&run_id])?;
                Self::row_to_compaction_run(&row)
            })
        })
        .await?
    }

    async fn reindex(&self) -> Result<usize> {
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let entries: Vec<(String, String)> = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let stmt = format!(
                "
                SELECT id, content
                FROM {qualified_table}
                WHERE category NOT IN ('daily', 'conversation')
                  AND (
                      embedding IS NULL
                      OR embedding_provider IS NULL
                      OR embedding_model IS NULL
                      OR embedding_dimensions IS NULL
                      OR embedding_provider != $1
                      OR embedding_model != $2
                      OR embedding_dimensions != $3
                  )
                "
            );
            client.with_client(|client| {
                let rows = client.query(&stmt, &[&provider_name, &model_name, &dimensions])?;
                Ok(rows
                    .iter()
                    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                    .collect())
            })
        })
        .await??;

        let mut count = 0usize;
        for (id, content) in entries {
            if let Some(embedding) = self.get_or_compute_embedding(&content, None, None, true).await? {
                let embedding_bytes = vector::vec_to_bytes(&embedding);
                let embedding_vector = self
                    .pgvector_available
                    .then(|| Self::pgvector_literal(&embedding))
                    .flatten();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                let client = self.client.clone();
                let qualified_table = self.qualified_table.clone();
                let pgvector_available = self.pgvector_available;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let stmt = format!(
                        "
                        UPDATE {qualified_table}
                        SET embedding = $1,
                            embedding_provider = $2,
                            embedding_model = $3,
                            embedding_dimensions = $4
                        WHERE id = $5
                        "
                    );
                    client.with_client(|client| {
                        client.execute(
                            &stmt,
                            &[&embedding_bytes, &provider_name, &model_name, &dimensions, &id],
                        )?;
                        if pgvector_available {
                            let vector_stmt = format!(
                                "UPDATE {qualified_table} SET embedding_vector = $1::text::vector WHERE id = $2"
                            );
                            if let Some(embedding_vector) = embedding_vector.as_ref() {
                                client.execute(&vector_stmt, &[embedding_vector, &id])?;
                            }
                        }
                        Ok(())
                    })
                })
                .await??;
                count += 1;
            }
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let client = self.client.clone();
        let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
        let chunks: Vec<(String, String)> = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
            let stmt = format!(
                "
                SELECT chunk_id, content
                FROM {qualified_document_chunks_table}
                WHERE embedding IS NULL
                   OR embedding_provider IS NULL
                   OR embedding_model IS NULL
                   OR embedding_dimensions IS NULL
                   OR embedding_provider != $1
                   OR embedding_model != $2
                   OR embedding_dimensions != $3
                "
            );
            client.with_client(|client| {
                let rows = client.query(&stmt, &[&provider_name, &model_name, &dimensions])?;
                Ok(rows
                    .iter()
                    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                    .collect())
            })
        })
        .await??;

        for (chunk_id, content) in chunks {
            if let Some(embedding) = self.get_or_compute_embedding(&content, None, None, true).await? {
                let embedding_bytes = vector::vec_to_bytes(&embedding);
                let embedding_vector = self
                    .pgvector_available
                    .then(|| Self::pgvector_literal(&embedding))
                    .flatten();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                let client = self.client.clone();
                let qualified_document_chunks_table = self.qualified_document_chunks_table.clone();
                let pgvector_available = self.pgvector_available;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let stmt = format!(
                        "
                        UPDATE {qualified_document_chunks_table}
                        SET embedding = $1,
                            embedding_provider = $2,
                            embedding_model = $3,
                            embedding_dimensions = $4
                        WHERE chunk_id = $5
                        "
                    );
                    client.with_client(|client| {
                        client.execute(
                            &stmt,
                            &[&embedding_bytes, &provider_name, &model_name, &dimensions, &chunk_id],
                        )?;
                        if pgvector_available {
                            let vector_stmt = format!(
                                "UPDATE {qualified_document_chunks_table} SET embedding_vector = $1::text::vector WHERE chunk_id = $2"
                            );
                            if let Some(embedding_vector) = embedding_vector.as_ref() {
                                client.execute(&vector_stmt, &[embedding_vector, &chunk_id])?;
                            }
                        }
                        Ok(())
                    })
                })
                .await??;
                count += 1;
            }
        }

        Ok(count)
    }

    async fn health_check(&self) -> bool {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || client.with_client(|client| Ok(client.simple_query("SELECT 1").is_ok())))
            .await
            .unwrap_or(Ok(false))
            .unwrap_or(false)
    }
}

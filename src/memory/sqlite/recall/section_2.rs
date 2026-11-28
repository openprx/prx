impl SqliteMemory {
    /// Safe reindex: rebuild FTS5 + embeddings with rollback on failure
    pub async fn reindex(&self) -> anyhow::Result<usize> {
        // Step 1: Rebuild FTS5
        {
            let pool = self.pool.clone();
            crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = pool.write();
                conn.execute_batch(
                    "INSERT INTO memories_fts(memories_fts) VALUES('rebuild');
                     INSERT INTO document_chunks_fts(document_chunks_fts) VALUES('rebuild');",
                )?;
                Ok(())
            })
            .await??;
        }

        // Step 2: Re-embed eligible memories that lack embeddings
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let pool = self.pool.clone();
        let entries: Vec<(String, String, Option<String>)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, content, ma_scope_id
                 FROM memories
                 WHERE category NOT IN ('daily', 'conversation')
                   AND (
                       embedding IS NULL
                       OR embedding_provider IS NULL
                       OR embedding_model IS NULL
                       OR embedding_dimensions IS NULL
                       OR embedding_provider != ?1
                       OR embedding_model != ?2
                       OR embedding_dimensions != ?3
                   )",
            )?;
            let rows = stmt.query_map(params![provider_name, model_name, dimensions], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        let mut count = 0;
        for (id, content, scope) in &entries {
            if let Ok(Some(emb)) = self.embed_and_cache(content, scope.as_deref()).await {
                let bytes = vector::vec_to_bytes(&emb);
                let pool = self.pool.clone();
                let id = id.clone();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.write();
                    conn.execute(
                        "UPDATE memories
                         SET embedding = ?1,
                             embedding_provider = ?2,
                             embedding_model = ?3,
                             embedding_dimensions = ?4
                         WHERE id = ?5",
                        params![bytes, provider_name, model_name, dimensions, id],
                    )?;
                    Ok(())
                })
                .await??;
                count += 1;
            }
        }

        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let pool = self.pool.clone();
        let chunks: Vec<(String, String, Option<String>)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT chunk_id, content, ma_scope_id
                 FROM document_chunks
                 WHERE embedding IS NULL
                    OR embedding_provider IS NULL
                    OR embedding_model IS NULL
                    OR embedding_dimensions IS NULL
                    OR embedding_provider != ?1
                    OR embedding_model != ?2
                    OR embedding_dimensions != ?3",
            )?;
            let rows = stmt.query_map(params![provider_name, model_name, dimensions], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        for (chunk_id, content, scope) in &chunks {
            if let Ok(Some(emb)) = self.embed_and_cache(content, scope.as_deref()).await {
                let bytes = vector::vec_to_bytes(&emb);
                let pool = self.pool.clone();
                let chunk_id = chunk_id.clone();
                let provider_name = self.embedding_provider_name();
                let model_name = self.embedding_model_name();
                let dimensions = self.embedding_dimensions_i64();
                crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.write();
                    conn.execute(
                        "UPDATE document_chunks
                         SET embedding = ?1,
                             embedding_provider = ?2,
                             embedding_model = ?3,
                             embedding_dimensions = ?4
                         WHERE chunk_id = ?5",
                        params![bytes, provider_name, model_name, dimensions, chunk_id],
                    )?;
                    Ok(())
                })
                .await??;
                count += 1;
            }
        }

        Ok(count)
    }

    pub(super) async fn mirror_operator_write_impl(
        &self,
        key: &str,
        content: &str,
        category: &MemoryCategory,
    ) -> anyhow::Result<()> {
        // With the memory ACL on the Markdown mirror is off altogether, as it
        // always was.
        if self.acl_enabled {
            return Ok(());
        }
        let db_path = self.db_path.clone();
        let key = key.to_string();
        let content = content.to_string();
        let category = category.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            Self::append_backup_entry(&db_path, &key, &content, &category);
        })
        .await?;
        Ok(())
    }

    /// Keep only the entries whose row lies in `partition` (MA-01h); `None`
    /// keeps all.
    async fn retain_in_partition(
        &self,
        entries: Vec<MemoryEntry>,
        partition: Option<crate::memory::AgentPartition>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if partition.is_none() || entries.is_empty() {
            return Ok(entries);
        }
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            retain_partition_entries(&conn, entries, partition.as_ref())
        })
        .await?
    }

    pub(super) async fn recall_impl(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Compute query embedding (async, before blocking work)
        let query_embedding = self.get_or_compute_embedding(query).await?;

        let pool = self.pool.clone();
        let query = query.to_string();
        let sid = session_id.map(String::from);
        let vector_weight = self.vector_weight;
        let keyword_weight = self.keyword_weight;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let vector_candidate_cap = self.vector_candidate_cap;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let session_ref = sid.as_deref();

            // FTS5 BM25 keyword search
            let keyword_results = match Self::fts5_search(&conn, &query, limit * 2) {
                Ok(results) => results,
                Err(e) => {
                    tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "FTS5 search failed (returning empty)");
                    Vec::new()
                }
            };

            // Vector similarity search (if embeddings available)
            let vector_results = query_embedding.as_ref().map_or_else(Vec::new, |qe| {
                match Self::vector_search(
                    &conn,
                    qe,
                    &embedding_provider,
                    &embedding_model,
                    embedding_dimensions,
                    limit * 2,
                    None,
                    session_ref,
                    vector_candidate_cap,
                ) {
                    Ok(results) => results,
                    Err(e) => {
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "Vector search failed (returning empty)");
                        Vec::new()
                    }
                }
            });

            // Hybrid merge
            let merged = if vector_results.is_empty() {
                keyword_results
                    .iter()
                    .map(|(id, score)| vector::ScoredResult {
                        id: id.clone(),
                        vector_score: None,
                        keyword_score: Some(*score),
                        final_score: *score,
                    })
                    .collect::<Vec<_>>()
            } else {
                vector::hybrid_merge(&vector_results, &keyword_results, vector_weight, keyword_weight, limit)
            };

            // Fetch full entries for merged results in a single query
            // instead of N round-trips (N+1 pattern).
            let mut results = Vec::new();
            if !merged.is_empty() {
                let placeholders: String = (1..=merged.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT id, key, content, category, created_at, session_id, useful_count \
                     FROM memories WHERE id IN ({placeholders})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let id_params: Vec<Box<dyn rusqlite::types::ToSql>> = merged
                    .iter()
                    .map(|s| Box::new(s.id.clone()) as Box<dyn rusqlite::types::ToSql>)
                    .collect();
                let params_ref: Vec<&dyn rusqlite::types::ToSql> = id_params.iter().map(AsRef::as_ref).collect();
                let rows = stmt.query_map(params_ref.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<u32>>(6)?,
                    ))
                })?;

                let mut entry_map = std::collections::HashMap::new();
                for row in rows {
                    let (id, key, content, cat, ts, sid, useful_count) = row?;
                    entry_map.insert(id, (key, content, cat, ts, sid, useful_count));
                }

                for scored in &merged {
                    if let Some((key, content, cat, ts, sid, useful_count)) = entry_map.remove(&scored.id) {
                        let entry = MemoryEntry {
                            origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                            source_event_id: None,
                            id: scored.id.clone(),
                            key,
                            content,
                            category: Self::str_to_category(&cat),
                            timestamp: ts,
                            session_id: sid,
                            score: Some(f64::from(scored.final_score)),
                            tags: None,
                            access_count: None,
                            useful_count,
                            source: None,
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        };
                        if let Some(filter_sid) = session_ref {
                            if entry.session_id.as_deref() != Some(filter_sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            // If hybrid returned nothing, fall back to LIKE search.
            // Cap keyword count so we don't create too many SQL shapes,
            // which helps prepared-statement cache efficiency.
            if results.is_empty() {
                const MAX_LIKE_KEYWORDS: usize = 8;
                let keywords: Vec<String> = query
                    .split_whitespace()
                    .take(MAX_LIKE_KEYWORDS)
                    .map(|w| format!("%{w}%"))
                    .collect();
                if !keywords.is_empty() {
                    let conditions: Vec<String> = keywords
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("(content LIKE ?{} OR key LIKE ?{})", i * 2 + 1, i * 2 + 2))
                        .collect();
                    let where_clause = conditions.join(" OR ");
                    let sql = format!(
                        "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                         WHERE {where_clause}
                         ORDER BY updated_at DESC
                         LIMIT ?{}",
                        keywords.len() * 2 + 1
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    for kw in &keywords {
                        param_values.push(Box::new(kw.clone()));
                        param_values.push(Box::new(kw.clone()));
                    }
                    #[allow(clippy::cast_possible_wrap)]
                    param_values.push(Box::new(limit as i64));
                    let params_ref: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(AsRef::as_ref).collect();
                    let rows = stmt.query_map(params_ref.as_slice(), |row| {
                        Ok(MemoryEntry {
                            origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                            source_event_id: None,
                            id: row.get(0)?,
                            key: row.get(1)?,
                            content: row.get(2)?,
                            category: Self::str_to_category(&row.get::<_, String>(3)?),
                            timestamp: row.get(4)?,
                            session_id: row.get(5)?,
                            score: Some(1.0),
                            tags: None,
                            access_count: None,
                            useful_count: row.get(6)?,
                            source: None,
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        })
                    })?;
                    for row in rows {
                        let entry = row?;
                        if let Some(sid) = session_ref {
                            if entry.session_id.as_deref() != Some(sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            results.truncate(limit);
            Ok(results)
        })
        .await?
    }

    pub(super) async fn recall_with_context_impl(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let Some(context) = context.cloned() else {
            return self.recall(query, limit, session_id).await;
        };

        let mut entries = self
            .recall(query, limit.saturating_mul(3).max(limit), session_id)
            .await?;
        if entries.is_empty() {
            return Ok(entries);
        }

        let pool = self.pool.clone();
        let capped_limit = limit;

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let principal = sqlite_read_principal(&conn, &context);
            // FIX-P1-06: the Anonymous `(channel, chat_id, raw_sender)` triple is
            // now produced by `build_sql_scope` itself, so all roles share the
            // single scope builder (no hardcoded branch).
            let (scope_sql, scope_params) = principal.build_sql_scope();

            let mut allowed_ids = std::collections::HashSet::new();
            if !entries.is_empty() {
                let id_placeholders = (0..entries.len()).map(|_| "?").collect::<Vec<_>>().join(",");
                let (partition_sql, partition_params) =
                    memory_partition_conjunct(super::events::MA_SCOPE_COLUMN, context.agent_partition.as_ref());
                let sql = format!(
                    "SELECT id, source_event_id FROM memories WHERE id IN ({id_placeholders}) AND ({scope_sql}){partition_sql}"
                );
                let mut params = entries
                    .iter()
                    .map(|entry| Value::from(entry.id.clone()))
                    .collect::<Vec<_>>();
                params.extend(scope_params);
                params.extend(partition_params);
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(params), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                for row in rows {
                    let (id, event) = row?;
                    for entry in &mut entries {
                        if entry.id == id {
                            entry.source_event_id = event.clone();
                        }
                    }
                    allowed_ids.insert(id);
                }
            }

            let visible = entries
                .into_iter()
                .filter(|entry| allowed_ids.contains(&entry.id))
                .collect::<Vec<_>>();
            let mut visible = post_filter(visible, &principal, |entry| entry.content.as_str());
            visible.truncate(capped_limit);
            Ok(visible)
        })
        .await?
    }

    pub(super) async fn count_rehearsal_hits_impl(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=16).contains(&limit),
            "invalid rehearsal count bounds"
        );
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        let fts_query = crate::memory::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(0);
        }
        let db_path = self.db_path.clone();
        let context = context.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<u32> {
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            // A dedicated read-only connection cannot wait for a pooled reader
            // after the decision deadline. SQLite itself interrupts slow FTS.
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let principal = sqlite_read_principal(&conn, &context);
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let (partition_sql, partition_params) =
                memory_partition_conjunct(MEMORY_ALIAS_SCOPE_COLUMN, context.agent_partition.as_ref());
            let sql = format!(
                "SELECT m.content FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}){partition_sql} \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + partition_params.len() + 2);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
            params.extend(partition_params);
            params.push(Value::from(i64::from(limit) * 3));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
            let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            let visible = post_filter(candidates, &principal, String::as_str);
            anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
            Ok(u32::try_from(visible.len().min(usize::from(limit)))?)
        })
        .await?
    }

    pub(super) async fn retrieve_decision_memory_impl(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=5).contains(&limit),
            "invalid decision Memory bounds"
        );
        anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
        anyhow::ensure!(
            session_id.is_none_or(|id| !id.is_empty() && id.len() <= 512),
            "invalid decision Memory session"
        );
        let fts_query = crate::memory::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let session_id = session_id.map(str::to_owned);
        let db_path = self.db_path.clone();
        let context = context.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<DecisionMemoryHit>> {
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
            let principal = sqlite_read_principal(&conn, &context);
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let (partition_sql, partition_params) =
                memory_partition_conjunct(MEMORY_ALIAS_SCOPE_COLUMN, context.agent_partition.as_ref());
            let sql = format!(
                "SELECT m.id, m.content, m.created_at, m.source_event_id, m.key, m.category, m.session_id FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}){partition_sql} \
                 AND length(CAST(m.id AS BLOB)) <= 128 \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 AND length(CAST(m.created_at AS BLOB)) <= 64 \
                 AND (m.source_event_id IS NULL OR length(CAST(m.source_event_id AS BLOB)) <= 128) \
                 AND length(CAST(m.key AS BLOB)) <= 1024 AND length(CAST(m.category AS BLOB)) <= 128 \
                 AND (m.session_id IS NULL OR length(CAST(m.session_id AS BLOB)) <= 512) \
                 AND (? IS NULL OR m.session_id = ?) \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + partition_params.len() + 4);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
            params.extend(partition_params);
            params.push(session_id.clone().map(Value::from).unwrap_or(Value::Null));
            params.push(session_id.map(Value::from).unwrap_or(Value::Null));
            params.push(Value::from(i64::from(limit) * 3));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), |row| {
                Ok(DecisionMemoryHit {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
                    source_event_id: row.get(3)?,
                    key: row.get(4)?, category: Self::str_to_category(&row.get::<_, String>(5)?),
                    session_id: row.get(6)?,
                })
            })?;
            let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            let mut visible = post_filter(candidates, &principal, |hit| hit.content.as_str());
            visible.truncate(usize::from(limit));
            anyhow::ensure!(std::time::Instant::now() < deadline, "decision Memory deadline expired");
            Ok(visible)
        })
        .await?
    }

    pub(super) async fn recall_with_context_mode_impl(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5 defence in depth: an owner-isolated context is enforced
        // whatever mode was requested. The tool already selects Enforce for an
        // isolated caller; the backend must not be talked out of it by a
        // caller, a config flag or a future call site.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        // MA-01h: the partition is not an ACL tier; Observe mode never reads
        // past it.
        let unrestricted = self.recall(query, limit, session_id).await?;
        let unrestricted = self
            .retain_in_partition(unrestricted, context.agent_partition.clone())
            .await?;
        let scoped = self
            .recall_with_context(query, limit, session_id, Some(&context))
            .await?;
        let pool = self.pool.clone();
        let query_owned = query.to_string();
        let query_for_topics = query_owned.clone();
        let mut selected = match mode {
            MemoryReadMode::Enforce => scoped.clone(),
            MemoryReadMode::Observe => unrestricted.clone(),
        };
        let mut selected_ids = selected
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let topic_rows =
            crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<(Vec<MemoryEntry>, Principal)> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let principal = sqlite_read_principal(&conn, &context);
                let topic_principal = match mode {
                    MemoryReadMode::Enforce => principal.clone(),
                    MemoryReadMode::Observe => sqlite_owner_read_principal(),
                };
                let mut rows = Vec::new();
                for topic in crate::memory::topic::search_topics_fts(&conn, &query_for_topics, 3)? {
                    for entry in
                        crate::memory::topic::query_topic_context(&conn, &topic.id, &topic_principal, limit.max(1))?
                    {
                        rows.push(MemoryEntry {
                            origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                            source_event_id: None,
                            id: entry.id,
                            key: entry.key,
                            content: entry.content,
                            category: MemoryCategory::Conversation,
                            timestamp: entry.created_at,
                            session_id: None,
                            score: Some(0.55),
                            tags: None,
                            access_count: None,
                            useful_count: None,
                            source: Some("topic_projection".to_string()),
                            source_confidence: None,
                            verification_status: None,
                            lifecycle_state: None,
                            compressed_from: None,
                        });
                    }
                }
                let rows = retain_partition_entries(&conn, rows, context.agent_partition.as_ref())?;
                Ok((rows, principal))
            })
            .await??;
        for entry in topic_rows.0 {
            if selected_ids.insert(entry.id.clone()) {
                selected.push(entry);
            }
        }
        selected.sort_by(|left, right| {
            right
                .score
                .unwrap_or(0.0)
                .partial_cmp(&left.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.key.cmp(&right.key))
        });
        selected.truncate(limit.max(1));

        let would_deny = unrestricted
            .iter()
            .any(|entry| !scoped.iter().any(|allowed| allowed.id == entry.id));
        let pool = self.pool.clone();
        let principal = topic_rows.1;
        let selected_empty = selected.is_empty();
        crate::runtime::blocking::spawn_blocking(move || {
            let conn = pool.write();
            log_access(
                &conn,
                &principal,
                "search",
                Some(&query_owned),
                None,
                Some(match mode {
                    MemoryReadMode::Enforce => "acl_enforced",
                    MemoryReadMode::Observe => "observe_mode",
                }),
                if mode == MemoryReadMode::Observe && would_deny {
                    "would_deny"
                } else if selected_empty {
                    "no_results"
                } else {
                    "allowed"
                },
            );
        })
        .await?;
        Ok(selected)
    }

    pub(super) async fn get_impl(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories WHERE key = ?1",
            )?;

            let mut rows = stmt.query_map(params![key], |row| {
                Ok(MemoryEntry {
                    origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                    source_event_id: None,
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: row.get(6)?,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                })
            })?;

            match rows.next() {
                Some(Ok(entry)) => Ok(Some(entry)),
                _ => Ok(None),
            }
        })
        .await?
    }

    pub(super) async fn get_with_context_impl(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        let pool = self.pool.clone();
        let key = key.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let principal = sqlite_read_principal(&conn, &context);
            let (scope_sql, scope_params) = principal.build_sql_scope();
            let (partition_sql, partition_params) =
                memory_partition_conjunct(super::events::MA_SCOPE_COLUMN, context.agent_partition.as_ref());
            let mut query_params = Vec::with_capacity(scope_params.len() + partition_params.len() + 1);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            query_params.extend(partition_params);
            let sql = format!(
                "SELECT id, key, content, category, created_at, session_id, useful_count
                 FROM memories WHERE key = ?1 AND ({scope_sql}){partition_sql} LIMIT 1"
            );
            let entry = conn
                .query_row(&sql, params_from_iter(query_params), |row| {
                    Ok(MemoryEntry {
                        origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                        source_event_id: None,
                        id: row.get(0)?,
                        key: row.get(1)?,
                        content: row.get(2)?,
                        category: Self::str_to_category(&row.get::<_, String>(3)?),
                        timestamp: row.get(4)?,
                        session_id: row.get(5)?,
                        score: None,
                        tags: None,
                        access_count: None,
                        useful_count: row.get(6)?,
                        source: None,
                        source_confidence: None,
                        verification_status: None,
                        lifecycle_state: None,
                        compressed_from: None,
                    })
                })
                .optional()?;
            let mut visible = post_filter(entry.into_iter().collect(), &principal, |entry| entry.content.as_str());
            Ok(visible.pop())
        })
        .await?
    }

    pub(super) async fn get_with_context_mode_impl(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let context = context.cloned().unwrap_or_default();
        // C03 / V02-R5: see `recall_with_context_mode` — isolation outranks the
        // requested mode.
        let mode = if context.require_owner_match {
            MemoryReadMode::Enforce
        } else {
            mode
        };
        let scoped = self.get_with_context(key, Some(&context)).await?;
        let unrestricted = self.get(key).await?;
        // MA-01h: as in `recall_with_context_mode`, Observe never reads past the partition.
        let unrestricted = self
            .retain_in_partition(unrestricted.into_iter().collect(), context.agent_partition.clone())
            .await?
            .pop();
        let selected = match mode {
            MemoryReadMode::Enforce => scoped.clone(),
            MemoryReadMode::Observe => unrestricted.clone(),
        };
        // A scoped Ramflux row read outside its view is audited exactly like a
        // missing key, so the log is not an existence oracle for it.
        let rf_row = key.starts_with(crate::memory::RAMFLUX_MEMORY_KEY_PREFIX);
        let would_deny = !rf_row && unrestricted.is_some() && scoped.is_none();
        let memory_id = selected.as_ref().filter(|_| !rf_row).map(|entry| entry.id.clone());
        let selected_is_some = selected.is_some() && !rf_row;
        let key = key.to_string();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let conn = pool.write();
            let principal = sqlite_read_principal(&conn, &context);
            log_access(
                &conn,
                &principal,
                "get",
                None,
                memory_id.as_deref().or(Some(key.as_str())),
                Some(match mode {
                    MemoryReadMode::Enforce => "acl_enforced",
                    MemoryReadMode::Observe => "observe_mode",
                }),
                if mode == MemoryReadMode::Observe && would_deny {
                    "would_deny"
                } else if selected_is_some {
                    "allowed"
                } else {
                    "denied"
                },
            );
        })
        .await?;
        Ok(selected)
    }

    pub(super) async fn list_impl(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        const DEFAULT_LIST_LIMIT: i64 = 1000;

        let pool = self.pool.clone();
        let category = category.cloned();
        let sid = session_id.map(String::from);

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let session_ref = sid.as_deref();
            let mut results = Vec::new();

            let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<MemoryEntry> {
                Ok(MemoryEntry {
                    origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                    source_event_id: None,
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: row.get(6)?,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                })
            };

            if let Some(ref cat) = category {
                let cat_str = Self::category_to_str(cat);
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                     WHERE category = ?1 ORDER BY updated_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![cat_str, DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id, useful_count FROM memories
                     ORDER BY updated_at DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            }

            Ok(results)
        })
        .await?
    }

    pub(super) async fn list_owner_ids_impl(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let pool = self.pool.clone();
        let category = Self::category_to_str(category);
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let mut stmt = conn.prepare("SELECT id, owner_id FROM memories WHERE category = ?1")?;
                let owners = stmt
                    .query_map(params![category], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<std::collections::HashMap<String, Option<String>>>>()?;
                Ok(Some(owners))
            },
        )
        .await?
    }

    pub(super) async fn forget_with_context_impl(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<bool> {
        let Some(context) = context.cloned() else {
            return self.forget(key).await;
        };

        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let principal = sqlite_read_principal(&conn, &context);
            // FIX-P1-06: unified scope builder for all roles (see recall_with_context).
            let (scope_sql, scope_params) = principal.build_sql_scope();
            // R-B1d D-1: the delete guard. The fixed parameter is ?1 = key and
            // the scope predicate occupies ?2..; the owner guard therefore
            // binds at the next free index and its value is appended last, so
            // the template order is preserved.
            let owner_anchor = Self::forget_owner_anchor(&context);
            let owner_guard = Self::forget_owner_guard(&context, scope_params.len() + 2);
            // MA-01h: a partitioned caller deletes only a row of its own partition.
            // The conjunct's bare markers follow every numbered one.
            let (partition_sql, partition_params) =
                memory_partition_conjunct(super::events::MA_SCOPE_COLUMN, context.agent_partition.as_ref());
            let mut query_params = Vec::with_capacity(scope_params.len() + partition_params.len() + 2);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            if let (true, Some(owner)) = (owner_guard.bound_owner, owner_anchor.as_deref()) {
                query_params.push(Value::from(owner.to_string()));
            }
            query_params.extend(partition_params);
            let owner_guard_sql = owner_guard.sql;
            let query = format!(
                "SELECT id FROM memories WHERE key = ?1 AND ({scope_sql}){owner_guard_sql}{partition_sql} LIMIT 1"
            );
            let visible_id = conn
                .query_row(&query, params_from_iter(query_params), |row| row.get::<_, String>(0))
                .optional()?;

            let Some(memory_id) = visible_id else {
                log_access(&conn, &principal, "forget", Some(&key), None, None, "denied");
                return Ok(false);
            };

            let (forgotten, dropped) = Self::forget_row(&conn, &principal, &key, &memory_id)?;
            drop(conn);
            if dropped > 0 {
                crate::memory::sqlite::transcript_index::drain_after_commit(&pool);
            }
            Ok(forgotten)
        })
        .await?
    }

    /// The delete half shared by the scoped and the in-partition forget: one
    /// transaction takes the prefix rows, the memory, the tool results that
    /// quoted it and the access log entry together or not at all.
    ///
    /// Returns whether a memory was deleted and how many session prefix rows went with it.
    fn forget_row(
        conn: &Connection,
        principal: &Principal,
        key: &str,
        memory_id: &str,
    ) -> anyhow::Result<(bool, usize)> {
        let tx = begin_immediate(conn)?;
        let probes = crate::memory::sqlite::derived_sweep::memory_probes(
            &tx,
            "SELECT workspace_id, owner_id, key, created_at FROM memories WHERE id = ?1",
            params![memory_id],
        )?;
        let dropped = crate::memory::sqlite::rf_forget::drop_prefix_rows_of(
            &tx,
            "SELECT DISTINCT workspace_id, owner_id FROM memories WHERE id = ?1",
            params![memory_id],
        )?;
        let affected = tx.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])?;
        crate::memory::sqlite::derived_sweep::rewrite_memory_copies(&tx, &probes, &chrono::Utc::now().to_rfc3339())?;
        // A scoped Ramflux row forgotten outside its view is audited like a
        // missing key, so the log is not an existence oracle for it.
        if key.starts_with(crate::memory::RAMFLUX_MEMORY_KEY_PREFIX) {
            log_access(&tx, principal, "forget", Some(key), None, None, "denied");
        } else {
            log_access(&tx, principal, "forget", Some(key), Some(memory_id), None, "allowed");
        }
        tx.commit()?;
        Ok((affected > 0, dropped))
    }

    /// The ACL-off delete of an in-process caller: the key inside the
    /// caller's partition, without the visibility predicate (the delete half of
    /// `MemoryReadMode::Observe`).
    pub(super) async fn forget_in_partition_impl(
        &self,
        key: &str,
        context: &MemoryWriteContext,
    ) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        let key = key.to_string();
        let context = context.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let principal = sqlite_read_principal(&conn, &context);
            let (partition_sql, partition_params) =
                memory_partition_conjunct(super::events::MA_SCOPE_COLUMN, context.agent_partition.as_ref());
            let mut query_params = Vec::with_capacity(partition_params.len() + 1);
            query_params.push(Value::from(key.clone()));
            query_params.extend(partition_params);
            let query = format!("SELECT id FROM memories WHERE key = ?1{partition_sql} LIMIT 1");
            let memory_id = conn
                .query_row(&query, params_from_iter(query_params), |row| row.get::<_, String>(0))
                .optional()?;
            let Some(memory_id) = memory_id else {
                log_access(&conn, &principal, "forget", Some(&key), None, None, "denied");
                return Ok(false);
            };
            let (forgotten, dropped) = Self::forget_row(&conn, &principal, &key, &memory_id)?;
            drop(conn);
            if dropped > 0 {
                crate::memory::sqlite::transcript_index::drain_after_commit(&pool);
            }
            Ok(forgotten)
        })
        .await?
    }

    pub(super) async fn move_to_trash_impl(
        &self,
        key: &str,
        reason: &str,
        grace_days: u32,
    ) -> anyhow::Result<Option<String>> {
        let pool = self.pool.clone();
        let key = key.to_string();
        let reason = reason.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let conn = pool.write();
            let tx = begin_immediate(&conn)?;
            // FIX-P1-11: snapshot the row's value before soft-deleting it. We DO NOT
            // physically delete from `memories`; we record a trash row so it can be
            // restored within the grace window. Parent stamp is read inside the
            // writer transaction (review M3).
            let row: Option<(String, String, Option<String>)> = tx
                .query_row(
                    "SELECT content, category, ma_scope_id FROM memories WHERE key = ?1",
                    params![key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((content, category, ma_scope_id)) = row else {
                return Ok(None);
            };
            // Idempotency: if an unrestored trash entry already exists for this key,
            // return it rather than duplicating.
            let existing: Option<String> = tx
                .query_row(
                    "SELECT trash_id FROM memory_trash WHERE memory_key = ?1 AND restored_at IS NULL LIMIT 1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(trash_id) = existing {
                return Ok(Some(trash_id));
            }
            let trash_id = format!("trash-{}", Uuid::now_v7());
            let now = Utc::now();
            let grace_until = now + chrono::Duration::days(i64::from(grace_days));
            tx.execute(
                "INSERT INTO memory_trash (
                    trash_id, memory_key, content, category, reason, trashed_at, grace_until, ma_scope_id
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    trash_id,
                    key,
                    content,
                    category,
                    reason,
                    now.to_rfc3339(),
                    grace_until.to_rfc3339(),
                    ma_scope_id,
                ],
            )?;
            tx.commit()?;
            Ok(Some(trash_id))
        })
        .await?
    }

    pub(super) async fn increment_useful_count_impl(&self, id: &str) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let id = id.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            conn.execute(
                "UPDATE memories
                 SET useful_count = COALESCE(useful_count, 0) + 1,
                     updated_at = ?1
                 WHERE id = ?2",
                params![Local::now().to_rfc3339(), id],
            )?;
            Ok(())
        })
        .await?
    }
}

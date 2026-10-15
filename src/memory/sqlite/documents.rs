//! Document ingestion, chunk search, source links, retrieval traces and compaction runs.

use super::vector;
use crate::memory::principal::{OwnerPrincipal, Role};
use crate::memory::traits::{
    CompactionRun, CompactionRunInput, DocumentChunkRecord, DocumentIngestInput, DocumentRecord, DocumentSearchResult,
    MemoryLink, MemoryLinkInput, MemoryPrincipal, RetrievalTrace, RetrievalTraceInput,
};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::{SQLITE_DIALECT, SqliteMemory};

impl SqliteMemory {
    fn document_owner_for_principal(principal: &MemoryPrincipal) -> Option<String> {
        // C03 / V02-R4: an owner-isolated principal's channel/sender pair is the
        // caller-chosen external subject alone, so deriving the document owner
        // from it would let two issuers sharing a subject read each other's
        // private documents. Use the authenticated owner id instead; when it is
        // absent the principal admits no owner-scoped document at all.
        if principal.require_owner_match {
            return crate::memory::owner_predicate::has_owner_id(principal)
                .then(|| principal.owner_id.clone())
                .flatten();
        }
        let channel = principal.channel.as_deref()?.trim();
        let sender = principal.sender.as_deref()?.trim();
        if channel.is_empty() || sender.is_empty() {
            return None;
        }
        Some(
            OwnerPrincipal::new(
                principal.workspace_id.clone(),
                channel,
                sender,
                principal.session_key.clone().unwrap_or_default(),
                vec![Role::Anonymous],
            )
            .owner_id,
        )
    }

    /// The guard appended to the *visibility* disjunct of every document chunk
    /// search statement (V02-R8).
    ///
    /// Those statements admit a chunk when the owning document is `global` or
    /// `workspace` **or** when the chunk's owner matches the caller. The first
    /// disjunct short-circuits the second, and `documents.visibility` defaults
    /// to `workspace`, so an owner-isolated caller was handed a snippet of every
    /// other owner's documents — including the conversation transcript the
    /// OS-paging evictor ingests verbatim.
    ///
    /// `document_search` renders chunk content to its caller, so it has to agree
    /// with `document_get_chunk`'s `chunk_visible_to_principal`, which is
    /// already the intended semantic: an owner-isolated principal reads a chunk
    /// only when the chunk's `owner_id` equals its authenticated owner id. A
    /// `global` or `workspace` document owned by someone else and an
    /// unattributed chunk are both hidden, and a caller with no owner id reaches
    /// nothing. The owner disjunct beside this guard already expresses exactly
    /// that test, so the whole correction is to stop the visibility disjunct
    /// short-circuiting it.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — asked for an arm that carries no owner of its own
    /// (`has_owner = false`), which is precisely what this arm is: a document's
    /// visibility proves nothing about who owns the chunk. It therefore renders
    /// the empty string for a non-isolated principal, leaving the statement and
    /// its bind list byte-identical, and `" AND FALSE"` for an isolated one.
    /// It binds no parameter in either case, so the placeholder accounting of
    /// these statements is unchanged.
    pub(super) fn document_visibility_guard(principal: &MemoryPrincipal) -> String {
        // Unused: this arm never binds, because it never has an owner to bind.
        const UNBOUND_INDEX: usize = 1;
        crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            UNBOUND_INDEX,
            principal.require_owner_match,
            false,
        )
        .sql
    }

    pub(super) async fn ingest_document_impl(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let pool = self.pool.clone();
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedding_dimensions_i64();
        let raw_chunks: Vec<(usize, Option<String>, String)> =
            crate::memory::chunker::chunk_markdown(&input.content, 1_000)
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
            let embedding_bytes = if self.embedder.dimensions() == 0 {
                None
            } else {
                self.get_or_compute_embedding(&content)
                    .await?
                    .map(|embedding| vector::vec_to_bytes(&embedding))
            };
            prepared_chunks.push((chunk_index, heading, content, embedding_bytes));
        }

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<DocumentRecord> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let document_id = input.document_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_sha256 = Self::content_sha256_hex(&input.content);
            let visibility = input.visibility.as_str().to_string();
            let chunk_count = prepared_chunks.len();

            tx.execute(
                "INSERT INTO documents (
                    document_id, workspace_id, owner_id, topic_id, task_id, source_message_event_id,
                    source_kind, source_uri, title, content_sha256, mime_type, visibility,
                    metadata_json, chunk_count, created_at, updated_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)
                 ON CONFLICT(document_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    topic_id = excluded.topic_id,
                    task_id = excluded.task_id,
                    source_message_event_id = excluded.source_message_event_id,
                    source_kind = excluded.source_kind,
                    source_uri = excluded.source_uri,
                    title = excluded.title,
                    content_sha256 = excluded.content_sha256,
                    mime_type = excluded.mime_type,
                    visibility = excluded.visibility,
                    metadata_json = excluded.metadata_json,
                    chunk_count = excluded.chunk_count,
                    updated_at = excluded.updated_at",
                params![
                    document_id,
                    input.workspace_id,
                    input.owner_id,
                    input.topic_id,
                    input.task_id,
                    input.source_message_event_id,
                    input.source_kind,
                    input.source_uri,
                    input.title,
                    content_sha256,
                    input.mime_type,
                    visibility,
                    input.metadata_json,
                    i64::try_from(chunk_count).unwrap_or(i64::MAX),
                    now,
                ],
            )?;
            tx.execute(
                "DELETE FROM document_chunks WHERE document_id = ?1",
                params![document_id],
            )?;

            for (chunk_index, heading, content, embedding_bytes) in prepared_chunks {
                let chunk_id = format!("{document_id}:chunk:{chunk_index}");
                let source_anchor = format!("{document_id}#chunk-{chunk_index}");
                let token_estimate = content.chars().count().div_ceil(4);
                let chunk_hash = Self::content_sha256_hex(&content);
                let chunk_embedding_provider = embedding_bytes.as_ref().map(|_| embedding_provider.clone());
                let chunk_embedding_model = embedding_bytes.as_ref().map(|_| embedding_model.clone());
                let chunk_embedding_dimensions = embedding_bytes.as_ref().map(|_| embedding_dimensions);
                tx.execute(
                    "INSERT INTO document_chunks (
                        chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                        chunk_index, heading, content, content_sha256, embedding,
                        embedding_provider, embedding_model, embedding_dimensions,
                        source_anchor, token_estimate, created_at
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![
                        chunk_id,
                        document_id,
                        input.workspace_id,
                        input.owner_id,
                        input.topic_id,
                        input.task_id,
                        i64::try_from(chunk_index).unwrap_or(i64::MAX),
                        heading,
                        content,
                        chunk_hash,
                        embedding_bytes,
                        chunk_embedding_provider,
                        chunk_embedding_model,
                        chunk_embedding_dimensions,
                        source_anchor,
                        i64::try_from(token_estimate).unwrap_or(i64::MAX),
                        now,
                    ],
                )?;
            }

            tx.execute(
                // C03: the ingested document's own owner is recorded in the
                // column, so a `session`/`private` ingest event is attributed.
                // V02-R6: the turn's scoping keys are bound too, so the event is
                // reachable through the `session`/`agent`/`private` arms of the
                // read predicate instead of carrying unmatchable NULLs.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'document.ingested', 'documents', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    input.workspace_id,
                    input.owner_id,
                    document_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.visibility.as_str(),
                    serde_json::json!({
                        "owner_id": input.owner_id,
                        "topic_id": input.topic_id,
                        "task_id": input.task_id,
                        "source_message_event_id": input.source_message_event_id,
                        "chunk_count": chunk_count,
                        "content_sha256": content_sha256
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            let document = tx.query_row(
                "SELECT id, document_id, workspace_id, owner_id, topic_id, task_id,
                        source_message_event_id, source_kind, source_uri, title,
                        content_sha256, mime_type, visibility, metadata_json,
                        chunk_count, created_at, updated_at
                 FROM documents
                 WHERE document_id = ?1
                 LIMIT 1",
                params![document_id],
                Self::document_from_row,
            )?;
            tx.commit()?;
            Ok(document)
        })
        .await?
    }

    pub(super) async fn retrieve_decision_documents_impl(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionDocumentHit>> {
        anyhow::ensure!(
            query.len() <= 1_024 && (1..=5).contains(&limit),
            "invalid decision Document bounds"
        );
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "decision Document deadline expired"
        );
        let fts_query = crate::memory::topic::build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let db_path = self.db_path.clone();
        let principal = principal.clone();
        let owner_id = Self::document_owner_for_principal(&principal);
        let visibility_guard = Self::document_visibility_guard(&principal);
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Vec<crate::memory::traits::DecisionDocumentHit>> {
                let conn = Self::open_rehearsal_reader(&db_path, deadline)?;
                let mut stmt = conn.prepare(&format!(
                    "SELECT c.chunk_id, c.document_id, c.content, c.created_at
                 FROM document_chunks_fts f
                 JOIN document_chunks c ON c.rowid = f.rowid
                 JOIN documents d ON d.document_id = c.document_id
                 WHERE document_chunks_fts MATCH ?1
                   AND c.workspace_id = ?2 AND d.workspace_id = ?2
                   AND (d.visibility IN ('global', 'workspace'){visibility_guard}
                        OR (?3 IS NOT NULL AND c.owner_id = ?3))
                   AND length(CAST(c.chunk_id AS BLOB)) <= 128
                   AND length(CAST(c.document_id AS BLOB)) <= 128
                   AND length(CAST(c.content AS BLOB)) <= 4096
                   AND length(CAST(c.created_at AS BLOB)) <= 64
                 ORDER BY bm25(document_chunks_fts), c.chunk_id LIMIT ?4"
                ))?;
                let rows = stmt.query_map(
                    params![fts_query, principal.workspace_id, owner_id, i64::from(limit)],
                    |row| {
                        Ok(crate::memory::traits::DecisionDocumentHit {
                            chunk_id: row.get(0)?,
                            document_id: row.get(1)?,
                            content: row.get(2)?,
                            created_at: row.get(3)?,
                        })
                    },
                )?;
                let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "decision Document deadline expired"
                );
                Ok(hits)
            },
        )
        .await?
    }

    pub(super) async fn search_document_chunks_impl(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let query = query.to_string();
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let owner_id = Self::document_owner_for_principal(&principal);
        // V02-R8: empty for every non-isolated principal, so both statements
        // below stay byte-identical for them.
        let visibility_guard = Self::document_visibility_guard(&principal);
        let query_embedding = self.get_or_compute_embedding(&query).await?;
        let embedding_provider = self.embedding_provider_name();
        let embedding_model = self.embedding_model_name();
        let embedding_dimensions = self.embedder.dimensions();
        let embedding_dimensions_i64 = self.embedding_dimensions_i64();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<DocumentSearchResult>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let fts_query = crate::memory::topic::build_safe_fts_query(&query);
            let mut results = Vec::new();
            let mut seen = std::collections::HashSet::new();
            if !fts_query.is_empty() {
                let mut stmt = conn.prepare(&format!(
                    "SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                            c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                            c.token_estimate, c.created_at, bm25(document_chunks_fts) AS score,
                            d.source_kind AS source_kind
                     FROM document_chunks_fts f
                     JOIN document_chunks c ON c.rowid = f.rowid
                     JOIN documents d ON d.document_id = c.document_id
                     WHERE document_chunks_fts MATCH ?1
                       AND c.workspace_id = ?2
                       AND (
                           d.visibility IN ('global', 'workspace'){visibility_guard}
                           OR (?3 IS NOT NULL AND c.owner_id = ?3)
                       )
                     ORDER BY score ASC
                     LIMIT ?4",
                ))?;
                let rows = stmt.query_map(params![fts_query, principal.workspace_id, owner_id, limit], |row| {
                    let chunk = Self::document_chunk_from_row(row)?;
                    let score: f32 = row.get(14)?;
                    let source_kind: Option<String> = row.get(15)?;
                    Ok(DocumentSearchResult {
                        chunk,
                        score,
                        source_kind,
                    })
                })?;
                for row in rows {
                    let result = row?;
                    seen.insert(result.chunk.chunk_id.clone());
                    results.push(result);
                }
            }

            if let Some(query_embedding) = query_embedding {
                let mut stmt = conn.prepare(&format!(
                    "SELECT c.id, c.chunk_id, c.document_id, c.workspace_id, c.owner_id, c.topic_id, c.task_id,
                            c.chunk_index, c.heading, c.content, c.content_sha256, c.source_anchor,
                            c.token_estimate, c.created_at, c.embedding, d.source_kind AS source_kind
                     FROM document_chunks c
                     JOIN documents d ON d.document_id = c.document_id
                     WHERE c.embedding IS NOT NULL
                       AND c.embedding_provider = ?1
                       AND c.embedding_model = ?2
                       AND c.embedding_dimensions = ?3
                       AND c.workspace_id = ?4
                       AND (
                           d.visibility IN ('global', 'workspace'){visibility_guard}
                           OR (?5 IS NOT NULL AND c.owner_id = ?5)
                       )",
                ))?;
                let rows = stmt.query_map(
                    params![
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions_i64,
                        principal.workspace_id,
                        owner_id
                    ],
                    |row| {
                        let chunk = Self::document_chunk_from_row(row)?;
                        let embedding_blob: Vec<u8> = row.get(14)?;
                        let source_kind: Option<String> = row.get(15)?;
                        Ok((chunk, embedding_blob, source_kind))
                    },
                )?;
                let mut vector_results = Vec::new();
                for row in rows {
                    let (chunk, embedding_blob, source_kind) = row?;
                    if seen.contains(&chunk.chunk_id) {
                        continue;
                    }
                    let embedding = vector::bytes_to_vec(&embedding_blob);
                    if embedding.len() != embedding_dimensions {
                        tracing::debug!(
                            chunk_id = %chunk.chunk_id,
                            expected_dimensions = embedding_dimensions,
                            actual_dimensions = embedding.len(),
                            "Skipping stale document chunk embedding with mismatched dimensions"
                        );
                        continue;
                    }
                    let score = vector::cosine_similarity(&query_embedding, &embedding);
                    if score > 0.0 {
                        vector_results.push(DocumentSearchResult {
                            chunk,
                            score,
                            source_kind,
                        });
                    }
                }
                vector_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                for result in vector_results {
                    if results.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
                        break;
                    }
                    seen.insert(result.chunk.chunk_id.clone());
                    results.push(result);
                }
            }

            Ok(results)
        })
        .await?
    }

    pub(super) async fn get_document_chunk_impl(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        let pool = self.pool.clone();
        let chunk_id = chunk_id.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<DocumentChunkRecord>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            conn.query_row(
                "SELECT id, chunk_id, document_id, workspace_id, owner_id, topic_id, task_id,
                        chunk_index, heading, content, content_sha256, source_anchor,
                        token_estimate, created_at
                 FROM document_chunks
                 WHERE chunk_id = ?1
                 LIMIT 1",
                params![chunk_id],
                Self::document_chunk_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn link_memory_source_impl(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryLink> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let link_id = input.link_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            conn.execute(
                "INSERT OR IGNORE INTO memory_links (
                    link_id, workspace_id, owner_id, memory_key, memory_event_id,
                    message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    link_id,
                    input.workspace_id,
                    input.owner_id,
                    input.memory_key,
                    input.memory_event_id,
                    input.message_event_id,
                    input.document_id,
                    input.chunk_id,
                    input.link_type,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, link_id, workspace_id, owner_id, memory_key, memory_event_id,
                        message_event_id, document_id, chunk_id, link_type, payload_json, created_at
                 FROM memory_links
                 WHERE link_id = ?1
                 LIMIT 1",
                params![link_id],
                Self::memory_link_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn append_retrieval_trace_impl(
        &self,
        input: RetrievalTraceInput,
    ) -> anyhow::Result<RetrievalTrace> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<RetrievalTrace> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let trace_id = input.trace_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let candidate_count = i64::try_from(input.candidate_count).unwrap_or(i64::MAX);
            let selected_count = i64::try_from(input.selected_count).unwrap_or(i64::MAX);
            let dropped_count = i64::try_from(input.dropped_count).unwrap_or(i64::MAX);
            let budget_tokens = input
                .budget_tokens
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
            conn.execute(
                "INSERT INTO retrieval_traces (
                    trace_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    source, query, candidate_count, selected_count, dropped_count,
                    budget_tokens, selected_json, dropped_json, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(trace_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    session_key = excluded.session_key,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    source = excluded.source,
                    query = excluded.query,
                    candidate_count = excluded.candidate_count,
                    selected_count = excluded.selected_count,
                    dropped_count = excluded.dropped_count,
                    budget_tokens = excluded.budget_tokens,
                    selected_json = excluded.selected_json,
                    dropped_json = excluded.dropped_json,
                    payload_json = excluded.payload_json",
                params![
                    trace_id,
                    input.workspace_id,
                    input.owner_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.source,
                    input.query,
                    candidate_count,
                    selected_count,
                    dropped_count,
                    budget_tokens,
                    input.selected_json,
                    input.dropped_json,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, trace_id, workspace_id, owner_id, session_key, agent_id,
                        persona_id, source, query, candidate_count, selected_count,
                        dropped_count, budget_tokens, selected_json, dropped_json,
                        payload_json, created_at
                 FROM retrieval_traces
                 WHERE trace_id = ?1
                 LIMIT 1",
                params![trace_id],
                Self::retrieval_trace_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }

    pub(super) async fn append_compaction_run_impl(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        input.validate_source_event_provenance()?;
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<CompactionRun> {
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let source_message_count = i64::try_from(input.source_message_count).unwrap_or(i64::MAX);
            let source_token_estimate = i64::try_from(input.source_token_estimate).unwrap_or(i64::MAX);
            conn.execute(
                "INSERT INTO compaction_runs (
                    run_id, workspace_id, owner_id, session_key, agent_id, persona_id,
                    \"trigger\", mode, source_message_count, source_token_estimate,
                    summary, summary_memory_key, source_event_ids_json,
                    source_event_range_json, source_document_refs_json,
                    fidelity_status, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(run_id) DO UPDATE SET
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    session_key = excluded.session_key,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    \"trigger\" = excluded.\"trigger\",
                    mode = excluded.mode,
                    source_message_count = excluded.source_message_count,
                    source_token_estimate = excluded.source_token_estimate,
                    summary = excluded.summary,
                    summary_memory_key = excluded.summary_memory_key,
                    source_event_ids_json = excluded.source_event_ids_json,
                    source_event_range_json = excluded.source_event_range_json,
                    source_document_refs_json = excluded.source_document_refs_json,
                    fidelity_status = excluded.fidelity_status,
                    payload_json = excluded.payload_json",
                params![
                    run_id,
                    input.workspace_id,
                    input.owner_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.trigger,
                    input.mode,
                    source_message_count,
                    source_token_estimate,
                    input.summary,
                    input.summary_memory_key,
                    input.source_event_ids_json,
                    input.source_event_range_json,
                    input.source_document_refs_json,
                    input.fidelity_status,
                    input.payload_json,
                    now,
                ],
            )?;
            conn.query_row(
                "SELECT id, run_id, workspace_id, owner_id, session_key, agent_id,
                        persona_id, \"trigger\", mode, source_message_count, source_token_estimate,
                        summary, summary_memory_key, source_event_ids_json,
                        source_event_range_json, source_document_refs_json,
                        fidelity_status, payload_json, created_at
                 FROM compaction_runs
                 WHERE run_id = ?1
                 LIMIT 1",
                params![run_id],
                Self::compaction_run_from_row,
            )
            .map_err(Into::into)
        })
        .await?
    }
}

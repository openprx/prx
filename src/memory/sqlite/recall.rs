//! Store, recall, forget, FTS5/vector search, owner guards and reindex.

use super::vector;
use crate::memory::filter::{MemorySafetyFilter, SourceMetadata, safety_rejection_message};
use crate::memory::principal::{ChatType, MemoryWriteContext, Principal, classify_memory, log_access, post_filter};
use crate::memory::topic::resolve_topic;
use crate::memory::traits::{
    DecisionMemoryHit, Memory, MemoryCategory, MemoryEntry, MemoryReadMode, MemoryStoreMetadata,
    validate_memory_write_target,
};
use crate::self_system::evolution::record::Actor;
use anyhow::Context;
use chrono::{Local, Utc};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use std::fmt::Write as _;
use std::path::Path;
use uuid::Uuid;

use super::{SQLITE_DIALECT, SqliteMemory, sqlite_owner_read_principal, sqlite_read_principal};

/// Default ceiling on the embedded memories `vector_search` will score, used
/// when no configuration is supplied.
///
/// Cosine similarity cannot use an index, so the scan is bounded by insertion
/// order. Operators override this with `[memory] vector_candidate_cap`.
pub(super) const VECTOR_CANDIDATE_CAP: usize = 2_000;

/// Candidate count for one vector recall.
///
/// The configured cap is authoritative — it is the number an operator tunes —
/// and is only ever raised to `limit`, because scoring fewer candidates than
/// the caller asked for results would return a short list for no reason.
/// Before 0.8.126 this was `max(limit * 50, 2000)` with no way to change it,
/// so a store larger than the cap lost recall with nothing to turn.
pub(super) const fn vector_candidate_cap(limit: usize, configured: usize) -> usize {
    if limit > configured { limit } else { configured }
}

pub(super) struct VectorCandidateQuery {
    pub(super) sql: String,
    pub(super) params: Vec<Box<dyn rusqlite::types::ToSql>>,
    candidate_cap: usize,
}

pub(super) fn vector_candidate_query(
    provider: &str,
    model: &str,
    dimensions: usize,
    limit: usize,
    category: Option<&str>,
    session_id: Option<&str>,
    configured_candidate_cap: usize,
) -> VectorCandidateQuery {
    let mut sql = "SELECT id, embedding FROM memories
                   WHERE embedding IS NOT NULL
                     AND embedding_provider = ?1
                     AND embedding_model = ?2
                     AND embedding_dimensions = ?3"
        .to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(provider.to_string()),
        Box::new(model.to_string()),
        Box::new(i64::try_from(dimensions).unwrap_or(i64::MAX)),
    ];
    let mut idx = 4;

    if let Some(category) = category {
        let _ = write!(sql, " AND category = ?{idx}");
        params.push(Box::new(category.to_string()));
        idx += 1;
    }
    if let Some(session_id) = session_id {
        let _ = write!(sql, " AND session_id = ?{idx}");
        params.push(Box::new(session_id.to_string()));
        idx += 1;
    }
    let candidate_cap = vector_candidate_cap(limit, configured_candidate_cap);
    let _ = write!(sql, " ORDER BY update_seq DESC, rowid DESC LIMIT ?{idx}");
    params.push(Box::new(i64::try_from(candidate_cap).unwrap_or(i64::MAX)));

    VectorCandidateQuery {
        sql,
        params,
        candidate_cap,
    }
}

/// One process says "your memory store has outgrown the candidate cap" once.
///
/// This is a warning, not a debug line: past this point vector recall is
/// silently incomplete — an old-but-relevant memory is never scored — and the
/// only fix is an operator raising `[memory] vector_candidate_cap`. Emitting it
/// per recall would bury the turn logs, so it is latched per process.
static VECTOR_CANDIDATE_CAP_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl SqliteMemory {
    pub(super) fn open_rehearsal_reader(db_path: &Path, deadline: std::time::Instant) -> anyhow::Result<Connection> {
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        // macOS may expose a trusted temporary parent through `/var` ->
        // `/private/var`. Resolve the parent while refusing a symlink at the
        // database leaf; a replaced regular file remains a separate threat.
        let parent = db_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing decision database parent"))?;
        let file_name = db_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("missing decision database name"))?;
        let read_path = parent.canonicalize()?.join(file_name);
        anyhow::ensure!(std::time::Instant::now() < deadline, "rehearsal count deadline expired");
        let conn = Connection::open_with_flags(
            read_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(25))?;
        conn.progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline))?;
        Ok(conn)
    }

    /// Inspect one key namespace through a byte-for-byte read-only connection.
    /// This is used by migration dry-runs that must not initialize or mutate
    /// the configured database.
    pub async fn read_key_prefix_read_only(
        workspace_dir: &Path,
        prefix: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let db_path = workspace_dir.join("memory/brain.db");
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let prefix = prefix.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
            let connection = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("failed to open {} read-only", db_path.display()))?;
            let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            let pattern = format!("{escaped}%");
            let mut statement =
                connection.prepare("SELECT key, content FROM memories WHERE key LIKE ?1 ESCAPE '\\'")?;
            let rows = statement.query_map([pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await?
    }

    /// Delete an exact key namespace in one transaction. SQLite FTS triggers
    /// remove the corresponding search rows in the same commit.
    pub async fn delete_key_prefix_transactional(&self, prefix: &str) -> anyhow::Result<usize> {
        let pool = self.pool.clone();
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{escaped}%");
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let affected = tx.execute("DELETE FROM memories WHERE key LIKE ?1 ESCAPE '\\'", params![pattern])?;
            tx.commit()?;
            Ok(affected)
        })
        .await?
    }

    pub(super) async fn store_internal(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: Option<MemoryStoreMetadata>,
    ) -> anyhow::Result<()> {
        validate_memory_write_target(key, session_id)?;
        // V02-R9: an owner-isolated write is confined to the caller's own keys.
        // `Some(owner)` only for an isolated context; a blank owner id refuses
        // here, before anything is written.
        let isolated_owner = crate::memory::owner_predicate::isolated_store_owner(context)?;
        self.check_memory_safety(content, context).await?;

        // Only long-lived categories need vector embeddings.
        let needs_embedding = matches!(&category, MemoryCategory::Core | MemoryCategory::Custom(_));
        let embedding_bytes = if needs_embedding {
            self.get_or_compute_embedding(content)
                .await?
                .map(|emb| vector::vec_to_bytes(&emb))
        } else {
            None
        };
        let embedding_provider = embedding_bytes.as_ref().map(|_| self.embedding_provider_name());
        let embedding_model = embedding_bytes.as_ref().map(|_| self.embedding_model_name());
        let embedding_dimensions = embedding_bytes.as_ref().map(|_| self.embedding_dimensions_i64());

        let pool = self.pool.clone();
        let key = key.to_string();
        let content = content.to_string();
        let sid = session_id.map(String::from);
        let write_ctx = context.cloned();
        let mut metadata = metadata.unwrap_or_default();
        if let Some(owner) = isolated_owner.as_ref() {
            // The row an isolated caller writes is attributed to its
            // authenticated owner and nothing else.
            metadata.owner_id = Some(owner.clone());
        }
        let owner_guard =
            crate::memory::owner_predicate::store_conflict_guard(SQLITE_DIALECT, "memories", isolated_owner.is_some());

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            // R-MCPB F-4: an owner-isolated write — ownership check, guarded
            // upsert and guarded metadata update — is one immediate
            // transaction, so a refusal at any step leaves nothing behind.
            // Non-isolated writes keep their historical autocommit statements.
            let isolated_tx = isolated_owner
                .as_ref()
                .map(|_| rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate))
                .transpose()?;
            let now = Local::now().to_rfc3339();
            let cat = Self::category_to_str(&category);
            let id = Uuid::new_v4().to_string();

            if let Some(ctx) = write_ctx {
                // Checked before topic resolution so a refused write leaves no
                // trace in the topic tables. The conflict guard below is the
                // atomic backstop: another process (a session worker) may
                // write the same database between this read and the upsert.
                if let Some(owner) = isolated_owner.as_deref() {
                    let stored_owner: Option<Option<String>> = conn
                        .query_row(
                            "SELECT owner_id FROM memories WHERE key = ?1",
                            params![&key],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if stored_owner.is_some_and(|stored| stored.as_deref() != Some(owner)) {
                        return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                            key: key.clone(),
                        }
                        .into());
                    }
                }
                let principal = sqlite_read_principal(&conn, &ctx);
                let classified = classify_memory(&ctx, &content, &principal);
                let topic_id = match resolve_topic(&conn, &content, &principal) {
                    Ok(topic_id) => topic_id,
                    Err(error) => {
                        tracing::warn!("memory topic resolve failed: {error}");
                        None
                    }
                };
                let risk_json = serde_json::to_string(&classified.risk_signals)?;
                let sender_id = if ctx.channel.is_some() && ctx.raw_sender.is_some() {
                    Some(principal.user_id)
                } else {
                    None
                };
                let explicit_sender_id = ctx.sender_id.or(sender_id);
                let owner_id = metadata.owner_id.clone().or_else(|| explicit_sender_id.clone());
                let chat_type = ctx
                    .chat_type
                    .map(|raw| ChatType::from_str(&raw).as_str().to_string());

                let changed = conn.execute(
                    &format!("INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, channel, chat_type, chat_id,
                        sender_id, raw_sender, topic_id, visibility, sensitivity,
                        risk_signals, policy_version
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
                     ON CONFLICT(key) DO UPDATE SET
                        content = excluded.content,
                        category = excluded.category,
                        embedding = excluded.embedding,
                        embedding_provider = excluded.embedding_provider,
                        embedding_model = excluded.embedding_model,
                        embedding_dimensions = excluded.embedding_dimensions,
                        updated_at = excluded.updated_at,
                        session_id = excluded.session_id,
                        workspace_id = excluded.workspace_id,
                        owner_id = excluded.owner_id,
                        channel = excluded.channel,
                        chat_type = excluded.chat_type,
                        chat_id = excluded.chat_id,
                        sender_id = excluded.sender_id,
                        raw_sender = excluded.raw_sender,
                        topic_id = excluded.topic_id,
                        visibility = excluded.visibility,
                        sensitivity = excluded.sensitivity,
                        risk_signals = excluded.risk_signals,
                        policy_version = excluded.policy_version{owner_guard}"),
                    params![
                        id,
                        &key,
                        &content,
                        cat,
                        embedding_bytes,
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions,
                        now,
                        now,
                        sid,
                        metadata.workspace_id.clone(),
                        owner_id,
                        ctx.channel,
                        chat_type,
                        ctx.chat_id,
                        explicit_sender_id,
                        ctx.raw_sender,
                        topic_id,
                        classified.visibility.as_str(),
                        classified.sensitivity.as_str(),
                        risk_json,
                        classified.policy_version,
                    ],
                )?;
                // The guard suppressed the update: the key exists under
                // another owner (or none). Nothing was written.
                if isolated_owner.is_some() && changed == 0 {
                    return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                        key: key.clone(),
                    }
                    .into());
                }
            } else {
                conn.execute(
                    "INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, agent_id, persona_id,
                        source_event_id, source, channel, topic_id
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                     ON CONFLICT(key) DO UPDATE SET
                        content = excluded.content,
                        category = excluded.category,
                        embedding = excluded.embedding,
                        embedding_provider = excluded.embedding_provider,
                        embedding_model = excluded.embedding_model,
                        embedding_dimensions = excluded.embedding_dimensions,
                        updated_at = excluded.updated_at,
                        session_id = excluded.session_id,
                        workspace_id = excluded.workspace_id,
                        owner_id = excluded.owner_id,
                        agent_id = excluded.agent_id,
                        persona_id = excluded.persona_id,
                        source_event_id = excluded.source_event_id,
                        source = excluded.source,
                        channel = excluded.channel,
                        topic_id = excluded.topic_id",
                    params![
                        id,
                        &key,
                        &content,
                        cat,
                        embedding_bytes,
                        embedding_provider,
                        embedding_model,
                        embedding_dimensions,
                        now,
                        now,
                        sid,
                        metadata.workspace_id.clone(),
                        metadata.owner_id.clone(),
                        metadata.agent_id.clone(),
                        metadata.persona_id.clone(),
                        metadata.source_event_id.clone(),
                        metadata.source.clone(),
                        // FIX-P1-08: persist the originating channel so anonymous
                        // principals can resolve channel scope on later recall.
                        metadata.channel.clone(),
                        metadata.topic_id.clone(),
                    ],
                )?;
            }

            if metadata.workspace_id.is_some()
                || metadata.owner_id.is_some()
                || metadata.agent_id.is_some()
                || metadata.persona_id.is_some()
                || metadata.source_event_id.is_some()
                || metadata.source.is_some()
            {
                // R-MCPB F-4: for an owner-isolated write the update is
                // confined to the caller's own row (`?2` is its owner id), so
                // a row another writer changed after the upsert is neither
                // re-owned nor rewritten; that write is refused and rolled
                // back instead.
                let metadata_owner_guard = if isolated_owner.is_some() {
                    " AND owner_id IS ?2"
                } else {
                    ""
                };
                let updated = conn.execute(
                    &format!(
                        "UPDATE memories
                     SET workspace_id = ?1,
                         owner_id = COALESCE(?2, owner_id),
                         agent_id = ?3,
                         persona_id = ?4,
                         source_event_id = ?5,
                         source = ?6,
                         updated_at = ?7
                     WHERE key = ?8{metadata_owner_guard}"
                    ),
                    params![
                        metadata.workspace_id,
                        metadata.owner_id,
                        metadata.agent_id,
                        metadata.persona_id,
                        metadata.source_event_id,
                        metadata.source,
                        Local::now().to_rfc3339(),
                        &key,
                    ],
                )?;
                if isolated_owner.is_some() && updated == 0 {
                    return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                        key: key.clone(),
                    }
                    .into());
                }
            }
            if let Some(tx) = isolated_tx {
                tx.commit()?;
            }

            // D-026: no write is mirrored into the workspace Markdown files
            // here. `MEMORY.md` is inlined into every surface's system prompt,
            // so only a write the local operator made is mirrored, explicitly,
            // through `Memory::mirror_operator_write`.
            Ok(())
        })
        .await?
    }

    async fn check_memory_safety(&self, content: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<()> {
        let source = SourceMetadata {
            actor: Self::safety_actor_for_context(context),
            historical_accuracy: None,
        };
        let result = MemorySafetyFilter::default().check(content, &source).await;
        if result.passed {
            Ok(())
        } else {
            anyhow::bail!("{}", safety_rejection_message(&result.issues));
        }
    }

    const fn safety_actor_for_context(context: Option<&MemoryWriteContext>) -> Actor {
        match context {
            Some(ctx) if ctx.raw_sender.is_some() || ctx.sender_id.is_some() => Actor::User,
            Some(_) => Actor::Agent,
            None => Actor::Agent,
        }
    }

    /// Deterministic content hash for embedding cache.
    /// Uses SHA-256 (truncated) instead of DefaultHasher, which is
    /// explicitly documented as unstable across Rust versions.
    pub(super) fn content_hash(text: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(text.as_bytes());
        // First 16 bytes → 32 hex chars. 128-bit hash reduces collision risk
        // to negligible levels (~2^-64 birthday bound vs ~2^-32 with 64-bit).
        let mut hex = String::with_capacity(32);
        // SAFETY: SHA-256 output is always 32 bytes, so ..16 is always valid
        #[allow(clippy::indexing_slicing)]
        for byte in &hash[..16] {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    pub(super) fn content_sha256_hex(text: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(text.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in hash {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// The owner id an owner-isolated delete is confined to, with blanks
    /// treated as absent (the rule
    /// [`crate::memory::owner_predicate::has_owner_id`] applies to reads).
    ///
    /// A non-isolated context never yields one, so the guard below renders
    /// nothing for it and the historical statement is unchanged.
    pub(super) fn forget_owner_anchor(context: &MemoryWriteContext) -> Option<String> {
        if !context.require_owner_match {
            return None;
        }
        context
            .owner_id
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
            .map(str::to_string)
    }

    /// The guard appended to `forget_with_context`'s visibility scope
    /// (R-B1d D-1).
    ///
    /// Delete-by-key was confined to the shared visibility predicate alone.
    /// That predicate owner-qualifies every **non-public** arm for an isolated
    /// caller, but `visibility = 'public'` stays shared by design — correct for
    /// a read, wrong for a destructive operation, because it let an
    /// owner-isolated caller remove a public row belonging to someone else.
    /// This guard is therefore appended at the top level of the `WHERE` rather
    /// than inside one arm: an owner-isolated caller deletes only rows whose
    /// `owner_id` equals its authenticated owner id, whatever their visibility.
    ///
    /// The fragment comes from
    /// [`crate::memory::owner_predicate::owner_match_suffix`] — the single
    /// owner-isolation seam — so it renders the empty string for a
    /// non-isolated caller (statement and bind list byte-identical),
    /// `" AND FALSE"` for an isolated caller with no usable owner id (nothing
    /// is deleted and the call reports that nothing was found), and
    /// `" AND owner_id = ?N"` otherwise. A stored `NULL` `owner_id` compares to
    /// `NULL`, so an unattributed row is not deletable by an isolated caller
    /// either. `index` is the caller's next free placeholder, so the
    /// one-marker-one-parameter accounting is preserved.
    pub(super) fn forget_owner_guard(
        context: &MemoryWriteContext,
        index: usize,
    ) -> crate::memory::owner_predicate::OwnerMatchPredicate {
        crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            index,
            context.require_owner_match,
            Self::forget_owner_anchor(context).is_some(),
        )
    }

    pub(super) fn embedding_provider_name(&self) -> String {
        self.embedder.name().to_string()
    }

    pub(super) fn embedding_model_name(&self) -> String {
        self.embedder.model().to_string()
    }

    pub(super) fn embedding_dimensions_i64(&self) -> i64 {
        i64::try_from(self.embedder.dimensions()).unwrap_or(i64::MAX)
    }

    /// Get embedding from cache, or compute + cache it
    pub(super) async fn get_or_compute_embedding(&self, text: &str) -> anyhow::Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None); // Noop embedder
        }

        let hash = Self::content_hash(text);
        let now = Local::now().to_rfc3339();
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();

        // Check cache (offloaded to blocking thread)
        let pool = self.pool.clone();
        let hash_c = hash.clone();
        let now_c = now.clone();
        let provider_c = provider_name.clone();
        let model_c = model_name.clone();
        let cached = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<Vec<f32>>> {
            let conn = pool.write();
            let mut stmt = conn.prepare(
                "SELECT embedding FROM embedding_cache
                 WHERE content_hash = ?1
                   AND provider = ?2
                   AND model = ?3
                   AND dimensions = ?4",
            )?;
            let blob: Option<Vec<u8>> = stmt
                .query_row(params![hash_c, provider_c, model_c, dimensions], |row| row.get(0))
                .ok();
            if let Some(bytes) = blob {
                // FIX-P0-26: touch the exact composite-key row, not every row
                // sharing this content_hash.
                conn.execute(
                    "UPDATE embedding_cache SET accessed_at = ?1 \
                     WHERE content_hash = ?2 AND provider = ?3 AND model = ?4 AND dimensions = ?5",
                    params![now_c, hash_c, provider_c, model_c, dimensions],
                )?;
                let embedding = vector::bytes_to_vec(&bytes);
                if embedding.len() == usize::try_from(dimensions).unwrap_or(usize::MAX) {
                    return Ok(Some(embedding));
                }
            }
            Ok(None)
        })
        .await??;

        if cached.is_some() {
            return Ok(cached);
        }

        // Compute the embedding. Remote providers await network I/O here; the
        // default local provider is synchronous CPU work that offloads itself to
        // the blocking pool once a batch is large enough to matter.
        let embedding = self.embedder.embed_one(text).await?;
        if embedding.len() != self.embedder.dimensions() {
            anyhow::bail!(
                "embedding dimension mismatch: provider={} model={} expected={} got={}",
                provider_name,
                model_name,
                self.embedder.dimensions(),
                embedding.len()
            );
        }
        let bytes = vector::vec_to_bytes(&embedding);

        // Store in cache + LRU eviction (offloaded to blocking thread)
        let pool = self.pool.clone();
        #[allow(clippy::cast_possible_wrap)]
        let cache_max = self.cache_max as i64;
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            conn.execute(
                "INSERT OR REPLACE INTO embedding_cache (
                    content_hash, embedding, provider, model, dimensions, created_at, accessed_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![hash, bytes, provider_name, model_name, dimensions, now, now],
            )?;
            // Two-step LRU eviction: count first, then delete oldest if over limit.
            // Avoids relying on MAX() as a scalar function in a LIMIT clause.
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;
            if count > cache_max {
                let to_delete = count - cache_max;
                conn.execute(
                    "DELETE FROM embedding_cache WHERE content_hash IN (
                        SELECT content_hash FROM embedding_cache
                        ORDER BY accessed_at ASC LIMIT ?1
                    )",
                    params![to_delete],
                )?;
            }
            Ok(())
        })
        .await??;

        Ok(Some(embedding))
    }

    /// FTS5 BM25 keyword search
    fn fts5_search(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<(String, f32)>> {
        // Escape FTS5 special chars and build query
        let fts_query: String = crate::memory::topic::build_safe_fts_query(query);

        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let sql = "SELECT m.id, bm25(memories_fts) as score
                   FROM memories_fts f
                   JOIN memories m ON m.rowid = f.rowid
                   WHERE memories_fts MATCH ?1
                   ORDER BY score
                   LIMIT ?2";

        let mut stmt = conn.prepare(sql)?;
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        let rows = stmt.query_map(params![fts_query, limit_i64], |row| {
            let id: String = row.get(0)?;
            let score: f64 = row.get(1)?;
            // BM25 returns negative scores (lower = better), negate for ranking
            #[allow(clippy::cast_possible_truncation)]
            Ok((id, (-score) as f32))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Vector similarity search: scan embeddings and compute cosine similarity.
    ///
    /// Optional `category` and `session_id` filters reduce full-table scans
    /// when the caller already knows the scope of relevant memories.
    ///
    /// Cosine similarity has no index, so the candidate set is bounded before
    /// scoring: without a bound this is O(N) over every embedded memory on a
    /// path that runs once per turn.
    ///
    /// The bound orders by `update_seq`, not by `updated_at` and not by
    /// `rowid`.
    ///
    /// `updated_at` is `Local::now().to_rfc3339()` — an RFC 3339 string carrying
    /// the writer's UTC offset — and it is ordered lexicographically by SQLite.
    /// Within one fixed offset that happens to match chronological order, but a
    /// DST transition, a machine timezone change, or a store shared between
    /// processes in different zones reorders whole blocks of rows, and any row
    /// historically written with `Utc::now()` sorts in a third order again. The
    /// rows dropped by `LIMIT` would then be the wrong ones.
    ///
    /// `rowid` cannot be reordered by a clock, but it does not move either:
    /// every writer here upserts with `ON CONFLICT(key) DO UPDATE`, which
    /// rewrites the content and the embedding of an existing key in place. A
    /// long-lived key refreshed a second ago therefore kept its original
    /// position, and in a store larger than the cap it stayed outside the
    /// window — the memory whose embedding was *just* made relevant was the one
    /// that could not be scored.
    ///
    /// `update_seq` is a per-store counter bumped by a trigger on every insert
    /// and every update (see `init_schema`). It is an integer, so it has no
    /// timezone, no offset and no DST; it is monotonic, so it cannot be
    /// reordered by a clock; and it follows the row when the row is refreshed.
    /// "Newest" here therefore means most recently *written*, created or
    /// refreshed. `rowid` breaks ties so the window is deterministic on a store
    /// whose sequence was backfilled by the migration.
    ///
    /// `candidate_cap` is the configured `[memory] vector_candidate_cap`. Hitting it is
    /// a real recall regression — an old-but-relevant memory is never scored —
    /// so it is reported once per process at `warn`, not at `debug`.
    pub(super) fn vector_search(
        conn: &Connection,
        query_embedding: &[f32],
        provider: &str,
        model: &str,
        dimensions: usize,
        limit: usize,
        category: Option<&str>,
        session_id: Option<&str>,
        candidate_cap: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let VectorCandidateQuery {
            sql,
            params,
            candidate_cap,
        } = vector_candidate_query(provider, model, dimensions, limit, category, session_id, candidate_cap);

        let mut stmt = conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(params_ref.as_slice(), |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        let mut candidates = 0usize;
        for row in rows {
            let (id, blob) = row?;
            candidates += 1;
            let emb = vector::bytes_to_vec(&blob);
            if emb.len() != dimensions {
                tracing::debug!(
                    memory_id = %id,
                    expected_dimensions = dimensions,
                    actual_dimensions = emb.len(),
                    "Skipping stale memory embedding with mismatched dimensions"
                );
                continue;
            }
            let sim = vector::cosine_similarity(query_embedding, &emb);
            if sim > 0.0 {
                scored.push((id, sim));
            }
        }

        if candidates >= candidate_cap && !VECTOR_CANDIDATE_CAP_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                candidate_cap,
                limit,
                "Vector recall hit its candidate cap: older embedded memories are no longer scored and \
                 cannot be recalled. Raise [memory] vector_candidate_cap to score more of the store."
            );
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

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
        let entries: Vec<(String, String)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT id, content
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
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        let mut count = 0;
        for (id, content) in &entries {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
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
        let chunks: Vec<(String, String)> = crate::runtime::blocking::spawn_blocking(move || {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(
                "SELECT chunk_id, content
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
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        for (chunk_id, content) in &chunks {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
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
                    tracing::warn!("FTS5 search failed (returning empty): {e}");
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
                        tracing::warn!("Vector search failed (returning empty): {e}");
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

        let entries = self
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
                let sql = format!("SELECT id FROM memories WHERE id IN ({id_placeholders}) AND ({scope_sql})");
                let mut params = entries
                    .iter()
                    .map(|entry| Value::from(entry.id.clone()))
                    .collect::<Vec<_>>();
                params.extend(scope_params);
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
                for row in rows {
                    allowed_ids.insert(row?);
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
            let sql = format!(
                "SELECT m.content FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}) \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + 2);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
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
            let sql = format!(
                "SELECT m.id, m.content, m.created_at, m.source_event_id, m.key, m.category, m.session_id FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE memories_fts MATCH ? AND ({scope_sql}) \
                 AND length(CAST(m.id AS BLOB)) <= 128 \
                 AND length(CAST(m.content AS BLOB)) <= 4096 \
                 AND length(CAST(m.created_at AS BLOB)) <= 64 \
                 AND (m.source_event_id IS NULL OR length(CAST(m.source_event_id AS BLOB)) <= 128) \
                 AND length(CAST(m.key AS BLOB)) <= 1024 AND length(CAST(m.category AS BLOB)) <= 128 \
                 AND (m.session_id IS NULL OR length(CAST(m.session_id AS BLOB)) <= 512) \
                 AND (? IS NULL OR m.session_id = ?) \
                 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut params = Vec::with_capacity(scope_params.len() + 4);
            params.push(Value::from(fts_query));
            params.extend(scope_params);
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
        let unrestricted = self.recall(query, limit, session_id).await?;
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
            let mut query_params = Vec::with_capacity(scope_params.len() + 1);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            let sql = format!(
                "SELECT id, key, content, category, created_at, session_id, useful_count
                 FROM memories WHERE key = ?1 AND ({scope_sql}) LIMIT 1"
            );
            let entry = conn
                .query_row(&sql, params_from_iter(query_params), |row| {
                    Ok(MemoryEntry {
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
        let selected = match mode {
            MemoryReadMode::Enforce => scoped.clone(),
            MemoryReadMode::Observe => unrestricted.clone(),
        };
        let would_deny = unrestricted.is_some() && scoped.is_none();
        let memory_id = selected.as_ref().map(|entry| entry.id.clone());
        let selected_is_some = selected.is_some();
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
            let mut query_params = Vec::with_capacity(scope_params.len() + 2);
            query_params.push(Value::from(key.clone()));
            query_params.extend(scope_params);
            if let (true, Some(owner)) = (owner_guard.bound_owner, owner_anchor.as_deref()) {
                query_params.push(Value::from(owner.to_string()));
            }
            let owner_guard_sql = owner_guard.sql;
            let query = format!("SELECT id FROM memories WHERE key = ?1 AND ({scope_sql}){owner_guard_sql} LIMIT 1");
            let visible_id = conn
                .query_row(&query, params_from_iter(query_params), |row| row.get::<_, String>(0))
                .optional()?;

            let Some(memory_id) = visible_id else {
                log_access(&conn, &principal, "forget", Some(&key), None, None, "denied");
                return Ok(false);
            };

            let affected = conn.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])?;
            log_access(
                &conn,
                &principal,
                "forget",
                Some(&key),
                Some(&memory_id),
                None,
                "allowed",
            );
            Ok(affected > 0)
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
            // FIX-P1-11: snapshot the row's value before soft-deleting it. We DO NOT
            // physically delete from `memories`; we record a trash row so it can be
            // restored within the grace window.
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT content, category FROM memories WHERE key = ?1",
                    params![key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((content, category)) = row else {
                return Ok(None);
            };
            // Idempotency: if an unrestored trash entry already exists for this key,
            // return it rather than duplicating.
            let existing: Option<String> = conn
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
            conn.execute(
                "INSERT INTO memory_trash (
                    trash_id, memory_key, content, category, reason, trashed_at, grace_until
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    trash_id,
                    key,
                    content,
                    category,
                    reason,
                    now.to_rfc3339(),
                    grace_until.to_rfc3339(),
                ],
            )?;
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

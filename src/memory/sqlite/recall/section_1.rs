// Store, recall, forget, FTS5/vector search, owner guards and reindex.

use super::vector;
use crate::memory::filter::{MemorySafetyFilter, SourceMetadata, safety_rejection_message};
use crate::memory::principal::{ChatType, MemoryWriteContext, Principal, classify_memory, log_access, post_filter};
use crate::memory::topic::{delete_orphan_topic, resolve_topic};
use crate::memory::traits::{
    DecisionMemoryHit, Memory, MemoryCategory, MemoryEntry, MemoryReadMode, MemoryStoreMetadata,
    validate_memory_write_target,
};
use crate::runtime::sqlite_pool::begin_immediate;
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

pub(super) struct PreparedEmbedding {
    pub(super) bytes: Vec<u8>,
    pub(super) hash: String,
}

enum WriteOutcome {
    Done,
    Retry,
}

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

/// `memories.ma_scope_id` qualified by the `m` alias of the FTS joins.
const MEMORY_ALIAS_SCOPE_COLUMN: &str = "m.ma_scope_id";

/// MA-01h: the partition conjunct on a `memories` column with bare `?`
/// markers, for statements whose scope predicate binds positionally.
fn memory_partition_conjunct(column: &str, partition: Option<&crate::memory::AgentPartition>) -> (String, Vec<Value>) {
    let (sql, values) = super::events::partition_conjunct(column, partition, || "?".to_string());
    (sql, values.into_iter().map(Value::from).collect())
}

/// The ids among `ids` whose `memories` row lies in `partition`.
fn partition_visible_memory_ids(
    conn: &Connection,
    ids: &[&str],
    partition: &crate::memory::AgentPartition,
) -> anyhow::Result<std::collections::HashSet<String>> {
    if ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let (partition_sql, partition_values) = memory_partition_conjunct(super::events::MA_SCOPE_COLUMN, Some(partition));
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT id FROM memories WHERE id IN ({placeholders}){partition_sql}");
    let mut values = ids.iter().map(|id| Value::from((*id).to_string())).collect::<Vec<_>>();
    values.extend(partition_values);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Keep only the entries whose row lies in `partition`; `None` keeps all.
fn retain_partition_entries(
    conn: &Connection,
    entries: Vec<MemoryEntry>,
    partition: Option<&crate::memory::AgentPartition>,
) -> anyhow::Result<Vec<MemoryEntry>> {
    let Some(partition) = partition else {
        return Ok(entries);
    };
    let ids = entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>();
    let visible = partition_visible_memory_ids(conn, &ids, partition)?;
    Ok(entries
        .into_iter()
        .filter(|entry| visible.contains(&entry.id))
        .collect())
}

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
        let mut conn = Connection::open_with_flags(
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
            let conn = pool.write();
            let tx = begin_immediate(&conn)?;
            crate::memory::sqlite::rf_forget::drop_prefix_rows_of(
                &tx,
                "SELECT DISTINCT workspace_id, owner_id FROM memories WHERE key LIKE ?1 ESCAPE '\\'",
                params![pattern],
            )?;
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
        crate::memory::owner_predicate::isolated_store_owner(context)?;
        self.check_memory_safety(content, context).await?;
        self.write_checked(key, content, category, session_id, context, metadata)
            .await
    }

    /// The session-snapshot write (`Memory::store_session_snapshot` and
    /// `Memory::store_session_snapshot_with_metadata`). The snapshot is the
    /// owner's transcript, not recall memory, so the memory content filter
    /// does not run. Target validation and the checked write stay.
    pub(super) async fn store_session_snapshot_impl(
        &self,
        key: &str,
        content: &str,
        session_id: Option<&str>,
        metadata: Option<MemoryStoreMetadata>,
    ) -> anyhow::Result<()> {
        validate_memory_write_target(key, session_id)?;
        let context = super::terminal_epoch::snapshot_context(&self.db_path, key, session_id)?;
        let linked = context.as_ref().and_then(|context| context.session_key.as_deref()).or(session_id);
        self.write_checked(key, content, MemoryCategory::Conversation, linked, context.as_ref(), metadata)
            .await
    }

    /// The write after the target and safety checks passed.
    async fn write_checked(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: Option<MemoryStoreMetadata>,
    ) -> anyhow::Result<()> {
        let isolated_owner = crate::memory::owner_predicate::isolated_store_owner(context)?;
        let partition = context.and_then(|ctx| ctx.agent_partition.clone());
        let session_key = context.and_then(|ctx| ctx.session_key.clone());
        let needs_embedding = matches!(&category, MemoryCategory::Core | MemoryCategory::Custom(_));
        for attempt in 0..2_u8 {
            let salt_scope = self
                .salt_scope_for_write(
                    partition.as_ref(),
                    session_key.as_deref(),
                    context.and_then(|ctx| ctx.workspace_id.as_deref()),
                    context
                        .and_then(|ctx| ctx.owner_id.as_deref())
                        .or(isolated_owner.as_deref()),
                )
                .await?;
            let prepared = if needs_embedding {
                self.embed_without_cache(content, salt_scope.as_deref()).await?
            } else {
                None
            };
            let embedding_bytes = prepared.as_ref().map(|item| item.bytes.clone());
            let embedding_provider = prepared.as_ref().map(|_| self.embedding_provider_name());
            let embedding_model = prepared.as_ref().map(|_| self.embedding_model_name());
            let embedding_dimensions = prepared.as_ref().map(|_| self.embedding_dimensions_i64());
            let cache_hash = prepared.as_ref().map(|item| item.hash.clone());

            let pool = self.pool.clone();
            let key = key.to_string();
            let content = content.to_string();
            let sid = session_id.map(String::from);
            let write_ctx = context.cloned();
            let mut metadata = metadata.clone().unwrap_or_default();
            if let Some(owner) = isolated_owner.as_ref() {
                metadata.owner_id = Some(owner.clone());
            }
            let owner_guard = crate::memory::owner_predicate::store_conflict_guard(
                SQLITE_DIALECT,
                "memories",
                isolated_owner.is_some(),
            );
            let expected_salt = salt_scope.clone();
            let cache_now = Local::now().to_rfc3339();
            let provider_name = self.embedding_provider_name();
            let model_name = self.embedding_model_name();
            let dimensions = self.embedding_dimensions_i64();
            let cache_max = i64::try_from(self.cache_max).unwrap_or(i64::MAX);
            let cache_bytes = embedding_bytes.clone();
            let isolated_owner = isolated_owner.clone();
            let category = category.clone();
            let provenance = self.write_provenance.clone();

            let outcome = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<WriteOutcome> {
                let conn = pool.write();
                let tx = begin_immediate(&conn)?;
                let existed = match provenance.as_deref() {
                    Some(_) => super::write_log::target_exists(&tx, super::write_log::WriteTarget::Memory, &key)?,
                    None => false,
                };
                let now = Local::now().to_rfc3339();
                let cat = Self::category_to_str(&category);
                let id = Uuid::new_v4().to_string();
                let bound_scope = if let Some(ctx) = write_ctx.as_ref() {
                    super::agent_migration::resolve_input_scope(
                        &tx,
                        ctx.agent_partition.as_ref(),
                        ctx.session_key.as_deref(),
                        ctx.workspace_id.as_deref().unwrap_or(""),
                        ctx.owner_id.as_deref(),
                    )?
                } else {
                    None
                };
                if expected_salt.is_some() && bound_scope != expected_salt {
                    return Ok(WriteOutcome::Retry);
                }

                let previous_topic_id: Option<String> = tx
                    .query_row("SELECT topic_id FROM memories WHERE key = ?1", params![&key], |row| row.get(0))
                    .optional()?
                    .flatten();

                if let Some(ctx) = write_ctx {
                    if let Some(owner) = isolated_owner.as_deref() {
                        let stored_owner: Option<Option<String>> = tx
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
                    let principal = sqlite_read_principal(&tx, &ctx);
                    let classified = classify_memory(&ctx, &content, &principal);
                    let topic_id = match resolve_topic(&tx, &content, &principal, bound_scope.as_deref()) {
                        Ok(topic_id) => topic_id,
                        Err(error) => {
                            tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "memory topic resolve failed");
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
                    let scope_guard = super::agent_migration::scope_conflict_guard("memories", !owner_guard.is_empty());
                    let coalesce = super::agent_migration::scope_coalesce_set("memories");

                    let changed = tx.execute(
                        &format!("INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, channel, chat_type, chat_id,
                        sender_id, raw_sender, topic_id, visibility, sensitivity,
                        risk_signals, policy_version, ma_scope_id
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
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
                        policy_version = excluded.policy_version,
                        {coalesce}{owner_guard}{scope_guard}"),
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
                            bound_scope,
                        ],
                    )?;
                    if changed == 0 {
                        return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                            key: key.clone(),
                        }
                        .into());
                    }
                    if let Some(previous) = previous_topic_id.as_deref()
                        && Some(previous) != topic_id.as_deref()
                    {
                        delete_orphan_topic(&tx, previous)?;
                    }
                } else {
                    let coalesce = super::agent_migration::scope_coalesce_set("memories");
                    let scope_guard = super::agent_migration::scope_conflict_guard("memories", false);
                    let changed = tx.execute(
                        &format!(
                            "INSERT INTO memories (
                        id, key, content, category, embedding, embedding_provider,
                        embedding_model, embedding_dimensions, created_at, updated_at,
                        session_id, workspace_id, owner_id, agent_id, persona_id,
                        source_event_id, source, channel, topic_id, ma_scope_id
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
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
                        topic_id = excluded.topic_id,
                        {coalesce}{scope_guard}"
                        ),
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
                            metadata.channel.clone(),
                            metadata.topic_id.clone(),
                            bound_scope,
                        ],
                    )?;
                    if changed == 0 {
                        return Err(crate::memory::owner_predicate::OwnerScopedStoreRefusal::KeyNotAvailable {
                            key: key.clone(),
                        }
                        .into());
                    }
                    if let Some(previous) = previous_topic_id.as_deref()
                        && Some(previous) != metadata.topic_id.as_deref()
                    {
                        delete_orphan_topic(&tx, previous)?;
                    }
                }

                if metadata.workspace_id.is_some()
                    || metadata.owner_id.is_some()
                    || metadata.agent_id.is_some()
                    || metadata.persona_id.is_some()
                    || metadata.source_event_id.is_some()
                    || metadata.source.is_some()
                {
                    let metadata_owner_guard = if isolated_owner.is_some() {
                        " AND owner_id IS ?2"
                    } else {
                        ""
                    };
                    let updated = tx.execute(
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
                if let (Some(hash), Some(bytes)) = (cache_hash, cache_bytes) {
                    Self::insert_embedding_cache(
                        &tx,
                        &hash,
                        &bytes,
                        &provider_name,
                        &model_name,
                        dimensions,
                        &cache_now,
                        bound_scope.as_deref(),
                        cache_max,
                    )?;
                }
                if let Some(provenance) = provenance.as_deref() {
                    super::write_log::record_memory_upsert(&tx, provenance, &key, existed)?;
                }
                tx.commit()?;
                Ok(WriteOutcome::Done)
            })
            .await??;
            match outcome {
                WriteOutcome::Done => break,
                WriteOutcome::Retry if attempt == 0 => continue,
                WriteOutcome::Retry => return Err(crate::memory::AgentScopeRefused.into()),
            }
        }
        Ok(())
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
        for byte in hash.iter().take(16) {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// SHA-256("ma:" + scope_id + NUL + text) as the partitioned cache key.
    pub(super) fn scoped_content_hash(scope_id: Option<&str>, text: &str) -> String {
        use sha2::{Digest, Sha256};
        let Some(scope_id) = scope_id else {
            return Self::content_hash(text);
        };
        let mut hasher = Sha256::new();
        hasher.update(b"ma:");
        hasher.update(scope_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(text.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub(super) async fn salt_scope_for_write(
        &self,
        partition: Option<&crate::memory::AgentPartition>,
        session_key: Option<&str>,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let Some(partition) = partition.cloned() else {
            return Ok(None);
        };
        if partition.workspace_id != workspace_id.unwrap_or("") || owner_id != Some(partition.owner_id.as_str()) {
            return Err(crate::memory::AgentScopeRefused.into());
        }
        let session_key = session_key.map(str::to_owned);
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            let conn = pool.write();
            super::agent_migration::lookup_or_mint_write_scope(&conn, &partition, session_key.as_deref()).map(Some)
        })
        .await?
    }

    pub(super) async fn embed_without_cache(
        &self,
        text: &str,
        scope_id: Option<&str>,
    ) -> anyhow::Result<Option<PreparedEmbedding>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None);
        }
        let hash = Self::scoped_content_hash(scope_id, text);
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let pool = self.pool.clone();
        let hash_c = hash.clone();
        let cached = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<Vec<f32>>> {
            let conn = pool.write();
            Self::lookup_embedding_cache(&conn, &hash_c, &provider_name, &model_name, dimensions)
        })
        .await??;
        let embedding = if let Some(embedding) = cached {
            embedding
        } else {
            self.embedder.embed_one(text).await?
        };
        if embedding.len() != self.embedder.dimensions() {
            anyhow::bail!(
                "embedding dimension mismatch: provider={} model={} expected={} got={}",
                self.embedding_provider_name(),
                self.embedding_model_name(),
                self.embedder.dimensions(),
                embedding.len()
            );
        }
        Ok(Some(PreparedEmbedding {
            bytes: vector::vec_to_bytes(&embedding),
            hash,
        }))
    }

    fn lookup_embedding_cache(
        conn: &Connection,
        hash: &str,
        provider: &str,
        model: &str,
        dimensions: i64,
    ) -> anyhow::Result<Option<Vec<f32>>> {
        let mut stmt = conn.prepare(
            "SELECT embedding FROM embedding_cache
                 WHERE content_hash = ?1
                   AND provider = ?2
                   AND model = ?3
                   AND dimensions = ?4",
        )?;
        let blob: Option<Vec<u8>> = stmt
            .query_row(params![hash, provider, model, dimensions], |row| row.get(0))
            .ok();
        let Some(bytes) = blob else {
            return Ok(None);
        };
        let embedding = vector::bytes_to_vec(&bytes);
        if embedding.len() == usize::try_from(dimensions).unwrap_or(usize::MAX) {
            Ok(Some(embedding))
        } else {
            Ok(None)
        }
    }

    pub(super) fn insert_embedding_cache(
        conn: &Connection,
        hash: &str,
        bytes: &[u8],
        provider: &str,
        model: &str,
        dimensions: i64,
        now: &str,
        scope_id: Option<&str>,
        cache_max: i64,
    ) -> anyhow::Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO embedding_cache (
                    content_hash, embedding, provider, model, dimensions, created_at, accessed_at, ma_scope_id
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![hash, bytes, provider, model, dimensions, now, now, scope_id],
        )?;
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

    /// Get embedding from cache, or compute + cache it (unpartitioned hash).
    pub(super) async fn get_or_compute_embedding(&self, text: &str) -> anyhow::Result<Option<Vec<f32>>> {
        self.embed_and_cache(text, None).await
    }

    async fn embed_and_cache(&self, text: &str, scope_id: Option<&str>) -> anyhow::Result<Option<Vec<f32>>> {
        let Some(prepared) = self.embed_without_cache(text, scope_id).await? else {
            return Ok(None);
        };
        let embedding = vector::bytes_to_vec(&prepared.bytes);
        let pool = self.pool.clone();
        let now = Local::now().to_rfc3339();
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_max = i64::try_from(self.cache_max).unwrap_or(i64::MAX);
        let scope = scope_id.map(str::to_owned);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            Self::insert_embedding_cache(
                &conn,
                &prepared.hash,
                &prepared.bytes,
                &provider_name,
                &model_name,
                dimensions,
                &now,
                scope.as_deref(),
                cache_max,
            )
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
                    memory_id = %crate::agent::diagnostics::text_class(&id),
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

}

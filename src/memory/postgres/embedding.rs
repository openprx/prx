//! Embedding cache lookups and pgvector literal helpers.
use super::PostgresMemory;
use crate::memory::{traits::MemoryCategory, vector};
use anyhow::{Context, Result};
use chrono::Utc;

impl PostgresMemory {
    /// Render an embedding in pgvector's text input form (`[a,b,c]`).
    ///
    /// PG-VECTOR: the result is bound as a Rust `String`, so every statement
    /// that receives it casts the parameter to `text` first and only then to
    /// `vector`. A parameter cast straight to `vector` makes the server type
    /// the parameter as `vector`, which a `String` cannot bind to, and the
    /// statement fails before it runs. `tests/pgvector_binding_regression.rs`
    /// guards every such cast.
    pub(super) fn pgvector_literal(embedding: &[f32]) -> Option<String> {
        if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
            return None;
        }
        Some(format!(
            "[{}]",
            embedding
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }

    const fn memory_category_needs_embedding(category: &MemoryCategory) -> bool {
        matches!(category, MemoryCategory::Core | MemoryCategory::Custom(_))
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

    pub(super) async fn get_or_compute_embedding(
        &self,
        text: &str,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        system: bool,
    ) -> Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None);
        }
        let (scope_workspace, scope_owner) = if system {
            ("__prx_system__".to_string(), "__prx_system__".to_string())
        } else {
            (
                workspace_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("PostgreSQL embedding cache requires workspace context")?
                    .to_string(),
                owner_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("PostgreSQL embedding cache requires owner context")?
                    .to_string(),
            )
        };
        let content_hash = Self::content_hash(text);
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_table = self.qualified_embedding_cache_table.clone();
        let client = self.client.clone();
        let cache_hash = content_hash.clone();
        let cache_provider = provider_name.clone();
        let cache_model = model_name.clone();
        let select_workspace = scope_workspace.clone();
        let select_owner = scope_owner.clone();
        let cached = tokio::task::spawn_blocking(move || -> Result<Option<Vec<f32>>> {
            let select_stmt = format!(
                "
                SELECT embedding
                FROM {cache_table}
                WHERE workspace_id = $1
                  AND owner_id = $2
                  AND content_hash = $3
                  AND provider = $4
                  AND model = $5
                  AND dimensions = $6
                LIMIT 1
                "
            );
            let update_stmt = format!(
                "
                UPDATE {cache_table}
                SET accessed_at = $7
                WHERE workspace_id = $1
                  AND owner_id = $2
                  AND content_hash = $3
                  AND provider = $4
                  AND model = $5
                  AND dimensions = $6
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The cache policy is owner exact; it has no shared arm to narrow.
                Self::apply_rls_context_raw(&mut tx, system, Some(&select_workspace), Some(&select_owner), false)?;
                let row = tx.query_opt(
                    &select_stmt,
                    &[
                        &select_workspace,
                        &select_owner,
                        &cache_hash,
                        &cache_provider,
                        &cache_model,
                        &dimensions,
                    ],
                )?;
                let Some(row) = row else {
                    tx.commit()?;
                    return Ok(None);
                };
                let bytes: Vec<u8> = row.get(0);
                let embedding = vector::bytes_to_vec(&bytes);
                if embedding.len() != usize::try_from(dimensions).unwrap_or(usize::MAX) {
                    tx.commit()?;
                    return Ok(None);
                }
                let now = Utc::now();
                tx.execute(
                    &update_stmt,
                    &[
                        &select_workspace,
                        &select_owner,
                        &cache_hash,
                        &cache_provider,
                        &cache_model,
                        &dimensions,
                        &now,
                    ],
                )?;
                tx.commit()?;
                Ok(Some(embedding))
            })
        })
        .await??;
        if cached.is_some() {
            return Ok(cached);
        }

        let embedding = self.embedder.embed_one(text).await?;
        if embedding.len() != self.embedder.dimensions() {
            anyhow::bail!(
                "embedding dimension mismatch: provider={} model={} expected={} got={}",
                self.embedder.name(),
                self.embedder.model(),
                self.embedder.dimensions(),
                embedding.len()
            );
        }
        let cache_table = self.qualified_embedding_cache_table.clone();
        let client = self.client.clone();
        let provider_name = self.embedding_provider_name();
        let model_name = self.embedding_model_name();
        let dimensions = self.embedding_dimensions_i64();
        let cache_max = self.embedding_cache_max_rows;
        let bytes = vector::vec_to_bytes(&embedding);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let insert_stmt = format!(
                "
                INSERT INTO {cache_table} (
                    workspace_id, owner_id, content_hash, embedding, provider, model,
                    dimensions, created_at, accessed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
                ON CONFLICT (workspace_id, owner_id, content_hash, provider, model, dimensions)
                DO UPDATE SET embedding = EXCLUDED.embedding, accessed_at = EXCLUDED.accessed_at
                "
            );
            let evict_stmt = format!(
                "
                DELETE FROM {cache_table}
                WHERE workspace_id = $1 AND owner_id = $2
                  AND (content_hash, provider, model, dimensions) IN (
                    SELECT content_hash, provider, model, dimensions
                    FROM {cache_table}
                    WHERE workspace_id = $1 AND owner_id = $2
                    ORDER BY accessed_at ASC
                    LIMIT GREATEST(
                        (SELECT COUNT(*) FROM {cache_table}
                         WHERE workspace_id = $1 AND owner_id = $2) - $3,
                        0
                    )
                )
                "
            );
            client.with_client(|client| {
                let mut tx = client.transaction()?;
                // The cache policy is owner exact; it has no shared arm to narrow.
                Self::apply_rls_context_raw(&mut tx, system, Some(&scope_workspace), Some(&scope_owner), false)?;
                let now = Utc::now();
                tx.execute(
                    &insert_stmt,
                    &[
                        &scope_workspace,
                        &scope_owner,
                        &content_hash,
                        &bytes,
                        &provider_name,
                        &model_name,
                        &dimensions,
                        &now,
                    ],
                )?;
                tx.execute(&evict_stmt, &[&scope_workspace, &scope_owner, &cache_max])?;
                tx.commit()?;
                Ok(())
            })
        })
        .await??;
        Ok(Some(embedding))
    }

    pub(super) async fn embedding_metadata_for_category(
        &self,
        category: &MemoryCategory,
        content: &str,
        workspace_id: Option<&str>,
        owner_id: Option<&str>,
        system: bool,
    ) -> Result<(
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    )> {
        if !Self::memory_category_needs_embedding(category) {
            return Ok((None, None, None, None, None));
        }
        let Some(embedding) = self
            .get_or_compute_embedding(content, workspace_id, owner_id, system)
            .await?
        else {
            return Ok((None, None, None, None, None));
        };
        let pgvector_literal = self
            .pgvector_available
            .then(|| Self::pgvector_literal(&embedding))
            .flatten();
        Ok((
            Some(vector::vec_to_bytes(&embedding)),
            Some(self.embedding_provider_name()),
            Some(self.embedding_model_name()),
            Some(self.embedding_dimensions_i64()),
            pgvector_literal,
        ))
    }
}

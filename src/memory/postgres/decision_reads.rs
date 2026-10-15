//! Bounded, read-only decision IO. Never leases the ordinary blocking pool.
use super::super::principal::{ChatType, MemoryWriteContext, Principal, Role};
use super::super::traits::MemoryPrincipal;
use super::super::traits::{DecisionDocumentHit, DecisionMemoryHit, DecisionSessionHit};
use super::{PG_DIALECT, PostgresMemory, scope_params_to_strings};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::future::Future;
use std::time::Instant;

impl PostgresMemory {
    /// Own both the protocol driver and read future. Dropping this future drops
    /// the connection rather than leaving a detached blocking worker behind.
    /// Server-side cancellation latency still requires real PostgreSQL evidence.
    async fn decision_read<T, F, Fut>(&self, deadline: Instant, read: F) -> Result<T>
    where
        F: FnOnce(tokio_postgres::Client) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .context("decision read deadline expired")?;
        let mut config: tokio_postgres::Config = self
            .client
            .db_url
            .parse()
            .context("invalid PostgreSQL decision connection configuration")?;
        config.connect_timeout(remaining);
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128);
        let options = format!(
            "{} -c default_transaction_read_only=on -c statement_timeout={timeout_ms} -c lock_timeout={timeout_ms}",
            config.get_options().unwrap_or_default()
        );
        config.options(&options);
        let operation = async {
            let (client, connection) = config
                .connect(tokio_postgres::NoTls)
                .await
                .context("PostgreSQL decision connection failed")?;
            tokio::pin!(connection);
            tokio::select! {
                result = async {
                    Self::decision_timeout(&client, deadline).await?;
                    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").await?;
                    read(client).await
                } => {
                    let result = result?;
                    anyhow::ensure!(Instant::now() < deadline, "decision read deadline expired");
                    Ok(result)
                },
                result = &mut connection => {
                    result.context("PostgreSQL decision protocol failed")?;
                    anyhow::bail!("PostgreSQL decision connection closed before read completed")
                }
            }
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .context("decision read deadline expired")?
    }

    /// Refresh before every IO phase; zero means unlimited to PostgreSQL.
    async fn decision_timeout(client: &tokio_postgres::Client, deadline: Instant) -> Result<()> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .context("decision read deadline expired")?;
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128).to_string();
        client
            .query_one(
                "SELECT set_config('statement_timeout', $1, false), set_config('lock_timeout', $1, false)",
                &[&millis],
            )
            .await?;
        anyhow::ensure!(Instant::now() < deadline, "decision read deadline expired");
        Ok(())
    }

    async fn decision_scope(
        client: &tokio_postgres::Client,
        system: bool,
        workspace: Option<&str>,
        owner: Option<&str>,
        isolated: bool,
        deadline: Instant,
    ) -> Result<()> {
        Self::decision_timeout(client, deadline).await?;
        if system {
            client
                .batch_execute("SELECT set_config('prx.rls_bypass', 'on', false)")
                .await?;
        } else {
            let workspace = workspace
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("decision read requires workspace")?;
            let owner = owner
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("decision read requires owner")?;
            let isolated = if isolated { "on" } else { "off" };
            client
                .query_one(
                    "SELECT set_config('prx.rls_bypass', 'off', false), \
                set_config('prx.current_workspace', $1, false), \
                set_config('prx.current_owner', $2, false), \
                set_config('prx.owner_isolated', $3, false)",
                    &[&workspace, &owner, &isolated],
                )
                .await?;
        }
        Ok(())
    }

    pub(super) async fn count_decision_rehearsal(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: Instant,
    ) -> Result<u32> {
        anyhow::ensure!(
            query.len() <= 1024 && (1..=16).contains(&limit),
            "invalid rehearsal count bounds"
        );
        let hits = self
            .decision_memory_rows(query, limit, context, None, deadline, true)
            .await?;
        Ok(u32::try_from(hits.len())?)
    }

    pub(super) async fn read_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> Result<Vec<DecisionMemoryHit>> {
        anyhow::ensure!(
            query.len() <= 1024 && (1..=5).contains(&limit),
            "invalid decision Memory bounds"
        );
        self.decision_memory_rows(query, limit, context, session_id, deadline, false)
            .await
    }

    async fn decision_memory_rows(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: Instant,
        count_only: bool,
    ) -> Result<Vec<DecisionMemoryHit>> {
        anyhow::ensure!(Instant::now() < deadline, "decision Memory deadline expired");
        anyhow::ensure!(
            session_id.is_none_or(|id| !id.is_empty() && id.len() <= 512),
            "invalid decision Memory session"
        );
        if query.trim().is_empty() || Self::isolated_read_without_owner(context) {
            return Ok(Vec::new());
        }
        self.decision_read(deadline, |client| async move {
            let principal = self.decision_principal(&client, context, deadline).await?;
            let fallback = context.sender_id.clone().unwrap_or_else(|| format!("anonymous:{}:{}",
                context.channel.as_deref().unwrap_or("unknown"), context.raw_sender.as_deref().unwrap_or("unknown")));
            let owner = if context.require_owner_match { context.owner_id.as_deref() } else { Some(fallback.as_str()) };
            Self::decision_scope(&client, super::super::principal::is_system_principal(&principal.user_id),
                context.workspace_id.as_deref(), owner, context.require_owner_match, deadline).await?;
            let (scope_sql, scope_params) = principal.build_sql_scope_pg(5);
            let scope_values = scope_params_to_strings(scope_params);
            let cap = i64::from(limit) * 3;
            let workspace = context.workspace_id.as_deref().map(str::trim);
            let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&query, &cap, &workspace, &session_id];
            for value in &scope_values { params.push(value); }
            // Count only inspects content, so metadata sizes must not silently
            // remove otherwise eligible rehearsal hits.
            let (projection, metadata_guard) = if count_only {
                ("''::TEXT, content, created_at, NULL::TEXT, ''::TEXT, ''::TEXT, NULL::TEXT", "")
            } else {
                ("id, content, created_at, source_event_id, key, category, session_id",
                    "AND octet_length(id) <= 128 AND (source_event_id IS NULL OR octet_length(source_event_id) <= 128) AND octet_length(key) <= 1024 AND octet_length(category) <= 128 AND (session_id IS NULL OR octet_length(session_id) <= 512)")
            };
            let sql = format!("SELECT {projection} FROM {} WHERE ({scope_sql}) \
                AND ($3::TEXT IS NULL OR workspace_id = $3) AND ($4::TEXT IS NULL OR session_id = $4) \
                AND octet_length(content) <= 4096 {metadata_guard} \
                AND to_tsvector('simple', key || ' ' || content) @@ plainto_tsquery('simple', $1) \
                ORDER BY ts_rank_cd(to_tsvector('simple', key || ' ' || content), plainto_tsquery('simple', $1)) DESC, id LIMIT $2", self.qualified_table);
            Self::decision_timeout(&client, deadline).await?;
            let rows = client.query(&sql, &params).await?;
            let candidates = rows.iter().map(|row| -> Result<DecisionMemoryHit> {
                Ok(DecisionMemoryHit { id: row.try_get(0)?, content: row.try_get(1)?,
                    created_at: row.try_get::<_, DateTime<Utc>>(2)?.to_rfc3339(), source_event_id: row.try_get(3)?, key: row.try_get(4)?,
                    category: Self::parse_category(&row.try_get::<_, String>(5)?), session_id: row.try_get(6)? })
            }).collect::<Result<Vec<_>>>()?;
            let mut visible = super::super::principal::post_filter(candidates, &principal, |hit| hit.content.as_str());
            visible.truncate(usize::from(limit));
            Ok(visible)
        }).await
    }

    pub(super) async fn read_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: Instant,
    ) -> Result<Vec<DecisionDocumentHit>> {
        anyhow::ensure!(
            query.len() <= 1024 && (1..=5).contains(&limit),
            "invalid decision Document bounds"
        );
        anyhow::ensure!(Instant::now() < deadline, "decision Document deadline expired");
        let owner = if principal.require_owner_match {
            Self::document_owner_for_principal(principal)
        } else {
            principal
                .owner_id
                .clone()
                .or_else(|| Self::document_owner_for_principal(principal))
        };
        if query.trim().is_empty() || (principal.require_owner_match && owner.is_none()) {
            return Ok(Vec::new());
        }
        self.decision_read(deadline, |client| async move {
            Self::decision_scope(&client, Self::is_system_principal(principal), Some(&principal.workspace_id),
                owner.as_deref(), principal.require_owner_match, deadline).await?;
            let guard = Self::document_visibility_guard(principal);
            let sql = format!("SELECT c.chunk_id, c.document_id, c.content, c.created_at \
                FROM {} c JOIN {} d ON d.document_id = c.document_id \
                WHERE c.workspace_id = $2 AND d.workspace_id = $2 \
                AND (d.visibility IN ('global', 'workspace'){guard} OR ($3::TEXT IS NOT NULL AND c.owner_id = $3)) \
                AND octet_length(c.chunk_id) <= 128 AND octet_length(c.document_id) <= 128 AND octet_length(c.content) <= 4096 \
                AND to_tsvector('simple', c.content) @@ plainto_tsquery('simple', $1) \
                ORDER BY ts_rank_cd(to_tsvector('simple', c.content), plainto_tsquery('simple', $1)) DESC, c.chunk_id LIMIT $4",
                self.qualified_document_chunks_table, self.qualified_documents_table);
            Self::decision_timeout(&client, deadline).await?;
            let rows = client.query(&sql, &[&query, &principal.workspace_id, &owner, &i64::from(limit)]).await?;
            rows.iter().map(|row| Ok(DecisionDocumentHit {
                chunk_id: row.try_get(0)?, document_id: row.try_get(1)?, content: row.try_get(2)?,
                created_at: row.try_get::<_, DateTime<Utc>>(3)?.to_rfc3339(),
            })).collect()
        }).await
    }

    pub(super) async fn read_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: Instant,
    ) -> Result<Vec<DecisionSessionHit>> {
        anyhow::ensure!(
            (1..=5).contains(&limit) && !before_event_id.is_empty() && before_event_id.len() <= 128,
            "invalid decision Session bounds"
        );
        anyhow::ensure!(Instant::now() < deadline, "decision Session deadline expired");
        let session = principal
            .session_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .context("selected Session requires a session")?;
        anyhow::ensure!(
            crate::memory::owner_predicate::has_owner_id(principal),
            "selected Session requires an authenticated owner"
        );
        self.decision_read(deadline, |client| async move {
            Self::decision_scope(&client, Self::is_system_principal(principal), Some(&principal.workspace_id),
                principal.owner_id.as_deref(), principal.require_owner_match, deadline).await?;
            let source_sql = format!("SELECT id FROM {} WHERE event_id = $1 AND workspace_id = $2 \
                AND session_key = $3 AND owner_id = $4 AND role = 'user' AND event_type = 'message.created'", self.qualified_message_events_table);
            Self::decision_timeout(&client, deadline).await?;
            let source = client.query_opt(&source_sql, &[&before_event_id, &principal.workspace_id, &session, &principal.owner_id]).await?
                .context("selected Session source event is unavailable")?;
            let before: i64 = source.try_get(0)?;
            let legacy = Self::legacy_session_key_params(principal);
            let mut indices = vec![3];
            indices.extend((0..legacy.len()).map(|offset| 9 + offset));
            let session_fragment = crate::memory::session_predicate::session_key_match_fragment(PG_DIALECT, &indices);
            let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
                PG_DIALECT, Self::owner_index(9, legacy.len()), principal.require_owner_match, true);
            let private_fragment = crate::memory::owner_predicate::owner_match_suffix(
                PG_DIALECT, Self::owner_index(9, legacy.len()), true, true);
            let shared = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);
            let sql = format!("SELECT event_id, role, content, created_at FROM {} \
                WHERE id < $1 AND workspace_id = $2 AND {} AND ({} \
                    OR (visibility = 'agent' AND (($4::TEXT IS NOT NULL AND agent_id = $4) OR ($5::TEXT IS NOT NULL AND persona_id = $5))) \
                    OR (visibility = 'session'{}) \
                    OR (visibility = 'private' AND (($4::TEXT IS NOT NULL AND agent_id = $4) OR ($5::TEXT IS NOT NULL AND persona_id = $5) \
                        OR ($6::TEXT IS NOT NULL AND sender = $6)){}) \
                    OR (visibility = 'system' AND $7::BOOLEAN)) \
                AND role IN ('user', 'assistant') AND event_type = 'message.created' \
                AND octet_length(event_id) <= 128 AND octet_length(content) <= 4096 \
                ORDER BY id DESC LIMIT $8", self.qualified_message_events_table, session_fragment.sql,
                shared, owner_fragment.sql, private_fragment.sql);
            let system = Self::is_system_principal(principal);
            let cap = i64::from(limit);
            let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&before, &principal.workspace_id,
                &session, &principal.agent_id, &principal.persona_id, &principal.sender, &system, &cap];
            for key in &legacy { params.push(key); }
            if private_fragment.bound_owner { params.push(&principal.owner_id); }
            Self::decision_timeout(&client, deadline).await?;
            let rows = client.query(&sql, &params).await?;
            let mut hits = rows.iter().map(|row| -> Result<DecisionSessionHit> {
                Ok(DecisionSessionHit { event_id: row.try_get(0)?, role: row.try_get(1)?, content: row.try_get(2)?,
                    created_at: row.try_get::<_, DateTime<Utc>>(3)?.to_rfc3339() })
            }).collect::<Result<Vec<_>>>()?;
            hits.reverse();
            Ok(hits)
        }).await
    }

    async fn decision_principal(
        &self,
        client: &tokio_postgres::Client,
        context: &MemoryWriteContext,
        deadline: Instant,
    ) -> Result<Principal> {
        let channel = context.channel.clone().unwrap_or_default();
        let raw_sender = context.raw_sender.clone().unwrap_or_default();
        let mut user_id = format!(
            "anonymous:{}:{}",
            context.channel.as_deref().unwrap_or("unknown"),
            context.raw_sender.as_deref().unwrap_or("unknown")
        );
        let mut policy: Option<(String, String, String, String)> = None;
        let mut bound_identity = false;
        if context.channel.is_some() && context.raw_sender.is_some() {
            let sql = format!(
                "SELECT CASE WHEN octet_length(user_id) <= 1024 THEN user_id END FROM {} WHERE channel = $1 AND channel_account = $2 LIMIT 1",
                self.qualified_identity_bindings_table()
            );
            Self::decision_timeout(client, deadline).await?;
            if let Some(row) = client.query_opt(&sql, &[&channel, &raw_sender]).await? {
                bound_identity = true;
                user_id = row.try_get(0)?;
                let sql = format!(
                    "SELECT CASE WHEN octet_length(role) <= 64 THEN role END, CASE WHEN octet_length(projects) <= 16384 THEN projects END, CASE WHEN octet_length(visibility_ceiling) <= 64 THEN visibility_ceiling END, CASE WHEN octet_length(blocked_patterns) <= 16384 THEN blocked_patterns END FROM {} WHERE user_id = $1 LIMIT 1",
                    self.qualified_user_policies_table()
                );
                Self::decision_timeout(client, deadline).await?;
                policy = client
                    .query_opt(&sql, &[&user_id])
                    .await?
                    .map(|row| -> Result<_> {
                        Ok((row.try_get(0)?, row.try_get(1)?, row.try_get(2)?, row.try_get(3)?))
                    })
                    .transpose()?;
            }
        }
        // Malformed/oversized policy data cannot silently discard ACL
        // restrictions. Bound wire bytes, collection counts and regex compiled
        // size before applying the same principal scope and content filter.
        let mut blocked_patterns = Vec::new();
        if let Some((_, projects, _, blocked)) = &policy {
            let projects: Vec<String> = serde_json::from_str(projects).context("invalid decision project policy")?;
            anyhow::ensure!(projects.len() <= 64, "decision project policy exceeds bounds");
            let patterns: Vec<String> = serde_json::from_str(blocked).context("invalid decision blocked policy")?;
            anyhow::ensure!(patterns.len() <= 64, "decision blocked policy exceeds bounds");
            for pattern in patterns {
                blocked_patterns.push(
                    regex::RegexBuilder::new(&pattern)
                        .size_limit(64 * 1024)
                        .build()
                        .context("invalid or oversized decision blocked pattern")?,
                );
                anyhow::ensure!(Instant::now() < deadline, "decision policy deadline expired");
            }
        }
        // Avoid a second unbounded compilation in principal_from_policy.
        let policy = policy.map(|(role, projects, ceiling, _)| (role, projects, ceiling, "[]".to_owned()));
        let mut principal = super::super::principal::principal_from_policy(
            user_id,
            policy,
            channel,
            context.chat_id.clone().unwrap_or_default(),
            context
                .chat_type
                .as_deref()
                .map(ChatType::from_str)
                .unwrap_or(ChatType::Dm),
            raw_sender,
        )
        .with_owner_scope(context);
        principal.blocked_patterns = blocked_patterns;
        if !bound_identity {
            principal.role = Role::Anonymous;
            principal.acl_enforced = true;
        }
        Ok(principal)
    }
}

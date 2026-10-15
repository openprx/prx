//! Schema creation, checksummed migrations, RLS policy SQL and read-only isolation evidence.
use super::pool::POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS;
use super::rows::{quote_identifier, related_table_name, validate_identifier};
use super::{POSTGRES_CONNECT_TIMEOUT_CAP_SECS, PostgresMemory};
use anyhow::{Context, Result};
use chrono::Utc;
use postgres::{Client, NoTls};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// D-028: the read-only policy that shares `public` memory rows across owners.
pub(super) const MEMORIES_PUBLIC_READ_POLICY: &str = "memories_public_read";

/// Migrations the isolation evidence verifies as recorded with the checksum
/// this build carries: 21 (owner-isolation policies), 23 (their shared read
/// arm closed to owner-isolated callers), 24 (`memories_public_read`).
const VERIFIED_ISOLATION_MIGRATIONS: [i64; 3] = [21, 23, 24];

/// One `pg_policies` row, as the isolation evidence reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PolicyRow {
    pub(super) name: String,
    /// `PERMISSIVE` or `RESTRICTIVE`.
    pub(super) permissive: String,
    pub(super) roles: Vec<String>,
}

/// A vector table must carry exactly the `expected` policies, each
/// permissive and granted to `PUBLIC`, as `init_vector_rls` installs them.
///
/// Any other policy fails the evidence: a permissive one widens access by
/// OR, and a restrictive or role-limited variant of an expected one breaks
/// the reads it exists to grant.
pub(super) fn check_policy_inventory(target: &str, rows: &[PolicyRow], expected: &[&str]) -> Result<()> {
    let mut found = rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>();
    found.sort_unstable();
    let mut wanted = expected.to_vec();
    wanted.sort_unstable();
    anyhow::ensure!(
        found == wanted,
        "A04 table {target} carries policies {found:?}; expected exactly {wanted:?}"
    );
    for row in rows {
        anyhow::ensure!(
            row.permissive == "PERMISSIVE" && row.roles == ["public"],
            "A04 policy {} on {target} is not a permissive policy for PUBLIC",
            row.name
        );
    }
    Ok(())
}

impl PostgresMemory {
    /// Read-only proof that the A04 vector isolation migration and policies are
    /// active on the configured PostgreSQL deployment. The returned reference
    /// contains only migration/policy hashes, never credentials or row data.
    pub fn verify_vector_isolation(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
    ) -> Result<String> {
        let db_url = db_url.to_string();
        let schema = schema.to_string();
        let table = table.to_string();
        std::thread::Builder::new()
            .name("postgres-vector-evidence".to_string())
            .spawn(move || Self::verify_vector_isolation_blocking(&db_url, &schema, &table, connect_timeout_secs))
            .context("failed to spawn PostgreSQL evidence verifier thread")?
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL evidence verifier thread panicked"))?
    }

    fn verify_vector_isolation_blocking(
        db_url: &str,
        schema: &str,
        table: &str,
        connect_timeout_secs: Option<u64>,
    ) -> Result<String> {
        validate_identifier(schema, "schema")?;
        validate_identifier(table, "table")?;
        let chunks = related_table_name(table, "_document_chunks")?;
        let cache = related_table_name(table, "_embedding_cache")?;
        let mut pg_config: postgres::Config = db_url.parse().context("invalid PostgreSQL connection URL")?;
        // A stalled handshake is a network fault, not a long-running task, so it
        // stays bounded even when the operator configures nothing. See
        // `POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS`.
        pg_config.connect_timeout(Duration::from_secs(
            connect_timeout_secs
                .unwrap_or(POSTGRES_DEFAULT_CONNECT_TIMEOUT_SECS)
                .clamp(1, POSTGRES_CONNECT_TIMEOUT_CAP_SECS),
        ));
        let mut client = pg_config
            .connect(NoTls)
            .context("PostgreSQL isolation evidence connection failed")?;
        let migration_table = format!("{}.memory_schema_migrations", quote_identifier(schema));
        // 21 installed the owner-isolation policies, 23 closed their shared
        // read arm to owner-isolated callers, 24 added the public read grant:
        // the evidence names all three.
        let mut migration_proof = String::from("postgres");
        for wanted in VERIFIED_ISOLATION_MIGRATIONS {
            let (version, _, descriptor) = Self::memory_schema_migration_registry()
                .iter()
                .find(|(version, _, _)| *version == wanted)
                .with_context(|| format!("A04 schema migration {wanted} is not registered"))?;
            let expected_checksum = Self::schema_migration_checksum(descriptor);
            let row = client
                .query_opt(
                    &format!("SELECT checksum FROM {migration_table} WHERE version = $1"),
                    &[version],
                )?
                .with_context(|| format!("A04 schema migration {wanted} is not recorded"))?;
            let recorded_checksum: String = row.get(0);
            anyhow::ensure!(
                recorded_checksum == expected_checksum,
                "A04 schema migration {wanted} checksum does not match the runtime evaluator"
            );
            migration_proof = format!("{migration_proof}:migration:{version}:sha256:{expected_checksum}");
        }

        let targets = [
            (table, "memories_owner_isolation"),
            (chunks.as_str(), "document_chunks_owner_isolation"),
            (cache.as_str(), "embedding_cache_owner_isolation"),
        ];
        let mut policy_proof = String::new();
        for (target, policy) in targets {
            let qualified = format!("{}.{}", quote_identifier(schema), quote_identifier(target));
            let flags = client
                .query_opt(
                    "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = to_regclass($1::text)",
                    &[&qualified],
                )?
                .with_context(|| format!("A04 vector table {target} is missing"))?;
            anyhow::ensure!(
                flags.get::<_, bool>(0) && flags.get::<_, bool>(1),
                "A04 RLS is not enabled and forced for {target}"
            );
            // Enumerate, not just look up: a stray permissive policy — added by
            // hand, or left behind by another build — ORs its own access in
            // while every named policy still checks out (R-B2 F-5).
            let inventory = client
                .query(
                    "SELECT policyname::text, permissive, roles::text[] \
                     FROM pg_policies WHERE schemaname = $1 AND tablename = $2",
                    &[&schema, &target],
                )?
                .iter()
                .map(|row| PolicyRow {
                    name: row.get(0),
                    permissive: row.get(1),
                    roles: row.get(2),
                })
                .collect::<Vec<_>>();
            let expected: &[&str] = if target == table {
                &[policy, MEMORIES_PUBLIC_READ_POLICY]
            } else {
                &[policy]
            };
            check_policy_inventory(target, &inventory, expected)?;
            let policy_row = client
                .query_opt(
                    "SELECT COALESCE(qual, ''), COALESCE(with_check, '') \
                     FROM pg_policies \
                     WHERE schemaname = $1 AND tablename = $2 AND policyname = $3",
                    &[&schema, &target, &policy],
                )?
                .with_context(|| format!("A04 policy {policy} is missing"))?;
            let using_expr: String = policy_row.get(0);
            let check_expr: String = policy_row.get(1);
            for required in ["prx.current_workspace", "prx.current_owner"] {
                anyhow::ensure!(
                    using_expr.contains(required) && check_expr.contains(required),
                    "A04 policy {policy} lacks required transaction context"
                );
            }
            // OQ-8: a shared read arm must be closed to owner-isolated callers.
            anyhow::ensure!(
                !using_expr.contains("'workspace'") || using_expr.contains("prx.owner_isolated"),
                "A04 policy {policy} shares workspace rows with owner-isolated callers"
            );
            policy_proof.push_str(policy);
            policy_proof.push('\0');
            policy_proof.push_str(&using_expr);
            policy_proof.push('\0');
            policy_proof.push_str(&check_expr);
            policy_proof.push('\0');
            // D-028: the `public` tier is granted by its own read-only policy,
            // never by the owner-isolation policy that also governs writes.
            anyhow::ensure!(
                !using_expr.contains("'public'") && !check_expr.contains("'public'"),
                "A04 policy {policy} admits public rows outside the read-only public policy"
            );
        }
        let public_row = client
            .query_opt(
                "SELECT cmd, COALESCE(qual, ''), COALESCE(with_check, '') \
                 FROM pg_policies \
                 WHERE schemaname = $1 AND tablename = $2 AND policyname = $3",
                &[&schema, &table, &MEMORIES_PUBLIC_READ_POLICY],
            )?
            .with_context(|| format!("A04 policy {MEMORIES_PUBLIC_READ_POLICY} is missing"))?;
        let public_cmd: String = public_row.get(0);
        let public_using: String = public_row.get(1);
        let public_check: String = public_row.get(2);
        anyhow::ensure!(
            public_cmd == "SELECT"
                && public_check.is_empty()
                && public_using.contains("'public'")
                && public_using.contains("prx.current_workspace")
                && public_using.contains("prx.current_owner"),
            "A04 policy {MEMORIES_PUBLIC_READ_POLICY} is not a read-only, workspace-scoped public grant"
        );
        policy_proof.push_str(MEMORIES_PUBLIC_READ_POLICY);
        policy_proof.push('\0');
        policy_proof.push_str(&public_cmd);
        policy_proof.push('\0');
        policy_proof.push_str(&public_using);
        policy_proof.push('\0');
        let policy_hash = Self::schema_migration_checksum(&policy_proof);
        Ok(format!("{migration_proof}:policy-sha256:{policy_hash}"))
    }

    // SAFETY: `schema_ident` and `qualified_table` are validated+quoted at
    // construction time via `validate_identifier()` + `quote_identifier()`,
    // which enforce `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$`. SQL injection is not
    // possible through these interpolated identifiers.
    pub(super) fn init_schema(
        client: &mut Client,
        schema_ident: &str,
        qualified_table: &str,
        qualified_message_events_table: &str,
        qualified_memory_events_table: &str,
        qualified_drafts_table: &str,
        qualified_documents_table: &str,
        qualified_document_chunks_table: &str,
        qualified_memory_links_table: &str,
        qualified_retrieval_traces_table: &str,
        qualified_compaction_runs_table: &str,
        qualified_embedding_cache_table: &str,
        embedding_dimensions: usize,
    ) -> Result<bool> {
        client.batch_execute(&format!(
            "
            CREATE SCHEMA IF NOT EXISTS {schema_ident};

            CREATE TABLE IF NOT EXISTS {qualified_table} (
                id TEXT PRIMARY KEY,
                key TEXT UNIQUE NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                session_id TEXT,
                useful_count INTEGER NOT NULL DEFAULT 0,
                workspace_id TEXT,
                owner_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                source_event_id TEXT,
                source TEXT,
                embedding BYTEA,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions BIGINT,
                channel TEXT,
                chat_type TEXT,
                chat_id TEXT,
                sender_id TEXT,
                raw_sender TEXT,
                topic_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                sensitivity TEXT NOT NULL DEFAULT 'normal',
                risk_signals TEXT,
                policy_version BIGINT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_category ON {qualified_table}(category);
            CREATE INDEX IF NOT EXISTS idx_memories_session_id ON {qualified_table}(session_id);
            CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON {qualified_table}(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_memories_context_scope
                ON {qualified_table}(channel, chat_id, raw_sender);
            CREATE INDEX IF NOT EXISTS idx_memories_owner_scope
                ON {qualified_table}(owner_id);
            CREATE INDEX IF NOT EXISTS idx_memories_sender_scope
                ON {qualified_table}(sender_id);
            CREATE INDEX IF NOT EXISTS idx_memories_visibility
                ON {qualified_table}(visibility, sensitivity);
            CREATE INDEX IF NOT EXISTS idx_memories_embedding_current
                ON {qualified_table}(embedding_provider, embedding_model, embedding_dimensions, session_id, id)
                WHERE embedding IS NOT NULL;

            CREATE TABLE IF NOT EXISTS {schema_ident}.agent_identity_bindings (
                binding_id TEXT PRIMARY KEY,
                external_subject TEXT NOT NULL,
                external_issuer TEXT NOT NULL,
                auth_method TEXT NOT NULL,
                prx_owner_id TEXT NOT NULL,
                prx_principal_id TEXT NOT NULL,
                capabilities TEXT NOT NULL,
                expires_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_used_at TIMESTAMPTZ,
                UNIQUE (external_issuer, external_subject, auth_method)
            );
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_lookup
                ON {schema_ident}.agent_identity_bindings(external_issuer, external_subject);
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_owner
                ON {schema_ident}.agent_identity_bindings(prx_owner_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.approval_grants (
                grant_id TEXT PRIMARY KEY,
                version BIGINT NOT NULL,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                session_key TEXT,
                issuer_authority TEXT NOT NULL,
                issuer_authority_id TEXT NOT NULL,
                issuer_public_key_id TEXT NOT NULL,
                capability_op_id TEXT NOT NULL,
                capability_op_id_match TEXT NOT NULL,
                capability_risk_level TEXT NOT NULL CHECK (capability_risk_level IN ('low','medium','high','critical')),
                resource_constraints_json JSONB NOT NULL DEFAULT '{{}}'::jsonb,
                grant_json JSONB NOT NULL,
                signature_alg TEXT NOT NULL,
                signed_payload_sha256 TEXT NOT NULL,
                issued_at TIMESTAMPTZ NOT NULL,
                not_before TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                max_uses BIGINT NOT NULL,
                uses_consumed BIGINT NOT NULL DEFAULT 0,
                related_task_id TEXT,
                related_message_event_id BIGINT,
                revoked_at TIMESTAMPTZ,
                revocation_reason TEXT,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_approval_grants_owner
                ON {schema_ident}.approval_grants(workspace_id, owner_id, issued_at DESC);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_principal
                ON {schema_ident}.approval_grants(workspace_id, principal_id, issued_at DESC);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_capability
                ON {schema_ident}.approval_grants(workspace_id, capability_op_id, expires_at);
            CREATE INDEX IF NOT EXISTS idx_approval_grants_active
                ON {schema_ident}.approval_grants(workspace_id, expires_at, revoked_at);

            CREATE TABLE IF NOT EXISTS {schema_ident}.approval_grant_events (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                grant_id TEXT NOT NULL REFERENCES {schema_ident}.approval_grants(grant_id) ON DELETE CASCADE,
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'grant.issued','grant.verified','grant.consumed','grant.revoked',
                    'grant.rejected','grant.expired'
                )),
                actor TEXT NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                payload_json JSONB
            );
            CREATE INDEX IF NOT EXISTS idx_approval_grant_events_grant
                ON {schema_ident}.approval_grant_events(grant_id, occurred_at);
            CREATE INDEX IF NOT EXISTS idx_approval_grant_events_type
                ON {schema_ident}.approval_grant_events(event_type, occurred_at);

            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS useful_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS workspace_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS agent_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS persona_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source_event_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding BYTEA;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_provider TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_model TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_dimensions BIGINT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS channel TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS chat_type TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS chat_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS sender_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS raw_sender TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS topic_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS sensitivity TEXT NOT NULL DEFAULT 'normal';
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS risk_signals TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS policy_version BIGINT;

            CREATE TABLE IF NOT EXISTS {qualified_embedding_cache_table} (
                workspace_id TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding BYTEA NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions BIGINT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL,
                accessed_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions)
            );
            ALTER TABLE {qualified_embedding_cache_table}
                ADD COLUMN IF NOT EXISTS workspace_id TEXT NOT NULL DEFAULT '__prx_system__';
            ALTER TABLE {qualified_embedding_cache_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT NOT NULL DEFAULT '__prx_system__';
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_accessed
                ON {qualified_embedding_cache_table}(accessed_at);
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_provider_model
                ON {qualified_embedding_cache_table}(provider, model, dimensions, accessed_at);

            CREATE TABLE IF NOT EXISTS {qualified_message_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                idempotency_key TEXT,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                source TEXT NOT NULL,
                channel TEXT,
                session_key TEXT,
                parent_session_key TEXT,
                run_id TEXT,
                parent_run_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                sender TEXT,
                recipient TEXT,
                role TEXT NOT NULL,
                event_type TEXT NOT NULL,
                source_ref_json TEXT,
                subject_ref_json TEXT,
                goal_id TEXT,
                causation_event_id TEXT,
                correlation_id TEXT,
                attempt_id TEXT,
                lease_epoch BIGINT,
                config_generation_id BIGINT,
                config_source_revision TEXT,
                content TEXT NOT NULL,
                content_hash TEXT,
                raw_payload_json TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_message_events_table}
                ADD COLUMN IF NOT EXISTS event_type TEXT;
            ALTER TABLE {qualified_message_events_table}
                ADD COLUMN IF NOT EXISTS source_ref_json TEXT,
                ADD COLUMN IF NOT EXISTS subject_ref_json TEXT,
                ADD COLUMN IF NOT EXISTS goal_id TEXT,
                ADD COLUMN IF NOT EXISTS causation_event_id TEXT,
                ADD COLUMN IF NOT EXISTS correlation_id TEXT,
                ADD COLUMN IF NOT EXISTS attempt_id TEXT,
                ADD COLUMN IF NOT EXISTS lease_epoch BIGINT,
                ADD COLUMN IF NOT EXISTS config_generation_id BIGINT,
                ADD COLUMN IF NOT EXISTS config_source_revision TEXT;

            CREATE INDEX IF NOT EXISTS idx_message_events_workspace_id
                ON {qualified_message_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_owner_id
                ON {qualified_message_events_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_session
                ON {qualified_message_events_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_agent
                ON {qualified_message_events_table}(workspace_id, agent_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_channel_sender
                ON {qualified_message_events_table}(workspace_id, channel, sender, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_visibility
                ON {qualified_message_events_table}(workspace_id, visibility, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_event_type
                ON {qualified_message_events_table}(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_correlation
                ON {qualified_message_events_table}(workspace_id, correlation_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_created_at
                ON {qualified_message_events_table}(created_at);
            CREATE INDEX IF NOT EXISTS idx_message_events_config_generation
                ON {qualified_message_events_table}(workspace_id, config_generation_id, id);

            CREATE TABLE IF NOT EXISTS {qualified_memory_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                event_type TEXT NOT NULL,
                subject_table TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                session_key TEXT,
                run_id TEXT,
                parent_run_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS run_id TEXT;
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS parent_run_id TEXT;
            -- Schema v16 (C03 / V02-R4): owner attribution for memory events.
            -- Existing rows keep a NULL owner (unattributed) and are never
            -- rewritten; an owner-isolated principal does not admit them.
            ALTER TABLE {qualified_memory_events_table} ADD COLUMN IF NOT EXISTS owner_id TEXT;

            CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
                ON {qualified_memory_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_type
                ON {qualified_memory_events_table}(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_session
                ON {qualified_memory_events_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_run
                ON {qualified_memory_events_table}(workspace_id, run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_parent_run
                ON {qualified_memory_events_table}(workspace_id, parent_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_owner
                ON {qualified_memory_events_table}(workspace_id, owner_id, id);

            CREATE TABLE IF NOT EXISTS {qualified_drafts_table} (
                id BIGSERIAL PRIMARY KEY,
                draft_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                worker_run_id TEXT NOT NULL,
                parent_run_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                key TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                source_event_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                status TEXT NOT NULL DEFAULT 'pending',
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );

            -- Backfill legacy columns BEFORE creating indexes that reference them:
            -- a legacy drafts table predating `source_event_id` would make the
            -- `idx_memory_drafts_source_event` index below fail (`column ...
            -- does not exist`) and abort this whole batch.
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS owner_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS parent_run_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS agent_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS persona_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS source_event_id TEXT;
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';
            ALTER TABLE {qualified_drafts_table}
                ADD COLUMN IF NOT EXISTS payload_json TEXT;

            CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                ON {qualified_drafts_table}(worker_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_owner
                ON {qualified_drafts_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                ON {qualified_drafts_table}(status, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_source_event
                ON {qualified_drafts_table}(source_event_id);

            CREATE TABLE IF NOT EXISTS {qualified_documents_table} (
                id BIGSERIAL PRIMARY KEY,
                document_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                topic_id TEXT,
                task_id TEXT,
                source_message_event_id TEXT,
                source_kind TEXT NOT NULL,
                source_uri TEXT,
                title TEXT,
                content_sha256 TEXT NOT NULL,
                mime_type TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                metadata_json TEXT,
                chunk_count BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_document_chunks_table} (
                id BIGSERIAL PRIMARY KEY,
                chunk_id TEXT UNIQUE NOT NULL,
                document_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                topic_id TEXT,
                task_id TEXT,
                chunk_index BIGINT NOT NULL,
                heading TEXT,
                content TEXT NOT NULL,
                content_sha256 TEXT NOT NULL,
                embedding BYTEA,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions BIGINT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                source_anchor TEXT NOT NULL,
                token_estimate BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL
            );

            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding BYTEA;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_provider TEXT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_model TEXT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_dimensions BIGINT;
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'workspace';

            CREATE TABLE IF NOT EXISTS {qualified_memory_links_table} (
                id BIGSERIAL PRIMARY KEY,
                link_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                memory_key TEXT,
                memory_event_id TEXT,
                message_event_id TEXT,
                document_id TEXT NOT NULL,
                chunk_id TEXT,
                link_type TEXT NOT NULL,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_retrieval_traces_table} (
                id BIGSERIAL PRIMARY KEY,
                trace_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                source TEXT NOT NULL,
                query TEXT NOT NULL,
                candidate_count BIGINT NOT NULL,
                selected_count BIGINT NOT NULL,
                dropped_count BIGINT NOT NULL,
                budget_tokens BIGINT,
                selected_json TEXT,
                dropped_json TEXT,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {qualified_compaction_runs_table} (
                id BIGSERIAL PRIMARY KEY,
                run_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                trigger TEXT NOT NULL,
                mode TEXT NOT NULL,
                source_message_count BIGINT NOT NULL,
                source_token_estimate BIGINT NOT NULL,
                summary TEXT NOT NULL,
                summary_memory_key TEXT,
                source_event_ids_json TEXT,
                source_event_range_json TEXT,
                source_document_refs_json TEXT,
                fidelity_status TEXT NOT NULL,
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );
            ALTER TABLE {qualified_compaction_runs_table}
                ADD COLUMN IF NOT EXISTS source_event_range_json TEXT;

            CREATE INDEX IF NOT EXISTS idx_documents_workspace
                ON {qualified_documents_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_owner
                ON {qualified_documents_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_topic
                ON {qualified_documents_table}(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_task
                ON {qualified_documents_table}(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_hash
                ON {qualified_documents_table}(workspace_id, content_sha256);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_document
                ON {qualified_document_chunks_table}(document_id, chunk_index);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_workspace
                ON {qualified_document_chunks_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_owner
                ON {qualified_document_chunks_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_topic
                ON {qualified_document_chunks_table}(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_content_tsv
                ON {qualified_document_chunks_table}
                USING GIN (to_tsvector('simple', content));
            CREATE INDEX IF NOT EXISTS idx_document_chunks_embedding_current
                ON {qualified_document_chunks_table}(embedding_provider, embedding_model, embedding_dimensions, workspace_id, owner_id, id)
                WHERE embedding IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_memory_links_workspace
                ON {qualified_memory_links_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_document
                ON {qualified_memory_links_table}(document_id, chunk_id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_key
                ON {qualified_memory_links_table}(workspace_id, memory_key);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_workspace
                ON {qualified_retrieval_traces_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_owner
                ON {qualified_retrieval_traces_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_session
                ON {qualified_retrieval_traces_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_source
                ON {qualified_retrieval_traces_table}(workspace_id, source, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_workspace
                ON {qualified_compaction_runs_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_owner
                ON {qualified_compaction_runs_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_session
                ON {qualified_compaction_runs_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_trigger
                ON {qualified_compaction_runs_table}(workspace_id, trigger, id);

            -- FIX-P0-21: principal resolution tables (parity with SQLite).
            CREATE TABLE IF NOT EXISTS {schema_ident}.identity_bindings (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                channel         TEXT NOT NULL,
                channel_account TEXT NOT NULL,
                display_name    TEXT,
                bound_at        TEXT NOT NULL,
                bound_by        TEXT NOT NULL,
                UNIQUE(channel, channel_account)
            );
            CREATE INDEX IF NOT EXISTS idx_ib_user
                ON {schema_ident}.identity_bindings(user_id);
            CREATE INDEX IF NOT EXISTS idx_ib_channel_account
                ON {schema_ident}.identity_bindings(channel, channel_account);

            CREATE TABLE IF NOT EXISTS {schema_ident}.chat_profiles (
                id          TEXT PRIMARY KEY,
                channel     TEXT NOT NULL,
                chat_id     TEXT NOT NULL,
                chat_kind   TEXT NOT NULL,
                title       TEXT,
                purpose     TEXT,
                notes       TEXT,
                tags        TEXT NOT NULL DEFAULT '[]',
                updated_by  TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                UNIQUE(channel, chat_id)
            );
            CREATE INDEX IF NOT EXISTS idx_chat_profiles_lookup
                ON {schema_ident}.chat_profiles(channel, chat_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.user_policies (
                user_id             TEXT PRIMARY KEY,
                role                TEXT NOT NULL DEFAULT 'guest',
                projects            TEXT NOT NULL DEFAULT '[]',
                visibility_ceiling  TEXT NOT NULL DEFAULT 'private',
                blocked_patterns    TEXT NOT NULL DEFAULT '[]',
                policy_version      BIGINT NOT NULL DEFAULT 1,
                updated_at          TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS {schema_ident}.access_audit_log (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                requester   TEXT NOT NULL,
                action      TEXT NOT NULL,
                query       TEXT,
                memory_id   TEXT,
                policy_rule TEXT,
                result      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_time
                ON {schema_ident}.access_audit_log(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_requester
                ON {schema_ident}.access_audit_log(requester);

            -- FIX-P0-19/20: conversation sessions + turns with owner ACL (parity
            -- with SQLite). owner_id is nullable in Phase 1/2; legacy rows carry
            -- `legacy:<session_key>` and are gated by the legacy visibility flag.
            CREATE TABLE IF NOT EXISTS {schema_ident}.sessions (
                session_key          TEXT PRIMARY KEY,
                channel              TEXT NOT NULL,
                sender               TEXT NOT NULL,
                owner_id             TEXT,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL,
                message_count        BIGINT NOT NULL DEFAULT 0,
                last_message_preview TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                ON {schema_ident}.sessions(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_channel_updated_at
                ON {schema_ident}.sessions(channel, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_owner
                ON {schema_ident}.sessions(owner_id);

            CREATE TABLE IF NOT EXISTS {schema_ident}.conversation_turns (
                id               BIGSERIAL PRIMARY KEY,
                session_key      TEXT NOT NULL,
                owner_id         TEXT,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                timestamp        TEXT NOT NULL,
                message_id       TEXT,
                message_event_id TEXT,
                agent_id         TEXT,
                persona_id       TEXT,
                visibility       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_session_key
                ON {schema_ident}.conversation_turns(session_key);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_timestamp
                ON {schema_ident}.conversation_turns(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_owner_session
                ON {schema_ident}.conversation_turns(owner_id, session_key);

            -- FIX-P1-18: additional hot-path indexes for owner/topic scoped reads.
            CREATE INDEX IF NOT EXISTS idx_document_chunks_task
                ON {qualified_document_chunks_table}(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_owner
                ON {qualified_memory_links_table}(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_event
                ON {qualified_memory_links_table}(memory_event_id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_message_event
                ON {qualified_memory_links_table}(message_event_id);
            CREATE INDEX IF NOT EXISTS idx_documents_source_event
                ON {qualified_documents_table}(source_message_event_id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_summary_key
                ON {qualified_compaction_runs_table}(workspace_id, summary_memory_key, id);
            "
        ))?;

        Self::ensure_message_event_idempotency_scope(client, qualified_message_events_table)?;

        // FIX-P1-19: document_chunks → documents FK with ON DELETE CASCADE.
        // Added as a guarded step (not in the idempotent batch) because adding a
        // constraint that already exists raises an error rather than being a
        // no-op; we therefore detect-then-add and tolerate the duplicate.
        Self::ensure_document_chunks_fk(client, qualified_documents_table, qualified_document_chunks_table)?;
        Self::ensure_embedding_cache_scope_key(client, qualified_embedding_cache_table)?;

        // A04: vector-bearing memory and document rows are protected by an
        // authoritative, fail-closed RLS migration. Startup must fail if the
        // configured role cannot install or verify these policies; silently
        // running without isolation would make the attestation untruthful.
        Self::init_vector_rls(
            client,
            qualified_table,
            qualified_document_chunks_table,
            qualified_embedding_cache_table,
        )?;

        // FIX-P0-25: record-and-verify versioned schema migrations. The DDL was
        // already executed by the idempotent batch above; this ledger records a
        // stable (version, name, checksum) per logical step so drift is detected
        // (checksum mismatch → bail) at startup.
        Self::run_memory_schema_migrations(client, schema_ident)?;

        Ok(Self::try_init_pgvector(
            client,
            qualified_table,
            qualified_document_chunks_table,
            embedding_dimensions,
        ))
    }

    /// SHA-256 (hex) of a migration's canonical descriptor text.
    pub(crate) fn schema_migration_checksum(text: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Versioned registry of the canonical schema steps created by `init_schema`,
    /// retro-fitted in execution order. Each text is a stable canonical descriptor
    /// used only as a checksum anchor (version unchanged ⇒ text unchanged). NEVER
    /// mutate an existing entry's text; append a new version for new schema changes.
    pub(crate) const fn memory_schema_migration_registry() -> &'static [(i64, &'static str, &'static str)] {
        &[
            (
                1,
                "memories_core",
                "memories(id,key,content,category,created_at,updated_at,session_id,useful_count,workspace_id,owner_id,agent_id,persona_id,source_event_id,source,embedding,embedding_provider,embedding_model,embedding_dimensions,channel,chat_type,chat_id,sender_id,raw_sender,topic_id,visibility,sensitivity,risk_signals,policy_version)",
            ),
            (
                2,
                "agent_identity_bindings",
                "agent_identity_bindings(binding_id,external_subject,external_issuer,auth_method,prx_owner_id,prx_principal_id,capabilities,expires_at,created_at,last_used_at)",
            ),
            (3, "approval_grants", "approval_grants + approval_grant_events"),
            (
                4,
                "embedding_cache",
                "embedding_cache(content_hash,embedding,provider,model,dimensions,created_at,accessed_at)",
            ),
            (
                5,
                "message_events",
                "message_events(id,event_id,idempotency_key,workspace_id,owner_id,source,channel,session_key,parent_session_key,run_id,parent_run_id,agent_id,persona_id,sender,recipient,role,event_type,content,content_hash,raw_payload_json,visibility,created_at,updated_at)",
            ),
            (
                6,
                "memory_events",
                "memory_events(event_id,workspace_id,event_type,subject_table,subject_id,session_key,agent_id,persona_id,visibility,payload_json,created_at)",
            ),
            (
                7,
                "memory_drafts",
                "memory_drafts(id,draft_id,workspace_id,owner_id,worker_run_id,parent_run_id,session_key,agent_id,persona_id,key,content,category,source_event_id,visibility,status,payload_json,created_at,updated_at)",
            ),
            (
                8,
                "documents",
                "documents(id,document_id,workspace_id,owner_id,topic_id,task_id,source_message_event_id,source_kind,source_uri,title,content_sha256,mime_type,visibility,metadata_json,chunk_count,created_at,updated_at)",
            ),
            (
                9,
                "document_chunks",
                "document_chunks(id,document_id,chunk_index,content,content_sha256,embedding,token_count,owner_id,created_at)",
            ),
            (
                10,
                "memory_links_traces_compaction",
                "memory_links + retrieval_traces + compaction_runs",
            ),
            (
                11,
                "evolution_proposals",
                "evolution_proposals(id,draft_id,owner_id,principal_id,workspace_id,topic_id,task_id,source_message_event_ids_json,source_memory_event_ids_json,evidence_hashes_json,target_resource_json,proposed_change_json,risk_level,mode,created_at,created_by_runtime,judge_verdict_json,applied_at,applied_by,rollback_anchor_json) + evolution_proposal_events(id,draft_id,event_type,occurred_at,actor,payload_json)",
            ),
            (
                12,
                "memory_events_run_lineage",
                "memory_events + run_id + parent_run_id + idx_memory_events_run + idx_memory_events_parent_run",
            ),
            (
                13,
                "principal_resolution_tables",
                "identity_bindings(id,user_id,channel,channel_account,display_name,bound_at,bound_by) + user_policies(user_id,role,projects,visibility_ceiling,blocked_patterns,policy_version,updated_at) + access_audit_log(id,timestamp,requester,action,query,memory_id,policy_rule,result)",
            ),
            (
                14,
                "sessions_and_conversation_turns",
                "sessions(session_key,channel,sender,owner_id,created_at,updated_at,message_count,last_message_preview) + conversation_turns(id,session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,persona_id,visibility) + idx_conversation_turns_owner_session",
            ),
            (
                15,
                "owner_topic_indexes_and_chunk_fk",
                "idx_document_chunks_task + idx_memory_links_owner + idx_memory_links_memory_event + idx_memory_links_message_event + idx_documents_source_event + idx_compaction_runs_summary_key + fk_document_chunks_document(ON DELETE CASCADE)",
            ),
            (
                16,
                "memories_row_level_security",
                "memories ENABLE+FORCE ROW LEVEL SECURITY + POLICY memories_owner_isolation(USING/WITH CHECK: current_setting('prx.rls_bypass', true)='on' OR current_setting('prx.current_owner', true) IS NULL OR owner_id IS NOT DISTINCT FROM current_setting('prx.current_owner', true))",
            ),
            (
                17,
                "compaction_source_event_range",
                "compaction_runs.source_event_ids_json contains only real MessageEvent event_id strings + source_event_range_json(first_event_id,last_event_id,first_row_id,last_row_id,source_event_count)",
            ),
            (
                18,
                "message_event_workspace_idempotency",
                "message_events UNIQUE(workspace_id,idempotency_key) replaces global UNIQUE(idempotency_key) + workspace-scoped conflict lookup",
            ),
            (
                19,
                "message_event_config_generation",
                "message_events + config_generation_id + config_source_revision + idx_message_events_config_generation",
            ),
            (
                20,
                "message_event_execution_metadata",
                "message_events + source_ref_json + subject_ref_json + goal_id + causation_event_id + correlation_id + attempt_id + lease_epoch",
            ),
            (
                21,
                "vector_owner_workspace_row_level_security",
                "memories + document_chunks + embedding_cache scoped by workspace_id/owner_id and ENABLE+FORCE ROW LEVEL SECURITY; transaction-local workspace required; private/cache owner exact; shared reads tenant-scoped; writes owner exact; missing context denies",
            ),
            (
                22,
                "memory_event_owner_isolation",
                // C03 / V02-R4, parity with SQLite v16: `memory_events` gains
                // `owner_id` so an externally authenticated (owner-isolated)
                // principal can be refused another owner's `session`/`private`
                // rows. Added by `ADD COLUMN IF NOT EXISTS`; legacy rows keep
                // NULL (unattributed).
                "memory_events + owner_id + idx_memory_events_owner",
            ),
            (
                23,
                "vector_rls_owner_isolated_reads",
                // OQ-8 / D-019: the policy DDL itself is reinstalled on every
                // start by `init_vector_rls`; this entry records the contract
                // change so a schema initialized by this build is identifiable.
                "memories + document_chunks POLICY *_owner_isolation USING: shared global/workspace read arm additionally requires COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'; prx.owner_isolated set transaction-local from require_owner_match; WITH CHECK and embedding_cache unchanged",
            ),
            (
                24,
                "memories_public_read_policy",
                // D-028: `public` memory rows are readable across owners, as
                // the shared SQL predicate and SQLite already allow. Read only.
                "memories POLICY memories_public_read FOR SELECT USING: transaction-local workspace and owner present AND workspace_id = prx.current_workspace AND visibility = 'public'; permissive, read only (UPDATE/DELETE/INSERT still need memories_owner_isolation); independent of prx.owner_isolated; document_chunks and embedding_cache unchanged",
            ),
        ]
    }

    /// Record-and-verify versioned schema migrations (FIX-P0-25, Postgres).
    ///
    /// - already-applied version, matching checksum → skipped;
    /// - already-applied version, differing checksum → `bail!` (fail-fast);
    /// - unapplied version → recorded.
    ///
    /// SAFETY: `schema_ident` is validated+quoted at construction via
    /// `validate_identifier()` + `quote_identifier()`; the interpolated table
    /// name cannot carry SQL injection. All values are passed as bind params.
    fn run_memory_schema_migrations(client: &mut Client, schema_ident: &str) -> Result<()> {
        let migrations_table = format!("{schema_ident}.memory_schema_migrations");
        client.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {migrations_table} (
                    version    BIGINT PRIMARY KEY,
                    name       TEXT NOT NULL,
                    checksum   TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                )"
            ),
            &[],
        )?;

        let select_sql = format!("SELECT checksum FROM {migrations_table} WHERE version = $1");
        let insert_sql =
            format!("INSERT INTO {migrations_table} (version, name, checksum, applied_at) VALUES ($1, $2, $3, $4)");

        for (version, name, text) in Self::memory_schema_migration_registry() {
            let checksum = Self::schema_migration_checksum(text);
            match client.query_opt(&select_sql, &[version])? {
                Some(row) => {
                    let recorded: String = row.get(0);
                    if recorded != checksum {
                        anyhow::bail!(
                            "memory schema migration checksum mismatch for version {version} ({name}): \
                             expected {checksum}, found {recorded}"
                        );
                    }
                }
                None => {
                    let name = (*name).to_string();
                    client.execute(&insert_sql, &[version, &name, &checksum, &Utc::now().to_rfc3339()])?;
                }
            }
        }
        Ok(())
    }

    /// FIX-P1-19: ensure `document_chunks.document_id` references
    /// `documents.document_id` with `ON DELETE CASCADE`.
    ///
    /// `ADD CONSTRAINT` is not idempotent in PostgreSQL, so we first probe
    /// `pg_constraint` for the named constraint and only add it when missing.
    ///
    /// SAFETY: both qualified table identifiers are validated and quoted at
    /// construction; all dynamic metadata values use bind params.
    fn ensure_document_chunks_fk(
        client: &mut Client,
        qualified_documents_table: &str,
        qualified_document_chunks_table: &str,
    ) -> Result<()> {
        const FK_NAME: &str = "fk_document_chunks_document";
        let exists: bool = client
            .query_one(
                "SELECT EXISTS (
                    SELECT 1
                    FROM pg_constraint
                    WHERE conrelid = to_regclass($1::text)
                      AND conname = $2
                )",
                &[&qualified_document_chunks_table, &FK_NAME],
            )?
            .get(0);
        if exists {
            return Ok(());
        }
        let add_fk = format!(
            "ALTER TABLE {qualified_document_chunks_table}
             ADD CONSTRAINT {FK_NAME}
             FOREIGN KEY (document_id)
             REFERENCES {qualified_documents_table}(document_id)
             ON DELETE CASCADE"
        );
        if let Err(error) = client.batch_execute(&add_fk) {
            // Tolerate a concurrent initializer that added the same constraint
            // between our probe and ALTER; surface anything else.
            let refreshed: bool = client
                .query_one(
                    "SELECT EXISTS (
                        SELECT 1
                        FROM pg_constraint
                        WHERE conrelid = to_regclass($1::text)
                          AND conname = $2
                    )",
                    &[&qualified_document_chunks_table, &FK_NAME],
                )?
                .get(0);
            if !refreshed {
                return Err(anyhow::anyhow!("failed to add document_chunks foreign key: {error}"));
            }
        }
        Ok(())
    }

    /// Upgrade the embedding cache from its historical global content key to
    /// a workspace/owner-scoped key. The cache stores vectors and therefore
    /// must not reveal reuse or content-derived state across principals.
    fn ensure_embedding_cache_scope_key(client: &mut Client, qualified_cache_table: &str) -> Result<()> {
        let primary = client.query_opt(
            "SELECT c.conname, pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             WHERE c.conrelid = to_regclass($1::text) AND c.contype = 'p'",
            &[&qualified_cache_table],
        )?;
        if let Some(row) = primary {
            let name: String = row.get(0);
            let definition: String = row.get(1);
            if definition.contains("workspace_id") && definition.contains("owner_id") {
                return Ok(());
            }
            let constraint = quote_identifier(&name);
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_cache_table} DROP CONSTRAINT {constraint}; \
                 ALTER TABLE {qualified_cache_table} \
                 ADD PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions);"
            ))?;
        } else {
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_cache_table} \
                 ADD PRIMARY KEY (workspace_id, owner_id, content_hash, provider, model, dimensions);"
            ))?;
        }
        Ok(())
    }

    /// Replace the legacy global MessageEvent idempotency constraint with a
    /// workspace-scoped unique index. The constraint name is discovered from
    /// PostgreSQL metadata because derived table names may be truncated.
    fn ensure_message_event_idempotency_scope(client: &mut Client, qualified_message_events_table: &str) -> Result<()> {
        let legacy_constraint = client.query_opt(
            "SELECT conname
             FROM pg_constraint
             WHERE conrelid = to_regclass($1::text)
               AND contype = 'u'
               AND pg_get_constraintdef(oid) = 'UNIQUE (idempotency_key)'
             LIMIT 1",
            &[&qualified_message_events_table],
        )?;
        if let Some(row) = legacy_constraint {
            let constraint_name: String = row.get(0);
            validate_identifier(&constraint_name, "message event idempotency constraint")?;
            let constraint_ident = quote_identifier(&constraint_name);
            client.batch_execute(&format!(
                "ALTER TABLE {qualified_message_events_table}
                 DROP CONSTRAINT IF EXISTS {constraint_ident}"
            ))?;
        }
        client.batch_execute(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_message_events_workspace_idempotency
             ON {qualified_message_events_table}(workspace_id, idempotency_key)
             WHERE idempotency_key IS NOT NULL"
        ))?;
        Ok(())
    }

    fn try_init_pgvector(
        client: &mut Client,
        qualified_table: &str,
        qualified_document_chunks_table: &str,
        embedding_dimensions: usize,
    ) -> bool {
        if embedding_dimensions == 0 {
            return false;
        }
        let Ok(dimensions) = i64::try_from(embedding_dimensions) else {
            return false;
        };
        let init = format!(
            "
            CREATE EXTENSION IF NOT EXISTS vector;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS embedding_vector vector({dimensions});
            ALTER TABLE {qualified_document_chunks_table}
                ADD COLUMN IF NOT EXISTS embedding_vector vector({dimensions});
            CREATE INDEX IF NOT EXISTS idx_memories_embedding_vector_hnsw
                ON {qualified_table}
                USING hnsw (embedding_vector vector_cosine_ops)
                WHERE embedding_vector IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_document_chunks_embedding_vector_hnsw
                ON {qualified_document_chunks_table}
                USING hnsw (embedding_vector vector_cosine_ops)
                WHERE embedding_vector IS NOT NULL;
            "
        );
        match client.batch_execute(&init) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(error = %error, "Postgres pgvector/ANN initialization skipped");
                false
            }
        }
    }

    /// Install the A04 vector isolation policy on every table that stores a
    /// retrievable vector. Missing workspace/owner context is deliberately not
    /// an allow case. The migration is fatal because starting without the policy
    /// would expose a different security contract than the report advertises.
    pub(super) fn init_vector_rls(
        client: &mut Client,
        qualified_memories_table: &str,
        qualified_document_chunks_table: &str,
        qualified_embedding_cache_table: &str,
    ) -> Result<()> {
        let mut tx = client.transaction()?;
        for (table, policy, shared_reads) in [
            (qualified_memories_table, "memories_owner_isolation", true),
            (qualified_document_chunks_table, "document_chunks_owner_isolation", true),
            (
                qualified_embedding_cache_table,
                "embedding_cache_owner_isolation",
                false,
            ),
        ] {
            tx.batch_execute(&Self::vector_rls_policy_sql(table, policy, shared_reads))?;
        }
        tx.batch_execute(&Self::memories_public_read_policy_sql(
            qualified_memories_table,
            MEMORIES_PUBLIC_READ_POLICY,
        ))?;
        tx.commit()?;
        Ok(())
    }

    /// The read-only `public` policy on `memories` (D-028).
    ///
    /// `public` is the explicit sharing path (D-016), and the shared SQL
    /// predicate admits another owner's `public` memory row for every caller,
    /// owner-isolated or not. This permissive `FOR SELECT` policy lets RLS
    /// agree, within the caller's workspace and only with a full
    /// transaction-local context. It grants nothing to `UPDATE`, `DELETE` or
    /// `INSERT`: those still need the owner-exact `memories_owner_isolation`
    /// policy, so another owner's `public` row stays unwritable. It does not
    /// read `prx.owner_isolated`. `document_chunks` has no `public` tier.
    pub(super) fn memories_public_read_policy_sql(table: &str, policy: &str) -> String {
        format!(
            "
            DROP POLICY IF EXISTS {policy} ON {table};
            CREATE POLICY {policy} ON {table}
                FOR SELECT
                USING (
                    NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                    AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                    AND workspace_id = current_setting('prx.current_workspace', true)
                    AND visibility = 'public'
                );
            "
        )
    }

    /// The row-level-security DDL for one vector table.
    ///
    /// Reads and writes both require the transaction-local workspace and
    /// owner. Inserted and updated rows must carry the caller's owner (WITH
    /// CHECK). Update and delete *targeting* is governed by USING, the read
    /// scope below: owner-isolated callers can update or delete only their
    /// own rows, but for other callers the policy also admits other owners'
    /// `workspace`/`global` rows, and the statement's own SQL predicate
    /// decides (R-B2 F-3). On the shared tables (`memories`,
    /// `document_chunks`) a read also admits another owner's `global` or
    /// `workspace` row, but only for a caller that is not owner-isolated:
    /// OQ-8 / D-019 make the shared SQL predicate the owner contract, and an
    /// owner-isolated caller (`prx.owner_isolated = 'on'`, set by
    /// [`Self::apply_rls_context_raw`]) reaches only its own rows here, so the
    /// policy floor matches that predicate instead of being wider than it. An
    /// unset flag keeps the historical tenant-wide read scope. The embedding
    /// cache has no shared arm at all.
    pub(super) fn vector_rls_policy_sql(table: &str, policy: &str, shared_reads: bool) -> String {
        let read_scope = if shared_reads {
            "(owner_id = current_setting('prx.current_owner', true) \
             OR (visibility IN ('global', 'workspace') \
                 AND COALESCE(current_setting('prx.owner_isolated', true), '') <> 'on'))"
        } else {
            "owner_id = current_setting('prx.current_owner', true)"
        };
        format!(
            "
            ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;
            ALTER TABLE {table} FORCE ROW LEVEL SECURITY;
            DROP POLICY IF EXISTS {policy} ON {table};
            CREATE POLICY {policy} ON {table}
                USING (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR (
                        NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                        AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                        AND workspace_id = current_setting('prx.current_workspace', true)
                        AND {read_scope}
                    )
                )
                WITH CHECK (
                    current_setting('prx.rls_bypass', true) = 'on'
                    OR (
                        NULLIF(current_setting('prx.current_workspace', true), '') IS NOT NULL
                        AND NULLIF(current_setting('prx.current_owner', true), '') IS NOT NULL
                        AND workspace_id = current_setting('prx.current_workspace', true)
                        AND owner_id = current_setting('prx.current_owner', true)
                    )
                );
            "
        )
    }
}

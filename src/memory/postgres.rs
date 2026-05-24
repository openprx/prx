use super::principal::MemoryWriteContext;
use super::traits::{
    Memory, MemoryCategory, MemoryDraft, MemoryDraftInput, MemoryEntry, MemoryEvent, MemoryEventInput, MemoryPrincipal,
    MemoryStoreMetadata, MemoryVisibility, MessageEvent, MessageEventInput, SessionContextQuery, SharedContextQuery,
    validate_memory_write_target,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use postgres::{Client, NoTls, Row};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Maximum allowed connect timeout (seconds) to avoid unreasonable waits.
const POSTGRES_CONNECT_TIMEOUT_CAP_SECS: u64 = 300;

/// PostgreSQL-backed persistent memory.
///
/// This backend focuses on reliable CRUD and keyword recall using SQL, without
/// requiring extension setup (for example pgvector).
pub struct PostgresMemory {
    client: Arc<PostgresClientSlot>,
    qualified_table: String,
    qualified_message_events_table: String,
    qualified_memory_events_table: String,
    qualified_drafts_table: String,
}

struct PostgresClientSlot {
    client: Mutex<Option<Client>>,
}

impl PostgresClientSlot {
    const fn new(client: Client) -> Self {
        Self {
            client: Mutex::new(Some(client)),
        }
    }

    fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        let mut guard = self.client.lock();
        let client = guard.as_mut().context("PostgreSQL memory backend client is closed")?;
        f(client)
    }
}

impl Drop for PostgresClientSlot {
    fn drop(&mut self) {
        let Some(client) = self.client.get_mut().take() else {
            return;
        };

        let handle = std::thread::Builder::new()
            .name("postgres-memory-drop".to_string())
            .spawn(move || drop(client));

        if let Ok(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl PostgresMemory {
    pub fn new(db_url: &str, schema: &str, table: &str, connect_timeout_secs: Option<u64>) -> Result<Self> {
        validate_identifier(schema, "storage schema")?;
        validate_identifier(table, "storage table")?;

        let schema_ident = quote_identifier(schema);
        let table_ident = quote_identifier(table);
        let message_events_table = related_table_name(table, "_message_events")?;
        let memory_events_table = related_table_name(table, "_memory_events")?;
        let drafts_table = related_table_name(table, "_drafts")?;
        let message_events_table_ident = quote_identifier(&message_events_table);
        let memory_events_table_ident = quote_identifier(&memory_events_table);
        let drafts_table_ident = quote_identifier(&drafts_table);
        let qualified_table = format!("{schema_ident}.{table_ident}");
        let qualified_message_events_table = format!("{schema_ident}.{message_events_table_ident}");
        let qualified_memory_events_table = format!("{schema_ident}.{memory_events_table_ident}");
        let qualified_drafts_table = format!("{schema_ident}.{drafts_table_ident}");

        let client = Self::initialize_client(
            db_url.to_string(),
            connect_timeout_secs,
            schema_ident,
            qualified_table.clone(),
            qualified_message_events_table.clone(),
            qualified_memory_events_table.clone(),
            qualified_drafts_table.clone(),
        )?;

        Ok(Self {
            client: Arc::new(PostgresClientSlot::new(client)),
            qualified_table,
            qualified_message_events_table,
            qualified_memory_events_table,
            qualified_drafts_table,
        })
    }

    fn initialize_client(
        db_url: String,
        connect_timeout_secs: Option<u64>,
        schema_ident: String,
        qualified_table: String,
        qualified_message_events_table: String,
        qualified_memory_events_table: String,
        qualified_drafts_table: String,
    ) -> Result<Client> {
        let init_handle = std::thread::Builder::new()
            .name("postgres-memory-init".to_string())
            .spawn(move || -> Result<Client> {
                let mut config: postgres::Config = db_url.parse().context("invalid PostgreSQL connection URL")?;

                if let Some(timeout_secs) = connect_timeout_secs {
                    let bounded = timeout_secs.min(POSTGRES_CONNECT_TIMEOUT_CAP_SECS);
                    config.connect_timeout(Duration::from_secs(bounded));
                }

                let mut client = config
                    .connect(NoTls)
                    .context("failed to connect to PostgreSQL memory backend")?;

                Self::init_schema(
                    &mut client,
                    &schema_ident,
                    &qualified_table,
                    &qualified_message_events_table,
                    &qualified_memory_events_table,
                    &qualified_drafts_table,
                )?;
                Ok(client)
            })
            .context("failed to spawn PostgreSQL initializer thread")?;

        init_handle
            .join()
            .map_err(|_| anyhow::anyhow!("PostgreSQL initializer thread panicked"))?
    }

    // SAFETY: `schema_ident` and `qualified_table` are validated+quoted at
    // construction time via `validate_identifier()` + `quote_identifier()`,
    // which enforce `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$`. SQL injection is not
    // possible through these interpolated identifiers.
    fn init_schema(
        client: &mut Client,
        schema_ident: &str,
        qualified_table: &str,
        qualified_message_events_table: &str,
        qualified_memory_events_table: &str,
        qualified_drafts_table: &str,
    ) -> Result<()> {
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
                agent_id TEXT,
                persona_id TEXT,
                source_event_id TEXT,
                source TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_category ON {qualified_table}(category);
            CREATE INDEX IF NOT EXISTS idx_memories_session_id ON {qualified_table}(session_id);
            CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON {qualified_table}(updated_at DESC);

            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS useful_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS workspace_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS agent_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS persona_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source_event_id TEXT;
            ALTER TABLE {qualified_table}
                ADD COLUMN IF NOT EXISTS source TEXT;

            CREATE TABLE IF NOT EXISTS {qualified_message_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                idempotency_key TEXT UNIQUE,
                workspace_id TEXT NOT NULL,
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
                content TEXT NOT NULL,
                content_hash TEXT,
                raw_payload_json TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_message_events_workspace_id
                ON {qualified_message_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_session
                ON {qualified_message_events_table}(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_agent
                ON {qualified_message_events_table}(workspace_id, agent_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_channel_sender
                ON {qualified_message_events_table}(workspace_id, channel, sender, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_visibility
                ON {qualified_message_events_table}(workspace_id, visibility, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_created_at
                ON {qualified_message_events_table}(created_at);

            CREATE TABLE IF NOT EXISTS {qualified_memory_events_table} (
                id BIGSERIAL PRIMARY KEY,
                event_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                subject_table TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                payload_json TEXT,
                created_at TIMESTAMPTZ NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
                ON {qualified_memory_events_table}(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_type
                ON {qualified_memory_events_table}(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_session
                ON {qualified_memory_events_table}(workspace_id, session_key, id);

            CREATE TABLE IF NOT EXISTS {qualified_drafts_table} (
                id BIGSERIAL PRIMARY KEY,
                draft_id TEXT UNIQUE NOT NULL,
                workspace_id TEXT NOT NULL,
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

            CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                ON {qualified_drafts_table}(worker_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                ON {qualified_drafts_table}(status, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_source_event
                ON {qualified_drafts_table}(source_event_id);

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
            "
        ))?;

        Ok(())
    }

    fn category_to_str(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "core".to_string(),
            MemoryCategory::Daily => "daily".to_string(),
            MemoryCategory::Conversation => "conversation".to_string(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn content_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn is_system_principal(principal: &MemoryPrincipal) -> bool {
        principal.agent_id.as_deref() == Some("system") || principal.persona_id.as_deref() == Some("system")
    }

    fn parse_category(value: &str) -> MemoryCategory {
        match value {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn row_to_entry(row: &Row) -> Result<MemoryEntry> {
        let timestamp: DateTime<Utc> = row.get(4);

        Ok(MemoryEntry {
            id: row.get(0),
            key: row.get(1),
            content: row.get(2),
            category: Self::parse_category(&row.get::<_, String>(3)),
            timestamp: timestamp.to_rfc3339(),
            session_id: row.get(5),
            score: row.try_get(6).ok(),
            tags: None,
            access_count: None,
            useful_count: row.try_get(7).ok(),
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: None,
            compressed_from: None,
        })
    }

    fn row_to_message_event(row: &Row) -> Result<MessageEvent> {
        let created_at: DateTime<Utc> = row.get(19);
        let updated_at: DateTime<Utc> = row.get(20);
        let visibility = row
            .get::<_, String>(18)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MessageEvent {
            id: row.get(0),
            event_id: row.get(1),
            idempotency_key: row.get(2),
            workspace_id: row.get(3),
            source: row.get(4),
            channel: row.get(5),
            session_key: row.get(6),
            parent_session_key: row.get(7),
            run_id: row.get(8),
            parent_run_id: row.get(9),
            agent_id: row.get(10),
            persona_id: row.get(11),
            sender: row.get(12),
            recipient: row.get(13),
            role: row.get(14),
            content: row.get(15),
            content_hash: row.get(16),
            raw_payload_json: row.get(17),
            visibility,
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    fn row_to_memory_event(row: &Row) -> Result<MemoryEvent> {
        let created_at: DateTime<Utc> = row.get(11);
        let visibility = row
            .get::<_, String>(9)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryEvent {
            id: row.get(0),
            event_id: row.get(1),
            workspace_id: row.get(2),
            event_type: row.get(3),
            subject_table: row.get(4),
            subject_id: row.get(5),
            session_key: row.get(6),
            agent_id: row.get(7),
            persona_id: row.get(8),
            visibility,
            payload_json: row.get(10),
            created_at: created_at.to_rfc3339(),
        })
    }

    fn row_to_draft(row: &Row) -> Result<MemoryDraft> {
        let created_at: DateTime<Utc> = row.get(15);
        let updated_at: DateTime<Utc> = row.get(16);
        let visibility = row
            .get::<_, String>(12)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryDraft {
            id: row.get(0),
            draft_id: row.get(1),
            workspace_id: row.get(2),
            worker_run_id: row.get(3),
            parent_run_id: row.get(4),
            session_key: row.get(5),
            agent_id: row.get(6),
            persona_id: row.get(7),
            key: row.get(8),
            content: row.get(9),
            category: Self::parse_category(&row.get::<_, String>(10)),
            source_event_id: row.get(11),
            visibility,
            status: row.get(13),
            payload_json: row.get(14),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }
}

fn validate_identifier(value: &str, field_name: &str) -> Result<()> {
    // PostgreSQL identifiers: start with letter or underscore, followed by
    // letters, digits, or underscores.  Maximum length is 63 bytes (NAMEDATALEN-1).
    // We enforce the regex ^[a-zA-Z_][a-zA-Z0-9_]{0,62}$ to prevent any
    // possibility of SQL injection through identifier manipulation.
    if value.is_empty() {
        anyhow::bail!("{field_name} must not be empty");
    }

    if value.len() > 63 {
        anyhow::bail!(
            "{field_name} exceeds PostgreSQL identifier limit of 63 characters; got {} characters",
            value.len()
        );
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("{field_name} must not be empty");
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("{field_name} must start with an ASCII letter or underscore; got '{value}'");
    }

    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        anyhow::bail!("{field_name} can only contain ASCII letters, numbers, and underscores; got '{value}'");
    }

    Ok(())
}

fn related_table_name(base: &str, suffix: &str) -> Result<String> {
    let max_base_len = 63usize
        .checked_sub(suffix.len())
        .context("related table suffix exceeds PostgreSQL identifier length")?;
    let prefix = if base.len() > max_base_len {
        &base[..max_base_len]
    } else {
        base
    };
    let name = format!("{prefix}{suffix}");
    validate_identifier(&name, "related storage table")?;
    Ok(name)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{value}\"")
}

#[async_trait]
impl Memory for PostgresMemory {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn store(&self, key: &str, content: &str, category: MemoryCategory, session_id: Option<&str>) -> Result<()> {
        self.store_with_metadata(key, content, category, session_id, MemoryStoreMetadata::default())
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
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
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
                        workspace_id, agent_id, persona_id, source_event_id, source
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
                    agent_id = EXCLUDED.agent_id,
                    persona_id = EXCLUDED.persona_id,
                    source_event_id = EXCLUDED.source_event_id,
                    source = EXCLUDED.source
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
                        &metadata.agent_id,
                        &metadata.persona_id,
                        &metadata.source_event_id,
                        &metadata.source,
                    ],
                )?;
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
        let _ = context;
        self.store_with_metadata(key, content, category, session_id, metadata)
            .await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let query = query.trim().to_string();
        let sid = session_id.map(str::to_string);

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
            rows.iter()
                .map(Self::row_to_entry)
                .collect::<Result<Vec<MemoryEntry>>>()
        })
        .await?
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

    async fn append_message_event(&self, input: MessageEventInput) -> Result<MessageEvent> {
        let client = self.client.clone();
        let qualified_message_events_table = self.qualified_message_events_table.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MessageEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let content_hash = Self::content_hash(&input.content);
            let visibility = input.visibility.as_str().to_string();

            let insert_stmt = format!(
                "
                INSERT INTO {qualified_message_events_table} (
                    event_id, idempotency_key, workspace_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, content, content_hash, raw_payload_json,
                    visibility, created_at, updated_at
                )
                VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    $11, $12, $13, $14, $15, $16, $17, $18, $19, $20
                )
                ON CONFLICT DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, content, content_hash, raw_payload_json,
                       visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE event_id = $1 OR ($2::TEXT IS NOT NULL AND idempotency_key = $2)
                ORDER BY CASE WHEN event_id = $1 THEN 0 ELSE 1 END
                LIMIT 1
                "
            );
            let outbox_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id, session_key,
                    agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, 'message_events', $4, $5, $6, $7, $8, NULL, $9)
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
                        &input.source,
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
                        &input.content,
                        &content_hash,
                        &input.raw_payload_json,
                        &visibility,
                        &now,
                        &now,
                    ],
                )?;
                let row = tx.query_one(&select_stmt, &[&event_id, &input.idempotency_key])?;
                let event = Self::row_to_message_event(&row)?;
                if inserted > 0 {
                    let outbox_event_type = if event.role == "event" {
                        "worker.result.created"
                    } else {
                        "message.created"
                    };
                    tx.execute(
                        &outbox_stmt,
                        &[
                            &Uuid::new_v4().to_string(),
                            &event.workspace_id,
                            &outbox_event_type,
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

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, content, content_hash, raw_payload_json,
                       visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5)
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ))
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $8
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &after_id,
                        &principal.workspace_id,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.session_key,
                        &principal.sender,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
            })?;
            rows.iter()
                .map(Self::row_to_message_event)
                .collect::<Result<Vec<MessageEvent>>>()
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

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, content, content_hash, raw_payload_json,
                       visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5)
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                                  OR ($6::TEXT IS NOT NULL AND sender = $6)
                              ))
                              OR (visibility = 'system' AND $7::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id DESC
                LIMIT $8
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &after_id,
                        &principal.workspace_id,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.session_key,
                        &principal.sender,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
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
            .collect::<std::collections::HashSet<_>>();

        tokio::task::spawn_blocking(move || -> Result<Vec<MessageEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, idempotency_key, workspace_id, source, channel, session_key,
                       parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                       sender, recipient, role, content, content_hash, raw_payload_json,
                       visibility, created_at, updated_at
                FROM {qualified_message_events_table}
                WHERE id > $1
                  AND workspace_id = $2
                  AND session_key = $3
                  AND (
                      visibility IN ('global', 'workspace')
                      OR (visibility = 'agent' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                      ))
                      OR visibility = 'session'
                      OR (visibility = 'private' AND (
                          ($4::TEXT IS NOT NULL AND agent_id = $4)
                          OR ($5::TEXT IS NOT NULL AND persona_id = $5)
                          OR ($6::TEXT IS NOT NULL AND sender = $6)
                      ))
                      OR (visibility = 'system' AND $7::BOOLEAN)
                  )
                ORDER BY id DESC
                LIMIT $8
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &after_id,
                        &principal.workspace_id,
                        &session_key,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.sender,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
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

    async fn append_memory_event(&self, input: MemoryEventInput) -> Result<MemoryEvent> {
        let client = self.client.clone();
        let qualified_memory_events_table = self.qualified_memory_events_table.clone();

        tokio::task::spawn_blocking(move || -> Result<MemoryEvent> {
            let now = Utc::now();
            let event_id = input.event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let insert_stmt = format!(
                "
                INSERT INTO {qualified_memory_events_table} (
                    event_id, workspace_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (event_id) DO NOTHING
                "
            );
            let select_stmt = format!(
                "
                SELECT id, event_id, workspace_id, event_type, subject_table, subject_id,
                       session_key, agent_id, persona_id, visibility, payload_json, created_at
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
                        &input.event_type,
                        &input.subject_table,
                        &input.subject_id,
                        &input.session_key,
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

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryEvent>> {
            let stmt = format!(
                "
                SELECT id, event_id, workspace_id, event_type, subject_table, subject_id,
                       session_key, agent_id, persona_id, visibility, payload_json, created_at
                FROM {qualified_memory_events_table}
                WHERE id > $1
                  AND (
                      visibility = 'global'
                      OR (
                          workspace_id = $2
                          AND (
                              visibility = 'workspace'
                              OR (visibility = 'agent' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'session' AND $5::TEXT IS NOT NULL AND session_key = $5)
                              OR (visibility = 'private' AND (
                                  ($3::TEXT IS NOT NULL AND agent_id = $3)
                                  OR ($4::TEXT IS NOT NULL AND persona_id = $4)
                              ))
                              OR (visibility = 'system' AND $6::BOOLEAN)
                          )
                      )
                  )
                ORDER BY id ASC
                LIMIT $7
                "
            );
            let rows = client.with_client(|client| {
                Ok(client.query(
                    &stmt,
                    &[
                        &after_id,
                        &principal.workspace_id,
                        &principal.agent_id,
                        &principal.persona_id,
                        &principal.session_key,
                        &system_allowed,
                        &limit_i64,
                    ],
                )?)
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
        let draft_id = input.draft_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let category = Self::category_to_str(&input.category);
        let visibility = input.visibility.as_str().to_string();

        tokio::task::spawn_blocking(move || -> Result<MemoryDraft> {
            let now = Utc::now();
            let stmt = format!(
                "
                INSERT INTO {qualified_drafts_table}
                    (
                        draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'pending', $13, $14, $15)
                ON CONFLICT (draft_id) DO UPDATE SET
                    workspace_id = EXCLUDED.workspace_id,
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
                    id, draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let row = client.with_client(|client| {
                Ok(client.query_one(
                    &stmt,
                    &[
                        &draft_id,
                        &input.workspace_id,
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
                )?)
            })?;
            Self::row_to_draft(&row)
        })
        .await?
    }

    async fn list_memory_drafts_for_run(&self, worker_run_id: &str) -> Result<Vec<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let worker_run_id = worker_run_id.to_string();

        tokio::task::spawn_blocking(move || -> Result<Vec<MemoryDraft>> {
            let stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                FROM {qualified_drafts_table}
                WHERE worker_run_id = $1
                ORDER BY id
                "
            );
            let rows = client.with_client(|client| Ok(client.query(&stmt, &[&worker_run_id])?))?;
            rows.iter()
                .map(Self::row_to_draft)
                .collect::<Result<Vec<MemoryDraft>>>()
        })
        .await?
    }

    async fn merge_memory_draft(&self, draft_id: &str) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_table = self.qualified_table.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let draft_id = draft_id.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let select_stmt = format!(
                "
                SELECT
                    id, draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
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
                        workspace_id, agent_id, persona_id, source_event_id, source
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'memory_draft')
                ON CONFLICT (key) DO UPDATE SET
                    content = EXCLUDED.content,
                    category = EXCLUDED.category,
                    updated_at = EXCLUDED.updated_at,
                    session_id = EXCLUDED.session_id,
                    workspace_id = EXCLUDED.workspace_id,
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
                    id, draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let row = client.with_client(|client| {
                client.execute(
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
                        &draft.agent_id,
                        &draft.persona_id,
                        &draft.source_event_id,
                    ],
                )?;
                Ok(client.query_one(&update_stmt, &[&draft_id, &now])?)
            })?;
            Ok(Some(Self::row_to_draft(&row)?))
        })
        .await?
    }

    async fn reject_memory_draft(&self, draft_id: &str, reason: Option<&str>) -> Result<Option<MemoryDraft>> {
        let client = self.client.clone();
        let qualified_drafts_table = self.qualified_drafts_table.clone();
        let draft_id = draft_id.to_string();
        let reason = reason.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<Option<MemoryDraft>> {
            let now = Utc::now();
            let payload = reason.map(|reason| serde_json::json!({ "reason": reason }).to_string());
            let stmt = format!(
                "
                UPDATE {qualified_drafts_table}
                SET
                    status = 'rejected',
                    payload_json = COALESCE($3, payload_json),
                    updated_at = $2
                WHERE draft_id = $1
                RETURNING
                    id, draft_id, workspace_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                "
            );
            let row = client.with_client(|client| Ok(client.query_opt(&stmt, &[&draft_id, &now, &payload])?))?;
            row.as_ref().map(Self::row_to_draft).transpose()
        })
        .await?
    }

    async fn increment_useful_count(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> bool {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || client.with_client(|client| Ok(client.simple_query("SELECT 1").is_ok())))
            .await
            .unwrap_or(Ok(false))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_identifiers_pass_validation() {
        assert!(validate_identifier("public", "schema").is_ok());
        assert!(validate_identifier("_memories_01", "table").is_ok());
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        assert!(validate_identifier("", "schema").is_err());
        assert!(validate_identifier("1bad", "schema").is_err());
        assert!(validate_identifier("bad-name", "table").is_err());
    }

    #[test]
    fn identifier_length_limit_enforced() {
        // Exactly 63 chars — valid
        let max_len = "a".repeat(63);
        assert!(validate_identifier(&max_len, "table").is_ok());

        // 64 chars — exceeds PostgreSQL NAMEDATALEN-1 limit
        let too_long = "a".repeat(64);
        assert!(validate_identifier(&too_long, "table").is_err());

        // SQL injection attempts
        assert!(validate_identifier("table; DROP TABLE users", "table").is_err());
        assert!(validate_identifier("table\"--", "table").is_err());
        assert!(validate_identifier("table\x00name", "table").is_err());
    }

    #[test]
    fn related_table_name_adds_suffix_with_identifier_limit() {
        assert_eq!(related_table_name("memories", "_drafts").unwrap(), "memories_drafts");

        let max_len = "a".repeat(63);
        let related = related_table_name(&max_len, "_drafts").unwrap();
        assert_eq!(related.len(), 63);
        assert!(related.ends_with("_drafts"));
    }

    #[test]
    fn parse_category_maps_known_and_custom_values() {
        assert_eq!(PostgresMemory::parse_category("core"), MemoryCategory::Core);
        assert_eq!(PostgresMemory::parse_category("daily"), MemoryCategory::Daily);
        assert_eq!(
            PostgresMemory::parse_category("conversation"),
            MemoryCategory::Conversation
        );
        assert_eq!(
            PostgresMemory::parse_category("custom_notes"),
            MemoryCategory::Custom("custom_notes".into())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_does_not_panic_inside_tokio_runtime() {
        let outcome = std::panic::catch_unwind(|| {
            PostgresMemory::new(
                "postgres://openprx:password@127.0.0.1:1/openprx",
                "public",
                "memories",
                Some(1),
            )
        });

        assert!(outcome.is_ok(), "PostgresMemory::new should not panic");
        assert!(
            outcome.unwrap().is_err(),
            "PostgresMemory::new should return a connect error for an unreachable endpoint"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_memory_fabric_conformance_from_env() {
        let Ok(db_url) = std::env::var("OPENPRX_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!("prx_test_{}", Uuid::new_v4().simple());
        let table = "memories";
        let mem = PostgresMemory::new(&db_url, &schema, table, Some(5)).unwrap();

        let user = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-user-1".to_string()),
                idempotency_key: Some("idem-user-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                source: "postgres-test".to_string(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("sender-1".to_string()),
                recipient: Some("prx".to_string()),
                role: "user".to_string(),
                content: "hello from postgres".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        let duplicate = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-user-duplicate".to_string()),
                idempotency_key: Some("idem-user-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                source: "postgres-test".to_string(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("sender-1".to_string()),
                recipient: Some("prx".to_string()),
                role: "user".to_string(),
                content: "duplicate should not replace".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        assert_eq!(duplicate.event_id, user.event_id);
        assert_eq!(duplicate.content, user.content);

        let assistant = mem
            .append_message_event(MessageEventInput {
                event_id: Some("event-assistant-1".to_string()),
                idempotency_key: None,
                workspace_id: "workspace-a".to_string(),
                source: "postgres-test".to_string(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                parent_session_key: None,
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                sender: Some("prx".to_string()),
                recipient: Some("sender-1".to_string()),
                role: "assistant".to_string(),
                content: "postgres reply".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();

        let principal = MemoryPrincipal {
            workspace_id: "workspace-a".to_string(),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            channel: Some("telegram".to_string()),
            sender: Some("sender-1".to_string()),
        };
        let events = mem.list_message_events_since(&principal, 0, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.first().map(|event| event.event_id.as_str()),
            Some("event-user-1")
        );
        assert_eq!(
            events.get(1).map(|event| event.event_id.as_str()),
            Some("event-assistant-1")
        );

        let shared = mem
            .load_recent_shared_context(SharedContextQuery {
                principal: principal.clone(),
                since_event_id: None,
                limit: 10,
                include_roles: vec!["assistant".to_string()],
            })
            .await
            .unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(
            shared.first().map(|event| event.event_id.as_str()),
            Some(assistant.event_id.as_str())
        );

        let session = mem
            .load_recent_session_context(SessionContextQuery {
                principal: principal.clone(),
                since_event_id: Some(user.id),
                limit: 10,
                include_roles: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(session.len(), 1);
        assert_eq!(
            session.first().map(|event| event.event_id.as_str()),
            Some("event-assistant-1")
        );

        let outbox_count = mem
            .list_memory_events_since(&principal, 0, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.subject_table == "message_events")
            .count();
        assert_eq!(outbox_count, 2);

        let custom_event = mem
            .append_memory_event(MemoryEventInput {
                event_id: Some("memory-event-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                event_type: "memory.custom".to_string(),
                subject_table: "memories".to_string(),
                subject_id: "subject-1".to_string(),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                visibility: MemoryVisibility::Workspace,
                payload_json: Some("{\"ok\":true}".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(custom_event.event_type, "memory.custom");

        let draft = mem
            .create_memory_draft(MemoryDraftInput {
                draft_id: Some("draft-1".to_string()),
                workspace_id: "workspace-a".to_string(),
                worker_run_id: "worker-run-1".to_string(),
                parent_run_id: Some("parent-run-1".to_string()),
                session_key: Some("telegram_sender-1".to_string()),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                key: "draft-key".to_string(),
                content: "draft content".to_string(),
                category: MemoryCategory::Conversation,
                source_event_id: Some(user.event_id.clone()),
                visibility: MemoryVisibility::Private,
                payload_json: None,
            })
            .await
            .unwrap();
        assert_eq!(draft.status, "pending");
        let merged = mem.merge_memory_draft("draft-1").await.unwrap().unwrap();
        assert_eq!(merged.status, "merged");
        let stored = mem.get("draft-key").await.unwrap().unwrap();
        assert_eq!(stored.content, "draft content");

        drop(mem);
        tokio::task::spawn_blocking(move || {
            let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
                .unwrap();
        })
        .await
        .unwrap();
    }
}

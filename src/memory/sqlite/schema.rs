//! SQLite schema creation and versioned migrations.

use anyhow::Context;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use super::{SqliteMemory, init_approval_grant_schema};

impl SqliteMemory {
    /// Initialize all tables: memories, FTS5, `embedding_cache`
    pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "-- Core memories table
            CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                content     TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'core',
                embedding   BLOB,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions INTEGER,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                workspace_id TEXT,
                owner_id    TEXT,
                agent_id     TEXT,
                persona_id   TEXT,
                source_event_id TEXT,
                source       TEXT,
                channel      TEXT,
                chat_type    TEXT,
                chat_id      TEXT,
                sender_id    TEXT,
                raw_sender   TEXT,
                topic_id     TEXT,
                -- FIX-P2-04: unify the `memories.visibility` default with Postgres
                -- (`'workspace'`). Inserts always set visibility explicitly, so this
                -- only affects rows created without an explicit value.
                visibility   TEXT NOT NULL DEFAULT 'workspace',
                sensitivity  TEXT NOT NULL DEFAULT 'normal',
                risk_signals TEXT DEFAULT '[]',
                policy_version INTEGER DEFAULT 1,
                useful_count INTEGER NOT NULL DEFAULT 0,
                -- Monotonic per-store write counter, maintained by the
                -- `memories_update_seq_*` triggers created after the column
                -- migrations below. It is the only ordering key vector recall
                -- trusts: `updated_at` is a local-offset RFC 3339 string, and
                -- `rowid` never moves when an existing key is refreshed.
                update_seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
            CREATE INDEX IF NOT EXISTS idx_memories_key ON memories(key);
            -- The vector-recall candidate index and the update-sequence
            -- triggers are created after the column migrations below, because
            -- the columns they span are added by ALTER TABLE on stores that
            -- predate them.

            -- FTS5 full-text search (BM25 scoring)
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key, content, content=memories, content_rowid=rowid
            );

            -- FTS5 triggers: keep in sync with memories table
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
            END;
            -- Superseded by `memories_au_fts`: it rewrote the FTS index for
            -- every UPDATE, including updates that touch no indexed column.
            -- That is wasted work for a metadata write, and it is a hard error
            -- for an update issued from inside another trigger on this table
            -- (`memories_update_seq_ai` below): the FTS 'delete' command then
            -- runs against a row whose index entry the insert trigger may not
            -- have written yet, which FTS5 reports as a corrupt external
            -- content table. Renaming rather than redefining keeps
            -- `CREATE TRIGGER IF NOT EXISTS` a no-op on later opens.
            DROP TRIGGER IF EXISTS memories_au;
            CREATE TRIGGER IF NOT EXISTS memories_au_fts AFTER UPDATE ON memories
            WHEN new.key IS NOT old.key OR new.content IS NOT old.content
            BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;

            -- Embedding cache with LRU eviction.
            -- FIX-P0-26: the cache key is the composite (content_hash, provider,
            -- model, dimensions) so the same content cached under different
            -- providers/models/dimensions does not collide (parity with Postgres).
            CREATE TABLE IF NOT EXISTS embedding_cache (
                content_hash TEXT NOT NULL,
                embedding    BLOB NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                model        TEXT NOT NULL DEFAULT '',
                dimensions   INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL,
                PRIMARY KEY (content_hash, provider, model, dimensions)
            );
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS identity_bindings (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                channel         TEXT NOT NULL,
                channel_account TEXT NOT NULL,
                display_name    TEXT,
                bound_at        TEXT NOT NULL,
                bound_by        TEXT NOT NULL,
                UNIQUE(channel, channel_account)
            );
            CREATE INDEX IF NOT EXISTS idx_ib_user ON identity_bindings(user_id);
            CREATE INDEX IF NOT EXISTS idx_ib_channel_account ON identity_bindings(channel, channel_account);

            CREATE TABLE IF NOT EXISTS chat_profiles (
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
            CREATE INDEX IF NOT EXISTS idx_chat_profiles_lookup ON chat_profiles(channel, chat_id);

            CREATE TABLE IF NOT EXISTS agent_identity_bindings (
                binding_id        TEXT PRIMARY KEY,
                external_subject  TEXT NOT NULL,
                external_issuer   TEXT NOT NULL,
                auth_method       TEXT NOT NULL,
                prx_owner_id      TEXT NOT NULL,
                prx_principal_id  TEXT NOT NULL,
                capabilities      TEXT NOT NULL,
                expires_at        TEXT,
                created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_used_at      TEXT,
                UNIQUE (external_issuer, external_subject, auth_method)
            );
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_lookup
                ON agent_identity_bindings(external_issuer, external_subject);
            CREATE INDEX IF NOT EXISTS idx_agent_bindings_owner
                ON agent_identity_bindings(prx_owner_id);

            CREATE TABLE IF NOT EXISTS user_policies (
                user_id             TEXT PRIMARY KEY,
                role                TEXT NOT NULL DEFAULT 'guest',
                projects            TEXT NOT NULL DEFAULT '[]',
                visibility_ceiling  TEXT NOT NULL DEFAULT 'private',
                blocked_patterns    TEXT NOT NULL DEFAULT '[]',
                policy_version      INTEGER NOT NULL DEFAULT 1,
                updated_at          TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS topics (
                id              TEXT PRIMARY KEY,
                title           TEXT NOT NULL,
                project         TEXT,
                external_id     TEXT,
                external_url    TEXT,
                fingerprint     TEXT,
                status          TEXT NOT NULL DEFAULT 'open',
                tags            TEXT DEFAULT '[]',
                summary         TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                resolved_at     TEXT,
                UNIQUE(project, external_id),
                UNIQUE(fingerprint)
            );
            CREATE INDEX IF NOT EXISTS idx_topic_project ON topics(project);
            CREATE INDEX IF NOT EXISTS idx_topic_status ON topics(status);
            CREATE INDEX IF NOT EXISTS idx_topic_external ON topics(external_id);

            CREATE VIRTUAL TABLE IF NOT EXISTS topics_fts
                USING fts5(title, summary, tags, content='topics', content_rowid='rowid');

            CREATE TRIGGER IF NOT EXISTS topics_ai AFTER INSERT ON topics BEGIN
                INSERT INTO topics_fts(rowid, title, summary, tags)
                VALUES (new.rowid, new.title, new.summary, new.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS topics_ad AFTER DELETE ON topics BEGIN
                INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
                VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
            END;

            CREATE TRIGGER IF NOT EXISTS topics_au AFTER UPDATE ON topics BEGIN
                INSERT INTO topics_fts(topics_fts, rowid, title, summary, tags)
                VALUES ('delete', old.rowid, old.title, old.summary, old.tags);
                INSERT INTO topics_fts(rowid, title, summary, tags)
                VALUES (new.rowid, new.title, new.summary, new.tags);
            END;

            CREATE TABLE IF NOT EXISTS topic_participants (
                topic_id    TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                role        TEXT NOT NULL DEFAULT 'participant',
                joined_at   TEXT NOT NULL,
                PRIMARY KEY (topic_id, user_id),
                FOREIGN KEY (topic_id) REFERENCES topics(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS topic_aliases (
                from_topic_id TEXT NOT NULL,
                to_topic_id   TEXT NOT NULL,
                reason        TEXT,
                operator      TEXT NOT NULL,
                created_at    TEXT NOT NULL,
                PRIMARY KEY (from_topic_id),
                FOREIGN KEY (from_topic_id) REFERENCES topics(id) ON DELETE CASCADE,
                FOREIGN KEY (to_topic_id) REFERENCES topics(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS access_audit_log (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                requester   TEXT NOT NULL,
                action      TEXT NOT NULL,
                query       TEXT,
                memory_id   TEXT,
                policy_rule TEXT,
                result      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_time ON access_audit_log(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_requester ON access_audit_log(requester);

            CREATE TABLE IF NOT EXISTS sessions (
                session_key          TEXT PRIMARY KEY,
                channel              TEXT NOT NULL,
                sender               TEXT NOT NULL,
                owner_id             TEXT,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL,
                message_count        INTEGER NOT NULL DEFAULT 0,
                last_message_preview TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_channel_updated_at ON sessions(channel, updated_at DESC);

            CREATE TABLE IF NOT EXISTS conversation_turns (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key      TEXT NOT NULL,
                owner_id         TEXT,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                timestamp        TEXT NOT NULL,
                message_id       TEXT,
                message_event_id TEXT,
                agent_id         TEXT,
                persona_id       TEXT,
                visibility       TEXT,
                FOREIGN KEY (session_key) REFERENCES sessions(session_key) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_session_key ON conversation_turns(session_key);
            CREATE INDEX IF NOT EXISTS idx_conversation_turns_timestamp ON conversation_turns(timestamp DESC);

            CREATE TABLE IF NOT EXISTS message_events (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id           TEXT NOT NULL UNIQUE,
                idempotency_key    TEXT,
                workspace_id       TEXT NOT NULL,
                owner_id           TEXT,
                source             TEXT NOT NULL,
                channel            TEXT,
                session_key        TEXT,
                parent_session_key TEXT,
                run_id             TEXT,
                parent_run_id      TEXT,
                agent_id           TEXT,
                persona_id         TEXT,
                sender             TEXT,
                recipient          TEXT,
                role               TEXT NOT NULL,
                event_type         TEXT NOT NULL,
                source_ref_json    TEXT,
                subject_ref_json   TEXT,
                goal_id            TEXT,
                causation_event_id TEXT,
                correlation_id     TEXT,
                attempt_id         TEXT,
                lease_epoch        INTEGER,
                config_generation_id INTEGER,
                config_source_revision TEXT,
                content            TEXT NOT NULL,
                content_hash       TEXT,
                raw_payload_json   TEXT,
                visibility         TEXT NOT NULL DEFAULT 'workspace',
                created_at         TEXT NOT NULL,
                updated_at         TEXT NOT NULL,
                UNIQUE (workspace_id, idempotency_key)
            );
            CREATE INDEX IF NOT EXISTS idx_message_events_workspace_id
                ON message_events(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_session
                ON message_events(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_agent
                ON message_events(workspace_id, agent_id, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_channel_sender
                ON message_events(workspace_id, channel, sender, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_visibility
                ON message_events(workspace_id, visibility, id);
            CREATE INDEX IF NOT EXISTS idx_message_events_created_at
                ON message_events(created_at);

            CREATE TABLE IF NOT EXISTS memory_events (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id      TEXT NOT NULL UNIQUE,
                workspace_id  TEXT NOT NULL,
                owner_id      TEXT,
                event_type    TEXT NOT NULL,
                subject_table TEXT NOT NULL,
                subject_id    TEXT NOT NULL,
                session_key   TEXT,
                run_id        TEXT,
                parent_run_id TEXT,
                agent_id      TEXT,
                persona_id    TEXT,
                visibility    TEXT NOT NULL DEFAULT 'workspace',
                payload_json  TEXT,
                created_at    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
                ON memory_events(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_type
                ON memory_events(workspace_id, event_type, id);
            CREATE INDEX IF NOT EXISTS idx_memory_events_session
                ON memory_events(workspace_id, session_key, id);
            -- NOTE: idx_memory_events_run / idx_memory_events_parent_run are created
            -- later (after the run_id/parent_run_id ALTER backfill below), because a
            -- legacy `memory_events` table predating schema v7 lacks those columns and
            -- a `CREATE INDEX ... ON memory_events(run_id)` here would fail with
            -- `no such column: run_id` inside this single batch.

            CREATE TABLE IF NOT EXISTS memory_trash (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                trash_id      TEXT NOT NULL UNIQUE,
                memory_key    TEXT NOT NULL,
                content       TEXT NOT NULL,
                category      TEXT NOT NULL,
                reason        TEXT NOT NULL,
                trashed_at    TEXT NOT NULL,
                grace_until   TEXT NOT NULL,
                restored_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_memory_trash_key
                ON memory_trash(memory_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_trash_grace
                ON memory_trash(grace_until) WHERE restored_at IS NULL;

            CREATE TABLE IF NOT EXISTS memory_drafts (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id        TEXT NOT NULL UNIQUE,
                workspace_id    TEXT NOT NULL,
                owner_id        TEXT,
                worker_run_id   TEXT NOT NULL,
                parent_run_id   TEXT,
                session_key     TEXT,
                agent_id        TEXT,
                persona_id      TEXT,
                key             TEXT NOT NULL,
                content         TEXT NOT NULL,
                category        TEXT NOT NULL,
                source_event_id TEXT,
                visibility      TEXT NOT NULL DEFAULT 'workspace',
                status          TEXT NOT NULL DEFAULT 'pending',
                payload_json    TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                ON memory_drafts(worker_run_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                ON memory_drafts(status, id);
            -- NOTE: idx_memory_drafts_source_event is created later (after the
            -- memory_drafts ALTER backfill below). A legacy `memory_drafts` table
            -- predating the `source_event_id` column would make this inline
            -- `CREATE INDEX ... ON memory_drafts(source_event_id)` fail with
            -- `no such column: source_event_id` inside this single batch.

            CREATE TABLE IF NOT EXISTS documents (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                document_id             TEXT NOT NULL UNIQUE,
                workspace_id            TEXT NOT NULL,
                owner_id                TEXT,
                topic_id                TEXT,
                task_id                 TEXT,
                source_message_event_id TEXT,
                source_kind             TEXT NOT NULL,
                source_uri              TEXT,
                title                   TEXT,
                content_sha256          TEXT NOT NULL,
                mime_type               TEXT,
                visibility              TEXT NOT NULL DEFAULT 'workspace',
                metadata_json           TEXT,
                chunk_count             INTEGER NOT NULL DEFAULT 0,
                created_at              TEXT NOT NULL,
                updated_at              TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_documents_workspace
                ON documents(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_owner
                ON documents(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_topic
                ON documents(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_task
                ON documents(workspace_id, task_id, id);
            CREATE INDEX IF NOT EXISTS idx_documents_hash
                ON documents(content_sha256);

            CREATE TABLE IF NOT EXISTS document_chunks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                chunk_id        TEXT NOT NULL UNIQUE,
                document_id     TEXT NOT NULL,
                workspace_id    TEXT NOT NULL,
                owner_id        TEXT,
                topic_id        TEXT,
                task_id         TEXT,
                chunk_index     INTEGER NOT NULL,
                heading         TEXT,
                content         TEXT NOT NULL,
                content_sha256  TEXT NOT NULL,
                embedding       BLOB,
                embedding_provider TEXT,
                embedding_model TEXT,
                embedding_dimensions INTEGER,
                source_anchor   TEXT NOT NULL,
                token_estimate  INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL,
                FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_document_chunks_document
                ON document_chunks(document_id, chunk_index);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_workspace
                ON document_chunks(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_owner
                ON document_chunks(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_topic
                ON document_chunks(workspace_id, topic_id, id);
            CREATE INDEX IF NOT EXISTS idx_document_chunks_task
                ON document_chunks(workspace_id, task_id, id);

            CREATE VIRTUAL TABLE IF NOT EXISTS document_chunks_fts USING fts5(
                content, heading, content='document_chunks', content_rowid='rowid'
            );
            CREATE TRIGGER IF NOT EXISTS document_chunks_ai AFTER INSERT ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(rowid, content, heading)
                VALUES (new.rowid, new.content, new.heading);
            END;
            CREATE TRIGGER IF NOT EXISTS document_chunks_ad AFTER DELETE ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content, heading)
                VALUES ('delete', old.rowid, old.content, old.heading);
            END;
            CREATE TRIGGER IF NOT EXISTS document_chunks_au AFTER UPDATE ON document_chunks BEGIN
                INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content, heading)
                VALUES ('delete', old.rowid, old.content, old.heading);
                INSERT INTO document_chunks_fts(rowid, content, heading)
                VALUES (new.rowid, new.content, new.heading);
            END;

            CREATE TABLE IF NOT EXISTS memory_links (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                link_id          TEXT NOT NULL UNIQUE,
                workspace_id     TEXT NOT NULL,
                owner_id         TEXT,
                memory_key       TEXT,
                memory_event_id  TEXT,
                message_event_id TEXT,
                document_id      TEXT NOT NULL,
                chunk_id         TEXT,
                link_type        TEXT NOT NULL,
                payload_json     TEXT,
                created_at       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_links_workspace
                ON memory_links(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_owner
                ON memory_links(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_memory_key
                ON memory_links(workspace_id, memory_key, id);
            CREATE INDEX IF NOT EXISTS idx_memory_links_document
                ON memory_links(document_id, chunk_id, id);

            CREATE TABLE IF NOT EXISTS retrieval_traces (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                trace_id            TEXT NOT NULL UNIQUE,
                workspace_id        TEXT NOT NULL,
                owner_id            TEXT,
                session_key         TEXT,
                agent_id            TEXT,
                persona_id          TEXT,
                source              TEXT NOT NULL,
                query               TEXT NOT NULL,
                candidate_count     INTEGER NOT NULL,
                selected_count      INTEGER NOT NULL,
                dropped_count       INTEGER NOT NULL,
                budget_tokens       INTEGER,
                selected_json       TEXT,
                dropped_json        TEXT,
                payload_json        TEXT,
                created_at          TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_workspace
                ON retrieval_traces(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_owner
                ON retrieval_traces(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_session
                ON retrieval_traces(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_retrieval_traces_source
                ON retrieval_traces(workspace_id, source, id);

            CREATE TABLE IF NOT EXISTS compaction_runs (
                id                        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id                    TEXT NOT NULL UNIQUE,
                workspace_id              TEXT NOT NULL,
                owner_id                  TEXT,
                session_key               TEXT,
                agent_id                  TEXT,
                persona_id                TEXT,
                trigger                   TEXT NOT NULL,
                mode                      TEXT NOT NULL,
                source_message_count      INTEGER NOT NULL,
                source_token_estimate     INTEGER NOT NULL,
                summary                   TEXT NOT NULL,
                summary_memory_key        TEXT,
                source_event_ids_json     TEXT,
                source_event_range_json   TEXT,
                source_document_refs_json TEXT,
                fidelity_status           TEXT NOT NULL,
                payload_json              TEXT,
                created_at                TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_workspace
                ON compaction_runs(workspace_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_owner
                ON compaction_runs(workspace_id, owner_id, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_session
                ON compaction_runs(workspace_id, session_key, id);
            CREATE INDEX IF NOT EXISTS idx_compaction_runs_trigger
                ON compaction_runs(workspace_id, \"trigger\", id);

            CREATE TABLE IF NOT EXISTS evolution_proposals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id TEXT NOT NULL UNIQUE,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                topic_id TEXT,
                task_id TEXT,
                source_message_event_ids_json TEXT NOT NULL DEFAULT '[]',
                source_memory_event_ids_json TEXT NOT NULL DEFAULT '[]',
                evidence_hashes_json TEXT NOT NULL DEFAULT '[]',
                target_resource_json TEXT NOT NULL,
                proposed_change_json TEXT NOT NULL,
                risk_level TEXT NOT NULL CHECK (risk_level IN ('low','medium','high','critical')),
                mode TEXT NOT NULL CHECK (mode IN ('draft_only','shadow','auto')),
                created_at TEXT NOT NULL,
                created_by_runtime TEXT NOT NULL,
                judge_verdict_json TEXT,
                applied_at TEXT,
                applied_by TEXT,
                rollback_anchor_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_owner_workspace
                ON evolution_proposals(owner_id, workspace_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_pending
                ON evolution_proposals(workspace_id, applied_at) WHERE applied_at IS NULL;
            CREATE INDEX IF NOT EXISTS idx_evolution_proposals_task
                ON evolution_proposals(task_id) WHERE task_id IS NOT NULL;

            CREATE TABLE IF NOT EXISTS evolution_proposal_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                draft_id TEXT NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'proposal.drafted','proposal.judged','proposal.approved',
                    'proposal.rejected','proposal.applied','proposal.rollback','proposal.evidence_mismatch'
                )),
                occurred_at TEXT NOT NULL,
                actor TEXT NOT NULL,
                payload_json TEXT,
                FOREIGN KEY (draft_id) REFERENCES evolution_proposals(draft_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_evolution_proposal_events_draft
                ON evolution_proposal_events(draft_id, occurred_at);",
        )?;
        init_approval_grant_schema(conn)?;

        let mut column_stmt = conn.prepare("PRAGMA table_info(memories)")?;
        let existing_columns = column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut names = std::collections::HashSet::new();
        for column in existing_columns {
            names.insert(column?);
        }

        // Migration: add missing columns for backward compatibility.
        let missing_columns = [
            ("session_id", "ALTER TABLE memories ADD COLUMN session_id TEXT"),
            ("workspace_id", "ALTER TABLE memories ADD COLUMN workspace_id TEXT"),
            ("owner_id", "ALTER TABLE memories ADD COLUMN owner_id TEXT"),
            ("agent_id", "ALTER TABLE memories ADD COLUMN agent_id TEXT"),
            ("persona_id", "ALTER TABLE memories ADD COLUMN persona_id TEXT"),
            (
                "source_event_id",
                "ALTER TABLE memories ADD COLUMN source_event_id TEXT",
            ),
            ("source", "ALTER TABLE memories ADD COLUMN source TEXT"),
            ("channel", "ALTER TABLE memories ADD COLUMN channel TEXT"),
            ("chat_type", "ALTER TABLE memories ADD COLUMN chat_type TEXT"),
            ("chat_id", "ALTER TABLE memories ADD COLUMN chat_id TEXT"),
            ("sender_id", "ALTER TABLE memories ADD COLUMN sender_id TEXT"),
            ("raw_sender", "ALTER TABLE memories ADD COLUMN raw_sender TEXT"),
            ("topic_id", "ALTER TABLE memories ADD COLUMN topic_id TEXT"),
            (
                "visibility",
                // FIX-P2-04: match the CREATE TABLE default and Postgres parity.
                "ALTER TABLE memories ADD COLUMN visibility TEXT NOT NULL DEFAULT 'workspace'",
            ),
            (
                "sensitivity",
                "ALTER TABLE memories ADD COLUMN sensitivity TEXT NOT NULL DEFAULT 'normal'",
            ),
            (
                "risk_signals",
                "ALTER TABLE memories ADD COLUMN risk_signals TEXT DEFAULT '[]'",
            ),
            (
                "policy_version",
                "ALTER TABLE memories ADD COLUMN policy_version INTEGER DEFAULT 1",
            ),
            (
                "useful_count",
                "ALTER TABLE memories ADD COLUMN useful_count INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "embedding_provider",
                "ALTER TABLE memories ADD COLUMN embedding_provider TEXT",
            ),
            (
                "embedding_model",
                "ALTER TABLE memories ADD COLUMN embedding_model TEXT",
            ),
            (
                "embedding_dimensions",
                "ALTER TABLE memories ADD COLUMN embedding_dimensions INTEGER",
            ),
            (
                "update_seq",
                "ALTER TABLE memories ADD COLUMN update_seq INTEGER NOT NULL DEFAULT 0",
            ),
        ];
        for (name, alter_sql) in missing_columns {
            if !names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memories.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memories.{name}: {e}"));
                    }
                }
            }
        }

        // Only now do `memories.embedding_provider` / `embedding_model` /
        // `embedding_dimensions` exist on every store: a database created by an
        // older build (or by `snapshot::hydrate_from_snapshot`, which writes a
        // minimal `memories` table) gets them from the ALTER TABLE block above,
        // so an index spanning them cannot live in the CREATE TABLE batch.
        conn.execute_batch(
            // Backfill: every row written before `update_seq` existed takes its
            // `rowid`, which is the insertion order the 0.8.126 window used, so
            // an untouched legacy store keeps the exact candidate set it had.
            // `rowid` is >= 1, so a backfilled row is never confused with the
            // column default again, and the next write continues above the
            // highest backfilled value. The `WHERE` clause makes this a no-op on
            // every later open, and the trigger below does not fire for it
            // because it changes `update_seq` itself.
            "UPDATE memories SET update_seq = rowid WHERE update_seq = 0;

             -- Superseded index: it led with `updated_at`, a local-offset RFC
             -- 3339 string whose lexicographic order is not chronological across
             -- timezone or DST changes, and it carried none of the
             -- embedding-identity columns every vector query filters on, so a
             -- store whose embedding model had changed scanned past large runs
             -- of non-matching rows to fill the candidate set.
             DROP INDEX IF EXISTS idx_memories_embedding_recency;
             -- Superseded index: same three leading columns, but without
             -- `update_seq` it could only order the window by `rowid`, which
             -- does not move when `ON CONFLICT(key) DO UPDATE` refreshes an
             -- existing key. A just-refreshed old memory therefore stayed
             -- outside the window. Renaming the index rather than redefining it
             -- keeps `CREATE INDEX IF NOT EXISTS` a no-op on later opens instead
             -- of rebuilding the index on every process start.
             DROP INDEX IF EXISTS idx_memories_embedding_candidates;
             -- Leads with the three columns `vector_search` always
             -- equality-filters and ends with the ordering key, so the
             -- newest-first candidate window is a backward index scan with no
             -- sort and no scan of rows belonging to another embedding model.
             CREATE INDEX IF NOT EXISTS idx_memories_embedding_candidates_seq
                 ON memories(embedding_provider, embedding_model, embedding_dimensions, update_seq)
                 WHERE embedding IS NOT NULL;
             -- Serves the `MAX(update_seq)` lookup in the triggers below as a
             -- single covering-index descent instead of a table scan per write.
             CREATE INDEX IF NOT EXISTS idx_memories_update_seq ON memories(update_seq);

             -- The update sequence is maintained in the schema, not in the call
             -- sites: `memories` is written by upserts here, by metadata and
             -- useful-count updates, by draft merges and by the webhook and
             -- fitness writers. A trigger is the one place that cannot be
             -- forgotten by a new writer, and it also covers rows an older
             -- build wrote without it.
             --
             -- Both bodies update only `update_seq`, which is why
             -- `memories_au_fts` is guarded on `key`/`content`: an unguarded
             -- FTS sync trigger would fire for these updates and corrupt the
             -- external content index.
             CREATE TRIGGER IF NOT EXISTS memories_update_seq_ai
             AFTER INSERT ON memories
             BEGIN
                 UPDATE memories
                    SET update_seq = (SELECT COALESCE(MAX(update_seq), 0) + 1 FROM memories)
                  WHERE rowid = new.rowid;
             END;
             -- The `WHEN` guard is what makes the body's own UPDATE terminate:
             -- it raises `update_seq`, so a re-entry (which only happens under
             -- `PRAGMA recursive_triggers = ON`) sees new.update_seq <>
             -- old.update_seq and does nothing. It also leaves the backfill
             -- above and any deliberate sequence rewrite alone.
             CREATE TRIGGER IF NOT EXISTS memories_update_seq_au
             AFTER UPDATE ON memories
             WHEN new.update_seq = old.update_seq
             BEGIN
                 UPDATE memories
                    SET update_seq = (SELECT COALESCE(MAX(update_seq), 0) + 1 FROM memories)
                  WHERE rowid = new.rowid;
             END;",
        )?;

        let mut chunk_column_stmt = conn.prepare("PRAGMA table_info(document_chunks)")?;
        let existing_chunk_columns = chunk_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut chunk_names = std::collections::HashSet::new();
        for column in existing_chunk_columns {
            chunk_names.insert(column?);
        }
        for (name, alter_sql) in [
            ("embedding", "ALTER TABLE document_chunks ADD COLUMN embedding BLOB"),
            (
                "embedding_provider",
                "ALTER TABLE document_chunks ADD COLUMN embedding_provider TEXT",
            ),
            (
                "embedding_model",
                "ALTER TABLE document_chunks ADD COLUMN embedding_model TEXT",
            ),
            (
                "embedding_dimensions",
                "ALTER TABLE document_chunks ADD COLUMN embedding_dimensions INTEGER",
            ),
        ] {
            if !chunk_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column document_chunks.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add document_chunks.{name}: {e}"));
                    }
                }
            }
        }

        let mut cache_column_stmt = conn.prepare("PRAGMA table_info(embedding_cache)")?;
        let existing_cache_columns = cache_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut cache_names = std::collections::HashSet::new();
        for column in existing_cache_columns {
            cache_names.insert(column?);
        }
        for (name, alter_sql) in [
            ("provider", "ALTER TABLE embedding_cache ADD COLUMN provider TEXT"),
            ("model", "ALTER TABLE embedding_cache ADD COLUMN model TEXT"),
            (
                "dimensions",
                "ALTER TABLE embedding_cache ADD COLUMN dimensions INTEGER",
            ),
        ] {
            if !cache_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column embedding_cache.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add embedding_cache.{name}: {e}"));
                    }
                }
            }
        }

        // FIX-P0-26: upgrade a legacy single-column `embedding_cache` primary key
        // (`content_hash`) to the composite `(content_hash, provider, model,
        // dimensions)` so different providers/models/dimensions for the same
        // content no longer collide. SQLite cannot alter a primary key in place,
        // so the table is rebuilt when the legacy PK is detected.
        Self::upgrade_embedding_cache_primary_key(&conn)?;

        let mut msg_column_stmt = conn.prepare("PRAGMA table_info(message_events)")?;
        let existing_msg_columns = msg_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut msg_names = std::collections::HashSet::new();
        for column in existing_msg_columns {
            msg_names.insert(column?);
        }
        if !msg_names.contains("owner_id") {
            match conn.execute_batch("ALTER TABLE message_events ADD COLUMN owner_id TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.owner_id already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.owner_id: {e}")),
            }
        }
        if !msg_names.contains("event_type") {
            match conn.execute_batch("ALTER TABLE message_events ADD COLUMN event_type TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.event_type already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.event_type: {e}")),
            }
        }
        for (name, sql_type) in [
            ("source_ref_json", "TEXT"),
            ("subject_ref_json", "TEXT"),
            ("goal_id", "TEXT"),
            ("causation_event_id", "TEXT"),
            ("correlation_id", "TEXT"),
            ("attempt_id", "TEXT"),
            ("lease_epoch", "INTEGER"),
            ("config_generation_id", "INTEGER"),
            ("config_source_revision", "TEXT"),
        ] {
            if msg_names.contains(name) {
                continue;
            }
            let alter_sql = format!("ALTER TABLE message_events ADD COLUMN {name} {sql_type}");
            match conn.execute_batch(&alter_sql) {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column message_events.{name} already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add message_events.{name}: {e}")),
            }
        }
        Self::upgrade_message_event_idempotency_scope(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_message_events_event_type
                ON message_events(workspace_id, event_type, id);
             CREATE INDEX IF NOT EXISTS idx_message_events_correlation
                ON message_events(workspace_id, correlation_id, id);
             CREATE INDEX IF NOT EXISTS idx_message_events_config_generation
                ON message_events(workspace_id, config_generation_id, id);",
        )?;

        let mut session_column_stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let existing_session_columns = session_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut session_names = std::collections::HashSet::new();
        for column in existing_session_columns {
            session_names.insert(column?);
        }
        if !session_names.contains("owner_id") {
            match conn.execute_batch("ALTER TABLE sessions ADD COLUMN owner_id TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!("Column sessions.owner_id already exists (concurrent migration): {err}");
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to add sessions.owner_id: {e}")),
            }
        }

        let mut turn_column_stmt = conn.prepare("PRAGMA table_info(conversation_turns)")?;
        let existing_turn_columns = turn_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut turn_names = std::collections::HashSet::new();
        for column in existing_turn_columns {
            turn_names.insert(column?);
        }
        let missing_turn_columns = [
            ("owner_id", "ALTER TABLE conversation_turns ADD COLUMN owner_id TEXT"),
            (
                "message_event_id",
                "ALTER TABLE conversation_turns ADD COLUMN message_event_id TEXT",
            ),
            ("agent_id", "ALTER TABLE conversation_turns ADD COLUMN agent_id TEXT"),
            (
                "persona_id",
                "ALTER TABLE conversation_turns ADD COLUMN persona_id TEXT",
            ),
            (
                "visibility",
                "ALTER TABLE conversation_turns ADD COLUMN visibility TEXT",
            ),
        ];
        for (name, alter_sql) in missing_turn_columns {
            if !turn_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!(
                            "Column conversation_turns.{name} already exists (concurrent migration): {err}"
                        );
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add conversation_turns.{name}: {e}"));
                    }
                }
            }
        }

        let mut draft_column_stmt = conn.prepare("PRAGMA table_info(memory_drafts)")?;
        let existing_draft_columns = draft_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut draft_names = std::collections::HashSet::new();
        for column in existing_draft_columns {
            draft_names.insert(column?);
        }
        let missing_draft_columns = [
            ("owner_id", "ALTER TABLE memory_drafts ADD COLUMN owner_id TEXT"),
            (
                "parent_run_id",
                "ALTER TABLE memory_drafts ADD COLUMN parent_run_id TEXT",
            ),
            ("agent_id", "ALTER TABLE memory_drafts ADD COLUMN agent_id TEXT"),
            ("persona_id", "ALTER TABLE memory_drafts ADD COLUMN persona_id TEXT"),
            (
                "source_event_id",
                "ALTER TABLE memory_drafts ADD COLUMN source_event_id TEXT",
            ),
            (
                "visibility",
                "ALTER TABLE memory_drafts ADD COLUMN visibility TEXT NOT NULL DEFAULT 'workspace'",
            ),
            ("payload_json", "ALTER TABLE memory_drafts ADD COLUMN payload_json TEXT"),
        ];
        for (name, alter_sql) in missing_draft_columns {
            if !draft_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memory_drafts.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memory_drafts.{name}: {e}"));
                    }
                }
            }
        }

        let mut compaction_column_stmt = conn.prepare("PRAGMA table_info(compaction_runs)")?;
        let existing_compaction_columns = compaction_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut compaction_names = std::collections::HashSet::new();
        for column in existing_compaction_columns {
            compaction_names.insert(column?);
        }
        if !compaction_names.contains("source_event_range_json") {
            match conn.execute_batch("ALTER TABLE compaction_runs ADD COLUMN source_event_range_json TEXT") {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
                    tracing::debug!(
                        "Column compaction_runs.source_event_range_json already exists (concurrent migration): {err}"
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to add compaction_runs.source_event_range_json: {e}"
                    ));
                }
            }
        }

        // Schema v7 (`memory_events_run_lineage`) added `run_id` / `parent_run_id`
        // to `memory_events`. A legacy table created under v4 lacks these columns,
        // and the checksum-anchor registry alone never alters an existing table, so
        // the gateway hit `no such column: run_id` at runtime. Backfill the columns
        // idempotently (mirrors the `memories` / `memory_drafts` blocks above), then
        // create the lineage indexes — both must run *after* the ALTERs.
        let mut event_column_stmt = conn.prepare("PRAGMA table_info(memory_events)")?;
        let existing_event_columns = event_column_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut event_names = std::collections::HashSet::new();
        for column in existing_event_columns {
            event_names.insert(column?);
        }
        let missing_event_columns = [
            ("run_id", "ALTER TABLE memory_events ADD COLUMN run_id TEXT"),
            (
                "parent_run_id",
                "ALTER TABLE memory_events ADD COLUMN parent_run_id TEXT",
            ),
            // Schema v16 (C03 / V02-R4): owner attribution for memory events, so
            // an owner-isolated principal can be denied another owner's
            // `session`/`private` rows. Existing rows keep a NULL owner and are
            // never rewritten; NULL means unattributed.
            ("owner_id", "ALTER TABLE memory_events ADD COLUMN owner_id TEXT"),
        ];
        for (name, alter_sql) in missing_event_columns {
            if !event_names.contains(name) {
                match conn.execute_batch(alter_sql) {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
                        if msg.contains("duplicate column name") =>
                    {
                        tracing::debug!("Column memory_events.{name} already exists (concurrent migration): {err}");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to add memory_events.{name}: {e}"));
                    }
                }
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memory_events_run
                 ON memory_events(workspace_id, run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_events_parent_run
                 ON memory_events(workspace_id, parent_run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_events_owner
                 ON memory_events(workspace_id, owner_id, id);",
        )?;

        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);
             CREATE INDEX IF NOT EXISTS idx_mem_owner ON memories(owner_id);
             CREATE INDEX IF NOT EXISTS idx_mem_vis_chan_type_chat
                 ON memories(visibility, channel, chat_type, chat_id, sensitivity, created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_mem_sender ON memories(sender_id);
             CREATE INDEX IF NOT EXISTS idx_mem_topic_time ON memories(topic_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_mem_channel ON memories(channel);
             CREATE INDEX IF NOT EXISTS idx_mem_workspace_agent ON memories(workspace_id, agent_id, persona_id);
             CREATE INDEX IF NOT EXISTS idx_mem_source_event ON memories(source_event_id);
             CREATE INDEX IF NOT EXISTS idx_message_events_owner
                 ON message_events(workspace_id, owner_id, id);
             CREATE INDEX IF NOT EXISTS idx_sessions_owner
                 ON sessions(owner_id);
             CREATE INDEX IF NOT EXISTS idx_conversation_turns_owner_session
                 ON conversation_turns(owner_id, session_key);
             CREATE INDEX IF NOT EXISTS idx_conversation_turns_message_event
                 ON conversation_turns(message_event_id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_worker_run
                 ON memory_drafts(worker_run_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_owner
                 ON memory_drafts(workspace_id, owner_id, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_status
                 ON memory_drafts(status, id);
             CREATE INDEX IF NOT EXISTS idx_memory_drafts_source_event
                 ON memory_drafts(source_event_id);",
        )?;

        conn.execute_batch(
            "UPDATE sessions
             SET owner_id = (
                 SELECT me.owner_id
                 FROM message_events me
                 WHERE me.session_key = sessions.session_key
                   AND me.owner_id IS NOT NULL
                 ORDER BY me.id ASC
                 LIMIT 1
             )
             WHERE owner_id IS NULL;
             UPDATE sessions
             SET owner_id = 'legacy:' || session_key
             WHERE owner_id IS NULL;
             UPDATE conversation_turns
             SET owner_id = (
                 SELECT s.owner_id
                 FROM sessions s
                 WHERE s.session_key = conversation_turns.session_key
             )
             WHERE owner_id IS NULL;
             UPDATE conversation_turns
             SET owner_id = 'legacy:' || session_key
             WHERE owner_id IS NULL;",
        )?;

        // FIX-P2-04: normalize legacy rows whose `visibility` was left implicitly
        // NULL or empty so the column always carries the unified 'workspace'
        // default. Explicit values (including an intentional 'private') are left
        // untouched — this never downgrades a stricter visibility.
        conn.execute(
            "UPDATE memories SET visibility = 'workspace' \
             WHERE visibility IS NULL OR visibility = ''",
            [],
        )?;

        Self::run_memory_schema_migrations(conn)?;

        Ok(())
    }

    /// FIX-P0-26: rebuild a legacy `embedding_cache` whose primary key is the
    /// single `content_hash` column into one keyed by the composite
    /// `(content_hash, provider, model, dimensions)`.
    ///
    /// Detection uses `PRAGMA table_info`: if exactly one column has a non-zero
    /// `pk` ordinal the table predates the composite key and is rebuilt. The
    /// rebuild coalesces legacy NULL provider/model/dimensions to the same
    /// defaults the fresh schema uses and de-duplicates on the composite key,
    /// keeping the most recently accessed row.
    fn upgrade_embedding_cache_primary_key(conn: &Connection) -> anyhow::Result<()> {
        let pk_columns: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('embedding_cache') WHERE pk > 0",
            [],
            |row| row.get(0),
        )?;
        // 0 ⇒ table missing (fresh CREATE handles it); >1 ⇒ already composite.
        if pk_columns != 1 {
            return Ok(());
        }

        // De-duplicate on the composite key first: legacy rows could differ only
        // by NULL vs '' provider/model/dimensions that collapse to the same
        // normalized key. `GROUP BY` keeps one row per composite key (the one with
        // the most recent `accessed_at` via MAX), avoiding a UNIQUE violation that
        // `INSERT OR REPLACE` does not reliably resolve inside `execute_batch`.
        conn.execute_batch(
            "CREATE TABLE embedding_cache_v2 (
                content_hash TEXT NOT NULL,
                embedding    BLOB NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                model        TEXT NOT NULL DEFAULT '',
                dimensions   INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL,
                PRIMARY KEY (content_hash, provider, model, dimensions)
            );
            INSERT INTO embedding_cache_v2 (
                content_hash, embedding, provider, model, dimensions, created_at, accessed_at
            )
            SELECT content_hash, embedding, norm_provider, norm_model, norm_dimensions,
                   created_at, accessed_at
            FROM (
                SELECT content_hash, embedding,
                       COALESCE(provider, '') AS norm_provider,
                       COALESCE(model, '')    AS norm_model,
                       COALESCE(dimensions, 0) AS norm_dimensions,
                       created_at, accessed_at,
                       ROW_NUMBER() OVER (
                           PARTITION BY content_hash, COALESCE(provider, ''),
                                        COALESCE(model, ''), COALESCE(dimensions, 0)
                           ORDER BY accessed_at DESC
                       ) AS rn
                FROM embedding_cache
            )
            WHERE rn = 1;
            DROP TABLE embedding_cache;
            ALTER TABLE embedding_cache_v2 RENAME TO embedding_cache;
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;
        Ok(())
    }

    /// Rebuild legacy `message_events` tables whose `idempotency_key` was
    /// globally unique. Idempotency belongs to a workspace boundary: two
    /// independent workspaces may legitimately receive the same external
    /// provider key, while retries inside one workspace must still converge.
    fn upgrade_message_event_idempotency_scope(conn: &Connection) -> anyhow::Result<()> {
        let mut index_stmt = conn.prepare("PRAGMA index_list('message_events')")?;
        let indexes = index_stmt.query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))?;
        for index in indexes {
            let (index_name, unique) = index?;
            if unique == 0 {
                continue;
            }
            let mut columns_stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
            let columns = columns_stmt
                .query_map([index_name], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if columns == ["workspace_id", "idempotency_key"] {
                return Ok(());
            }
        }

        let rebuild = conn.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE message_events RENAME TO message_events_legacy_idempotency_scope;
             CREATE TABLE message_events (
                 id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id           TEXT NOT NULL UNIQUE,
                 idempotency_key    TEXT,
                 workspace_id       TEXT NOT NULL,
                 owner_id           TEXT,
                 source             TEXT NOT NULL,
                 channel            TEXT,
                 session_key        TEXT,
                 parent_session_key TEXT,
                 run_id             TEXT,
                 parent_run_id      TEXT,
                 agent_id           TEXT,
                 persona_id         TEXT,
                 sender             TEXT,
                 recipient          TEXT,
                 role               TEXT NOT NULL,
                 event_type         TEXT NOT NULL,
                 source_ref_json    TEXT,
                 subject_ref_json   TEXT,
                 goal_id            TEXT,
                 causation_event_id TEXT,
                 correlation_id     TEXT,
                 attempt_id         TEXT,
                 lease_epoch        INTEGER,
                 config_generation_id INTEGER,
                 config_source_revision TEXT,
                 content            TEXT NOT NULL,
                 content_hash       TEXT,
                 raw_payload_json   TEXT,
                 visibility         TEXT NOT NULL DEFAULT 'workspace',
                 created_at         TEXT NOT NULL,
                 updated_at         TEXT NOT NULL,
                 UNIQUE (workspace_id, idempotency_key)
             );
             INSERT INTO message_events (
                 id, event_id, idempotency_key, workspace_id, owner_id, source, channel,
                 session_key, parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                 sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                 goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch,
                 config_generation_id, config_source_revision,
                 content, content_hash, raw_payload_json, visibility, created_at, updated_at
             )
             SELECT
                 id, event_id, idempotency_key, workspace_id, owner_id, source, channel,
                 session_key, parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                 sender, recipient, role, COALESCE(event_type, 'message.legacy'),
                 source_ref_json, subject_ref_json, goal_id, causation_event_id,
                 correlation_id, attempt_id, lease_epoch, config_generation_id,
                 config_source_revision, content, content_hash,
                 raw_payload_json, visibility, created_at, updated_at
             FROM message_events_legacy_idempotency_scope;
             DROP TABLE message_events_legacy_idempotency_scope;
             CREATE INDEX idx_message_events_workspace_id
                 ON message_events(workspace_id, id);
             CREATE INDEX idx_message_events_owner
                 ON message_events(workspace_id, owner_id, id);
             CREATE INDEX idx_message_events_session
                 ON message_events(workspace_id, session_key, id);
             CREATE INDEX idx_message_events_agent
                 ON message_events(workspace_id, agent_id, id);
             CREATE INDEX idx_message_events_channel_sender
                 ON message_events(workspace_id, channel, sender, id);
             CREATE INDEX idx_message_events_visibility
                 ON message_events(workspace_id, visibility, id);
             CREATE INDEX idx_message_events_event_type
                 ON message_events(workspace_id, event_type, id);
             CREATE INDEX idx_message_events_correlation
                 ON message_events(workspace_id, correlation_id, id);
             CREATE INDEX idx_message_events_created_at
                 ON message_events(created_at);
             CREATE INDEX idx_message_events_config_generation
                 ON message_events(workspace_id, config_generation_id, id);
             COMMIT;",
        );
        if let Err(error) = rebuild {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(error).context("failed to scope message event idempotency to workspace");
        }
        Ok(())
    }

    /// Record-and-verify versioned schema migrations (FIX-P0-25).
    ///
    /// The legacy `init_schema` above creates / upgrades tables with idempotent
    /// `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ... ADD COLUMN` statements and
    /// previously swallowed ALTER failures via `tracing::debug`. This ledger gives
    /// every logical schema step a stable `version`, a `name`, and a SHA-256
    /// `checksum` of its canonical descriptor, so drift is detected at startup:
    ///
    /// - already-applied version, checksum matches → skipped;
    /// - already-applied version, checksum differs → `bail!` (fail-fast: an
    ///   engineer changed a registered step without bumping its version);
    /// - unapplied version → recorded (the DDL itself already ran above).
    ///
    /// New schema changes MUST append a new `(version, name, sql)` entry to
    /// [`Self::memory_schema_migration_registry`] and NEVER mutate an existing
    /// entry's text.
    pub(super) fn run_memory_schema_migrations(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_schema_migrations (
                version    INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                checksum   TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );",
        )?;

        for (version, name, sql) in Self::memory_schema_migration_registry() {
            let checksum = Self::schema_migration_checksum(sql);
            let recorded: Option<String> = conn
                .query_row(
                    "SELECT checksum FROM memory_schema_migrations WHERE version = ?1",
                    params![version],
                    |row| row.get(0),
                )
                .optional()?;
            match recorded {
                Some(recorded) => {
                    if recorded != checksum {
                        anyhow::bail!(
                            "memory schema migration checksum mismatch for version {version} ({name}): \
                             expected {checksum}, found {recorded}"
                        );
                    }
                }
                None => {
                    conn.execute(
                        "INSERT INTO memory_schema_migrations (version, name, checksum, applied_at) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![version, name, checksum, Utc::now().to_rfc3339()],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// SHA-256 (hex) of a migration's canonical descriptor text.
    pub(crate) fn schema_migration_checksum(sql: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(sql.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Versioned registry of the canonical schema steps created by `init_schema`,
    /// retro-fitted in execution order. Each text is a stable canonical descriptor
    /// used only as a checksum anchor (version unchanged ⇒ text unchanged).
    pub(crate) const fn memory_schema_migration_registry() -> &'static [(i64, &'static str, &'static str)] {
        &[
            (
                1,
                "memories_core",
                "memories(id,key,content,category,embedding,embedding_provider,embedding_model,embedding_dimensions,created_at,updated_at,workspace_id,owner_id,agent_id,persona_id,source_event_id,source,channel,chat_type,chat_id,sender_id,raw_sender,topic_id,visibility,sensitivity,risk_signals,policy_version,useful_count) + memories_fts + embedding_cache",
            ),
            (
                2,
                "identity_topics_audit",
                "identity_bindings + agent_identity_bindings + user_policies + topics + topics_fts + topic_participants + topic_aliases + access_audit_log",
            ),
            (
                3,
                "sessions_and_turns",
                "sessions(session_key,channel,sender,owner_id,created_at,updated_at,message_count,last_message_preview) + conversation_turns(id,session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,persona_id,visibility)",
            ),
            (
                4,
                "message_and_memory_events",
                "message_events(id,event_id,idempotency_key,workspace_id,owner_id,source,channel,session_key,parent_session_key,run_id,parent_run_id,agent_id,persona_id,sender,recipient,role,event_type,content,content_hash,raw_payload_json,visibility,created_at,updated_at) + memory_events(id,event_id,workspace_id,event_type,subject_table,subject_id,session_key,agent_id,persona_id,visibility,payload_json,created_at)",
            ),
            (
                5,
                "memory_drafts",
                "memory_drafts(id,draft_id,workspace_id,owner_id,worker_run_id,parent_run_id,session_key,agent_id,persona_id,key,content,category,source_event_id,visibility,status,payload_json,created_at,updated_at)",
            ),
            (
                6,
                "documents_links_traces_compaction",
                "documents + document_chunks + memory_links + retrieval_traces + compaction_runs + evolution_proposals + evolution_proposal_events",
            ),
            (
                7,
                "memory_events_run_lineage",
                "memory_events + run_id + parent_run_id + idx_memory_events_run + idx_memory_events_parent_run",
            ),
            (
                8,
                "memory_trash",
                "memory_trash(id,trash_id,memory_key,content,category,reason,trashed_at,grace_until,restored_at) + idx_memory_trash_key + idx_memory_trash_grace",
            ),
            (
                9,
                "memories_visibility_default_workspace",
                // FIX-P2-04: the `memories.visibility` column default is unified to
                // 'workspace' (CREATE TABLE + ALTER) for parity with Postgres. The
                // backfill below normalizes legacy rows whose visibility was left
                // implicitly NULL/empty; it never downgrades an explicit value.
                "memories.visibility default 'workspace' (parity with postgres) + backfill NULL/'' -> 'workspace'",
            ),
            (
                10,
                "embedding_cache_composite_primary_key",
                // FIX-P0-26: embedding_cache primary key upgraded from single
                // `content_hash` to composite `(content_hash, provider, model,
                // dimensions)` (parity with Postgres). The legacy table is rebuilt
                // in-place by `upgrade_embedding_cache_primary_key`.
                "embedding_cache PRIMARY KEY (content_hash, provider, model, dimensions) + rebuild legacy single-key table",
            ),
            (
                11,
                "approval_grant_revocations",
                // FIX-P3-06: out-of-band grant revocation ledger. A grant can be
                // revoked without rewriting its signed payload by inserting a row
                // here; the gate consults this table in addition to the in-grant
                // `revoked_at` field. Created by `init_approval_grant_schema`.
                "approval_grant_revocations(grant_id,revoked_at,reason) + idx_approval_grant_revocations_revoked_at",
            ),
            (
                12,
                "compaction_source_event_range",
                "compaction_runs.source_event_ids_json contains only real MessageEvent event_id strings + source_event_range_json(first_event_id,last_event_id,first_row_id,last_row_id,source_event_count)",
            ),
            (
                13,
                "message_event_workspace_idempotency",
                "message_events UNIQUE(workspace_id,idempotency_key) replaces global UNIQUE(idempotency_key) + workspace-scoped conflict lookup",
            ),
            (
                14,
                "message_event_config_generation",
                "message_events + config_generation_id + config_source_revision + idx_message_events_config_generation",
            ),
            (
                15,
                "message_event_execution_metadata",
                "message_events + source_ref_json + subject_ref_json + goal_id + causation_event_id + correlation_id + attempt_id + lease_epoch",
            ),
            (
                16,
                "memory_event_owner_isolation",
                // C03 / V02-R4: `memory_events` gains `owner_id` so an
                // externally authenticated (owner-isolated) principal can be
                // refused another owner's `session`/`private` rows. Added by an
                // idempotent ALTER; legacy rows keep NULL (unattributed).
                "memory_events + owner_id + idx_memory_events_owner",
            ),
        ]
    }
}

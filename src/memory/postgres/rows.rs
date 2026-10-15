//! Row mappers, category helpers and SQL identifier validation.
use super::PostgresMemory;
use crate::memory::traits::{
    ChatProfile, CompactionRun, DocumentChunkRecord, DocumentRecord, MemoryCategory, MemoryDraft, MemoryEntry,
    MemoryEvent, MemoryLink, MemoryVisibility, MessageEvent, RetrievalTrace,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use postgres::Row;
use sha2::{Digest, Sha256};

pub(crate) fn validate_identifier(value: &str, field_name: &str) -> Result<()> {
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

pub(crate) fn related_table_name(base: &str, suffix: &str) -> Result<String> {
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

pub(crate) fn quote_identifier(value: &str) -> String {
    format!("\"{value}\"")
}

pub(super) fn chat_profile_from_pg_row(row: &Row) -> ChatProfile {
    let tags_json: String = row.get(7);
    let tags = serde_json::from_str::<Vec<String>>(&tags_json).unwrap_or_default();
    ChatProfile {
        id: row.get(0),
        channel: row.get(1),
        chat_id: row.get(2),
        chat_kind: row.get(3),
        title: row.get(4),
        purpose: row.get(5),
        notes: row.get(6),
        tags,
        updated_by: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
    }
}

impl PostgresMemory {
    pub(super) fn category_to_str(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "core".to_string(),
            MemoryCategory::Daily => "daily".to_string(),
            MemoryCategory::Conversation => "conversation".to_string(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    pub(super) fn content_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub(super) fn content_sha256_hex(content: &str) -> String {
        Self::content_hash(content)
    }

    pub(super) fn parse_category(value: &str) -> MemoryCategory {
        match value {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    pub(super) fn row_to_entry(row: &Row) -> Result<MemoryEntry> {
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

    pub(super) fn row_to_message_event(row: &Row) -> Result<MessageEvent> {
        let source_legacy: String = row.get(5);
        let source_ref_json: Option<String> = row.get(17);
        let source = source_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| source_legacy.into());
        let subject_ref_json: Option<String> = row.get(18);
        let subject = subject_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let created_at: DateTime<Utc> = row.get(30);
        let updated_at: DateTime<Utc> = row.get(31);
        let visibility = row
            .get::<_, String>(29)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MessageEvent {
            id: row.get(0),
            event_id: row.get(1),
            idempotency_key: row.get(2),
            workspace_id: row.get(3),
            owner_id: row.get(4),
            source,
            channel: row.get(6),
            session_key: row.get(7),
            parent_session_key: row.get(8),
            run_id: row.get(9),
            parent_run_id: row.get(10),
            agent_id: row.get(11),
            persona_id: row.get(12),
            sender: row.get(13),
            recipient: row.get(14),
            role: row.get(15),
            event_type: row
                .get::<_, Option<String>>(16)
                .unwrap_or_else(|| "message.legacy".to_string()),
            subject,
            goal_id: row.get(19),
            causation_event_id: row.get(20),
            correlation_id: row.get(21),
            attempt_id: row.get(22),
            lease_epoch: row.get(23),
            config_generation_id: row
                .get::<_, Option<i64>>(24)
                .and_then(|value| u64::try_from(value).ok()),
            config_source_revision: row.get(25),
            content: row.get(26),
            content_hash: row.get(27),
            raw_payload_json: row.get(28),
            visibility,
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_memory_event(row: &Row) -> Result<MemoryEvent> {
        let created_at: DateTime<Utc> = row.get(14);
        let visibility = row
            .get::<_, String>(12)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryEvent {
            id: row.get(0),
            event_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            event_type: row.get(4),
            subject_table: row.get(5),
            subject_id: row.get(6),
            session_key: row.get(7),
            run_id: row.get(8),
            parent_run_id: row.get(9),
            agent_id: row.get(10),
            persona_id: row.get(11),
            visibility,
            payload_json: row.get(13),
            created_at: created_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_draft(row: &Row) -> Result<MemoryDraft> {
        let created_at: DateTime<Utc> = row.get(16);
        let updated_at: DateTime<Utc> = row.get(17);
        let visibility = row
            .get::<_, String>(13)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);

        Ok(MemoryDraft {
            id: row.get(0),
            draft_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            worker_run_id: row.get(4),
            parent_run_id: row.get(5),
            session_key: row.get(6),
            agent_id: row.get(7),
            persona_id: row.get(8),
            key: row.get(9),
            content: row.get(10),
            category: Self::parse_category(&row.get::<_, String>(11)),
            source_event_id: row.get(12),
            visibility,
            status: row.get(14),
            payload_json: row.get(15),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_document(row: &Row) -> Result<DocumentRecord> {
        let created_at: DateTime<Utc> = row.get(15);
        let updated_at: DateTime<Utc> = row.get(16);
        let visibility = row
            .get::<_, String>(12)
            .parse::<MemoryVisibility>()
            .unwrap_or(MemoryVisibility::Workspace);
        let chunk_count: i64 = row.get(14);

        Ok(DocumentRecord {
            id: row.get(0),
            document_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            topic_id: row.get(4),
            task_id: row.get(5),
            source_message_event_id: row.get(6),
            source_kind: row.get(7),
            source_uri: row.get(8),
            title: row.get(9),
            content_sha256: row.get(10),
            mime_type: row.get(11),
            visibility,
            metadata_json: row.get(13),
            chunk_count: usize::try_from(chunk_count.max(0)).unwrap_or(usize::MAX),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_document_chunk(row: &Row) -> Result<DocumentChunkRecord> {
        let created_at: DateTime<Utc> = row.get(13);
        let chunk_index: i64 = row.get(7);
        let token_estimate: i64 = row.get(12);
        Ok(DocumentChunkRecord {
            id: row.get(0),
            chunk_id: row.get(1),
            document_id: row.get(2),
            workspace_id: row.get(3),
            owner_id: row.get(4),
            topic_id: row.get(5),
            task_id: row.get(6),
            chunk_index: usize::try_from(chunk_index.max(0)).unwrap_or(usize::MAX),
            heading: row.get(8),
            content: row.get(9),
            content_sha256: row.get(10),
            source_anchor: row.get(11),
            token_estimate: usize::try_from(token_estimate.max(0)).unwrap_or(usize::MAX),
            created_at: created_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_memory_link(row: &Row) -> Result<MemoryLink> {
        let created_at: DateTime<Utc> = row.get(11);
        Ok(MemoryLink {
            id: row.get(0),
            link_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            memory_key: row.get(4),
            memory_event_id: row.get(5),
            message_event_id: row.get(6),
            document_id: row.get(7),
            chunk_id: row.get(8),
            link_type: row.get(9),
            payload_json: row.get(10),
            created_at: created_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_retrieval_trace(row: &Row) -> Result<RetrievalTrace> {
        let created_at: DateTime<Utc> = row.get(16);
        let candidate_count: i64 = row.get(9);
        let selected_count: i64 = row.get(10);
        let dropped_count: i64 = row.get(11);
        let budget_tokens: Option<i64> = row.get(12);
        Ok(RetrievalTrace {
            id: row.get(0),
            trace_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            session_key: row.get(4),
            agent_id: row.get(5),
            persona_id: row.get(6),
            source: row.get(7),
            query: row.get(8),
            candidate_count: usize::try_from(candidate_count.max(0)).unwrap_or(usize::MAX),
            selected_count: usize::try_from(selected_count.max(0)).unwrap_or(usize::MAX),
            dropped_count: usize::try_from(dropped_count.max(0)).unwrap_or(usize::MAX),
            budget_tokens: budget_tokens.and_then(|value| usize::try_from(value.max(0)).ok()),
            selected_json: row.get(13),
            dropped_json: row.get(14),
            payload_json: row.get(15),
            created_at: created_at.to_rfc3339(),
        })
    }

    pub(super) fn row_to_compaction_run(row: &Row) -> Result<CompactionRun> {
        let created_at: DateTime<Utc> = row.get(18);
        let source_message_count: i64 = row.get(9);
        let source_token_estimate: i64 = row.get(10);
        Ok(CompactionRun {
            id: row.get(0),
            run_id: row.get(1),
            workspace_id: row.get(2),
            owner_id: row.get(3),
            session_key: row.get(4),
            agent_id: row.get(5),
            persona_id: row.get(6),
            trigger: row.get(7),
            mode: row.get(8),
            source_message_count: usize::try_from(source_message_count.max(0)).unwrap_or(usize::MAX),
            source_token_estimate: usize::try_from(source_token_estimate.max(0)).unwrap_or(usize::MAX),
            summary: row.get(11),
            summary_memory_key: row.get(12),
            source_event_ids_json: row.get(13),
            source_event_range_json: row.get(14),
            source_document_refs_json: row.get(15),
            fidelity_status: row.get(16),
            payload_json: row.get(17),
            created_at: created_at.to_rfc3339(),
        })
    }
}

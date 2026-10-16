//! Row mappers and category codecs shared by the SQLite domain modules.

use crate::memory::traits::{
    ChatProfile, CompactionRun, DocumentChunkRecord, DocumentRecord, MemoryCategory, MemoryDraft, MemoryEvent,
    MemoryLink, MemoryVisibility, MessageEvent, RetrievalTrace,
};
use chrono::{DateTime, Utc};
use rusqlite::Row;

use super::SqliteMemory;

impl SqliteMemory {
    pub(super) fn chat_profile_from_row(row: &Row<'_>) -> rusqlite::Result<ChatProfile> {
        let tags_json: String = row.get(7)?;
        let tags = serde_json::from_str::<Vec<String>>(&tags_json).unwrap_or_default();
        Ok(ChatProfile {
            id: row.get(0)?,
            channel: row.get(1)?,
            chat_id: row.get(2)?,
            chat_kind: row.get(3)?,
            title: row.get(4)?,
            purpose: row.get(5)?,
            notes: row.get(6)?,
            tags,
            updated_by: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }

    pub(super) fn category_to_str(cat: &MemoryCategory) -> String {
        match cat {
            MemoryCategory::Core => "core".into(),
            MemoryCategory::Daily => "daily".into(),
            MemoryCategory::Conversation => "conversation".into(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    pub(super) fn str_to_category(s: &str) -> MemoryCategory {
        match s {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    pub(super) fn message_event_from_row(row: &Row<'_>) -> rusqlite::Result<MessageEvent> {
        let source_legacy: String = row.get(5)?;
        let source_ref_json: Option<String> = row.get(17)?;
        let source = source_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| source_legacy.into());
        let subject_ref_json: Option<String> = row.get(18)?;
        let subject = subject_ref_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let visibility_raw: String = row.get(29)?;
        Ok(MessageEvent {
            id: row.get(0)?,
            event_id: row.get(1)?,
            idempotency_key: row.get(2)?,
            workspace_id: row.get(3)?,
            owner_id: row.get(4)?,
            source,
            channel: row.get(6)?,
            session_key: row.get(7)?,
            parent_session_key: row.get(8)?,
            run_id: row.get(9)?,
            parent_run_id: row.get(10)?,
            agent_id: row.get(11)?,
            persona_id: row.get(12)?,
            sender: row.get(13)?,
            recipient: row.get(14)?,
            role: row.get(15)?,
            event_type: row
                .get::<_, Option<String>>(16)?
                .unwrap_or_else(|| "message.legacy".to_string()),
            subject,
            goal_id: row.get(19)?,
            causation_event_id: row.get(20)?,
            correlation_id: row.get(21)?,
            attempt_id: row.get(22)?,
            lease_epoch: row.get(23)?,
            config_generation_id: row
                .get::<_, Option<i64>>(24)?
                .and_then(|value| u64::try_from(value).ok()),
            config_source_revision: row.get(25)?,
            content: row.get(26)?,
            content_hash: row.get(27)?,
            raw_payload_json: row.get(28)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            created_at: row.get(30)?,
            updated_at: row.get(31)?,
        })
    }

    pub(super) fn evolution_proposal_from_row(
        row: &Row<'_>,
    ) -> rusqlite::Result<anyhow::Result<crate::self_system::evolution::EvolutionProposalDraft>> {
        use crate::self_system::evolution::proposal::ProposalRowValues;
        let draft_id: String = row.get(0)?;
        let owner_id: String = row.get(1)?;
        let principal_id: String = row.get(2)?;
        let workspace_id: String = row.get(3)?;
        let topic_id: Option<String> = row.get(4)?;
        let task_id: Option<String> = row.get(5)?;
        let source_message_event_ids_json: String = row.get(6)?;
        let source_memory_event_ids_json: String = row.get(7)?;
        let evidence_hashes_json: String = row.get(8)?;
        let target_resource_json: String = row.get(9)?;
        let proposed_change_json: String = row.get(10)?;
        let risk_level: String = row.get(11)?;
        let mode: String = row.get(12)?;
        let created_at_raw: String = row.get(13)?;
        let created_by_runtime: String = row.get(14)?;
        let judge_verdict_json: Option<String> = row.get(15)?;
        let applied_at_raw: Option<String> = row.get(16)?;
        let applied_by: Option<String> = row.get(17)?;
        let rollback_anchor_json: Option<String> = row.get(18)?;

        Ok((move || {
            let created_at = DateTime::parse_from_rfc3339(&created_at_raw)?.with_timezone(&Utc);
            let applied_at = match applied_at_raw.as_deref() {
                Some(raw) => Some(DateTime::parse_from_rfc3339(raw)?.with_timezone(&Utc)),
                None => None,
            };
            ProposalRowValues::decode(
                draft_id,
                owner_id,
                principal_id,
                workspace_id,
                topic_id,
                task_id,
                &source_message_event_ids_json,
                &source_memory_event_ids_json,
                &evidence_hashes_json,
                &target_resource_json,
                &proposed_change_json,
                &risk_level,
                &mode,
                created_at,
                created_by_runtime,
                judge_verdict_json.as_deref(),
                applied_at,
                applied_by,
                rollback_anchor_json.as_deref(),
            )
        })())
    }

    pub(super) fn memory_event_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryEvent> {
        let visibility_raw: String = row.get(12)?;
        Ok(MemoryEvent {
            id: row.get(0)?,
            event_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            event_type: row.get(4)?,
            subject_table: row.get(5)?,
            subject_id: row.get(6)?,
            session_key: row.get(7)?,
            run_id: row.get(8)?,
            parent_run_id: row.get(9)?,
            agent_id: row.get(10)?,
            persona_id: row.get(11)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            payload_json: row.get(13)?,
            created_at: row.get(14)?,
        })
    }

    pub(super) fn memory_draft_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryDraft> {
        let category_raw: String = row.get(11)?;
        let visibility_raw: String = row.get(13)?;
        Ok(MemoryDraft {
            id: row.get(0)?,
            draft_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            worker_run_id: row.get(4)?,
            parent_run_id: row.get(5)?,
            session_key: row.get(6)?,
            agent_id: row.get(7)?,
            persona_id: row.get(8)?,
            key: row.get(9)?,
            content: row.get(10)?,
            category: Self::str_to_category(&category_raw),
            source_event_id: row.get(12)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            status: row.get(14)?,
            payload_json: row.get(15)?,
            created_at: row.get(16)?,
            updated_at: row.get(17)?,
        })
    }

    pub(super) fn document_from_row(row: &Row<'_>) -> rusqlite::Result<DocumentRecord> {
        let visibility_raw: String = row.get(12)?;
        let chunk_count_raw: i64 = row.get(14)?;
        Ok(DocumentRecord {
            id: row.get(0)?,
            document_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            topic_id: row.get(4)?,
            task_id: row.get(5)?,
            source_message_event_id: row.get(6)?,
            source_kind: row.get(7)?,
            source_uri: row.get(8)?,
            title: row.get(9)?,
            content_sha256: row.get(10)?,
            mime_type: row.get(11)?,
            visibility: visibility_raw.parse().unwrap_or(MemoryVisibility::Workspace),
            metadata_json: row.get(13)?,
            chunk_count: usize::try_from(chunk_count_raw).unwrap_or(0),
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
        })
    }

    pub(super) fn document_chunk_from_row(row: &Row<'_>) -> rusqlite::Result<DocumentChunkRecord> {
        let chunk_index_raw: i64 = row.get(7)?;
        let token_estimate_raw: i64 = row.get(12)?;
        Ok(DocumentChunkRecord {
            id: row.get(0)?,
            chunk_id: row.get(1)?,
            document_id: row.get(2)?,
            workspace_id: row.get(3)?,
            owner_id: row.get(4)?,
            topic_id: row.get(5)?,
            task_id: row.get(6)?,
            chunk_index: usize::try_from(chunk_index_raw).unwrap_or(0),
            heading: row.get(8)?,
            content: row.get(9)?,
            content_sha256: row.get(10)?,
            source_anchor: row.get(11)?,
            token_estimate: usize::try_from(token_estimate_raw).unwrap_or(0),
            created_at: row.get(13)?,
        })
    }

    pub(super) fn memory_link_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryLink> {
        Ok(MemoryLink {
            id: row.get(0)?,
            link_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            memory_key: row.get(4)?,
            memory_event_id: row.get(5)?,
            message_event_id: row.get(6)?,
            document_id: row.get(7)?,
            chunk_id: row.get(8)?,
            link_type: row.get(9)?,
            payload_json: row.get(10)?,
            created_at: row.get(11)?,
        })
    }

    pub(super) fn retrieval_trace_from_row(row: &Row<'_>) -> rusqlite::Result<RetrievalTrace> {
        let candidate_count_raw: i64 = row.get(9)?;
        let selected_count_raw: i64 = row.get(10)?;
        let dropped_count_raw: i64 = row.get(11)?;
        let budget_tokens_raw: Option<i64> = row.get(12)?;
        Ok(RetrievalTrace {
            id: row.get(0)?,
            trace_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            session_key: row.get(4)?,
            agent_id: row.get(5)?,
            persona_id: row.get(6)?,
            source: row.get(7)?,
            query: row.get(8)?,
            candidate_count: usize::try_from(candidate_count_raw).unwrap_or(0),
            selected_count: usize::try_from(selected_count_raw).unwrap_or(0),
            dropped_count: usize::try_from(dropped_count_raw).unwrap_or(0),
            budget_tokens: budget_tokens_raw.and_then(|value| usize::try_from(value).ok()),
            selected_json: row.get(13)?,
            dropped_json: row.get(14)?,
            payload_json: row.get(15)?,
            created_at: row.get(16)?,
        })
    }

    pub(super) fn compaction_run_from_row(row: &Row<'_>) -> rusqlite::Result<CompactionRun> {
        let source_message_count_raw: i64 = row.get(9)?;
        let source_token_estimate_raw: i64 = row.get(10)?;
        Ok(CompactionRun {
            id: row.get(0)?,
            run_id: row.get(1)?,
            workspace_id: row.get(2)?,
            owner_id: row.get(3)?,
            session_key: row.get(4)?,
            agent_id: row.get(5)?,
            persona_id: row.get(6)?,
            trigger: row.get(7)?,
            mode: row.get(8)?,
            source_message_count: usize::try_from(source_message_count_raw).unwrap_or(0),
            source_token_estimate: usize::try_from(source_token_estimate_raw).unwrap_or(0),
            summary: row.get(11)?,
            summary_memory_key: row.get(12)?,
            source_event_ids_json: row.get(13)?,
            source_event_range_json: row.get(14)?,
            source_document_refs_json: row.get(15)?,
            fidelity_status: row.get(16)?,
            payload_json: row.get(17)?,
            created_at: row.get(18)?,
        })
    }
}

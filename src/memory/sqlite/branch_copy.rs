//! One-transaction branch transcript copy (see `memory::branch_copy`).
use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::{MessageEvent, SqliteMemory};
use crate::memory::branch_copy::{
    BranchCopiedHandoff, BranchCopyError, BranchCopyPlan, BranchCopyResult, CONTEXT_SWITCH_EVENT_TYPE, CopiedRow,
    branch_copy_input, is_source_row,
};

impl SqliteMemory {
    pub(super) async fn copy_branch_events(&self, plan: BranchCopyPlan) -> anyhow::Result<BranchCopyResult> {
        plan.check_bounds()?;
        let pool = self.pool.clone();
        let key_root = self.db_path.parent().map(std::path::Path::to_path_buf);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<BranchCopyResult> {
            let connection = pool.write();
            // IMMEDIATE takes the write lock before the first read, so no other
            // writer can interleave and the copied row ids stay contiguous.
            let tx = rusqlite::Transaction::new_unchecked(&connection, rusqlite::TransactionBehavior::Immediate)?;
            let mut copies: HashMap<String, CopiedRow> = HashMap::with_capacity(plan.total_rows());
            let mut newest_handoff = None;
            let mut previous_row_id: Option<i64> = None;
            let batches = plan.handoff_batches.iter().chain(std::iter::once(&plan.final_batch));
            for batch in batches {
                for original_event_id in &batch.original_event_ids {
                    let original = read_source_row(&tx, &plan, original_event_id)?;
                    let new_event_id = Uuid::new_v4().to_string();
                    let input = branch_copy_input(&plan, &original, &copies, &new_event_id)?;
                    input.validate()?;
                    match super::recovery_authority::stable_key_fence(&tx, key_root.as_deref(), &input)? {
                        super::recovery_authority::Fence::Clear => {}
                        super::recovery_authority::Fence::Tombstoned => {
                            return Err(crate::memory::ingress_recovery::StableKeyTombstoned.into());
                        }
                    }
                    let content = input.content.clone();
                    let (copy, inserted) = Self::append_event_row(&tx, input, false)?;
                    if !inserted || copy.event_id != new_event_id {
                        return Err(BranchCopyError::AlreadyCopied {
                            event_id: original_event_id.clone(),
                        }
                        .into());
                    }
                    if previous_row_id.is_some_and(|previous| copy.id != previous.saturating_add(1)) {
                        return Err(BranchCopyError::NotContiguous.into());
                    }
                    previous_row_id = Some(copy.id);
                    if original.event_type == CONTEXT_SWITCH_EVENT_TYPE {
                        newest_handoff = Some(BranchCopiedHandoff {
                            original_event_id: original_event_id.clone(),
                            event_id: copy.event_id.clone(),
                            content,
                        });
                    }
                    copies.insert(
                        original_event_id.clone(),
                        CopiedRow {
                            event_id: copy.event_id,
                            row_id: copy.id,
                        },
                    );
                }
            }
            tx.commit()?;
            Ok(BranchCopyResult {
                copied_rows: copies.len(),
                newest_handoff,
            })
        })
        .await?
    }
}

/// Read one original row; it must belong to the plan's source session.
fn read_source_row(
    tx: &rusqlite::Transaction<'_>,
    plan: &BranchCopyPlan,
    event_id: &str,
) -> anyhow::Result<MessageEvent> {
    let row = tx
        .query_row(
            "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
             FROM message_events
             WHERE workspace_id = ?1 AND event_id = ?2
             LIMIT 1",
            params![plan.workspace_id, event_id],
            SqliteMemory::message_event_from_row,
        )
        .optional()?;
    row.filter(|event| is_source_row(plan, event)).ok_or_else(|| {
        BranchCopyError::SourceRowMissing {
            event_id: event_id.to_string(),
        }
        .into()
    })
}

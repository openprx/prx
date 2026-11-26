//! Audit rows of the context hook contributions an opening turn or a session
//! opening takes in. One row per contribution, with role `tool` and event type
//! `hook.contribution.recorded`. The row carries the metadata only: the text
//! lives in the stored attachment tail or the session prefix row, so it is
//! never stored twice. Replay skips the row by event type (see `replay.rs`).

use crate::hooks::context::{ContributionKind, HookContribution};
use crate::memory::{MemoryFabric, MessageEventScope};

/// Event type of the audit row of one context hook contribution.
pub(crate) const HOOK_CONTRIBUTION_EVENT_TYPE: &str = "hook.contribution.recorded";
/// Schema version of the audit row payload.
const HOOK_ROW_SCHEMA_VERSION: u32 = 1;
/// Content of the row: a label, never the contribution text.
const HOOK_ROW_CONTENT: &str = "hook contribution";

/// The snake-case name of a contribution kind, as the audit row stores it.
const fn kind_name(kind: ContributionKind) -> &'static str {
    match kind {
        ContributionKind::AddContext => "add_context",
        ContributionKind::ReplaceOutput => "replace_output",
        ContributionKind::Note => "note",
        ContributionKind::SummaryInstructions => "summary_instructions",
    }
}

/// Append one audit row per contribution. Best effort: a failed write is logged
/// with its error digest only and never fails the turn.
pub(crate) async fn append_hook_contributions(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    contributions: &[HookContribution],
) {
    for contribution in contributions {
        let payload = serde_json::json!({
            "schema_version": HOOK_ROW_SCHEMA_VERSION,
            "hook": contribution.hook,
            "point": contribution.point.as_str(),
            "kind": kind_name(contribution.kind),
            "chars": contribution.text.chars().count(),
            "run_id": scope.run_id,
        });
        if let Err(error) = fabric
            .record_tool_round_event(
                scope.clone(),
                HOOK_CONTRIBUTION_EVENT_TYPE,
                HOOK_ROW_CONTENT,
                Some(payload.to_string()),
            )
            .await
        {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(error_len, error_sha256, "Failed to persist hook contribution");
        }
    }
}

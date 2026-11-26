//! Skill-load rows: one `skill.load.recorded` row for each successful load of a
//! skill's instruction file. The handoff note reader that lists them is in the
//! same file below (see `skills_in_use`).
//!
//! A load is a `skill_read` call without a resource path whose outcome
//! succeeded and whose output names the skill. The row keeps the name and the
//! size of the body, never the body itself: the `tool.result.recorded` row
//! already holds that text. The row has role `tool`, so it stays off the sync
//! stream and out of every history loader, like the other tool rows.

use std::collections::HashSet;

use serde_json::Value;

use crate::context::reduce::handoff_note::{HANDOFF_SKILLS_MAX_COUNT, is_valid_handoff_skill_name};
use crate::memory::erasure::Unsupported;
use crate::memory::{Memory, MemoryFabric, MemoryPrincipal, MessageEventScope};
use crate::tools::ToolExecutionStatus;

pub(crate) const SKILL_LOAD_EVENT_TYPE: &str = "skill.load.recorded";

/// Backend of the root `skill_read` tool and of every per-skill alias of it.
const SKILL_READ_BACKEND: &str = "skill_read";

/// Skill-load rows read per handoff, newest first.
const SKILL_LOAD_READ_ROWS: usize = 64;

/// One successful skill instruction load, as recognised from its tool outcome.
#[derive(Debug, Clone)]
pub(crate) struct SkillLoad {
    pub(crate) name: String,
    pub(crate) revision_id: Option<String>,
    pub(crate) content_bytes: usize,
}

/// The skill a finished call loaded, or `None`.
///
/// Only a succeeded `skill_read` call with no non-empty `resource_path` counts.
/// The name and the body size come from the tool's own JSON output, which must
/// carry a string `skill` and a string `content`. Anything else (a failed or
/// refused call, a resource read, or output that does not parse) is not a load.
pub(crate) fn skill_load_of(
    backend: Option<&str>,
    arguments: &Value,
    status: ToolExecutionStatus,
    output: Option<&str>,
) -> Option<SkillLoad> {
    if status != ToolExecutionStatus::Succeeded || backend != Some(SKILL_READ_BACKEND) {
        return None;
    }
    let is_resource_read = arguments
        .get("resource_path")
        .and_then(Value::as_str)
        .is_some_and(|path| !path.trim().is_empty());
    if is_resource_read {
        return None;
    }
    let parsed: Value = serde_json::from_str(output?).ok()?;
    let object = parsed.as_object()?;
    let name = object.get("skill")?.as_str()?;
    let content = object.get("content")?.as_str()?;
    let revision_id = object.get("revision_id").and_then(Value::as_str).map(str::to_owned);
    Some(SkillLoad {
        name: name.to_owned(),
        revision_id,
        content_bytes: content.len(),
    })
}

/// Append the `skill.load.recorded` row of one call. A write error is logged
/// with its digest only.
pub(super) async fn append_skill_load(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    round: usize,
    call_id: &str,
    load: &SkillLoad,
) {
    let payload = serde_json::json!({
        "schema_version": 1,
        "run_id": scope.run_id,
        "round": round,
        "call_id": call_id,
        "skill": load.name,
        "revision_id": load.revision_id,
        "content_bytes": load.content_bytes,
    });
    if let Err(error) = fabric
        .record_tool_round_event(
            scope.clone(),
            SKILL_LOAD_EVENT_TYPE,
            format!("skill {}", load.name),
            Some(payload.to_string()),
        )
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::warn!(error_len, error_sha256, "Failed to persist skill load");
    }
}

/// The skill names of `session_key` a handoff note can list, newest first, at
/// most [`HANDOFF_SKILLS_MAX_COUNT`].
///
/// Names that the note validator would refuse are skipped, so the note never
/// fails validation because of them. Duplicates are removed case-insensitively
/// and the newest spelling is kept. A read error gives an empty list and is
/// logged with its digest and not with the names; a handoff is never refused
/// because of it.
pub(crate) async fn skills_in_use(memory: &dyn Memory, principal: &MemoryPrincipal, session_key: &str) -> Vec<String> {
    let rows = match memory
        .newest_session_events_of_type(principal, session_key, SKILL_LOAD_EVENT_TYPE, SKILL_LOAD_READ_ROWS)
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.is::<Unsupported>() => return Vec::new(),
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                "Failed to read skill loads for the handoff note; the note lists none"
            );
            return Vec::new();
        }
    };
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for event in rows {
        if event.session_key.as_deref() != Some(session_key) {
            continue;
        }
        let Some(name) = event
            .raw_payload_json
            .as_deref()
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            .and_then(|payload| payload.get("skill").and_then(Value::as_str).map(str::to_owned))
        else {
            continue;
        };
        if !is_valid_handoff_skill_name(&name) || !seen.insert(name.to_lowercase()) {
            continue;
        }
        names.push(name);
        if names.len() == HANDOFF_SKILLS_MAX_COUNT {
            break;
        }
    }
    names
}

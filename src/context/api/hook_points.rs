//! The context hook points of the tool result, the handoff and the child
//! open: the contributions that change what a tool result carries, what a
//! summary handoff is asked for, and what a child's first input carries.

use std::fmt::Write as _;
use std::sync::Arc;

use super::tail::TurnTailInput;
use crate::context::transcript::append_hook_contributions;
use crate::hooks::context::{ContextPoint, ContributionKind, HookContribution};
use crate::hooks::{HookEvent, HookIdentity, HookManager};
use crate::memory::MemoryFabric;
use crate::runtime::envelope::RuntimeSource;

/// One tool call of a round: its name, its call id, whether it succeeded and
/// its raw output.
pub(crate) type ToolResultCall<'a> = (&'a str, String, bool, &'a str);

/// The texts the model sees for the tool calls of one round, and every
/// contribution the round took. Each call's `ToolResult` context actions run
/// concurrently with the other calls. A call's text is `None` when nothing
/// contributed, so the output stays as it was.
///
/// The first `replace_output` (in the order the hook manager returns them)
/// replaces the output; the later ones are ignored. Every `note` is appended
/// as `[Hook note: <hook>] <text>`.
pub(crate) async fn tool_result_texts(
    hooks: &HookManager,
    calls: &[ToolResultCall<'_>],
) -> (Vec<Option<String>>, Vec<HookContribution>) {
    let runs = calls.iter().map(|(tool, call_id, success, output)| {
        hooks.contribute(
            ContextPoint::ToolResult,
            serde_json::json!({
                "tool": tool,
                "call_id": call_id,
                "success": success,
                "output": output,
            }),
        )
    });
    let per_call = futures::future::join_all(runs).await;

    let mut texts = Vec::with_capacity(calls.len());
    let mut all = Vec::new();
    for ((_, _, _, output), contributions) in calls.iter().zip(per_call) {
        let mut replacement: Option<&str> = None;
        let mut notes = String::new();
        for contribution in &contributions {
            match contribution.kind {
                ContributionKind::ReplaceOutput if replacement.is_none() => {
                    replacement = Some(contribution.text.as_str());
                }
                ContributionKind::Note => {
                    let _ = write!(notes, "\n[Hook note: {}] {}", contribution.hook, contribution.text);
                }
                _ => {}
            }
        }
        texts.push(if replacement.is_none() && notes.is_empty() {
            None
        } else {
            Some(format!("{}{notes}", replacement.unwrap_or(*output)))
        });
        all.extend(contributions);
    }
    (texts, all)
}

/// The summary instructions of a handoff about to be written: the
/// `reduction_started` notify event, then the before-handoff contributions of
/// the turn the reduction runs in. Outside a turn there is no manager, so
/// nothing is asked. The contributions carry no audit row: the handoff's own
/// summary is the stored effect.
///
/// `reduction_started` is emitted on every in-turn reduction; the contribution
/// work runs only when a before-handoff action is configured.
pub(crate) async fn summary_instructions(trigger: &str, cold_messages: usize) -> Vec<String> {
    let data = serde_json::json!({
        "trigger": trigger,
        "cold_messages": cold_messages,
    });
    crate::hooks::notify(HookEvent::ReductionStarted, data.clone());
    let Some(manager) = crate::hooks::current_manager() else {
        return Vec::new();
    };
    if !manager.has_context_actions(ContextPoint::BeforeHandoff) {
        return Vec::new();
    }
    let contributions = manager.contribute(ContextPoint::BeforeHandoff, data).await;
    tracing::debug!(count = contributions.len(), "before-handoff contributions");
    contributions
        .into_iter()
        .map(|contribution| contribution.text)
        .collect()
}

/// Whether a turn opened from `source` is a child's first input: the sources
/// of the sessions_spawn task, the session-worker process and the delegate
/// tool. A child takes the child-open contribution in place of the turn-open
/// and session-open points.
pub(crate) const fn is_child_open(source: RuntimeSource) -> bool {
    matches!(
        source,
        RuntimeSource::SessionWorker | RuntimeSource::SessionsSpawn | RuntimeSource::Delegate
    )
}

/// The child-open contributions of a child's first input, rendered as the
/// stored attachment tail: one `[Hook context: <hook>]` block per
/// `child_started` contribution, each with one audit row under the child's
/// key. Empty without a manager (the child then has no hook).
pub(crate) async fn child_open_tail(input: TurnTailInput<'_>) -> String {
    let TurnTailInput {
        memory,
        envelope,
        memory_config,
        visible_message,
        hooks,
    } = input;
    let Some(hooks) = hooks else {
        return String::new();
    };
    if !hooks.has_context_actions(ContextPoint::ChildStarted) {
        return String::new();
    }
    let mode = match envelope.source {
        RuntimeSource::SessionWorker => "process",
        RuntimeSource::Delegate => "delegate",
        _ => "task",
    };
    let data = serde_json::json!({
        "child_session": envelope.session_key,
        "mode": mode,
        "brief_chars": visible_message.chars().count(),
    });
    let contributions = crate::hooks::identity_scope(
        HookIdentity::for_session(envelope),
        hooks.contribute(ContextPoint::ChildStarted, data),
    )
    .await;
    if contributions.is_empty() {
        return String::new();
    }
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    append_hook_contributions(&fabric, &envelope.message_scope(), &contributions).await;
    contributions
        .iter()
        .map(|contribution| format!("\n\n[Hook context: {}]\n{}", contribution.hook, contribution.text))
        .collect()
}

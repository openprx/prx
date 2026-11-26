//! The context hook points of the tool result and the handoff: the
//! contributions that change what a tool result carries and what a summary
//! handoff is asked for. The child-open point joins here later.

use std::fmt::Write as _;

use crate::hooks::context::{ContextPoint, ContributionKind, HookContribution};
use crate::hooks::{HookEvent, HookManager};

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
pub(crate) async fn summary_instructions(trigger: &str, cold_messages: usize) -> Vec<String> {
    let data = serde_json::json!({
        "trigger": trigger,
        "cold_messages": cold_messages,
    });
    crate::hooks::notify(HookEvent::ReductionStarted, data.clone());
    let contributions = crate::hooks::contribute_in_identity(ContextPoint::BeforeHandoff, data).await;
    tracing::debug!(count = contributions.len(), "before-handoff contributions");
    contributions
        .into_iter()
        .map(|contribution| contribution.text)
        .collect()
}

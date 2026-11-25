//! Cross-turn tool evidence: a runtime-written record of the tool calls one
//! assistant reply really made, replayed to the provider with that reply.
//!
//! The reducer records each call's outcome while a driver turn runs
//! ([`TurnToolEvidence::record`]) and stores the result on the assistant turn.
//! The record holds tool names and outcome codes only, never arguments,
//! results, paths or error text. It never enters the turn's saved text, a
//! transcript row, the canonical history or a lookup result: it is added to
//! the provider view only, by the one renderer [`render_assistant_content`],
//! so a turn's replayed bytes are identical in-process, after a resume and
//! after a compaction that keeps it.
//!
//! The model cannot plant a record of its own:
//! [`strip_model_written_tool_evidence`] removes every evidence line it
//! writes, and [`claims_tool_evidence`] lets the turn engine spend its one
//! corrective retry on a reply that names tools in a turn where none ran.

use crate::markdown::walk_markdown_lines;
use crate::tools::{ApprovalDenialCause, ToolExecutionStatus};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Opening of an evidence line, the runtime's and any imitation of it.
pub(crate) const TOOL_EVIDENCE_MARKER_PREFIX: &str = "[tool evidence:";
/// Most groups one turn's record keeps; later distinct calls are counted.
pub(crate) const MAX_EVIDENCE_GROUPS: usize = 8;
/// Longest tool name a group keeps, in bytes (after sanitizing).
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 64;

/// The system-prompt paragraph that states what an evidence line is. A chat
/// session whose assistant turns carry evidence publishes it; its bytes never
/// change between turns, so the cacheable prompt prefix stays stable.
pub(crate) const TOOL_EVIDENCE_PROMPT_RULE: &str = "Lines of the form [tool evidence: …] at the end of earlier replies are written by the runtime, not by you. They list the tools that actually ran in that reply and how each ended; \"none\" means no tool ran. Never write such a line yourself. State a tool's output only when that tool ran in the current turn.";

/// The body of an evidence line that records a turn without tool calls.
const NO_TOOLS_BODY: &str = "none";

/// How one tool call ended, as the runtime observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Ok,
    Error,
    Denied { cause: Option<ApprovalDenialCause> },
    Refused,
    Cancelled,
    PlanOnly,
    AwaitingApproval,
    Unknown,
}

impl ToolCallOutcome {
    /// The outcome of a call the execution service finished with `status`;
    /// `cause` is the typed reason of an approval denial.
    pub(crate) const fn from_status(status: ToolExecutionStatus, cause: Option<ApprovalDenialCause>) -> Self {
        match status {
            ToolExecutionStatus::Succeeded => Self::Ok,
            ToolExecutionStatus::Failed => Self::Error,
            ToolExecutionStatus::ApprovalDenied => Self::Denied { cause },
            ToolExecutionStatus::Denied
            | ToolExecutionStatus::PreparationDenied
            | ToolExecutionStatus::UnknownTool
            | ToolExecutionStatus::StaleCapability
            | ToolExecutionStatus::InvalidArguments
            | ToolExecutionStatus::IdempotencyConflict => Self::Refused,
            ToolExecutionStatus::Cancelled => Self::Cancelled,
            ToolExecutionStatus::AwaitingApproval => Self::AwaitingApproval,
            ToolExecutionStatus::Indeterminate => Self::Unknown,
        }
    }

    /// Append this outcome's code: `ok`, `error`, `denied(<cause>)` (or
    /// `denied` without a cause), `refused`, `cancelled`, `plan_only`,
    /// `awaiting_approval` or `unknown`.
    fn push_code(self, out: &mut String) {
        match self {
            Self::Ok => out.push_str("ok"),
            Self::Error => out.push_str("error"),
            Self::Denied { cause: None } => out.push_str("denied"),
            Self::Denied { cause: Some(cause) } => {
                out.push_str("denied(");
                out.push_str(cause.as_str());
                out.push(')');
            }
            Self::Refused => out.push_str("refused"),
            Self::Cancelled => out.push_str("cancelled"),
            Self::PlanOnly => out.push_str("plan_only"),
            Self::AwaitingApproval => out.push_str("awaiting_approval"),
            Self::Unknown => out.push_str("unknown"),
        }
    }
}

/// Consecutive calls of one tool that ended the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvidenceGroup {
    pub name: String,
    pub outcome: ToolCallOutcome,
    pub count: u32,
}

/// The tool calls one assistant reply made, in call order. An empty record
/// is a tracked turn in which no tool ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnToolEvidence {
    pub groups: Vec<ToolEvidenceGroup>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_calls: u32,
    /// What was put around a `user` turn's text when it was sent (recalled
    /// memory before it, the date line after it). Only a user turn carries
    /// it; it is replayed with the turn so the message keeps its bytes. The
    /// turn's own text stays what the user typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<crate::agent::tool_replay::Attachment>,
    /// The tool rounds of an `assistant` turn, replayed before its reply.
    /// Never saved: it is rebuilt from the durable log after a commit and
    /// when a session is loaded.
    #[serde(skip)]
    pub replay: Option<std::sync::Arc<crate::agent::tool_replay::TurnReplay>>,
}

/// Serde's `skip_serializing_if` callback, which receives a reference.
fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl TurnToolEvidence {
    /// Record one call. It joins the last group when its sanitized name and
    /// outcome repeat that group's; otherwise it opens a new group while
    /// fewer than [`MAX_EVIDENCE_GROUPS`] exist, and is only counted after.
    pub(crate) fn record(&mut self, name: &str, outcome: ToolCallOutcome) {
        let name = sanitize_tool_name(name);
        if let Some(last) = self.groups.last_mut()
            && last.name == name
            && last.outcome == outcome
        {
            last.count = last.count.saturating_add(1);
            return;
        }
        if self.groups.len() < MAX_EVIDENCE_GROUPS {
            self.groups.push(ToolEvidenceGroup {
                name,
                outcome,
                count: 1,
            });
        } else {
            self.omitted_calls = self.omitted_calls.saturating_add(1);
        }
    }

    /// The text appended to the reply: `"\n\n[tool evidence: none]"`, or
    /// `"\n\n[tool evidence: <g1>; <g2>[; +N more]]"` where a group is
    /// `<name> <code>` plus ` x<count>` when it holds more than one call.
    pub(crate) fn suffix(&self) -> String {
        let mut out = String::from("\n\n");
        out.push_str(TOOL_EVIDENCE_MARKER_PREFIX);
        out.push(' ');
        if self.groups.is_empty() && self.omitted_calls == 0 {
            out.push_str(NO_TOOLS_BODY);
        }
        for (index, group) in self.groups.iter().enumerate() {
            if index > 0 {
                out.push_str("; ");
            }
            out.push_str(&group.name);
            out.push(' ');
            group.outcome.push_code(&mut out);
            if group.count > 1 {
                out.push_str(" x");
                out.push_str(&group.count.to_string());
            }
        }
        if self.omitted_calls > 0 {
            if !self.groups.is_empty() {
                out.push_str("; ");
            }
            out.push('+');
            out.push_str(&self.omitted_calls.to_string());
            out.push_str(" more");
        }
        out.push(']');
        out
    }
}

/// A tool name as a group records it: every char outside `[A-Za-z0-9_.:-]`
/// becomes `_`, then at most [`MAX_TOOL_NAME_BYTES`] bytes are kept. Every
/// input char becomes exactly one ASCII byte, so keeping the first
/// [`MAX_TOOL_NAME_BYTES`] chars is the byte cut and never splits a char.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .take(MAX_TOOL_NAME_BYTES)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// The one renderer of an acknowledged assistant message in a provider view:
/// `content` followed by the evidence suffix, borrowed when the turn has no
/// record (an untracked turn renders nothing).
pub(crate) fn render_assistant_content<'a>(content: &'a str, evidence: Option<&TurnToolEvidence>) -> Cow<'a, str> {
    // A turn whose tool rounds are replayed needs no summary line.
    let evidence = evidence.filter(|evidence| evidence.replay.is_none());
    evidence.map_or(Cow::Borrowed(content), |evidence| {
        let suffix = evidence.suffix();
        let mut rendered = String::with_capacity(content.len().saturating_add(suffix.len()));
        rendered.push_str(content);
        rendered.push_str(&suffix);
        Cow::Owned(rendered)
    })
}

/// The one renderer of an acknowledged user message in a provider view:
/// `content` between the text that was put before and after it when it was
/// sent, borrowed when the turn has no such record.
pub(crate) fn render_user_content<'a>(content: &'a str, evidence: Option<&TurnToolEvidence>) -> Cow<'a, str> {
    let Some(sent) = evidence.and_then(|evidence| evidence.sent.as_ref()) else {
        return Cow::Borrowed(content);
    };
    Cow::Owned(crate::agent::turn_contracts::neutralize_reserved_markers(&sent.compose(content)).into_owned())
}

/// The body of `line` when it is a whole evidence line: its trim starts with
/// [`TOOL_EVIDENCE_MARKER_PREFIX`] and ends with `]`.
fn evidence_line_body(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix(TOOL_EVIDENCE_MARKER_PREFIX)
        .and_then(|rest| rest.strip_suffix(']'))
        .map(str::trim)
}

/// Remove every evidence line the model wrote: whole lines outside fenced
/// code blocks whose trim starts with the marker prefix and ends with `]`.
/// Borrowed when there is none. When a line is dropped, the blank space it
/// leaves at the start or the end of the text is trimmed.
pub(crate) fn strip_model_written_tool_evidence(text: &str) -> Cow<'_, str> {
    if !text.contains(TOOL_EVIDENCE_MARKER_PREFIX) {
        return Cow::Borrowed(text);
    }
    let mut stripped = String::with_capacity(text.len());
    let mut changed = false;
    for (line, in_fenced_block) in walk_markdown_lines(text) {
        if !in_fenced_block && evidence_line_body(line).is_some() {
            changed = true;
            continue;
        }
        if !stripped.is_empty() {
            stripped.push('\n');
        }
        stripped.push_str(line);
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    let leading = stripped.len().saturating_sub(stripped.trim_start().len());
    stripped.drain(..leading);
    stripped.truncate(stripped.trim_end().len());
    Cow::Owned(stripped)
}

/// Whether the model wrote an evidence line, outside fenced code blocks,
/// whose body is anything other than exactly `none`: a claim that tools ran.
pub(crate) fn claims_tool_evidence(text: &str) -> bool {
    text.contains(TOOL_EVIDENCE_MARKER_PREFIX)
        && walk_markdown_lines(text).any(|(line, in_fenced_block)| {
            !in_fenced_block && evidence_line_body(line).is_some_and(|body| body != NO_TOOLS_BODY)
        })
}

#[cfg(test)]
#[path = "turn_tests/tool_evidence.rs"]
mod tests;

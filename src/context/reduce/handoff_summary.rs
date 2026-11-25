//! The model summary a `switch` handoff note carries.
//!
//! The cold window a handoff moves out of the window is rendered as plain text,
//! the turn's own model writes one summary of it, and the reply is cleaned and
//! bounded before the note stores it. A failed or empty call never fails the
//! turn: the note records the status instead.

use super::handoff_note::{
    CONTEXT_HANDOFF_END, CONTEXT_HANDOFF_START, HANDOFF_SUMMARY_MAX_BYTES, format_compact_context_handoff,
    is_compact_context_handoff, is_context_handoff_message, validated_context_handoff,
};
use crate::agent::diagnostics::error_log_digest;
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
use crate::agent::tool_evidence::strip_model_written_tool_evidence;
use crate::agent::turn_contracts::TOOL_RESULTS_PREFIX;
use crate::providers::{ChatMessage, Provider};
use crate::security::redaction::redact_secrets;

/// Lower bound of the source text the summary reads, in characters.
const SOURCE_BUDGET_MIN_CHARS: usize = 4_000;
/// Upper bound of the source text the summary reads, in characters.
const SOURCE_BUDGET_MAX_CHARS: usize = 60_000;
/// Characters of one tool result the summary reads.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;
/// Bytes of one tool call's arguments the summary reads.
const TOOL_ARGUMENTS_MAX_BYTES: usize = 500;
/// Temperature of the summary call.
const SUMMARY_TEMPERATURE: f64 = 0.2;
/// What replaces a reserved marker the model wrote into its summary.
const MARKER_REPLACEMENT: &str = "(marker removed)";
/// The markers a stored summary may not contain (see the note validator).
const FORBIDDEN_MARKERS: [&str; 4] = [
    CONTEXT_HANDOFF_START,
    CONTEXT_HANDOFF_END,
    "[document_ingest_ref]",
    "[/document_ingest_ref]",
];

const SYSTEM_PROMPT: &str = "You write the summary of a conversation that is being moved out of the \
    model's context window, so that the same assistant can continue the work from it. Write the summary \
    for your own continuation, under exactly these headings, in this order:\n\
    ## Goal and constraints\n\
    ## Decisions\n\
    ## State of the work\n\
    ## Open items and next step\n\
    ## Files and artifacts\n\
    ## Tools and skills used\n\
    Under each heading write short bullet points, or the word none when nothing applies. Copy identifiers, \
    paths, numbers and names exactly as they appear in the conversation. Do not invent facts. Keep the whole \
    summary at most 600 words. Start directly with the first heading, with no preamble.";

/// The outcome of the summary call for one handoff.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandoffSummary {
    /// The cleaned, bounded summary text.
    Written(String),
    /// The call failed; nothing was stored from it.
    Failed,
    /// The call answered, but nothing was left after the clean-up.
    Empty,
}

impl HandoffSummary {
    /// The `summary_status` value the note records.
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Written(_) => "written",
            Self::Failed => "failed",
            Self::Empty => "empty",
        }
    }

    /// The summary text, present exactly when the status is `written`.
    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Written(text) => Some(text),
            Self::Failed | Self::Empty => None,
        }
    }
}

/// Asks `provider` once, with `model`, for the summary of the cold window
/// `cold`. `max_context_tokens` sizes the source text read; `extra_instructions`
/// are appended to the system prompt, one per line. Never fails: a provider
/// error is logged by its digest and reported as [`HandoffSummary::Failed`].
pub(crate) async fn summarize_cold_window(
    provider: &dyn Provider,
    model: &str,
    cold: &[ChatMessage],
    max_context_tokens: usize,
    extra_instructions: &[String],
    trigger: &'static str,
) -> HandoffSummary {
    let source = build_cold_window_source(cold, source_budget_chars(max_context_tokens));
    let system = system_prompt(extra_instructions);
    let user = format!("Summarize the earlier conversation below so that the work can continue from it.\n\n{source}");
    match provider
        .chat_with_system(Some(system.as_str()), &user, model, SUMMARY_TEMPERATURE)
        .await
    {
        Ok(raw) => {
            let cleaned = clean_summary(&raw);
            if cleaned.is_empty() {
                HandoffSummary::Empty
            } else {
                HandoffSummary::Written(cleaned)
            }
        }
        Err(error) => {
            let (error_len, error_sha256) = error_log_digest(&error);
            tracing::warn!(
                trigger,
                error_len,
                error_sha256,
                "handoff summary failed; the handoff is written without a summary"
            );
            HandoffSummary::Failed
        }
    }
}

fn system_prompt(extra_instructions: &[String]) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    for instruction in extra_instructions {
        prompt.push('\n');
        prompt.push_str(instruction);
    }
    prompt
}

/// Characters of source text the summary may read: `max_context_tokens * 3 / 2`,
/// clamped to the bounds.
fn source_budget_chars(max_context_tokens: usize) -> usize {
    (max_context_tokens.saturating_mul(3) / 2).clamp(SOURCE_BUDGET_MIN_CHARS, SOURCE_BUDGET_MAX_CHARS)
}

/// The cold window as `ROLE: text` blocks, one per message, bounded to
/// `budget_chars` characters.
fn build_cold_window_source(cold: &[ChatMessage], budget_chars: usize) -> String {
    let blocks: Vec<String> = cold.iter().map(render_cold_message).collect();
    bound_chars(&blocks.join("\n\n"), budget_chars)
}

fn render_cold_message(message: &ChatMessage) -> String {
    if is_context_handoff_message(message) {
        return render_earlier_handoff(&message.content);
    }
    if message.role == "assistant"
        && let Some(calls) = tool_calls_of(&message.content)
    {
        return calls
            .iter()
            .map(|(name, arguments)| {
                format!(
                    "ASSISTANT (tool calls): {name}({})",
                    cut_bytes(arguments, TOOL_ARGUMENTS_MAX_BYTES)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if message.role == "tool" || message.content.starts_with(TOOL_RESULTS_PREFIX) {
        return format!("TOOL: {}", cut_chars(message.content.trim(), TOOL_RESULT_MAX_CHARS));
    }
    format!("{}: {}", message.role.to_uppercase(), message.content.trim())
}

/// An earlier handoff in the cold window: its summary when it has one, else its
/// compact rendered line. Its raw JSON is never sent.
fn render_earlier_handoff(content: &str) -> String {
    if let Some(handoff) = validated_context_handoff(content) {
        return match handoff.summary() {
            Some(summary) => format!("EARLIER HANDOFF SUMMARY: {summary}"),
            None => format_compact_context_handoff(&handoff),
        };
    }
    if is_compact_context_handoff(content) {
        return content.trim().to_string();
    }
    "[an earlier context handoff that does not validate is omitted]".to_string()
}

/// The `(name, arguments)` of each call of an assistant tool-call envelope, in
/// call order; `None` when `content` is not one with at least one call.
fn tool_calls_of(content: &str) -> Option<Vec<(String, String)>> {
    if !content.starts_with('{') || !content.contains("\"tool_calls\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let calls = value.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }
    Some(
        calls
            .iter()
            .map(|call| {
                let function = call.get("function");
                let name = call
                    .get("name")
                    .or_else(|| function.and_then(|function| function.get("name")))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = match call
                    .get("arguments")
                    .or_else(|| function.and_then(|function| function.get("arguments")))
                {
                    Some(serde_json::Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                (name, arguments)
            })
            .collect(),
    )
}

/// Keeps the first 20% and the last 80% of `budget` characters of `text` with a
/// line naming how many characters were left out between them.
fn bound_chars(text: &str, budget: usize) -> String {
    let total = text.chars().count();
    if total <= budget {
        return text.to_string();
    }
    let head_chars = budget / 5;
    let tail_chars = budget.saturating_sub(head_chars);
    let head_end = text
        .char_indices()
        .nth(head_chars)
        .map_or(text.len(), |(index, _)| index);
    let tail_start = text
        .char_indices()
        .nth(total.saturating_sub(tail_chars))
        .map_or(text.len(), |(index, _)| index);
    let omitted = total.saturating_sub(head_chars).saturating_sub(tail_chars);
    format!(
        "{}\n[... {omitted} characters of the middle omitted ...]\n{}",
        text.get(..head_end).unwrap_or_default(),
        text.get(tail_start..).unwrap_or_default(),
    )
}

/// The model's text made safe to store: the fabricated tool markers, the tool
/// evidence lines and the secrets removed, the reserved markers replaced, the
/// text trimmed and cut to [`HANDOFF_SUMMARY_MAX_BYTES`].
fn clean_summary(raw: &str) -> String {
    let stripped = strip_assistant_fabricated_tool_markers(raw);
    let stripped = strip_model_written_tool_evidence(&stripped);
    let mut text = redact_secrets(&stripped);
    for marker in FORBIDDEN_MARKERS {
        text = text.replace(marker, MARKER_REPLACEMENT);
    }
    cut_bytes(text.trim(), HANDOFF_SUMMARY_MAX_BYTES).to_string()
}

/// The first `max` characters of `text`.
fn cut_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => text.get(..index).unwrap_or(text),
        None => text,
    }
}

/// The longest prefix of `text` of at most `max` bytes that ends on a character
/// boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default()
}

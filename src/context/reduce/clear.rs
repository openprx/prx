//! Clearing old tool results: the first and cheapest step of the ladder. While
//! the request is over the warning threshold, the oldest tool results of the
//! view give way to a one-line stub, oldest first, until the request fits.
//! The log is untouched: the step edits this request's view only, and the
//! stub says how to get the result back.
//!
//! What is never cleared:
//!
//! - the results of the current turn (everything from the turn's own request
//!   on: [`TurnHistory::current_turn_start`]), which the model is still
//!   working with;
//! - the results of the newest [`KEEP_RECENT_TOOL_ROUNDS`] rounds before it.
//!   A round is one assistant tool-call message and all its results. The
//!   existing `keep_recent_messages` setting counts messages a handoff keeps
//!   word for word and does not govern tool rounds, so this is a constant;
//! - a result that is already a stub.
//!
//! A stub replaces the text of a result and keeps its message, so the round
//! stays whole and the tool-call protocol holds (see `normalize.rs`).

use super::engine::{PreTurn, Round};
use super::normalize::is_tool_call_envelope;
use super::preflight::preflight_config;
use super::{Outcome, Reason, Reducer, Removed, Tier};
use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, estimate_history_tokens, plan_context_budget};
use crate::agent::turn_history::TurnHistory;
use crate::hooks::HookEvent;
use crate::providers::ChatMessage;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::ops::Range;

/// The newest tool rounds before the current turn whose results are kept in
/// full.
pub(crate) const KEEP_RECENT_TOOL_ROUNDS: usize = 3;

/// The start of every stub. A result that holds it is already cleared.
const CLEARED_PREFIX: &str = "[tool result cleared:";

/// The longest tool name a stub repeats.
const MAX_STUB_TOOL_NAME_BYTES: usize = 64;

/// A tool name as a stub may repeat it: the characters tool names are made of,
/// at most [`MAX_STUB_TOOL_NAME_BYTES`]. Anything else is the model's or a
/// tool's text and does not enter the next request.
fn stub_tool_name(name: &str) -> Option<&str> {
    let valid = !name.is_empty()
        && name.len() <= MAX_STUB_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    valid.then_some(name)
}

/// A stored-document id as `tool_output_document_id` writes it:
/// `tool-output:<tool name>:<16 lowercase hex digits>`, the tool name the one
/// the stub names.
fn is_tool_output_document_id(id: &str, tool: &str) -> bool {
    let Some(rest) = id.strip_prefix("tool-output:") else {
        return false;
    };
    let Some((name, digest)) = rest.rsplit_once(':') else {
        return false;
    };
    name == tool
        && digest.len() == 16
        && digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The one-line text a cleared result is replaced by, on every surface. The
/// tool name and the document id are repeated only when they have the shape
/// PRX writes them in; otherwise the name is `unknown` and the output is said
/// not to be kept.
pub(crate) fn cleared_result_text(tool: &str, status: &str, bytes: u64, document_id: Option<&str>) -> String {
    let tool = stub_tool_name(tool);
    let status = if status == "error" { "error" } else { "ok" };
    let lookup = document_id
        .zip(tool)
        .filter(|(id, tool)| is_tool_output_document_id(id, tool))
        .map_or_else(
            || "the output is not kept in view; run the tool again if it is needed".to_owned(),
            |(id, _)| format!("full output stored as document {id}, read it with document_get_chunk"),
        );
    format!(
        "{CLEARED_PREFIX} tool {}, status {status}, {bytes} bytes; {lookup}]",
        tool.unwrap_or("unknown")
    )
}

#[derive(Deserialize)]
struct ResultPayload {
    tool_call_id: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize)]
struct CallNames {
    tool_calls: Vec<CallName>,
}

#[derive(Deserialize)]
struct CallName {
    id: Option<String>,
    name: Option<String>,
    function: Option<FunctionName>,
}

#[derive(Deserialize)]
struct FunctionName {
    name: Option<String>,
}

fn tool_name_of(envelope: &ChatMessage, call_id: &str) -> Option<String> {
    let calls = serde_json::from_str::<CallNames>(&envelope.content).ok()?;
    calls
        .tool_calls
        .into_iter()
        .find(|call| call.id.as_deref() == Some(call_id))
        .and_then(|call| call.name.or_else(|| call.function.and_then(|function| function.name)))
}

/// The id of the stored document a result points at, when its output was
/// stored at record time: the reference block PRX appends as the last thing of
/// the result. A block the tool's own output holds earlier is not read. The
/// stub validates the id's shape.
fn document_id_of(content: &str) -> Option<&str> {
    let body = content.trim_end().strip_suffix("[/document_ingest_ref]")?;
    let (_, block) = body.rsplit_once("[document_ingest_ref]")?;
    block
        .lines()
        .find_map(|line| line.strip_prefix("document_id: "))
        .map(str::trim)
}

/// The tool results the step may clear, oldest first, as (round envelope
/// index, result index).
fn candidates(view: &[ChatMessage], turn_history: &TurnHistory) -> Vec<(usize, usize)> {
    let Some(current) = turn_history.current_turn_start(view) else {
        return Vec::new();
    };
    let mut rounds: Vec<(usize, Range<usize>)> = Vec::new();
    let mut index = 0usize;
    while index < current {
        let Some(message) = view.get(index) else { break };
        if !is_tool_call_envelope(message) {
            index = index.saturating_add(1);
            continue;
        }
        let start = index.saturating_add(1);
        let mut end = start;
        while end < current && view.get(end).is_some_and(|result| result.role == "tool") {
            end = end.saturating_add(1);
        }
        rounds.push((index, start..end));
        index = end;
    }
    let clearable = rounds.len().saturating_sub(KEEP_RECENT_TOOL_ROUNDS);
    rounds
        .into_iter()
        .take(clearable)
        .flat_map(|(envelope, results)| results.map(move |result| (envelope, result)))
        .collect()
}

/// The stub for the result at `result` of the round opened at `envelope`, or
/// `None` when the result is already cleared or cannot be read.
fn stub_for(view: &[ChatMessage], envelope: usize, result: usize) -> Option<ChatMessage> {
    let message = view.get(result)?;
    if message.content.contains(CLEARED_PREFIX) {
        return None;
    }
    let payload = serde_json::from_str::<ResultPayload>(&message.content).ok()?;
    let call_id = payload.tool_call_id?;
    let content = payload.content?;
    let tool = view
        .get(envelope)
        .and_then(|round| tool_name_of(round, &call_id))
        .unwrap_or_else(|| "unknown".to_owned());
    let status = if content.starts_with("Error") { "error" } else { "ok" };
    let bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
    let text = cleared_result_text(&tool, status, bytes, document_id_of(&content));
    Some(ChatMessage::tool(
        serde_json::json!({ "tool_call_id": call_id, "content": text }).to_string(),
    ))
}

/// Clear old tool results of `view` until it fits under the warning threshold
/// of `config`. Returns how many results were cleared.
fn clear_old_results(
    turn_history: &mut TurnHistory,
    view: &mut Vec<ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
) -> usize {
    let budget = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    if !budget.over_warning {
        return 0;
    }
    let mut excess = budget.used_tokens.saturating_sub(budget.warning_threshold_tokens);
    let mut replacements: Vec<(usize, ChatMessage)> = Vec::new();
    for (envelope, result) in candidates(view, turn_history) {
        if excess == 0 {
            break;
        }
        let Some(stub) = stub_for(view, envelope, result) else {
            continue;
        };
        let Some(old) = view.get(result) else { continue };
        let saved = estimate_history_tokens(std::slice::from_ref(old))
            .saturating_sub(estimate_history_tokens(std::slice::from_ref(&stub)));
        if saved == 0 {
            continue;
        }
        excess = excess.saturating_sub(saved);
        replacements.push((result, stub));
    }
    if replacements.is_empty() {
        return 0;
    }
    let tokens_before = estimate_history_tokens(view);
    let cleared = replacements.len();
    turn_history.reshape(view, |view| {
        for (index, stub) in replacements {
            if let Some(slot) = view.get_mut(index) {
                *slot = stub;
            }
        }
    });
    let tokens_after = estimate_history_tokens(view);
    tracing::info!(
        results_cleared = cleared,
        tokens_before,
        tokens_after,
        "cleared old tool results from the request"
    );
    crate::hooks::notify(
        HookEvent::ReductionFinished,
        serde_json::json!({
            "step": "clear_tool_results",
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
            "results_cleared": cleared,
        }),
    );
    cleared
}

/// Whether the view holds a tool result the step could clear. A cheap scan:
/// the budget is only measured when this holds.
fn has_candidates(view: &[ChatMessage], turn_history: &TurnHistory) -> bool {
    view.iter().any(|message| message.role == "tool") && !candidates(view, turn_history).is_empty()
}

/// Clear old tool results on every surface that runs the engine: before the
/// loop opens and on each provider round, ahead of any handoff or trim.
pub(crate) struct ClearOldResults;

#[async_trait]
impl<'a> Reducer<PreTurn<'a>> for ClearOldResults {
    fn name(&self) -> &'static str {
        "clear_old_results"
    }

    fn tier(&self) -> Tier {
        Tier::Clear
    }

    fn applies(&self, state: &PreTurn<'a>) -> bool {
        has_candidates(state.history, state.turn_history)
    }

    async fn apply(&self, state: &mut PreTurn<'a>) -> Result<Outcome> {
        let cleared = clear_old_results(state.turn_history, state.history, state.config);
        Ok(outcome(cleared))
    }
}

#[async_trait]
impl<'a> Reducer<Round<'a>> for ClearOldResults {
    fn name(&self) -> &'static str {
        "clear_old_results"
    }

    fn tier(&self) -> Tier {
        Tier::Clear
    }

    fn applies(&self, state: &Round<'a>) -> bool {
        has_candidates(state.history, state.turn_history)
    }

    async fn apply(&self, state: &mut Round<'a>) -> Result<Outcome> {
        let reserved_config = preflight_config(state.config, state.tool_surface);
        let cleared = clear_old_results(state.turn_history, state.history, &reserved_config);
        Ok(outcome(cleared))
    }
}

fn outcome(cleared: usize) -> Outcome {
    Outcome {
        reason: Reason::OverWarning,
        removed: Removed {
            items: cleared,
            ..Removed::default()
        },
    }
}

//! Tool round-trip companions: the messages of a live tool round that have no
//! row of their own, and the cut that never separates them from their request.

use crate::providers::ChatMessage;

/// True when `message` exists only inside a live tool round-trip and therefore
/// never became its own durable `message.created` event.
///
/// The loop appends three such shapes: the `role="tool"` result envelopes
/// (native mode), the `[Tool results]` user block (prompt mode), and the
/// assistant envelope carrying native `tool_calls`. They belong to the
/// assistant turn that requested them, so transcript provenance folds them into
/// that turn instead of refusing to resolve the whole cold window. Before this,
/// any agentic session was permanently unable to compact in `switch` mode.
pub(crate) fn is_tool_roundtrip_companion(message: &ChatMessage) -> bool {
    match message.role.as_str() {
        "tool" => true,
        "user" => message
            .content
            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX),
        "assistant" => {
            message.content.starts_with('{')
                && message.content.contains("\"tool_calls\"")
                && serde_json::from_str::<serde_json::Value>(&message.content).is_ok_and(|value| {
                    value
                        .get("tool_calls")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|calls| !calls.is_empty())
                })
        }
        _ => false,
    }
}

/// Which messages of a compaction source are tool round-trip companions
/// ([`is_tool_roundtrip_companion`]): folded into the assistant turn that
/// requested them rather than resolved to a transcript event of their own.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoundCompanions<'a> {
    /// Every message is an acknowledged session turn: none is a companion,
    /// whatever its text.
    Acknowledged,
    /// One flag per message, from the canonical history's provenance
    /// ([`crate::agent::turn_history::CanonicalHistory::tool_round_companions`]).
    Marked(&'a [bool]),
}

impl RoundCompanions<'_> {
    pub(crate) fn is_companion(self, index: usize) -> bool {
        match self {
            Self::Acknowledged => false,
            Self::Marked(flags) => flags.get(index).copied().unwrap_or_default(),
        }
    }
}

/// True when a companion `message` is the *request* half of a tool round-trip: the assistant
/// envelope carrying native `tool_calls`, or the prompt-mode `[Tool results]`
/// block that stands in for one when the request itself is a durable message.
///
/// Counting request halves (rather than every companion) makes "round-trips" a
/// number the model and the user can reason about: one per tool batch the
/// window contains.
pub(crate) fn is_tool_roundtrip_request(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
}

/// Names of the tools called inside `message`, in call order.
pub(crate) fn tool_names_in_roundtrip_request(message: &ChatMessage) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) else {
        return Vec::new();
    };
    let Some(calls) = value.get("tool_calls").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            call.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_string)
        })
        .collect()
}

/// How many tool round-trips a switch handoff folds away, and which tools they
/// called.
///
/// A `switch` handoff is an event-id pointer, not a summary, and tool results
/// never became events of their own. Everything this counts is therefore
/// verbatim context that no lookup can bring back, which is precisely what the
/// note has to say out loud.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldedToolRoundtrips {
    pub(crate) count: usize,
    pub(crate) tools: Vec<String>,
}

/// Names listed in the handoff note. Beyond this the list stops being a hint and
/// starts being the payload it is meant to replace.
pub(crate) const FOLDED_TOOL_NAME_LIMIT: usize = 12;

pub(crate) fn folded_tool_roundtrips(
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> FoldedToolRoundtrips {
    let mut count = 0usize;
    let mut tools: Vec<String> = Vec::new();
    for message in source_messages
        .iter()
        .enumerate()
        .filter(|(index, message)| companions.is_companion(*index) && is_tool_roundtrip_request(message))
        .map(|(_, message)| message)
    {
        count = count.saturating_add(1);
        for name in tool_names_in_roundtrip_request(message) {
            if !tools.iter().any(|existing| *existing == name) {
                tools.push(name);
            }
        }
    }
    tools.truncate(FOLDED_TOOL_NAME_LIMIT);
    FoldedToolRoundtrips { count, tools }
}

/// The one line the model and the user read to learn what the window lost.
pub(crate) fn format_folded_tool_roundtrips_notice(folded: &FoldedToolRoundtrips) -> String {
    if folded.count == 0 {
        return "no tool round-trips were folded into this handoff".to_string();
    }
    let tools = if folded.tools.is_empty() {
        "unknown".to_string()
    } else {
        folded.tools.join(", ")
    };
    format!(
        "{} tool round-trips (tools: {tools}) were folded into this handoff and are not recoverable in this window",
        folded.count
    )
}

/// The end of a switch's cold window `start..requested_end`, capped so it
/// holds at most [`MAX_REFERENCED_EVENTS`] durable messages (every message
/// that is not a tool round companion resolves to one transcript event).
///
/// A handoff references at most that many events, so a larger cold window
/// would be refused (`bound:too_many_events`) on every rollover. Folding the
/// oldest messages up to the bound instead leaves the rest in the window for
/// a later switch. The capped end is the next durable message, so the hot
/// window still starts on a durable turn.
///
/// [`MAX_REFERENCED_EVENTS`]: crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS
pub(crate) fn switch_cold_window_end(companions: &[bool], start: usize, requested_end: usize) -> usize {
    let mut durable = 0usize;
    for index in start..requested_end {
        if companions.get(index).copied().unwrap_or_default() {
            continue;
        }
        if durable == crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS {
            return index;
        }
        durable = durable.saturating_add(1);
    }
    requested_end
}

/// Move the compaction boundary backwards so the retained hot window never
/// starts in the middle of a tool round-trip.
///
/// A `role="tool"` result whose requesting assistant message was compacted away
/// is an orphan every native provider rejects, so the boundary walks back until
/// the first retained message is a durable user/assistant turn.
pub(crate) fn compaction_boundary_without_orphan_tool_results(
    companions: &[bool],
    start: usize,
    compact_end: usize,
) -> usize {
    let mut boundary = compact_end;
    while boundary > start {
        let Some(first_retained) = companions.get(boundary) else {
            break;
        };
        if !*first_retained {
            break;
        }
        boundary = boundary.saturating_sub(1);
    }
    if boundary > start {
        return boundary;
    }
    // Walking back reached `start`: the whole requested cold window sits inside
    // one long tool round-trip chain. Giving up here returned "nothing to
    // compact" on every rollover, so `switch` silently degraded to the lossy
    // trim for the rest of the session. Walk forward to the first retained
    // message that can stand at the head of the hot window instead — the cold
    // window grows, but it still folds into one handoff.
    let mut forward = compact_end;
    while forward < companions.len() {
        if !companions.get(forward).copied().unwrap_or_default() {
            return forward;
        }
        forward = forward.saturating_add(1);
    }
    // Every message after `start` is a companion: there is no durable turn to
    // retain and nothing safe to cut.
    start
}

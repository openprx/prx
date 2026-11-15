//! A scripted model for the chat compaction tests (CF-R5).
//!
//! It answers by reading each real request body: a planted fact is
//! acknowledged, `read <file>` turns call `file_read` (tool-heavy history), and
//! a recall question follows the context-switch handoff's own lookup pointer
//! with a `transcript_history_lookup` call. The recall reply quotes the fact
//! only when the lookup result returned it from the real SQLite transcript.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use crate::loopback_openai::{Script, Scripted};

/// The fact planted in the first turn and recalled after the switch.
pub const CODENAME: &str = "AZURE-HERON-77";
/// The first turn: plants the fact.
pub const PLANT: &str = "Remember this: the codename is AZURE-HERON-77.";
/// The recall question.
pub const RECALL: &str = "What is the codename?";
/// Reply to a recall whose lookup returned the fact.
pub const RECALLED: &str = "RECALLED-AZURE-HERON-77";
/// Reply to every other turn that is not a tool round-trip.
pub const NOTED: &str = "NOTED-OK";
/// Reply prefix after a `file_read` result; the file name follows.
pub const READ_DONE: &str = "READ-DONE-";
/// A user line prefix: look up exactly the JSON arguments that follow.
pub const LOOKUP_EXACTLY: &str = "lookup exactly ";
/// Reply to a lookup the tool refused (another session's rows).
pub const LOOKUP_REFUSED: &str = "LOOKUP-REFUSED";
/// The lookup tool's refusal of rows outside the current session.
const LOOKUP_REFUSAL: &str = "outside the authenticated session";
/// Name of the recovery tool the handoff points at.
pub const LOOKUP_TOOL: &str = "transcript_history_lookup";
/// Markers around the handoff note.
pub const HANDOFF_START: &str = "[context_handoff]";
pub const HANDOFF_END: &str = "[/context_handoff]";
/// The small window of the automatic switch case, in tokens.
///
/// Sized from the measured `llm.request.context.compiled` estimates
/// (cl100k proxy) so the history has the room it had before the chat surface
/// published the four direct sessions tools (CF-R6): without them the tool
/// reserve is 540 native + 116 section + 4 framing = 660 tokens, with them
/// 2,212 + 132 + 4 = 2,348 (+1,688). A 4,000 window then left
/// 4,000 - 256 - 2,348 = 1,396 tokens, 531 of them the system prompt, too
/// little for the handoff plus one tool-heavy turn, so every switch ended in a
/// lossy trim. 5,700 restores the old history room:
/// 5,700 - 256 - 2,348 = 3,096 (was 4,000 - 256 - 660 = 3,084).
pub const SMALL_WINDOW_TOKENS: usize = 5_700;
/// The window of the long switch, branch and restart cases, in tokens.
///
/// Every turn's request also quotes the session's own stored events in a
/// `[Current session context]` preamble (capped at 12,288 bytes, about 3,500
/// tokens). A session with many stored events (a long history, handoffs, a
/// branch's copied rows) therefore needs that much more room than
/// [`SMALL_WINDOW_TOKENS`], or every request ends in a lossy trim:
/// 256 reserve + 2,348 tools + ~830 system + ~3,500 preamble leaves
/// 9,500 - 6,934 = ~2,566 tokens of history, so a few tool-heavy turns still
/// force a switch and the handoff plus the kept tail fit afterwards.
pub const RECALL_WINDOW_TOKENS: usize = 9_500;
/// Bytes of each tool-heavy fixture file (about 500 tokens of prose).
pub const FILLER_BYTES: usize = 2_000;

/// Write `count` filler files `f0.txt…` into `workspace` (no codename inside).
pub fn write_filler_files(workspace: &std::path::Path, count: usize) {
    for index in 0..count {
        let line = format!("filler {index} lorem ipsum dolor sit amet consectetur adipiscing elit. ");
        let body = line.repeat(FILLER_BYTES / line.len() + 1);
        std::fs::write(workspace.join(format!("f{index}.txt")), &body[..FILLER_BYTES]).expect("test: filler file");
    }
}

/// The `read f<i>.txt` user line for filler file `index`.
pub fn read_line(index: usize) -> String {
    format!("read f{index}.txt")
}

/// The reply that completes the `read` turn of filler file `index`.
pub fn read_done(index: usize) -> String {
    format!("{READ_DONE}f{index}.txt")
}

/// The `path` argument of the newest `file_read` call in `messages`.
fn newest_read_path(messages: &[serde_json::Value]) -> String {
    messages
        .iter()
        .rev()
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
        .find_map(|call| {
            let arguments: serde_json::Value = serde_json::from_str(call["function"]["arguments"].as_str()?).ok()?;
            arguments["path"].as_str().map(str::to_owned)
        })
        .unwrap_or_default()
}

fn text_of(message: &serde_json::Value) -> String {
    match &message["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// The newest handoff note in `request`, if any.
pub fn handoff_note(request: &serde_json::Value) -> Option<serde_json::Value> {
    request["messages"].as_array()?.iter().rev().find_map(|message| {
        let text = text_of(message);
        let start = text.find(HANDOFF_START)? + HANDOFF_START.len();
        let end = text[start..].find(HANDOFF_END)? + start;
        serde_json::from_str(text[start..end].trim()).ok()
    })
}

/// The lookup arguments of the newest handoff note in `request`, if any.
pub fn handoff_lookup_arguments(request: &serde_json::Value) -> Option<serde_json::Value> {
    handoff_note(request).map(|note| note["lookup"]["arguments"].clone())
}

/// The lookup arguments of the newest handoff note among the events of a
/// lookup result (`text` is the tool output JSON).
fn nested_handoff_lookup(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let output: serde_json::Value = serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    output["events"]
        .as_array()?
        .iter()
        .filter_map(|event| {
            let content = event["content"].as_str()?;
            let start = content.find(HANDOFF_START)? + HANDOFF_START.len();
            let end = content[start..].find(HANDOFF_END)? + start;
            serde_json::from_str::<serde_json::Value>(content[start..end].trim()).ok()
        })
        .max_by_key(|note| note["to_generation"].as_u64().unwrap_or(0))
        .map(|note| note["lookup"]["arguments"].clone())
}

/// Whether `request` publishes `name` as a tool.
pub fn publishes(request: &serde_json::Value, name: &str) -> bool {
    request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
}

/// The script described in the module docs.
pub fn script() -> Script {
    Box::new(|index, request| {
        let messages = request["messages"].as_array().cloned().unwrap_or_default();
        let Some(last) = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user" || message["role"] == "tool")
        else {
            return Scripted::text(NOTED);
        };
        let content = text_of(last);
        if last["role"] == "tool" {
            if content.contains(CODENAME) {
                return Scripted::text(RECALLED);
            }
            if content.contains(LOOKUP_REFUSAL) {
                return Scripted::text(LOOKUP_REFUSED);
            }
            // A lookup that returned an older handoff: follow its pointer.
            if let Some(arguments) = nested_handoff_lookup(&content) {
                return Scripted::tool(&format!("call-lookup-{index}"), LOOKUP_TOOL, &arguments);
            }
            return Scripted::text(&format!("{READ_DONE}{}", newest_read_path(&messages)));
        }
        if let Some(raw) = content
            .rfind(LOOKUP_EXACTLY)
            .map(|at| &content[at + LOOKUP_EXACTLY.len()..])
        {
            let arguments: serde_json::Value = serde_json::from_str(raw.trim()).expect("test: lookup arguments JSON");
            return Scripted::tool(&format!("call-lookup-{index}"), LOOKUP_TOOL, &arguments);
        }
        if content.contains(RECALL) {
            return match handoff_lookup_arguments(request) {
                Some(arguments) if publishes(request, LOOKUP_TOOL) => {
                    Scripted::tool(&format!("call-lookup-{index}"), LOOKUP_TOOL, &arguments)
                }
                _ => Scripted::text("NO-HANDOFF"),
            };
        }
        // The newest `read` instruction: recalled memory may prefix older ones.
        if let Some(file) = content
            .rfind("read f")
            .and_then(|at| content[at + "read ".len()..].split_whitespace().next())
        {
            return Scripted::tool(
                &format!("call-read-{index}"),
                "file_read",
                &serde_json::json!({ "path": file }),
            );
        }
        Scripted::text(NOTED)
    })
}

/// A request is a main turn request carrying the handoff and the lookup tool.
pub fn carries_handoff_and_lookup(request: &serde_json::Value) -> bool {
    handoff_lookup_arguments(request).is_some() && publishes(request, LOOKUP_TOOL)
}

/// Counts of the context-switch handoff and `handoff_unavailable` degradation
/// events in the workspace's real SQLite store.
pub fn switch_event_counts(workspace: &std::path::Path) -> (usize, usize) {
    let events = crate::loopback_openai::message_events(workspace);
    let handoffs = events
        .iter()
        .filter(|event| event.event_type == "context.switch.created")
        .count();
    let unavailable = events
        .iter()
        .filter(|event| {
            event.event_type == "context.compaction.degraded" && event.content.contains("handoff_unavailable")
        })
        .count();
    (handoffs, unavailable)
}

/// The chat session id of the first persisted user message
/// (`chat:terminal:local-user:<id>` key).
pub fn chat_session_id(workspace: &std::path::Path) -> String {
    let database = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store");
    let key: String = database
        .query_row(
            "SELECT session_key FROM message_events WHERE role = 'user' AND session_key LIKE 'chat:%' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("test: chat session key");
    key.rsplit(':').next().unwrap_or_default().to_owned()
}

/// Persisted assistant replies so far (a `/branch` copy of an earlier reply is
/// not a reply); 0 while the store or its schema does not exist yet (the chat
/// process creates both on start).
pub fn assistant_reply_count(workspace: &std::path::Path) -> usize {
    let Ok(database) = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return 0;
    };
    database
        .query_row(
            "SELECT COUNT(*) FROM message_events WHERE role = 'assistant' AND event_type = 'message.created' \
             AND (idempotency_key IS NULL OR idempotency_key NOT LIKE 'branch-copy:%')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_or(0, |count| usize::try_from(count).unwrap_or(0))
}

/// One stored context-switch handoff row.
pub struct StoredHandoff {
    pub event_id: String,
    pub session_key: String,
    /// The handoff note (JSON between the handoff markers).
    pub note: serde_json::Value,
}

fn open_store(workspace: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store")
}

/// Every `context.switch.created` row of the real store, oldest first.
pub fn stored_handoffs(workspace: &std::path::Path) -> Vec<StoredHandoff> {
    let database = open_store(workspace);
    let mut statement = database
        .prepare(
            "SELECT event_id, COALESCE(session_key, ''), content FROM message_events \
             WHERE event_type = 'context.switch.created' ORDER BY id",
        )
        .expect("test: handoff query");
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("test: handoff rows")
        .map(|row| {
            let (event_id, session_key, content) = row.expect("test: handoff row");
            let start = content.find(HANDOFF_START).expect("test: handoff start marker") + HANDOFF_START.len();
            let end = content[start..].find(HANDOFF_END).expect("test: handoff end marker") + start;
            let note = serde_json::from_str(content[start..end].trim()).expect("test: handoff note JSON");
            StoredHandoff {
                event_id,
                session_key,
                note,
            }
        })
        .collect()
}

/// The handoffs of the session whose key ends with `session_id`.
pub fn session_handoffs(workspace: &std::path::Path, session_id: &str) -> Vec<StoredHandoff> {
    stored_handoffs(workspace)
        .into_iter()
        .filter(|handoff| handoff.session_key.ends_with(session_id))
        .collect()
}

/// Assert `handoffs` form one chain: generation 1, 2, … with each parent the
/// previous row's event id.
pub fn assert_handoff_chain(handoffs: &[StoredHandoff], min_generation: u64) {
    assert!(
        handoffs.len() as u64 >= min_generation,
        "at least {min_generation} handoffs; found {}",
        handoffs.len()
    );
    let mut parent: Option<&str> = None;
    for (index, handoff) in handoffs.iter().enumerate() {
        let generation = index as u64 + 1;
        assert_eq!(
            handoff.note["to_generation"], generation,
            "handoff {index}: {}",
            handoff.note
        );
        assert_eq!(
            handoff.note["handoff_event_id"],
            handoff.event_id.as_str(),
            "handoff {index}"
        );
        match parent {
            None => assert!(
                handoff.note["parent_handoff_event_id"].is_null(),
                "generation 1 has no parent"
            ),
            Some(parent) => assert_eq!(
                handoff.note["parent_handoff_event_id"], parent,
                "generation {generation} points at its parent"
            ),
        }
        parent = Some(handoff.event_id.as_str());
    }
}

/// Content of the stored user message that planted the fact.
pub fn planted_row_content(workspace: &std::path::Path) -> String {
    open_store(workspace)
        .query_row(
            "SELECT content FROM message_events WHERE role = 'user' AND event_type = 'message.created' \
             AND content LIKE ?1 ORDER BY id LIMIT 1",
            [format!("%{CODENAME}%")],
            |row| row.get(0),
        )
        .expect("test: planted user row")
}

/// Whether any stored event records the `over_hard_limit_after_handoff`
/// degradation.
pub fn over_hard_limit_after_handoff(workspace: &std::path::Path) -> bool {
    crate::loopback_openai::message_events(workspace)
        .iter()
        .any(|event| event.content.contains("over_hard_limit_after_handoff"))
}

/// The content of every `role: tool` message of every request that carries
/// the codename (the lookup results).
pub fn lookup_results(requests: &[serde_json::Value]) -> Vec<String> {
    requests
        .iter()
        .filter_map(|request| request["messages"].as_array())
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(text_of)
        .filter(|text| text.contains(CODENAME))
        .collect()
}

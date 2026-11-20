//! Cross-turn tool evidence in `prx chat` over a loopback OpenAI-compatible
//! provider (real HTTP/SSE bytes) with a real config dir and SQLite store.
//!
//! Every assistant reply recorded through a driver turn is replayed to the
//! provider with a runtime-written `[tool evidence: …]` line: the tools that
//! really ran in that reply, or `none`. The line is the same bytes in-process,
//! after a restart and in a one-shot resume, and it never reaches the saved
//! turn text or a transcript row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use std::time::{Duration, Instant};

use loopback_openai::{LoopbackOpenAi, Script, Scripted, write_config};
use plain_chat::{Output, Running, Sandbox};

const MODEL: &str = "prx-evidence-model";
/// Longest wait for one turn's reply to be stored.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
/// Polling interval of the real SQLite store.
const POLL: Duration = Duration::from_millis(50);
/// Bound on the saved-session read.
const READ_DEADLINE: Duration = Duration::from_secs(30);
/// Most saved sessions the read takes; the test expects one.
const MAX_SAVED_SESSIONS: usize = 4;
/// The opening of every evidence line.
const EVIDENCE: &str = "[tool evidence:";

/// The first turn's reply, after its real `shell` call.
const EV_1: &str = "EV-1";
/// The replayed first reply.
const EV_1_REPLAYED: &str = "EV-1\n\n[tool evidence: shell ok]";
/// The second turn's reply (no tool).
const EV_2: &str = "EV-2";
/// The replayed second reply.
const EV_2_REPLAYED: &str = "EV-2\n\n[tool evidence: none]";

/// The text of the newest user message of `request`.
fn newest_user_text(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// `run echo ev-1` calls `shell` and answers `EV-1` after its result; every
/// other `say ev-N` line is answered `EV-N` without a tool. The newest user
/// message may open with a recall preamble quoting earlier lines, so a line
/// is recognized by how the message ends.
fn script() -> Script {
    Box::new(|index, request| {
        let last_is_tool = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|message| message["role"] == "tool");
        let user = newest_user_text(request);
        let line = user.trim_end();
        if line.ends_with("run echo ev-1") {
            return if last_is_tool {
                Scripted::text(EV_1)
            } else {
                Scripted::tool(
                    &format!("call-shell-{index}"),
                    "shell",
                    &serde_json::json!({"command": "echo ev-1"}),
                )
            };
        }
        for (marker, reply) in [("say ev-2", EV_2), ("say ev-3", "EV-3"), ("say ev-4", "EV-4")] {
            if line.ends_with(marker) {
                return Scripted::text(reply);
            }
        }
        Scripted::text("UNSCRIPTED")
    })
}

/// A Full-autonomy config against the evidence script.
fn serve(sandbox: &Sandbox) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((5, 3)), script());
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        // The history alone carries the earlier turns.
        config.memory.auto_save = false;
    });
    server
}

/// Wait until the store holds `count` assistant replies.
fn wait_for_replies(sandbox: &Sandbox, count: usize) {
    let started = Instant::now();
    // 0 until the chat process has created the store.
    while compaction_script::assistant_reply_count(&sandbox.workspace) < count {
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "reply {count} was not stored within TURN_DEADLINE"
        );
        std::thread::sleep(POLL);
    }
}

/// Send `line` to a held chat and wait for reply number `replies`.
fn turn(sandbox: &Sandbox, chat: &mut Running, line: &str, replies: usize) {
    chat.send(line);
    wait_for_replies(sandbox, replies);
}

fn finish(mut chat: Running) -> Output {
    chat.send("/exit");
    let output = chat.wait();
    assert_eq!(
        output.status.code(),
        Some(0),
        "chat exits cleanly; stderr: {}",
        output.stderr
    );
    assert!(!output.stderr.contains("panicked"), "stderr: {}", output.stderr);
    output
}

/// The id from the plain front end's `current chat session: <id>` line.
fn current_session_id(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("current chat session: "))
        .map_or_else(
            || panic!("no session id line in stderr: {stderr}"),
            |id| id.trim().to_owned(),
        )
}

/// The assistant message texts of `request`, in order.
fn assistant_texts(request: &serde_json::Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "assistant")
                .filter_map(|message| message["content"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The first request whose newest user line contains `marker`.
fn request_for<'a>(requests: &'a [serde_json::Value], marker: &str) -> &'a serde_json::Value {
    requests
        .iter()
        .find(|request| newest_user_text(request).trim_end().ends_with(marker))
        .unwrap_or_else(|| panic!("no request for {marker}"))
}

/// Three turns in one held `chat --plain`: a real `shell` turn and two
/// tool-less turns. Returns the session id.
fn three_turns(sandbox: &Sandbox) -> String {
    let mut chat = Running::spawn(sandbox.command(&["chat", "--plain"]));
    turn(sandbox, &mut chat, "run echo ev-1", 1);
    turn(sandbox, &mut chat, "say ev-2", 2);
    turn(sandbox, &mut chat, "say ev-3", 3);
    let output = finish(chat);
    current_session_id(&output.stderr)
}

/// The saved session row, read through the real SQLite memory.
fn saved_session(sandbox: &Sandbox) -> serde_json::Value {
    use openprx::memory::traits::Memory as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: runtime");
    let memory = openprx::memory::sqlite::SqliteMemory::new(&sandbox.workspace).expect("test: open the real memory");
    let entries = runtime
        .block_on(async {
            tokio::time::timeout(
                READ_DEADLINE,
                memory.list_key_prefix("chat_session:", MAX_SAVED_SESSIONS),
            )
            .await
        })
        .expect("test: the session read finishes within READ_DEADLINE")
        .expect("test: list saved sessions");
    assert_eq!(entries.len(), 1, "one saved session");
    serde_json::from_str(&entries[0].content).expect("test: session row JSON")
}

#[test]
fn next_turn_request_carries_evidence_of_the_previous_reply() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox);
    three_turns(&sandbox);

    let requests = server.requests();
    assert_eq!(
        assistant_texts(request_for(&requests, "say ev-2")),
        vec![EV_1_REPLAYED.to_owned()],
        "the second turn's request replays the shell reply with its evidence"
    );
    assert_eq!(
        assistant_texts(request_for(&requests, "say ev-3")),
        vec![EV_1_REPLAYED.to_owned(), EV_2_REPLAYED.to_owned()],
        "the third turn's request replays both replies, the tool-less one with none"
    );
}

#[test]
fn resumed_session_replays_identical_evidence_bytes() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox);
    let session = three_turns(&sandbox);

    let mut resumed = Running::spawn(sandbox.command(&["chat", "--plain", "--session", &session]));
    turn(&sandbox, &mut resumed, "say ev-4", 4);
    let output = finish(resumed);
    assert_eq!(
        current_session_id(&output.stderr),
        session,
        "the resumed chat keeps its session"
    );

    let requests = server.requests();
    let in_process = assistant_texts(request_for(&requests, "say ev-3"));
    let after_restart = assistant_texts(request_for(&requests, "say ev-4"));
    assert_eq!(
        after_restart.get(..in_process.len()),
        Some(in_process.as_slice()),
        "a new process replays the earlier replies byte for byte"
    );
    assert_eq!(
        after_restart.get(2).map(String::as_str),
        Some("EV-3\n\n[tool evidence: none]"),
        "the last in-process reply is replayed with its evidence too"
    );
}

#[test]
fn evidence_never_reaches_saved_turns_rows_or_lookup() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox);
    three_turns(&sandbox);

    let session = saved_session(&sandbox);
    let turns = session["turns"].as_array().expect("test: saved turns");
    assert!(
        turns
            .iter()
            .all(|turn| !turn["content"].as_str().unwrap_or_default().contains(EVIDENCE)),
        "no saved turn text holds evidence: {turns:?}"
    );
    let recorded: Vec<&serde_json::Value> = turns
        .iter()
        .filter(|turn| turn["role"] == "assistant")
        .map(|turn| &turn["tool_evidence"])
        .collect();
    assert_eq!(recorded.len(), 3, "three replies are saved");
    assert_eq!(
        recorded[0]["groups"][0]["name"], "shell",
        "the shell reply's record is saved: {recorded:?}"
    );
    assert!(
        recorded[1].is_object() && recorded[1]["groups"].as_array().is_some_and(Vec::is_empty),
        "the tool-less reply's empty record is saved: {recorded:?}"
    );
    assert!(
        loopback_openai::message_events(&sandbox.workspace)
            .iter()
            .filter(|event| event.event_type == "message.created")
            .all(|event| !event.content.contains(EVIDENCE)),
        "no transcript row (what the lookup tool reads) holds evidence"
    );
}

#[test]
fn one_shot_message_with_session_replays_evidence() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox);
    let session = three_turns(&sandbox);

    let output = Running::spawn(sandbox.command(&["chat", "--message", "say ev-4", "--session", &session])).wait();
    assert!(
        output.status.success(),
        "the one-shot succeeds; stderr: {}",
        output.stderr
    );
    assert_eq!(output.stdout, "EV-4\n", "stdout is the reply alone");

    let requests = server.requests();
    let one_shot = assistant_texts(request_for(&requests, "say ev-4"));
    assert_eq!(
        one_shot.first().map(String::as_str),
        Some(EV_1_REPLAYED),
        "the one-shot resume replays the shell reply with its evidence: {one_shot:?}"
    );
    assert_eq!(
        one_shot.get(1).map(String::as_str),
        Some(EV_2_REPLAYED),
        "and the tool-less reply with none: {one_shot:?}"
    );
}

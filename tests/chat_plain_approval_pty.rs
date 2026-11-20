//! Supervised `file_edit` and `shell` in `prx chat --plain` on a real PTY over
//! a loopback provider (real HTTP/SSE): the terminal prompt `[y/N/a]` approves
//! once, denies, covers the session's later edits, and Ctrl-C at the prompt
//! cancels only the turn (CF-R3, CR-2). Every resolution leaves one durable
//! `tool.approval.resolved` row; a denial tells the operator and the model
//! that the call did not run. Full autonomy runs without a prompt or a row.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;
#[path = "support/pty.rs"]
mod pty;

use loopback_openai::{LoopbackOpenAi, Script, Scripted, approval_rows, reserved_rows, tool_result_text};
use plain_chat::Sandbox;
use pty::Pty;

const MODEL: &str = "prx-loop-model";
const ORIGINAL: &str = "alpha\nbeta\ngamma\ndelta\n";
const PROMPT: &str = "[y/N/a]";

fn edit_call(id: &str, old: &str, new: &str) -> Scripted {
    Scripted::tool(
        id,
        "file_edit",
        &serde_json::json!({"path": "notes.txt", "old_string": old, "new_string": new}),
    )
}

/// A `chat --plain` at `level` on a PTY with `notes.txt` = [`ORIGINAL`].
fn start_at(sandbox: &Sandbox, level: openprx::security::AutonomyLevel, script: Script) -> (LoopbackOpenAi, Pty) {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    loopback_openai::write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = level;
    });
    std::fs::write(sandbox.workspace.join("notes.txt"), ORIGINAL).expect("test: notes.txt");
    let mut pty = Pty::spawn(sandbox.command(&["chat", "--plain"]));
    pty.read_until(0, "prx>");
    (server, pty)
}

/// A Supervised `chat --plain` on a PTY with `notes.txt` = [`ORIGINAL`].
fn start(sandbox: &Sandbox, script: Script) -> (LoopbackOpenAi, Pty) {
    start_at(sandbox, openprx::security::AutonomyLevel::Supervised, script)
}

fn notes(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(sandbox.workspace.join("notes.txt")).expect("test: read notes.txt")
}

/// What one answered edit turn left: the sandbox, the prompt capture, the
/// capture after the answer and the recorded provider requests.
struct EditTurn {
    sandbox: Sandbox,
    prompt: String,
    after_answer: String,
    requests: Vec<serde_json::Value>,
}

/// One edit turn answered with `answer` at the prompt, then `/exit`.
fn one_edit(answer: &str, reply: &str) -> EditTurn {
    let sandbox = Sandbox::new();
    let reply_text = reply.to_owned();
    let (server, mut pty) = start(
        &sandbox,
        Box::new(move |index, _| match index {
            0 => edit_call("call-edit", "beta", "BETA"),
            _ => Scripted::text(&reply_text),
        }),
    );
    pty.send("change beta to BETA in notes.txt\r");
    let prompt = pty.read_until(0, PROMPT);
    let mark = pty.mark();
    pty.send(&format!("{answer}\r"));
    let after_answer = pty.read_until(mark, reply);
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the tool call and the reply");
    EditTurn {
        sandbox,
        prompt,
        after_answer,
        requests,
    }
}

#[test]
fn y_edits_once() {
    let turn = one_edit("y", "EDIT-DONE");
    assert!(
        turn.prompt.contains("Approve file_edit"),
        "the prompt names the tool: {}",
        turn.prompt
    );
    assert_eq!(
        notes(&turn.sandbox),
        "alpha\nBETA\ngamma\ndelta\n",
        "only the target line changes"
    );
    assert_eq!(
        approval_rows(&turn.sandbox.workspace),
        ["operation_id=call-edit capability=file_edit verdict=approved"],
        "the approval is recorded once"
    );
}

#[test]
fn n_leaves_file() {
    let turn = one_edit("n", "EDIT-DENIED");
    assert_eq!(
        notes(&turn.sandbox),
        ORIGINAL,
        "a denied edit leaves the file unchanged"
    );
    assert_eq!(
        approval_rows(&turn.sandbox.workspace),
        ["operation_id=call-edit capability=file_edit verdict=denied cause=user_rejected"],
        "the denial is recorded once"
    );
    assert_eq!(
        tool_result_text(&turn.requests[1], "call-edit").as_deref(),
        Some("Denied: User rejected tool approval. The file_edit call did not run and produced no output."),
        "the model reads that the edit did not run"
    );
    assert!(
        turn.after_answer.contains("Denied file_edit"),
        "the operator sees the denial: {}",
        turn.after_answer
    );
}

#[test]
fn n_denies_shell_records_row_and_sends_explicit_result() {
    let sandbox = Sandbox::new();
    let (server, mut pty) = start(
        &sandbox,
        Box::new(|index, _| match index {
            0 => Scripted::tool(
                "call-shell",
                "shell",
                &serde_json::json!({"command": "echo plain-deny"}),
            ),
            _ => Scripted::text("SHELL-DENIED"),
        }),
    );
    pty.send("run echo plain-deny\r");
    let prompt = pty.read_until(0, PROMPT);
    assert!(prompt.contains("Approve shell"), "the prompt names the tool: {prompt}");
    let mark = pty.mark();
    pty.send("n\r");
    let after_answer = pty.read_until(mark, "SHELL-DENIED");
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");

    assert!(
        after_answer.contains("Denied shell"),
        "the operator sees the denial: {after_answer}"
    );
    assert_eq!(
        approval_rows(&sandbox.workspace),
        ["operation_id=call-shell capability=shell verdict=denied cause=user_rejected"],
        "the denial is recorded once"
    );
    assert!(
        !reserved_rows(&sandbox.workspace)
            .iter()
            .any(|row| row.starts_with("operation_id=call-shell ")),
        "a denied call is never reserved"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the tool call and the reply");
    assert_eq!(
        tool_result_text(&requests[1], "call-shell").as_deref(),
        Some("Denied: User rejected tool approval. The shell call did not run and produced no output."),
        "the model reads that the shell call did not run"
    );
}

#[test]
fn full_autonomy_shell_runs_without_prompt_or_approval_row() {
    let sandbox = Sandbox::new();
    let (server, mut pty) = start_at(
        &sandbox,
        openprx::security::AutonomyLevel::Full,
        Box::new(|index, _| match index {
            0 => Scripted::tool("call-full", "shell", &serde_json::json!({"command": "echo full-run"})),
            _ => Scripted::text("FULL-DONE"),
        }),
    );
    let mark = pty.mark();
    pty.send("run echo full-run\r");
    let turn = pty.read_until(mark, "FULL-DONE");
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");

    assert!(!turn.contains(PROMPT), "Full autonomy never prompts: {turn}");
    assert!(
        approval_rows(&sandbox.workspace).is_empty(),
        "Full autonomy writes no approval row"
    );
    assert!(
        loopback_openai::message_events(&sandbox.workspace)
            .iter()
            .any(|event| event.event_type == "tool.execution.finalized"
                && event.content.starts_with("operation_id=call-full status=Succeeded")),
        "the shell call ran and finalized as succeeded"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the tool call and the reply");
    assert!(
        tool_result_text(&requests[1], "call-full").is_some_and(|text| text.contains("full-run")),
        "the model reads the real shell output"
    );
}

#[test]
fn a_covers_second_edit() {
    let sandbox = Sandbox::new();
    let (server, mut pty) = start(
        &sandbox,
        Box::new(|index, _| match index {
            0 => edit_call("call-edit-1", "beta", "BETA"),
            1 => Scripted::text("FIRST-DONE"),
            2 => edit_call("call-edit-2", "delta", "DELTA"),
            _ => Scripted::text("SECOND-DONE"),
        }),
    );
    pty.send("change beta to BETA in notes.txt\r");
    pty.read_until(0, PROMPT);
    let mark = pty.mark();
    pty.send("a\r");
    pty.read_until(mark, "FIRST-DONE");
    let mark = pty.mark();
    pty.send("change delta to DELTA in notes.txt\r");
    let second = pty.read_until(mark, "SECOND-DONE");
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");

    assert!(
        !second.contains(PROMPT),
        "the session's second edit is not prompted: {second}"
    );
    assert_eq!(server.requests().len(), 4, "two tool calls and two replies");
    assert_eq!(
        approval_rows(&sandbox.workspace),
        [
            "operation_id=call-edit-1 capability=file_edit verdict=approved",
            "operation_id=call-edit-2 capability=file_edit verdict=approved",
        ],
        "the asked call and the session-allowed call are each recorded"
    );
    assert_eq!(
        notes(&sandbox),
        "alpha\nBETA\ngamma\nDELTA\n",
        "both target lines change"
    );
}

#[test]
fn ctrl_c_at_prompt_cancels_turn() {
    let sandbox = Sandbox::new();
    let (server, mut pty) = start(
        &sandbox,
        Box::new(|index, _| match index {
            0 => edit_call("call-edit", "beta", "BETA"),
            _ => Scripted::text("AFTER-CANCEL"),
        }),
    );
    pty.send("change beta to BETA in notes.txt\r");
    pty.read_until(0, PROMPT);
    let mark = pty.mark();
    pty.send("\u{3}");
    pty.read_until(mark, "approval cancelled");
    // The session continues: the next line is a new turn, not an answer.
    let mark = pty.mark();
    pty.send("are you there\r");
    pty.read_until(mark, "AFTER-CANCEL");
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");

    assert_eq!(notes(&sandbox), ORIGINAL, "the cancelled edit never ran");
    assert_eq!(
        approval_rows(&sandbox.workspace),
        ["operation_id=call-edit capability=file_edit verdict=cancelled"],
        "the cancelled approval is recorded once"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the cancelled turn's tool call and the next turn");
    let last_user = requests[1]["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_default();
    assert!(
        last_user.contains("are you there"),
        "the line after Ctrl-C reached the chat as a turn: {last_user}"
    );
}

/// The assistant message texts of one recorded provider request, in order.
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

#[test]
fn denied_shell_is_replayed_as_denied_evidence() {
    let sandbox = Sandbox::new();
    let (server, mut pty) = start(
        &sandbox,
        Box::new(|index, _| match index {
            0 => Scripted::tool(
                "call-shell",
                "shell",
                &serde_json::json!({"command": "echo t5-shell-deny"}),
            ),
            1 => Scripted::text("SHELL-DENIED"),
            _ => Scripted::text("NEXT-OK"),
        }),
    );
    pty.send("run echo t5-shell-deny\r");
    pty.read_until(0, PROMPT);
    let mark = pty.mark();
    pty.send("n\r");
    pty.read_until(mark, "SHELL-DENIED");
    let mark = pty.mark();
    pty.send("what did that command print\r");
    pty.read_until(mark, "NEXT-OK");
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "the tool call, the denied reply and the next turn");
    assert_eq!(
        assistant_texts(&requests[2]),
        ["SHELL-DENIED\n\n[tool evidence: shell denied(user_rejected)]"],
        "the next request replays the reply with the denial the runtime recorded"
    );
}

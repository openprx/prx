//! Supervised `file_edit` in `prx chat --plain` on a real PTY over a loopback
//! provider (real HTTP/SSE): the terminal prompt `[y/N/a]` approves once,
//! denies, covers the session's later edits, and Ctrl-C at the prompt cancels
//! only the turn (CF-R3, CR-2).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;
#[path = "support/pty.rs"]
mod pty;

use loopback_openai::{LoopbackOpenAi, Script, Scripted};
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

/// A Supervised `chat --plain` on a PTY with `notes.txt` = [`ORIGINAL`].
fn start(sandbox: &Sandbox, script: Script) -> (LoopbackOpenAi, Pty) {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    loopback_openai::write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir = sandbox.workspace.clone();
        config.autonomy.level = openprx::security::AutonomyLevel::Supervised;
    });
    std::fs::write(sandbox.workspace.join("notes.txt"), ORIGINAL).expect("test: notes.txt");
    let mut pty = Pty::spawn(sandbox.command(&["chat", "--plain"]));
    pty.read_until(0, "prx>");
    (server, pty)
}

fn notes(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(sandbox.workspace.join("notes.txt")).expect("test: read notes.txt")
}

/// One edit turn answered with `answer` at the prompt, then `/exit`.
fn one_edit(answer: &str, reply: &str) -> (Sandbox, String) {
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
    pty.read_until(mark, reply);
    pty.send("/exit\r");
    assert_eq!(pty.wait_exit(), Some(0), "chat exits cleanly");
    assert_eq!(server.requests().len(), 2, "the tool call and the reply");
    (sandbox, prompt)
}

#[test]
fn y_edits_once() {
    let (sandbox, prompt) = one_edit("y", "EDIT-DONE");
    assert!(
        prompt.contains("Approve file_edit"),
        "the prompt names the tool: {prompt}"
    );
    assert_eq!(
        notes(&sandbox),
        "alpha\nBETA\ngamma\ndelta\n",
        "only the target line changes"
    );
}

#[test]
fn n_leaves_file() {
    let (sandbox, _) = one_edit("n", "EDIT-DENIED");
    assert_eq!(notes(&sandbox), ORIGINAL, "a denied edit leaves the file unchanged");
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

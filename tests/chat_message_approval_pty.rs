//! Supervised `file_edit` in `prx chat --message` over a loopback provider
//! (real HTTP/SSE): on a real PTY the terminal prompt `[y/N/a]` approves or
//! denies and the one-shot exits 0, `a` covers the rest of the turn, Ctrl-C
//! exits 1; without a controlling terminal only `OPENPRX_APPROVAL_OVERRIDE`
//! allows (CF-R3, CR-2).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;
#[path = "support/pty.rs"]
mod pty;

use loopback_openai::{LoopbackOpenAi, Script, Scripted, approval_rows};
use plain_chat::Sandbox;
use pty::Pty;

const MODEL: &str = "prx-loop-model";
const ORIGINAL: &str = "alpha\nbeta\ngamma\ndelta\n";
const PROMPT: &str = "[y/N/a]";
const REQUEST: &str = "change beta to BETA in notes.txt";

fn edit_call(id: &str, old: &str, new: &str) -> Scripted {
    Scripted::tool(
        id,
        "file_edit",
        &serde_json::json!({"path": "notes.txt", "old_string": old, "new_string": new}),
    )
}

/// A Supervised config with `notes.txt` = [`ORIGINAL`] and the loopback provider.
fn prepare(sandbox: &Sandbox, script: Script) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    loopback_openai::write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Supervised;
    });
    std::fs::write(sandbox.workspace.join("notes.txt"), ORIGINAL).expect("test: notes.txt");
    server
}

fn notes(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(sandbox.workspace.join("notes.txt")).expect("test: read notes.txt")
}

/// One edit turn answered with `answer` at the prompt; returns the sandbox,
/// the capture and the exit code.
fn one_edit(answer: &str, reply: &str) -> (Sandbox, String, Option<i32>) {
    let sandbox = Sandbox::new();
    let reply_text = reply.to_owned();
    let server = prepare(
        &sandbox,
        Box::new(move |index, _| match index {
            0 => edit_call("call-edit", "beta", "BETA"),
            _ => Scripted::text(&reply_text),
        }),
    );
    let mut pty = Pty::spawn(sandbox.command(&["chat", "--message", REQUEST]));
    let prompt = pty.read_until(0, PROMPT);
    assert!(
        prompt.contains("Approve file_edit"),
        "the prompt names the tool: {prompt}"
    );
    let mark = pty.mark();
    pty.send(&format!("{answer}\r"));
    pty.read_until(mark, reply);
    let code = pty.wait_exit();
    assert_eq!(server.requests().len(), 2, "the tool call and the reply");
    let captured = pty.captured();
    (sandbox, captured, code)
}

#[test]
fn y_exit_0_edited() {
    let (sandbox, _, code) = one_edit("y", "EDIT-DONE");
    assert_eq!(code, Some(0), "an answered one-shot exits 0");
    assert_eq!(
        notes(&sandbox),
        "alpha\nBETA\ngamma\ndelta\n",
        "only the target line changes"
    );
}

#[test]
fn n_exit_0_unchanged() {
    let (sandbox, captured, code) = one_edit("n", "EDIT-DENIED");
    assert_eq!(code, Some(0), "a denied tool still ends the turn with a reply");
    assert_eq!(notes(&sandbox), ORIGINAL, "a denied edit leaves the file unchanged");
    assert_eq!(
        approval_rows(&sandbox.workspace),
        ["operation_id=call-edit capability=file_edit verdict=denied cause=user_rejected"],
        "the denial is recorded once"
    );
    assert!(
        captured.contains("Denied file_edit"),
        "the operator sees the denial: {captured}"
    );
}

#[test]
fn a_covers_the_rest_of_the_turn() {
    let sandbox = Sandbox::new();
    let server = prepare(
        &sandbox,
        Box::new(|index, _| match index {
            0 => edit_call("call-edit-1", "beta", "BETA"),
            1 => edit_call("call-edit-2", "delta", "DELTA"),
            _ => Scripted::text("BOTH-DONE"),
        }),
    );
    let mut pty = Pty::spawn(sandbox.command(&["chat", "--message", "change beta and delta in notes.txt"]));
    pty.read_until(0, PROMPT);
    let mark = pty.mark();
    pty.send("a\r");
    let rest = pty.read_until(mark, "BOTH-DONE");
    assert_eq!(pty.wait_exit(), Some(0), "the one-shot exits 0");
    assert!(!rest.contains(PROMPT), "the turn's second edit is not prompted: {rest}");
    assert_eq!(server.requests().len(), 3, "two tool calls and the reply");
    assert_eq!(
        notes(&sandbox),
        "alpha\nBETA\ngamma\nDELTA\n",
        "both target lines change"
    );
}

#[test]
fn ctrl_c_exit_1() {
    let sandbox = Sandbox::new();
    let server = prepare(
        &sandbox,
        Box::new(|index, _| match index {
            0 => edit_call("call-edit", "beta", "BETA"),
            _ => Scripted::text("MUST-NOT-ANSWER"),
        }),
    );
    let mut pty = Pty::spawn(sandbox.command(&["chat", "--message", REQUEST]));
    pty.read_until(0, PROMPT);
    pty.send("\u{3}");
    assert_eq!(pty.wait_exit(), Some(1), "Ctrl-C at the prompt fails the one-shot");
    assert!(
        !pty.captured().contains("MUST-NOT-ANSWER"),
        "the cancelled turn prints no reply: {}",
        pty.captured()
    );
    assert_eq!(server.requests().len(), 1, "no request after the cancellation");
    assert_eq!(notes(&sandbox), ORIGINAL, "a cancelled edit leaves the file unchanged");
}

/// `setsid --wait prx …`: the one-shot runs in a new session with no
/// controlling terminal, so `/dev/tty` cannot be opened.
#[cfg(target_os = "linux")]
fn without_terminal(sandbox: &Sandbox, args: &[&str], approval_override: Option<&str>) -> plain_chat::Output {
    let base = sandbox.command(args);
    let mut command = std::process::Command::new("setsid");
    command
        .arg("--wait")
        .arg(base.get_program())
        .args(base.get_args())
        .env_clear();
    for (key, value) in base.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    if let Some(value) = approval_override {
        command.env("OPENPRX_APPROVAL_OVERRIDE", value);
    }
    plain_chat::Running::spawn(command).wait()
}

#[cfg(target_os = "linux")]
#[test]
fn no_tty_denies_unless_override() {
    for (approval_override, expected) in [(None, ORIGINAL), (Some("allow"), "alpha\nBETA\ngamma\ndelta\n")] {
        let sandbox = Sandbox::new();
        let server = prepare(
            &sandbox,
            Box::new(|index, _| match index {
                0 => edit_call("call-edit", "beta", "BETA"),
                _ => Scripted::text("NO-TTY-DONE"),
            }),
        );
        let output = without_terminal(&sandbox, &["chat", "--message", REQUEST], approval_override);
        assert!(
            output.status.success(),
            "override {approval_override:?}: stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert_eq!(output.stdout, "NO-TTY-DONE\n", "stdout is the reply alone");
        assert!(
            !output.stderr.contains(PROMPT),
            "no prompt without a terminal: {}",
            output.stderr
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "the tool call and the reply");
        assert_eq!(notes(&sandbox), expected, "override {approval_override:?}");
        let rows = approval_rows(&sandbox.workspace);
        if approval_override.is_some() {
            assert_eq!(
                rows,
                ["operation_id=call-edit capability=file_edit verdict=approved"],
                "an allowing override is recorded as an approval"
            );
        } else {
            assert_eq!(
                rows,
                ["operation_id=call-edit capability=file_edit verdict=denied cause=no_terminal"],
                "no terminal is recorded as its own cause, not a user rejection"
            );
            assert_eq!(
                loopback_openai::tool_result_text(&requests[1], "call-edit").as_deref(),
                Some(
                    "Denied: No terminal was available to ask for approval. \
                     The file_edit call did not run and produced no output."
                ),
                "the model reads why the edit did not run"
            );
            assert!(
                output.stderr.contains("Denied file_edit"),
                "the operator sees the denial on stderr: {}",
                output.stderr
            );
        }
    }
}

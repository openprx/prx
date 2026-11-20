//! Supervised `file_edit` and `shell` in the fullscreen chat over a loopback
//! provider (real HTTP/SSE): `y` runs the call once, `n` leaves it unrun with
//! a `Denied` notice, `a` covers the second edit of the session without
//! another approval card (CF-R3). Every resolution leaves one durable
//! `tool.approval.resolved` row, and a denial tells the model that the call
//! did not run.

use super::{
    HarnessGuard, PHASE_CAP, SessionGuard, build_provider_chat_command_in, new_harness_guard,
    prepare_owned_chat_fixture, read_until_all_with_dsr, read_until_with_dsr, spawn_chat_command,
    wait_for_graceful_exit,
};
use crate::loopback_openai::{
    LoopbackOpenAi, Script, Scripted, approval_rows, event_position, reserved_rows, tool_result_text,
};

const MODEL: &str = "prx-loop-model";
const ORIGINAL: &str = "alpha\nbeta\ngamma\ndelta\n";
/// A word of the approval card's title (the renderer may move the cursor
/// over the blank between title words, so one word is matched).
const CARD: &str = "Approval";

fn workspace(guard: &HarnessGuard) -> std::path::PathBuf {
    guard.config_dir.join("workspace")
}

fn edit_call(id: &str, old: &str, new: &str) -> Scripted {
    Scripted::tool(
        id,
        "file_edit",
        &serde_json::json!({"path": "notes.txt", "old_string": old, "new_string": new}),
    )
}

/// A Supervised fullscreen chat against the loopback provider, with
/// `notes.txt` holding [`ORIGINAL`].
fn start(guard: &HarnessGuard, script: Script) -> (LoopbackOpenAi, SessionGuard) {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    let provider = server.provider();
    crate::loopback_openai::write_config(&guard.config_dir, &provider, MODEL, |config| {
        config.workspace_dir = workspace(guard);
        config.autonomy.level = openprx::security::AutonomyLevel::Supervised;
    });
    std::fs::create_dir_all(workspace(guard)).expect("test: workspace");
    std::fs::write(workspace(guard).join("notes.txt"), ORIGINAL).expect("test: notes.txt");
    prepare_owned_chat_fixture(guard, &provider, MODEL);
    let command = build_provider_chat_command_in(guard, &provider, MODEL, &[], &[("PRX_TUI", "1")]);
    (server, SessionGuard::new(spawn_chat_command(command)))
}

fn notes(guard: &HarnessGuard) -> String {
    std::fs::read_to_string(workspace(guard).join("notes.txt")).expect("test: read notes.txt")
}

/// A word of the denial notice line (`Denied <tool>: <reason> — ...`).
const NOTICE: &str = "Denied";

/// One tool turn: the model calls `call`, the card is answered with `key`, and
/// the screen must show every one of `after_key` (the reply last) before
/// `/exit`. Returns the guard and the recorded provider requests.
fn one_call(prompt: &str, call: Scripted, key: &str, after_key: &[&str]) -> (HarnessGuard, Vec<serde_json::Value>) {
    let guard = new_harness_guard().expect("isolated chat paths");
    let mut call = Some(call);
    let reply_text = after_key.last().copied().unwrap_or_default().to_owned();
    let (server, mut session) = start(
        &guard,
        Box::new(move |index, _| match (index, call.take()) {
            (0, Some(call)) => call,
            _ => Scripted::text(&reply_text),
        }),
    );
    let tty = session.session();
    read_until_with_dsr(tty, MODEL, PHASE_CAP);
    tty.send(&format!("{prompt}\r")).expect("send turn");
    read_until_with_dsr(tty, CARD, PHASE_CAP);
    tty.send(key).expect("answer the approval card");
    read_until_all_with_dsr(tty, after_key, PHASE_CAP);
    tty.send("/exit\r").expect("send /exit");
    wait_for_graceful_exit(tty);
    drop(session);
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the tool call and the reply");
    (guard, requests)
}

/// One edit turn answered with `key` at the approval card, then `/exit`.
fn one_edit(key: &str, after_key: &[&str]) -> (HarnessGuard, Vec<serde_json::Value>) {
    one_call(
        "change beta to BETA in notes.txt",
        edit_call("call-edit", "beta", "BETA"),
        key,
        after_key,
    )
}

fn shell_call() -> Scripted {
    Scripted::tool("call-shell", "shell", &serde_json::json!({"command": "echo tui-shell"}))
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_file_edit_approve_edits_once() {
    let (guard, _) = one_edit("y", &["EDIT-DONE"]);
    assert_eq!(
        notes(&guard),
        "alpha\nBETA\ngamma\ndelta\n",
        "only the target line changes"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_file_edit_deny_leaves_file() {
    let (guard, requests) = one_edit("n", &[NOTICE, "EDIT-DENIED"]);
    assert_eq!(notes(&guard), ORIGINAL, "a denied edit leaves the file unchanged");
    assert_eq!(
        approval_rows(&workspace(&guard)),
        ["operation_id=call-edit capability=file_edit verdict=denied cause=user_rejected"],
        "the denial is recorded once"
    );
    assert_eq!(
        tool_result_text(&requests[1], "call-edit").as_deref(),
        Some("Denied: User rejected tool approval. The file_edit call did not run and produced no output."),
        "the model reads that the edit did not run"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_shell_deny_shows_card_notice_and_records_denial() {
    let (guard, requests) = one_call("run echo tui-shell", shell_call(), "n", &[NOTICE, "SHELL-DENIED"]);
    assert_eq!(
        approval_rows(&workspace(&guard)),
        ["operation_id=call-shell capability=shell verdict=denied cause=user_rejected"],
        "the denial is recorded once"
    );
    assert!(
        !reserved_rows(&workspace(&guard))
            .iter()
            .any(|row| row.starts_with("operation_id=call-shell ")),
        "a denied call is never reserved"
    );
    assert_eq!(
        tool_result_text(&requests[1], "call-shell").as_deref(),
        Some("Denied: User rejected tool approval. The shell call did not run and produced no output."),
        "the model reads that the shell call did not run"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_shell_approve_runs_once_with_approval_row() {
    let (guard, requests) = one_call("run echo tui-shell", shell_call(), "y", &["SHELL-DONE"]);
    let workspace = workspace(&guard);
    assert_eq!(
        approval_rows(&workspace),
        ["operation_id=call-shell capability=shell verdict=approved"],
        "the approval is recorded once"
    );
    let approved =
        event_position(&workspace, "tool.approval.resolved", "operation_id=call-shell ").expect("test: approval row");
    let reserved = event_position(&workspace, "tool.execution.reserved", "operation_id=call-shell ")
        .expect("test: reservation row");
    assert!(approved < reserved, "the approval is recorded before the reservation");
    assert!(
        event_position(
            &workspace,
            "tool.execution.finalized",
            "operation_id=call-shell status=Succeeded"
        )
        .is_some(),
        "the approved shell call ran once and succeeded"
    );
    assert!(
        tool_result_text(&requests[1], "call-shell").is_some_and(|text| text.contains("tui-shell")),
        "the model reads the real shell output"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_file_edit_always_then_second_edit() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(
        &guard,
        Box::new(|index, _| match index {
            0 => edit_call("call-edit-1", "beta", "BETA"),
            1 => Scripted::text("FIRST-DONE"),
            2 => edit_call("call-edit-2", "delta", "DELTA"),
            _ => Scripted::text("SECOND-DONE"),
        }),
    );
    let tty = session.session();
    read_until_with_dsr(tty, MODEL, PHASE_CAP);
    tty.send("change beta to BETA in notes.txt\r").expect("send first turn");
    read_until_with_dsr(tty, CARD, PHASE_CAP);
    tty.send("a").expect("approve always");
    read_until_with_dsr(tty, "FIRST-DONE", PHASE_CAP);
    tty.send("change delta to DELTA in notes.txt\r")
        .expect("send second turn");
    let second = read_until_with_dsr(tty, "SECOND-DONE", PHASE_CAP);
    tty.send("/exit\r").expect("send /exit");
    wait_for_graceful_exit(tty);
    drop(session);

    assert!(
        !second.contains(CARD),
        "the second edit of the session needs no approval card: {second}"
    );
    assert_eq!(server.requests().len(), 4, "two tool calls and two replies");
    assert_eq!(notes(&guard), "alpha\nBETA\ngamma\nDELTA\n", "both target lines change");
}

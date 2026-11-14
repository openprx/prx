//! Supervised `file_edit` in the fullscreen chat over a loopback provider
//! (real HTTP/SSE): `y` edits once, `n` leaves the file, `a` covers the
//! second edit of the session without another approval card (CF-R3).

use super::{
    EXIT_TIMEOUT, HarnessGuard, STARTUP_TIMEOUT, SessionGuard, TURN_TIMEOUT, build_provider_chat_command_in,
    new_harness_guard, prepare_owned_chat_fixture, read_until_with_dsr, spawn_chat_command, wait_for_exit,
};
use crate::loopback_openai::{LoopbackOpenAi, Script, Scripted};

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

/// One edit turn answered with `key` at the approval card, then `/exit`.
fn one_edit(key: &str, reply: &str) -> HarnessGuard {
    let guard = new_harness_guard().expect("isolated chat paths");
    let reply_text = reply.to_owned();
    let (server, mut session) = start(
        &guard,
        Box::new(move |index, _| match index {
            0 => edit_call("call-edit", "beta", "BETA"),
            _ => Scripted::text(&reply_text),
        }),
    );
    let tty = session.session();
    read_until_with_dsr(tty, MODEL, STARTUP_TIMEOUT);
    tty.send("change beta to BETA in notes.txt\r").expect("send turn");
    read_until_with_dsr(tty, CARD, TURN_TIMEOUT);
    tty.send(key).expect("answer the approval card");
    read_until_with_dsr(tty, reply, TURN_TIMEOUT);
    tty.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(tty, EXIT_TIMEOUT), "chat should exit cleanly");
    drop(session);
    assert_eq!(server.requests().len(), 2, "the tool call and the reply");
    guard
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_file_edit_approve_edits_once() {
    let guard = one_edit("y", "EDIT-DONE");
    assert_eq!(
        notes(&guard),
        "alpha\nBETA\ngamma\ndelta\n",
        "only the target line changes"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn supervised_file_edit_deny_leaves_file() {
    let guard = one_edit("n", "EDIT-DENIED");
    assert_eq!(notes(&guard), ORIGINAL, "a denied edit leaves the file unchanged");
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
    read_until_with_dsr(tty, MODEL, STARTUP_TIMEOUT);
    tty.send("change beta to BETA in notes.txt\r").expect("send first turn");
    read_until_with_dsr(tty, CARD, TURN_TIMEOUT);
    tty.send("a").expect("approve always");
    read_until_with_dsr(tty, "FIRST-DONE", TURN_TIMEOUT);
    tty.send("change delta to DELTA in notes.txt\r")
        .expect("send second turn");
    let second = read_until_with_dsr(tty, "SECOND-DONE", TURN_TIMEOUT);
    tty.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(tty, EXIT_TIMEOUT), "chat should exit cleanly");
    drop(session);

    assert!(
        !second.contains(CARD),
        "the second edit of the session needs no approval card: {second}"
    );
    assert_eq!(server.requests().len(), 4, "two tool calls and two replies");
    assert_eq!(notes(&guard), "alpha\nBETA\ngamma\nDELTA\n", "both target lines change");
}

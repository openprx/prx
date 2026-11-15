//! Fullscreen chat compaction over a loopback OpenAI-compatible provider (real
//! HTTP/SSE) with a real SQLite transcript (CF-R5, CF-R12).
//!
//! Tool-heavy turns push the history past a small window and the in-turn
//! `switch` preflight writes a durable handoff; an explicit `/compact` below
//! the threshold does the same. The next request carries the handoff and the
//! lookup tool, and the scripted lookup returns the planted fact from the real
//! transcript. The start banner and `/sessions` name the chat session.

use super::guarded_delivery::screen_text;
use std::time::{Duration, Instant};

use super::{
    EXIT_TIMEOUT, HarnessGuard, STARTUP_TIMEOUT, SessionGuard, TURN_TIMEOUT, build_provider_chat_command_in,
    drain_with_dsr, new_harness_guard, prepare_owned_chat_fixture, read_until_all_with_dsr, read_until_with_dsr,
    spawn_chat_command, wait_for_exit,
};
use crate::compaction_script::{
    PLANT, RECALL, RECALLED, SMALL_WINDOW_TOKENS, assistant_reply_count, carries_handoff_and_lookup, chat_session_id,
    read_done, read_line, switch_event_counts, write_filler_files,
};
use crate::loopback_openai::LoopbackOpenAi;

const MODEL: &str = "prx-loop-model";
/// A window the manual case never crosses.
const LARGE_WINDOW_TOKENS: usize = 200_000;
/// Response reserve in both cases.
const RESERVE_TOKENS: usize = 256;
/// Messages kept verbatim after compaction.
const KEEP_RECENT: usize = 2;

fn workspace(guard: &HarnessGuard) -> std::path::PathBuf {
    guard.config_dir.join("workspace")
}

fn start(guard: &HarnessGuard, window: usize, filler_files: usize) -> (LoopbackOpenAi, SessionGuard) {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), crate::compaction_script::script());
    let provider = server.provider();
    crate::loopback_openai::write_config(&guard.config_dir, &provider, MODEL, |config| {
        config.workspace_dir = workspace(guard);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = window;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = KEEP_RECENT;
    });
    prepare_owned_chat_fixture(guard, &provider, MODEL);
    std::fs::create_dir_all(workspace(guard)).expect("test: workspace");
    write_filler_files(&workspace(guard), filler_files);
    let command = build_provider_chat_command_in(guard, &provider, MODEL, &[], &[("PRX_TUI", "1")]);
    (server, SessionGuard::new(spawn_chat_command(command)))
}

/// Drain interval while a turn runs.
const DRAIN_SLICE: Duration = Duration::from_millis(100);

/// Send `line` and wait until the real store holds `replies` assistant
/// replies, draining the PTY meanwhile. The fullscreen renderer redraws only
/// changed cells, so a reply sharing a prefix with an earlier one is not a
/// reliable PTY needle; the durable reply is.
fn turn(guard: &HarnessGuard, session: &mut SessionGuard, line: &str, replies: usize) {
    let session = session.session();
    session.send(format!("{line}\r")).expect("send turn");
    let started = Instant::now();
    while assistant_reply_count(&workspace(guard)) < replies {
        assert!(
            started.elapsed() < TURN_TIMEOUT,
            "reply {replies} to `{line}` timed out"
        );
        drain_with_dsr(session, DRAIN_SLICE);
    }
}

/// The newest persisted assistant reply.
fn last_reply(guard: &HarnessGuard) -> String {
    crate::loopback_openai::assistant_messages(&workspace(guard))
        .pop()
        .unwrap_or_default()
}

fn exit(session: &mut SessionGuard) {
    let session = session.session();
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat should exit cleanly");
}

fn assert_handoff_and_recall(guard: &HarnessGuard, server: &LoopbackOpenAi) {
    let (handoffs, unavailable) = switch_event_counts(&workspace(guard));
    assert!(handoffs >= 1, "a context-switch handoff event is stored");
    assert_eq!(unavailable, 0, "no handoff_unavailable degradation is stored");
    assert!(
        server.requests().iter().any(carries_handoff_and_lookup),
        "a later request carries the handoff and transcript_history_lookup"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn switch_compaction_and_recall() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(&guard, SMALL_WINDOW_TOKENS, 6);
    let banner = read_until_all_with_dsr(session.session(), &[MODEL, "session"], STARTUP_TIMEOUT);
    turn(&guard, &mut session, PLANT, 1);
    for index in 0..6 {
        turn(&guard, &mut session, &read_line(index), index + 2);
        assert_eq!(last_reply(&guard), read_done(index), "tool turn {index} completed");
    }
    turn(&guard, &mut session, RECALL, 8);
    assert_eq!(last_reply(&guard), RECALLED, "the lookup returned the planted fact");
    let id = chat_session_id(&workspace(&guard));
    session.session().send("/sessions\r").expect("send /sessions");
    let sessions = read_until_with_dsr(session.session(), &id, TURN_TIMEOUT);
    exit(&mut session);
    drop(session);

    assert_handoff_and_recall(&guard, &server);
    assert!(
        screen_text(&banner).contains(&format!("session {id}")),
        "the start banner shows the session id `{id}`"
    );
    assert!(
        screen_text(&sessions).contains("current chat session"),
        "/sessions names the current chat session first"
    );
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn manual_compact_below_threshold() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(&guard, LARGE_WINDOW_TOKENS, 2);
    read_until_with_dsr(session.session(), MODEL, STARTUP_TIMEOUT);
    turn(&guard, &mut session, PLANT, 1);
    turn(&guard, &mut session, &read_line(0), 2);
    turn(&guard, &mut session, &read_line(1), 3);
    session.session().send("/compact\r").expect("send /compact");
    let compacted = screen_text(&read_until_with_dsr(session.session(), "Compacted", TURN_TIMEOUT));
    assert!(
        !compacted.contains("Nothing to compact") && !compacted.contains("lossy"),
        "manual compaction compacted: {compacted}"
    );
    turn(&guard, &mut session, RECALL, 4);
    assert_eq!(last_reply(&guard), RECALLED, "the lookup returned the planted fact");
    exit(&mut session);
    drop(session);

    assert_handoff_and_recall(&guard, &server);
}

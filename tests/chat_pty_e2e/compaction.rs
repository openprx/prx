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
    HarnessGuard, MULTI_STAGE_BUDGET, PHASE_CAP, SessionGuard, TestDeadline, build_provider_chat_command_in,
    drain_with_dsr, new_harness_guard, prepare_owned_chat_fixture, read_until_all_with_dsr, read_until_with_dsr,
    spawn_chat_command, wait_for_graceful_exit,
};
use crate::compaction_script::{
    PLANT, RECALL, RECALL_WINDOW_TOKENS, RECALLED, assert_handoff_chain, assistant_reply_count,
    carries_handoff_and_lookup, chat_session_id, lookup_results, over_hard_limit_after_handoff, planted_row_content,
    read_done, read_line, session_handoffs, stored_handoffs, switch_event_counts, write_filler_files,
};
use crate::loopback_openai::LoopbackOpenAi;

const MODEL: &str = "prx-loop-model";
/// A window the manual case never crosses.
const LARGE_WINDOW_TOKENS: usize = 200_000;
/// Response reserve in both cases.
const RESERVE_TOKENS: usize = 256;
/// Messages kept verbatim after compaction.
const KEEP_RECENT: usize = 2;
/// Scripted usage of every completion (`prompt`, `completion`).
const USAGE: (u64, u64) = (11, 7);
/// Filler files of the switch case; the automatic switch comes well before
/// the last one.
const SWITCH_FILLERS: usize = 10;

fn workspace(guard: &HarnessGuard) -> std::path::PathBuf {
    guard.config_dir.join("workspace")
}

fn start(guard: &HarnessGuard, window: usize, filler_files: usize) -> (LoopbackOpenAi, SessionGuard) {
    let server = LoopbackOpenAi::start(MODEL, Some(USAGE), crate::compaction_script::script());
    let provider = server.provider();
    crate::loopback_openai::write_config(&guard.config_dir, &provider, MODEL, |config| {
        config.workspace_dir = workspace(guard);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = window;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = KEEP_RECENT;
        // Context comes from the session and its handoff alone: a saved
        // long-term memory of the planted line is recalled by its own,
        // separate path and would put the fact back outside the handoff.
        config.memory.auto_save = false;
    });
    prepare_owned_chat_fixture(guard, &provider, MODEL);
    std::fs::create_dir_all(workspace(guard)).expect("test: workspace");
    write_filler_files(&workspace(guard), filler_files);
    // One aggregate budget for every session of this scenario (see MULTI_STAGE_BUDGET).
    let deadline = TestDeadline::new(MULTI_STAGE_BUDGET);
    (server, respawn(guard, &provider, &[], deadline))
}

/// Start the fullscreen chat against an already configured fixture, sharing `deadline`.
fn respawn(guard: &HarnessGuard, provider: &str, args: &[&str], deadline: TestDeadline) -> SessionGuard {
    let command = build_provider_chat_command_in(guard, provider, MODEL, args, &[("PRX_TUI", "1")]);
    SessionGuard::with_deadline(spawn_chat_command(command), deadline)
}

/// Drain interval while a turn runs.
const DRAIN_SLICE: Duration = Duration::from_millis(100);

/// Send `line` and wait until the real store holds `replies` assistant
/// replies, draining the PTY meanwhile. The fullscreen renderer redraws only
/// changed cells, so a reply sharing a prefix with an earlier one is not a
/// reliable PTY needle; the durable reply is.
fn turn(guard: &HarnessGuard, session: &mut SessionGuard, line: &str, replies: usize) {
    let session = session.session();
    session.enter_phase(&format!("turn: reply {replies}"));
    session.send(format!("{line}\r")).expect("send turn");
    let started = Instant::now();
    let limit = PHASE_CAP.min(session.deadline.remaining());
    while assistant_reply_count(&workspace(guard)) < replies {
        assert!(
            started.elapsed() < limit,
            "reply {replies} to `{line}` timed out in phase `{}`",
            session.phase
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
    wait_for_graceful_exit(session);
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
    let (server, mut session) = start(&guard, RECALL_WINDOW_TOKENS, SWITCH_FILLERS);
    let deadline = session.session().deadline;
    let banner = read_until_all_with_dsr(session.session(), &[MODEL, "session"], PHASE_CAP);
    turn(&guard, &mut session, PLANT, 1);
    turn(&guard, &mut session, &read_line(0), 2);
    assert_eq!(last_reply(&guard), read_done(0), "tool turn 0 completed");
    session.session().send("/compact\r").expect("send /compact");
    let compacted = screen_text(&read_until_with_dsr(session.session(), "Compacted", PHASE_CAP));
    assert!(!compacted.contains("lossy"), "manual compaction compacted: {compacted}");
    let mut replies = 2;
    let mut next_file = 1;
    while stored_handoffs(&workspace(&guard)).len() < 2 && next_file < SWITCH_FILLERS {
        replies += 1;
        turn(&guard, &mut session, &read_line(next_file), replies);
        assert_eq!(
            last_reply(&guard),
            read_done(next_file),
            "tool turn {next_file} completed"
        );
        next_file += 1;
    }
    let id = chat_session_id(&workspace(&guard));
    // Right after the automatic switch: both read the compacted state.
    session.session().send("/compact\r").expect("send /compact");
    session.session().send("/branch 1\r").expect("send /branch");
    let branched = screen_text(&read_until_with_dsr(session.session(), "Created branch", PHASE_CAP));
    replies += 1;
    turn(&guard, &mut session, RECALL, replies);
    assert_eq!(last_reply(&guard), RECALLED, "the branch recalls the planted fact");
    session.session().send("/sessions\r").expect("send /sessions");
    let sessions = read_until_with_dsr(session.session(), "current chat session", PHASE_CAP);
    exit(&mut session);
    drop(session);

    let mut resumed = respawn(&guard, &server.provider(), &["--session", &id], deadline);
    read_until_with_dsr(resumed.session(), MODEL, PHASE_CAP);
    replies += 1;
    turn(&guard, &mut resumed, RECALL, replies);
    assert_eq!(
        last_reply(&guard),
        RECALLED,
        "the restarted session recalls the planted fact"
    );
    exit(&mut resumed);
    drop(resumed);

    assert_handoff_and_recall(&guard, &server);
    assert_handoff_chain(&session_handoffs(&workspace(&guard), &id), 2);
    assert!(!branched.contains("Branch aborted"), "/branch succeeded: {branched}");
    let has_branch_handoff = stored_handoffs(&workspace(&guard))
        .iter()
        .any(|handoff| !handoff.session_key.ends_with(&id));
    assert!(
        has_branch_handoff,
        "/branch 1 kept the handoff turn of the compacted state"
    );
    let row = planted_row_content(&workspace(&guard));
    let results = lookup_results(&server.requests());
    assert!(
        !results.is_empty() && results.iter().all(|result| result.contains(&row)),
        "every lookup result carries the stored row `{row}`: {results:?}"
    );
    assert!(
        !over_hard_limit_after_handoff(&workspace(&guard)),
        "the window fits handoff, tail and tool schemas"
    );
    assert!(
        screen_text(&banner).contains(&format!("session {id}")),
        "the start banner shows the session id `{id}`"
    );
    assert!(
        screen_text(&sessions).contains("current chat session"),
        "/sessions names the current chat session first"
    );
}

/// `/cost` right after a reply reports that turn's exact scripted usage
/// (the final usage lands in the turn's acknowledged commit).
#[test]
#[serial_test::serial(prx_chat_pty)]
fn cost_right_after_a_reply_reports_that_turns_exact_usage() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (_server, mut session) = start(&guard, LARGE_WINDOW_TOKENS, 0);
    read_until_with_dsr(session.session(), MODEL, PHASE_CAP);
    turn(&guard, &mut session, PLANT, 1);
    session.session().send("/cost\r").expect("send /cost");
    let report = screen_text(&read_until_all_with_dsr(
        session.session(),
        &["Session cost", "Completion tokens", "Total tokens"],
        PHASE_CAP,
    ));
    exit(&mut session);
    drop(session);

    let report = report.split_whitespace().collect::<Vec<_>>().join(" ");
    for line in [
        "Metered requests: 1".to_owned(),
        format!("Prompt tokens: {} (incl. cache)", USAGE.0),
        format!("Completion tokens: {}", USAGE.1),
        format!("Total tokens: {}", USAGE.0 + USAGE.1),
    ] {
        assert!(report.contains(&line), "/cost reports `{line}`: {report}");
    }
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn manual_compact_below_threshold() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(&guard, LARGE_WINDOW_TOKENS, 2);
    read_until_with_dsr(session.session(), MODEL, PHASE_CAP);
    turn(&guard, &mut session, PLANT, 1);
    turn(&guard, &mut session, &read_line(0), 2);
    turn(&guard, &mut session, &read_line(1), 3);
    session.session().send("/compact\r").expect("send /compact");
    let compacted = screen_text(&read_until_with_dsr(session.session(), "Compacted", PHASE_CAP));
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

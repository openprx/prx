//! `prx chat --plain` compaction over a loopback OpenAI-compatible provider
//! (real HTTP/SSE) with a real SQLite transcript (CF-R5, CF-R12).
//!
//! One held chat process per case, fed one line per completed turn: tool-heavy
//! turns push the history past a small window and the `switch` preflight
//! writes a durable handoff; an explicit `/compact` below the threshold does
//! the same. The next request carries the handoff and the lookup tool, and the
//! scripted lookup returns the planted fact from the real transcript.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use std::time::{Duration, Instant};

use compaction_script::{
    PLANT, READ_DONE, RECALL, RECALLED, SMALL_WINDOW_TOKENS, assistant_reply_count, carries_handoff_and_lookup,
    read_line, switch_event_counts, write_filler_files,
};
use loopback_openai::{LoopbackOpenAi, write_config};
use plain_chat::{Output, Running, Sandbox};

const MODEL: &str = "prx-loop-model";
/// A window the manual case never crosses.
const LARGE_WINDOW_TOKENS: usize = 200_000;
/// Response reserve in both cases.
const RESERVE_TOKENS: usize = 256;
/// Messages kept verbatim after compaction.
const KEEP_RECENT: usize = 2;
/// Longest wait for one turn's assistant event.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
/// Polling interval of the real SQLite store.
const POLL: Duration = Duration::from_millis(50);

fn configure(sandbox: &Sandbox, window: usize) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), compaction_script::script());
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = window;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = KEEP_RECENT;
    });
    server
}

/// Wait until the store holds `count` assistant replies.
fn wait_for_replies(sandbox: &Sandbox, count: usize) {
    let started = Instant::now();
    loop {
        if assistant_reply_count(&sandbox.workspace) >= count {
            return;
        }
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "turn {count} did not complete within TURN_DEADLINE"
        );
        std::thread::sleep(POLL);
    }
}

/// Feed `lines` to one held `chat --plain`; a line that is not a slash command
/// waits for its reply before the next is sent.
fn held_chat(sandbox: &Sandbox, lines: &[String]) -> Output {
    let mut running = Running::spawn(sandbox.command(&["chat", "--plain"]));
    let mut replies = 0;
    for line in lines {
        running.send(line);
        if !line.starts_with('/') {
            replies += 1;
            wait_for_replies(sandbox, replies);
        }
    }
    running.send("/exit");
    running.wait()
}

/// The checks both cases share once the handoff exists.
fn assert_handoff_and_recall(sandbox: &Sandbox, server: &LoopbackOpenAi, output: &Output) {
    let (handoffs, unavailable) = switch_event_counts(&sandbox.workspace);
    let shapes: Vec<(usize, usize)> = server
        .requests()
        .iter()
        .map(|request| {
            (
                request["messages"].as_array().map_or(0, Vec::len),
                request.to_string().len(),
            )
        })
        .collect();
    assert!(
        handoffs >= 1,
        "a context-switch handoff event is stored; requests (messages, bytes): {shapes:?}\nstdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
    assert_eq!(unavailable, 0, "no handoff_unavailable degradation is stored");
    assert!(
        server.requests().iter().any(carries_handoff_and_lookup),
        "a later request carries the handoff and transcript_history_lookup"
    );
    assert!(
        output.stdout.contains(RECALLED),
        "the lookup returned the planted fact from the real transcript; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
}

#[test]
fn switch_compaction_and_recall() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, 6);
    let server = configure(&sandbox, SMALL_WINDOW_TOKENS);
    let mut lines = vec![PLANT.to_owned()];
    lines.extend((0..6).map(read_line));
    lines.push(RECALL.to_owned());
    let output = held_chat(&sandbox, &lines);
    assert!(output.status.success(), "stderr: {}", output.stderr);

    assert!(
        output.stdout.contains(READ_DONE),
        "tool turns completed: {}",
        output.stdout
    );
    assert_handoff_and_recall(&sandbox, &server, &output);
    assert_eq!(
        output.stderr.matches("current chat session: ").count(),
        1,
        "the session id is printed once at start on stderr: {}",
        output.stderr
    );
}

#[test]
fn manual_compact_below_threshold() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, 2);
    let server = configure(&sandbox, LARGE_WINDOW_TOKENS);
    let lines = vec![
        PLANT.to_owned(),
        read_line(0),
        read_line(1),
        "/compact".to_owned(),
        RECALL.to_owned(),
    ];
    let output = held_chat(&sandbox, &lines);
    assert!(output.status.success(), "stderr: {}", output.stderr);

    assert!(
        output.stdout.contains("Compacted context"),
        "the manual compaction printed its compacted feedback: {}",
        output.stdout
    );
    assert!(!output.stdout.contains("Nothing to compact"), "{}", output.stdout);
    assert!(!output.stdout.contains("Context trimmed (lossy)"), "{}", output.stdout);
    assert_handoff_and_recall(&sandbox, &server, &output);
}

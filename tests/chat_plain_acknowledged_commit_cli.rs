//! `prx chat --plain` turns commit through the acknowledged session (R3/R4)
//! over a loopback OpenAI-compatible provider (real HTTP/SSE) and a real
//! SQLite store. Built without `terminal-tui` this is the persistence-only
//! executor; with it, the reducer driver. One test per case:
//!
//! - a normal reply (and, without `terminal-tui`, an empty one) ends in a
//!   saved session row holding the turn;
//! - EOF on stdin after a turn exits the process cleanly with code 0;
//! - `/cost` right after a reply reports that turn's exact scripted usage;
//! - a preflight/switch compaction is acknowledged and every later request
//!   carries its current request exactly once;
//! - with `RUST_LOG=trace` no session content reaches stderr or `chat.log`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use std::time::{Duration, Instant};

use compaction_script::{PLANT, assistant_reply_count, read_line, write_filler_files};
use loopback_openai::{LoopbackOpenAi, Script, Scripted, write_config};
use plain_chat::{Output, Running, Sandbox};

const MODEL: &str = "prx-ack-model";
/// Scripted usage of every completion (`prompt`, `completion`).
const USAGE: (u64, u64) = (11, 7);
/// Longest wait for one turn's durable reply or session row.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
/// Polling interval of the real SQLite store.
const POLL: Duration = Duration::from_millis(50);
const SENTINEL: &str = "SENTINEL-7f3a";
/// Largest `chat.log` the trace case reads; a bigger one fails the test
/// rather than being loaded whole.
const CHAT_LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;

fn serve(sandbox: &Sandbox, script: Script) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some(USAGE), script);
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
    });
    server
}

/// Contents of the saved chat session rows.
fn saved_session_rows(sandbox: &Sandbox) -> Vec<String> {
    let Ok(database) = rusqlite::Connection::open_with_flags(
        sandbox.workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return Vec::new();
    };
    let Ok(mut statement) = database.prepare("SELECT content FROM memories WHERE key LIKE 'chat_session:%'") else {
        return Vec::new();
    };
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// The turns `(role, content)` of the single saved session row.
fn saved_turns(sandbox: &Sandbox) -> Vec<(String, String)> {
    let rows = saved_session_rows(sandbox);
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let session: serde_json::Value = serde_json::from_str(row).expect("test: session row JSON");
    session["turns"]
        .as_array()
        .map(|turns| {
            turns
                .iter()
                .map(|turn| {
                    (
                        turn["role"].as_str().unwrap_or_default().to_owned(),
                        turn["content"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until the saved session row holds `turns` turns.
fn wait_for_saved_turns(sandbox: &Sandbox, turns: usize) {
    let started = Instant::now();
    while saved_turns(sandbox).len() < turns {
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "the saved session row did not reach {turns} turns within TURN_DEADLINE"
        );
        std::thread::sleep(POLL);
    }
}

fn spawn_plain(sandbox: &Sandbox) -> Running {
    Running::spawn(sandbox.command(&["chat", "--plain"]))
}

fn finish(mut running: Running) -> Output {
    running.send("/exit");
    running.wait()
}

fn assert_clean(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr);
    assert!(!output.stderr.contains("panicked"), "stderr: {}", output.stderr);
    assert!(
        !output.stdout.contains("history commit") && !output.stderr.contains("history commit"),
        "no unconfirmed history commit; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
}

#[test]
fn a_reply_is_committed_into_the_saved_session_row() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("PRX-ACK-REPLY")));
    let mut chat = spawn_plain(&sandbox);
    chat.send("first question");
    wait_for_saved_turns(&sandbox, 2);
    let output = finish(chat);

    assert_clean(&output);
    assert!(output.stdout.contains("PRX-ACK-REPLY"), "stdout: {}", output.stdout);
    assert_eq!(
        saved_turns(&sandbox),
        vec![
            ("user".to_owned(), "first question".to_owned()),
            ("assistant".to_owned(), "PRX-ACK-REPLY".to_owned()),
        ],
        "the saved row holds the committed turn"
    );
}

/// Two isolated tool-call JSON lines: not one JSON value, so the agent loop
/// reads them as reply text rather than a call, and the channel sanitizer then
/// strips both lines, leaving an empty reply.
#[cfg(not(feature = "terminal-tui"))]
const EMPTY_AFTER_SANITIZE: &str = "{\"name\":\"shell\",\"arguments\":{\"command\":\"true\"}}\n{\"name\":\"shell\",\"arguments\":{\"command\":\"true\"}}";

/// Non-TUI only: a reply that is empty after sanitization completes as a turn
/// (`run.rs` empty completion). A reply that is empty on the wire never gets
/// there: the agent loop retries it once and then fails the turn (unchanged
/// behaviour, in both builds).
#[cfg(not(feature = "terminal-tui"))]
#[test]
fn an_empty_reply_is_committed_into_the_saved_session_row() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text(EMPTY_AFTER_SANITIZE)));
    let mut chat = spawn_plain(&sandbox);
    chat.send("say nothing");
    wait_for_saved_turns(&sandbox, 1);
    let output = finish(chat);

    assert_clean(&output);
    let turns = saved_turns(&sandbox);
    assert_eq!(
        turns.first(),
        Some(&("user".to_owned(), "say nothing".to_owned())),
        "the saved row holds the committed turn: {turns:?}"
    );
}

/// The last user message of a provider request.
#[cfg(not(feature = "terminal-tui"))]
fn last_user_text(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
        .map(text_of)
        .unwrap_or_default()
}

/// Non-TUI only (the empty completion is a non-TUI path): an empty
/// successful reply to `Q` leaves `Q` as the trailing answerless user turn. A
/// second, identical `Q` that the provider refuses is settled by its admission
/// identity, so the committed first `Q` stays in the acknowledged session (the
/// next request carries it exactly once) and in the SQLite row.
#[cfg(not(feature = "terminal-tui"))]
#[test]
fn a_failed_repeat_of_a_request_keeps_the_committed_request_with_the_same_text() {
    const REPEATED: &str = "REPEATED-QUESTION-71";
    const AFTER: &str = "AFTER-THE-FAILURE-71";
    let sandbox = Sandbox::new();
    let mut repeated_calls = 0usize;
    let server = serve(
        &sandbox,
        Box::new(move |_, request| {
            let last = last_user_text(request);
            if last.ends_with(AFTER) {
                return Scripted::text("AFTER-OK");
            }
            if !last.ends_with(REPEATED) {
                return Scripted::text("ok");
            }
            repeated_calls += 1;
            if repeated_calls == 1 {
                Scripted::text(EMPTY_AFTER_SANITIZE)
            } else {
                Scripted::Error {
                    status: 400,
                    message: "scripted refusal of this request".to_owned(),
                }
            }
        }),
    );
    let mut chat = spawn_plain(&sandbox);
    chat.send(REPEATED);
    wait_for_saved_turns(&sandbox, 1);
    chat.send(REPEATED);
    chat.send(AFTER);
    wait_for_saved_turns(&sandbox, 3);
    let output = finish(chat);

    // A failed turn makes plain chat exit non-zero ("one or more chat turns
    // failed"); the history commits themselves are all acknowledged.
    assert!(!output.stderr.contains("panicked"), "stderr: {}", output.stderr);
    assert!(
        !output.stdout.contains("history commit") && !output.stderr.contains("history commit"),
        "no unconfirmed history commit; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
    assert_eq!(
        saved_turns(&sandbox),
        vec![
            ("user".to_owned(), REPEATED.to_owned()),
            ("user".to_owned(), AFTER.to_owned()),
            ("assistant".to_owned(), "AFTER-OK".to_owned()),
        ],
        "the committed request stays in the SQLite row; only the failed repeat is gone"
    );
    let requests = server.requests();
    let after_request = requests
        .iter()
        .rev()
        .find(|request| last_user_text(request).ends_with(AFTER))
        .expect("test: the request after the failure was sent");
    let earlier_users: Vec<String> = after_request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .map(text_of)
                .filter(|text| !text.ends_with(AFTER))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        earlier_users,
        vec![REPEATED.to_owned()],
        "the next request carries the committed request exactly once and not the failed repeat"
    );
}

/// Controller ruling (G6, input-channel close): EOF on stdin after a turn
/// ends the chat with exit code 0.
#[test]
fn eof_after_a_turn_exits_cleanly() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("PRX-EOF-REPLY")));
    let mut chat = spawn_plain(&sandbox);
    chat.send("one turn before EOF");
    wait_for_saved_turns(&sandbox, 2);
    let output = chat.wait();

    assert_clean(&output);
    assert!(output.stdout.contains("PRX-EOF-REPLY"), "stdout: {}", output.stdout);
}

#[test]
fn cost_right_after_a_reply_reports_that_turns_exact_usage() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("PRX-COST-REPLY")));
    let mut chat = spawn_plain(&sandbox);
    chat.send("a metered turn");
    wait_for_saved_turns(&sandbox, 2);
    chat.send("/cost");
    let output = finish(chat);

    assert_clean(&output);
    let report = output
        .stdout
        .split("Session cost:")
        .nth(1)
        .unwrap_or_else(|| panic!("/cost printed its report: {}", output.stdout));
    for line in [
        "Metered requests:  1".to_owned(),
        format!("Prompt tokens:     {} (incl. cache)", USAGE.0),
        format!("Completion tokens: {}", USAGE.1),
        format!("Total tokens:      {}", USAGE.0 + USAGE.1),
    ] {
        assert!(report.contains(&line), "/cost reports `{line}`: {report}");
    }
}

/// The text of `message`.
fn text_of(message: &serde_json::Value) -> String {
    match &message["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect(),
        _ => String::new(),
    }
}

#[test]
fn a_compaction_before_the_call_is_acknowledged_and_the_request_occurs_once() {
    const FILLERS: usize = 8;
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, FILLERS);
    let server = LoopbackOpenAi::start(MODEL, Some(USAGE), compaction_script::script());
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = compaction_script::SMALL_WINDOW_TOKENS;
        config.agent.compaction.reserve_tokens = 256;
        config.agent.compaction.keep_recent_messages = 2;
        config.memory.auto_save = false;
    });
    let mut chat = spawn_plain(&sandbox);
    let mut lines = vec![PLANT.to_owned()];
    lines.extend((0..FILLERS).map(read_line));
    for (index, line) in lines.iter().enumerate() {
        chat.send(line);
        let started = Instant::now();
        while assistant_reply_count(&sandbox.workspace) < index + 1 {
            assert!(started.elapsed() < TURN_DEADLINE, "turn {index} timed out");
            std::thread::sleep(POLL);
        }
        if !compaction_script::stored_handoffs(&sandbox.workspace).is_empty() {
            break;
        }
    }
    let output = finish(chat);

    assert_clean(&output);
    assert!(
        !compaction_script::stored_handoffs(&sandbox.workspace).is_empty(),
        "the window forced a compaction before a call"
    );
    #[cfg(not(feature = "terminal-tui"))]
    assert!(
        output.stdout.contains("Compacted context"),
        "the preflight reports an acknowledged compaction: {}",
        output.stdout
    );
    assert!(!output.stdout.contains("Compaction refused"), "{}", output.stdout);
    let requests = server.requests();
    let carried: Vec<&serde_json::Value> = requests
        .iter()
        .filter(|request| compaction_script::handoff_note(request).is_some())
        .collect();
    assert!(
        !carried.is_empty(),
        "a request after the compaction carries the handoff"
    );
    for request in carried {
        let messages = request["messages"].as_array().expect("test: messages");
        let current = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .map(text_of)
            .expect("test: a current request");
        let line = lines
            .iter()
            .find(|line| current.contains(line.as_str()))
            .unwrap_or_else(|| panic!("the current request names a sent line: {current}"));
        let occurrences = messages
            .iter()
            .filter(|message| message["role"] == "user" && text_of(message).contains(line.as_str()))
            .count();
        assert_eq!(occurrences, 1, "`{line}` occurs exactly once in the request");
    }
}

/// `chat.log` read up to [`CHAT_LOG_MAX_BYTES`] plus one byte; more than the
/// limit fails the test. A missing log reads as empty.
fn read_bounded_chat_log(path: &std::path::Path) -> String {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    file.take(CHAT_LOG_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .expect("test: read chat.log");
    assert!(
        u64::try_from(bytes.len()).is_ok_and(|len| len <= CHAT_LOG_MAX_BYTES),
        "chat.log exceeds CHAT_LOG_MAX_BYTES ({CHAT_LOG_MAX_BYTES} bytes)"
    );
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn trace_logging_carries_no_session_content() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("PRX-TRACE-REPLY")));
    let mut command = sandbox.command(&["chat", "--plain"]);
    command.env("RUST_LOG", "trace");
    let mut chat = Running::spawn(command);
    chat.send(&format!("remember {SENTINEL} for me"));
    wait_for_saved_turns(&sandbox, 2);
    chat.send("/cost");
    let output = finish(chat);

    assert_clean(&output);
    let chat_log = read_bounded_chat_log(&sandbox.config_dir.join("chat.log"));
    let logs = format!("{}{chat_log}", output.stderr);
    assert!(
        logs.contains("TRACE") || logs.contains("DEBUG"),
        "trace logging was captured on stderr or chat.log"
    );
    assert!(
        saved_turns(&sandbox)
            .iter()
            .any(|(_, content)| content.contains(SENTINEL)),
        "the sentinel turn is saved"
    );
    assert!(!output.stderr.contains(SENTINEL), "the sentinel reached stderr");
    assert!(!chat_log.contains(SENTINEL), "the sentinel reached chat.log");
}

/// Make SQLite refuse every write of a session row that records an
/// interrupted background session, i.e. only the exit summary.
fn refuse_exit_summary_writes(sandbox: &Sandbox) {
    let connection =
        rusqlite::Connection::open(sandbox.workspace.join("memory/brain.db")).expect("test: open the real database");
    connection
        .execute_batch(
            "CREATE TRIGGER test_refuse_exit_insert BEFORE INSERT ON memories \
             WHEN NEW.content LIKE '%\"interrupted\"%' \
             BEGIN SELECT RAISE(ABORT, 'test: the database refuses the exit summary'); END;
             CREATE TRIGGER test_refuse_exit_update BEFORE UPDATE ON memories \
             WHEN NEW.content LIKE '%\"interrupted\"%' \
             BEGIN SELECT RAISE(ABORT, 'test: the database refuses the exit summary'); END;",
        )
        .expect("test: install the refusing triggers");
}

#[test]
fn an_exit_summary_the_store_refuses_fails_the_exit() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("noted")));
    let mut running = spawn_plain(&sandbox);
    running.send("hello");
    wait_for_saved_turns(&sandbox, 2);
    refuse_exit_summary_writes(&sandbox);
    // A live background shell is interrupted at exit; its summary is the
    // only write the database refuses.
    running.send("/shell sleep 30");
    let output = finish(running);
    assert!(
        !output.status.success(),
        "a refused exit summary fails the exit; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
    assert!(
        output.stderr.contains("unconfirmed history commit"),
        "the typed stop error is reported; stderr: {}",
        output.stderr
    );
    assert!(!output.stderr.contains("panicked"), "stderr: {}", output.stderr);
    assert!(
        output.stdout.contains("Started background shell"),
        "the background shell was running at exit; stdout: {}",
        output.stdout
    );
}

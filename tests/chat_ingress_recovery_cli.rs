//! Real-process proofs that no earlier ingress row blocks new chat input.
//!
//! Each case runs the built `prx` binary with an isolated config dir, a real
//! SQLite store, real lock files and a loopback OpenAI-compatible provider
//! (real HTTP/SSE bytes). Every child is bounded and killed on drop.
//!
//! - a chat killed mid-turn whose lock directory then moves (as a reboot that
//!   renumbers the volume does) does not block the next turn, which reaches
//!   the provider exactly once; the old row becomes `unconfirmed` with cause
//!   `lock_missing`;
//! - `prx chat --message` succeeds while a live `--plain` REPL holds the scope;
//! - a normal turn leaves no `pending` ingress row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use std::time::{Duration, Instant};

use loopback_openai::{LoopbackOpenAi, Script, Scripted, write_config};
use plain_chat::{Output, Running, Sandbox};

const MODEL: &str = "prx-ingress-model";
/// Longest wait for one provider request or one durable terminal.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
/// Polling interval of the loopback log and the real store.
const POLL: Duration = Duration::from_millis(50);
/// Most ingress rows a case reads; more fails the test.
const MAX_INGRESS_ROWS: usize = 64;
const FIRST: &str = "PRX-INGRESS-FIRST-QUESTION";
const SECOND: &str = "PRX-INGRESS-SECOND-QUESTION";
const REPLY: &str = "PRX-INGRESS-REPLY";

fn serve(sandbox: &Sandbox, script: Script) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((5, 3)), script);
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.memory.auto_save = false;
    });
    server
}

/// `prx <args…>` with stdin closed at once, bounded.
fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    Running::spawn(sandbox.command(args)).wait()
}

/// The text of every user message of `request`.
fn user_texts(request: &serde_json::Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .filter_map(|message| message["content"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "{what} did not happen within TURN_DEADLINE"
        );
        std::thread::sleep(POLL);
    }
}

/// One durable ingress row: its state, its input text, its evidence cause and
/// its evidence text.
#[derive(Debug)]
struct IngressRow {
    state: String,
    input: String,
    cause: Option<String>,
    evidence: Option<String>,
}

/// Every ingress row of the real store, read-only and bounded.
fn ingress_rows(sandbox: &Sandbox) -> Vec<IngressRow> {
    let database = rusqlite::Connection::open_with_flags(
        sandbox.workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store");
    let mut statement = database
        .prepare(
            "SELECT r.state, substr(e.content, 1, 4096),
                    substr(json_extract(v.raw_payload_json, '$.ingress_recovery.cause'), 1, 64),
                    substr(v.content, 1, 4096)
             FROM ingress_recovery r
             JOIN message_events e ON e.event_id = r.input_event_id
             LEFT JOIN message_events v ON v.event_id = r.evidence_event_id
             ORDER BY r.cursor LIMIT ?1",
        )
        .expect("test: ingress query");
    let rows: Vec<IngressRow> = statement
        .query_map([i64::try_from(MAX_INGRESS_ROWS + 1).unwrap()], |row| {
            Ok(IngressRow {
                state: row.get(0)?,
                input: row.get(1)?,
                cause: row.get(2)?,
                evidence: row.get(3)?,
            })
        })
        .expect("test: ingress rows")
        .collect::<rusqlite::Result<_>>()
        .expect("test: ingress row");
    assert!(
        rows.len() <= MAX_INGRESS_ROWS,
        "more than {MAX_INGRESS_ROWS} ingress rows"
    );
    rows
}

fn row_for<'a>(rows: &'a [IngressRow], input: &str) -> &'a IngressRow {
    let matching: Vec<_> = rows.iter().filter(|row| row.input.contains(input)).collect();
    assert_eq!(matching.len(), 1, "one ingress row for {input}: {rows:?}");
    matching[0]
}

/// `turn.finalized` rows committed so far; 0 while the store is still being created.
fn finalized_terminals(sandbox: &Sandbox) -> i64 {
    let Ok(database) = rusqlite::Connection::open_with_flags(
        sandbox.workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return 0;
    };
    database
        .query_row(
            "SELECT COUNT(*) FROM message_events WHERE event_type = 'turn.finalized'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

#[test]
fn a_killed_chat_and_a_moved_lock_directory_do_not_block_the_next_turn() {
    let sandbox = Sandbox::new();
    let server = serve(
        &sandbox,
        Box::new(|index, _| {
            if index == 0 {
                Scripted::Stall
            } else {
                Scripted::text(REPLY)
            }
        }),
    );
    let mut chat = Running::spawn(sandbox.command(&["chat", "--plain"]));
    chat.send(FIRST);
    wait_until("the first provider request", || !server.requests().is_empty());
    // kill -9 mid-turn: no terminal, the lifetime lock is released by the kernel.
    drop(chat);

    let root = sandbox.workspace.join(".prx-ingress-recovery-v1");
    let locks: Vec<_> = std::fs::read_dir(&root)
        .expect("test: lock root")
        .take(2)
        .map(|entry| entry.expect("test: lock entry").path())
        .collect();
    assert_eq!(locks.len(), 1, "one lock directory: {locks:?}");
    std::fs::rename(&locks[0], locks[0].with_extension("moved")).expect("test: move the lock directory");

    let doctor = run(&sandbox, &["doctor", "ingress"]);
    assert_eq!(
        doctor.status.code(),
        Some(1),
        "a dead pending row is an error: {}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.contains("state=pending") && doctor.stdout.contains("lock=missing"),
        "doctor lists the stale row: {}",
        doctor.stdout
    );
    assert!(!doctor.stdout.contains(FIRST), "doctor prints no message content");

    let next = run(&sandbox, &["chat", "--message", SECOND]);
    assert_eq!(next.status.code(), Some(0), "stderr: {}", next.stderr);
    assert!(next.stdout.contains(REPLY), "stdout: {}", next.stdout);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "the new input reaches the provider once, nothing is re-sent"
    );
    // The killed input is never re-sent as an input message. (Like every
    // recorded event it may be listed in the recent workspace-events context.)
    let users = user_texts(&requests[1]);
    assert!(
        users.iter().all(|text| text.trim() != FIRST),
        "the killed turn is never re-sent: {users:?}"
    );
    let current = users.last().expect("test: the new input message");
    assert!(
        current.trim_end().ends_with(SECOND),
        "the request carries the new input as its input: {current}"
    );
    assert!(
        current
            .lines()
            .filter(|line| line.contains(FIRST))
            .all(|line| line.starts_with("- [")),
        "the killed input appears only as a listed workspace event: {current}"
    );

    let rows = ingress_rows(&sandbox);
    let old = row_for(&rows, FIRST);
    assert_eq!(old.state, "unconfirmed", "{rows:?}");
    assert_eq!(old.cause.as_deref(), Some("lock_missing"), "{rows:?}");
    let new = row_for(&rows, SECOND);
    assert_eq!(new.state, "reconciled", "{rows:?}");

    let doctor = run(&sandbox, &["doctor", "ingress"]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "no dead pending row remains: {}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.contains("state=unconfirmed cause=lock_missing"),
        "doctor lists the orphan with its cause: {}",
        doctor.stdout
    );
}

#[test]
fn a_one_shot_message_succeeds_while_a_repl_holds_the_scope() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox, Box::new(|_, _| Scripted::text(REPLY)));
    let mut repl = Running::spawn(sandbox.command(&["chat", "--plain"]));
    repl.send(FIRST);
    wait_until("the REPL's first terminal", || finalized_terminals(&sandbox) >= 1);

    let one_shot = run(&sandbox, &["chat", "--message", SECOND]);
    assert_eq!(one_shot.status.code(), Some(0), "stderr: {}", one_shot.stderr);
    assert!(one_shot.stdout.contains(REPLY), "stdout: {}", one_shot.stdout);
    assert_eq!(server.requests().len(), 2, "each input reaches the provider once");

    let rows = ingress_rows(&sandbox);
    for input in [FIRST, SECOND] {
        let row = row_for(&rows, input);
        assert_eq!(row.state, "reconciled", "{rows:?}");
        assert_eq!(
            row.evidence.as_deref(),
            Some("turn.ingress.reconciled final committed by its owner"),
            "each row is settled by its own owner: {rows:?}"
        );
    }

    repl.send("/exit");
    let output = repl.wait();
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr);
}

#[test]
fn a_normal_turn_leaves_no_pending_ingress_row() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text(REPLY)));
    let output = plain_chat::run_plain(&sandbox, &[FIRST]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr);
    assert!(output.stdout.contains(REPLY), "stdout: {}", output.stdout);

    let rows = ingress_rows(&sandbox);
    let row = row_for(&rows, FIRST);
    assert_eq!(row.state, "reconciled", "{rows:?}");
    assert!(rows.iter().all(|row| row.state != "pending"), "{rows:?}");

    let doctor = run(&sandbox, &["doctor", "ingress"]);
    assert_eq!(doctor.status.code(), Some(0), "stdout: {}", doctor.stdout);
    assert!(
        doctor.stdout.contains("no unsettled ingress rows"),
        "stdout: {}",
        doctor.stdout
    );
}

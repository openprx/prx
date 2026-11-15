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
    PLANT, READ_DONE, RECALL, RECALL_WINDOW_TOKENS, RECALLED, assistant_reply_count, carries_handoff_and_lookup,
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
    set_window(sandbox, &server, window);
    server
}

/// (Re)write the configuration for `server` with a `window`-token context.
fn set_window(sandbox: &Sandbox, server: &LoopbackOpenAi, window: usize) {
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
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

/// Filler files available to the switch case; the automatic switch comes
/// well before the last one.
const SWITCH_FILLERS: usize = 10;

/// One held `chat --plain` driven turn by turn.
struct HeldChat<'a> {
    sandbox: &'a Sandbox,
    running: Running,
}

impl<'a> HeldChat<'a> {
    fn spawn(sandbox: &'a Sandbox, args: &[&str]) -> Self {
        let mut command_args = vec!["chat", "--plain"];
        command_args.extend_from_slice(args);
        let command = sandbox.command(&command_args);
        Self {
            sandbox,
            running: Running::spawn(command),
        }
    }

    /// Send a turn and wait until the store holds `replies` replies.
    fn turn(&mut self, line: &str, replies: usize) {
        self.running.send(line);
        wait_for_replies(self.sandbox, replies);
    }

    fn command(&mut self, line: &str) {
        self.running.send(line);
    }

    fn exit(mut self) -> Output {
        self.running.send("/exit");
        self.running.wait()
    }
}

/// The id from the plain front end's `current chat session: <id>` line.
fn current_session_id(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("current chat session: "))
        .map_or_else(
            || panic!("no session id line in stderr: {stderr}"),
            |id| id.trim().to_owned(),
        )
}

/// The id from the `Created branch <id> from the first …` line.
fn created_branch_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Created branch "))
        .and_then(|rest| rest.split_whitespace().next())
        .map_or_else(|| panic!("no branch line in stdout: {stdout}"), str::to_owned)
}

fn assert_clean(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label}: exit {:?}; stderr: {}",
        output.status,
        output.stderr
    );
    assert!(
        !output.stderr.contains("panicked"),
        "{label}: a panic on stderr: {}",
        output.stderr
    );
}

/// The first request carrying a handoff of `generation` keeps the recent tail
/// verbatim and omits only the replaced cold prefix: only the system prompt
/// precedes the handoff, and every message after it but the current request
/// is byte-equal to a stored transcript message or to a message an earlier
/// request carried.
fn assert_switch_request_keeps_the_tail(sandbox: &Sandbox, requests: &[serde_json::Value], generation: u64) {
    let index = requests
        .iter()
        .position(|request| {
            compaction_script::handoff_note(request).is_some_and(|note| note["to_generation"] == generation)
        })
        .unwrap_or_else(|| panic!("a request carries the generation {generation} handoff"));
    let messages = requests[index]["messages"].as_array().expect("test: messages");
    let handoff_at = messages
        .iter()
        .position(|message| message.to_string().contains(compaction_script::HANDOFF_START))
        .expect("test: handoff message");
    assert!(
        messages[..handoff_at].iter().all(|message| message["role"] == "system"),
        "only the system prompt precedes the handoff: {:?}",
        &messages[..handoff_at]
    );
    let stored: Vec<String> = loopback_openai::message_events(&sandbox.workspace)
        .into_iter()
        .filter(|event| event.event_type == "message.created")
        .map(|event| event.content)
        .collect();
    let earlier: Vec<&serde_json::Value> = requests[..index]
        .iter()
        .filter_map(|request| request["messages"].as_array())
        .flatten()
        .collect();
    let tail = &messages[handoff_at + 1..];
    let (current, kept_tail) = tail
        .split_last()
        .expect("test: the current request follows the handoff");
    for message in kept_tail {
        let verbatim = earlier.contains(&message)
            || message["content"]
                .as_str()
                .is_some_and(|content| stored.iter().any(|row| row == content));
        assert!(verbatim, "a tail message is not carried verbatim: {message}");
    }
    let note = compaction_script::handoff_note(&requests[index]).expect("test: handoff note");
    assert_eq!(
        Some(tail.len() as u64),
        note["retained_recent_message_count"].as_u64(),
        "exactly the retained tail follows the handoff: {tail:?}"
    );
    assert!(
        !tail
            .iter()
            .any(|message| message.to_string().contains(compaction_script::CODENAME)),
        "the planted fact left the context with the cold prefix: {current}"
    );
}

/// The lookup returned exactly the stored planted row.
fn assert_lookup_returned_the_stored_row(sandbox: &Sandbox, server: &LoopbackOpenAi) {
    let row = compaction_script::planted_row_content(&sandbox.workspace);
    let results = compaction_script::lookup_results(&server.requests());
    assert!(!results.is_empty(), "a lookup result reached the model");
    assert!(
        results.iter().all(|result| result.contains(&row)),
        "every lookup result carries the stored row `{row}`: {results:?}"
    );
}

#[test]
fn switch_compaction_and_recall() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = configure(&sandbox, RECALL_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    let mut replies = 0;
    for line in [PLANT.to_owned(), read_line(0)] {
        replies += 1;
        chat.turn(&line, replies);
    }
    chat.command("/compact");
    let mut next_file = 1;
    while compaction_script::stored_handoffs(&sandbox.workspace).len() < 2 && next_file < SWITCH_FILLERS {
        replies += 1;
        chat.turn(&read_line(next_file), replies);
        next_file += 1;
    }
    // Right after the automatic switch: both read the compacted state.
    chat.command("/compact");
    chat.command("/branch 1");
    replies += 1;
    chat.turn(RECALL, replies);
    let first = chat.exit();
    assert_clean(&first, "first run");
    let session_id = current_session_id(&first.stderr);
    let branch_id = created_branch_id(&first.stdout);

    let mut resumed = HeldChat::spawn(&sandbox, &["--session", &session_id]);
    replies += 1;
    resumed.turn(RECALL, replies);
    let second = resumed.exit();
    assert_clean(&second, "restart");

    let original = compaction_script::session_handoffs(&sandbox.workspace, &session_id);
    compaction_script::assert_handoff_chain(&original, 2);
    assert!(
        first.stdout.contains("Compacted context"),
        "the manual compaction compacted: {}",
        first.stdout
    );
    assert!(!first.stdout.contains("lossy"), "{}", first.stdout);
    assert!(!first.stdout.contains("Compaction refused"), "{}", first.stdout);
    assert!(
        !compaction_script::session_handoffs(&sandbox.workspace, &branch_id).is_empty(),
        "/branch 1 kept the handoff turn of the compacted state"
    );
    assert_eq!(
        format!("{}{}", first.stdout, second.stdout).matches(RECALLED).count(),
        2,
        "the branch and the restarted session both recall the fact; stdout: {}\n{}",
        first.stdout,
        second.stdout
    );
    assert_lookup_returned_the_stored_row(&sandbox, &server);
    assert_switch_request_keeps_the_tail(&sandbox, &server.requests(), 2);
    assert!(
        !compaction_script::over_hard_limit_after_handoff(&sandbox.workspace),
        "the window fits handoff, tail and tool schemas"
    );
    assert!(
        first.stdout.contains(READ_DONE),
        "tool turns completed: {}",
        first.stdout
    );
    assert_eq!(
        first.stderr.matches("current chat session: ").count(),
        1,
        "the session id is printed once at start on stderr: {}",
        first.stderr
    );
    assert_handoff_and_recall(&sandbox, &server, &first);
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

#[test]
fn compact_right_after_a_restart_is_compacted() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, 2);
    let _server = configure(&sandbox, LARGE_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    for (replies, line) in [PLANT.to_owned(), read_line(0), read_line(1)].iter().enumerate() {
        chat.turn(line, replies + 1);
    }
    let first = chat.exit();
    assert_clean(&first, "first run");
    let session_id = current_session_id(&first.stderr);

    // No turn between the resume and `/compact`: the guard describes the
    // reducer history the resume loaded.
    let mut resumed = HeldChat::spawn(&sandbox, &["--session", &session_id]);
    resumed.command("/compact");
    let second = resumed.exit();
    assert_clean(&second, "restart");

    assert!(
        second.stdout.contains("Compacted context"),
        "/compact right after the restart compacted; stdout: {}\nstderr: {}",
        second.stdout,
        second.stderr
    );
    assert!(
        !second.stdout.contains("Compaction refused"),
        "the reducer accepted the compaction of the resumed history: {}",
        second.stdout
    );
    assert_eq!(
        compaction_script::session_handoffs(&sandbox.workspace, &session_id).len(),
        1,
        "the restarted session stored exactly one handoff"
    );
}

/// Rows `/branch` copied into the branch (`branch-copy:` idempotency keys).
fn branch_copy_rows(sandbox: &Sandbox) -> usize {
    let database = rusqlite::Connection::open_with_flags(
        sandbox.workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store");
    database
        .query_row(
            "SELECT COUNT(*) FROM message_events WHERE idempotency_key LIKE 'branch-copy:%'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_or(0, |count| usize::try_from(count).unwrap_or(0))
}

/// R3d: a nested chain (generation 2) whose copy exceeds 32 events is
/// branched; the branch switches again, restarts and still recalls the fact
/// through its own copies, while the original session's rows stay refused.
///
/// The chain is built by two manual compactions under a window no turn
/// crosses, so the first handoff alone references more than 32 rows; the
/// branch then runs under the recall window until it switches on its own.
#[test]
fn branch_of_a_nested_chain_recalls_the_fact_through_its_own_copies() {
    /// Short turns before the first compaction: two rows each, so the first
    /// handoff alone references more than 32 rows.
    const NOTES: usize = 17;
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = configure(&sandbox, LARGE_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    let mut replies = 1;
    chat.turn(PLANT, replies);
    for index in 0..NOTES {
        replies += 1;
        chat.turn(&format!("note {index}"), replies);
    }
    chat.command("/compact");
    for index in 0..3 {
        replies += 1;
        chat.turn(&format!("later note {index}"), replies);
    }
    chat.command("/compact");
    chat.command("/branch 1");
    let built = chat.exit();
    assert_clean(&built, "first run");
    let session_id = current_session_id(&built.stderr);
    let branch_id = created_branch_id(&built.stdout);
    let original = compaction_script::session_handoffs(&sandbox.workspace, &session_id);
    compaction_script::assert_handoff_chain(&original, 2);
    let first_batch = original[0].note["lookup"]["arguments"]["event_ids"]
        .as_array()
        .map_or(0, Vec::len);
    assert!(
        first_batch > 32,
        "the first handoff references more than 32 rows: {first_batch}"
    );
    assert!(
        branch_copy_rows(&sandbox) > 32,
        "the branch copy exceeds 32 events: {}; stdout: {}",
        branch_copy_rows(&sandbox),
        built.stdout
    );

    // In the branch, read until it switches on its own.
    set_window(&sandbox, &server, RECALL_WINDOW_TOKENS);
    let mut in_branch = HeldChat::spawn(&sandbox, &["--session", &branch_id]);
    let branch_handoffs = |sandbox: &Sandbox| compaction_script::session_handoffs(&sandbox.workspace, &branch_id).len();
    let copied_handoffs = branch_handoffs(&sandbox);
    let mut next_file = 0;
    while branch_handoffs(&sandbox) == copied_handoffs && next_file < SWITCH_FILLERS {
        replies += 1;
        in_branch.turn(&read_line(next_file), replies);
        next_file += 1;
    }
    let switched = in_branch.exit();
    assert_clean(&switched, "branch run");
    let branch = compaction_script::session_handoffs(&sandbox.workspace, &branch_id);
    compaction_script::assert_handoff_chain(&branch, 3);
    let original_ids: Vec<&str> = original.iter().map(|handoff| handoff.event_id.as_str()).collect();
    assert!(
        branch
            .iter()
            .all(|handoff| !original_ids.contains(&handoff.event_id.as_str())),
        "every branch handoff is a copy with its own id"
    );

    // The restart follows the whole chain in one turn: three lookup results
    // (the first handoff's alone names 36+ rows) stay inline only with room.
    set_window(&sandbox, &server, LARGE_WINDOW_TOKENS);
    let mut resumed = HeldChat::spawn(&sandbox, &["--session", &branch_id]);
    replies += 1;
    resumed.turn(RECALL, replies);
    let original_lookup = original[0].note["lookup"]["arguments"].to_string();
    replies += 1;
    resumed.turn(
        &format!("{}{original_lookup}", compaction_script::LOOKUP_EXACTLY),
        replies,
    );
    let second = resumed.exit();
    assert_clean(&second, "branch restart");

    assert!(
        second.stdout.contains(RECALLED),
        "the restarted branch recalls the fact through its copies: {}",
        second.stdout
    );
    assert!(
        second.stdout.contains(compaction_script::LOOKUP_REFUSED),
        "the original session's rows are refused from the branch: {}",
        second.stdout
    );
    assert_lookup_returned_the_stored_row(&sandbox, &server);
    assert!(
        !compaction_script::over_hard_limit_after_handoff(&sandbox.workspace),
        "no request after the handoff exceeded the hard limit"
    );
}

/// Controller ruling 3 (G7a): after a compaction, `[Current session context]`
/// quotes none of the session's stored events the handoff replaced, so the
/// planted fact reaches the model only through the handoff's lookup pointer.
#[test]
fn a_compacted_fact_reaches_the_model_only_through_the_lookup() {
    const AFTER: &str = "a note after the compaction";
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, 2);
    let server = configure(&sandbox, LARGE_WINDOW_TOKENS);
    let lines = vec![
        PLANT.to_owned(),
        read_line(0),
        read_line(1),
        "/compact".to_owned(),
        AFTER.to_owned(),
        RECALL.to_owned(),
    ];
    let output = held_chat(&sandbox, &lines);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(output.stdout.contains("Compacted context"), "{}", output.stdout);

    let requests = server.requests();
    let recall = requests
        .iter()
        .find(|request| {
            request["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .is_some_and(|message| message.to_string().contains(RECALL))
        })
        .expect("test: the recall request");
    let mut outside_handoff = recall.clone();
    outside_handoff["messages"]
        .as_array_mut()
        .expect("test: messages")
        .retain(|message| !message.to_string().contains(compaction_script::HANDOFF_START));
    let outside_handoff = outside_handoff.to_string();
    assert!(
        outside_handoff.contains("[Current session context]") && outside_handoff.contains(AFTER),
        "the request still quotes the session's rows after the handoff: {outside_handoff}"
    );
    assert!(
        !outside_handoff.contains(compaction_script::CODENAME),
        "the planted fact appears outside the handoff: {outside_handoff}"
    );
    assert!(
        carries_handoff_and_lookup(recall),
        "the recall request carries the handoff and the lookup tool"
    );
    let planted = compaction_script::planted_row_content(&sandbox.workspace);
    let results = compaction_script::lookup_results(&requests);
    assert!(
        !results.is_empty() && results.iter().all(|result| result.contains(&planted)),
        "the lookup returned the stored planted row `{planted}`: {results:?}"
    );
    assert!(output.stdout.contains(RECALLED), "stdout: {}", output.stdout);
}

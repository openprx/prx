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
    PLANT, RECALL, RECALL_WINDOW_TOKENS, RECALLED, assistant_reply_count, carries_handoff_and_lookup, read_line,
    switch_event_counts, write_filler_files,
};
use loopback_openai::{LoopbackOpenAi, Script, Scripted, write_config};
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
    set_window_and_mode(
        sandbox,
        server,
        window,
        openprx::config::schema::AgentCompactionMode::Switch,
    );
}

/// (Re)write the configuration for `server` with a `window`-token context in
/// compaction `mode`.
fn set_window_and_mode(
    sandbox: &Sandbox,
    server: &LoopbackOpenAi,
    window: usize,
    mode: openprx::config::schema::AgentCompactionMode,
) {
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = mode;
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

    #[cfg(not(feature = "terminal-tui"))]
    fn interrupt(&self) {
        self.running.interrupt();
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

/// `text` without the runtime's tool-evidence line: a replayed assistant
/// reply is its stored text plus that line, which is never stored.
fn without_tool_evidence(text: &str) -> &str {
    text.rfind("\n\n[tool evidence: ")
        .filter(|at| text[*at..].ends_with(']') && !text[*at + 2..].contains('\n'))
        .map_or(text, |at| &text[..at])
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

/// An ordinary line the script answers with plain text (no tool call).
const ORDINARY: &str = "Just say noted, nothing else.";

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

/// The newest user message of `request`.
fn newest_user_text(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
        .map(ToString::to_string)
        .unwrap_or_default()
}

/// Longest wait for the cancelled call's connection to close.
#[cfg(not(feature = "terminal-tui"))]
const CANCEL_DEADLINE: Duration = Duration::from_secs(30);

/// The user line whose first call the provider refuses as a context overflow.
const FORCE_OVERFLOW: &str = "FORCE-OVERFLOW say noted";

/// The compaction script, except that the first call of the `FORCE_OVERFLOW`
/// turn is refused as a context overflow and every later call of that turn
/// is answered `NOTED`; `forced` is set once the call was refused.
fn script_forcing_one_overflow(forced: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Script {
    let mut base = compaction_script::script();
    Box::new(move |index, request| {
        if newest_user_text(request).contains(FORCE_OVERFLOW) {
            if forced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Scripted::text(compaction_script::NOTED);
            }
            return Scripted::Error {
                status: 400,
                message: "This model's maximum context length is 8192 tokens (context_length_exceeded)".to_owned(),
            };
        }
        base(index, request)
    })
}

/// The text of a request message (string or text parts).
fn message_text(message: &serde_json::Value) -> String {
    match &message["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// Wait until the saved session row ends with the ordinary turn's reply.
fn wait_for_saved_ordinary_turn(sandbox: &Sandbox) -> Vec<(String, String)> {
    let started = Instant::now();
    loop {
        let saved = compaction_script::saved_session_turns(&sandbox.workspace);
        let committed =
            saved.len() >= 2 && saved[saved.len() - 2].1.contains(ORDINARY) && saved[saved.len() - 1].0 == "assistant";
        if committed {
            return saved;
        }
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "the saved session did not commit the ordinary turn within TURN_DEADLINE: {saved:?}"
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn an_overflow_recovery_after_the_preflight_is_adopted_by_the_session() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let forced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = LoopbackOpenAi::start(
        MODEL,
        Some((11, 7)),
        script_forcing_one_overflow(std::sync::Arc::clone(&forced)),
    );
    // A window no preflight crosses: only the provider's refusal compacts.
    set_window(&sandbox, &server, LARGE_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    chat.turn(PLANT, 1);
    chat.turn(&read_line(0), 2);
    chat.turn(&read_line(1), 3);
    chat.turn(FORCE_OVERFLOW, 4);
    assert!(
        forced.load(std::sync::atomic::Ordering::SeqCst),
        "the provider refused one call as an overflow"
    );
    chat.turn(ORDINARY, 5);
    // The saved session once the ordinary turn is committed.
    let saved_before_compact = wait_for_saved_ordinary_turn(&sandbox);
    chat.command("/compact");
    let output = chat.exit();
    assert_clean(&output, "overflow recovery then an ordinary turn and /compact");

    // The recovery ran in this turn (switch mode records its degradation).
    let (_, unavailable) = switch_event_counts(&sandbox.workspace);
    assert!(
        unavailable >= 1 || !compaction_script::stored_handoffs(&sandbox.workspace).is_empty(),
        "the overflow recovery compacted the history; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
    // The next provider request reads the saved session: every saved turn
    // before the ordinary one, in order, and nothing the session dropped.
    let ordinary = server
        .requests()
        .into_iter()
        .find(|request| newest_user_text(request).contains(ORDINARY))
        .expect("the ordinary turn reached the provider");
    let sent: Vec<(String, String)> = ordinary["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_owned(),
                message_text(message),
            )
        })
        .collect();
    let expected = &saved_before_compact[..saved_before_compact.len() - 1];
    assert_eq!(
        sent.len(),
        expected.len(),
        "the ordinary request carries exactly the saved turns\nsent: {sent:?}\nsaved: {expected:?}"
    );
    for ((sent_role, sent_text), (saved_role, saved_text)) in sent.iter().zip(expected) {
        assert!(
            sent_role == saved_role && without_tool_evidence(sent_text).ends_with(saved_text.as_str()),
            "the ordinary request matches the saved session turn for turn\nsent: {sent:?}\nsaved: {expected:?}"
        );
    }
    // `/compact` read the same history the ordinary request was built from:
    // everything after its leading system prompt, plus the committed reply.
    let request_messages = ordinary["messages"].as_array().map_or(0, Vec::len);
    let leading_system = usize::from(ordinary["messages"][0]["role"] == "system");
    let compacted_from: usize = output
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Compacted context: "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no compaction line in stdout: {}", output.stdout));
    assert_eq!(
        compacted_from,
        request_messages - leading_system + 1,
        "/compact compacted the history the ordinary request was built from; stdout: {}",
        output.stdout
    );
}

/// The user line of the multi-round tool turn.
const MULTI_READ: &str = "MULTI-READ every filler";
/// Tool rounds of the [`MULTI_READ`] turn before its text reply.
const MULTI_READ_ROUNDS: usize = 8;

/// The compaction script, except that the [`MULTI_READ`] turn reads one
/// filler file per provider call for [`MULTI_READ_ROUNDS`] calls, so its
/// tool output grows the history past the window inside the turn.
fn script_with_multi_read() -> Script {
    let mut base = compaction_script::script();
    let mut rounds = 0;
    Box::new(move |index, request| {
        if newest_user_text(request).contains(MULTI_READ) {
            if rounds < MULTI_READ_ROUNDS {
                let file = format!("f{rounds}.txt");
                rounds += 1;
                return Scripted::tool(
                    &format!("call-multi-{index}"),
                    "file_read",
                    &serde_json::json!({ "path": file }),
                );
            }
            return Scripted::text(compaction_script::NOTED);
        }
        base(index, request)
    })
}

/// The per-provider preflight inside a tool turn (after tool output grew the
/// history past the window) shrinks that request's view, through real
/// HTTP/SSE and real SQLite. A trim is the projection of one request and a
/// handoff is built from the untrimmed canonical history, so the session keeps
/// every turn (as bytes or through the handoff), and the next ordinary request
/// is exactly the saved session (the run loop's view never keeps a change the
/// session does not have).
#[test]
fn a_mid_turn_preflight_trim_leaves_the_session_and_the_next_request_in_agreement() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script_with_multi_read());
    set_window(&sandbox, &server, compaction_script::SMALL_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    chat.turn(PLANT, 1);
    let planted_events = first_message_event_ids(&sandbox, 2);
    chat.turn(MULTI_READ, 2);
    chat.turn(ORDINARY, 3);
    let saved = wait_for_saved_ordinary_turn(&sandbox);
    chat.command("/compact");
    let output = chat.exit();
    assert_clean(&output, "a multi-round tool turn then an ordinary turn");

    let requests = server.requests();
    let multi_sizes: Vec<usize> = requests
        .iter()
        .filter(|request| newest_user_text(request).contains(MULTI_READ))
        .map(|request| request["messages"].to_string().len())
        .collect();
    let peak = multi_sizes.iter().copied().max().unwrap_or_default();
    assert!(
        multi_sizes.len() > MULTI_READ_ROUNDS / 2 && multi_sizes.last().is_some_and(|last| *last < peak),
        "the preflight trimmed inside the tool turn (its requests shrank after their peak): {multi_sizes:?}"
    );
    let saved_handoff_refs = saved_handoff_references(&saved);
    assert!(
        saved
            .iter()
            .any(|(_, content)| content.contains(compaction_script::CODENAME))
            || planted_events.iter().all(|event| saved_handoff_refs.contains(event)),
        "the session lost no turn: the planted exchange is kept as bytes or through the handoff: {saved:?}"
    );
    let ordinary = requests
        .iter()
        .find(|request| newest_user_text(request).contains(ORDINARY))
        .expect("the ordinary turn reached the provider");
    let sent: Vec<(String, String)> = ordinary["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_owned(),
                message_text(message),
            )
        })
        .collect();
    let expected = &saved[..saved.len() - 1];
    assert_eq!(
        sent.len(),
        expected.len(),
        "the ordinary request carries exactly the saved turns\nsent: {sent:?}\nsaved: {expected:?}"
    );
    for ((sent_role, sent_text), (saved_role, saved_text)) in sent.iter().zip(expected) {
        assert!(
            sent_role == saved_role
                && without_tool_evidence(sent_text).ends_with(as_provider_sees(saved_text).as_str()),
            "the ordinary request matches the saved session turn for turn\nsent: {sent:?}\nsaved: {expected:?}"
        );
    }
    assert!(
        output.stdout.contains("Compacted context") && !output.stdout.contains("Compaction refused"),
        "/compact reads the same history; stdout: {}",
        output.stdout
    );
}

/// The `/compact` feedback when no handoff or summary can be built.
const NO_DURABLE_COMPACTION: &str =
    "No durable compaction available; requests will be trimmed per request. Nothing was changed.";

/// The user and assistant message texts of `request`, in order.
fn conversation_texts(request: &serde_json::Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "user" || message["role"] == "assistant")
                .map(message_text)
                .collect()
        })
        .unwrap_or_default()
}

/// The saved session turns once the row holds `count` of them (the reply
/// event is stored before the row is saved).
fn wait_for_saved_turn_count(sandbox: &Sandbox, count: usize) -> Vec<(String, String)> {
    let started = Instant::now();
    loop {
        let turns = compaction_script::saved_session_turns(&sandbox.workspace);
        if turns.len() >= count {
            return turns;
        }
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "the saved session row did not reach {count} turns within TURN_DEADLINE: {turns:?}"
        );
        std::thread::sleep(POLL);
    }
}

/// `/compact` when no handoff or summary can be built (`off` mode) changes
/// nothing in the session: it reports the typed outcome, the saved turns
/// before it are kept byte for byte, and the next request still carries them.
#[test]
fn manual_compact_without_a_durable_compaction_changes_nothing() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, 2);
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), compaction_script::script());
    set_window_and_mode(
        &sandbox,
        &server,
        LARGE_WINDOW_TOKENS,
        openprx::config::schema::AgentCompactionMode::Off,
    );
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    for (replies, line) in [PLANT.to_owned(), read_line(0), read_line(1)].iter().enumerate() {
        chat.turn(line, replies + 1);
    }
    let before = wait_for_saved_turn_count(&sandbox, 6);
    chat.command("/compact");
    chat.turn("a plain follow-up", 4);
    let output = chat.exit();
    assert_clean(&output, "manual compact without a durable compaction");

    assert!(
        output.stdout.contains(NO_DURABLE_COMPACTION),
        "/compact reports that no durable compaction was available: {}",
        output.stdout
    );
    assert!(!output.stdout.contains("Compacted context"), "{}", output.stdout);
    assert!(!output.stdout.contains("Context trimmed (lossy)"), "{}", output.stdout);
    let after = compaction_script::saved_session_turns(&sandbox.workspace);
    assert_eq!(
        before.len(),
        6,
        "three exchanges were saved before /compact: {before:?}"
    );
    assert_eq!(
        after.get(..before.len()),
        Some(before.as_slice()),
        "the turns saved before /compact are retained byte for byte"
    );
    assert_eq!(
        after.len(),
        before.len() + 2,
        "only the follow-up exchange was added: {after:?}"
    );
    let requests = server.requests();
    let follow_up = requests
        .iter()
        .rev()
        .find(|request| newest_user_text(request).contains("a plain follow-up"))
        .expect("test: the follow-up request was sent");
    let texts = conversation_texts(follow_up);
    assert!(
        texts.iter().any(|text| text.contains(PLANT)),
        "the next request still carries the oldest turn: {texts:?}"
    );
}

/// Bytes of each oversized `off`-mode user line (about 600 tokens).
const OVERSIZED_LINE_BYTES: usize = 2_400;
/// Oversized lines sent in the `off`-mode trim case; together they exceed
/// [`compaction_script::SMALL_WINDOW_TOKENS`].
const OVERSIZED_LINES: usize = 8;

/// The `index`-th oversized user line.
fn oversized_line(index: usize) -> String {
    let head = format!("oversized note {index} ");
    let filler = "lorem ipsum dolor sit amet ".repeat(OVERSIZED_LINE_BYTES / 27 + 1);
    format!("{head}{}", &filler[..OVERSIZED_LINE_BYTES - head.len()])
}

/// A keep window larger than any history here: no switch handoff can ever
/// be built (there is nothing older than the kept tail to replace).
const KEEP_EVERYTHING: usize = 64;

/// In `switch` mode with a keep window larger than the history no handoff
/// can be built (and `switch` never pages out), so once the session outgrows
/// the window every request is trimmed: the preflight, the rollover and the
/// turn's own preflight trim the provider view for that request only. The
/// saved session keeps every turn byte for byte.
#[test]
fn a_per_request_trim_never_reaches_the_saved_session() {
    let sandbox = Sandbox::new();
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), compaction_script::script());
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = compaction_script::SMALL_WINDOW_TOKENS;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = KEEP_EVERYTHING;
        config.memory.auto_save = false;
    });
    let lines: Vec<String> = (0..OVERSIZED_LINES).map(oversized_line).collect();
    let output = held_chat(&sandbox, &lines);
    assert_clean(&output, "per-request trim");

    let expected: Vec<(String, String)> = lines
        .iter()
        .flat_map(|line| {
            [
                ("user".to_owned(), line.clone()),
                ("assistant".to_owned(), compaction_script::NOTED.to_owned()),
            ]
        })
        .collect();
    assert_eq!(
        compaction_script::saved_session_turns(&sandbox.workspace),
        expected,
        "the saved session keeps every turn byte for byte"
    );
    let requests = server.requests();
    let last = requests
        .iter()
        .rev()
        .find(|request| newest_user_text(request).contains(lines.last().map_or("", String::as_str)))
        .expect("test: the last line's request was sent");
    let carried = conversation_texts(last)
        .iter()
        .filter(|text| text.contains("oversized note"))
        .count();
    assert!(
        carried < OVERSIZED_LINES,
        "the last request was trimmed ({carried} of {OVERSIZED_LINES} lines carried), so the case exercised a trim"
    );
}

/// The user line of the multi-round tool turn in which a per-request trim
/// runs first and a durable handoff follows.
const TRIM_THEN_HANDOFF: &str = "TRIM-THEN-HANDOFF read every filler";
/// Messages a handoff keeps verbatim in the trim-then-handoff case. The tool
/// turn crosses the window while the history still has at most this many
/// non-system messages, so nothing can be handed off and the preflight trims
/// the request (dropping the planted exchange); two tool rounds later the
/// history outgrows it by the planted exchange, which the handoff then
/// replaces.
const TRIM_THEN_HANDOFF_KEEP: usize = 15;

/// What the [`TRIM_THEN_HANDOFF`] script saw.
#[derive(Default)]
struct TrimThenHandoff {
    /// A request of the turn carried neither the planted message nor a
    /// handoff: the per-request trim had left the planted exchange out.
    trimmed_before_handoff: std::sync::atomic::AtomicBool,
}

/// Whether `request` carries the planted user message itself (the request's
/// session-context preamble quotes it too, so a substring match would not
/// tell).
fn carries_planted_message(request: &serde_json::Value) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|message| message_text(message) == PLANT))
}

/// The compaction script, except that the [`TRIM_THEN_HANDOFF`] turn reads one
/// filler file per provider call until its request carries a handoff, then
/// ends with `NOTED`.
fn script_trim_then_handoff(seen: std::sync::Arc<TrimThenHandoff>) -> Script {
    let mut base = compaction_script::script();
    let mut rounds = 0;
    Box::new(move |index, request| {
        if !newest_user_text(request).contains(TRIM_THEN_HANDOFF) {
            return base(index, request);
        }
        if compaction_script::handoff_id(request).is_some() || rounds >= SWITCH_FILLERS {
            return Scripted::text(compaction_script::NOTED);
        }
        if !carries_planted_message(request) {
            seen.trimmed_before_handoff
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let file = format!("f{rounds}.txt");
        rounds += 1;
        Scripted::tool(
            &format!("call-trim-{index}"),
            "file_read",
            &serde_json::json!({ "path": file }),
        )
    })
}

/// The event ids of the first `count` `message.created` rows of the real
/// store (the turns before the multi-round tool turn), oldest first.
fn first_message_event_ids(sandbox: &Sandbox, count: i64) -> Vec<String> {
    let database = rusqlite::Connection::open_with_flags(
        sandbox.workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store");
    let mut statement = database
        .prepare("SELECT event_id FROM message_events WHERE event_type = 'message.created' ORDER BY id LIMIT ?1")
        .expect("test: event id query");
    statement
        .query_map([count], |row| row.get::<_, String>(0))
        .expect("test: event id rows")
        .map(|row| row.expect("test: event id row"))
        .collect()
}

/// The saved session turns once the row ends with the reply to `line`.
fn wait_for_saved_reply_to(sandbox: &Sandbox, line: &str) -> Vec<(String, String)> {
    let started = Instant::now();
    loop {
        let saved = compaction_script::saved_session_turns(&sandbox.workspace);
        let committed =
            saved.len() >= 2 && saved[saved.len() - 2].1.contains(line) && saved[saved.len() - 1].0 == "assistant";
        if committed {
            return saved;
        }
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "the saved session did not commit the reply to `{line}` within TURN_DEADLINE: {saved:?}"
        );
        std::thread::sleep(POLL);
    }
}

/// The event ids a handoff note points its lookup at.
fn referenced_event_ids(note: &serde_json::Value) -> Vec<String> {
    note["lookup"]["arguments"]["event_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The event ids every handoff turn of a saved session points its lookup at.
fn saved_handoff_references(saved: &[(String, String)]) -> Vec<String> {
    saved
        .iter()
        .filter_map(|(_, content)| {
            let start = content.find(compaction_script::HANDOFF_START)? + compaction_script::HANDOFF_START.len();
            let end = content[start..].find(compaction_script::HANDOFF_END)? + start;
            serde_json::from_str::<serde_json::Value>(content[start..end].trim()).ok()
        })
        .flat_map(|note| referenced_event_ids(&note))
        .collect()
}

/// A per-request trim runs first inside a multi-round tool turn and leaves
/// the planted exchange out of the provider view; later in the same turn the
/// preflight hands off, through real HTTP/SSE and real SQLite. The handoff is
/// built from the untrimmed canonical history, so it references the planted
/// exchange the trim left out, the saved session keeps every turn from before
/// the trim (as retained bytes or through the handoff), and a later lookup
/// recovers the planted fact.
#[test]
fn a_handoff_after_a_trim_in_the_same_turn_keeps_the_trimmed_turns() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let seen = std::sync::Arc::new(TrimThenHandoff::default());
    let server = LoopbackOpenAi::start(
        MODEL,
        Some((11, 7)),
        script_trim_then_handoff(std::sync::Arc::clone(&seen)),
    );
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = compaction_script::SMALL_WINDOW_TOKENS;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = TRIM_THEN_HANDOFF_KEEP;
        config.memory.auto_save = false;
    });
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    chat.turn(PLANT, 1);
    let pre_trim_events = first_message_event_ids(&sandbox, 2);
    chat.turn(TRIM_THEN_HANDOFF, 2);
    let saved = wait_for_saved_reply_to(&sandbox, TRIM_THEN_HANDOFF);
    chat.turn(RECALL, 3);
    let output = chat.exit();
    assert_clean(&output, "a trim then a handoff in one turn, then a recall");

    let shapes: Vec<(usize, bool, bool)> = server
        .requests()
        .iter()
        .filter(|request| newest_user_text(request).contains(TRIM_THEN_HANDOFF))
        .map(|request| {
            (
                request["messages"].as_array().map_or(0, Vec::len),
                carries_planted_message(request),
                compaction_script::handoff_id(request).is_some(),
            )
        })
        .collect();
    assert!(
        seen.trimmed_before_handoff.load(std::sync::atomic::Ordering::SeqCst),
        "a request of the tool turn left the planted exchange out before any handoff, so the trim ran first; \
         requests (messages, planted message, handoff): {shapes:?}\nsaved: {saved:?}"
    );
    assert_eq!(
        pre_trim_events.len(),
        2,
        "the planted exchange was stored: {pre_trim_events:?}"
    );
    // The handoff the tool turn itself saved (the row is read before the
    // recall turn starts).
    let saved_handoff_refs = saved_handoff_references(&saved);
    assert!(
        pre_trim_events.iter().all(|event| saved_handoff_refs.contains(event)),
        "the tool turn's handoff references the turns the earlier trim left out: {pre_trim_events:?} in \
         {saved_handoff_refs:?}; requests: {shapes:?}\nsaved: {saved:?}"
    );
    let pre_trim_turns = [PLANT, compaction_script::NOTED];
    for (event, text) in pre_trim_events.iter().zip(pre_trim_turns) {
        assert!(
            saved_handoff_refs.contains(event) || saved.iter().any(|(_, content)| content == text),
            "the saved session keeps the pre-trim turn `{text}` as bytes or through its handoff: {saved:?}"
        );
    }
    assert!(
        saved.iter().any(|(_, content)| content.contains(TRIM_THEN_HANDOFF)),
        "the saved session holds the tool turn's request: {saved:?}"
    );

    assert!(
        output.stdout.contains(RECALLED),
        "the lookup recovered the fact planted in the trimmed turn; stdout: {}\nstderr: {}",
        output.stdout,
        output.stderr
    );
    assert_lookup_returned_the_stored_row(&sandbox, &server);
}

/// The bytes the file-mention enrichment adds to the enriched request of the
/// [`ENRICHED_REQUEST`] turn (the mentioned file's content).
const ENRICHMENT_MARKER: &str = "ENRICHMENT-ONLY-5e1d";
/// A request whose provider form carries the content of the mentioned file.
const ENRICHED_REQUEST: &str = "keep @enrichment.txt in mind";

/// An enriched earlier request, then a durable compaction in the middle of a
/// later multi-round tool turn, through real HTTP/SSE and real SQLite. The
/// turn's canonical history is the acknowledged session plus the plain
/// request, so the handoff resolves the earlier turn's exact provenance and
/// neither the saved session nor any stored handoff holds the enrichment,
/// which only ever reached the provider.
#[test]
fn an_enriched_earlier_request_stays_plain_through_a_mid_turn_compaction() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    std::fs::write(
        sandbox.workspace.join("enrichment.txt"),
        format!("{ENRICHMENT_MARKER}\n"),
    )
    .expect("test: write the mentioned file");
    let seen = std::sync::Arc::new(TrimThenHandoff::default());
    let server = LoopbackOpenAi::start(
        MODEL,
        Some((11, 7)),
        script_trim_then_handoff(std::sync::Arc::clone(&seen)),
    );
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = compaction_script::SMALL_WINDOW_TOKENS;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = TRIM_THEN_HANDOFF_KEEP;
        config.memory.auto_save = false;
    });
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    chat.turn(ENRICHED_REQUEST, 1);
    let enriched_events = first_message_event_ids(&sandbox, 2);
    chat.turn(TRIM_THEN_HANDOFF, 2);
    let saved = wait_for_saved_reply_to(&sandbox, TRIM_THEN_HANDOFF);
    let output = chat.exit();
    assert_clean(&output, "an enriched request, then a mid-turn compaction");

    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| newest_user_text(request).contains(ENRICHMENT_MARKER)),
        "the first turn's provider request carried the file enrichment"
    );
    assert!(
        requests
            .iter()
            .any(|request| newest_user_text(request).contains(TRIM_THEN_HANDOFF)
                && compaction_script::handoff_id(request).is_some()),
        "the tool turn handed off in the middle of the turn; saved: {saved:?}"
    );
    assert!(
        saved.iter().all(|(_, content)| !content.contains(ENRICHMENT_MARKER)),
        "the saved session holds no enrichment: {saved:?}"
    );
    let handoff_refs = saved_handoff_references(&saved);
    assert!(
        enriched_events.iter().all(|event| handoff_refs.contains(event))
            || saved.iter().any(|(_, content)| content == ENRICHED_REQUEST),
        "the earlier request is kept as its plain bytes or through an exact handoff reference: \
         events {enriched_events:?}, references {handoff_refs:?}, saved {saved:?}"
    );
    let stored = compaction_script::stored_handoffs(&sandbox.workspace);
    assert!(!stored.is_empty(), "the mid-turn handoff was stored");
    let notes: Vec<String> = stored.iter().map(|handoff| handoff.note.to_string()).collect();
    assert!(
        notes.iter().all(|note| !note.contains(ENRICHMENT_MARKER)),
        "no stored handoff holds the enrichment: {notes:?}"
    );
}

/// The system prompt the compaction summarizer receives.
const SUMMARIZER_PROMPT: &str = "context compaction summarizer";
/// The prefix of every summary the scripted summarizer writes; the summary's
/// ordinal follows it.
const SUMMARY_PREFIX: &str = "SUMMARY-ORDINAL-";

/// Whether `request` is a compaction summarizer call.
fn is_summarizer_request(request: &serde_json::Value) -> bool {
    request["messages"].as_array().is_some_and(|messages| {
        messages
            .iter()
            .any(|message| message["role"] == "system" && message_text(message).contains(SUMMARIZER_PROMPT))
    })
}

/// The compaction script with a summarizer that numbers its summaries and a
/// [`MULTI_READ`] turn of [`MULTI_READ_ROUNDS`] reads.
fn script_numbered_summaries() -> Script {
    let mut base = script_with_multi_read();
    let mut summaries = 0usize;
    Box::new(move |index, request| {
        if is_summarizer_request(request) {
            summaries += 1;
            return Scripted::text(&format!(
                "## Decisions\n{SUMMARY_PREFIX}{summaries}\n## Progress\nfiller files read\n## Pending\nnone"
            ));
        }
        base(index, request)
    })
}

/// Largest `chat.log` the consecutive-summary case reads.
const CHAT_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Lines of stderr and `chat.log` that record a refused compaction patch.
/// `chat.log` is read up to [`CHAT_LOG_MAX_BYTES`] plus one byte; more than
/// the limit fails the test, so no refusal beyond a prefix goes unseen.
fn refused_patch_lines(sandbox: &Sandbox, stderr: &str) -> Vec<String> {
    use std::io::Read as _;
    let mut chat_log_bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(sandbox.config_dir.join("chat.log")) {
        file.take(CHAT_LOG_MAX_BYTES + 1)
            .read_to_end(&mut chat_log_bytes)
            .expect("test: read chat.log");
    }
    assert!(
        u64::try_from(chat_log_bytes.len()).is_ok_and(|len| len <= CHAT_LOG_MAX_BYTES),
        "chat.log exceeds CHAT_LOG_MAX_BYTES ({CHAT_LOG_MAX_BYTES} bytes); refused patches past the cap would be unseen"
    );
    let chat_log = String::from_utf8_lossy(&chat_log_bytes);
    stderr
        .lines()
        .chain(chat_log.lines())
        .filter(|line| {
            line.contains("HistoryCompactionPatch rejected")
                || line.contains("does not describe the session history")
                || line.contains("does not match its source")
                || line.contains("does not replay to its compacted history")
        })
        .map(str::to_owned)
        .collect()
}

/// Summary compactions in consecutive turns, the first an outer rollover at a
/// turn's start, through real HTTP/SSE and real SQLite: every summary is
/// accepted by the session (no refused patch, no guard mismatch), and the
/// saved session ends on the newest summary.
#[test]
fn consecutive_summary_compactions_after_an_outer_rollover_are_all_accepted() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script_numbered_summaries());
    set_window_and_mode(
        &sandbox,
        &server,
        compaction_script::SMALL_WINDOW_TOKENS,
        openprx::config::schema::AgentCompactionMode::Safeguard,
    );
    let mut command = sandbox.command(&["chat", "--plain"]);
    command.env("RUST_LOG", "warn");
    let mut chat = HeldChat {
        sandbox: &sandbox,
        running: Running::spawn(command),
    };
    let mut replies = 0;
    for index in 0..4 {
        replies += 1;
        chat.turn(&read_line(index), replies);
    }
    replies += 1;
    chat.turn(MULTI_READ, replies);
    replies += 1;
    chat.turn(&read_line(9), replies);
    let saved = wait_for_saved_reply_to(&sandbox, &read_line(9));
    let output = chat.exit();
    assert_clean(&output, "consecutive summary compactions");

    let summaries = server
        .requests()
        .iter()
        .filter(|request| is_summarizer_request(request))
        .count();
    assert!(
        summaries >= 2,
        "at least two summary compactions ran ({summaries}); saved: {saved:?}"
    );
    let refused = refused_patch_lines(&sandbox, &output.stderr);
    assert!(
        refused.is_empty(),
        "no compaction patch was refused or mismatched its guard: {refused:?}"
    );
    let newest = format!("{SUMMARY_PREFIX}{summaries}");
    assert!(
        saved.iter().any(|(_, content)| content.contains(&newest)),
        "the saved session adopted the newest summary `{newest}`: {saved:?}"
    );
}

/// PR #207 review r10. Non-TUI only: the inline turn records its request at
/// admission, so an adopted outer preflight saves a snapshot that already
/// holds it. The TUI build runs every turn on a detached provider worker,
/// which records the request only at its commit (and, in this scenario, its
/// overflow recovery writes no summary), so the case does not arise there.
#[cfg(not(feature = "terminal-tui"))]
mod hot_request {
    use super::*;

    /// The marker of the request that an outer preflight and then an in-turn
    /// compaction both keep in their hot tail (PR #207 review r10).
    const HOT_REQUEST_MARKER: &str = "HOT-REQUEST-R10";
    /// The reply of the hot request once the provider answers it.
    const HOT_REPLY: &str = "HOT-REQUEST-ANSWERED";
    /// Ordinary turns before the hot request.
    const HOT_PRIOR_TURNS: usize = 2;
    /// Bytes of each earlier line (about 1,900 tokens; its `[Current session
    /// context]` quote in a later request is cut to 3 KiB).
    const HOT_PRIOR_BYTES: usize = 6_000;
    /// Bytes of the hot request (about 4,500 tokens).
    const HOT_REQUEST_BYTES: usize = 14_000;
    /// The window of the hot-request cases, in tokens: about 9,600 for history
    /// after the response and tool reserves. Sized so that the earlier turns
    /// never compact (about 5,900 tokens at the second), the hot request crosses
    /// the hard limit at its admission (about 11,300) so its outer preflight
    /// summarizes, the summarized view fits (about 7,600, the earlier lines left
    /// only as session-context quotes), and the next turn fits with the request
    /// once (about 8,600), so it compacts nothing.
    const HOT_WINDOW_TOKENS: usize = 12_000;
    /// The provider's refusal that makes the in-turn recovery compact.
    const OVERFLOW_REFUSAL: &str = "This model's maximum context length is 8192 tokens (context_length_exceeded)";

    /// `bytes` bytes of prose opening with `label`.
    fn labelled_prose(label: &str, bytes: usize) -> String {
        let line = format!("{label} lorem ipsum dolor sit amet consectetur adipiscing elit. ");
        let body = line.repeat(bytes / line.len() + 1);
        body[..bytes].trim_end().to_owned()
    }

    /// How the hot request's turn ends after its in-turn compaction.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HotRequestEnd {
        Answered,
        Failed,
        Cancelled,
    }

    /// Numbered summaries; the first call of the hot request is refused as a
    /// context overflow (`overflowed` is set), every later call of it gets
    /// `after_recovery` (`after_recovery_served` counts them); everything else
    /// is answered `NOTED`.
    fn script_hot_request(
        after_recovery: Scripted,
        overflowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        after_recovery_served: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Script {
        let mut summaries = 0usize;
        Box::new(move |_, request| {
            if is_summarizer_request(request) {
                summaries += 1;
                return Scripted::text(&format!(
                    "## Decisions\n{SUMMARY_PREFIX}{summaries}\n## Progress\nearlier lines noted\n## Pending\nnone"
                ));
            }
            if is_memory_flush_request(request) {
                return Scripted::text(compaction_script::NOTED);
            }
            let newest = newest_user_message_text(request);
            // The next turn's request quotes the hot request in its session
            // context; it is routed by the line it ends with.
            if !newest.ends_with(ORDINARY) && newest.contains(HOT_REQUEST_MARKER) {
                if !overflowed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Scripted::Error {
                        status: 400,
                        message: OVERFLOW_REFUSAL.to_owned(),
                    };
                }
                after_recovery_served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return after_recovery.clone();
            }
            Scripted::text(compaction_script::NOTED)
        })
    }

    /// The text of the newest user message of `request`.
    fn newest_user_message_text(request: &serde_json::Value) -> String {
        request["messages"]
            .as_array()
            .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
            .map(|message| message_text(message).trim_end().to_owned())
            .unwrap_or_default()
    }

    /// Each message of `request` as its role and the first and last bytes of
    /// its text, for failure messages.
    fn message_outline(request: &serde_json::Value) -> String {
        const EDGE_CHARS: usize = 48;
        request["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .map(|message| {
                        let text = message_text(message);
                        let chars: Vec<char> = text.chars().collect();
                        let head: String = chars.iter().take(EDGE_CHARS).collect();
                        let tail: String = chars.iter().skip(chars.len().saturating_sub(EDGE_CHARS)).collect();
                        format!("{}[{}]: {head} ... {tail}", message["role"], chars.len())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// A main call of the hot request's turn (not a summary, not the next turn).
    fn is_hot_call(request: &serde_json::Value) -> bool {
        let newest = newest_user_message_text(request);
        !is_summarizer_request(request)
            && !is_memory_flush_request(request)
            && !newest.ends_with(ORDINARY)
            && newest.contains(HOT_REQUEST_MARKER)
    }

    /// The system prompt of the memory flush a compaction writes first.
    const MEMORY_FLUSH_PROMPT: &str = "memory flush notes before context compaction";

    /// Whether `request` is the memory flush call before a compaction.
    fn is_memory_flush_request(request: &serde_json::Value) -> bool {
        request["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message["role"] == "system" && message_text(message).contains(MEMORY_FLUSH_PROMPT))
        })
    }

    /// The user messages of `request` that are the hot request `line` (the
    /// provider form may prefix it with enrichment, never append to it).
    fn hot_request_copies(request: &serde_json::Value, line: &str) -> usize {
        request["messages"].as_array().map_or(0, |messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "user" && message_text(message).trim_end().ends_with(line))
                .count()
        })
    }

    /// PR #207 review r10: the admitted request keeps its pending provenance
    /// across an adopted outer preflight. The request is admitted over the
    /// window, so the preflight summarizes the earlier turns and the saved
    /// session holds the request; the provider then refuses the first call as an
    /// overflow, and the in-turn recovery summarizes again while the request is
    /// still in its hot tail. Through real HTTP/SSE and real SQLite, the request
    /// is saved and resent once after an answer, and not at all after a failure
    /// or a cancellation.
    fn hot_request_through_two_compactions(end: HotRequestEnd) {
        let sandbox = Sandbox::new();
        let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let after_recovery_served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let after_recovery = match end {
            HotRequestEnd::Answered => Scripted::text(HOT_REPLY),
            HotRequestEnd::Failed => Scripted::Error {
                status: 400,
                message: "scripted refusal of this request".to_owned(),
            },
            HotRequestEnd::Cancelled => Scripted::Stall,
        };
        let server = LoopbackOpenAi::start(
            MODEL,
            Some((11, 7)),
            script_hot_request(
                after_recovery,
                std::sync::Arc::clone(&overflowed),
                std::sync::Arc::clone(&after_recovery_served),
            ),
        );
        set_window_and_mode(
            &sandbox,
            &server,
            HOT_WINDOW_TOKENS,
            openprx::config::schema::AgentCompactionMode::Safeguard,
        );
        let mut command = sandbox.command(&["chat", "--plain"]);
        command.env("RUST_LOG", "warn");
        let mut chat = HeldChat {
            sandbox: &sandbox,
            running: Running::spawn(command),
        };
        for index in 0..HOT_PRIOR_TURNS {
            chat.turn(&labelled_prose(&format!("PRIOR-{index}"), HOT_PRIOR_BYTES), index + 1);
        }
        let requests_before_hot = server.requests().len();
        let hot = labelled_prose(HOT_REQUEST_MARKER, HOT_REQUEST_BYTES);
        let mut replies = HOT_PRIOR_TURNS;
        if end == HotRequestEnd::Answered {
            replies += 1;
            chat.turn(&hot, replies);
        } else {
            chat.command(&hot);
            let started = Instant::now();
            while after_recovery_served.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                assert!(
                    started.elapsed() < TURN_DEADLINE,
                    "the hot request's call after the recovery was not sent within TURN_DEADLINE"
                );
                std::thread::sleep(POLL);
            }
        }
        if end == HotRequestEnd::Cancelled {
            chat.interrupt();
            let started = Instant::now();
            while server.stalls_closed_by_client() == 0 {
                assert!(
                    started.elapsed() < CANCEL_DEADLINE,
                    "the cancelled call's connection was not closed within CANCEL_DEADLINE"
                );
                std::thread::sleep(POLL);
            }
        }
        replies += 1;
        chat.command(ORDINARY);
        let started = Instant::now();
        while assistant_reply_count(&sandbox.workspace) < replies {
            if started.elapsed() >= TURN_DEADLINE {
                let output = chat.exit();
                let outlines: Vec<String> = server.requests().iter().map(message_outline).collect();
                panic!(
                    "the turn after the hot request ({end:?}) did not reply within TURN_DEADLINE\nrequests: {outlines:#?}\nstdout: {}\nstderr: {}",
                    output.stdout, output.stderr
                );
            }
            std::thread::sleep(POLL);
        }
        let saved = wait_for_saved_ordinary_turn(&sandbox);
        let output = chat.exit();

        assert!(
            !output.stderr.contains("panicked"),
            "no panic on stderr: {}",
            output.stderr
        );
        assert!(
            !output.stdout.contains("history commit") && !output.stderr.contains("history commit"),
            "no unconfirmed history commit; stdout: {}\nstderr: {}",
            output.stdout,
            output.stderr
        );
        if end != HotRequestEnd::Failed {
            assert_clean(
                &output,
                "a hot request through an outer preflight and an in-turn compaction",
            );
        }
        let refused = refused_patch_lines(&sandbox, &output.stderr);
        assert!(
            refused.is_empty(),
            "no compaction patch was refused or mismatched its guard: {refused:?}"
        );

        // Both compactions ran in the hot request's turn and kept it hot.
        assert!(
            overflowed.load(std::sync::atomic::Ordering::SeqCst),
            "the provider refused the hot request's first call as an overflow"
        );
        let requests = server.requests();
        let next_call = requests
            .iter()
            .position(|request| {
                !is_summarizer_request(request)
                    && !is_memory_flush_request(request)
                    && newest_user_message_text(request).ends_with(ORDINARY)
            })
            .expect("test: the turn after the hot request reached the provider");
        let first_hot_call = requests
            .iter()
            .enumerate()
            .skip(requests_before_hot)
            .find(|(_, request)| is_hot_call(request))
            .map(|(index, _)| index)
            .expect("test: the hot request reached the provider");
        let last_hot_call = requests
            .iter()
            .rposition(is_hot_call)
            .expect("test: the hot request reached the provider");
        // The summaries of the hot request's own turn: its outer preflight and
        // its in-turn recovery, all before its last call.
        let turn_summaries: Vec<usize> = requests
            .iter()
            .enumerate()
            .take(last_hot_call)
            .skip(requests_before_hot)
            .filter(|(_, request)| is_summarizer_request(request))
            .map(|(index, _)| index)
            .collect();
        assert!(
            turn_summaries.iter().any(|index| *index < first_hot_call),
            "the outer preflight summarized before the hot request's first call; summaries at {turn_summaries:?}, first call at {first_hot_call}; turn requests: {:#?}",
            requests[requests_before_hot..]
                .iter()
                .map(message_outline)
                .collect::<Vec<_>>()
        );
        assert!(
            turn_summaries.iter().any(|index| *index > first_hot_call),
            "the in-turn recovery summarized after the refused call; summaries at {turn_summaries:?}, first call at {first_hot_call}"
        );
        for index in &turn_summaries {
            assert!(
                !requests[*index].to_string().contains(HOT_REQUEST_MARKER),
                "summary request {index} folded the hot request; it must stay in the hot tail: {}",
                message_outline(&requests[*index])
            );
        }
        let summaries_through_the_turn = requests[..next_call]
            .iter()
            .filter(|request| is_summarizer_request(request))
            .count();
        let newest_summary = format!("{SUMMARY_PREFIX}{summaries_through_the_turn}");
        assert!(
            saved.iter().any(|(_, content)| content.contains(&newest_summary)),
            "the saved session adopted the in-turn summary `{newest_summary}`: {saved:?}"
        );

        // Request multiplicity in SQLite and in the next provider request.
        let expected_copies = usize::from(end == HotRequestEnd::Answered);
        let saved_copies = saved
            .iter()
            .filter(|(role, content)| role == "user" && content.contains(HOT_REQUEST_MARKER))
            .count();
        assert_eq!(
            saved_copies, expected_copies,
            "the saved session holds the hot request {expected_copies} time(s) ({end:?}): {saved:?}"
        );
        if end == HotRequestEnd::Answered {
            assert_eq!(
                saved
                    .iter()
                    .filter(|(role, content)| role == "assistant" && content.contains(HOT_REPLY))
                    .count(),
                1,
                "the saved session holds the hot request's answer once: {saved:?}"
            );
        }
        let next_request = &requests[next_call];
        assert_eq!(
            hot_request_copies(next_request, &hot),
            expected_copies,
            "the next provider request carries the hot request {expected_copies} time(s) ({end:?}): {}",
            next_request["messages"]
        );
    }

    #[test]
    fn a_hot_request_after_an_adopted_preflight_is_saved_and_resent_once() {
        hot_request_through_two_compactions(HotRequestEnd::Answered);
    }

    #[test]
    fn a_failed_hot_request_after_an_adopted_preflight_leaves_no_copy() {
        hot_request_through_two_compactions(HotRequestEnd::Failed);
    }

    #[test]
    fn a_cancelled_hot_request_after_an_adopted_preflight_leaves_no_copy() {
        hot_request_through_two_compactions(HotRequestEnd::Cancelled);
    }
}

/// The question whose committed answer reads like a native tool-call envelope.
const JSON_QUESTION: &str = "JSON-ANSWER reply with the example envelope";
/// An ordinary final answer that reads like a tool-call envelope: it names no
/// executable tool, so the turn commits it as text.
const JSON_ANSWER: &str = r#"{"tool_calls":[{"id":"example"}]}"#;
/// A tool turn that reads one filler per call until its request carries a
/// handoff (an in-turn compaction reported to the session authority).
const READ_UNTIL_HANDOFF: &str = "READ-UNTIL-HANDOFF read every filler";

/// The compaction script, except that [`JSON_QUESTION`] is answered with
/// [`JSON_ANSWER`] and the [`READ_UNTIL_HANDOFF`] turn reads one filler file
/// per provider call until its request carries a handoff, then ends with
/// `NOTED`.
fn script_json_answer_then_handoff() -> Script {
    let mut base = compaction_script::script();
    let mut rounds = 0;
    Box::new(move |index, request| {
        // The tool turn's request quotes earlier turns in its session-context
        // preamble, so its own marker is checked first.
        let newest = newest_user_text(request);
        if !newest.contains(READ_UNTIL_HANDOFF) {
            if newest.contains(JSON_QUESTION) {
                return Scripted::text(JSON_ANSWER);
            }
            return base(index, request);
        }
        if compaction_script::handoff_id(request).is_some() || rounds >= SWITCH_FILLERS {
            return Scripted::text(compaction_script::NOTED);
        }
        let file = format!("f{rounds}.txt");
        rounds += 1;
        Scripted::tool(
            &format!("call-json-{index}"),
            "file_read",
            &serde_json::json!({ "path": file }),
        )
    })
}

/// What one JSON-answer run left behind.
struct JsonAnswerRun {
    /// The `message.created` event ids of the question and the answer.
    answer_turn_events: Vec<String>,
    /// The saved session once the tool turn committed.
    saved: Vec<(String, String)>,
    /// Every request of the tool turn that carried a handoff.
    handoff_requests: Vec<serde_json::Value>,
    /// Refused or mismatched compaction patch lines (stderr and `chat.log`).
    refused: Vec<String>,
}

/// One held chat over real HTTP/SSE and real SQLite in `switch` mode keeping
/// `keep_recent` messages: the [`JSON_QUESTION`] turn commits
/// [`JSON_ANSWER`], then the [`READ_UNTIL_HANDOFF`] tool turn hands off in
/// the middle of the turn. A TUI build runs the turn on a detached provider
/// worker, whose sink reports the compaction; a non-TUI build runs the inline
/// loop, whose inline sink reports it.
fn run_json_answer_then_handoff(keep_recent: usize) -> JsonAnswerRun {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script_json_answer_then_handoff());
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        config.agent.compaction.mode = openprx::config::schema::AgentCompactionMode::Switch;
        config.agent.compaction.max_context_tokens = compaction_script::SMALL_WINDOW_TOKENS;
        config.agent.compaction.reserve_tokens = RESERVE_TOKENS;
        config.agent.compaction.keep_recent_messages = keep_recent;
        config.memory.auto_save = false;
    });
    let mut command = sandbox.command(&["chat", "--plain"]);
    command.env("RUST_LOG", "warn");
    let mut chat = HeldChat {
        sandbox: &sandbox,
        running: Running::spawn(command),
    };
    chat.turn(JSON_QUESTION, 1);
    let committed = wait_for_saved_reply_to(&sandbox, JSON_QUESTION);
    assert_eq!(
        committed.last().map(|(_, content)| content.as_str()),
        Some(JSON_ANSWER),
        "the envelope-shaped answer was committed as the turn's text: {committed:?}"
    );
    let answer_turn_events = first_message_event_ids(&sandbox, 2);
    chat.turn(READ_UNTIL_HANDOFF, 2);
    let saved = wait_for_saved_reply_to(&sandbox, READ_UNTIL_HANDOFF);
    let output = chat.exit();
    assert_clean(&output, "a JSON-shaped answer, then an in-turn handoff");
    let handoff_requests = server
        .requests()
        .into_iter()
        .filter(|request| {
            newest_user_text(request).contains(READ_UNTIL_HANDOFF) && compaction_script::handoff_id(request).is_some()
        })
        .collect();
    JsonAnswerRun {
        answer_turn_events,
        saved,
        handoff_requests,
        refused: refused_patch_lines(&sandbox, &output.stderr),
    }
}

/// Messages kept verbatim when the answer must sit in the hot tail. Even, so
/// the first handoff the tool turn can make (the history outgrows it by one
/// message: the question) leaves exactly the question in the cold window.
const JSON_ANSWER_HOT_KEEP: usize = 16;
/// Messages kept verbatim when the answer must fall into the cold window: the
/// first possible handoff leaves the question and the answer cold.
const JSON_ANSWER_COLD_KEEP: usize = 15;

/// A committed answer that reads like a tool-call envelope sits in the hot
/// tail an in-turn handoff retains: the session accepts the compaction (no
/// refused patch, no guard mismatch), the handoff references only the
/// question (the answer is retained, not handed off), and the saved session
/// keeps the answer as bytes.
#[test]
fn a_committed_json_answer_in_the_hot_tail_survives_an_in_turn_handoff() {
    let run = run_json_answer_then_handoff(JSON_ANSWER_HOT_KEEP);

    assert!(
        run.refused.is_empty(),
        "no compaction patch was refused or mismatched its guard: {:?}",
        run.refused
    );
    assert!(
        !run.handoff_requests.is_empty(),
        "the tool turn handed off in the middle of the turn; saved: {:?}",
        run.saved
    );
    let saved_handoff_refs = saved_handoff_references(&run.saved);
    assert_eq!(
        run.answer_turn_events.len(),
        2,
        "the question and the answer were stored: {:?}",
        run.answer_turn_events
    );
    assert_eq!(
        saved_handoff_refs,
        run.answer_turn_events[..1].to_vec(),
        "the saved handoff references the question only: the answer sits in the retained hot tail; saved: {:?}",
        run.saved
    );
    assert!(
        run.saved
            .iter()
            .any(|(role, content)| role == "assistant" && content == JSON_ANSWER),
        "the saved session keeps the answer as bytes: {:?}",
        run.saved
    );
}

/// A committed answer that reads like a tool-call envelope falls into the
/// cold window of an in-turn handoff: the session accepts the compaction, and
/// the handoff resolves the answer to its own stored event (an acknowledged
/// turn is never folded as tool traffic), so the saved session keeps it
/// through the handoff's references.
#[test]
fn a_committed_json_answer_in_the_cold_window_is_referenced_by_the_handoff() {
    let run = run_json_answer_then_handoff(JSON_ANSWER_COLD_KEEP);

    assert!(
        run.refused.is_empty(),
        "no compaction patch was refused or mismatched its guard: {:?}",
        run.refused
    );
    assert!(
        !run.handoff_requests.is_empty(),
        "the tool turn handed off in the middle of the turn; saved: {:?}",
        run.saved
    );
    assert_eq!(
        run.answer_turn_events.len(),
        2,
        "the question and the answer were stored: {:?}",
        run.answer_turn_events
    );
    let saved_handoff_refs = saved_handoff_references(&run.saved);
    assert!(
        run.answer_turn_events
            .iter()
            .all(|event| saved_handoff_refs.contains(event)),
        "the saved handoff references the question and the answer: {:?} in {saved_handoff_refs:?}; saved: {:?}",
        run.answer_turn_events,
        run.saved
    );
}

/// The system-prompt paragraph a chat session states about handoffs (M1).
const HANDOFF_RULE: &str = "A message that starts with [context_handoff] means earlier turns of this session were moved out of this window; they still exist. Never treat the oldest visible message as the start of the session. Before answering a question about anything that may have happened earlier than the visible window, call transcript_history_lookup with that handoff's handoff_event_id and read the result; if the result contains another [context_handoff], look that one up too.";

/// The compact rendering of the stored `note`, as the provider must see it.
fn expected_compact_handoff(note: &serde_json::Value) -> String {
    let id = note["handoff_event_id"].as_str().expect("test: handoff id");
    let events = note["lookup"]["arguments"]["event_ids"].as_array().map_or(0, Vec::len);
    let folded = note["folded_tool_roundtrips"]["count"].as_u64().unwrap_or(0);
    let mut text = format!(
        "[context_handoff] handoff_event_id={id} — {events} earlier messages were moved out of this window. \
         They are recoverable with transcript_history_lookup(handoff_event_id=\"{id}\")."
    );
    if folded > 0 {
        text = format!("{text} {folded} tool round-trips were folded.");
    }
    text
}

/// A stored message as a provider view replays it: a stored handoff note in
/// its compact rendering, every other text unchanged.
fn as_provider_sees(stored: &str) -> String {
    stored
        .strip_prefix(compaction_script::HANDOFF_START)
        .and_then(|rest| rest.trim().strip_suffix(compaction_script::HANDOFF_END))
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json.trim()).ok())
        .map_or_else(|| stored.to_owned(), |note| expected_compact_handoff(&note))
}

/// After an automatic switch, the next request carries the handoff only in
/// its compact form (never the stored note), the handoff rule once in the
/// system prompt, and the lookup tool; the store keeps the stored note.
#[test]
fn request_after_a_switch_carries_the_compact_handoff_and_the_rule() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = configure(&sandbox, RECALL_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    let mut replies = 1;
    chat.turn(PLANT, replies);
    let mut next_file = 0;
    while compaction_script::stored_handoffs(&sandbox.workspace).is_empty() && next_file < SWITCH_FILLERS {
        replies += 1;
        chat.turn(&read_line(next_file), replies);
        next_file += 1;
    }
    replies += 1;
    chat.turn(ORDINARY, replies);
    let output = chat.exit();
    assert_clean(&output, "automatic switch then an ordinary turn");

    let stored = compaction_script::stored_handoffs(&sandbox.workspace);
    let [handoff] = stored.as_slice() else {
        panic!("exactly one automatic switch; stdout: {}", output.stdout);
    };
    let expected = expected_compact_handoff(&handoff.note);
    let ordinary = server
        .requests()
        .into_iter()
        .find(|request| newest_user_text(request).contains(ORDINARY))
        .expect("the ordinary turn reached the provider");
    assert_eq!(
        compaction_script::compact_handoff(&ordinary).as_deref(),
        Some(expected.as_str()),
        "the request carries the compact handoff, byte for byte"
    );
    let bytes = ordinary.to_string();
    assert!(
        !bytes.contains("[/context_handoff]") && !bytes.contains("\\\"source_event_range\\\""),
        "the stored note never reaches the provider: {bytes}"
    );
    let system = ordinary["messages"][0]["content"].as_str().unwrap_or_default();
    assert_eq!(
        system.matches(HANDOFF_RULE).count(),
        1,
        "the rule is stated once: {system}"
    );
    assert!(
        compaction_script::publishes(&ordinary, compaction_script::LOOKUP_TOOL),
        "the lookup tool is published"
    );
    assert_eq!(
        handoff.note["handoff_event_id"],
        handoff.event_id.as_str(),
        "the store keeps the stored note"
    );
}

/// A fact behind two handoffs is recovered in one turn by two lookups: the
/// newest handoff's id, then the id of the handoff its result contains.
#[test]
fn nested_handoff_is_recoverable_in_two_lookups() {
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
    replies += 1;
    chat.turn(RECALL, replies);
    let output = chat.exit();
    assert_clean(&output, "two handoffs then a recall");

    let session_id = current_session_id(&output.stderr);
    let chain = compaction_script::session_handoffs(&sandbox.workspace, &session_id);
    compaction_script::assert_handoff_chain(&chain, 2);
    assert_eq!(chain.len(), 2, "exactly two handoffs; stdout: {}", output.stdout);
    let last = server
        .requests()
        .into_iter()
        .rev()
        .find(|request| newest_user_text(request).contains(RECALL))
        .expect("the recall turn reached the provider");
    let messages = last["messages"].as_array().expect("test: messages");
    let recall_at = messages
        .iter()
        .rposition(|message| message["role"] == "user" && message.to_string().contains(RECALL))
        .expect("test: the recall line");
    let looked_up: Vec<String> = messages[recall_at..]
        .iter()
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
        .filter(|call| call["function"]["name"] == compaction_script::LOOKUP_TOOL)
        .filter_map(|call| {
            let arguments: serde_json::Value = serde_json::from_str(call["function"]["arguments"].as_str()?).ok()?;
            arguments["handoff_event_id"].as_str().map(str::to_owned)
        })
        .collect();
    assert_eq!(
        looked_up,
        vec![chain[1].event_id.clone(), chain[0].event_id.clone()],
        "the newest handoff, then its parent"
    );
    assert!(output.stdout.contains(RECALLED), "stdout: {}", output.stdout);
    assert_lookup_returned_the_stored_row(&sandbox, &server);
}

/// The line of the long tool turn that runs after an automatic switch.
const LONG_READS: &str = "LONG-READS every filler";
/// Tool rounds of the [`LONG_READS`] turn before its text reply: one per
/// filler file, enough tool output to overflow the small window inside the
/// turn several times.
const LONG_READ_ROUNDS: usize = SWITCH_FILLERS;

/// The compaction script, except that the [`LONG_READS`] turn reads one
/// filler file per provider call for [`LONG_READ_ROUNDS`] calls.
fn script_with_long_reads() -> Script {
    let mut base = compaction_script::script();
    let mut rounds = 0;
    Box::new(move |index, request| {
        if newest_user_text(request).contains(LONG_READS) {
            if rounds < LONG_READ_ROUNDS {
                let file = format!("f{rounds}.txt");
                rounds += 1;
                return Scripted::tool(
                    &format!("call-long-{index}"),
                    "file_read",
                    &serde_json::json!({ "path": file }),
                );
            }
            return Scripted::text(compaction_script::NOTED);
        }
        base(index, request)
    })
}

/// The handoff message of `request` as the model sees it (the newest
/// assistant message that opens with the handoff marker), if any.
fn shown_handoff(request: &serde_json::Value) -> Option<String> {
    request["messages"].as_array()?.iter().rev().find_map(|message| {
        let text = message_text(message);
        (message["role"] == "assistant" && text.starts_with(compaction_script::HANDOFF_START)).then_some(text)
    })
}

/// The non-system `(role, text)` messages of `request`.
fn conversation_messages(request: &serde_json::Value) -> Vec<(String, String)> {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] != "system")
                .map(|message| {
                    (
                        message["role"].as_str().unwrap_or_default().to_owned(),
                        message_text(message),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// M2 (REQ-HANDOFF-RECALL-AND-TRIM): tool turns drive one real `prx chat
/// --plain` across the small window until an automatic switch; a long tool
/// turn then overflows the window again and again. Every per-request trim of
/// that turn takes older messages and keeps the handoff, so each of its
/// requests still shows the model the pointer to the folded turns, in the
/// compact form a chat view renders it in.
#[test]
fn a_long_turn_after_a_switch_keeps_the_handoff_in_every_request() {
    let sandbox = Sandbox::new();
    write_filler_files(&sandbox.workspace, SWITCH_FILLERS);
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script_with_long_reads());
    set_window(&sandbox, &server, compaction_script::SMALL_WINDOW_TOKENS);
    let mut chat = HeldChat::spawn(&sandbox, &[]);
    let mut replies = 1;
    chat.turn(PLANT, replies);
    let mut next_file = 0;
    while switch_event_counts(&sandbox.workspace).0 < 1 && next_file < SWITCH_FILLERS {
        replies += 1;
        chat.turn(&read_line(next_file), replies);
        next_file += 1;
    }
    assert!(
        switch_event_counts(&sandbox.workspace).0 >= 1,
        "premise: the filler reads drive the session across the window and switch"
    );
    replies += 1;
    chat.turn(LONG_READS, replies);
    let output = chat.exit();
    assert_clean(&output, "a long tool turn after an automatic switch");

    let requests = server.requests();
    let long_turn: Vec<&serde_json::Value> = requests
        .iter()
        .filter(|request| newest_user_text(request).contains(LONG_READS))
        .collect();
    assert!(
        long_turn.len() > LONG_READ_ROUNDS,
        "premise: the long turn ran its {LONG_READ_ROUNDS} tool rounds: {} provider calls",
        long_turn.len()
    );
    let trimmed_under_one_handoff = long_turn.windows(2).any(|pair| {
        let (earlier, later) = (pair[0], pair[1]);
        let later_messages = conversation_messages(later);
        shown_handoff(earlier).is_some()
            && shown_handoff(earlier) == shown_handoff(later)
            && conversation_messages(earlier)
                .iter()
                .any(|message| !later_messages.contains(message))
    });
    assert!(
        trimmed_under_one_handoff,
        "premise: a per-request trim (not a switch) dropped messages inside the long turn"
    );
    let compact_prefix = format!("{} handoff_event_id=", compaction_script::HANDOFF_START);
    for (index, request) in long_turn.iter().enumerate() {
        assert!(
            shown_handoff(request).is_some_and(|handoff| handoff.starts_with(&compact_prefix)),
            "request {index} of the long turn carries the compact [context_handoff]: {:?}",
            conversation_messages(request)
                .iter()
                .map(|(role, text)| format!("{role}: {}", text.chars().take(60).collect::<String>()))
                .collect::<Vec<_>>()
        );
    }
}

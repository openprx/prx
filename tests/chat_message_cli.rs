//! `prx chat` turns over a loopback OpenAI-compatible provider (real
//! HTTP/SSE bytes) with a real config dir and SQLite store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use loopback_openai::{LoopbackOpenAi, Script, Scripted, message_events, write_config};
use plain_chat::{Output, Running, Sandbox, run_plain};

const MODEL: &str = "prx-message-model";
/// Prompt prefixes of the deleted assessment and completion review.
const FORMER_PROMPTS: [&str; 2] = ["Classify the execution", "Review the proposed final"];
/// Prefixes of the event types the deleted assessment and review recorded.
const REMOVED_EVENT_PREFIXES: [&str; 2] = ["turn.execution_", "turn.completion_"];
const PLANTED: &str = "PRX-FALCON-7";

/// A Full-autonomy config against `script`.
fn serve(sandbox: &Sandbox, script: Script) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((5, 3)), script);
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.workspace_dir.clone_from(&sandbox.workspace);
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
        // Recall comes from the session history alone, never from autosaved memories.
        config.memory.auto_save = false;
    });
    server
}

/// `prx chat <args…>` with stdin closed at once, bounded.
fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    Running::spawn(sandbox.command(args)).wait()
}

/// `(id, turns)` of every session `prx chat --list-sessions` prints, newest first.
fn listed(sandbox: &Sandbox) -> Vec<(String, String)> {
    let output = run(sandbox, &["chat", "--list-sessions"]);
    assert!(output.status.success(), "list failed; stderr: {}", output.stderr);
    output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("  ")?.split(" | ");
            let id = fields.next()?.to_owned();
            let turns = fields.nth(1)?.to_owned();
            Some((id, turns))
        })
        .collect()
}

/// Session ids printed by `prx chat --list-sessions`, newest first.
fn listed_sessions(sandbox: &Sandbox) -> Vec<String> {
    listed(sandbox).into_iter().map(|(id, _)| id).collect()
}

/// The text of every message of `request`, joined.
fn request_text(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The last user message of `request`.
fn last_user_message(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "user"))
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Recalls [`PLANTED`] only when the conversation sent to the model holds it.
fn recall_script() -> Script {
    Box::new(|_, request| {
        let text = request_text(request);
        if last_user_message(request).contains("What is the codename") {
            if text.contains(PLANTED) {
                Scripted::text(&format!("CODENAME {PLANTED}"))
            } else {
                Scripted::text("CODENAME UNKNOWN")
            }
        } else {
            Scripted::text("NOTED")
        }
    })
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

/// CF-R6: a plain session resumed with `--session <id>` (real stdin) publishes
/// `sessions_spawn` directly on its first request.
#[test]
fn resumed_plain_turn_publishes_sessions_spawn() {
    let sandbox = Sandbox::new();
    let server = LoopbackOpenAi::start(MODEL, Some((5, 3)), Box::new(|_, _| Scripted::text("PRX-RESUME-OK")));
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
    });

    let first = run_plain(&sandbox, &["Say hello."]);
    assert!(first.status.success(), "first chat failed; stderr: {}", first.stderr);
    let session = current_session_id(&first.stderr);
    let before = server.requests().len();
    assert!(before >= 1, "the first chat reached the provider");

    let mut resumed = plain_chat::Running::spawn(sandbox.command(&["chat", "--plain", "--session", &session]));
    resumed.send("Say hello again.");
    resumed.send("/exit");
    let output = resumed.wait();
    assert!(
        output.status.success(),
        "resumed chat failed; stderr: {}",
        output.stderr
    );
    assert!(
        output.stdout.contains("PRX-RESUME-OK"),
        "resumed reply missing: {}",
        output.stdout
    );
    assert_eq!(
        current_session_id(&output.stderr),
        session,
        "the resumed chat keeps its session"
    );

    let requests = server.requests();
    let resumed_first = requests.get(before).expect("test: resumed chat request");
    assert!(
        compaction_script::publishes(resumed_first, "sessions_spawn"),
        "the resumed turn publishes sessions_spawn directly: {:?}",
        resumed_first["tools"]
    );
}

/// AR-R2: stdout is the reply plus a newline, stderr is empty, exit 0.
#[test]
fn reply_only_exit_0() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox, Box::new(|_, _| Scripted::text("pong")));
    let output = run(&sandbox, &["chat", "--message", "Reply with pong"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "pong\n", "stdout is the reply alone");
    assert!(output.stderr.is_empty(), "stderr: {}", output.stderr);
    assert_eq!(server.requests().len(), 1, "one main request");
}

/// AR-R2: an unsaved one-shot leaves no session row, but its memory events
/// are written as for any turn.
#[test]
fn not_saved_by_default() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, Box::new(|_, _| Scripted::text("UNSAVED-OK")));
    let output = run(&sandbox, &["chat", "-q", "Say something."]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "UNSAVED-OK\n");
    assert!(
        listed_sessions(&sandbox).is_empty(),
        "the one-shot is not a saved session"
    );
    let events = message_events(&sandbox.workspace);
    assert!(
        events
            .iter()
            .any(|event| event.role == "user" && event.content.contains("Say something.")),
        "the user message event is recorded"
    );
    assert!(
        events
            .iter()
            .any(|event| event.role == "assistant" && event.content == "UNSAVED-OK"),
        "the reply event is recorded"
    );
}

/// AR-R1: `--save` makes the turn a resumable session that `--plain` recalls.
#[test]
fn save_flag_resumable() {
    let sandbox = Sandbox::new();
    let _server = serve(&sandbox, recall_script());
    let output = run(
        &sandbox,
        &[
            "chat",
            "--message",
            &format!("Remember the codename {PLANTED}."),
            "--save",
        ],
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "NOTED\n");
    let sessions = listed_sessions(&sandbox);
    assert_eq!(sessions.len(), 1, "one saved session: {sessions:?}");

    let resumed = run_plain_in(&sandbox, &sessions[0], "What is the codename?");
    assert!(resumed.status.success(), "stderr: {}", resumed.stderr);
    assert!(
        resumed.stdout.contains(&format!("CODENAME {PLANTED}")),
        "the resumed session recalls the planted fact: {}",
        resumed.stdout
    );
}

/// `chat --plain --session <id>` fed one line, then `/exit`.
fn run_plain_in(sandbox: &Sandbox, session: &str, line: &str) -> Output {
    let mut chat = Running::spawn(sandbox.command(&["chat", "--plain", "--session", session]));
    chat.send(line);
    chat.send("/exit");
    chat.wait()
}

/// AR-R1: `--workdir` applies to the one-shot's shell tool.
#[test]
fn workdir_applies() {
    let sandbox = Sandbox::new();
    let workdir = sandbox.root().join("project");
    std::fs::create_dir_all(&workdir).expect("test: workdir");
    let workdir = workdir.canonicalize().expect("test: canonical workdir");
    let _server = serve(
        &sandbox,
        Box::new(|index, request| {
            if index == 0 {
                return Scripted::tool("call-pwd", "shell", &serde_json::json!({"command": "pwd"}));
            }
            let tool = request["messages"]
                .as_array()
                .and_then(|messages| messages.iter().rev().find(|message| message["role"] == "tool"))
                .and_then(|message| message["content"].as_str())
                .unwrap_or("no tool result")
                .to_owned();
            Scripted::text(&format!("PWD-RESULT {tool}"))
        }),
    );
    let workdir_arg = workdir.to_str().expect("test: UTF-8 workdir");
    let output = run(
        &sandbox,
        &["chat", "--workdir", workdir_arg, "--message", "Run shell pwd"],
    );
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert!(output.stdout.starts_with("PWD-RESULT "), "stdout: {}", output.stdout);
    assert!(
        output.stdout.contains(workdir_arg),
        "the shell ran in the --workdir: {}",
        output.stdout
    );
}

/// D2: an unknown `--session` exits 1 with no request and no session row.
#[test]
fn unknown_session_exit_1() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox, Box::new(|_, _| Scripted::text("MUST-NOT-RUN")));
    let missing = "00000000-0000-4000-8000-000000000000";
    let output = run(&sandbox, &["chat", "--message", "hello", "--session", missing]);
    assert_eq!(output.status.code(), Some(1), "stdout: {}", output.stdout);
    assert!(output.stdout.is_empty(), "stdout: {}", output.stdout);
    assert!(
        output.stderr.contains(&format!("Error: session '{missing}' not found")),
        "stderr: {}",
        output.stderr
    );
    assert!(server.requests().is_empty(), "no provider request");
    assert!(listed_sessions(&sandbox).is_empty(), "no session row");
}

/// D3: `--continue` runs in and saves to the latest session; with none it
/// exits 1 before any request.
#[test]
fn continue_runs_in_latest() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox, recall_script());
    let none = run(&sandbox, &["chat", "--continue", "--message", "hello"]);
    assert_eq!(none.status.code(), Some(1), "stdout: {}", none.stdout);
    assert!(
        none.stderr.contains("Error: no saved chat session to continue"),
        "{}",
        none.stderr
    );
    assert!(server.requests().is_empty(), "no request without a session");

    let first = run_plain(&sandbox, &[&format!("Remember the codename {PLANTED}.")]);
    assert!(first.status.success(), "stderr: {}", first.stderr);
    let sessions = listed_sessions(&sandbox);
    assert_eq!(sessions.len(), 1, "{sessions:?}");

    let before = listed(&sandbox);
    let output = run(&sandbox, &["chat", "--continue", "-q", "What is the codename?"]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(
        output.stdout,
        format!("CODENAME {PLANTED}\n"),
        "ran in the latest session"
    );
    let after = listed(&sandbox);
    assert_eq!(after.len(), 1, "no new session: {after:?}");
    assert_eq!(after[0].0, sessions[0], "the turn joined the latest session");
    assert_ne!(
        after[0].1, before[0].1,
        "the --continue turn was saved: {before:?} -> {after:?}"
    );
    assert!(server.requests().len() >= 2, "both turns reached the provider");
}

/// D4: slash text is literal model input.
#[test]
fn slash_text_is_literal() {
    let sandbox = Sandbox::new();
    let server = serve(&sandbox, Box::new(|_, _| Scripted::text("SLASH-OK")));
    let output = run(&sandbox, &["chat", "--message", "/compact"]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "SLASH-OK\n");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "the slash text went to the model");
    assert!(
        last_user_message(&requests[0]).ends_with("/compact"),
        "the user text is the literal slash text: {}",
        requests[0]
    );
}

/// CF-R1 (plain turn path): a tool turn sends exactly its two main requests.
#[test]
fn tool_turn_sends_no_auxiliary_requests() {
    let sandbox = Sandbox::new();
    let server = serve(
        &sandbox,
        Box::new(|index, _| match index {
            0 => Scripted::tool(
                "call-write",
                "file_write",
                &serde_json::json!({"path": "x.txt", "content": "hello"}),
            ),
            _ => Scripted::text("WROTE-DONE"),
        }),
    );
    let output = run(&sandbox, &["chat", "--message", "write x.txt with hello"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "WROTE-DONE\n", "stdout is the reply alone");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the tool call and the reply only");
    for request in &requests {
        assert!(
            request["tools"].as_array().is_some_and(|tools| !tools.is_empty()),
            "every request is a main request carrying tools: {request}"
        );
        let body = request.to_string();
        for prompt in FORMER_PROMPTS {
            assert!(!body.contains(prompt), "auxiliary prompt on the wire: {body}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(sandbox.workspace.join("x.txt")).expect("test: written file"),
        "hello"
    );
    for event in message_events(&sandbox.workspace) {
        assert!(
            !REMOVED_EVENT_PREFIXES
                .iter()
                .any(|prefix| event.event_type.starts_with(prefix)),
            "removed event {}",
            event.event_type
        );
    }
}

/// CF-R8 through `--message`: one subscription row with the exact wire tokens.
#[test]
fn kimi_code_subscription_usage_is_reported() {
    const USAGE: (u64, u64) = (12, 7);
    let sandbox = Sandbox::new();
    let server = LoopbackOpenAi::start(
        "kimi-for-coding",
        Some(USAGE),
        Box::new(|_, _| Scripted::text("PRX-SUBSCRIPTION-OK")),
    );
    write_config(&sandbox.config_dir, "kimi-code", "kimi-for-coding", |config| {
        config.api_url = Some(server.base_url.clone());
        config.cost.prices.clear();
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
    });
    let output = run(&sandbox, &["chat", "--message", "One subscription turn."]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(output.stdout, "PRX-SUBSCRIPTION-OK\n");

    let cost = run(&sandbox, &["cost", "--json"]);
    assert!(cost.status.success(), "prx cost failed; stderr: {}", cost.stderr);
    let report: serde_json::Value = serde_json::from_str(&cost.stdout).expect("test: cost JSON");
    let all_time = &report["all_time"];
    assert_eq!(all_time["requests"], 1, "{report}");
    assert_eq!(all_time["total_tokens"], USAGE.0 + USAGE.1, "{report}");
    assert_eq!(all_time["subscription_requests"], 1, "{report}");
    assert_eq!(all_time["cost_usd"].as_f64(), Some(0.0), "{report}");
}

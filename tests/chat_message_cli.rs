//! `prx chat` turns over a loopback OpenAI-compatible provider (real
//! HTTP/SSE bytes) with a real config dir and SQLite store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use loopback_openai::{LoopbackOpenAi, Scripted, write_config};
use plain_chat::{Sandbox, run_plain};

const MODEL: &str = "prx-message-model";

/// The id from the plain front end's `current chat session: <id>` line.
fn current_session_id(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("current chat session: "))
        .map(|id| id.trim().to_owned())
        .unwrap_or_else(|| panic!("no session id line in stderr: {stderr}"))
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

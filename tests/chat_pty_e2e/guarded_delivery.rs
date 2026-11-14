//! Fullscreen chat over a loopback OpenAI-compatible provider (real HTTP/SSE):
//! a tool turn sends only main requests, a guard-rejected draft never renders,
//! and middleware-transformed output renders once and equals the persisted
//! assistant event.

use super::{
    EXIT_TIMEOUT, HarnessGuard, STARTUP_TIMEOUT, SessionGuard, TURN_TIMEOUT, build_provider_chat_command_in,
    new_harness_guard, prepare_owned_chat_fixture, read_until_with_dsr, spawn_chat_command, wait_for_exit,
};
use crate::loopback_openai::{LoopbackOpenAi, Script, Scripted, assistant_messages, message_events};

const MODEL: &str = "prx-loop-model";
/// Leading words of the deleted auxiliary requests' system texts.
const FORMER_PROMPTS: [&str; 2] = ["Classify the execution", "Review the proposed final"];
/// Prefixes of the event types the deleted assessment and review recorded.
const REMOVED_EVENT_PREFIXES: [&str; 2] = ["turn.execution_", "turn.completion_"];

/// Start the loopback provider and a fullscreen chat against it.
fn start(guard: &HarnessGuard, script: Script) -> (LoopbackOpenAi, SessionGuard) {
    start_with(guard, script, |_| {})
}

/// [`start`], running `prepare` after the config is written and before chat starts.
fn start_with(
    guard: &HarnessGuard,
    script: Script,
    prepare: impl FnOnce(&HarnessGuard),
) -> (LoopbackOpenAi, SessionGuard) {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    let provider = server.provider();
    crate::loopback_openai::write_config(&guard.config_dir, &provider, MODEL, |config| {
        config.workspace_dir = workspace(guard);
        config.plugins.enabled = true;
    });
    prepare_owned_chat_fixture(guard, &provider, MODEL);
    prepare(guard);
    let command = build_provider_chat_command_in(guard, &provider, MODEL, &[], &[("PRX_TUI", "1")]);
    let session = SessionGuard::new(spawn_chat_command(command));
    (server, session)
}

/// The workspace chat resolves under the harness config dir.
fn workspace(guard: &HarnessGuard) -> std::path::PathBuf {
    guard.config_dir.join("workspace")
}

/// The capture as screen text: a cursor-position sequence (`ESC [ r ; c H`),
/// which the fullscreen renderer uses to skip unchanged blank cells, becomes
/// one space; every other control sequence is dropped.
fn screen_text(captured: &str) -> String {
    let mut out = String::with_capacity(captured.len());
    let mut chars = captured.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() != Some(&'[') {
            chars.next();
            continue;
        }
        chars.next();
        let mut last = None;
        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() || next == '~' {
                last = Some(next);
                break;
            }
        }
        if last == Some('H') {
            out.push(' ');
        }
    }
    out
}

fn occurrences(captured: &str, needle: &str) -> usize {
    screen_text(captured).matches(needle).count()
}

/// Send `first`, wait for `first_reply`, then send a second line and wait for
/// `SECOND-DONE`, returning both captures joined (so text rendered after the
/// first reply is inspected too).
fn two_turns(session: &mut SessionGuard, first: &str, first_reply: &str) -> String {
    let session = session.session();
    read_until_with_dsr(session, MODEL, STARTUP_TIMEOUT);
    session.send(format!("{first}\r")).expect("send first turn");
    let mut captured = read_until_with_dsr(session, first_reply, TURN_TIMEOUT);
    session.send("second turn\r").expect("send second turn");
    captured.push_str(&read_until_with_dsr(session, "SECOND-DONE", TURN_TIMEOUT));
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat should exit cleanly");
    captured
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn tool_turn_sends_no_auxiliary_requests() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(
        &guard,
        Box::new(|index, _| match index {
            0 => Scripted::tool(
                "call-write",
                "file_write",
                &serde_json::json!({"path": "x.txt", "content": "hello"}),
            ),
            1 => Scripted::text("WROTE-DONE"),
            _ => Scripted::text("SECOND-DONE"),
        }),
    );
    let captured = two_turns(&mut session, "write x.txt with hello", "WROTE-DONE");
    drop(session);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "two main requests for the tool turn plus the second turn"
    );
    for request in requests.iter().take(2) {
        assert!(
            request["tools"].as_array().is_some_and(|tools| !tools.is_empty()),
            "every tool-turn request is a main request carrying tools: {request}"
        );
    }
    for request in &requests {
        let body = request.to_string();
        for prompt in FORMER_PROMPTS {
            assert!(!body.contains(prompt), "auxiliary prompt on the wire: {body}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(workspace(&guard).join("x.txt")).expect("test: written file"),
        "hello"
    );
    assert_eq!(
        occurrences(&captured, "WROTE-DONE"),
        1,
        "the reply renders once: {captured}"
    );
    let events = message_events(&workspace(&guard));
    for event in &events {
        assert!(
            !REMOVED_EVENT_PREFIXES
                .iter()
                .any(|prefix| event.event_type.starts_with(prefix)),
            "removed event {}",
            event.event_type
        );
    }
}

#[test]
#[serial_test::serial(prx_chat_pty)]
fn guarded_delivery_rejected_draft_never_renders() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (server, mut session) = start(
        &guard,
        Box::new(|index, _| match index {
            0 => Scripted::Text(vec![
                "[Used tools: shell]\n".to_owned(),
                "DRAFT-".to_owned(),
                "MUST-NOT-RENDER".to_owned(),
            ]),
            1 => Scripted::text("FINAL-OK"),
            _ => Scripted::text("SECOND-DONE"),
        }),
    );
    let captured = two_turns(&mut session, "say something", "FINAL-OK");
    drop(session);

    assert_eq!(server.requests().len(), 3, "draft, guard retry, second turn");
    for fragment in ["[Used tools", "DRAFT-", "MUST-NOT-RENDER"] {
        assert!(
            !screen_text(&captured).contains(fragment),
            "rejected draft rendered `{fragment}`: {captured}"
        );
    }
    assert_eq!(
        occurrences(&captured, "FINAL-OK"),
        1,
        "accepted reply renders once: {captured}"
    );
}

#[cfg(feature = "wasm-plugins")]
#[test]
#[serial_test::serial(prx_chat_pty)]
fn guarded_delivery_middleware_output_renders_once() {
    let guard = new_harness_guard().expect("isolated chat paths");
    let (_server, mut session) = start_with(
        &guard,
        Box::new(|index, _| {
            if index == 0 {
                Scripted::Text(vec!["plain ".to_owned(), "response".to_owned()])
            } else {
                Scripted::text("SECOND-DONE")
            }
        }),
        install_middleware,
    );
    let captured = two_turns(&mut session, "hello", "[wasm-middleware]");
    drop(session);

    assert_eq!(
        occurrences(&captured, "[wasm-middleware] plain response"),
        1,
        "transformed reply renders once: {captured}"
    );
    assert_eq!(
        occurrences(&captured, "plain response"),
        1,
        "the untransformed text never renders: {captured}"
    );
    let assistant = assistant_messages(&workspace(&guard));
    assert_eq!(
        assistant.first().map(String::as_str),
        Some("[wasm-middleware] plain response"),
        "the persisted reply equals the rendered text"
    );
}

/// Install and trust the committed middleware fixture through the real CLI.
#[cfg(feature = "wasm-plugins")]
fn install_middleware(guard: &HarnessGuard) {
    let plugin_dir = workspace(guard).join("plugins/pipeline-middleware");
    std::fs::create_dir_all(&plugin_dir).expect("test: plugin dir");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
    for file in ["plugin.toml", "plugin.wasm"] {
        std::fs::copy(fixture.join(file), plugin_dir.join(file)).expect("test: copy plugin fixture");
    }
    let source = plugin_dir.to_str().expect("test: UTF-8 plugin path");
    let run = |args: &[&str]| {
        let output = std::process::Command::new(super::prx_binary())
            .arg("--config-dir")
            .arg(&guard.config_dir)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &guard.home_dir)
            .env("OPENPRX_WORKSPACE", workspace(guard))
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("test: run prx plugin");
        assert!(
            output.status.success(),
            "prx {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let inspected = run(&["plugin", "inspect", source]);
    let start = inspected.find('{').expect("test: inspect JSON");
    let identity: serde_json::Value = serde_json::from_str(inspected[start..].trim()).expect("test: inspect JSON");
    let sha = identity["sha256"].as_str().expect("test: wasm sha256").to_owned();
    let manifest = identity["manifest_sha256"]
        .as_str()
        .expect("test: manifest sha256")
        .to_owned();
    run(&[
        "plugin",
        "trust",
        source,
        "--sha256",
        &sha,
        "--manifest-sha256",
        &manifest,
        "--yes",
    ]);
}

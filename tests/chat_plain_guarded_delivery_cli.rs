//! `prx chat --plain` over a loopback OpenAI-compatible provider (real HTTP/SSE):
//! a guard-rejected draft never reaches stdout, and middleware-transformed
//! output is printed once and equals the persisted assistant event.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use loopback_openai::{LoopbackOpenAi, Script, Scripted, assistant_messages, write_config};
use plain_chat::{Sandbox, run_plain};

const MODEL: &str = "prx-loop-model";

fn configure(sandbox: &Sandbox, script: Script) -> LoopbackOpenAi {
    let server = LoopbackOpenAi::start(MODEL, Some((11, 7)), script);
    write_config(&sandbox.config_dir, &server.provider(), MODEL, |config| {
        config.plugins.enabled = true;
    });
    server
}

#[test]
fn rejected_draft_never_renders() {
    let sandbox = Sandbox::new();
    let server = configure(
        &sandbox,
        Box::new(|index, _| {
            if index == 0 {
                Scripted::Text(vec![
                    "[Used tools: shell]\n".to_owned(),
                    "DRAFT-".to_owned(),
                    "MUST-NOT-RENDER".to_owned(),
                ])
            } else {
                Scripted::text("FINAL-OK")
            }
        }),
    );
    let output = run_plain(&sandbox, &["say something"]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    assert_eq!(server.requests().len(), 2, "draft plus the guard's retry");
    let printed = format!("{}{}", output.stdout, output.stderr);
    for fragment in ["[Used tools", "DRAFT-", "MUST-NOT-RENDER"] {
        assert!(
            !printed.contains(fragment),
            "rejected draft printed `{fragment}`: {printed}"
        );
    }
    assert_eq!(
        output.stdout.matches("FINAL-OK").count(),
        1,
        "stdout: {}",
        output.stdout
    );
}

#[cfg(feature = "wasm-plugins")]
#[test]
fn middleware_output_renders_once() {
    let sandbox = Sandbox::new();
    let _server = configure(
        &sandbox,
        Box::new(|_, _| Scripted::Text(vec!["plain ".to_owned(), "response".to_owned()])),
    );
    plain_chat::install_pipeline_middleware(&sandbox);
    let output = run_plain(&sandbox, &["hello"]);
    assert!(output.status.success(), "stderr: {}", output.stderr);
    let printed = format!("{}{}", output.stdout, output.stderr);
    assert_eq!(
        printed.matches("[wasm-middleware] plain response").count(),
        1,
        "transformed reply printed once: {printed}"
    );
    assert_eq!(
        printed.matches("plain response").count(),
        1,
        "the untransformed text is never printed: {printed}"
    );
    let assistant = assistant_messages(&sandbox.workspace);
    assert_eq!(
        assistant.first().map(String::as_str),
        Some("[wasm-middleware] plain response"),
        "the persisted reply equals the rendered text"
    );
}

//! Single-shot `prx agent` keeps stdout as the reply line.
//!
//! Requires `--features test-mock` so the in-process mock provider can answer
//! without a network. With `RUST_LOG` unset, the default warn filter must hide
//! INFO tracing (and INFO tool-audit lines) from both stdout and stderr. With
//! `RUST_LOG=info`, INFO must appear on stderr and still not on stdout.
#![cfg(feature = "test-mock")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Stdio};

const REPLY: &str = "QUIET_AGENT_REPLY_SENTINEL";

fn write_mode(path: &std::path::Path, contents: &str) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .expect("open fixture config")
        .write_all(contents.as_bytes())
        .expect("write fixture config");
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn run_single_shot_agent(rust_log: Option<&str>) -> std::process::Output {
    let home = tempfile::tempdir().expect("home");
    let config_dir = home.path().join("config");
    std::fs::create_dir_all(config_dir.join("workspace")).expect("dirs");
    write_mode(
        &config_dir.join("config.toml"),
        r#"
default_provider = "mock"
default_model = "mock"

[observability]
backend = "none"

[causal_tree]
schema_version = 2
enabled = false

[autonomy]
level = "full"
workspace_only = true

[memory]
backend = "sqlite"

[[providers.models]]
provider = "mock"
model = "mock"
operations = ["chat"]
native_tools = true

[cost.prices."mock/mock"]
input = 0.0
output = 0.0
cache_write = 0.0
cache_read = 0.0
"#,
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("agent")
        .arg("-p")
        .arg("mock")
        .arg("--model")
        .arg("mock")
        .arg(REPLY)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("XDG_STATE_HOME", home.path().join(".local/state"))
        .env("OPENPRX_MOCK_RESPONSE", REPLY)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::null());
    if let Some(filter) = rust_log {
        command.env("RUST_LOG", filter);
    }
    command.output().expect("run prx agent")
}

#[test]
fn single_shot_agent_stdout_is_exactly_the_reply_and_stderr_has_no_info() {
    let output = run_single_shot_agent(None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "prx agent failed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(
        stdout.as_ref(),
        format!("{REPLY}\n"),
        "stdout must be exactly the reply line; stderr={stderr:?}"
    );
    let stderr_plain = strip_ansi(&stderr);
    assert!(
        !stderr_plain.contains("INFO"),
        "stderr must not contain INFO tracing: {stderr_plain:?}"
    );
    assert!(
        !stderr_plain.to_ascii_lowercase().contains("tool execution audited"),
        "INFO tool-audit lines must not appear at the default level: {stderr_plain:?}"
    );
}

#[test]
fn single_shot_agent_rust_log_info_writes_info_to_stderr_not_stdout() {
    let output = run_single_shot_agent(Some("info"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "prx agent failed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(
        stdout.as_ref(),
        format!("{REPLY}\n"),
        "stdout must be exactly the reply line; stderr={stderr:?}"
    );
    let stderr_plain = strip_ansi(&stderr);
    assert!(
        stderr_plain.contains("INFO"),
        "RUST_LOG=info must write INFO tracing to stderr: {stderr_plain:?}"
    );
}

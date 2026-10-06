//! A secret passed as a command-line argument draws a deprecation warning
//! naming the alternative (SP-PMM E-4, D-018 Q3); the flag keeps working.

use std::io::Write as _;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// A token-shaped sentinel. Not a real secret.
const SENTINEL: &str = "sk-ant-pmme-argv-sentinel";

fn prx(config_dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config_dir)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE");
    command
}

/// RED before PMM-E: the token was accepted silently.
#[test]
fn a_token_on_the_command_line_warns_on_stderr_and_is_still_saved() {
    let temp = TempDir::new().expect("test: temp dir");
    let config_dir = temp.path().join("config");
    let output = prx(&config_dir)
        .args(["auth", "paste-token", "--provider", "anthropic", "--token", SENTINEL])
        .output()
        .expect("test: run prx");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("warning: a secret passed with --token") && stderr.contains("--from-stdin"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains(SENTINEL) && !stdout.contains(SENTINEL));
    assert!(
        config_dir.join("auth-profiles.json").exists(),
        "behaviour is otherwise unchanged"
    );
}

/// RED before PMM-E: `--from-stdin` did not exist.
#[test]
fn a_token_piped_on_stdin_is_saved_without_a_warning() {
    let temp = TempDir::new().expect("test: temp dir");
    let config_dir = temp.path().join("config");
    let mut child = prx(&config_dir)
        .args(["auth", "paste-token", "--provider", "anthropic", "--from-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("test: spawn prx");
    child
        .stdin
        .take()
        .expect("test: stdin")
        .write_all(format!("{SENTINEL}\n").as_bytes())
        .expect("test: write stdin");
    let output = child.wait_with_output().expect("test: wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stderr.contains("warning: a secret passed"), "stderr: {stderr}");
    let store = std::fs::read_to_string(config_dir.join("auth-profiles.json")).expect("test: store");
    assert!(!store.contains(SENTINEL), "the token is stored encrypted");
}

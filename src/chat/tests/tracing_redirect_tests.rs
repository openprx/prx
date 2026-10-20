#![cfg(test)]

//! P3-1: tests for `setup_chat_tracing_to_file_in` and `TracingChatGuard`.
//!
//! The reload handle (`crate::CHAT_TRACING_RELOAD`) is a process-wide
//! `OnceLock` that `main()` initializes only for the `chat` subcommand,
//! so under `cargo test` it's empty. Happy-path tests cope with both
//! conditions: a successful redirect yields a guard, an absent reload
//! handle yields a clean `Err` (and no panic). What we strictly assert
//! is the I/O contract — directory creation, append-mode open,
//! non-panicking failure modes — which is what governs whether
//! `chat::run` can rely on this in production.
use super::*;

fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    base.join(format!("prx-p3-1-{tag}-{pid}-{nanos}"))
}

#[test]
fn setup_creates_directory_and_file() {
    let dir = unique_tmpdir("create");
    let log = dir.join("chat.log");
    assert!(!dir.exists(), "precondition: tmp dir must not exist");
    let result = setup_chat_tracing_to_file_in(&dir);
    // Directory + file must be created regardless of whether the global
    // reload handle is wired up (we open the file *before* reloading).
    assert!(dir.is_dir(), "chat log dir should exist after setup");
    assert!(log.is_file(), "chat.log should exist after setup");
    // Result is Ok only when CHAT_TRACING_RELOAD is initialized (chat
    // subcommand path in main). Either outcome is non-panicking.
    match result {
        Ok(_guard) => { /* worker guard drops here → flush */ }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("reload handle not initialized"),
                "unexpected error variant: {msg}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn setup_appends_does_not_truncate_existing_log() {
    let dir = unique_tmpdir("append");
    std::fs::create_dir_all(&dir).expect("test: create_dir_all must succeed");
    let log = dir.join("chat.log");
    std::fs::write(&log, b"pre-existing\n").expect("test: seed write must succeed");

    // Run setup; on Ok the guard drops immediately (flushing nothing
    // since nothing was logged); on Err we just confirm file is intact.
    let _ = setup_chat_tracing_to_file_in(&dir);

    let contents = std::fs::read_to_string(&log).expect("test: read log");
    assert!(
        contents.starts_with("pre-existing"),
        "OpenOptions::append must not truncate existing chat.log; got: {contents:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn setup_returns_err_when_path_is_not_a_directory() {
    // Point `dir` at an existing file → create_dir_all should fail with
    // ENOTDIR / "Not a directory". Must return Err, must not panic.
    let parent = unique_tmpdir("notdir-parent");
    std::fs::create_dir_all(&parent).expect("test: create parent");
    let file_as_dir = parent.join("not-a-dir");
    std::fs::write(&file_as_dir, b"x").expect("test: seed file");

    let result = setup_chat_tracing_to_file_in(&file_as_dir);
    assert!(
        result.is_err(),
        "expected Err when target path is a regular file, got Ok"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn resolve_chat_log_dir_yields_an_absolute_path() {
    // Should succeed via either UserDirs or HOME; on container runners
    // without HOME the function bails — accept either outcome but never
    // panic.
    match resolve_chat_log_dir() {
        Ok(p) => {
            assert!(p.ends_with(".openprx"), "must point at ~/.openprx, got {p:?}");
            assert!(p.is_absolute(), "chat log dir must be absolute, got {p:?}");
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("cannot determine home directory"),
                "unexpected error: {msg}"
            );
        }
    }
}

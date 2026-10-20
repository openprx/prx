#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]
use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tokio::sync::{Mutex, MutexGuard};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReadDirStream;

/// Helper to set env vars in tests (unsafe in edition 2024).
#[allow(unsafe_code)]
fn test_set_env(key: impl AsRef<std::ffi::OsStr>, value: impl AsRef<std::ffi::OsStr>) {
    // The lib-test home resolver never reads `HOME`; it follows a test's
    // own home here (D-070).
    if key.as_ref() == "HOME" {
        follow_test_home_env(Some(value.as_ref()));
    }
    // SAFETY: tests run single-threaded via serial test mutex or unique env keys.
    unsafe { std::env::set_var(key, value) }
}

/// Helper to remove env vars in tests (unsafe in edition 2024).
#[allow(unsafe_code)]
fn test_remove_env(key: impl AsRef<std::ffi::OsStr>) {
    if key.as_ref() == "HOME" {
        follow_test_home_env(None);
    }
    // SAFETY: tests run single-threaded via serial test mutex or unique env keys.
    unsafe { std::env::remove_var(key) }
}

fn agentic_delegate_config(allowed_tools: Vec<String>) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "openrouter".into(),
        model: "model-test".into(),
        system_prompt: None,
        api_key: None,
        credential_ref: None,
        temperature: None,
        max_depth: 3,
        agentic: true,
        allowed_tools,
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
    }
}

// ── Environment variable test helpers ─────────

async fn env_override_lock() -> MutexGuard<'static, ()> {
    static ENV_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::const_new(());
    ENV_OVERRIDE_TEST_LOCK.lock().await
}

fn runtime_proxy_cache_contains(cache_key: &str) -> bool {
    runtime_proxy_client_cache().read().contains_key(cache_key)
}

/// Serialise tests that mutate the global `RUNTIME_PROXY_CLIENT_CACHE` so
/// they cannot race against each other.
static PROXY_CACHE_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Outbound recipient ACL validation (plan a1) ─────────────────────────

fn scope_config_with(send_allow: &[&str], send_deny: &[&str]) -> ScopeConfig {
    ScopeConfig {
        default: ScopeDefault::Allow,
        rules: vec![ScopeRule {
            send_allow: send_allow.iter().map(|s| (*s).to_string()).collect(),
            send_deny: send_deny.iter().map(|s| (*s).to_string()).collect(),
            ..ScopeRule::default()
        }],
        assign_owners: Vec::new(),
    }
}

fn scope_rule_with(channel: Option<&str>, chat_type: Option<&str>, send_allow: &[&str]) -> ScopeRule {
    ScopeRule {
        channel: channel.map(str::to_string),
        chat_type: chat_type.map(str::to_string),
        send_allow: send_allow.iter().map(|s| (*s).to_string()).collect(),
        ..ScopeRule::default()
    }
}

// ── Assignment ACL validation (plan c1) ─────────────────────────────────

fn assign_rule(channel: Option<&str>, assign_allow: &[&str], assign_deny: &[&str]) -> ScopeRule {
    ScopeRule {
        channel: channel.map(str::to_string),
        assign_allow: assign_allow.iter().map(|entry| (*entry).to_string()).collect(),
        assign_deny: assign_deny.iter().map(|entry| (*entry).to_string()).collect(),
        ..ScopeRule::default()
    }
}

// ── `prx init` target resolution ────────────────────────────

/// Recursive `relative path -> (length, FNV-1a digest)` snapshot, used to
/// prove that a directory was not touched at all — not merely that it still
/// parses. Digested rather than kept verbatim so a failure prints a readable
/// diff instead of every byte of a configuration tree.
fn snapshot_dir(root: &Path) -> std::collections::BTreeMap<PathBuf, (usize, u64)> {
    fn digest(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    let mut snapshot = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                snapshot.insert(relative, (bytes.len(), digest(&bytes)));
            }
        }
    }
    snapshot
}

fn restore_home(original: Option<String>) {
    match original {
        Some(home) => test_set_env("HOME", home),
        None => test_remove_env("HOME"),
    }
}

/// A log capture for the INT-2B B2 tests: every event `f` emits, as text.
fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
    (result, logs)
}
mod agent_features;
mod channels;
mod defaults_and_integrations;
mod memory;
mod paths;
mod persistence;
mod proxy;
mod security;

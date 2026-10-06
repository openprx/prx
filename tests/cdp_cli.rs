//! `prx causal` read commands (V8.07, V6.04 CLI half, V6.09 CLI half).
//!
//! Each test runs the built binary over its own configuration directory.
//! Backends are loopback stubs; nothing leaves the machine.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openprx::config::Config;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const SECRET: &str = "sk-CLI-SECRET-SENTINEL-7Qm2xZp9";

/// The decision backend's own credential (D-093 N-7).
const DECISION_SECRET: &str = "sk-DECISION-CLI-SENTINEL-4Rt8wQ";

/// A configuration directory whose `config.toml` is the default with the
/// `(path, value)` overrides applied.
fn config_dir(temp: &TempDir, overrides: &[(&[&str], toml::Value)]) -> PathBuf {
    let dir = temp.path().join("config");
    std::fs::create_dir_all(dir.join("workspace")).expect("test: dirs");
    let mut root: toml::Value = toml::Value::try_from(Config::default()).expect("test: default config");
    for (path, value) in overrides {
        let (last, parents) = path.split_last().expect("test: path");
        let mut table = &mut root;
        for part in parents {
            table = table
                .as_table_mut()
                .expect("test: table")
                .entry((*part).to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        }
        table
            .as_table_mut()
            .expect("test: table")
            .insert((*last).to_string(), value.clone());
    }
    std::fs::write(
        dir.join("config.toml"),
        toml::to_string_pretty(&root).expect("test: toml"),
    )
    .expect("test: write");
    dir
}

fn causal(dir: &Path, args: &[&str]) -> Output {
    // HOME and the XDG directories point into the test's own directory
    // (D-091 F-15): nothing of the developer's home is read or written.
    let home = dir.parent().unwrap_or(dir).join("home");
    std::fs::create_dir_all(&home).expect("test: home");
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(dir)
        .arg("causal")
        .args(args)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("RUST_LOG", "info")
        .output()
        .expect("test: run prx")
}

/// Every file under `dir`, with its bytes.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

fn stdout_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "prx failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("test: json output")
}

/// A loopback backend that refuses every request with 503, counting them.
fn refusing_stub() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test: bind");
    let port = listener.local_addr().expect("test: addr").port();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            seen.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0_u8; 8192];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 503 STUB\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), count)
}

/// A loopback backend that refuses every request with 503, keeping each
/// request's head.
fn recording_stub() -> (String, Arc<parking_lot::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test: bind");
    let port = listener.local_addr().expect("test: addr").port();
    let heads = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let text = String::from_utf8_lossy(&buffer[..read]).to_string();
            seen.lock()
                .push(text.split("\r\n\r\n").next().unwrap_or_default().to_string());
            let _ = stream.write_all(b"HTTP/1.1 503 STUB\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), heads)
}

/// Store `DECISION_SECRET` as the decision backend's credential reference,
/// owned by the backend's provider, and point
/// `causal_tree.backend.credential_ref` at it.
fn with_decision_credential(dir: &Path, endpoint: &str) {
    let config_path = dir.join("config.toml");
    // The written configuration is the default one with overrides that do
    // not touch `[secrets]`: the store is where the CLI will look for it.
    let config = Config {
        config_path: config_path.clone(),
        workspace_dir: dir.join("workspace"),
        ..Config::default()
    };
    let stored = openprx::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("test: references")
        .set(&format!("custom:{endpoint}"), "decision-cli", DECISION_SECRET)
        .expect("test: stored");
    let mut root: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).expect("test: read")).expect("test: toml");
    root["causal_tree"]["backend"]
        .as_table_mut()
        .expect("test: backend table")
        .insert("credential_ref".to_string(), toml::Value::String(stored.id));
    std::fs::write(&config_path, toml::to_string_pretty(&root).expect("test: toml")).expect("test: write");
}

fn enabled_loopback(temp: &TempDir, endpoint: &str) -> PathBuf {
    config_dir(
        temp,
        &[
            (&["api_key"], toml::Value::String(SECRET.to_string())),
            (&["causal_tree", "enabled"], toml::Value::Boolean(true)),
            (
                &["causal_tree", "backend", "kind"],
                toml::Value::String("compatible_http".to_string()),
            ),
            (
                &["causal_tree", "backend", "endpoint"],
                toml::Value::String(endpoint.to_string()),
            ),
            (
                &["causal_tree", "backend", "model"],
                toml::Value::String("decider-1".to_string()),
            ),
            // The model is declared for the decision operation, as the strict
            // selection requires.
            (
                &["providers", "models"],
                toml::Value::Array(vec![
                    toml::from_str(&format!(
                        "provider = \"custom:{endpoint}\"\nmodel = \"decider-1\"\noperations = [\"decision_endpoint\"]\n"
                    ))
                    .expect("test: declaration"),
                ]),
            ),
        ],
    )
}

/// V8.07: `prx causal status` parses and reports the order, which the
/// builtin always ends; a disabled plane creates no store.
#[test]
fn prx_causal_status_parses_and_reports_order() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    let status = stdout_json(&causal(&dir, &["status", "--json"]));
    assert_eq!(status["enabled"], true);
    let order = status["order"].as_array().expect("test: order");
    assert_eq!(
        order.first().and_then(|slot| slot["kind"].as_str()),
        Some("compatible_http")
    );
    assert_eq!(
        order.first().and_then(|slot| slot["locality"].as_str()),
        Some("loopback")
    );
    assert_eq!(order.last().and_then(|slot| slot["kind"].as_str()), Some("builtin"));
    assert_eq!(status["store"]["mode"], "read_write");
    assert_eq!(status["store"]["schema_version"], 2);

    let backends = stdout_json(&causal(&dir, &["backends", "--json"]));
    assert_eq!(backends["order"][0]["kind"], "compatible_http");
    assert_eq!(backends["order"][0]["excluded"], serde_json::Value::Null);

    let disabled_temp = TempDir::new().expect("test: temp");
    let disabled = config_dir(&disabled_temp, &[]);
    let status = stdout_json(&causal(&disabled, &["status", "--json"]));
    assert_eq!(status["enabled"], false);
    assert_eq!(status["store"]["mode"], "read_only");
    assert!(
        !disabled.join("workspace/self/causal_tree/decisions.db").exists(),
        "a disabled plane creates no store"
    );
    assert!(
        !disabled.join("workspace/self/causal_tree/.decision_key").exists(),
        "a disabled plane creates no key"
    );
}

fn digest(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| hex::encode(Sha256::digest(bytes)))
}

/// V8.07 `prx_causal_test_does_not_modify_store_or_thresholds`: a test
/// against a loopback backend reaches it, falls back to the builtin when it
/// refuses, and leaves the store and the configuration as they were.
#[test]
fn prx_causal_test_does_not_modify_store_or_thresholds() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, requests) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    // Create the store first, so the test has something it could change.
    stdout_json(&causal(&dir, &["status", "--json"]));
    let store = dir.join("workspace/self/causal_tree");
    let files = ["decisions.db", "decisions.db-wal", "decisions.db-shm"];
    let before: Vec<_> = files.iter().map(|file| digest(&store.join(file))).collect();
    let config_before = digest(&dir.join("config.toml"));
    let seen_before = requests.load(Ordering::SeqCst);

    let test = stdout_json(&causal(&dir, &["test", "--json"]));
    assert_eq!(test["outcome"], "fell_back", "{test}");
    assert_eq!(test["sent"], true, "{test}");
    assert_eq!(test["served_by"], "builtin");
    assert!(
        requests.load(Ordering::SeqCst) > seen_before,
        "the loopback backend was asked"
    );
    let answered = test["answered"].as_array().expect("test: answered");
    assert_eq!(answered.len(), 3, "the fixture asks one question of each kind: {test}");

    let builtin = stdout_json(&causal(&dir, &["test", "--backend", "builtin", "--json"]));
    assert_eq!(builtin["outcome"], "passed");
    assert_eq!(builtin["sent"], false);

    let after: Vec<_> = files.iter().map(|file| digest(&store.join(file))).collect();
    assert_eq!(after, before, "the store is untouched");
    assert_eq!(
        digest(&dir.join("config.toml")),
        config_before,
        "the configuration is untouched"
    );

    // On a fresh workspace, a test creates only the decision key.
    let fresh = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&fresh, &endpoint);
    stdout_json(&causal(&dir, &["test", "--json"]));
    let created: Vec<String> = tree(&dir.join("workspace/self"))
        .into_iter()
        .map(|(path, _)| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(created, [".decision_key"], "a test creates nothing but the key");
}

/// V6.09 CLI half (D-093 N-7): no output of any subcommand, standard output
/// or error, carries a credential. The sentinel sits in the decision
/// backend's own credential path, an auth-profile reference that
/// `prx causal test` does send to the backend, and the root key holds
/// another.
#[test]
fn causal_cli_output_never_carries_a_credential_or_userinfo() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, heads) = recording_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    with_decision_credential(&dir, &endpoint);
    let runs: [&[&str]; 10] = [
        &["status"],
        &["status", "--json"],
        &["backends"],
        &["backends", "--json"],
        &["test"],
        &["test", "--json"],
        &["inspect", "--decision-id", "0192f0c4-0000-7000-8000-000000000000"],
        &["inspect", "--decision-id", "not-an-id"],
        &["evaluate", "--from", "2026-01-01", "--to", "2026-12-31"],
        &["evaluate", "--from", "2026-01-01", "--to", "2026-12-31", "--json"],
    ];
    for args in runs {
        let output = causal(&dir, args);
        for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains("CLI-SECRET-SENTINEL"), "{args:?} {stream}: {text}");
            assert!(!text.contains("DECISION-CLI-SENTINEL"), "{args:?} {stream}: {text}");
        }
    }
    let heads = heads.lock();
    assert!(
        heads.iter().any(|head| head
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {DECISION_SECRET}")))),
        "the decision credential was the one sent: {} requests",
        heads.len()
    );
}

/// V6.04 CLI half: an unknown decision reads as absent and fails.
#[test]
fn inspecting_an_unknown_decision_fails_without_detail() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    let output = causal(
        &dir,
        &["inspect", "--decision-id", "0192f0c4-0000-7000-8000-000000000000"],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no such decision in this workspace"), "{stderr}");
}

/// `evaluate` writes a private report and labels what it cannot estimate.
#[test]
fn evaluate_writes_a_private_report() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    let view = stdout_json(&causal(
        &dir,
        &["evaluate", "--from", "2026-01-01", "--to", "2026-12-31", "--json"],
    ));
    let path = PathBuf::from(view["report_path"].as_str().expect("test: path"));
    assert!(view["report"]["counterfactual_estimand"].as_str().is_some());
    assert!(path.starts_with(dir.join("workspace/self/causal_tree/reports")));
    let mode = std::fs::metadata(&path).expect("test: report").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(view["report"]["counterfactual"]["status"], "unavailable");
    assert_eq!(view["report"]["counterfactual"]["reason"], "no exploration data");
}

/// The first read-write open imports v1 feedback without the raw goal, and
/// `inspect` shows the imported decision in the operator's scope.
#[test]
fn status_imports_v1_feedback_and_inspect_shows_it() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    let feedback = dir.join("workspace/self/causal_tree/feedback");
    std::fs::create_dir_all(&feedback).expect("test: feedback dir");
    let line = serde_json::json!({
        "timestamp": "2026-09-01T10:00:00Z",
        "session_id": "s-1",
        "request_id": "r-1",
        "goal": "V1-GOAL-SENTINEL summarise my notes",
        "user_intent": "simple",
        "chosen_branch_id": "b-1",
        "fallback_branch_ids": [],
        "branches": [{"branch_id": "b-1", "label": "RetrieveThenAnswer"}]
    });
    std::fs::write(feedback.join("2026-09-01.jsonl"), format!("{line}\n")).expect("test: feedback");

    let status = stdout_json(&causal(&dir, &["status", "--json"]));
    assert_eq!(status["store"]["v1_import"]["imported"], 1, "{status}");
    assert_eq!(status["store"]["legacy_decisions"], 1);
    // R-I1-phase2 R-1: the run closed its writer; no pending WAL is left.
    for side in ["decisions.db-wal", "decisions.db-shm"] {
        assert!(
            !dir.join("workspace/self/causal_tree").join(side).exists(),
            "{side} is left after an enabled run"
        );
    }

    let db = dir.join("workspace/self/causal_tree/decisions.db");
    let id: String = rusqlite::Connection::open(&db)
        .expect("test: db")
        .query_row("SELECT decision_id FROM decisions", [], |row| row.get(0))
        .expect("test: id");
    let output = causal(&dir, &["inspect", "--decision-id", &id, "--json"]);
    let view = stdout_json(&output);
    assert_eq!(view["legacy_v1"], true);
    assert_eq!(view["has_owner"], false);
    assert_eq!(view["directive_kind"], "retrieve");
    assert_eq!(view["outcome_join"], "none");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("V1-GOAL-SENTINEL"), "{text}");

    let again = stdout_json(&causal(&dir, &["status", "--json"]));
    assert_eq!(again["store"]["legacy_decisions"], 1, "the import is idempotent");
}

/// D-091 F-5: on a disabled plane every read command, evaluate included,
/// leaves the workspace exactly as it was; the report goes to stdout only.
#[test]
fn a_disabled_plane_changes_nothing() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(&temp, &[(&["causal_tree", "enabled"], toml::Value::Boolean(false))]);
    let workspace = dir.join("workspace");
    let before = tree(&workspace);
    for args in [
        &["status", "--json"][..],
        &["backends", "--json"],
        &["test", "--json"],
        &["inspect", "--decision-id", "0192f0c4-0000-7000-8000-000000000000"],
    ] {
        let _ = causal(&dir, args);
    }
    let view = stdout_json(&causal(
        &dir,
        &["evaluate", "--from", "2026-01-01", "--to", "2026-12-31", "--json"],
    ));
    assert_eq!(view["report_path"], serde_json::Value::Null);
    assert_eq!(tree(&workspace), before, "nothing under the workspace changed");
}

/// D-082 N-9: a disabled plane never opens the credential store. With the
/// store held by another process, `status` answers at once without a
/// factory error, nothing is sent, and the store is left as it was.
#[test]
fn a_disabled_plane_never_opens_the_credential_store() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, requests) = refusing_stub();
    let dir = enabled_loopback(&temp, &endpoint);
    let config_path = dir.join("config.toml");
    let mut root: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).expect("test: read")).expect("test: toml");
    root["causal_tree"]["enabled"] = toml::Value::Boolean(false);
    root["causal_tree"]["backend"]
        .as_table_mut()
        .expect("test: backend table")
        .insert(
            "credential_ref".to_string(),
            toml::Value::String("cred-decision-held".to_string()),
        );
    std::fs::write(&config_path, toml::to_string_pretty(&root).expect("test: toml")).expect("test: write");
    let lock = dir.join("auth-profiles.lock");
    std::fs::write(&lock, "pid=1\n").expect("test: held lock");
    let started = std::time::Instant::now();
    let status = stdout_json(&causal(&dir, &["status", "--json"]));
    assert_eq!(status["enabled"], false);
    assert_eq!(status["factory_error"], serde_json::Value::Null, "{status}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "the held store was never waited for"
    );
    assert_eq!(std::fs::read(&lock).expect("test: lock"), b"pid=1\n");
    assert!(!dir.join("auth-profiles.json").exists(), "no store was created");
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

/// CDP-14: `prx causal config` prints the plan and writes nothing without
/// --yes; with it, the change is applied through the management service
/// and the backend's model is declared in the same change.
#[test]
fn causal_config_plans_first_and_applies_with_yes() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(&temp, &[]);
    let before = digest(&dir.join("config.toml"));
    let args = [
        "config",
        "set",
        "backend",
        "--kind",
        "compatible_http",
        "--endpoint",
        "http://127.0.0.1:8900/v1",
        "--model",
        "decider-1",
    ];
    let dry = causal(&dir, &args);
    let text = String::from_utf8_lossy(&dry.stdout).to_string();
    assert!(dry.status.success(), "{text}{}", String::from_utf8_lossy(&dry.stderr));
    assert!(text.contains("Dry run: nothing was written"), "{text}");
    assert_eq!(digest(&dir.join("config.toml")), before, "a dry run writes nothing");
    // A new decision endpoint is high risk: the confirmation names the
    // revision the plan reported.
    let revision = text
        .rsplit("--confirm-high-risk ")
        .next()
        .and_then(|rest| rest.split_whitespace().next())
        .expect("test: the dry run names the revision")
        .to_string();
    let mut applied = args.to_vec();
    applied.extend(["--yes", "--confirm-high-risk", revision.as_str()]);
    let output = causal(&dir, &applied);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written = std::fs::read_to_string(dir.join("config.toml")).expect("test: read");
    assert!(
        written.contains("compatible_http") && written.contains("decision_endpoint"),
        "{written}"
    );
}

/// D-094 C-6: switching to the builtin while remote processing is on turns
/// it off in the same plan, so the change is not refused.
#[test]
fn switching_to_the_builtin_turns_remote_off_in_the_same_change() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(
        &temp,
        &[(
            &["causal_tree", "backend", "remote_enabled"],
            toml::Value::Boolean(true),
        )],
    );
    let output = causal(&dir, &["config", "set", "backend", "--kind", "builtin"]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("remote_enabled"), "the plan turns remote off: {text}");
}

/// PMM-D-7: `test --live` without --yes names where the call would go and
/// sends nothing; a credential for `set secret` is read only once the
/// high-risk change is confirmed.
#[test]
fn live_tests_and_credentials_wait_for_confirmation() {
    let temp = TempDir::new().expect("test: temp");
    let (endpoint, requests) = refusing_stub();
    let remote = endpoint.replace("http://127.0.0.1", "https://decisions.invalid");
    let dir = config_dir(
        &temp,
        &[
            (&["causal_tree", "enabled"], toml::Value::Boolean(true)),
            (
                &["causal_tree", "backend", "kind"],
                toml::Value::String("compatible_http".to_string()),
            ),
            (&["causal_tree", "backend", "endpoint"], toml::Value::String(remote)),
            (
                &["causal_tree", "backend", "remote_enabled"],
                toml::Value::Boolean(true),
            ),
        ],
    );
    let view = stdout_json(&causal(&dir, &["test", "--live", "--json"]));
    assert_eq!(view["outcome"], "confirmation_required", "{view}");
    assert_eq!(view["sent"], false);
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    let loopback = TempDir::new().expect("test: temp");
    let (endpoint, _) = refusing_stub();
    let dir = enabled_loopback(&loopback, &endpoint);
    let output = causal(
        &dir,
        &[
            "config",
            "set",
            "secret",
            "--from-env",
            "PRX_TEST_UNSET_DECISION_SECRET",
        ],
    );
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("Dry run: nothing was read or written"), "{text}");
    assert!(!dir.join("auth-profiles.json").exists(), "nothing was stored");
}

/// `prx` with `args`, HOME and XDG inside `root`.
fn prx(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("test: home");
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(args)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .output()
        .expect("test: run prx")
}

/// CDP-14: `prx init` with a loopback decision backend writes a
/// configuration that loads and passes the decision check, with the model
/// declared for the decision endpoint.
#[test]
fn init_with_a_loopback_decision_backend_writes_a_valid_config() {
    let temp = TempDir::new().expect("test: temp");
    let target = temp.path().join("init");
    let target_text = target.to_string_lossy().to_string();
    let output = prx(
        temp.path(),
        &[
            "init",
            "--dir",
            &target_text,
            "--decision-backend",
            "compatible_http",
            "--decision-endpoint",
            "http://127.0.0.1:8900/v1",
            "--decision-model",
            "decider-1",
            "--decision-enable",
        ],
    );
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status = stdout_json(&causal(&target, &["status", "--json"]));
    assert_eq!(status["enabled"], true, "{status}");
    assert_eq!(status["order"][0]["kind"], "compatible_http", "{status}");
    assert_eq!(status["factory_error"], serde_json::Value::Null, "{status}");
}

/// CDP-14: `prx init` refuses a hosted decision backend before anything is
/// written, naming where to set one up instead.
#[test]
fn init_refuses_a_hosted_kind() {
    let temp = TempDir::new().expect("test: temp");
    let target = temp.path().join("init");
    let target_text = target.to_string_lossy().to_string();
    for kind in ["typesafe", "openrouter"] {
        let output = prx(
            temp.path(),
            &[
                "init",
                "--dir",
                &target_text,
                "--decision-backend",
                kind,
                "--decision-model",
                "jev-1.13.0",
            ],
        );
        assert!(!output.status.success(), "{kind}");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.contains("prx causal config set backend"), "{kind}: {text}");
        assert!(!target.join("config.toml").exists(), "{kind}: nothing was written");
    }
}

/// CDP-14: the quick-setup decision flags go through the one setup
/// service: a loopback choice is written, declared and checked; a hosted
/// one is refused before quick setup writes anything.
#[test]
fn quick_setup_decision_flags_use_the_shared_service() {
    let temp = TempDir::new().expect("test: temp");
    let dir = temp.path().join("onboard");
    let dir_text = dir.to_string_lossy().to_string();
    let output = prx(
        temp.path(),
        &[
            "--config-dir",
            &dir_text,
            "onboard",
            "--provider",
            "ollama",
            "--model",
            "llama3.2",
            "--decision-backend",
            "compatible_http",
            "--decision-endpoint",
            "http://127.0.0.1:8900/v1",
            "--decision-model",
            "decider-1",
        ],
    );
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let written = std::fs::read_to_string(dir.join("config.toml")).expect("test: config");
    assert!(
        written.contains("compatible_http") && written.contains("decision_endpoint"),
        "{written}"
    );

    let hosted = temp.path().join("hosted");
    let hosted_text = hosted.to_string_lossy().to_string();
    let output = prx(
        temp.path(),
        &[
            "--config-dir",
            &hosted_text,
            "onboard",
            "--provider",
            "ollama",
            "--decision-backend",
            "typesafe",
            "--decision-model",
            "jev-1.13.0",
        ],
    );
    assert!(!output.status.success());
    assert!(!hosted.join("config.toml").exists(), "nothing was written");
}

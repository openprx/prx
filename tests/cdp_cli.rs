//! `prx decision-engine` commands, the retired `prx causal` line (P2-R8,
//! OV-7) and the setup flags of `prx init` (CDP-14).
//!
//! Each test runs the built binary over its own configuration directory;
//! nothing leaves the machine.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use openprx::config::Config;
use tempfile::TempDir;

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

const MAX_CLI_OUTPUT_BYTES: usize = 1_048_576;

fn bounded_cli_output(command: Command) -> Output {
    use tokio::io::AsyncReadExt as _;
    async fn read_output(stream: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream
            .take((MAX_CLI_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_CLI_OUTPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CLI output exceeded its test bound",
            ));
        }
        Ok(bytes)
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: bounded CLI runtime")
        .block_on(async {
            let mut child = tokio::process::Command::from(command)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("test: owned CLI child");
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                let stdout = child.stdout.take().expect("test: owned stdout pipe");
                let stderr = child.stderr.take().expect("test: owned stderr pipe");
                let (stdout, stderr, status) = tokio::try_join!(read_output(stdout), read_output(stderr), child.wait())
                    .expect("test: bounded CLI process and output");
                Output { status, stdout, stderr }
            })
            .await
            .expect("test: CLI must finish within its deadline")
        })
}

/// The largest `config.toml` a test reads back.
const MAX_CONFIG_BYTES: u64 = 1_048_576;

/// `config.toml` of `dir` as stored, read under [`MAX_CONFIG_BYTES`]; a
/// larger file fails the test.
fn config_bytes(dir: &Path) -> Vec<u8> {
    use std::io::Read as _;
    let file = std::fs::File::open(dir.join("config.toml")).expect("test: open config.toml");
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .expect("test: read config.toml");
    assert!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "config.toml exceeded its test bound of {MAX_CONFIG_BYTES} bytes"
    );
    bytes
}

/// `config.toml` of `dir` as UTF-8 text, bounded as [`config_bytes`].
fn config_text(dir: &Path) -> String {
    String::from_utf8(config_bytes(dir)).expect("test: config.toml is UTF-8")
}

fn prx_in(dir: &Path, args: &[&str]) -> Output {
    // HOME and the XDG directories point into the test's own directory
    // (D-091 F-15): nothing of the developer's home is read or written.
    let home = dir.parent().unwrap_or(dir).join("home");
    std::fs::create_dir_all(&home).expect("test: home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(dir)
        .args(args)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("RUST_LOG", "info");
    bounded_cli_output(command)
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

/// P2-R8: `prx decision-engine status` reports the default-off engine.
#[test]
fn decision_engine_status_reports_disabled_by_default() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(&temp, &[]);
    let status = stdout_json(&prx_in(&dir, &["decision-engine", "status", "--json"]));
    assert_eq!(status["enabled"], false, "{status}");
    assert_eq!(status["mode"], "off", "{status}");
    assert_eq!(status["telemetry_cap"], 0, "{status}");
    assert_eq!(status["queue_depth"], 0, "{status}");
    let backends = stdout_json(&prx_in(&dir, &["decision-engine", "backends", "--json"]));
    let rows = backends.as_array().expect("test: rows");
    assert!(!rows.is_empty() && rows.len() <= 2, "{backends}");
    assert_eq!(
        rows.last().map(|row| row["kind"].clone()),
        Some("builtin".into()),
        "{backends}"
    );
    assert!(
        !dir.join("workspace").join("decision-engine.sqlite").exists(),
        "status opens no telemetry store"
    );
}

/// OV-7: `prx causal …` prints exactly one deprecation line and exits 0,
/// whatever its arguments.
#[test]
fn causal_command_prints_one_deprecation_line() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(&temp, &[]);
    for args in [
        &["causal"][..],
        &["causal", "status", "--json"],
        &["causal", "config", "set", "backend", "--kind", "builtin", "--yes"],
    ] {
        let output = prx_in(&dir, args);
        assert!(output.status.success(), "{args:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.lines().count(), 1, "{args:?}: {text}");
        assert!(text.contains("prx decision-engine"), "{args:?}: {text}");
    }
}

/// P2-R8: only allowlisted keys are set; an unknown key, every secret-
/// bearing key and `apply-calibration` fail with one error and exit ≠ 0,
/// and the configuration is unchanged.
#[test]
fn decision_engine_config_set_rejects_unknown_and_secret_keys() {
    let temp = TempDir::new().expect("test: temp");
    let dir = config_dir(&temp, &[]);
    let before = config_bytes(&dir);
    for (key, value) in [
        ("policy.max_candidates", "3"),
        ("backend.credential_ref", "cred-decision-x"),
        ("backend.api_key", "sk-CONFIG-SET-SENTINEL"),
        ("token", "t"),
        ("apply-calibration", "0192f0c4-0000-7000-8000-000000000001"),
    ] {
        let output = prx_in(&dir, &["decision-engine", "config", "set", key, value, "--yes"]);
        assert!(!output.status.success(), "{key}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot be set here"), "{key}: {stderr}");
        assert!(
            !stderr.contains("sk-CONFIG-SET-SENTINEL"),
            "{key}: the value is never echoed"
        );
    }
    assert_eq!(config_bytes(&dir), before);
    let output = prx_in(
        &dir,
        &["decision-engine", "config", "set", "telemetry_cap", "10", "--yes"],
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let written = config_text(&dir);
    assert!(written.contains("telemetry_cap = 10"), "{written}");
    // Allowlisted keys whose names contain a credential word are accepted.
    for (key, value, expected) in [
        ("prompt_budget_tokens", "300", "prompt_budget_tokens = 300"),
        ("token_ratio_limit", "0.5", "token_ratio_limit = 0.5"),
    ] {
        let output = prx_in(&dir, &["decision-engine", "config", "set", key, value, "--yes"]);
        assert!(
            output.status.success(),
            "{key}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let written = config_text(&dir);
        assert!(written.contains(expected), "{key}: {written}");
    }
}

/// Audit r2 F1: `prx decision-engine config set backend.model` selecting a
/// model whose catalog recommendation exceeds the configured
/// `latency_budget_ms` is refused with both values, with the engine on or
/// off, and `config.toml` is unchanged; after `latency_budget_ms` is raised
/// explicitly the same selection is applied.
#[test]
fn decision_engine_config_set_refuses_a_backend_below_its_latency_recommendation() {
    let declaration = |model: &str| {
        let mut entry = toml::map::Map::new();
        entry.insert("provider".into(), toml::Value::String("openrouter".into()));
        entry.insert("model".into(), toml::Value::String(model.into()));
        entry.insert(
            "operations".into(),
            toml::Value::Array(vec![toml::Value::String("decision_endpoint".into())]),
        );
        toml::Value::Table(entry)
    };
    let models = toml::Value::Array(vec![declaration("openai/gpt-5-mini"), declaration("typesafe/jev-1.13")]);
    let select = [
        "decision-engine",
        "config",
        "set",
        "backend.model",
        "typesafe/jev-1.13",
        "--yes",
    ];
    for enabled in [false, true] {
        let temp = TempDir::new().expect("test: temp");
        let dir = config_dir(
            &temp,
            &[
                (&["decision_engine", "enabled"], toml::Value::Boolean(enabled)),
                (&["decision_engine", "backend", "kind"], "openrouter".into()),
                (&["decision_engine", "backend", "model"], "openai/gpt-5-mini".into()),
                (
                    &["decision_engine", "backend", "pinned_snapshot"],
                    "typesafe/jev-1.13-20260917".into(),
                ),
                (
                    &["decision_engine", "backend", "credential_ref"],
                    "cred-openrouter-decision".into(),
                ),
                (&["providers", "models"], models.clone()),
            ],
        );
        let before = config_bytes(&dir);
        let output = prx_in(&dir, &select);
        assert!(!output.status.success(), "enabled = {enabled}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("typesafe/jev-1.13") && stderr.contains("3000") && stderr.contains("500"),
            "enabled = {enabled}: {stderr}"
        );
        assert_eq!(config_bytes(&dir), before, "enabled = {enabled}: nothing is written");
        if enabled {
            continue;
        }
        for args in [
            &["decision-engine", "config", "set", "latency_budget_ms", "3000", "--yes"][..],
            &select[..],
        ] {
            let output = prx_in(&dir, args);
            assert!(
                output.status.success(),
                "{args:?}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let loaded: Config = toml::from_str(&config_text(&dir)).expect("test: written config");
        assert_eq!(loaded.decision_engine.latency_budget_ms, 3_000);
        assert_eq!(
            loaded
                .decision_engine
                .backend
                .as_ref()
                .map(|backend| backend.model.as_str()),
            Some("typesafe/jev-1.13")
        );
    }
}

/// `prx` with `args`, HOME and XDG inside `root`.
fn prx(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("test: home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .args(args)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"));
    bounded_cli_output(command)
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
    let help = prx(temp.path(), &["init", "--help"]);
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.status.success()
            && help_text.contains("Enable causal recovery")
            && help_text.contains("decision_engine.enabled"),
        "test: help names recovery and the separate advisory switch: {help_text}"
    );
    let status = stdout_json(&prx_in(&target, &["decision-engine", "status", "--json"]));
    assert_eq!(
        status["enabled"], false,
        "test: init keeps the advisory engine off: {status}"
    );
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
        assert!(
            text.contains("prx decision-engine config set backend.kind"),
            "{kind}: {text}"
        );
        assert!(!target.join("config.toml").exists(), "{kind}: nothing was written");
    }
}

/// CDP-14: the quick-setup decision flags go through the one setup
/// service: a loopback choice is written, declared and checked; a hosted
/// one is refused before quick setup writes anything.
#[test]
fn quick_setup_decision_flags_use_the_shared_service() {
    let temp = TempDir::new().expect("test: temp");
    let dir = temp.path().join("init");
    let dir_text = dir.to_string_lossy().to_string();
    let output = prx(
        temp.path(),
        &[
            "--config-dir",
            &dir_text,
            "init",
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
    let written = openprx::config::files::read_merged_toml_with_gate(&dir.join("config.toml"))
        .expect("test: config")
        .to_string();
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
            "init",
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

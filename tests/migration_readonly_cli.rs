use openprx::config::Config;
use std::process::Command;
use tempfile::TempDir;

fn prx_command(config_dir: &std::path::Path, subcommand: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(config_dir)
        .args(["migrate", subcommand])
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .output()
        .expect("run prx migration command")
}

#[test]
fn status_does_not_initialize_missing_config_directory() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("missing-config");

    let output = prx_command(&config_dir, "status");

    assert!(!output.status.success());
    assert!(
        !config_dir.exists(),
        "status must not create a missing config directory"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("existing config is required"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn verify_does_not_initialize_missing_workspace_or_database() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    let workspace = config_dir.join("workspace");

    let output = prx_command(&config_dir, "verify");

    assert!(!output.status.success());
    assert!(!workspace.exists(), "verify must not create the workspace or database");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authoritative memory database is missing"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn baseline_is_a_read_only_compatibility_error() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    let workspace = config_dir.join("workspace");

    let output = prx_command(&config_dir, "baseline");

    assert!(!output.status.success());
    assert!(
        !workspace.exists(),
        "baseline compatibility handling must not create state"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline` is disabled"), "unexpected stderr: {stderr}");
}

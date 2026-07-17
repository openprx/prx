use openprx::config::Config;
use std::process::Command;
use tempfile::TempDir;

fn run_doctor(config_dir: &std::path::Path, doctor_args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(config_dir)
        .arg("doctor")
        .args(doctor_args)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .output()
}

#[test]
fn doctor_does_not_initialize_missing_config_directory() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("missing-config");

    let output = run_doctor(&config_dir, &[]).unwrap();

    assert!(!output.status.success());
    assert!(
        !config_dir.exists(),
        "doctor must not create a missing config directory"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("existing config is required"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn doctor_errors_exit_nonzero_without_initializing_workspace() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    let workspace = config_dir.join("workspace");

    let output = run_doctor(&config_dir, &[]).unwrap();

    assert!(!output.status.success(), "ERROR findings must produce a nonzero exit");
    assert!(!workspace.exists(), "doctor must not initialize the missing workspace");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[UNKNOWN]"),
        "typed state missing from stdout: {stdout}"
    );
    assert!(stdout.contains("errors"), "summary missing from stdout: {stdout}");
}

#[test]
fn doctor_runtime_does_not_create_memory_database() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    let db_path = workspace.join("memory").join("brain.db");

    let output = run_doctor(&config_dir, &["runtime"]).unwrap();

    assert!(
        !output.status.success(),
        "missing runtime state and memory ledger must be errors"
    );
    assert!(!db_path.exists(), "doctor runtime must not create brain.db");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("read-only sqlite backend probe failed"));
}

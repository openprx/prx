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

fn custom_slash_model_price_toml(price_key: &str) -> String {
    format!(
        "default_temperature = 0.7\n\
         default_provider = \"custom:http://127.0.0.1:9/v1\"\n\
         default_model = \"moonshotai/kimi-k2.5\"\n\
         \n\
         [cost.prices.{price_key:?}]\n\
         input = 1.0\n\
         output = 2.0\n\
         cache_write = 1.0\n\
         cache_read = 0.5\n"
    )
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
fn doctor_process_warns_when_a_cost_price_key_matches_no_configured_model() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        custom_slash_model_price_toml("custom:http://127.0.0.1:9/v1/moonshotai/kimi-k2.5"),
    )
    .unwrap();

    let output = run_doctor(&config_dir, &[]).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("[cost]"),
        "doctor stdout missing [cost] section: {stdout}"
    );
    assert!(
        stdout.contains("matches no configured provider/model"),
        "doctor stdout missing unmatched-key warning: {stdout}"
    );
    assert!(
        stdout.contains("the expected key is \"moonshotai/kimi-k2.5\""),
        "doctor stdout missing expected key: {stdout}"
    );
    assert!(
        stderr.contains("cost.prices key matches no configured model"),
        "load-path warning missing from stderr: {stderr}"
    );
}

#[test]
fn doctor_process_is_silent_on_cost_when_the_price_key_matches() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        custom_slash_model_price_toml("moonshotai/kimi-k2.5"),
    )
    .unwrap();

    let output = run_doctor(&config_dir, &[]).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("matches no configured provider/model"),
        "matching key must not warn in doctor stdout: {stdout}"
    );
    assert!(
        !stderr.contains("cost.prices key matches no configured model"),
        "matching key must not warn on load: {stderr}"
    );
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

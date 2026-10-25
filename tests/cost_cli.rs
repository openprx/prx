use openprx::config::Config;
use std::path::Path;
use std::process::{Command, Output};

fn run(config_dir: &Path) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(config_dir)
        .args(["cost", "--json"])
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .output()
}

#[test]
fn cost_json_is_clean_and_never_initializes_or_repairs_state() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");
    assert!(!run(&missing).unwrap().status.success());
    assert!(!missing.exists());
    let config_dir = temp.path().join("config");
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_path = config_dir.join("config.toml");
    let config_text = toml::to_string_pretty(&Config::default()).unwrap();
    std::fs::write(&config_path, &config_text).unwrap();
    let output = run(&config_dir).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout contains JSON only");
    assert_eq!(
        report.pointer("/all_time/requests").and_then(serde_json::Value::as_u64),
        Some(0)
    );
    assert_eq!(
        report
            .get("unsettled_dispatched_requests")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), config_text);
    let state = workspace.join("state");
    std::fs::create_dir(&state).unwrap();
    let ledger = state.join("costs.jsonl");
    std::fs::write(&ledger, "damaged\n").unwrap();
    let output = run(&config_dir).unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, [] as [u8; 0]);
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), "damaged\n");
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
}

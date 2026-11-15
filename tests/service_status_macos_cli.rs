#![cfg(target_os = "macos")]
//! `prx service status` on macOS queries only the PRX launchd job through the
//! real `launchctl` and prints its state, PID and last exit status.

use openprx::config::Config;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const STATUS_DEADLINE: Duration = Duration::from_secs(30);
const OUTPUT_CAP_BYTES: usize = 4096;

#[test]
fn status_names_only_the_prx_job() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let config_dir = temp.path().join("config");
    let home = temp.path().join("home");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&Config::default())?,
    )?;

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(&config_dir)
        .args(["service", "status"])
        .env("HOME", &home)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .output()?;
    assert!(started.elapsed() < STATUS_DEADLINE, "service status took too long");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.len() < OUTPUT_CAP_BYTES, "status output is a dump: {stdout}");
    assert!(stdout.contains("Manager: launchd"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("com.prx.daemon.plist"), "unexpected stdout: {stdout}");
    for line in stdout.lines() {
        assert!(
            ["Service state: ", "Manager: ", "Unit: ", "Detail: "]
                .iter()
                .any(|prefix| line.starts_with(prefix)),
            "unexpected status line: {line}"
        );
    }
    for text in [&stdout, &stderr] {
        assert!(!text.contains("com.apple."), "another launchd job leaked: {text}");
    }
    let detail = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Detail: "))
        .unwrap_or_default();
    assert!(
        detail.starts_with("pid=")
            || detail.starts_with("not running; last exit status=")
            || detail == "com.prx.daemon is not loaded",
        "unexpected detail: {detail}"
    );
    Ok(())
}

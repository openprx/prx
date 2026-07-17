#![cfg(target_os = "linux")]

use openprx::config::Config;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

struct ServiceFixture {
    _temp: TempDir,
    config_dir: std::path::PathBuf,
    home: std::path::PathBuf,
    bin_dir: std::path::PathBuf,
}

impl ServiceFixture {
    fn new(systemctl_script: &str, unit_exists: bool) -> anyhow::Result<Self> {
        let temp = TempDir::new()?;
        let config_dir = temp.path().join("config");
        let home = temp.path().join("home");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&bin_dir)?;
        std::fs::write(
            config_dir.join("config.toml"),
            toml::to_string_pretty(&Config::default())?,
        )?;

        let systemctl = bin_dir.join("systemctl");
        std::fs::write(&systemctl, systemctl_script)?;
        let mut permissions = std::fs::metadata(&systemctl)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&systemctl, permissions)?;

        if unit_exists {
            let unit = home.join(".config/systemd/user/prx.service");
            let parent = unit
                .parent()
                .ok_or_else(|| anyhow::anyhow!("systemd unit path has no parent"))?;
            std::fs::create_dir_all(parent)?;
            std::fs::write(unit, "existing unit")?;
        }

        Ok(Self {
            _temp: temp,
            config_dir,
            home,
            bin_dir,
        })
    }

    fn run(&self, subcommand: &str) -> std::io::Result<std::process::Output> {
        let path = format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_prx"))
            .arg("--config-dir")
            .arg(&self.config_dir)
            .args(["service", "--service-init", "systemd", subcommand])
            .env("HOME", &self.home)
            .env("PATH", path)
            .env_remove("OPENPRX_CONFIG_DIR")
            .env_remove("OPENPRX_WORKSPACE")
            .output()
    }
}

#[test]
fn status_is_structured_and_nonzero_when_stopped() -> anyhow::Result<()> {
    let fixture = ServiceFixture::new("#!/bin/sh\necho inactive\nexit 3\n", true)?;

    let output = fixture.run("status")?;

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Service state: stopped"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("Manager: systemd-user"));
    Ok(())
}

#[test]
fn stop_propagates_manager_failure_without_success_claim() -> anyhow::Result<()> {
    let fixture = ServiceFixture::new("#!/bin/sh\necho stop-failed >&2\nexit 17\n", true)?;

    let output = fixture.run("stop")?;

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Service stopped"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("stop-failed"));
    Ok(())
}

#[test]
fn install_propagates_enable_failure_without_success_claim() -> anyhow::Result<()> {
    let fixture = ServiceFixture::new(
        "#!/bin/sh\ncase \"$*\" in\n  *enable*) echo enable-failed >&2; exit 19 ;;\n  *) exit 0 ;;\nesac\n",
        false,
    )?;

    let output = fixture.run("install")?;

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Installed systemd"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("enable-failed"));
    let unit = std::fs::read_to_string(fixture.home.join(".config/systemd/user/prx.service"))?;
    assert!(unit.contains(&format!("--config-dir \"{}\"", fixture.config_dir.display())));
    Ok(())
}

#[test]
fn uninstall_propagates_reload_failure_without_success_claim() -> anyhow::Result<()> {
    let fixture = ServiceFixture::new(
        "#!/bin/sh\ncase \"$*\" in\n  *stop*) exit 0 ;;\n  *daemon-reload*) echo reload-failed >&2; exit 23 ;;\n  *) exit 0 ;;\nesac\n",
        true,
    )?;

    let output = fixture.run("uninstall")?;

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Service uninstalled"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reload-failed"));
    Ok(())
}

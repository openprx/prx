//! Initialization contracts through the built CLI, using isolated targets.
#![cfg(test)]
use std::process::{Command, Output, Stdio};

fn invoke(args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(args)
        .env("HOME", home.path())
        .env_remove("OPENPRX_WORKSPACE")
        .stdin(Stdio::null())
        .output()
        .expect("CLI process")
}

#[test]
fn init_exposes_full_setup_and_removes_the_old_command() {
    let help = invoke(&["init", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--interactive",
        "--channels-only",
        "--provider",
        "--model",
        "--from-env",
        "--decision-credential-stdin",
    ] {
        assert!(help.contains(flag), "missing {flag}");
    }
    assert!(!invoke(&["onboard", "--help"]).status.success());
}

#[test]
fn incompatible_setup_modes_fail_without_creating_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("configuration");
    let path = path.to_str().unwrap();
    for flags in [
        vec!["--interactive", "--provider", "openai"],
        vec!["--channels-only", "--force"],
        vec!["--channels-only"],
    ] {
        let mut args = vec!["init", "--dir", path];
        args.extend(flags);
        let output = invoke(&args);
        assert!(!output.status.success());
        assert!(!std::path::Path::new(path).join("config.toml").exists());
    }
}

#[test]
fn failed_quick_setup_does_not_select_an_uncommitted_workspace() {
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("candidate");
    let output = Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(["init", "--dir"])
        .arg(&target)
        .args(["--from-env", "PRX_MISSING_INIT_CREDENTIAL"])
        .env("HOME", home.path())
        .env_remove("OPENPRX_WORKSPACE")
        .env_remove("PRX_MISSING_INIT_CREDENTIAL")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!target.join("config.toml").exists());
    assert!(!home.path().join(".openprx/active_workspace.toml").exists());
}

#[test]
fn explicit_target_and_force_preserve_custom_identity_and_agents() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("configuration");
    let target = path.to_str().unwrap();
    let first = invoke(&["init", "--dir", target]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let soul = path.join("workspace/identities/main/SOUL.md");
    std::fs::write(&soul, "Operator identity sentinel").unwrap();
    let agent_file = path.join("config.d/agent.toml");
    let mut agent = std::fs::read_to_string(&agent_file).unwrap();
    agent.push_str("\n[agents.retained]\nprovider = \"openrouter\"\nmodel = \"openai/gpt-5-mini\"\nidentity_dir = \"identities/retained\"\n");
    std::fs::write(&agent_file, agent).unwrap();
    let refused = invoke(&["init", "--dir", target, "--provider", "openrouter"]);
    assert!(!refused.status.success());
    let repaired = invoke(&[
        "init",
        "--dir",
        target,
        "--force",
        "--provider",
        "openrouter",
        "--model",
        "openai/gpt-5-mini",
    ]);
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert_eq!(std::fs::read_to_string(soul).unwrap(), "Operator identity sentinel");
    let agent = std::fs::read_to_string(agent_file).unwrap();
    assert!(agent.contains("[agents.retained]"));
    assert!(agent.contains("identities/retained"));
}

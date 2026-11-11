//! Actual CLI, REPL, plain chat, task child and process-worker discovery.
#[path = "support/tool_search_wire.rs"]
mod wire;
use anyhow::{Context, Result, ensure};
use openprx::config::{Config, schema::ModelPricing};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use wire::{MODEL, WireProbe};

#[derive(Clone, Copy, Debug)]
enum Root {
    Single,
    Repl,
    PlainChat,
    TaskChild,
    ProcessWorker,
}
const ROOTS: &[Root] = &[
    Root::Single,
    Root::Repl,
    Root::PlainChat,
    Root::TaskChild,
    Root::ProcessWorker,
];
const PARENT_MODEL: &str = "prx-parent-probe";

fn config(
    workspace: &Path,
    config_dir: &Path,
    probe: &WireProbe,
    advice: bool,
    native: bool,
    child: bool,
) -> Result<Config> {
    let mut config = Config {
        workspace_dir: workspace.to_owned(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some(probe.provider()),
        default_model: Some(if child { PARENT_MODEL } else { MODEL }.to_owned()),
        api_key: Some("prx-wire-credential".to_owned()),
        api_url: Some(probe.endpoint().to_owned()),
        ..Config::default()
    };
    config.auth.codex_auth_json_auto_import = false;
    config.skills.open_skills_dir = Some(
        config
            .workspace_dir
            .join("missing-open-skills")
            .to_string_lossy()
            .into_owned(),
    );
    config.skills.openclaw_skills_dir = Some(
        config
            .workspace_dir
            .join("missing-openclaw-skills")
            .to_string_lossy()
            .into_owned(),
    );
    "none".clone_into(&mut config.memory.embedding_provider);
    config.memory.auto_save = false;
    config.reliability.provider_retries = 0;
    config.reliability.scheduler_retries = 0;
    config.decision_engine.enabled = advice;
    config.autonomy.level = openprx::security::AutonomyLevel::Full;
    for (model, native) in std::iter::once((MODEL, native)).chain(child.then_some((PARENT_MODEL, true))) {
        config.providers.models.push(serde_json::from_value(serde_json::json!({
            "provider":probe.provider(),"model":model,"operations":["chat"],"native_tools":native,
        }))?);
        config.cost.prices.insert(
            format!("{}/{model}", probe.provider()),
            ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
    }
    ensure!(
        config.tool_tiering.always_include.is_empty(),
        "no test-only direct override"
    );
    Ok(config)
}

fn command(config_dir: &Path, workspace: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config_dir)
        .env_clear()
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", home)
        .env("OPENPRX_CONFIG_DIR", config_dir)
        .env("OPENPRX_WORKSPACE", workspace)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("PRX_TUI", "0")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("RUST_LOG", "error");
    command
}

async fn interactive(mut command: Command, input: &str) -> Result<()> {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().context("interactive input")?;
    let stdout = child.stdout.take().context("interactive output")?;
    let mut stderr = child.stderr.take().context("interactive diagnostics")?;
    let diagnostics = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    });
    let result = async {
        stdin.write_all(format!("{input}\n").as_bytes()).await?;
        stdin.flush().await?;
        let mut lines = BufReader::new(stdout).lines();
        let observed = tokio::time::timeout(Duration::from_secs(90), async {
            let mut text = String::new();
            while let Some(line) = lines.next_line().await? {
                text.push_str(&line);
                text.push('\n');
                if text.contains(wire::DONE) {
                    return Ok::<_, anyhow::Error>(());
                }
                ensure!(text.len() < 1024 * 1024, "unexpected interactive output size");
            }
            anyhow::bail!("process ended before the real probe completed: {text}")
        })
        .await?;
        observed?;
        stdin.write_all(b"/quit\n").await?;
        stdin.flush().await?;
        drop(stdin);
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait()).await??;
        ensure!(status.success(), "interactive process failed: {status}");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let bytes = diagnostics.await??;
    result.with_context(|| format!("interactive diagnostics: {}", String::from_utf8_lossy(&bytes)))
}

async fn drive(root: Root, input: &str, advice: bool, skill: bool, native: bool) -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let config_dir = scratch.path().join("config");
    let workspace = config_dir.join("workspace");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&home)?;
    if skill {
        let path = workspace.join("skills/prx_probe");
        std::fs::create_dir_all(&path)?;
        std::fs::write(
            path.join("SKILL.md"),
            format!(
                "---\nname: prx_probe\ndescription: Read project probe instructions\n---\n{}\n",
                wire::INSTRUCTIONS
            ),
        )?;
    }
    let mode = match root {
        Root::TaskChild => Some("task"),
        Root::ProcessWorker => Some("process"),
        _ => None,
    };
    let probe = WireProbe::start(workspace.clone(), skill, mode).await?;
    if !native {
        probe.require_no_tools();
    }
    let config = config(&workspace, &config_dir, &probe, advice, native, mode.is_some())?;
    std::fs::write(config_dir.join("config.toml"), toml::to_string_pretty(&config)?)?;
    let mut command = command(&config_dir, &workspace, &home);
    let result = if matches!(root, Root::Repl | Root::PlainChat) {
        if matches!(root, Root::PlainChat) {
            command.args(["chat", "--plain"]);
        } else {
            command.arg("agent");
        }
        interactive(command, input).await
    } else {
        let source = if mode.is_some() {
            "PRX_PARENT sessions_spawn: use memory_get capability_search skill_read prx_probe in a child"
        } else {
            input
        };
        command.args(["agent", "-m", source]);
        let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await??;
        ensure!(
            output.status.success(),
            "{root:?} process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let marker = if mode.is_some() {
            "PRX_PARENT_FINISHED"
        } else {
            wire::DONE
        };
        ensure!(
            String::from_utf8_lossy(&output.stdout).contains(marker),
            "{root:?} did not complete; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    let wire = probe.finish().await;
    result.with_context(|| format!("{root:?} advice={advice} native={native}"))?;
    wire.with_context(|| format!("{root:?} actual provider wire"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_off_search_discovers_noncore_tool_on_all_roots() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, wire::INPUT, advice, false, true).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn brief_input_keeps_available_search() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, false, true).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_skill_metadata_is_readable_on_first_turn() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, true, true).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_tool_models_do_not_receive_search_or_unreadable_skill_metadata() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, true, false).await?;
        }
    }
    Ok(())
}

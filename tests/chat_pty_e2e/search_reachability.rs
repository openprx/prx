//! Real fullscreen chat + real HTTP provider; reuses the owned PTY harness.
use super::*;
use anyhow::{Result, ensure};
#[path = "../support/tool_search_wire.rs"]
mod wire;
use wire::{MODEL, WireProbe};

async fn drive(input: &str, advice: bool, skill: bool, native: bool) -> Result<()> {
    let guard = new_harness_guard()?;
    if skill {
        let path = guard.workspace_dir.join("skills/prx_probe");
        std::fs::create_dir_all(&path)?;
        std::fs::write(
            path.join("SKILL.md"),
            format!(
                "---\nname: prx_probe\ndescription: Read project probe instructions\n---\n{}\n",
                wire::INSTRUCTIONS
            ),
        )?;
    }
    let probe = WireProbe::start(guard.workspace_dir.clone(), skill, None).await?;
    if !native {
        probe.require_no_tools();
    }
    let mut config = openprx::config::Config {
        workspace_dir: guard.workspace_dir.clone(),
        config_path: guard.config_dir.join("config.toml"),
        default_provider: Some(probe.provider()),
        default_model: Some(MODEL.to_owned()),
        api_key: Some("prx-wire-credential".to_owned()),
        api_url: Some(probe.endpoint().to_owned()),
        ..openprx::config::Config::default()
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
    config.autonomy.level = openprx::security::AutonomyLevel::Full;
    config.decision_engine.enabled = advice;
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider":probe.provider(),"model":MODEL,"operations":["chat"],"native_tools":native,
    }))?);
    config.cost.prices.insert(
        format!("{}/{MODEL}", probe.provider()),
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    std::fs::write(&config.config_path, toml::to_string_pretty(&config)?)?;
    let mut command = Command::new(prx_binary());
    command
        .arg("--config-dir")
        .arg(&guard.config_dir)
        .arg("chat")
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", &guard.home_dir)
        .env("OPENPRX_CONFIG_DIR", &guard.config_dir)
        .env("OPENPRX_WORKSPACE", &guard.workspace_dir)
        .env("XDG_DATA_HOME", &guard.xdg_data)
        .env("XDG_CACHE_HOME", &guard.xdg_cache)
        .env("XDG_STATE_HOME", &guard.xdg_state)
        .env("XDG_CONFIG_HOME", &guard.xdg_config)
        .env("PRX_TUI", "1")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("RUST_LOG", "error");
    let input = input.to_owned();
    let work = tokio::task::spawn_blocking(move || -> Result<()> {
        let _scratch = guard;
        let mut owner = SessionGuard::new(spawn_chat_command(command));
        let session = owner.session();
        read_until_with_dsr(session, MODEL, STARTUP_TIMEOUT);
        session.send(format!("{input}\r"))?;
        read_until_with_dsr(session, wire::DONE, TURN_TIMEOUT);
        session.send("/quit\r")?;
        ensure!(
            wait_for_exit(session, EXIT_TIMEOUT),
            "fullscreen chat did not exit after completed probe"
        );
        ensure!(
            matches!(session.reaped.as_ref(), Some(expectrl::WaitStatus::Exited(_, 0))),
            "fullscreen chat did not exit successfully"
        );
        Ok(())
    })
    .await?;
    let wire = probe.finish().await;
    work?;
    wire
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(prx_chat_pty)]
async fn engine_off_search_discovers_noncore_tool_on_all_roots() -> Result<()> {
    for advice in [false, true] {
        drive(wire::INPUT, advice, false, true).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(prx_chat_pty)]
async fn brief_input_keeps_available_search() -> Result<()> {
    for advice in [false, true] {
        drive("hi", advice, false, true).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(prx_chat_pty)]
async fn installed_skill_metadata_is_readable_on_first_turn() -> Result<()> {
    for advice in [false, true] {
        drive("hi", advice, true, true).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(prx_chat_pty)]
async fn non_tool_models_do_not_receive_search_or_unreadable_skill_metadata() -> Result<()> {
    for advice in [false, true] {
        drive("hi", advice, true, false).await?;
    }
    Ok(())
}

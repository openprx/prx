#![cfg(test)]
//! Source admission and actual HTTP provider requests, with real backend reads.
#[path = "../../../tests/support/tool_search_wire.rs"]
mod wire;
use crate::{config::Config, memory::Memory, providers::Provider, tools::Tool};
use anyhow::{Context, Result, ensure};
use std::sync::Arc;
use wire::{MODEL, WireProbe};

#[derive(Clone, Copy, Debug)]
enum Root {
    Embedded,
    Chat,
    Channel,
    Webhook,
    Console,
    Cron,
    Xin,
}
const ROOTS: &[Root] = &[
    Root::Embedded,
    Root::Chat,
    Root::Channel,
    Root::Webhook,
    Root::Console,
    Root::Cron,
    Root::Xin,
];

#[inline(never)]
pub(crate) fn boxed_root_probe<F, Fut>(make: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(make())
}

pub(crate) fn provider(config: &Config) -> Result<Box<dyn Provider>> {
    let name = config.default_provider.as_deref().context("wire provider selection")?;
    let endpoint = name.strip_prefix("custom:").context("loopback provider kind")?;
    Ok(Box::new(
        crate::providers::compatible::OpenAiCompatibleProvider::new_optional_credential(
            name,
            endpoint,
            config.api_key.as_deref(),
            crate::providers::compatible::AuthStyle::Bearer,
        ),
    ))
}

pub(crate) fn registry(config: &Config, memory: Arc<dyn Memory>) -> Arc<Vec<Box<dyn Tool>>> {
    crate::agent::test_admission::registry_with_search(vec![
        Box::new(crate::tools::memory_get::MemoryGetTool::new(
            config.workspace_dir.clone(),
            memory,
            false,
        )),
        Box::new(crate::tools::SkillReadTool::new(
            config.workspace_dir.clone(),
            config.clone(),
        )),
    ])
}

fn config(workspace: &std::path::Path, probe: &WireProbe, advice: bool, native: bool) -> Result<Config> {
    let mut config = Config {
        workspace_dir: workspace.to_owned(),
        config_path: workspace.join("config.toml"),
        default_provider: Some(probe.provider()),
        default_model: Some(MODEL.to_owned()),
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
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider":probe.provider(),"model":MODEL,"operations":["chat"],"native_tools":native,
    }))?);
    config.cost.prices.insert(
        format!("{}/{MODEL}", probe.provider()),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    ensure!(
        config.tool_tiering.always_include.is_empty(),
        "no test-only direct overrides"
    );
    Ok(config)
}

async fn embedded(config: Config, input: &str) -> Result<()> {
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&config.workspace_dir)?);
    let registry = registry(&config, Arc::clone(&memory));
    let mut agent = crate::agent::agent::Agent::builder()
        .runtime_config(config.clone(), config.default_provider.as_deref().context("provider")?)
        .workspace_dir(config.workspace_dir.clone())
        .model_name(MODEL.to_owned())
        .provider(provider(&config)?)
        .tools(Arc::try_unwrap(registry).map_err(|_| anyhow::anyhow!("unshared registry"))?)
        .memory(memory)
        .observer(Arc::new(crate::observability::NoopObserver))
        .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
        .build()?;
    let result = agent.turn(input).await?;
    ensure!(result.contains(wire::DONE), "embedded probe did not complete: {result}");
    Ok(())
}

async fn drive(root: Root, input: &str, advice: bool, skill: bool, native: bool) -> Result<()> {
    let workspace = tempfile::tempdir()?;
    if skill {
        let dir = workspace.path().join("skills/prx_probe");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: prx_probe\ndescription: Read project probe instructions\n---\n{}\n",
                wire::INSTRUCTIONS
            ),
        )?;
    }
    let probe = WireProbe::start(
        workspace.path().to_owned(),
        skill.then_some(!matches!(root, Root::Console)),
        None,
    )
    .await?;
    if !native {
        probe.require_no_tools();
    }
    let config = config(workspace.path(), &probe, advice, native)?;
    let run = match root {
        Root::Embedded => boxed_root_probe(|| embedded(config, input)).await,
        Root::Chat => boxed_root_probe(|| crate::chat::dispatcher::drive_search_chat_entry(config, input)).await,
        Root::Channel => boxed_root_probe(|| crate::channels::drive_search_channel_entry(config, input)).await,
        Root::Webhook => boxed_root_probe(|| crate::gateway::drive_search_webhook_entry(config, input)).await,
        Root::Console => boxed_root_probe(|| crate::gateway::drive_search_console_entry(config, input)).await,
        Root::Cron => boxed_root_probe(|| crate::cron::scheduler::drive_search_cron_entry(config, input)).await,
        Root::Xin => boxed_root_probe(|| crate::xin::runner::drive_search_xin_entry(config, input)).await,
    };
    run.with_context(|| format!("{root:?} advice={advice} native={native}"))?;
    probe.finish().await.with_context(|| format!("{root:?} actual wire"))
}

#[tokio::test]
async fn engine_off_search_discovers_noncore_tool_on_all_roots() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, wire::INPUT, advice, false, true).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn brief_input_keeps_available_search() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, false, true).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn installed_skill_metadata_is_readable_on_first_turn() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, true, true).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn non_tool_models_do_not_receive_search_or_unreadable_skill_metadata() -> Result<()> {
    for root in ROOTS {
        for advice in [false, true] {
            drive(*root, "hi", advice, true, false).await?;
        }
    }
    Ok(())
}

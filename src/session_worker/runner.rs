use crate::agent::loop_::run_tool_call_loop;
use crate::channels::build_identity_prompt;
use crate::config::Config;
use crate::hooks::HookManager;
use crate::memory::Memory;
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::tools::{self, Tool};
use anyhow::{Context, Result};
use std::io::Write;
use std::sync::Arc;

const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

fn write_worker_result(result: &WorkerResult) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let json = serde_json::to_string(result).context("serialize worker result")?;
    stdout
        .write_all(json.as_bytes())
        .context("write worker result")?;
    stdout.write_all(b"\n").context("write worker newline")?;
    stdout.flush().context("flush worker stdout")?;
    Ok(())
}

fn select_tools_for_worker(
    source: Vec<Box<dyn Tool>>,
    allowed_tools: &[String],
) -> Result<Vec<Box<dyn Tool>>> {
    if allowed_tools.is_empty() {
        return Ok(source);
    }

    let mut selected = Vec::new();
    let mut remaining = source;

    for allowed in allowed_tools {
        let allowed = allowed.trim();
        if allowed.is_empty() {
            continue;
        }

        if let Some(index) = remaining
            .iter()
            .position(|tool| tool.name() == allowed || tool.supports_name(allowed))
        {
            selected.push(remaining.remove(index));
        } else {
            anyhow::bail!("Allowed tool '{allowed}' is not registered in worker process");
        }
    }

    Ok(selected)
}

fn resolve_system_prompt(manifest: &WorkerManifest) -> String {
    if let Some(prompt) = manifest
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return prompt.to_string();
    }

    if let Some(identity_dir) = manifest
        .identity_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let prompt = build_identity_prompt(&manifest.workspace_dir.join(identity_dir));
        if !prompt.trim().is_empty() {
            return prompt;
        }
    }

    DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string()
}

fn parse_tools_override(tools_json: &str) -> Result<Vec<String>> {
    serde_json::from_str(tools_json).with_context(|| "parse --tools JSON as string array")
}

async fn run_manifest(manifest: WorkerManifest) -> Result<WorkerResult> {
    let mut config = Config::load_or_init().await?;
    config.apply_env_overrides();
    config.workspace_dir = manifest.workspace_dir.clone();

    let provider_runtime_options = crate::providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };

    let provider: Arc<dyn Provider> =
        Arc::from(crate::providers::create_resilient_provider_with_options(
            &manifest.provider_name,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
            &provider_runtime_options,
        )?);

    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(
        manifest.memory_db_path.clone(),
        config.memory.acl_enabled,
    )?);

    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &manifest.workspace_dir,
    ));
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);

    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    let full_tools = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        memory,
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &manifest.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );

    let tools_registry = select_tools_for_worker(full_tools, &manifest.allowed_tools)?;
    let system_prompt = resolve_system_prompt(&manifest);

    let run_future = async {
        let mut history = vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(manifest.task.clone()),
        ];

        let observer = NoopObserver;
        let hooks = HookManager::new(manifest.workspace_dir.clone());
        run_tool_call_loop(
            provider.as_ref(),
            &mut history,
            tools_registry.as_slice(),
            &observer,
            &hooks,
            &manifest.provider_name,
            &manifest.model,
            manifest.temperature,
            true,
            None,
            "session-worker",
            &config.multimodal,
            config.agent.max_tool_iterations,
            None,
            None,
            None,
        )
        .await
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(manifest.timeout_seconds),
        run_future,
    )
    .await
    {
        Ok(Ok(output)) => Ok(WorkerResult {
            success: true,
            output: if output.trim().is_empty() {
                "[Sub-agent produced no output]".to_string()
            } else {
                output
            },
            error: None,
        }),
        Ok(Err(error)) => Ok(WorkerResult {
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
        }),
        Err(_) => Ok(WorkerResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "Sub-agent timed out after {}s",
                manifest.timeout_seconds
            )),
        }),
    }
}

pub async fn run_from_stdin(
    task: Option<String>,
    workspace: Option<String>,
    memory_db: Option<String>,
    tools: Option<String>,
    timeout: Option<u64>,
) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .read_line(&mut raw)
        .context("read worker manifest from stdin")?;

    let mut manifest: WorkerManifest = match serde_json::from_str(raw.trim()) {
        Ok(value) => value,
        Err(error) => {
            let result = WorkerResult {
                success: false,
                output: String::new(),
                error: Some(format!("Invalid worker manifest JSON: {error}")),
            };
            write_worker_result(&result)?;
            return Ok(());
        }
    };

    if let Some(task) = task {
        manifest.task = task;
    }
    if let Some(workspace) = workspace {
        manifest.workspace_dir = std::path::PathBuf::from(workspace);
    }
    if let Some(memory_db) = memory_db {
        manifest.memory_db_path = std::path::PathBuf::from(memory_db);
    }
    if let Some(timeout) = timeout {
        manifest.timeout_seconds = timeout;
    }
    if let Some(tools_json) = tools {
        manifest.allowed_tools = parse_tools_override(&tools_json)?;
    }

    let result = match run_manifest(manifest).await {
        Ok(result) => result,
        Err(error) => WorkerResult {
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
        },
    };

    write_worker_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_override_accepts_string_array() {
        let parsed = parse_tools_override(r#"["shell","file_read"]"#).unwrap();
        assert_eq!(parsed, vec!["shell".to_string(), "file_read".to_string()]);
    }

    #[test]
    fn parse_tools_override_rejects_invalid_json_shape() {
        let error = parse_tools_override(r#"{"tool":"shell"}"#).unwrap_err();
        assert!(error
            .to_string()
            .contains("parse --tools JSON as string array"));
    }
}

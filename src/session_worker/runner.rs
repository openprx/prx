use crate::agent::loop_::{
    ScopeContext, ToolConcurrencyGovernanceConfig, build_context_with_shared_events, run_tool_call_loop,
};
use crate::channels::build_identity_prompt;
use crate::config::Config;
use crate::hooks::HookManager;
use crate::memory::{
    Memory, MemoryCategory, MemoryFabric, MemoryPrincipal, MemoryVisibility, MessageEvent, MessageEventScope,
};
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::tools::sessions_spawn::{SPAWN_EXECUTION_CONTEXT, SpawnExecutionContext};
use crate::tools::{self, Tool};
use anyhow::{Context, Result};
use std::future::Future;
use std::io::Write;
use std::sync::Arc;

const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

fn write_worker_result(result: &WorkerResult) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let json = serde_json::to_string(result).context("serialize worker result")?;
    stdout.write_all(json.as_bytes()).context("write worker result")?;
    stdout.write_all(b"\n").context("write worker newline")?;
    stdout.flush().context("flush worker stdout")?;
    Ok(())
}

fn select_tools_for_worker(source: Vec<Box<dyn Tool>>, allowed_tools: &[String]) -> Result<Vec<Box<dyn Tool>>> {
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
    config.workspace_dir = manifest.workspace_dir.clone();

    let provider_runtime_options = crate::providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
    };

    let provider: Arc<dyn Provider> = Arc::from(crate::providers::create_resilient_provider_with_options(
        &manifest.provider_name,
        manifest.api_key.as_deref().or(config.api_key.as_deref()),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    )?);

    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(
        manifest.memory_db_path.clone(),
        config.memory.acl_enabled,
    )?);
    let memory_workspace_id = manifest
        .memory_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string());
    let memory_fabric =
        MemoryFabric::new(memory.clone(), memory_workspace_id).with_event_recording(manifest.memory_event_recording);
    let worker_event_scope = worker_message_event_scope(&manifest);
    if let Err(error) = memory_fabric
        .record_inbound_user_message(
            worker_event_scope.clone(),
            manifest.task.clone(),
            Some(format!("session_worker:{}:request", manifest.run_id)),
            None,
        )
        .await
    {
        tracing::warn!(run_id = %manifest.run_id, "failed to record session-worker request event: {error}");
    }

    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &manifest.workspace_dir));
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);

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
        manifest.api_key.as_deref().or(config.api_key.as_deref()),
        &config,
    );

    let tools_registry = select_tools_for_worker(full_tools, &manifest.allowed_tools)?;
    let system_prompt = resolve_system_prompt(&manifest);
    let shared_context = load_worker_shared_context(&manifest, &config).await;

    let run_future = async {
        let user_task = if shared_context.trim().is_empty() {
            manifest.task.clone()
        } else {
            format!("{shared_context}{}", manifest.task)
        };
        let mut history = vec![ChatMessage::system(system_prompt), ChatMessage::user(user_task)];

        let observer = NoopObserver;
        let hooks = HookManager::new(manifest.workspace_dir.clone());
        let scope_ctx = match (
            manifest.scope_sender.as_deref(),
            manifest.scope_channel.as_deref(),
            manifest.scope_chat_type.as_deref(),
            manifest.scope_chat_id.as_deref(),
        ) {
            (Some(sender), Some(channel), Some(chat_type), Some(chat_id))
                if !sender.is_empty() && !channel.is_empty() && !chat_type.is_empty() && !chat_id.is_empty() =>
            {
                Some(ScopeContext {
                    policy: &security,
                    sender,
                    channel,
                    chat_type,
                    chat_id,
                    policy_pipeline: None,
                })
            }
            _ => None,
        };
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
            manifest.max_iterations.max(1),
            config.agent.parallel_tools,
            config.agent.read_only_tool_concurrency_window,
            config.agent.read_only_tool_timeout_secs,
            config.agent.priority_scheduling_enabled,
            config.agent.low_priority_tools.clone(),
            ToolConcurrencyGovernanceConfig {
                kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
                rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
                rollout_channels: config.agent.concurrency_rollout_channels.clone(),
                auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
                rollback_timeout_rate_threshold: config.agent.concurrency_rollback_timeout_rate_threshold,
                rollback_cancel_rate_threshold: config.agent.concurrency_rollback_cancel_rate_threshold,
                rollback_error_rate_threshold: config.agent.concurrency_rollback_error_rate_threshold,
            },
            manifest.compaction_config.as_ref(),
            None,
            None,
            scope_ctx.as_ref(),
            None,
            Some(&config.tool_tiering),
            crate::agent::loop_::ChatMode::default(),
        )
        .await
    };

    let run_future = with_manifest_spawn_context(&manifest, run_future);

    let result = if manifest.timeout_seconds == 0 {
        // No timeout — run until natural completion (rely on callback)
        run_future.await
    } else {
        match tokio::time::timeout(std::time::Duration::from_secs(manifest.timeout_seconds), run_future).await {
            Ok(r) => r,
            Err(_) => {
                return Ok(WorkerResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Sub-agent timed out after {}s", manifest.timeout_seconds)),
                });
            }
        }
    };

    let worker_result = match result {
        Ok(output) => WorkerResult {
            success: true,
            output: if output.trim().is_empty() {
                "[Sub-agent produced no output]".to_string()
            } else {
                output
            },
            error: None,
        },
        Err(error) => WorkerResult {
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
        },
    };

    let event_content = if worker_result.output.trim().is_empty() {
        worker_result
            .error
            .clone()
            .unwrap_or_else(|| "[session-worker produced no output]".to_string())
    } else {
        worker_result.output.clone()
    };
    let worker_result_event = match memory_fabric
        .record_worker_result(
            worker_event_scope.clone(),
            event_content.clone(),
            Some(
                serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error
                })
                .to_string(),
            ),
        )
        .await
    {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(run_id = %manifest.run_id, "failed to record session-worker result event: {error}");
            None
        }
    };

    record_hybrid_worker_draft_if_needed(
        &manifest,
        &config,
        &memory_fabric,
        &worker_event_scope,
        &worker_result,
        worker_result_event.as_ref(),
        &event_content,
    )
    .await;

    Ok(worker_result)
}

async fn record_hybrid_worker_draft_if_needed(
    manifest: &WorkerManifest,
    config: &Config,
    memory_fabric: &MemoryFabric,
    worker_event_scope: &MessageEventScope,
    worker_result: &WorkerResult,
    worker_result_event: Option<&MessageEvent>,
    event_content: &str,
) {
    if manifest.memory_strategy.as_deref() != Some("hybrid") || !worker_result.success {
        return;
    }

    let draft_key = format!("worker_result:{}", manifest.run_id);
    match memory_fabric
        .create_worker_memory_draft(
            worker_event_scope,
            &manifest.run_id,
            &draft_key,
            event_content,
            MemoryCategory::Conversation,
            worker_result_event.map(|event| event.event_id.as_str()),
            Some(
                serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "merge_policy": "parent_decides"
                })
                .to_string(),
            ),
        )
        .await
    {
        Ok(draft) => {
            if let Some(shared_db_path) = manifest.shared_memory_db_path.as_ref() {
                match crate::memory::SqliteMemory::new_with_path_and_acl(
                    shared_db_path.clone(),
                    config.memory.acl_enabled,
                ) {
                    Ok(shared_memory) => {
                        let shared_workspace_id = shared_worker_workspace_id(manifest);
                        let shared_fabric = MemoryFabric::new(Arc::new(shared_memory), shared_workspace_id);
                        if let Err(error) = shared_fabric
                            .record_draft_merge_requested(&draft, Some(shared_fabric.workspace_id()))
                            .await
                        {
                            tracing::warn!(
                                run_id = %manifest.run_id,
                                draft_id = %draft.draft_id,
                                "failed to record hybrid draft merge request: {error}"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            run_id = %manifest.run_id,
                            "failed to open parent shared memory for hybrid draft request: {error}"
                        );
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(run_id = %manifest.run_id, "failed to create hybrid worker memory draft: {error}");
        }
    }
}

fn shared_worker_workspace_id(manifest: &WorkerManifest) -> String {
    manifest
        .shared_memory_db_path
        .as_ref()
        .and_then(|path| path.parent().and_then(std::path::Path::parent))
        .map(|path| path.to_string_lossy().to_string())
        .or_else(|| manifest.memory_workspace_id.clone())
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string())
}

async fn load_worker_shared_context(manifest: &WorkerManifest, config: &Config) -> String {
    let strategy = manifest.memory_strategy.as_deref().unwrap_or("shared_fabric");
    if strategy == "isolated_private" {
        return String::new();
    }

    let db_path = if strategy == "hybrid" {
        manifest
            .shared_memory_db_path
            .as_ref()
            .unwrap_or(&manifest.memory_db_path)
            .clone()
    } else {
        manifest.memory_db_path.clone()
    };
    let workspace_id = if strategy == "hybrid" {
        shared_worker_workspace_id(manifest)
    } else {
        manifest
            .memory_workspace_id
            .clone()
            .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string())
    };

    let shared_memory = match crate::memory::SqliteMemory::new_with_path_and_acl(db_path, config.memory.acl_enabled) {
        Ok(memory) => memory,
        Err(error) => {
            tracing::warn!(run_id = %manifest.run_id, "failed to open shared worker context memory: {error}");
            return String::new();
        }
    };
    build_context_with_shared_events(
        &shared_memory,
        MemoryPrincipal {
            workspace_id,
            agent_id: manifest.agent_id.clone(),
            persona_id: manifest.persona_id.clone(),
            session_key: Some(worker_session_scope_key(manifest).to_string()),
            channel: manifest.scope_channel.clone(),
            sender: manifest.scope_sender.clone(),
        },
        &manifest.task,
        config.memory.min_relevance_score,
    )
    .await
    .preamble
}

fn worker_session_scope_key(manifest: &WorkerManifest) -> &str {
    if manifest.session_scope_key.trim().is_empty() {
        "sessions_spawn:global"
    } else {
        manifest.session_scope_key.as_str()
    }
}

fn worker_message_event_scope(manifest: &WorkerManifest) -> MessageEventScope {
    let mut scope = MessageEventScope::new("session_worker", MemoryVisibility::Workspace)
        .with_channel(manifest.scope_channel.as_deref().unwrap_or("session_worker"))
        .with_session_key(worker_session_scope_key(manifest))
        .with_run_id(manifest.run_id.as_str());
    if let Some(agent_id) = manifest.agent_id.as_deref() {
        scope = scope.with_agent_id(agent_id);
    }
    if let Some(persona_id) = manifest.persona_id.as_deref() {
        scope = scope.with_persona_id(persona_id);
    }
    if let Some(parent_run_id) = manifest.parent_run_id.as_deref() {
        scope = scope.with_parent_run_id(parent_run_id);
    }
    if let Some(sender) = manifest.scope_sender.as_deref() {
        scope = scope.with_sender(sender);
    }
    if let Some(chat_id) = manifest.scope_chat_id.as_deref() {
        scope = scope.with_recipient(chat_id);
    }
    scope
}

async fn with_manifest_spawn_context<T, Fut>(manifest: &WorkerManifest, fut: Fut) -> T
where
    Fut: Future<Output = T>,
{
    if !manifest.session_scope_key.trim().is_empty() {
        SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    run_id: manifest.run_id.clone(),
                    session_scope_key: manifest.session_scope_key.clone(),
                    spawn_depth: manifest.spawn_depth,
                },
                fut,
            )
            .await
    } else {
        fut.await
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
        assert!(error.to_string().contains("parse --tools JSON as string array"));
    }

    #[tokio::test]
    async fn hybrid_worker_shared_context_reads_parent_fabric() {
        let parent = tempfile::TempDir::new().unwrap();
        let worker = tempfile::TempDir::new().unwrap();
        let shared_db = parent.path().join("memory").join("brain.db");
        std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();
        let shared_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db.clone(), false).unwrap();
        shared_memory
            .append_message_event(crate::memory::MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: parent.path().to_string_lossy().to_string(),
                source: "gateway".to_string(),
                channel: Some("webhook".to_string()),
                session_key: Some("gateway:external".to_string()),
                parent_session_key: None,
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: Some("client-a".to_string()),
                recipient: None,
                role: "user".to_string(),
                content: "parent shared context".to_string(),
                raw_payload_json: None,
                visibility: crate::memory::MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        let manifest = WorkerManifest {
            run_id: "run-hybrid".to_string(),
            task: "use context".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker.path().join("brain.db"),
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            shared_memory_db_path: Some(shared_db),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            timeout_seconds: 30,
            max_iterations: 1,
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };
        let context = load_worker_shared_context(&manifest, &Config::default()).await;

        assert!(context.contains("parent shared context"));
    }

    #[tokio::test]
    async fn hybrid_worker_result_creates_private_draft_and_parent_merge_request() {
        let parent = tempfile::TempDir::new().unwrap();
        let worker = tempfile::TempDir::new().unwrap();
        let shared_db = parent.path().join("memory").join("brain.db");
        let worker_db = worker.path().join("brain.db");
        std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();

        let worker_memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(worker_db.clone(), false).unwrap());
        let worker_fabric = MemoryFabric::new(worker_memory.clone(), worker.path().to_string_lossy().to_string());
        let scope = MessageEventScope::new("session_worker", crate::memory::MemoryVisibility::Workspace)
            .with_session_key("telegram:chat-1:alice")
            .with_run_id("run-hybrid")
            .with_parent_run_id("run-parent")
            .with_agent_id("agent-a")
            .with_persona_id("persona-a");
        let result = WorkerResult {
            success: true,
            output: "worker draft content".to_string(),
            error: None,
        };
        let result_event = worker_fabric
            .record_worker_result(scope.clone(), result.output.clone(), None)
            .await
            .unwrap();
        let manifest = WorkerManifest {
            run_id: "run-hybrid".to_string(),
            task: "produce draft".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker_db,
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            shared_memory_db_path: Some(shared_db.clone()),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            timeout_seconds: 30,
            max_iterations: 1,
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };

        record_hybrid_worker_draft_if_needed(
            &manifest,
            &Config::default(),
            &worker_fabric,
            &scope,
            &result,
            Some(&result_event),
            &result.output,
        )
        .await;

        let drafts = worker_memory.list_memory_drafts_for_run("run-hybrid").await.unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = drafts.first();
        assert_eq!(draft.map(|draft| draft.status.as_str()), Some("pending"));
        assert_eq!(draft.map(|draft| draft.content.as_str()), Some("worker draft content"));
        assert_eq!(
            draft.and_then(|draft| draft.source_event_id.as_deref()),
            Some(result_event.event_id.as_str())
        );

        let parent_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db, false).unwrap();
        let parent_events = parent_memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: parent.path().to_string_lossy().to_string(),
                    agent_id: Some("agent-a".to_string()),
                    persona_id: Some("persona-a".to_string()),
                    session_key: Some("telegram:chat-1:alice".to_string()),
                    channel: None,
                    sender: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(parent_events.len(), 1);
        let parent_event = parent_events.first();
        assert_eq!(
            parent_event.map(|event| event.event_type.as_str()),
            Some("memory.draft.merge_requested")
        );
        assert_eq!(
            parent_event.map(|event| event.subject_id.as_str()),
            draft.map(|draft| draft.draft_id.as_str())
        );
        let draft_key = draft.map(|draft| draft.key.as_str()).unwrap_or_default();
        assert!(parent_memory.get(draft_key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn process_mode_restores_spawn_context_for_nested_runs() {
        let manifest = WorkerManifest {
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: std::path::PathBuf::from("/tmp/ws"),
            memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
            memory_workspace_id: Some("/tmp/ws".to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/ws/memory/brain.db")),
            worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            timeout_seconds: 30,
            max_iterations: 1,
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            spawn_depth: 1,
            session_scope_key: "signal:group:test".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };

        let snapshot = with_manifest_spawn_context(&manifest, async {
            crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                .try_with(|ctx| (ctx.run_id.clone(), ctx.session_scope_key.clone(), ctx.spawn_depth))
                .ok()
        })
        .await;
        assert_eq!(
            snapshot,
            Some(("run-child".to_string(), "signal:group:test".to_string(), 1usize))
        );
    }

    #[test]
    fn worker_event_scope_preserves_spawn_lineage() {
        let manifest = WorkerManifest {
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: std::path::PathBuf::from("/tmp/worker"),
            memory_db_path: std::path::PathBuf::from("/tmp/parent/memory/brain.db"),
            memory_workspace_id: Some("/tmp/parent".to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/parent/memory/brain.db")),
            worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            timeout_seconds: 30,
            max_iterations: 1,
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };

        let scope = worker_message_event_scope(&manifest);
        assert_eq!(scope.source, "session_worker");
        assert_eq!(scope.channel.as_deref(), Some("telegram"));
        assert_eq!(scope.session_key.as_deref(), Some("telegram:chat-1:alice"));
        assert_eq!(scope.run_id.as_deref(), Some("run-child"));
        assert_eq!(scope.parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(scope.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(scope.persona_id.as_deref(), Some("persona-a"));
        assert_eq!(scope.sender.as_deref(), Some("alice"));
        assert_eq!(scope.recipient.as_deref(), Some("chat-1"));
    }
}

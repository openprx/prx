mod operator_source;
mod ordinary_source;

use crate::agent::context_recall::build_context_with_shared_events_and_scope;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_engine::run_turn;
use crate::channels::build_identity_prompt;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::config::Config;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryCategory, MemoryFabric, MessageEvent, MessageEventScope};
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::runtime;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::SideEffectGate;
use crate::security::policy::ResourceRiskLevel;
use crate::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult, config_source_generation};
use crate::tools::sessions_spawn::{
    SPAWN_EXECUTION_CONTEXT, STEER_CHANNEL_CAPACITY, SpawnExecutionContext, steering_instruction,
};
use crate::tools::{self, Tool};
use anyhow::{Context, Result};
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

const WORKER_ORCHESTRATION_TOOL_NAMES: &[&str] = &[
    "sessions_list",
    "sessions_send",
    "subagents",
    "sessions_history",
    "session_status",
    "sessions_spawn",
    "message_send",
    "image",
    "config_reload",
    "gateway",
];

/// Process workers do not own an inbound channel connection. Session result
/// announcements remain observable through the parent run registry, so nested
/// workers use a silent channel instead of writing into the stdout IPC stream.
struct WorkerChannel;

#[async_trait::async_trait]
impl Channel for WorkerChannel {
    fn name(&self) -> &str {
        "session-worker"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        anyhow::bail!("session-worker channel does not accept inbound messages")
    }
}

/// Every credential a worker hands out, each paired with the provider it was
/// issued for.
///
/// A worker runs on `manifest.provider_name`, which the parent may have taken
/// from a model's per-call `provider` argument, and it carries at most the
/// credential the parent released for that provider (`manifest.api_key`). The
/// root key, base URL and rotation keys belong to the configured default
/// provider. Each consumer below receives only the credential for the provider
/// it will present it to.
struct WorkerCredentials<'a> {
    /// Sealed credential material, retained only for manifest/config comparison.
    sealed_key: Option<&'a str>,
    /// The worker's own provider connection.
    provider: crate::providers::ProviderConnection<'a>,
    /// Reliability for the worker's own chain; rotation keys follow the root.
    reliability: std::borrow::Cow<'a, crate::config::ReliabilityConfig>,
    /// Fallback for the worker's `sessions_spawn`, which pairs it with
    /// `manifest.provider_name`.
    spawn_fallback: Option<&'a str>,
    /// Root key for the base tool factory, whose `delegate` scopes it to the
    /// configured default provider.
    root_api_key: Option<&'a str>,
}

fn worker_credentials<'a>(manifest: &'a WorkerManifest, config: &'a Config) -> WorkerCredentials<'a> {
    let configured_provider = crate::providers::selection::default_provider(config);
    let uses_root = if manifest.route_hint.is_some() {
        manifest.route_uses_root_connection
    } else {
        manifest.direct_uses_root_connection
    };
    let provider = if uses_root {
        crate::providers::configured_connection_for(
            &manifest.provider_name,
            configured_provider,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
        )
    } else {
        crate::providers::ProviderConnection {
            api_key: manifest.api_key.as_deref().map(str::trim).filter(|key| !key.is_empty()),
            api_url: None,
            released: false,
        }
    };
    WorkerCredentials {
        sealed_key: manifest.api_key.as_deref(),
        reliability: crate::providers::scoped_reliability(&config.reliability, &provider),
        spawn_fallback: if manifest.route_hint.is_some() {
            config.api_key.as_deref()
        } else {
            provider.api_key
        },
        provider,
        root_api_key: config.api_key.as_deref(),
    }
}

/// A process child accepts only the exact route its parent sealed. In
/// particular, a route key that happens to equal the root key is still not
/// permission to attach the root URL or rotation keys.
fn validate_worker_route(manifest: &WorkerManifest, config: &Config) -> Result<()> {
    let credentials = worker_credentials(manifest, config);
    let Some(hint) = manifest.route_hint.as_deref() else {
        if manifest.route_uses_root_connection || manifest.model.trim().starts_with("hint:") {
            anyhow::bail!("session-worker manifest has an unresolved or inconsistent model route");
        }
        if manifest.direct_uses_root_connection
            && (crate::providers::ProviderId::of(&manifest.provider_name)
                != crate::providers::ProviderId::of(crate::providers::selection::default_provider(config))
                || credentials.sealed_key != credentials.root_api_key)
        {
            anyhow::bail!("session-worker direct root connection does not match sealed config");
        }
        if let Some(agent) = manifest.agent_id.as_deref().and_then(|name| config.agents.get(name))
            && crate::providers::ProviderId::of(&agent.provider)
                == crate::providers::ProviderId::of(&manifest.provider_name)
        {
            if let Some(id) = agent.credential_ref.as_deref() {
                if manifest.direct_uses_root_connection {
                    anyhow::bail!("session-worker agent credential cannot use the root connection");
                }
                let selected = crate::providers::selection::check_with(
                    config,
                    &crate::providers::selection::catalog_for(config),
                    &manifest.provider_name,
                    &manifest.model,
                    crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
                    &crate::providers::catalog::Requirement::chat(),
                    crate::providers::catalog::UnknownPolicy::Invocation,
                )?;
                if selected.credential().api_key() != credentials.sealed_key {
                    anyhow::bail!("session-worker agent credential does not match sealed config");
                }
            } else if let Some(key) = agent.api_key.as_deref().filter(|key| !key.trim().is_empty()) {
                if manifest.direct_uses_root_connection || credentials.sealed_key != Some(key) {
                    anyhow::bail!("session-worker agent credential does not match sealed config");
                }
            }
        }
        return Ok(());
    };
    if manifest.direct_uses_root_connection {
        anyhow::bail!("session-worker routed connection has direct root provenance");
    }
    let catalog = crate::providers::selection::catalog_for(config);
    let selection = format!("hint:{hint}");
    let route = crate::providers::selection::route_target(&catalog, &manifest.provider_name, &selection)?
        .context("session-worker manifest route is missing")?;
    if route.provider != manifest.provider_name {
        anyhow::bail!("session-worker manifest route destination does not match sealed config");
    }
    if route.model != manifest.model {
        anyhow::bail!("session-worker manifest route destination does not match sealed config");
    }
    if manifest.route_uses_root_connection
        && (route.has_own_key
            || route.credential_ref.is_some()
            || crate::providers::ProviderId::of(&route.provider)
                != crate::providers::ProviderId::of(crate::providers::selection::default_provider(config)))
    {
        anyhow::bail!("session-worker manifest route cannot use the root connection");
    }
    if manifest.route_uses_root_connection && credentials.sealed_key != credentials.root_api_key {
        anyhow::bail!("session-worker manifest root connection does not match sealed config");
    }
    let source_provider = manifest
        .route_source_provider
        .as_deref()
        .context("session-worker manifest route source is missing")?;
    let credential = manifest
        .route_credential
        .as_ref()
        .context("session-worker manifest route credential provenance is missing")?;
    let resolved = crate::providers::selection::check_with(
        config,
        &catalog,
        source_provider,
        &selection,
        credential.clone(),
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    if resolved.credential().api_key() != credentials.sealed_key {
        anyhow::bail!("session-worker manifest route credential does not match sealed config");
    }
    if resolved.credential().reference() != credential {
        anyhow::bail!("session-worker manifest route credential provenance does not match sealed config");
    }
    let resolved_uses_root = matches!(
        resolved.credential().reference(),
        crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
    );
    if manifest.route_uses_root_connection != resolved_uses_root {
        anyhow::bail!("session-worker manifest route connection provenance does not match sealed config");
    }
    Ok(())
}

/// The worker's own `sessions_spawn`, which pairs `credentials.spawn_fallback`
/// with `manifest.provider_name` and accepts a per-call provider only among
/// the providers the operator configured (the same set every other surface
/// uses).
fn worker_spawn_tool(
    manifest: &WorkerManifest,
    config: &Config,
    credentials: &WorkerCredentials<'_>,
    provider: Arc<dyn Provider>,
    security: Arc<SecurityPolicy>,
    options: &crate::providers::ProviderRuntimeOptions,
    active_runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
) -> crate::tools::SessionsSpawnTool {
    let source_provider = manifest
        .route_source_provider
        .as_deref()
        .unwrap_or(manifest.provider_name.as_str());
    let spawn_provider_name = if manifest.route_hint.is_some() {
        source_provider
    } else {
        manifest.provider_name.as_str()
    };
    let spawn_model = manifest
        .route_hint
        .as_ref()
        .map_or_else(|| manifest.model.clone(), |hint| format!("hint:{hint}"));
    let tool = crate::tools::SessionsSpawnTool::new_with_registry(
        Arc::new(WorkerChannel),
        provider,
        spawn_provider_name,
        spawn_model,
        manifest.temperature,
        security,
        manifest.workspace_dir.clone(),
        config.multimodal.clone(),
        config.agents_with_references().into_owned(),
        credentials.spawn_fallback.map(str::to_string),
        options.clone(),
        config.sessions_spawn.clone(),
        active_runs,
    )
    .with_cost_config(config.cost.clone())
    .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(config))
    .with_model_catalog(crate::providers::selection::catalog_for(config))
    .with_hint_config(config)
    .with_fallback_api_key_owner(if manifest.route_hint.is_some() {
        crate::providers::selection::default_provider(config)
    } else {
        manifest.provider_name.as_str()
    })
    .with_fallback_root_connection(manifest.route_hint.is_some() || manifest.direct_uses_root_connection);
    if let (Some(source), Some(credential)) = (&manifest.route_source_provider, &manifest.route_credential) {
        tool.with_default_route_source(source, credential.clone())
    } else {
        tool
    }
}

/// Build the worker's own provider chain from its scoped credentials, after
/// resolving the manifest's provider and model for chat: a model that cannot
/// chat builds nothing (an id-only model runs, with a warning).
fn worker_provider(
    manifest: &WorkerManifest,
    config: &Config,
    credentials: &WorkerCredentials<'_>,
    options: &crate::providers::ProviderRuntimeOptions,
) -> Result<Box<dyn Provider>> {
    if manifest.model.trim().starts_with("hint:") {
        anyhow::bail!("session-worker received an unresolved model route");
    }
    let credential = if manifest.route_hint.is_some() {
        manifest
            .route_credential
            .clone()
            .context("session-worker manifest route credential provenance is missing")?
    } else if let Some((name, agent)) = manifest
        .agent_id
        .as_deref()
        .and_then(|name| config.agents.get(name).map(|agent| (name, agent)))
        .filter(|(_, agent)| {
            crate::providers::ProviderId::of(&agent.provider)
                == crate::providers::ProviderId::of(&manifest.provider_name)
        })
    {
        match agent
            .credential_ref
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
            None if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) => {
                crate::providers::credentials::CredentialRef::LegacyInline(
                    crate::providers::credentials::LegacySlot::Agent(name.to_string()),
                )
            }
            None => crate::providers::selection::credential_for(config, &manifest.provider_name),
        }
    } else {
        crate::providers::selection::credential_for(config, &manifest.provider_name)
    };
    build_worker_provider_selection(
        config,
        &manifest.provider_name,
        &manifest.model,
        credential,
        &credentials.provider,
        &credentials.reliability,
        options,
    )
}

/// Resolve and construct the exact provider/model connection a worker was
/// sealed to. This is the worker's sole provider-construction seam.
pub(crate) fn build_worker_provider_selection(
    config: &Config,
    provider_name: &str,
    model: &str,
    credential: crate::providers::credentials::CredentialRef,
    sealed_connection: &crate::providers::ProviderConnection<'_>,
    reliability: &crate::config::ReliabilityConfig,
    options: &crate::providers::ProviderRuntimeOptions,
) -> Result<Box<dyn Provider>> {
    let catalog = crate::providers::selection::catalog_for(config);
    let resolved = crate::providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    anyhow::ensure!(
        resolved.credential().api_key() == sealed_connection.api_key
            && resolved.credential().api_url() == sealed_connection.api_url,
        "session-worker resolved credential differs from its sealed connection"
    );
    crate::providers::create_provider_for(&resolved, &catalog, reliability, &config.cost, options)
}

fn write_worker_result(result: &impl serde::Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let json = serde_json::to_string(result).context("serialize worker result")?;
    stdout.write_all(json.as_bytes()).context("write worker result")?;
    stdout.write_all(b"\n").context("write worker newline")?;
    stdout.flush().context("flush worker stdout")?;
    Ok(())
}

/// Start the stdin control-frame reader for a running worker.
///
/// Line 1 of stdin was already consumed as the manifest; everything after it is
/// a [`WorkerControlFrame`]. The reader runs on a **detached OS thread**, not on
/// the tokio blocking pool and not on `tokio::io::stdin`, for three reasons:
///
/// * A `tokio::io::stdin` read that is pending when the worker finishes parks a
///   blocking-pool thread that nothing can wake: the parent deliberately keeps
///   the write end of the pipe open for the whole run, and a blocking read
///   observes no `CancellationToken`. Runtime drop joins blocking threads, so
///   that read is on the critical path of exit. It does **not** hang the
///   process — `main` shuts the runtime down with `RUNTIME_SHUTDOWN_TIMEOUT`
///   (2 s, `src/main.rs`), which caps the join — but the cap is the only thing
///   that ends it, so every process-mode worker would spend those 2 seconds
///   waiting after its `WorkerResult` line was already written and its answer
///   was already in the parent's hands. A detached OS thread is torn down at
///   process exit instead, so the single-line stdout protocol still ends the
///   process the moment the result is flushed.
///
///   (This bullet, and the commit message that introduced it, originally said
///   the process would "never exit". That was wrong in exactly the way that
///   matters for a future reader deciding whether the thread is still needed:
///   the shutdown cap was already in place, so the real cost is a fixed 2 s
///   tax on every worker exit, not a wedged process. The design decision is
///   unchanged — a 2 s tax per sub-agent is reason enough — only the stated
///   reason is corrected.)
/// * It leaves the configured blocking pool (`[runtime] max_blocking_threads`,
///   the process's last implicit concurrency gate) untouched.
/// * It reads the same global buffered `std::io::Stdin` handle the manifest was
///   read from, so bytes the manifest read buffered past its newline — a steer
///   frame that arrived in the same write — are not lost.
///
/// Back-pressure, not loss: `blocking_send` parks this thread while the queue is
/// full, which stops draining the pipe, which parks the parent's writer, which
/// fills the parent's bounded queue and finally slows the steering caller down.
/// There is deliberately no wall-clock deadline anywhere on that chain: a worker
/// too busy to accept a frame is the idle detector's business, not a timeout's.
fn spawn_stdin_control_reader(steer_tx: tokio::sync::mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // Parent closed the pipe: no further steering is possible.
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // A malformed frame must never end the run: skip it and keep reading.
            let Ok(frame) = serde_json::from_str::<WorkerControlFrame>(line) else {
                tracing::warn!("session-worker discarded an unparseable stdin control frame");
                continue;
            };
            let WorkerControlFrame::Steer { message } = frame;
            if steer_tx.blocking_send(message).is_err() {
                break;
            }
        }
    });
}

/// Output of one cancellable agent-loop segment: the history it advanced plus
/// its result.
type SteeredSegment = (
    Vec<ChatMessage>,
    Result<(String, crate::agent::turn_contracts::ToolLoopTrace)>,
);

/// Run an agent-loop segment, restarting it with an injected operator turn each
/// time a steer frame arrives.
///
/// This is the worker-process twin of the task-mode steering race in
/// `sessions_spawn::run_sub_agent_task`, and injects the identical
/// [`steering_instruction`] wording so both modes read the same in history.
/// A steer cancels the in-flight segment, waits for it to unwind (so the
/// history it already advanced is preserved), appends the operator turn and
/// re-enters the loop. A closed channel means no more steering, so the current
/// segment is simply awaited to its natural end.
async fn run_segments_with_steering<F, Fut>(
    mut history: Vec<ChatMessage>,
    steer_rx: &mut Option<tokio::sync::mpsc::Receiver<String>>,
    mut run_segment: F,
) -> SteeredSegment
where
    F: FnMut(Vec<ChatMessage>, CancellationToken, Option<String>) -> Fut,
    Fut: Future<Output = SteeredSegment>,
{
    let mut raw_steer = None;
    loop {
        let cancel = CancellationToken::new();
        let segment = run_segment(history, cancel.clone(), raw_steer.take());
        tokio::pin!(segment);

        let steered = tokio::select! {
            finished = &mut segment => return finished,
            steer = next_steer(steer_rx) => steer,
        };

        match steered {
            Some(message) => {
                cancel.cancel();
                let (mut advanced, cancelled) = segment.await;
                if cancelled
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>())
                {
                    return (advanced, cancelled);
                }
                advanced.push(ChatMessage::user(steering_instruction(&message)));
                raw_steer = Some(message);
                history = advanced;
            }
            // Steer channel closed — wait for natural completion.
            None => return segment.await,
        }
    }
}

/// Await the next steer message, or never resolve when the run has no steer
/// channel at all (in-process callers and tests).
async fn next_steer(steer_rx: &mut Option<tokio::sync::mpsc::Receiver<String>>) -> Option<String> {
    match steer_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn select_tools_for_worker(
    source: Vec<Box<dyn Tool>>,
    allowed_tools: &[String],
    channel_excluded_tools: &[String],
) -> Result<Arc<Vec<Box<dyn Tool>>>> {
    let normalized = allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if normalized.contains(&"*") && normalized.as_slice() != ["*"] {
        anyhow::bail!("Worker allowed_tools must use '*' exclusively");
    }

    let source = Arc::new(source);
    crate::tools::sessions_spawn::refresh_tools_for_explicit_allowlist(&source, allowed_tools).await?;
    Ok(crate::tools::sessions_spawn::resolve_tools_for_agent(
        source,
        "session-worker",
        crate::tools::sessions_spawn::MemoryScope::Shared,
        (!normalized.is_empty()).then_some(allowed_tools),
        channel_excluded_tools,
    ))
}

/// Bind a worker manifest to the tool filter.
///
/// This is the only place the manifest's two tool lists reach
/// [`select_tools_for_worker`]. It takes the whole [`WorkerManifest`] rather
/// than two `&[String]` arguments read out at the call site so that the wiring
/// itself is reachable from a test: a caller cannot hand the filter a list that
/// did not come from the sealed manifest, and
/// `a_worker_manifest_binds_its_channel_exclusions_to_the_tool_filter` exercises
/// this binding instead of the filter alone.
async fn select_tools_for_manifest(
    source: Vec<Box<dyn Tool>>,
    manifest: &WorkerManifest,
) -> Result<Arc<Vec<Box<dyn Tool>>>> {
    select_tools_for_worker(source, &manifest.allowed_tools, &manifest.channel_excluded_tools).await
}

fn verify_worker_orchestration_tool_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let registered = names.into_iter().collect::<std::collections::HashSet<_>>();
    let missing = WORKER_ORCHESTRATION_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !registered.contains(name))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "Session worker is missing required orchestration tools: {}",
            missing.join(", ")
        );
    }
    Ok(())
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

/// Build the worker's leading system message from the same runtime prompt
/// builder used by interactive chat and channel turns.
///
/// A process worker used to receive only the short delegated-agent instruction.
/// That omitted the normal Chat contract (available tools, working directory,
/// native-tool guidance, safety, skills, and runtime context). Native-tool
/// models could then produce a plausible prose completion without ever calling
/// the ToolSpecs that were present on the request. Keep an explicit agent
/// prompt as the final, task-specific section, but make the common execution
/// contract identical to the other agentic entrypoints.
fn build_worker_system_prompt(manifest: &WorkerManifest, config: &Config, native_tools: bool) -> String {
    // The worker may use an isolated workspace. Prompt rendering must describe
    // that workspace (and load its skills), rather than the parent process's
    // configured workspace path.
    let mut worker_config = config.clone();
    worker_config.workspace_dir = manifest.workspace_dir.clone();

    let skills = crate::skills::load_skills_with_config(&manifest.workspace_dir, &worker_config);
    build_worker_system_prompt_with_skills(manifest, &worker_config, native_tools, &skills)
}

fn build_worker_system_prompt_with_skills(
    manifest: &WorkerManifest,
    config: &Config,
    native_tools: bool,
    skills: &[crate::skills::Skill],
) -> String {
    let mut worker_config = config.clone();
    worker_config.workspace_dir = manifest.workspace_dir.clone();
    let runtime_prompt = crate::agent::runtime_prompt::build_runtime_system_prompt(
        &worker_config,
        &manifest.model,
        skills,
        native_tools,
    );
    let delegated_prompt = resolve_system_prompt(manifest);

    format!(
        "{runtime_prompt}\n\n## Delegated Agent Instructions\n\n{delegated_prompt}\n\n## Execution Integrity\n\nWhen the delegated task requires reading, writing, running, or verifying files, perform those operations with the provided tools before reporting completion. Never claim that a file, command, or check succeeded unless the corresponding tool actually produced the result."
    )
}

fn parse_tools_override(tools_json: &str) -> Result<Vec<String>> {
    serde_json::from_str(tools_json).with_context(|| "parse --tools JSON as string array")
}

fn path_has_parent_or_prefix(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
}

fn ensure_clean_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if path_has_parent_or_prefix(path) {
        anyhow::bail!("{label} must not contain parent directory or platform prefix components");
    }
    Ok(())
}

fn ensure_relative_clean_path(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        anyhow::bail!("{label} must be relative");
    }
    ensure_clean_path(path, label)
}

fn ensure_child_path(path: &Path, root: &Path, label: &str) -> Result<()> {
    ensure_clean_path(path, label)?;
    ensure_clean_path(root, "workspace_dir")?;
    if !path.starts_with(root) {
        anyhow::bail!("{label} must stay under workspace_dir");
    }
    Ok(())
}

fn normalized_worker_memory_strategy(manifest: &WorkerManifest) -> Result<&'static str> {
    match manifest.memory_strategy.as_deref().unwrap_or("shared_fabric").trim() {
        "" | "shared_fabric" => Ok("shared_fabric"),
        "isolated_private" => Ok("isolated_private"),
        "hybrid" => anyhow::bail!(crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE),
        other => anyhow::bail!("Invalid session-worker memory_strategy '{other}'"),
    }
}

/// Validate the sealed session-worker capability (FIX-P0-36).
///
/// The capability is no longer an opaque UUID compared by string equality. It
/// is an `HMAC_SHA256(secret, run_id \0 expiry \0 sha256(manifest))` token
/// (base64url). Validation:
/// 1. Both the manifest-embedded token and the env-supplied token must be
///    present and identical (defends against env/manifest desync).
/// 2. The capability must not be past its absolute expiry.
/// 3. The token must equal the HMAC recomputed from the *received* manifest
///    (capability field blanked), the run id, and the expiry — compared in
///    constant time. A legacy UUID, a forged token, a tampered manifest, or a
///    replay under a different run id all fail here.
///
/// The expiry is read from `OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY`.
/// Created only after verifying this exact manifest and its transport expiry.
/// Not cloneable or serializable; environment secrets can be scrubbed at boot.
pub(crate) struct AuthenticatedWorkerTransport {
    key: ring::hmac::Key,
    manifest_digest: [u8; 32],
}
impl std::fmt::Debug for AuthenticatedWorkerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedWorkerTransport").finish_non_exhaustive()
    }
}
impl AuthenticatedWorkerTransport {
    pub(crate) fn verify_manifest(&self, manifest: &WorkerManifest) -> Result<()> {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(manifest_signing_payload(manifest)?.as_bytes()).into();
        anyhow::ensure!(
            digest == self.manifest_digest,
            "worker manifest changed after transport authentication"
        );
        Ok(())
    }
}

#[cfg(test)]
fn validate_worker_capability_with_env(manifest: &WorkerManifest, env_capability: Option<&str>) -> Result<()> {
    authenticate_worker_capability_with_env(manifest, env_capability).map(|_| ())
}

fn authenticate_worker_capability_with_env(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
) -> Result<AuthenticatedWorkerTransport> {
    matching_worker_capability(manifest, env_capability)?;
    let expiry = capability_expiry_from_env()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    anyhow::ensure!(now <= expiry, "session-worker capability expired");
    authenticate_worker_capability_with_key(manifest, env_capability, expiry, now, session_worker_signing_key())
}

fn matching_worker_capability<'a>(manifest: &'a WorkerManifest, env_capability: Option<&str>) -> Result<&'a str> {
    let manifest_capability = manifest
        .parent_capability
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker manifest is missing parent capability")?;
    let env_capability = env_capability
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker parent capability env is missing")?;

    if !capability_constant_time_eq(manifest_capability.as_bytes(), env_capability.as_bytes()) {
        anyhow::bail!("session-worker parent capability mismatch");
    }

    Ok(manifest_capability)
}

fn authenticate_worker_capability_with_key(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
    expiry: u64,
    now: u64,
    key: ring::hmac::Key,
) -> Result<AuthenticatedWorkerTransport> {
    let manifest_capability = matching_worker_capability(manifest, env_capability)?;

    if now > expiry {
        anyhow::bail!("session-worker capability expired");
    }

    let payload = manifest_signing_payload(manifest)?;
    let expected = compute_worker_capability_with_key(&key, &manifest.run_id, expiry, &payload);
    if !capability_constant_time_eq(manifest_capability.as_bytes(), expected.as_bytes()) {
        anyhow::bail!("session-worker capability signature mismatch");
    }

    use sha2::{Digest, Sha256};
    Ok(AuthenticatedWorkerTransport {
        key,
        manifest_digest: Sha256::digest(payload.as_bytes()).into(),
    })
}

/// Read the capability absolute expiry (unix seconds) from the environment.
fn capability_expiry_from_env() -> Result<u64> {
    let raw = std::env::var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY")
        .context("session-worker capability expiry env is missing")?;
    raw.trim()
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("invalid session-worker capability expiry: {e}"))
}

/// Serialize a manifest with an empty `parent_capability` field, returning the
/// canonical JSON payload (alphabetical key order via `serde_json::Value`) — the
/// exact input the parent signed in `sessions_spawn::manifest_signing_payload`.
fn manifest_signing_payload(manifest: &WorkerManifest) -> Result<String> {
    let mut value = serde_json::to_value(manifest).context("serialize worker manifest for capability")?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    serde_json::to_string(&value).context("reserialize worker manifest for capability")
}

/// Recompute `HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest))` as
/// base64url (no padding). Mirror of `sessions_spawn::compute_worker_capability`.
#[cfg(test)]
fn compute_worker_capability(run_id: &str, expiry_unix: u64, manifest_json: &str) -> String {
    compute_worker_capability_with_key(&session_worker_signing_key(), run_id, expiry_unix, manifest_json)
}

fn compute_worker_capability_with_key(
    key: &ring::hmac::Key,
    run_id: &str,
    expiry_unix: u64,
    manifest_json: &str,
) -> String {
    use base64::Engine as _;
    use ring::hmac;
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(manifest_json.as_bytes());
    let manifest_hex = capability_hex_encode(&hasher.finalize());

    let mut payload = Vec::with_capacity(run_id.len() + manifest_hex.len() + 32);
    payload.extend_from_slice(run_id.as_bytes());
    payload.push(0);
    payload.extend_from_slice(expiry_unix.to_string().as_bytes());
    payload.push(0);
    payload.extend_from_slice(manifest_hex.as_bytes());

    let tag = hmac::sign(key, &payload);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref())
}

/// Lowercase hex-encode a byte slice.
fn capability_hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        // `b >> 4` and `b & 0x0f` are both in `0..16`, always valid indices into
        // the 16-byte `HEX` table; `.get().copied()` keeps the lookup panic-free.
        out.push(HEX.get((b >> 4) as usize).copied().unwrap_or(b'0') as char);
        out.push(HEX.get((b & 0x0f) as usize).copied().unwrap_or(b'0') as char);
    }
    out
}

/// Constant-time byte-slice equality.
fn capability_constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Process-level fallback secret (mirror of the minting side).
static SESSION_WORKER_FALLBACK_SECRET: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// Return the shared HMAC verification key. Resolution order mirrors the minting
/// side exactly so parent and child derive the same key:
/// `SESSION_WORKER_SECRET` env → persisted state-dir secret → per-process random.
fn session_worker_signing_key() -> ring::hmac::Key {
    use ring::hmac;
    if let Ok(secret) = std::env::var("SESSION_WORKER_SECRET") {
        if !secret.is_empty() {
            return hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        }
    }
    if let Some(bytes) = load_or_create_persisted_session_secret() {
        return hmac::Key::new(hmac::HMAC_SHA256, &bytes);
    }
    let bytes = SESSION_WORKER_FALLBACK_SECRET.get_or_init(generate_session_secret);
    hmac::Key::new(hmac::HMAC_SHA256, bytes)
}

/// Generate 32 random bytes, time-derived fallback on RNG failure (never panics).
fn generate_session_secret() -> [u8; 32] {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    let mut buf = [0u8; 32];
    if rng.fill(&mut buf).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_le_bytes();
        for (i, b) in buf.iter_mut().enumerate() {
            // `i % now.len()` is always within `now` (non-empty fixed-size array);
            // `.get().copied()` keeps the seed fill panic-free.
            *b = now.get(i % now.len()).copied().unwrap_or(0);
        }
    }
    buf
}

/// Path to the persisted session-worker secret under the OpenPRX state dir
/// (mirror of the minting side; resolved config directory, else `HOME`).
fn session_secret_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENPRX_SESSION_WORKER_SECRET_PATH") {
        return Some(std::path::PathBuf::from(explicit));
    }
    // Follows the resolved config directory so the secret minted by a
    // `--config-dir` run is the one its workers read back, and no run writes
    // key material into a configuration it was told not to touch.
    if let Some(config_dir) = crate::config::process_config_dir() {
        return Some(config_dir.join("keys").join("session_worker.secret"));
    }
    crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret"))
}

/// Load the persisted 32-byte secret, creating it on first use. Returns `None`
/// when no state dir is resolvable or filesystem ops fail.
fn load_or_create_persisted_session_secret() -> Option<[u8; 32]> {
    let path = session_secret_path()?;
    if let Ok(existing) = std::fs::read(&path) {
        if existing.len() == 32 {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&existing);
            return Some(buf);
        }
    }
    let secret = generate_session_secret();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    match std::fs::write(&path, secret) {
        Ok(()) => Some(secret),
        Err(error) => {
            tracing::warn!("failed to persist session-worker secret: {error}");
            None
        }
    }
}

fn validate_worker_manifest_with_capability_env(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
) -> Result<AuthenticatedWorkerTransport> {
    let transport = authenticate_worker_capability_with_env(manifest, env_capability)?;
    anyhow::ensure!(
        manifest.operator_source.is_none()
            || (manifest.ordinary_source.is_none()
                && manifest.child_source.is_none()
                && manifest.capability_manifest.is_none()),
        "session-worker cannot mix operator and delegated source authority"
    );
    if let Some(source) = &manifest.operator_source {
        let key = crate::decision::key::DecisionKey::from_worker_channel(
            &transport.key,
            &source.publication.workspace,
            &manifest.run_id,
            &manifest.config_generation,
        )?;
        let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
            &source.catalog,
            &key,
            crate::causal_tree::branch::CatalogGeneration(source.publication.catalog_generation),
        )?;
        let publication = source.publication.verify(&key, &catalog)?;
        anyhow::ensure!(
            manifest.owner_id.as_deref() == Some(publication.owner()),
            "operator worker owner differs from publication"
        );
        crate::providers::catalog::CatalogView::restore_operator_process(
            &source.provider_catalog,
            &key,
            publication.id(),
        )?;
        crate::runtime::child_source::parent::ParentSource::restore_operator_process(manifest, &publication, &key)?;
    }
    anyhow::ensure!(
        manifest.ordinary_source.is_none()
            || (manifest.capability_manifest.is_none() && manifest.child_source.is_none()),
        "session-worker cannot mix ordinary and durable source authority"
    );

    // The outer HMAC authenticates transport only. Inner capability seal,
    // durable parent/source lookup and executor parity are admission's job.
    match (&manifest.capability_manifest, &manifest.child_source) {
        (None, None) => {}
        (Some(capabilities), Some(source)) => {
            let parent = crate::causal_tree::ids::DecisionId::parse(&capabilities.parent_decision_id)
                .context("session-worker parent decision id is malformed")?;
            anyhow::ensure!(
                parent.to_string() == capabilities.parent_decision_id
                    && manifest.owner_id.as_deref() == Some(capabilities.owner.as_str())
                    && !capabilities.owner.is_empty()
                    && capabilities.workspace == source.parent_constraints.workspace
                    && !source.parent_store_workspace.is_empty()
                    && source.parent_decision_digest.len() == 64
                    && source
                        .parent_decision_digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    && !source.input_event_id.is_empty()
                    && source.input_event_id.len() <= 256
                    && !source.input_event_id.chars().any(char::is_control)
                    && manifest
                        .parent_run_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty() && id != manifest.run_id)
                    && !manifest.session_scope_key.is_empty()
                    && manifest.runtime_config_generation_id.is_some()
                    && manifest
                        .runtime_config_source_revision
                        .as_deref()
                        .is_some_and(|revision| !revision.is_empty()),
                "session-worker child source transport is inconsistent"
            );
        }
        _ => anyhow::bail!("session-worker child authority and source must be supplied together"),
    }

    let run_id = manifest.run_id.trim();
    if run_id.is_empty()
        || run_id.contains('/')
        || run_id.contains('\\')
        || run_id.contains("..")
        || run_id.chars().any(char::is_control)
    {
        anyhow::bail!("session-worker run_id must be a single non-empty path-safe segment");
    }

    ensure_clean_path(&manifest.workspace_dir, "workspace_dir")?;
    ensure_clean_path(&manifest.memory_db_path, "memory_db_path")?;
    if !manifest.config_dir.is_absolute()
        || manifest
            .config_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("session-worker config_dir must be an absolute path without parent traversal");
    }
    if manifest.config_generation.len() != 64
        || !manifest.config_generation.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("session-worker config_generation must be a SHA-256 hex digest");
    }
    if let Some(identity_dir) = manifest
        .identity_dir
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        ensure_relative_clean_path(identity_dir.trim(), "identity_dir")?;
    }

    let strategy = normalized_worker_memory_strategy(manifest)?;
    if let Some(worker_memory_db_path) = manifest.worker_memory_db_path.as_ref() {
        ensure_child_path(worker_memory_db_path, &manifest.workspace_dir, "worker_memory_db_path")?;
    }
    if matches!(strategy, "isolated_private" | "hybrid") {
        let worker_memory_db_path = manifest
            .worker_memory_db_path
            .as_ref()
            .context("worker_memory_db_path is required for isolated/hybrid session-worker memory")?;
        if manifest.memory_db_path != *worker_memory_db_path {
            anyhow::bail!("memory_db_path must match worker_memory_db_path for isolated/hybrid memory");
        }
    }

    if strategy == "shared_fabric" {
        let shared_memory_db_path = manifest
            .shared_memory_db_path
            .as_ref()
            .context("shared_memory_db_path is required for shared_fabric session-worker memory")?;
        ensure_clean_path(shared_memory_db_path, "shared_memory_db_path")?;
        if manifest.memory_db_path != *shared_memory_db_path {
            anyhow::bail!("memory_db_path must match shared_memory_db_path for shared_fabric memory");
        }
    }

    if strategy == "hybrid" {
        let shared_memory_db_path = manifest
            .shared_memory_db_path
            .as_ref()
            .context("shared_memory_db_path is required for hybrid session-worker memory")?;
        ensure_clean_path(shared_memory_db_path, "shared_memory_db_path")?;
    }

    Ok(transport)
}

fn validate_worker_cli_overrides(
    manifest: &WorkerManifest,
    task: Option<&str>,
    workspace: Option<&str>,
    memory_db: Option<&str>,
    tools: Option<&[String]>,
    config_dir: Option<&str>,
) -> Result<()> {
    if let Some(task) = task {
        if task != manifest.task {
            anyhow::bail!("session-worker CLI task override must match sealed manifest");
        }
    }
    if let Some(workspace) = workspace {
        if Path::new(workspace) != manifest.workspace_dir {
            anyhow::bail!("session-worker CLI workspace override must match sealed manifest");
        }
    }
    if let Some(memory_db) = memory_db {
        if Path::new(memory_db) != manifest.memory_db_path {
            anyhow::bail!("session-worker CLI memory-db override must match sealed manifest");
        }
    }
    if let Some(tools) = tools {
        if tools != manifest.allowed_tools.as_slice() {
            anyhow::bail!("session-worker CLI tools override must match sealed manifest");
        }
    }
    let config_dir = config_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker requires the parent --config-dir")?;
    if Path::new(config_dir) != manifest.config_dir {
        anyhow::bail!("session-worker CLI config-dir must match sealed manifest");
    }
    Ok(())
}

fn validate_parent_memory_backend(manifest: &WorkerManifest, configured_memory_backend: &str) -> Result<()> {
    if manifest.memory_backend.trim() != configured_memory_backend {
        anyhow::bail!(
            "session-worker parent memory backend mismatch: manifest={}, config={configured_memory_backend}",
            manifest.memory_backend
        );
    }
    Ok(())
}

#[cfg(test)]
async fn run_validated_manifest(
    mut manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
) -> Result<WorkerResult> {
    anyhow::ensure!(
        manifest.ordinary_source.is_none() && manifest.operator_source.is_none(),
        "worker source requires authenticated transport"
    );
    // Existing direct fixtures use the real full-manifest verifier with an
    // isolated fixture key, rather than bypassing transport with None.
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx_worker_transport_fixture_key");
    let payload = manifest_signing_payload(&manifest)?;
    let capability = compute_worker_capability_with_key(&key, &manifest.run_id, 300, &payload);
    manifest.parent_capability = Some(capability.clone());
    let transport = authenticate_worker_capability_with_key(&manifest, Some(&capability), 300, 0, key)?;
    run_authenticated_manifest(manifest, steer_rx, explicit_config_dir, Some(transport)).await
}

async fn run_authenticated_manifest(
    manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> Result<WorkerResult> {
    let parent = crate::runtime::turn_cleanup::current_tracker();
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(parent.as_ref(), CancellationToken::new())?;
    let prepared = cleanup
        .run(owned_worker_future(manifest, steer_rx, explicit_config_dir, transport))
        .await?;
    if let Some(completion) = prepared.completion {
        super::completion::persist(
            &completion.fabric,
            completion.scope,
            &completion.grant,
            &prepared.result,
        )
        .await?;
    }
    if let Some(authority) = prepared.ordinary_completion {
        super::completion::persist_ordinary(&authority, &prepared.result).await?;
    }
    if let Some(authority) = prepared.operator_completion {
        super::completion::persist_operator(&authority, &prepared.result).await?;
    }
    Ok(prepared.result)
}

struct PendingWorkerCompletion {
    fabric: MemoryFabric,
    scope: MessageEventScope,
    grant: Arc<crate::runtime::child_source::IssuedChildGrant>,
}

struct PreparedWorkerResult {
    _operator_lifetime: Option<crate::runtime::child_source::operator_process::OperatorProcessLifetime>,
    operator_completion: Option<Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>>,
    _ordinary_lifetime: Option<crate::runtime::child_source::ordinary_process::OrdinaryProcessLifetime>,
    result: WorkerResult,
    completion: Option<PendingWorkerCompletion>,
    ordinary_completion: Option<Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>>,
}

fn worker_source_workspace(manifest: &WorkerManifest) -> Result<&str> {
    match (
        &manifest.child_source,
        &manifest.ordinary_source,
        &manifest.operator_source,
    ) {
        (Some(source), None, None) => Ok(source.parent_constraints.workspace.as_str()),
        (None, Some(source), None) => Ok(source.parent_constraints.workspace.as_str()),
        (None, None, Some(source)) => Ok(source.publication.workspace.as_str()),
        _ => anyhow::bail!("worker source storage requires one authenticated source"),
    }
}

/// Decision source and settlement storage belong to the logical parent
/// workspace, independently of the worker's private execution memory.
fn worker_decision_fabric(
    manifest: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    execution_fabric: &MemoryFabric,
) -> Result<MemoryFabric> {
    let parent_workspace = worker_source_workspace(manifest)?;
    let config = &generation.effective;
    anyhow::ensure!(
        config.workspace_dir.to_string_lossy() == parent_workspace
            && manifest.runtime_config_generation_id == Some(generation.id.0)
            && manifest.runtime_config_source_revision.as_deref()
                == Some(generation.source_revision.fingerprint_sha256.as_str()),
        "worker decision storage differs from verified configuration"
    );
    if normalized_worker_memory_strategy(manifest)? == "shared_fabric" {
        anyhow::ensure!(
            execution_fabric.workspace_id() == parent_workspace,
            "shared worker memory differs from parent workspace"
        );
        return Ok(execution_fabric.clone());
    }
    // Keep execution tools on their existing private SQLite memory. Source
    // verification uses the parent's configured backend and ACL bindings.
    let memory: Arc<dyn Memory> = Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes_with_references(),
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        crate::memory::EmbeddingRootCredential::from_config(config),
        &config.identity_bindings,
        &config.user_policies,
    )?);
    Ok(MemoryFabric::new(memory, parent_workspace).with_event_recording(manifest.memory_event_recording))
}

enum WorkerSegmentSource {
    Operator {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::operator_process::OperatorProcessSegments>,
        authority: Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>,
    },
    Granted {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::ChildSegmentSequence>,
        grant: Arc<crate::runtime::child_source::IssuedChildGrant>,
        binding: Arc<crate::runtime::child_source::ChildProviderBinding>,
        provider: Arc<dyn Provider>,
    },
    Ordinary {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::ordinary_process::OrdinaryProcessSegments>,
        authority: Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>,
    },
}

struct WorkerChildSegments {
    source: WorkerSegmentSource,
    service: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    fabric: MemoryFabric,
    owner: Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    usage: Arc<crate::runtime::turn_terminal::TerminalUsageCollector>,
}
impl WorkerChildSegments {
    async fn recorded_count(&self) -> u64 {
        match &self.source {
            WorkerSegmentSource::Granted { sequence, .. } => sequence.lock().await.recorded_count(),
            WorkerSegmentSource::Ordinary { sequence, .. } => sequence.lock().await.recorded_count(),
            WorkerSegmentSource::Operator { sequence, .. } => sequence.lock().await.recorded_count(),
        }
    }

    async fn admit(
        &self,
        input: &str,
        cancel: &CancellationToken,
        acknowledgement: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> Result<crate::runtime::child_turn::ChildSegmentTurn> {
        let (source, config) = match &self.source {
            WorkerSegmentSource::Granted {
                sequence,
                grant,
                binding,
                provider,
            } => {
                let receipt = sequence.lock().await.record(&self.fabric, input).await?;
                let proof = Arc::new(binding.source_proof(grant, &receipt, provider)?);
                let execution = receipt.execution_context(proof.as_ref())?;
                ((proof, execution), Arc::clone(&grant.generation().effective))
            }
            WorkerSegmentSource::Ordinary { sequence, authority } => {
                authority.verify()?;
                let source = sequence.lock().await.record(input).await?;
                (source, Arc::clone(&authority.snapshots().generation.effective))
            }
            WorkerSegmentSource::Operator { sequence, authority } => {
                authority.verify()?;
                let source = sequence.lock().await.record(input).await?;
                (source, Arc::clone(&authority.snapshots().generation.effective))
            }
        };
        Ok(boxed_worker_segments(|| {
            crate::runtime::child_turn::ChildSegmentTurn::admit(
                Arc::clone(&self.service),
                source,
                self.fabric.clone(),
                config,
                &self.owner,
                cancel,
                Some(Arc::clone(&self.usage)),
            )
        })
        .await?
        .with_child_cleanup(acknowledgement))
    }
}

struct VerifiedWorkerParent {
    store: Arc<crate::causal_tree::store::CausalDecisionStore>,
    scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
}

/// Reconstruct the signed scope and inherited catalog. Actual executor parity,
/// source proof and child terminal ownership remain required before execution.
fn verify_worker_parent_authority(
    manifest: &WorkerManifest,
    logical_workspace: &Path,
) -> Result<Option<VerifiedWorkerParent>> {
    use crate::causal_tree::store::{CausalDecisionStore, DecisionScope};
    use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope};
    anyhow::ensure!(
        manifest.ordinary_source.is_none() && manifest.operator_source.is_none(),
        "durable parent verifier rejects non-delegated worker source"
    );
    let (wire, source) = match (&manifest.capability_manifest, &manifest.child_source) {
        (None, None) => return Ok(None),
        (Some(wire), Some(source)) => (wire, source),
        _ => anyhow::bail!("session-worker child authority and source must be supplied together"),
    };
    anyhow::ensure!(
        wire.workspace == source.parent_constraints.workspace,
        "session-worker parent workspace differs from configuration"
    );
    anyhow::ensure!(
        wire.workspace == logical_workspace.to_string_lossy(),
        "session-worker parent workspace differs from configuration"
    );
    let key = crate::decision::key::DecisionKey::load_existing(logical_workspace)?;
    let parent = CapabilityConstraints::from_worker_constraints(&source.parent_constraints, key)?;
    let child = ChildCapabilityManifest::from_worker_manifest(wire, &parent)?;
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
        &source.catalog,
        &parent.key,
        parent.catalog_generation,
    )?;
    for reference in &parent.allowed {
        catalog.resolve(std::slice::from_ref(reference))?;
    }
    let store = CausalDecisionStore::open_unshared_read_only(logical_workspace);
    let store_workspace = store.workspace_id().context("worker parent store namespace missing")?;
    anyhow::ensure!(
        store_workspace.as_ref() == source.parent_store_workspace,
        "worker original store namespace differs from configured decision store"
    );
    let scope = DecisionScope::Owner {
        workspace_id: store_workspace,
        owner_id: Arc::from(parent.authority.owner.as_str()),
    };
    let record = store
        .reader()
        .replay_record(&scope, &parent.decision_id)?
        .context("session-worker parent decision is unavailable")?;
    anyhow::ensure!(
        record.digest == source.parent_decision_digest
            && record.catalog_generation == parent.catalog_generation.to_hex()
            && record.decision.enabled_snapshot
            && !record.decision.legacy_v1
            && record.decision.admits_issued_child()
            && record.decision.run_id == manifest.parent_run_id
            && manifest.owner_id.as_deref() == Some(parent.authority.owner.as_str()),
        "session-worker parent decision differs from issued authority"
    );
    Ok(Some(VerifiedWorkerParent {
        store,
        scope: Arc::new(VerifiedChildScope::new(child, &parent)?),
        catalog,
    }))
}

fn owned_worker_future(
    manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<PreparedWorkerResult>> + Send + '_>> {
    Box::pin(run_owned_manifest(manifest, steer_rx, explicit_config_dir, transport))
}

async fn run_owned_manifest(
    manifest: WorkerManifest,
    mut steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> Result<PreparedWorkerResult> {
    transport
        .as_ref()
        .context("worker source requires authenticated transport")?
        .verify_manifest(&manifest)?;
    anyhow::ensure!(
        manifest.child_source.is_some() || manifest.ordinary_source.is_some() || manifest.operator_source.is_some(),
        "worker requires an authenticated decision source"
    );
    let explicit_config_dir = explicit_config_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker requires the parent --config-dir")?;
    if Path::new(explicit_config_dir) != manifest.config_dir {
        anyhow::bail!("session-worker config source does not match sealed manifest");
    }

    let generation_before = match config_source_generation(&manifest.config_dir) {
        Ok(generation) => generation,
        Err(error) if manifest.operator_source.is_some() => {
            let authenticated = transport.as_ref().context("operator worker transport missing")?;
            // This witness exists only before config load and any provider/segment work.
            let failure = super::startup_failure::AuthenticatedStartupFailure::attest(
                &authenticated.key,
                authenticated.manifest_digest,
                &manifest.run_id,
                error.to_string(),
            )?;
            return Err(failure.into());
        }
        Err(error) => return Err(error),
    };
    if generation_before != manifest.config_generation {
        anyhow::bail!(
            "session-worker config generation mismatch before load: expected {}, found {}",
            manifest.config_generation,
            generation_before
        );
    }
    let mut config = Config::load_existing_read_only_with_config_dir(Some(explicit_config_dir)).await?;
    // The worker is a separate process and never reaches `runtime::mode::dispatch`,
    // so it installs the hang-detection thresholds itself. Without this it would
    // silently run on the built-in defaults instead of the operator's config.
    crate::agent::idle::install(config.runtime.idle_hang_secs);
    let generation_after = config_source_generation(&manifest.config_dir)?;
    if generation_after != manifest.config_generation {
        anyhow::bail!(
            "session-worker config generation changed during load: expected {}, found {}",
            manifest.config_generation,
            generation_after
        );
    }
    // Config loading resolves relative paths from the worker's physical
    // config directory. Restore the authenticated parent's spelling before
    // checking signed parent snapshots, while requiring the same filesystem
    // identity as the loaded configuration.
    let source_workspace = worker_source_workspace(&manifest)?;
    anyhow::ensure!(
        crate::config::generation::process_paths_match(&config.workspace_dir, Path::new(source_workspace))?,
        "session-worker parent workspace differs from configuration"
    );
    config.workspace_dir = PathBuf::from(source_workspace);
    let operator_snapshots = if manifest.operator_source.is_some() {
        Some(operator_source::restore(
            &manifest,
            config.clone(),
            transport.as_ref().context("operator worker transport missing")?,
        )?)
    } else {
        None
    };
    let ordinary_snapshots = if manifest.ordinary_source.is_some() {
        Some(ordinary_source::restore(
            &manifest,
            config.clone(),
            transport.as_ref().context("ordinary worker transport missing")?,
        )?)
    } else {
        None
    };
    let child_parent = if ordinary_snapshots.is_some() || operator_snapshots.is_some() {
        None
    } else {
        verify_worker_parent_authority(&manifest, &config.workspace_dir)?
    };
    let child_generation = match (child_parent.as_ref(), manifest.child_source.as_ref()) {
        (Some(parent_authority), Some(source)) => {
            anyhow::ensure!(
                manifest.runtime_config_generation_id == Some(source.config_generation.id.0)
                    && manifest.runtime_config_source_revision.as_deref()
                        == Some(source.config_generation.source_revision.fingerprint_sha256.as_str()),
                "session-worker configuration binding differs from source lineage"
            );
            let (_, parent) = parent_authority.scope.gate_inputs();
            Some(crate::config::ConfigGeneration::restore_process(
                config.clone(),
                &source.config_generation,
                &parent.key,
            )?)
        }
        (None, None) => None,
        _ => anyhow::bail!("session-worker verified scope and source binding differ"),
    };
    anyhow::ensure!(
        !config.causal_tree.enabled || child_parent.is_some() || operator_snapshots.is_some(),
        "decision-enabled session-worker requires authenticated child authority"
    );

    if child_parent.is_none() && ordinary_snapshots.is_none() && operator_snapshots.is_none() {
        validate_worker_route(&manifest, &config)?;
    }
    let configured_memory_backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    validate_parent_memory_backend(&manifest, &configured_memory_backend)?;
    config.workspace_dir = manifest.workspace_dir.clone();
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = Arc::new(
        SecurityPolicy::from_config(&config.autonomy, &manifest.workspace_dir)
            .with_audit_config(config.security.audit.clone()),
    );

    let provider_runtime_options = crate::providers::provider_runtime_options_from_config(&config);

    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime =
        if manifest.provider_name.starts_with("wasm:") || configured_memory_backend.starts_with("wasm:") {
            crate::plugins::init_plugin_runtime(&manifest.workspace_dir, None).await
        } else {
            None
        };

    let credentials = worker_credentials(&manifest, &config);
    let legacy_provider: Option<Arc<dyn Provider>> =
        if child_parent.is_none() && ordinary_snapshots.is_none() && operator_snapshots.is_none() {
            Some(Arc::from(worker_provider(
                &manifest,
                &config,
                &credentials,
                &provider_runtime_options,
            )?))
        } else {
            None
        };

    let memory: Arc<dyn Memory> = if normalized_worker_memory_strategy(&manifest)? == "shared_fabric" {
        let parent_workspace = manifest
            .memory_workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("shared_fabric session-worker requires parent memory_workspace_id")?;
        Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
            &config.memory,
            &config.embedding_routes_with_references(),
            Some(&config.storage.provider.config),
            &parent_workspace,
            crate::memory::EmbeddingRootCredential::from_config(&config),
            &config.identity_bindings,
            &config.user_policies,
        )?)
    } else {
        Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(
            manifest.memory_db_path.clone(),
            config.memory.acl_enabled,
        )?)
    };
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&memory))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in session-worker: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&manifest.workspace_dir, Some(Arc::clone(&memory))).await
    };
    let memory_workspace_id = manifest
        .memory_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string());
    let memory_fabric =
        MemoryFabric::new(memory.clone(), memory_workspace_id).with_event_recording(manifest.memory_event_recording);
    let decision_fabric = child_generation
        .as_ref()
        .or_else(|| ordinary_snapshots.as_ref().map(|source| &source.generation))
        .or_else(|| operator_snapshots.as_ref().map(|source| &source.generation))
        .map(|generation| worker_decision_fabric(&manifest, generation, &memory_fabric))
        .transpose()?;
    let (ordinary_authority, ordinary_lifetime) = if let Some(source) = ordinary_snapshots {
        let (authority, lifetime) = source
            .issue(
                &manifest,
                decision_fabric
                    .as_ref()
                    .context("ordinary worker source storage missing")?
                    .clone(),
                &CancellationToken::new(),
            )
            .await?;
        (Some(authority), Some(lifetime))
    } else {
        (None, None)
    };
    let (operator_authority, operator_lifetime) = if let Some(source) = operator_snapshots {
        let (authority, lifetime) = source
            .issue(
                &manifest,
                decision_fabric
                    .as_ref()
                    .context("operator worker source storage missing")?
                    .clone(),
                &CancellationToken::new(),
            )
            .await?;
        (Some(authority), Some(lifetime))
    } else {
        (None, None)
    };
    let worker_event_scope = ordinary_authority.as_ref().map_or_else(
        || worker_message_event_scope(&manifest),
        |authority| authority.logical_scope(),
    );
    let worker_event_scope = operator_authority
        .as_ref()
        .map_or(worker_event_scope, |authority| authority.logical_scope());
    let source_receipt = if let Some(parent) = child_parent.as_ref() {
        let source_fabric = decision_fabric.as_ref().context("worker decision storage is missing")?;
        Some(crate::runtime::child_source::restore_worker_input(source_fabric, &manifest, &parent.scope).await?)
    } else if ordinary_authority.is_some() || operator_authority.is_some() {
        // The parent's optional original input was already read back by issuance.
        None
    } else {
        SideEffectGate::new(security.as_ref())
            .authorize_resource_operation(
                "session_worker",
                &format!("session_worker:request_event:{}", manifest.run_id),
                ResourceRiskLevel::Low,
                None,
            )
            .map_err(anyhow::Error::msg)?;
        crate::runtime::child_source::record_child_input(
            &memory_fabric,
            worker_event_scope.clone(),
            &manifest.task,
            Some(format!("session_worker:{}:request", manifest.run_id)),
            Some(worker_lineage_payload(&manifest).to_string()),
            None,
        )
        .await?
    };

    let parent_source = if let Some(parent) = child_parent.as_ref() {
        Some(
            crate::runtime::child_source::parent::ParentSource::restore_process(
                &manifest,
                &parent.store,
                &parent.scope,
                decision_fabric.as_ref().context("worker decision storage is missing")?,
                child_generation
                    .as_deref()
                    .context("worker child generation is missing")?,
                source_receipt.as_deref().context("worker child input is missing")?,
            )
            .await?,
        )
    } else {
        None
    };

    let child_grant = if let Some(provenance) = parent_source {
        let authority = child_parent.as_ref().context("worker parent authority missing")?;
        let source = manifest
            .child_source
            .as_ref()
            .context("worker source binding missing")?;
        let (_, constraints) = authority.scope.gate_inputs();
        let provider_catalog = crate::providers::catalog::CatalogView::restore_process(
            &source.provider_catalog,
            &constraints.key,
            constraints.decision_id,
        )?;
        Some(crate::runtime::child_source::IssuedChildGrant::restore_process(
            Arc::clone(&authority.scope),
            provenance,
            Arc::clone(&authority.catalog),
            Arc::clone(child_generation.as_ref().context("worker child generation missing")?),
            provider_catalog,
            (
                Arc::from(source.parent_store_workspace.as_str()),
                source.parent_decision_digest.clone(),
            ),
            crate::runtime::child_source::ChildGrantRuntime {
                decision_store: Arc::clone(&authority.store),
                source_fabric: decision_fabric
                    .as_ref()
                    .context("worker logical source fabric missing")?
                    .clone(),
                security: Arc::clone(&security),
            },
            source_receipt.as_deref().context("worker child input missing")?,
            &manifest,
            transport
                .as_ref()
                .context("worker execution workspace is not authenticated")?,
        )?)
    } else {
        None
    };

    // Logical completion belongs to the authenticated original spawn input.
    // Worker model segments and worker-result events keep their own scope.
    let logical_task_scope = if child_grant.is_some() {
        source_receipt
            .as_deref()
            .context("worker logical input missing")?
            .logical_task_scope()
    } else {
        worker_event_scope.clone()
    };

    // Capture completion-only authority before the driver can be cancelled.
    // Model segment authority remains independently checked per segment.
    let logical_terminal = match (
        child_grant.as_deref(),
        ordinary_authority.as_deref(),
        operator_authority.as_deref(),
    ) {
        (Some(grant), None, None) => crate::agent::terminal::logical::LogicalTaskTerminalRef::granted(
            grant,
            source_receipt.as_deref().context("worker logical input missing")?,
            &logical_task_scope,
        )?,
        (None, Some(authority), None) => {
            crate::agent::terminal::logical::LogicalTaskTerminalRef::ordinary_process(authority)?
        }
        (None, None, Some(authority)) => {
            crate::agent::terminal::logical::LogicalTaskTerminalRef::operator_process(authority)?
        }
        _ => anyhow::bail!("worker logical completion requires one authenticated source authority"),
    };

    let child_provider_binding = child_grant
        .as_ref()
        .map(|grant| {
            crate::runtime::child_source::ChildProviderBinding::build_worker(Arc::clone(grant), &manifest).map(Arc::new)
        })
        .transpose()?;
    let provider: Arc<dyn Provider> = if let Some(binding) = child_provider_binding.as_ref() {
        binding.provider()
    } else if let Some(authority) = &operator_authority {
        authority.binding().provider()
    } else if let Some(authority) = &ordinary_authority {
        authority.binding().provider()
    } else {
        legacy_provider.context("legacy worker provider missing")?
    };
    let child_source_proof = if let Some(binding) = child_provider_binding.as_ref() {
        Some(binding.source_proof(
            child_grant.as_deref().context("worker issued grant missing")?,
            source_receipt.as_deref().context("worker child input missing")?,
            &provider,
        )?)
    } else if let Some(authority) = &operator_authority {
        Some(authority.source_proof()?)
    } else if let Some(authority) = &ordinary_authority {
        Some(authority.source_proof()?)
    } else {
        None
    };

    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let shared_config = crate::config::new_shared(config.clone());
    let hooks = Arc::new(HookManager::new(manifest.workspace_dir.clone()).with_config(Arc::clone(&shared_config)));
    #[cfg(feature = "wasm-plugins")]
    if let Some(plugin_runtime) = &wasm_plugin_runtime {
        hooks.set_plugin_runtime(Arc::clone(plugin_runtime)).await;
    }

    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(plugin_runtime) = &wasm_plugin_runtime {
        extensions.extend(plugin_runtime.control_tool_arcs(Arc::clone(&security)));
    }
    let mut full_tools = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        shared_config.clone(),
        &security,
        runtime,
        memory.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &manifest.workspace_dir,
        &config.agents_with_references(),
        credentials.root_api_key,
        &config,
        extensions,
    )
    .tools;

    // Recreate the same orchestration surfaces that owning entrypoints append
    // after the base factory. Without this block process workers silently lose
    // sessions_spawn and its shared-registry siblings even with wildcard tool
    // inheritance.
    let active_runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>> =
        Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let session_fabric = decision_fabric.clone().unwrap_or_else(|| {
        MemoryFabric::new(
            Arc::clone(&memory),
            manifest.workspace_dir.to_string_lossy().to_string(),
        )
        .with_event_recording(config.memory.event_recording_config())
    });
    let session_memory = session_fabric.memory();
    let workspace_id = session_fabric.workspace_id().to_owned();
    let spawn_tool = worker_spawn_tool(
        &manifest,
        &config,
        &credentials,
        Arc::clone(&provider),
        Arc::clone(&security),
        &provider_runtime_options,
        Arc::clone(&active_runs),
    )
    .with_shared_memory(Arc::clone(&memory))
    .with_source_fabric(session_fabric.clone())
    .with_event_recording(config.memory.event_recording_config());
    let spawn_tools_handle = spawn_tool.tools_handle();
    full_tools.push(Box::new(
        crate::tools::SessionsListTool::new(Arc::clone(&active_runs))
            .with_shared_memory(Arc::clone(&session_memory), workspace_id.clone()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionsSendTool::with_security(Arc::clone(&active_runs), Arc::clone(&security))
            .with_shared_memory(Arc::clone(&session_memory))
            .with_source_fabric(session_fabric.clone())
            .with_event_recording(config.memory.event_recording_config()),
    ));
    full_tools.push(Box::new(
        crate::tools::SubagentsTool::with_security(Arc::clone(&active_runs), Arc::clone(&security))
            .with_shared_memory(Arc::clone(&session_memory))
            .with_source_fabric(session_fabric.clone())
            .with_event_recording(config.memory.event_recording_config()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionsHistoryTool::new(Arc::clone(&active_runs))
            .with_shared_memory(Arc::clone(&session_memory), workspace_id.clone()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionStatusTool::new(
            Arc::clone(&active_runs),
            &manifest.provider_name,
            &manifest.model,
            vec!["session-worker".to_string()],
        )
        .with_shared_memory(Arc::clone(&session_memory), workspace_id),
    ));
    full_tools.push(Box::new(spawn_tool));
    full_tools.push(Box::new(crate::tools::DaemonMessageSendTool::from_config(
        &config,
        Arc::clone(&security),
    )));
    full_tools.push(Box::new(
        crate::tools::ImageTool::new(
            Arc::clone(&provider),
            &manifest.model,
            manifest.temperature,
            Arc::clone(&security),
            config.multimodal.clone(),
        )
        .with_provider_name(&manifest.provider_name)
        .with_model_catalog(crate::providers::selection::catalog_for(&config)),
    ));
    full_tools.push(Box::new(crate::tools::ConfigReloadTool::with_security(
        shared_config.clone(),
        Arc::clone(&security),
    )));
    full_tools.push(Box::new(
        crate::tools::GatewayTool::new(
            shared_config,
            &manifest.provider_name,
            &manifest.model,
            vec!["session-worker".to_string()],
        )
        .with_tools_count(full_tools.len() + 1)
        .with_security(Arc::clone(&security)),
    ));

    verify_worker_orchestration_tool_names(full_tools.iter().map(|tool| tool.name()))?;

    let tools_registry = select_tools_for_manifest(full_tools, &manifest).await?;
    let _ = spawn_tools_handle.set(&tools_registry);
    let child_service = if let Some(grant) = child_grant.as_ref() {
        Some(
            crate::runtime::turn_publication::ChildDecisionPublication {
                allowed_tools: None,
                grant: Arc::clone(grant),
                registry: Arc::clone(&tools_registry),
                observer: Arc::new(NoopObserver),
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &ordinary_authority {
        let mut excluded_names: std::collections::BTreeSet<String> =
            config.tool_tiering.always_exclude.iter().cloned().collect();
        excluded_names.extend(manifest.channel_excluded_tools.iter().cloned());
        Some(
            crate::runtime::turn_publication::OrdinaryProcessDecisionPublication {
                authority: Arc::clone(authority),
                registry: Arc::clone(&tools_registry),
                security: Arc::clone(&security),
                runtime: crate::runtime::turn_decision::TurnLoopRuntime {
                    hooks: Arc::clone(&hooks),
                    observer: Arc::new(NoopObserver),
                },
                surface_policy: crate::tools::capability::catalog::SurfacePolicy {
                    allowed_tools: if manifest.allowed_tools.is_empty()
                        || manifest.allowed_tools.iter().any(|name| name == "*")
                    {
                        None
                    } else {
                        Some(manifest.allowed_tools.iter().cloned().collect())
                    },
                    excluded_names,
                    direct_names: config.tool_tiering.always_include.iter().cloned().collect(),
                    channel_allowed: false,
                },
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &operator_authority {
        let mut excluded_names: std::collections::BTreeSet<String> =
            config.tool_tiering.always_exclude.iter().cloned().collect();
        excluded_names.extend(manifest.channel_excluded_tools.iter().cloned());
        Some(
            crate::runtime::turn_publication::OperatorProcessDecisionPublication {
                authority: Arc::clone(authority),
                registry: Arc::clone(&tools_registry),
                security: Arc::clone(&security),
                runtime: crate::runtime::turn_decision::TurnLoopRuntime {
                    hooks: Arc::clone(&hooks),
                    observer: Arc::new(NoopObserver),
                },
                surface_policy: crate::tools::capability::catalog::SurfacePolicy {
                    allowed_tools: if manifest.allowed_tools.is_empty()
                        || manifest.allowed_tools.iter().any(|name| name == "*")
                    {
                        None
                    } else {
                        Some(manifest.allowed_tools.iter().cloned().collect())
                    },
                    excluded_names,
                    direct_names: config.tool_tiering.always_include.iter().cloned().collect(),
                    channel_allowed: false,
                },
            }
            .publish()
            .await?,
        )
    } else {
        None
    };
    let native_tools = match (child_service.as_ref(), child_source_proof.as_ref()) {
        (Some(service), Some(proof)) => service.model_can_call_tools(proof),
        (None, None) => {
            provider
                .capabilities_for(
                    &manifest.model,
                    crate::providers::traits::ProviderRequestMode::NonStreaming,
                )
                .native_tool_calling
        }
        _ => anyhow::bail!("worker child service and source proof differ"),
    };
    let provider_started_at = chrono::Utc::now();

    let child_segments = if let Some(grant) = child_grant.as_ref() {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Granted {
                sequence: tokio::sync::Mutex::new(crate::runtime::child_source::ChildSegmentSequence::new_worker(
                    Arc::clone(grant),
                    Arc::clone(source_receipt.as_ref().context("worker child input missing")?),
                )?),
                grant: Arc::clone(grant),
                binding: Arc::clone(
                    child_provider_binding
                        .as_ref()
                        .context("worker provider binding missing")?,
                ),
                provider: Arc::clone(&provider),
            },
            service: Arc::clone(child_service.as_ref().context("worker child service missing")?),
            fabric: decision_fabric
                .as_ref()
                .context("worker decision storage missing")?
                .clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("worker cleanup owner missing")?,
            usage: Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new(
                grant.parent().workspace_id().to_owned(),
                grant.parent().owner_id().to_owned(),
                manifest.run_id.clone(),
            )),
        }))
    } else if let Some(authority) = &ordinary_authority {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Ordinary {
                sequence: tokio::sync::Mutex::new(authority.segments()?),
                authority: Arc::clone(authority),
            },
            service: Arc::clone(child_service.as_ref().context("ordinary worker service missing")?),
            fabric: authority.source_fabric().clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("ordinary worker cleanup owner missing")?,
            usage: Arc::new(
                crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                    authority.snapshots().parent.workspace_id().to_owned(),
                    authority.snapshots().parent.owner_id().to_owned(),
                    manifest.run_id.clone(),
                    authority
                        .snapshots()
                        .generation
                        .effective
                        .memory
                        .event_recording_config(),
                )
                .with_ordinary_journal(
                    crate::runtime::child_source::ordinary_process::journal::OrdinarySegmentJournal::new(authority)?,
                ),
            ),
        }))
    } else if let Some(authority) = &operator_authority {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Operator {
                sequence: tokio::sync::Mutex::new(authority.segments()?),
                authority: Arc::clone(authority),
            },
            service: Arc::clone(child_service.as_ref().context("operator worker service missing")?),
            fabric: authority.source_fabric().clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("operator worker cleanup owner missing")?,
            usage: Arc::new(
                crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                    authority.snapshots().parent.workspace_id().to_owned(),
                    authority.snapshots().parent.owner_id().to_owned(),
                    manifest.run_id.clone(),
                    authority
                        .snapshots()
                        .generation
                        .effective
                        .memory
                        .event_recording_config(),
                )
                .with_operator_journal(
                    crate::runtime::child_source::operator_process::journal::OperatorSegmentJournal::new(authority)?,
                ),
            ),
        }))
    } else {
        None
    };

    let run_future = boxed_worker_segments(|| async {
        let initial_history = Vec::new();

        let observer = NoopObserver;
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
                    session_key: Some(manifest.session_scope_key.as_str()),
                    owner_id: manifest.owner_id.as_deref(),
                    owner_isolated: manifest.owner_isolated,
                    topic_id: manifest.topic_id.as_deref(),
                    task_id: manifest.parent_task_id.as_deref(),
                    source_message_event_id: manifest.source_message_event_id.as_deref(),
                    config_generation_id: manifest.runtime_config_generation_id,
                    config_source_revision: manifest.runtime_config_source_revision.as_deref(),
                })
            }
            _ => None,
        };
        // Borrow everything the segment needs once, outside the closure: an
        // `async move` segment future must capture `Copy` references, never move
        // fields out of the `FnMut` closure's captured environment.
        let provider_ref = provider.as_ref();
        let observer_ref = &observer;
        let hooks_ref = hooks.as_ref();
        let scope_ctx_ref = scope_ctx.as_ref();
        let memory_ref = &memory;
        let provider_name_ref: &str = &manifest.provider_name;
        let model_ref: &str = &manifest.model;
        let workspace_dir_ref: &Path = &manifest.workspace_dir;
        let compaction_config_ref = manifest.compaction_config.as_ref();
        let multimodal_ref = &config.multimodal;
        let low_priority_tools_ref: &[String] = &config.agent.low_priority_tools;
        let temperature = manifest.temperature;
        let read_only_window = config.agent.read_only_tool_concurrency_window;
        let priority_scheduling_enabled = config.agent.priority_scheduling_enabled;
        let tools_registry_ref = &tools_registry;
        let memory_fabric_ref = &memory_fabric;
        let child_segments_ref = &child_segments;
        let security_ref = security.as_ref();
        let raw_task = manifest.task.as_str();
        let manifest_ref = &manifest;
        let config_ref = &config;
        let (history, loop_result) = run_segments_with_steering(
            initial_history,
            &mut steer_rx,
            |mut segment_history, cancel, raw_steer| {
                let tools_registry = Arc::clone(tools_registry_ref);
                let memory = Arc::clone(memory_ref);
                let child_segments = child_segments_ref.clone();
                let request_event_fabric = child_segments
                    .as_ref()
                    .map_or_else(|| memory_fabric_ref.clone(), |segments| segments.fabric.clone());
                async move {
                    let parent = crate::runtime::turn_cleanup::current_tracker();
                    let mut cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
                        parent.as_ref(),
                        CancellationToken::new(),
                    ) {
                        Ok(cleanup) => cleanup,
                        Err(error) => {
                            return (
                                segment_history,
                                Err(error.context(crate::runtime::turn_cleanup::CleanupIncomplete)),
                            );
                        }
                    };
                    let child_turn = if let Some(segments) = child_segments.as_ref() {
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        cleanup = cleanup.with_acknowledgement(Some(sender));
                        let steering_input = raw_steer.as_deref().map(steering_instruction);
                        match boxed_worker_segments(|| {
                            segments.admit(steering_input.as_deref().unwrap_or(raw_task), &cancel, receiver)
                        })
                        .await
                        {
                            Ok(turn) => Some(turn),
                            Err(error) => return (segment_history, Err(error)),
                        }
                    } else {
                        None
                    };
                    if let Some(turn) = child_turn.as_ref() {
                        match turn.is_close() {
                            Ok(true) => {
                                let closed = async {
                                    cleanup.run(async { Ok(()) }).await?;
                                    let reply =
                                        turn.close(&cancel).await?.context("worker close disposition changed")?;
                                    Ok((reply, crate::agent::turn_contracts::ToolLoopTrace::default()))
                                }
                                .await;
                                if let Ok((reply, _)) = &closed {
                                    segment_history.push(ChatMessage::assistant(reply.clone()));
                                }
                                return (segment_history, closed);
                            }
                            Ok(false) => {}
                            Err(error) => return (segment_history, Err(error)),
                        }
                    }
                    let initial_input = segment_history.is_empty();
                    let selected_prompt = if let Some(turn) = child_turn.as_ref() {
                        match turn.prompt_skills() {
                            Ok(skills) => Some(build_worker_system_prompt_with_skills(
                                manifest_ref,
                                config_ref,
                                native_tools,
                                &skills,
                            )),
                            Err(error) => return (segment_history, Err(error)),
                        }
                    } else if segment_history.first().is_none_or(|message| message.role != "system") {
                        Some(build_worker_system_prompt(manifest_ref, config_ref, native_tools))
                    } else {
                        None
                    };
                    if let Some(prompt) = selected_prompt {
                        if let Some(system) = segment_history.first_mut().filter(|message| message.role == "system") {
                            *system = ChatMessage::system(prompt);
                        } else {
                            segment_history.insert(0, ChatMessage::system(prompt));
                        }
                    }
                    if initial_input {
                        let user_task = if child_turn.is_some() {
                            raw_task.to_owned()
                        } else {
                            let shared_context =
                                load_worker_shared_context(manifest_ref, config_ref, memory.as_ref()).await;
                            if shared_context.trim().is_empty() {
                                raw_task.to_owned()
                            } else {
                                format!("{shared_context}{raw_task}")
                            }
                        };
                        segment_history.push(crate::agent::turn_contracts::inbound_user_message(&user_task));
                    }
                    if let Some(turn) = child_turn.as_ref() {
                        if let Err(error) = turn.enrich_input(&mut segment_history) {
                            let result = cleanup.run(async { Err::<(), _>(error) }).await;
                            let error = result
                                .err()
                                .unwrap_or_else(|| anyhow::anyhow!("worker enrichment failed"));
                            if let Err(settlement) = turn.fail(&error, cancel.is_cancelled()).await {
                                return (segment_history, Err(settlement));
                            }
                            return (segment_history, Err(error));
                        }
                    }
                    let Some(admitted_turn) = child_turn.as_ref() else {
                        return (
                            segment_history,
                            Err(anyhow::anyhow!("worker decision admission missing")),
                        );
                    };
                    let adapter = child_turn.as_ref().map(|turn| turn.adapter());
                    let child_execution = adapter.as_ref().map(|adapter| adapter.tool_execution_context.clone());
                    let child_scope = child_execution.as_ref().map(|context| {
                        let envelope = &context.envelope;
                        ScopeContext::from_execution_context(
                            security_ref,
                            context,
                            envelope.recipient.as_deref().unwrap_or_default(),
                        )
                    });
                    let loop_scope = child_scope.as_ref().or(scope_ctx_ref);
                    let loop_result = cleanup
                        .run(run_turn(
                            &mut segment_history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "session-worker",
                                scope: loop_scope,
                                mode: crate::agent::turn_contracts::ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: multimodal_ref,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: read_only_window,
                                    priority_scheduling_enabled,
                                    low_priority_tool_names: low_priority_tools_ref.to_vec(),
                                },
                                compaction: child_turn
                                    .as_ref()
                                    .map(|turn| turn.compaction())
                                    .or(compaction_config_ref),
                                // The parent already selected and sealed this worker's
                                // capability set. Reapplying language-dependent intent
                                // tiering here would silently hide inherited tools and
                                // make process mode weaker than task mode.
                                decision: admitted_turn.routing(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: provider_ref,
                                provider_name: provider_name_ref,
                                model: model_ref,
                                temperature,
                                tools_registry,
                                observer: observer_ref,
                                hooks: hooks_ref,
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: adapter,
                                // The ledger comes from `memory` directly: a session worker without a
                                // resolved ingest scope must still be able to run side-effecting tools.
                                memory: crate::agent::turn_request::ToolLoopMemory::new(
                                    &memory,
                                    workspace_dir_ref,
                                    loop_scope.map(|ctx| {
                                        DocumentIngestRuntime::from_scope(Arc::clone(&memory), ctx).into_nested_turn()
                                    }),
                                )
                                .with_event_fabric(request_event_fabric),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                // Steering cancels the in-flight segment; without a token the
                                // worker could only append after the loop finished on its own.
                                cancellation: Some(cancel.clone()),
                                on_delta: None,
                                on_tool_call: None,
                            },
                        ))
                        .await;
                    if let Some(turn) = child_turn.as_ref() {
                        let settlement = match &loop_result {
                            Ok((outcome, trace)) => turn.complete(outcome, trace).await.map(|_| ()),
                            Err(error) => turn.fail(error, cancel.is_cancelled()).await,
                        };
                        if let Err(error) = settlement {
                            return (segment_history, Err(error));
                        }
                    }
                    let loop_result = loop_result.map(|(outcome, trace)| (outcome.into_text(), trace));
                    (segment_history, loop_result)
                }
            },
        )
        .await;
        (loop_result, history.len())
    });

    let run_future = with_manifest_spawn_context(
        &manifest,
        source_receipt,
        child_grant.clone(),
        child_provider_binding.clone(),
        child_segments.as_ref().map(|segments| Arc::clone(&segments.usage)),
        run_future,
    );

    let result = run_future.await;

    let (loop_result, history_commit_len) = result;
    let (mut worker_result, terminal_status) = match loop_result {
        Ok((output, trace)) => {
            let tokens_used = trace.tokens_used;
            (
                WorkerResult {
                    completion_evidence: None,
                    ordinary_completion: None,
                    operator_completion: None,
                    success: true,
                    output: if output.trim().is_empty() {
                        "[Sub-agent produced no output]".to_string()
                    } else {
                        output
                    },
                    error: None,
                    tokens_used: tokens_used.has_any_tokens().then_some(tokens_used),
                },
                crate::agent::terminal::TurnTerminalStatus::Completed,
            )
        }
        Err(error) => (
            WorkerResult {
                completion_evidence: None,
                ordinary_completion: None,
                operator_completion: None,
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
                tokens_used: None,
            },
            crate::agent::terminal::TurnTerminalStatus::Failed,
        ),
    };

    if let Some(segments) = child_segments.as_ref() {
        let recorded = segments.recorded_count().await;
        let confirmed = u64::try_from(segments.usage.confirmed_count())?;
        anyhow::ensure!(
            !worker_result.success || (recorded > 0 && recorded == confirmed),
            "worker cannot commit completion with unconfirmed segments"
        );
    }

    // Shared child segments already own provider settlement. The logical
    // worker terminal records only completion and delivery, in the same
    // logical source store as its admitted input and segment terminals.
    let terminal_fabric = decision_fabric.as_ref().unwrap_or(&memory_fabric);
    let terminal_receipt = logical_terminal
        .finalize(
            terminal_fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: None,
                terminal_id: manifest.run_id.clone(),
                scope: logical_task_scope.clone(),
                status: terminal_status,
                history: worker_result
                    .success
                    .then(|| crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: worker_result.output.clone(),
                        history_commit_len,
                    }),
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: if worker_result.success {
                        "session worker completed".to_string()
                    } else {
                        worker_result
                            .error
                            .clone()
                            .unwrap_or_else(|| "session worker failed".to_string())
                    },
                    started_at: provider_started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Deferred {
                    route: "session_worker_callback".to_string(),
                },
            },
        )
        .await
        .context("commit session-worker terminal before publishing result")?;
    if child_segments.is_some() {
        let event = &terminal_receipt.terminal_event;
        let expected_key = format!("turn:{}:final", manifest.run_id);
        anyhow::ensure!(
            (if manifest.memory_event_recording.enabled {
                event.id > 0
            } else {
                child_grant.is_none() && event.id == 0
            }) && terminal_receipt.terminal_id == manifest.run_id
                && terminal_receipt.usage_settlement.is_none()
                && event.workspace_id == terminal_fabric.workspace_id()
                && event.owner_id == logical_task_scope.owner_id
                && event.run_id == logical_task_scope.run_id
                && event.session_key == logical_task_scope.session_key
                && event.source == logical_task_scope.source
                && event.event_type == "turn.finalized"
                && event.idempotency_key.as_deref() == Some(expected_key.as_str()),
            "worker logical terminal differs from admitted scope"
        );
        let owner = logical_task_scope
            .owner_id
            .as_deref()
            .context("worker terminal owner missing")?;
        if event.id > 0 {
            let stored = terminal_fabric
                .memory()
                .find_owned_message_event(terminal_fabric.workspace_id(), owner, &event.event_id)
                .await?
                .context("worker logical terminal is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(event)?,
                "worker logical terminal differs from durable record"
            );
        }
    }

    if let (Some(segments), Some(grant)) = (child_segments.as_ref(), child_grant.as_ref()) {
        use sha2::Digest;
        let (child, constraints) = grant.scope().gate_inputs();
        let mut evidence = crate::session_worker::protocol::WorkerCompletionEvidence {
            run_id: manifest.run_id.clone(),
            input_event_id: manifest
                .child_source
                .as_ref()
                .context("worker child source missing")?
                .input_event_id
                .clone(),
            terminal_event_id: terminal_receipt.terminal_event.event_id.clone(),
            terminal_event_digest: format!(
                "{:x}",
                sha2::Sha256::digest(serde_json::to_vec(&terminal_receipt.terminal_event)?)
            ),
            recorded_segments: segments.recorded_count().await,
            segments: segments.usage.terminal_references(),
            seal: [0; 32],
        };
        evidence.validate_inventory(worker_result.success)?;
        evidence.seal = constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"worker-completion-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &evidence.signing_bytes(&worker_result)?,
            ],
        );
        worker_result.completion_evidence = Some(evidence);
    }

    if let (Some(segments), Some(authority)) = (child_segments.as_ref(), ordinary_authority.as_ref()) {
        use sha2::Digest;
        authority.verify()?;
        let terminal = if manifest.memory_event_recording.enabled {
            Some(crate::session_worker::protocol::WorkerSegmentTerminal {
                terminal_id: manifest.run_id.clone(),
                event_id: terminal_receipt.terminal_event.event_id.clone(),
                event_digest: format!(
                    "{:x}",
                    sha2::Sha256::digest(serde_json::to_vec(&terminal_receipt.terminal_event)?)
                ),
            })
        } else {
            None
        };
        let mut evidence = crate::session_worker::protocol::OrdinaryWorkerCompletionEvidence {
            run_id: manifest.run_id.clone(),
            process_id: authority.id().to_string(),
            input_event_id: authority.input().map(|receipt| receipt.event().event_id.clone()),
            recording_enabled: manifest.memory_event_recording.enabled,
            terminal,
            recorded_segments: segments.recorded_count().await,
            confirmed_segments: segments.usage.confirmed_terminal_ids(),
            segments: segments.usage.terminal_references(),
            usage: segments.usage.snapshot(),
            seal: [0; 32],
        };
        evidence.validate_inventory(worker_result.success)?;
        let (child, constraints) = authority.snapshots().scope.gate_inputs();
        evidence.seal = constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"ordinary-worker-completion-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &evidence.signing_bytes(&worker_result)?,
            ],
        );
        authority.verify()?;
        worker_result.ordinary_completion = Some(evidence);
    }

    if let (Some(segments), Some(authority)) = (child_segments.as_ref(), operator_authority.as_ref()) {
        authority.seal_completion(
            &mut worker_result,
            &terminal_receipt,
            segments.recorded_count().await,
            &segments.usage,
        )?;
    }

    // The result event and the hybrid memory draft persist the worker's reply
    // or error text: redacted like the terminal commit (D-053).
    let event_content = crate::agent::terminal::redact_persisted_text(if worker_result.output.trim().is_empty() {
        worker_result
            .error
            .as_deref()
            .unwrap_or("[session-worker produced no output]")
    } else {
        worker_result.output.as_str()
    });
    SideEffectGate::new(security.as_ref())
        .authorize_resource_operation(
            "session_worker",
            &format!("session_worker:result_event:{}", manifest.run_id),
            ResourceRiskLevel::Low,
            None,
        )
        .map_err(anyhow::Error::msg)?;
    let worker_result_event = match memory_fabric
        .record_worker_result(
            worker_event_scope.clone(),
            crate::agent::terminal::redact_persisted_text(&event_content),
            Some(
                crate::agent::terminal::redact_persisted_json(serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
                }))
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
        security.as_ref(),
    )
    .await;

    let completion = child_grant.map(|grant| PendingWorkerCompletion {
        fabric: terminal_fabric.clone(),
        scope: worker_event_scope,
        grant,
    });
    Ok(PreparedWorkerResult {
        _ordinary_lifetime: ordinary_lifetime,
        _operator_lifetime: operator_lifetime,
        operator_completion: operator_authority,
        result: worker_result,
        completion,
        ordinary_completion: ordinary_authority,
    })
}

/// Validate the sealed manifest, scrub the capability material, and only then
/// start the run.
///
/// `read_stdin_control_frames` asks for the post-manifest stdin reader. It is
/// started **after** `scrub_capability_env`, never before: that scrub uses
/// `unsafe { remove_var }`, whose soundness argument is that nothing else in
/// this process can be reading the environment while it runs (see the SAFETY
/// note on `scrub_capability_env` for what actually holds — the worker is
/// emphatically *not* single-threaded here). The reader is the first thread
/// this path starts, so starting it earlier would break exactly that argument,
/// and the steering protocol stays strictly downstream of authentication.
async fn run_manifest(
    manifest: WorkerManifest,
    read_stdin_control_frames: bool,
    explicit_config_dir: Option<&str>,
) -> Result<WorkerResult> {
    let env_capability = std::env::var("OPENPRX_SESSION_WORKER_CAPABILITY").ok();
    // Validate up front, then scrub the capability material from the environment
    // so it cannot leak to any grandchild process or be re-read after boot.
    let validation = validate_worker_manifest_with_capability_env(&manifest, env_capability.as_deref());
    scrub_capability_env();
    let transport = validation?;
    let steer_rx = read_stdin_control_frames.then(|| {
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
        spawn_stdin_control_reader(steer_tx);
        steer_rx
    });
    run_authenticated_manifest(manifest, steer_rx, explicit_config_dir, Some(transport)).await
}

/// Remove the capability-bearing environment variables after validation.
fn scrub_capability_env() {
    // SAFETY: `remove_var` is unsafe on edition 2024 because mutating the
    // process environment races with a concurrent `getenv` on another thread.
    // The obligation is therefore "no other thread touches the environment for
    // the duration of this call", which is *not* the same as the process being
    // single-threaded — and single-threaded is what this comment used to claim
    // and is plainly false: `main` builds a multi-threaded tokio runtime before
    // `async_main` runs, so scheduler threads already exist by the time any of
    // this executes.
    //
    // What actually holds is an ordering property of the session-worker entry
    // path, and all of it is on one task:
    //
    // * `async_main` dispatches to `Commands::SessionWorker` (`src/main.rs`)
    //   having done nothing but install the rustls provider and parse the CLI:
    //   no `tokio::spawn`, no `spawn_blocking`, no tracing subscriber. The
    //   runtime's worker threads therefore hold no work at all and sit in the
    //   scheduler park loop, which reads no environment.
    // * `run_from_stdin` reads and validates the manifest inline on this task.
    //   The one environment read on that stretch — the capability and its
    //   expiry — happens on this same thread, before this call.
    // * Every thread and process this worker later creates is created after
    //   this point: `spawn_stdin_control_reader` (deliberately sequenced after
    //   the scrub), everything `run_validated_manifest` builds, and any
    //   grandchild that would otherwise inherit the capability.
    //
    // So the safety is positional, not structural: it rests on nothing being
    // spawned earlier on this path, and no compiler check enforces that. Work
    // added to `async_main` ahead of the `SessionWorker` branch — a subscriber,
    // a background task, an eager pool — would reintroduce the race silently,
    // which is the reason the ordering is written down here rather than assumed.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("OPENPRX_SESSION_WORKER_CAPABILITY");
        std::env::remove_var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY");
    }
}

async fn record_hybrid_worker_draft_if_needed(
    manifest: &WorkerManifest,
    config: &Config,
    memory_fabric: &MemoryFabric,
    worker_event_scope: &MessageEventScope,
    worker_result: &WorkerResult,
    worker_result_event: Option<&MessageEvent>,
    event_content: &str,
    security: &SecurityPolicy,
) {
    if manifest.memory_strategy.as_deref() != Some("hybrid") || !worker_result.success {
        return;
    }

    if let Err(error) = SideEffectGate::new(security).authorize_resource_operation(
        "session_worker",
        &format!("session_worker:hybrid_draft:{}", manifest.run_id),
        ResourceRiskLevel::Low,
        None,
    ) {
        tracing::warn!(run_id = %manifest.run_id, "hybrid worker draft blocked by SideEffectGate: {error}");
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
                crate::agent::terminal::redact_persisted_json(serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "merge_policy": "parent_decides",
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
                }))
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

async fn load_worker_shared_context(manifest: &WorkerManifest, config: &Config, shared_memory: &dyn Memory) -> String {
    let strategy = manifest.memory_strategy.as_deref().unwrap_or("shared_fabric");
    if strategy == "isolated_private" {
        return String::new();
    }

    let workspace_id = if strategy == "hybrid" {
        shared_worker_workspace_id(manifest)
    } else {
        manifest
            .memory_workspace_id
            .clone()
            .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string())
    };

    let runtime_envelope = worker_runtime_envelope_for_workspace(manifest, workspace_id);
    let semantic_scope = match manifest.scope_chat_type.as_deref() {
        Some(chat_type)
            if !chat_type.is_empty()
                && manifest.scope_sender.as_deref().is_some_and(|value| !value.is_empty())
                && manifest.scope_channel.as_deref().is_some_and(|value| !value.is_empty())
                && manifest.scope_chat_id.as_deref().is_some_and(|value| !value.is_empty()) =>
        {
            Some(runtime_envelope.memory_write_context(chat_type))
        }
        _ => None,
    };

    // OWNER-VIS OV-D: the shared-events feed is limited to the reader's own
    // owner. A worker reads for the owner that spawned it — the owner its
    // manifest carries and its result events are recorded under — not for
    // the channel/sender composition of its own envelope.
    let mut principal = runtime_envelope.memory_principal();
    if let Some(owner_id) = manifest
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
    {
        principal.owner_id = Some(owner_id.to_string());
    }
    build_context_with_shared_events_and_scope(
        shared_memory,
        principal,
        &manifest.task,
        config.memory.min_relevance_score,
        semantic_scope.as_ref(),
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

fn worker_runtime_envelope(manifest: &WorkerManifest) -> RuntimeEnvelope {
    let workspace_id = manifest
        .memory_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string());
    worker_runtime_envelope_for_workspace(manifest, workspace_id)
}

fn worker_runtime_envelope_for_workspace(manifest: &WorkerManifest, workspace_id: String) -> RuntimeEnvelope {
    let mut envelope = RuntimeEnvelope::session_worker(
        workspace_id,
        worker_session_scope_key(manifest),
        manifest.run_id.clone(),
    )
    .with_channel(manifest.scope_channel.as_deref().unwrap_or("session_worker"));

    if let Some(agent_id) = manifest.agent_id.as_deref() {
        envelope = envelope.with_agent_id(agent_id);
    }
    if let Some(persona_id) = manifest.persona_id.as_deref() {
        envelope = envelope.with_persona_id(persona_id);
    }
    if let Some(parent_run_id) = manifest.parent_run_id.as_deref() {
        envelope = envelope.with_parent_run_id(parent_run_id);
    }
    if let Some(sender) = manifest.scope_sender.as_deref() {
        envelope = envelope.with_sender(sender);
    }
    if let Some(chat_id) = manifest.scope_chat_id.as_deref() {
        envelope = envelope.with_recipient(chat_id);
    }
    if let Some(owner_id) = manifest.owner_id.as_deref().filter(|owner_id| !owner_id.is_empty()) {
        envelope = envelope.with_owner_id(owner_id);
    }
    if manifest.owner_isolated {
        envelope = envelope.with_owner_isolation();
    }
    envelope.config_generation_id = manifest.runtime_config_generation_id;
    envelope.config_source_revision = manifest.runtime_config_source_revision.clone();
    envelope
}

fn worker_message_event_scope(manifest: &WorkerManifest) -> MessageEventScope {
    let mut scope = worker_runtime_envelope(manifest).message_scope();
    if let Some(owner_id) = manifest.owner_id.as_deref().filter(|value| !value.is_empty()) {
        scope.owner_id = Some(owner_id.to_string());
    }
    scope
}

fn worker_lineage_payload(manifest: &WorkerManifest) -> serde_json::Value {
    serde_json::json!({
        "owner_id": manifest.owner_id.as_deref(),
        "topic_id": manifest.topic_id.as_deref(),
        "parent_task_id": manifest.parent_task_id.as_deref(),
        "source_message_event_id": manifest.source_message_event_id.as_deref(),
        "parent_run_id": manifest.parent_run_id.as_deref(),
        "session_scope_key": manifest.session_scope_key.as_str(),
        "spawn_depth": manifest.spawn_depth
    })
}

#[inline(never)]
fn boxed_worker_segments<F, Fut>(make_work: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: Future + Send,
{
    Box::pin(make_work())
}

async fn with_manifest_spawn_context<T, Fut>(
    manifest: &WorkerManifest,
    input_receipt: Option<Arc<crate::runtime::child_source::ChildInputReceipt>>,
    incoming_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    provider_binding: Option<Arc<crate::runtime::child_source::ChildProviderBinding>>,
    terminal_usage: Option<Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    fut: Fut,
) -> T
where
    Fut: Future<Output = T>,
{
    if !manifest.session_scope_key.trim().is_empty() {
        let source_message_event_id = if incoming_grant.is_some() {
            input_receipt.as_ref().map(|receipt| receipt.event().event_id.clone())
        } else {
            manifest.source_message_event_id.clone()
        };
        SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    operator_authority: None,
                    ordinary_authority: None,
                    incoming_grant,
                    provider_binding,
                    terminal_usage,
                    input_receipt,
                    run_id: manifest.run_id.clone(),
                    session_scope_key: manifest.session_scope_key.clone(),
                    spawn_depth: manifest.spawn_depth,
                    owner_id: manifest.owner_id.clone(),
                    owner_isolated: manifest.owner_isolated,
                    topic_id: manifest.topic_id.clone(),
                    source_message_event_id,
                    // A resumed spawn-run process (not a turn root): children
                    // compute spawn_depth + 1, preserving the manifest depth chain.
                    is_turn_root: false,
                    // The spawning surface's exclusions travel in the sealed
                    // manifest, so everything this worker spawns in turn — task
                    // or process — stays inside the same boundary.
                    channel_excluded_tools: manifest.channel_excluded_tools.clone(),
                },
                fut,
            )
            .await
    } else {
        fut.await
    }
}

/// Accept legacy inline JSON and authenticated local reference envelopes.
fn parse_worker_manifest_line(raw: &str) -> Result<WorkerManifest> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    if value.get("worker_manifest_reference").is_some() {
        anyhow::ensure!(raw.len() < 3072, "worker reference exceeds stdin byte bound");
        #[cfg(unix)]
        {
            let capability = std::env::var("OPENPRX_SESSION_WORKER_CAPABILITY")
                .context("worker reference capability env missing")?;
            let expiry = capability_expiry_from_env()?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            super::manifest_reference::restore(value, &session_worker_signing_key(), &capability, expiry, now)
        }
        #[cfg(not(unix))]
        anyhow::bail!("worker manifest references are unsupported on this platform")
    } else {
        serde_json::from_str(raw).map_err(Into::into)
    }
}

pub async fn run_from_stdin(
    task: Option<String>,
    workspace: Option<String>,
    memory_db: Option<String>,
    tools: Option<String>,
    explicit_config_dir: Option<String>,
) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .read_line(&mut raw)
        .context("read worker manifest from stdin")?;

    let manifest: WorkerManifest = match parse_worker_manifest_line(raw.trim()) {
        Ok(value) => value,
        Err(error) => {
            let result = WorkerResult {
                completion_evidence: None,
                ordinary_completion: None,
                operator_completion: None,
                success: false,
                output: String::new(),
                error: Some(format!("Invalid worker manifest JSON: {error}")),
                tokens_used: None,
            };
            write_worker_result(&result)?;
            return Ok(());
        }
    };

    let parsed_tools = match tools.as_deref() {
        Some(tools_json) => Some(parse_tools_override(tools_json)?),
        None => None,
    };

    if let Err(error) = validate_worker_cli_overrides(
        &manifest,
        task.as_deref(),
        workspace.as_deref(),
        memory_db.as_deref(),
        parsed_tools.as_deref(),
        explicit_config_dir.as_deref(),
    ) {
        let result = WorkerResult {
            completion_evidence: None,
            ordinary_completion: None,
            operator_completion: None,
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
            tokens_used: None,
        };
        write_worker_result(&result)?;
        return Ok(());
    }

    let result = match run_manifest(manifest, true, explicit_config_dir.as_deref()).await {
        Ok(result) => result,
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<super::startup_failure::AuthenticatedStartupFailure>() {
                return write_worker_result(failure.envelope());
            }
            WorkerResult {
                completion_evidence: None,
                ordinary_completion: None,
                operator_completion: None,
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
                tokens_used: None,
            }
        }
    };

    write_worker_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod manifest_reference_tests {
        use super::*;
        use crate::session_worker::manifest_reference::{PreparedManifest, restore};
        use std::os::unix::fs::PermissionsExt as _;

        struct Fixture {
            workspace: tempfile::TempDir,
            key: ring::hmac::Key,
            manifest: WorkerManifest,
            capability: String,
            snapshot: PreparedManifest,
        }

        impl Fixture {
            fn new() -> Result<Self> {
                let workspace = tempfile::tempdir()?;
                let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx_manifest_reference_fixture");
                let mut manifest = base_manifest(workspace.path(), "");
                manifest.system_prompt = Some("prx reference roundtrip ".repeat(256));
                let capability = compute_worker_capability_with_key(
                    &key,
                    &manifest.run_id,
                    300,
                    &manifest_signing_payload(&manifest)?,
                );
                manifest.parent_capability = Some(capability.clone());
                let snapshot = PreparedManifest::prepare(&manifest, &key, 300)?;
                Ok(Self {
                    workspace,
                    key,
                    manifest,
                    capability,
                    snapshot,
                })
            }

            fn value(&self) -> Result<serde_json::Value> {
                Ok(serde_json::from_str(self.snapshot.line())?)
            }

            fn directory(&self) -> Result<PathBuf> {
                let value = self.value()?;
                Ok(PathBuf::from(
                    value
                        .pointer("/worker_manifest_reference/body/directory")
                        .and_then(serde_json::Value::as_str)
                        .context("reference directory missing")?,
                ))
            }

            fn restore(&self) -> Result<WorkerManifest> {
                restore(self.value()?, &self.key, &self.capability, 300, 0)
            }
        }

        #[test]
        fn reference_roundtrip_preserves_complete_manifest_and_original_capability() -> Result<()> {
            let mut fixture = Fixture::new()?;
            assert!(serde_json::to_vec(&fixture.manifest)?.len() > 3072);
            assert!(fixture.snapshot.line().len() < 3072);
            let restored = fixture.restore()?;
            assert_eq!(serde_json::to_vec(&restored)?, serde_json::to_vec(&fixture.manifest)?);
            let transport = authenticate_worker_capability_with_key(
                &restored,
                Some(&fixture.capability),
                300,
                0,
                fixture.key.clone(),
            )?;
            transport.verify_manifest(&restored)?;
            fixture.snapshot.close()?;
            Ok(())
        }

        #[test]
        fn reference_tampering_is_rejected_before_snapshot_access() -> Result<()> {
            let fixture = Fixture::new()?;
            let mut value = fixture.value()?;
            *value
                .pointer_mut("/worker_manifest_reference/body/directory")
                .context("reference directory missing")? = serde_json::json!("/prx_nonexistent_reference");
            let result = restore(value, &fixture.key, &fixture.capability, 300, 0);
            let error = result.err().context("tampered reference unexpectedly accepted")?;
            assert!(error.to_string().contains("signature is invalid"));
            assert!(!fixture.directory()?.join("claimed").exists());
            Ok(())
        }

        #[test]
        fn reference_wrong_channel_and_expiry_do_not_consume_snapshot() -> Result<()> {
            let fixture = Fixture::new()?;
            assert!(restore(fixture.value()?, &fixture.key, "prx_other_capability", 300, 0).is_err());
            assert!(restore(fixture.value()?, &fixture.key, &fixture.capability, 301, 0).is_err());
            assert!(restore(fixture.value()?, &fixture.key, &fixture.capability, 300, 301).is_err());
            assert!(!fixture.directory()?.join("claimed").exists());
            fixture.restore()?;
            Ok(())
        }

        #[test]
        fn reference_has_exactly_one_consumer_even_when_claims_race() -> Result<()> {
            let fixture = Fixture::new()?;
            let (first, second) = std::thread::scope(|scope| {
                let first = scope.spawn(|| fixture.restore().is_ok());
                let second = scope.spawn(|| fixture.restore().is_ok());
                (first.join(), second.join())
            });
            let first = first.map_err(|_| anyhow::anyhow!("first reference consumer panicked"))?;
            let second = second.map_err(|_| anyhow::anyhow!("second reference consumer panicked"))?;
            assert_ne!(first, second);
            assert!(fixture.restore().is_err());
            Ok(())
        }

        #[test]
        fn reference_refuses_symlink_leaf_and_ancestor() -> Result<()> {
            for ancestor in [false, true] {
                let fixture = Fixture::new()?;
                let directory = fixture.directory()?;
                let original = if ancestor {
                    directory.clone()
                } else {
                    directory.join("manifest.json")
                };
                let moved = fixture.workspace.path().join("prx_moved_snapshot");
                std::fs::rename(&original, &moved)?;
                std::os::unix::fs::symlink(&moved, &original)?;
                assert!(fixture.restore().is_err());
                std::fs::remove_file(&original)?;
                std::fs::rename(moved, original)?;
            }
            Ok(())
        }

        #[test]
        fn reference_refuses_writable_file_and_public_directory() -> Result<()> {
            for directory_case in [false, true] {
                let fixture = Fixture::new()?;
                let directory = fixture.directory()?;
                let path = if directory_case {
                    directory
                } else {
                    directory.join("manifest.json")
                };
                let mode = if directory_case { 0o755 } else { 0o600 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
                assert!(fixture.restore().is_err());
                let original_mode = if directory_case { 0o700 } else { 0o400 };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(original_mode))?;
            }
            Ok(())
        }

        #[test]
        fn reference_refuses_same_length_content_tampering_and_hardlinks() -> Result<()> {
            for hardlink in [false, true] {
                let fixture = Fixture::new()?;
                let path = fixture.directory()?.join("manifest.json");
                if hardlink {
                    std::fs::hard_link(&path, fixture.workspace.path().join("prx_extra_link"))?;
                } else {
                    let mut bytes = std::fs::read(&path)?;
                    *bytes.first_mut().context("snapshot empty")? = b'!';
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                    std::fs::write(&path, bytes)?;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
                }
                assert!(fixture.restore().is_err());
            }
            Ok(())
        }

        #[test]
        fn reference_lifetime_tracks_spawn_and_confirmed_cleanup() -> Result<()> {
            let fixture = Fixture::new()?;
            let path = fixture.directory()?;
            drop(fixture.snapshot);
            assert!(!path.exists(), "pre-spawn snapshot must be removed");

            let mut fixture = Fixture::new()?;
            let path = fixture.directory()?;
            fixture.snapshot.mark_spawned();
            drop(fixture.snapshot);
            assert!(path.exists(), "unconfirmed child retains its snapshot");
            std::fs::remove_dir_all(path)?;

            let mut fixture = Fixture::new()?;
            let path = fixture.directory()?;
            fixture.snapshot.mark_spawned();
            fixture.restore()?;
            fixture.snapshot.close()?;
            assert!(!path.exists(), "confirmed cleanup removes snapshot");
            Ok(())
        }

        #[test]
        fn legacy_inline_manifest_still_decodes_without_reference_environment() -> Result<()> {
            let workspace = tempfile::tempdir()?;
            let manifest = base_manifest(workspace.path(), "prx_inline_capability");
            let line = serde_json::to_string(&manifest)?;
            let decoded = parse_worker_manifest_line(&line)?;
            assert_eq!(serde_json::to_vec(&decoded)?, serde_json::to_vec(&manifest)?);
            let duplicate = line.replacen('{', r#"{"run_id":"prx_duplicate", "#, 1);
            assert!(parse_worker_manifest_line(&duplicate).is_err());
            Ok(())
        }
    }

    fn base_manifest(workspace: &Path, capability: &str) -> WorkerManifest {
        WorkerManifest {
            parent_capability: Some(capability.to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-worker".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: workspace.join("config"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: workspace.to_path_buf(),
            memory_db_path: workspace.join("memory").join("brain.db"),
            memory_workspace_id: Some(workspace.to_string_lossy().to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(workspace.join("memory").join("brain.db")),
            worker_memory_db_path: Some(workspace.join("worker.db")),
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: vec!["file_read".to_string()],
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: Some("identity/worker".to_string()),
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: None,
            owner_isolated: false,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            spawn_depth: 0,
            session_scope_key: "sessions_spawn:global".to_string(),
            parent_run_id: None,
            compaction_config: None,
        }
    }

    #[test]
    fn worker_keeps_gateway_credentials_out_of_an_overridden_provider() {
        let mut manifest = base_manifest(Path::new("/tmp/worker-credentials"), "capability");
        let mut config = Config::default();
        config.default_provider = Some("qwen-gateway".to_string());
        config.api_key = Some("gateway-key".to_string());
        config.api_url = Some("http://gateway.invalid/v1".to_string());

        manifest.provider_name = "kimi-code".to_string();
        let connection = worker_credentials(&manifest, &config).provider;
        assert_eq!(connection.api_key, None);
        assert_eq!(connection.api_url, None);

        manifest.api_key = Some("kimi-key".to_string());
        let connection = worker_credentials(&manifest, &config).provider;
        assert_eq!(connection.api_key, Some("kimi-key"));
        assert_eq!(connection.api_url, None);

        manifest.provider_name = "qwen-gateway".to_string();
        manifest.api_key = Some("gateway-key".to_string());
        manifest.direct_uses_root_connection = true;
        validate_worker_route(&manifest, &config).expect("root manifest matches config");
        let connection = worker_credentials(&manifest, &config).provider;
        assert_eq!(connection.api_key, Some("gateway-key"));
        assert_eq!(connection.api_url, Some("http://gateway.invalid/v1"));
    }

    #[test]
    fn direct_agent_key_equal_to_root_bytes_has_no_root_connection() {
        let mut manifest = base_manifest(Path::new("/tmp/worker-direct-provenance"), "capability");
        let mut config = Config::default();
        config.default_provider = Some("openai".to_string());
        config.api_key = Some("PRX-SAME-KEY".to_string());
        config.api_url = Some("http://root.invalid/v1".to_string());
        config.reliability.api_keys = vec!["PRX-ROOT-ROTATION".to_string()];
        config.agents.insert(
            "prx_agent".to_string(),
            toml::from_str("provider = 'openai'\nmodel = 'gpt-4o'\napi_key = 'PRX-SAME-KEY'\n").expect("agent fixture"),
        );
        manifest.provider_name = "openai".to_string();
        manifest.model = "gpt-4o".to_string();
        manifest.api_key = Some("PRX-SAME-KEY".to_string());
        manifest.agent_id = Some("prx_agent".to_string());
        manifest.direct_uses_root_connection = false;
        validate_worker_route(&manifest, &config).expect("agent-owned key remains non-root");
        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some("PRX-SAME-KEY"));
        assert_eq!(credentials.provider.api_url, None);
        assert!(credentials.reliability.api_keys.is_empty(), "expected an empty value");

        manifest.direct_uses_root_connection = true;
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "agent cannot claim root connection"
        );
    }

    #[test]
    fn selected_route_key_equal_to_root_key_still_gets_no_root_url_or_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = root_config(temp.path());
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: config.api_key.clone(),
            credential_ref: None,
        });
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = "openai".to_string();
        manifest.model = "gpt-4o-mini".to_string();
        manifest.api_key = config.api_key.clone();
        manifest.route_hint = Some("fast".to_string());
        manifest.route_source_provider = Some("openai".to_string());
        manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
        ));
        validate_worker_route(&manifest, &config).expect("sealed route matches");
        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
        assert_eq!(credentials.provider.api_url, None);
        assert!(credentials.reliability.api_keys.is_empty(), "expected an empty value");

        manifest.route_uses_root_connection = true;
        assert!(validate_worker_route(&manifest, &config).is_err());
        manifest.route_uses_root_connection = false;
        manifest.model = "different-model".to_string();
        assert!(validate_worker_route(&manifest, &config).is_err());
    }

    #[test]
    fn sealed_foreign_route_refuses_root_key_and_config_drift() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = root_config(temp.path());
        let destination = "custom:http://127.0.0.1:18081/v1";
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: destination.to_string(),
            model: "probe-model".to_string(),
            api_key: Some("PRX-WORKER-ROUTE-KEY".to_string()),
            credential_ref: None,
        });
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = destination.to_string();
        manifest.model = "probe-model".to_string();
        manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
        manifest.route_hint = Some("fast".to_string());
        manifest.route_source_provider = Some("openai".to_string());
        manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
        ));
        validate_worker_route(&manifest, &config).expect("selected route matches sealed config");
        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some("PRX-WORKER-ROUTE-KEY"));
        assert_eq!(credentials.provider.api_url, None);
        assert_eq!(credentials.spawn_fallback, config.api_key.as_deref());

        manifest.api_key = config.api_key.clone();
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "root key cannot become route key"
        );
        manifest.api_key = Some("PRX-WORKER-ROUTE-KEY".to_string());
        manifest.route_uses_root_connection = true;
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "foreign route cannot use root URL"
        );
        manifest.route_uses_root_connection = false;
        config.model_routes.get_mut(0).expect("route fixture").model = "changed-model".to_string();
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "route target drift is refused"
        );
        config.model_routes.get_mut(0).expect("route fixture").model = "probe-model".to_string();
        manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::Agent("missing-agent".to_string()),
        ));
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "tampered credential source is refused"
        );
    }

    #[test]
    fn agent_profile_route_keeps_credential_source_without_execution_identity() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = root_config(temp.path());
        let destination = "custom:http://127.0.0.1:18082/v1";
        config.secrets.encrypt = false;
        let profile = crate::auth::AuthService::new(temp.path(), false)
            .store_provider_token(
                destination,
                "agent-route",
                "PRX-WORKER-AGENT-PROFILE",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: agent profile");
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "agent".to_string(),
            provider: destination.to_string(),
            model: "probe-model".to_string(),
            api_key: None,
            credential_ref: None,
        });
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = destination.to_string();
        manifest.model = "probe-model".to_string();
        manifest.api_key = Some("PRX-WORKER-AGENT-PROFILE".to_string());
        manifest.route_hint = Some("agent".to_string());
        manifest.route_source_provider = Some(destination.to_string());
        manifest.route_credential = Some(crate::providers::credentials::CredentialRef::Profile { id: profile.id });
        manifest.agent_id = None;
        validate_worker_route(&manifest, &config).expect("sealed profile works independently of execution identity");
        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some("PRX-WORKER-AGENT-PROFILE"));
        assert_eq!(credentials.provider.api_url, None);
        assert_eq!(credentials.spawn_fallback, config.api_key.as_deref());

        manifest.api_key = config.api_key.clone();
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "root key is not agent profile"
        );
        manifest.api_key = Some("PRX-WORKER-AGENT-PROFILE".to_string());
        manifest.route_source_provider = Some("openai".to_string());
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "source substitution is refused"
        );
        manifest.route_source_provider = Some(destination.to_string());
        manifest.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::Root,
        ));
        assert!(
            validate_worker_route(&manifest, &config).is_err(),
            "reference substitution is refused"
        );
    }

    const WORKER_ROOT_SENTINEL: &str = "C0-WORKER-ROOT-SENTINEL";
    const WORKER_ROTATION_SENTINEL: &str = "C0-WORKER-ROTATION-SENTINEL";
    const WORKER_AGENT_SENTINEL: &str = "C0-WORKER-AGENT-SENTINEL";

    fn root_config(temp: &Path) -> Config {
        let mut config = Config::default();
        config.config_path = temp.join("config.toml");
        config.workspace_dir = temp.to_path_buf();
        config.default_provider = Some("openai".to_string());
        config.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
        config.api_url = Some("https://api.openai.example/v1".to_string());
        config.reliability.api_keys = vec![WORKER_ROTATION_SENTINEL.to_string()];
        config.reliability.fallback_providers = Vec::new();
        config.reliability.provider_retries = 0;
        config.reliability.provider_backoff_ms = 0;
        config.auth.codex_auth_json_auto_import = false;
        config.auth.codex_auth_json_path = temp.join("no-codex-auth.json");
        config
    }

    fn non_default_manifest(temp: &Path, api_key: Option<&str>) -> WorkerManifest {
        let mut manifest = base_manifest(temp, "capability");
        manifest.provider_name = "custom:https://model-chosen.example/v1".to_string();
        manifest.api_key = api_key.map(str::to_string);
        manifest
    }

    /// C0-C / R-C0 F-6, consumer 1: the worker's own chain on a provider the
    /// parent took from a model's `provider` argument gets neither the root
    /// connection nor the root rotation keys.
    #[test]
    fn a_non_default_worker_chain_gets_no_root_connection_or_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let config = root_config(temp.path());
        let manifest = non_default_manifest(temp.path(), None);

        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, None);
        assert_eq!(credentials.provider.api_url, None);
        assert!(
            credentials.reliability.api_keys.is_empty(),
            "rotation keys belong to the default provider"
        );
    }

    /// Consumer 2, the process-mode two-hop: the worker's `sessions_spawn`
    /// pairs its fallback with the worker's own provider, so the fallback is
    /// that provider's own credential and never the root key.
    #[test]
    fn a_non_default_worker_spawn_fallback_is_never_the_root_key() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let config = root_config(temp.path());

        let keyless = non_default_manifest(temp.path(), None);
        assert_eq!(
            worker_credentials(&keyless, &config).spawn_fallback,
            None,
            "the root key must not become the fallback for the worker's own provider"
        );

        let keyed = non_default_manifest(temp.path(), Some(WORKER_AGENT_SENTINEL));
        assert_eq!(
            worker_credentials(&keyed, &config).spawn_fallback,
            Some(WORKER_AGENT_SENTINEL)
        );
    }

    /// Consumer 3, the root slot: the base tools' `delegate` pairs this slot
    /// with the default provider, so it is the root key and never an agent key.
    #[test]
    fn a_worker_root_slot_is_never_an_agent_key() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let config = root_config(temp.path());
        let manifest = non_default_manifest(temp.path(), Some(WORKER_AGENT_SENTINEL));

        assert_eq!(
            worker_credentials(&manifest, &config).root_api_key,
            Some(WORKER_ROOT_SENTINEL),
            "an agent key must not stand in for the root key"
        );
    }

    async fn admitted_worker_fallback_spawn() {
        use crate::providers::credential_probe::CredentialProbe;
        let temp = tempfile::tempdir().expect("test: temp dir");
        let own = CredentialProbe::start().await;
        let fallback = CredentialProbe::start().await;
        let mut config = root_config(temp.path());
        config.reliability.fallback_providers = vec![fallback.custom_provider()];
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = own.custom_provider();
        manifest.api_key = None;
        manifest.workspace_dir = temp.path().to_path_buf();
        config.causal_tree.enabled = false;
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": manifest.provider_name,
                "model": manifest.model,
                "operations": ["chat"],
                "native_tools": true
            }))
            .expect("test: worker model metadata"),
        );
        config.cost.prices.insert(
            format!("{}/{}", manifest.provider_name, manifest.model),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        config.cost.prices.insert(
            format!("{}/{}", fallback.custom_provider(), manifest.model),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let options = crate::providers::provider_runtime_options_from_config(&config);
        let credentials = worker_credentials(&manifest, &config);
        let provider: Arc<dyn Provider> = Arc::from(
            worker_provider(&manifest, &config, &credentials, &options).expect("test: keyless endpoint builds"),
        );
        let security = Arc::new(SecurityPolicy {
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("test: worker spawn memory"));
        let source_fabric = MemoryFabric::new(Arc::clone(&memory), temp.path().to_string_lossy().into_owned())
            .with_event_recording(config.memory.event_recording_config());
        let active_runs = Arc::new(tokio::sync::RwLock::new(Vec::new()));
        let tool = worker_spawn_tool(
            &manifest,
            &config,
            &credentials,
            Arc::clone(&provider),
            security,
            &options,
            Arc::clone(&active_runs),
        )
        .with_shared_memory(Arc::clone(&memory))
        .with_source_fabric(source_fabric);
        let tools_handle = tool.tools_handle();
        let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(tool)]);
        assert!(tools_handle.set(&registry), "test: worker tool registry is set once");
        let mut admission_config = config.clone();
        admission_config.default_provider = Some(manifest.provider_name.clone());
        admission_config.default_model = Some(manifest.model.clone());
        let grant = crate::security::policy::ApprovalGrant::for_resource_operation(
            "sessions_spawn",
            "sessions_spawn:spawn",
            "test",
            None,
        );
        let message = ChannelMessage {
            id: "prx-worker-spawn-parent".to_owned(),
            sender: "prx_user".to_owned(),
            reply_target: "prx_worker".to_owned(),
            content: "sessions_spawn admitted worker fixture".to_owned(),
            channel: "session-worker".to_owned(),
            timestamp: 0,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            thread_ts: None,
            mentioned_uuids: Vec::new(),
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
            .expect("test: worker spawn cleanup owner");
        let result = cleanup
            .scope_admission(async {
                let admitted = crate::agent::test_admission::AdmittedTurn::ordinary_controlled_channel_with_memory(
                    admission_config,
                    message,
                    registry,
                    memory,
                    (&manifest.provider_name, &manifest.model),
                )
                .await;
                crate::runtime::child_source::scope_ordinary_provider_fixture(
                    provider,
                    manifest.provider_name.clone(),
                    manifest.model.clone(),
                    admitted.execute_tool(
                        "sessions_spawn",
                        serde_json::json!({
                            "task": "probe",
                            "provider": fallback.custom_provider(),
                            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: serde_json::to_value(grant)
                                .expect("test: grant serializes"),
                        }),
                    ),
                )
                .await
            })
            .await
            .expect("test: admitted spawn returns a tool result");

        assert!(
            result.success,
            "an operator-configured provider is spawnable: {:?}",
            result.error
        );
        let request_count = fallback.wait_for_requests(1, std::time::Duration::from_secs(10)).await;
        assert_eq!(request_count, 1, "spawned worker state: {:?}", active_runs.read().await);
        assert!(!fallback.saw(WORKER_ROOT_SENTINEL) && !fallback.saw(WORKER_ROTATION_SENTINEL));
        cleanup
            .run(async { Ok(()) })
            .await
            .expect("test: worker spawn cleanup is confirmed");
    }

    /// R-C0 F-5: a worker's `sessions_spawn` accepts every provider the
    /// operator configured, here a keyless fallback provider, like every other
    /// surface; it previously refused anything but its own and its agents'.
    #[tokio::test]
    async fn a_worker_spawn_accepts_an_operator_configured_fallback_provider() {
        boxed_worker_segments(admitted_worker_fallback_spawn).await;
    }

    /// PMM-C: a worker whose manifest names a model that cannot chat builds
    /// no provider, and nothing is sent.
    #[tokio::test]
    async fn a_worker_for_a_decision_only_model_builds_no_provider() {
        use crate::providers::credential_probe::CredentialProbe;
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let mut config = root_config(temp.path());
        config.providers.models.push(
            toml::from_str(&format!(
                "provider = \"{}\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
                probe.custom_provider()
            ))
            .expect("test: declaration"),
        );
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = probe.custom_provider();
        manifest.model = "jev-decision-test".to_string();
        manifest.api_key = None;
        let options = crate::providers::provider_runtime_options_from_config(&config);
        let credentials = worker_credentials(&manifest, &config);
        let Err(error) = worker_provider(&manifest, &config, &credentials, &options) else {
            panic!("a worker for a decision-only model built its provider");
        };
        assert!(format!("{error:#}").contains("cannot be used for chat"), "{error:#}");
        assert!(probe.requests().is_empty());
    }

    /// Finding 64: exercise the worker's own credential-choice and provider
    /// construction seam, rather than calling its lower-level typed builder.
    #[tokio::test]
    async fn worker_provider_refuses_a_cross_dialect_fallback_before_transport() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = crate::providers::credential_probe::CredentialProbe::start().await;
        let mut config = root_config(temp.path());
        crate::providers::entrypoint_consistency_tests::configure_cross_dialect(&mut config, &probe.custom_provider());
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = probe.custom_provider();
        manifest.model = crate::providers::entrypoint_consistency_tests::PRIMARY_MODEL.to_string();
        let options = crate::providers::provider_runtime_options_from_config(&config);
        let credentials = worker_credentials(&manifest, &config);

        let error = worker_provider(&manifest, &config, &credentials, &options)
            .err()
            .expect("a worker accepted an incompatible fallback");
        let shown = format!("{error:#}");
        crate::providers::entrypoint_consistency_tests::assert_cross_dialect_refusal("worker", &shown);
        assert!(probe.requests().is_empty(), "the worker reached transport: {shown}");
    }

    /// The worker's own chain on a non-default provider carries neither the
    /// root key nor a rotation key onto the wire.
    #[tokio::test]
    async fn a_worker_provider_on_a_non_default_endpoint_carries_no_root_credential() {
        use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
        let temp = tempfile::tempdir().expect("test: temp dir");
        let probe = CredentialProbe::start_with_statuses(vec![429]).await;
        let config = root_config(temp.path());
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = probe.custom_provider();
        manifest.model = PROBE_MODEL.to_string();
        manifest.api_key = None;
        let options = crate::providers::provider_runtime_options_from_config(&config);

        let credentials = worker_credentials(&manifest, &config);
        let provider =
            worker_provider(&manifest, &config, &credentials, &options).expect("test: keyless endpoint builds");
        let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

        assert!(!probe.requests().is_empty());
        assert!(!probe.saw(WORKER_ROOT_SENTINEL));
        assert!(
            !probe.saw(WORKER_ROTATION_SENTINEL),
            "a rotation key reached a worker's non-default provider"
        );
    }

    /// C0-f F-1: the root URL and rotation keys belong to the root key. A
    /// manifest key that is a named agent's own credential for the default
    /// provider identity is presented without them.
    #[test]
    fn an_agent_key_on_the_default_provider_does_not_borrow_the_root_url() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let config = root_config(temp.path());
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = "OPENAI".to_string();
        manifest.api_key = Some(WORKER_AGENT_SENTINEL.to_string());

        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some(WORKER_AGENT_SENTINEL));
        assert_eq!(
            credentials.provider.api_url, None,
            "the root URL belongs to the root key"
        );
        assert!(
            credentials.reliability.api_keys.is_empty(),
            "rotation keys belong to the root key"
        );

        manifest.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
        manifest.direct_uses_root_connection = true;
        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
        assert_eq!(credentials.provider.api_url, Some("https://api.openai.example/v1"));
    }

    /// Guard: a worker on the configured default keeps the root connection and
    /// the rotation keys, in any spelling of that provider.
    #[test]
    fn a_worker_on_the_default_provider_keeps_the_root_connection() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let config = root_config(temp.path());
        let mut manifest = base_manifest(temp.path(), "capability");
        manifest.provider_name = "OpenAI".to_string();
        manifest.api_key = Some(WORKER_ROOT_SENTINEL.to_string());
        manifest.direct_uses_root_connection = true;
        validate_worker_route(&manifest, &config).expect("root manifest matches config");

        let credentials = worker_credentials(&manifest, &config);
        assert_eq!(credentials.provider.api_key, Some(WORKER_ROOT_SENTINEL));
        assert_eq!(credentials.provider.api_url, Some("https://api.openai.example/v1"));
        assert_eq!(
            credentials.reliability.api_keys,
            vec![WORKER_ROTATION_SENTINEL.to_string()]
        );
        assert_eq!(credentials.spawn_fallback, Some(WORKER_ROOT_SENTINEL));
    }

    #[test]
    fn session_worker_secret_follows_the_resolved_config_dir_instead_of_home() {
        let _lock = crate::config::schema::process_config_dir_test_lock();
        let scoped = std::path::PathBuf::from("/tmp/openprx-secret-scope");
        let previous = crate::config::schema::swap_process_config_dir_for_tests(Some(scoped.clone()));
        let scoped_path = session_secret_path();
        crate::config::schema::swap_process_config_dir_for_tests(None);
        let unscoped_path = session_secret_path();
        crate::config::schema::swap_process_config_dir_for_tests(previous);

        assert_eq!(
            scoped_path,
            Some(scoped.join("keys").join("session_worker.secret")),
            "a scoped run must keep its session-worker secret inside the resolved config dir"
        );
        assert_eq!(
            unscoped_path,
            crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret")),
            "without a published config dir the default ~/.openprx location is kept"
        );
    }

    #[tokio::test]
    async fn worker_wildcard_preserves_its_source_registry() {
        let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
        let expected = source.len();
        let selected = select_tools_for_worker(source, &["*".to_string()], &[])
            .await
            .expect("wildcard selection");
        assert_eq!(selected.len(), expected);
    }

    /// One plain tool under a fixed public name, to build a registry whose
    /// membership is exactly what a test wants to reason about.
    struct NamedWorkerTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedWorkerTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "worker surface fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: self.0.to_string(),
                error: None,
            })
        }
    }

    fn worker_surface_fixture() -> Vec<Box<dyn Tool>> {
        [
            "gateway",
            "proxy_config",
            "config_reload",
            "nodes",
            "git_operations",
            "shell",
        ]
        .into_iter()
        .map(|name| Box::new(NamedWorkerTool(name)) as Box<dyn Tool>)
        .collect()
    }

    async fn worker_tool_names(allowed: &[String], excluded: &[String]) -> Vec<String> {
        select_tools_for_worker(worker_surface_fixture(), allowed, excluded)
            .await
            .expect("worker tool selection")
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_process_worker_cannot_regain_the_operations_surface_its_channel_hides() {
        // Fixture validity first: the exclusions come from the real default
        // channel surface, not from a list written for this test, so the test
        // fails if `channel_exclude` ever stops hiding the operations tools.
        let surface = crate::config::ToolTieringConfig::default().for_channel_surface();
        let hidden = surface.always_exclude.clone();
        for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
            assert!(
                hidden.iter().any(|entry| entry == name),
                "the default channel surface must hide '{name}' for this fixture to mean anything"
            );
        }

        let filtered = worker_tool_names(&[], &hidden).await;
        for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
            assert!(
                !filtered.iter().any(|entry| entry == name),
                "a worker started from an IM turn must not publish '{name}': {filtered:?}"
            );
        }
        assert!(
            filtered.iter().any(|entry| entry == "shell"),
            "the rest of the surface is untouched: {filtered:?}"
        );

        // The paired half: without an inherited list the same worker publishes
        // the whole registry, so the assertion above is about the list and not
        // about the fixture being empty.
        let unfiltered = worker_tool_names(&[], &[]).await;
        assert!(
            unfiltered.iter().any(|entry| entry == "gateway"),
            "an operator-rooted worker keeps the operations surface: {unfiltered:?}"
        );

        // An explicit `allowed_tools` is the agent's own contract and cannot
        // re-open the channel boundary.
        let named = worker_tool_names(&["gateway".to_string()], &hidden).await;
        assert!(
            named.is_empty(),
            "naming a hidden tool in allowed_tools must not publish it: {named:?}"
        );

        // An operator who re-enables one keeps it: `for_channel_surface` never
        // folds an `always_include` name into the hidden set.
        let mut reopened = crate::config::ToolTieringConfig::default();
        reopened.always_include.push("gateway".to_string());
        let reopened = reopened.for_channel_surface().always_exclude;
        let names = worker_tool_names(&[], &reopened).await;
        assert!(
            names.iter().any(|entry| entry == "gateway"),
            "always_include must still reach a process worker: {names:?}"
        );
    }

    async fn manifest_tool_names(manifest: &WorkerManifest) -> Vec<String> {
        select_tools_for_manifest(worker_surface_fixture(), manifest)
            .await
            .expect("worker tool selection")
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    /// The manifest's channel exclusions must reach the worker's tool filter.
    ///
    /// `a_process_worker_cannot_regain_the_operations_surface_its_channel_hides`
    /// hands `select_tools_for_worker` a list the test computed itself, so it
    /// pins the filter and not the production wiring: with
    /// `run_validated_manifest` passing `&[]` in place of
    /// `&manifest.channel_excluded_tools` that test stayed green (independent
    /// audit 2026-09-16 §1.4). This one goes through
    /// `select_tools_for_manifest`, the single binding `run_validated_manifest`
    /// uses, so the wiring itself is what is asserted.
    ///
    /// MUTATION GUARD: replace `&manifest.channel_excluded_tools` in
    /// `select_tools_for_manifest` with `&[]` and the first assertion fails.
    #[tokio::test]
    async fn a_worker_manifest_binds_its_channel_exclusions_to_the_tool_filter() {
        // Fixture validity first: the hidden set is the real default channel
        // surface, so the test dies if `channel_exclude` stops hiding these.
        let hidden = crate::config::ToolTieringConfig::default()
            .for_channel_surface()
            .always_exclude;
        for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
            assert!(
                hidden.iter().any(|entry| entry == name),
                "the default channel surface must hide '{name}' for this fixture to mean anything"
            );
        }

        let mut manifest = base_manifest(Path::new("/tmp/worker-manifest-exclusions"), "capability");
        manifest.allowed_tools = Vec::new();
        manifest.channel_excluded_tools = hidden;

        let filtered = manifest_tool_names(&manifest).await;
        for name in ["gateway", "proxy_config", "config_reload", "nodes", "git_operations"] {
            assert!(
                !filtered.iter().any(|entry| entry == name),
                "a worker resumed from this manifest must not publish '{name}': {filtered:?}"
            );
        }
        assert!(
            filtered.iter().any(|entry| entry == "shell"),
            "the rest of the surface is untouched: {filtered:?}"
        );

        // The paired half: the same manifest with an empty exclusion list
        // publishes the operations surface, so the assertion above is about the
        // manifest field and not about the fixture being empty.
        manifest.channel_excluded_tools = Vec::new();
        let unfiltered = manifest_tool_names(&manifest).await;
        assert!(
            unfiltered.iter().any(|entry| entry == "gateway"),
            "an operator-rooted manifest keeps the operations surface: {unfiltered:?}"
        );

        // The manifest's own `allowed_tools` reaches the same binding.
        manifest.allowed_tools = vec!["shell".to_string()];
        let allowed = manifest_tool_names(&manifest).await;
        assert_eq!(
            allowed,
            vec!["shell".to_string()],
            "allowed_tools must come from the manifest too: {allowed:?}"
        );
    }

    #[tokio::test]
    async fn a_worker_hands_its_inherited_exclusions_to_everything_it_spawns() {
        let workspace = tempfile::TempDir::new().expect("tempdir");
        let mut manifest = base_manifest(workspace.path(), "capability");
        manifest.session_scope_key = "sessions_spawn:parent".to_string();
        manifest.channel_excluded_tools = vec!["gateway".to_string()];
        let inherited = with_manifest_spawn_context(&manifest, None, None, None, None, async {
            crate::tools::sessions_spawn::ambient_channel_excluded_tools()
        })
        .await;
        assert_eq!(
            inherited,
            vec!["gateway".to_string()],
            "a grandchild spawned inside the worker must see the same boundary"
        );

        manifest.channel_excluded_tools.clear();
        let operator = with_manifest_spawn_context(&manifest, None, None, None, None, async {
            crate::tools::sessions_spawn::ambient_channel_excluded_tools()
        })
        .await;
        assert!(operator.is_empty(), "an operator-rooted worker seeds no exclusions");
    }

    struct WorkerDynamicAliases;

    #[async_trait::async_trait]
    impl Tool for WorkerDynamicAliases {
        fn name(&self) -> &str {
            "dynamic_router"
        }

        fn description(&self) -> &str {
            "Dynamic worker alias fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn specs(&self) -> Vec<crate::tools::ToolSpec> {
            ["dynamic__one", "dynamic__two"]
                .into_iter()
                .map(|name| crate::tools::ToolSpec {
                    name: name.to_string(),
                    description: name.to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }),
                })
                .collect()
        }

        fn supports_name(&self, name: &str) -> bool {
            matches!(name, "dynamic_router" | "dynamic__one" | "dynamic__two")
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: false,
                output: String::new(),
                error: Some("root execution is outside this fixture's contract".to_string()),
            })
        }

        async fn execute_named(
            &self,
            name: &str,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: name.to_string(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn worker_allowlist_can_select_multiple_aliases_from_one_dynamic_router() {
        let selected = select_tools_for_worker(
            vec![Box::new(WorkerDynamicAliases)],
            &["dynamic__one".to_string(), "dynamic__two".to_string()],
            &[],
        )
        .await
        .expect("dynamic aliases must resolve independently");

        assert_eq!(selected.len(), 2);
        for name in ["dynamic__one", "dynamic__two"] {
            let tool = selected
                .iter()
                .find(|tool| tool.name() == name)
                .expect("selected alias proxy");
            let result = tool
                .execute_named(name, serde_json::json!({"value": "ok"}))
                .await
                .expect("alias execution");
            assert_eq!(result.output, name);
        }
    }

    #[test]
    fn worker_orchestration_contract_detects_missing_entrypoint_tools() {
        verify_worker_orchestration_tool_names(WORKER_ORCHESTRATION_TOOL_NAMES.iter().copied())
            .expect("complete orchestration contract");

        let error = verify_worker_orchestration_tool_names(["sessions_spawn", "gateway"])
            .expect_err("incomplete orchestration contract must fail");
        assert!(error.to_string().contains("sessions_list"));
        assert!(error.to_string().contains("message_send"));
    }

    #[tokio::test]
    async fn worker_wildcard_must_be_exclusive() {
        let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
        let error = match select_tools_for_worker(source, &["*".to_string(), "shell".to_string()], &[]).await {
            Ok(_) => panic!("mixed wildcard must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exclusively"));
    }

    #[tokio::test]
    async fn worker_explicit_allowlist_inherits_transcript_recovery_dependency() {
        let mut source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
        source.push(Box::new(crate::tools::TranscriptHistoryLookupTool::new(Arc::new(
            crate::memory::NoneMemory::new(),
        ))));

        let selected = select_tools_for_worker(source, &["shell".to_string()], &[])
            .await
            .expect("restricted selection");
        let names = selected.iter().map(|tool| tool.name()).collect::<Vec<_>>();
        assert_eq!(names, vec!["shell", crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME]);
    }

    #[test]
    fn worker_system_prompt_keeps_agent_instructions_without_a_static_tool_catalog() {
        let workspace = tempfile::TempDir::new().expect("workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let mut manifest = base_manifest(workspace.path(), "capability");
        manifest.model = "k3".to_string();
        manifest.system_prompt = Some("Use file tools and verify every artifact.".to_string());

        let prompt = build_worker_system_prompt(&manifest, &config, true);

        assert!(!prompt.contains("## Tools"));
        assert!(prompt.contains("Working directory: `"));
        assert!(prompt.contains("Use tools when the request requires action"));
        assert!(prompt.contains("## Delegated Agent Instructions"));
        assert!(prompt.contains("Use file tools and verify every artifact."));
        assert!(prompt.contains("Never claim that a file, command, or check succeeded"));
    }

    /// A fake agent-loop segment: parks until cancelled for its first
    /// `natural_completion_on - 1` entries, then completes on its own. This
    /// exercises the steering race — cancel, unwind, re-enter, finish — without
    /// a provider, config, or memory backend.
    fn segment_cancellable_until(
        entries: Arc<std::sync::atomic::AtomicUsize>,
        natural_completion_on: usize,
    ) -> impl FnMut(
        Vec<ChatMessage>,
        CancellationToken,
        Option<String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = SteeredSegment> + Send>> {
        move |history, cancel, _raw_steer| {
            let entries = entries.clone();
            Box::pin(async move {
                let entry = entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if entry < natural_completion_on {
                    cancel.cancelled().await;
                }
                let output = format!("finished on entry {entry}");
                (
                    history,
                    Ok((output, crate::agent::turn_contracts::ToolLoopTrace::default())),
                )
            })
        }
    }

    #[tokio::test]
    async fn steering_injects_operator_turn_and_restarts_the_segment() {
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(4);
        let mut steer_rx = Some(steer_rx);
        let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Two steers cancel and re-enter the loop; the third entry finishes.
        let mut segment = segment_cancellable_until(entries.clone(), 3);

        let driver = tokio::spawn(async move {
            steer_tx.send("pivot to X".to_string()).await.expect("first steer");
            steer_tx.send("now do Y".to_string()).await.expect("second steer");
            drop(steer_tx);
        });

        let (history, result) = run_segments_with_steering(
            vec![ChatMessage::system("sys"), ChatMessage::user("task")],
            &mut steer_rx,
            &mut segment,
        )
        .await;
        driver.await.expect("driver");

        assert!(result.is_ok(), "steered run must still complete");
        let injected = history
            .iter()
            .filter(|message| message.content.contains("[Steering instruction from operator]"))
            .map(|message| message.content.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            injected,
            vec![
                "[Steering instruction from operator] pivot to X".to_string(),
                "[Steering instruction from operator] now do Y".to_string(),
            ],
            "both steer messages must land in the worker's own history"
        );
        assert_eq!(
            entries.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "each steer must cancel and re-enter the loop"
        );
    }

    #[tokio::test]
    async fn absent_steer_channel_runs_exactly_one_segment() {
        let mut steer_rx: Option<tokio::sync::mpsc::Receiver<String>> = None;
        let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let history_in = vec![ChatMessage::system("sys"), ChatMessage::user("task")];

        // Without a steer channel the segment must never be cancelled by the
        // steering race, so drive completion from outside.
        let (finished, result) =
            run_segments_with_steering(history_in, &mut steer_rx, |history, cancel, _raw_steer| {
                let entries = entries.clone();
                async move {
                    entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    cancel.cancel();
                    cancel.cancelled().await;
                    (
                        history,
                        Ok((
                            "done".to_string(),
                            crate::agent::turn_contracts::ToolLoopTrace::default(),
                        )),
                    )
                }
            })
            .await;

        assert_eq!(finished.len(), 2, "no operator turn may be injected");
        assert_eq!(result.expect("result").0, "done");
        assert_eq!(entries.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closed_steer_channel_waits_for_natural_completion() {
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(1);
        drop(steer_tx);
        let mut steer_rx = Some(steer_rx);
        let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let (history, result) = run_segments_with_steering(
            vec![ChatMessage::user("task")],
            &mut steer_rx,
            |history, cancel, _raw_steer| {
                let entries = entries.clone();
                async move {
                    entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // A closed channel resolves immediately; the segment must
                    // still be awaited to its own end rather than abandoned.
                    tokio::task::yield_now().await;
                    cancel.cancel();
                    (
                        history,
                        Ok((
                            "natural".to_string(),
                            crate::agent::turn_contracts::ToolLoopTrace::default(),
                        )),
                    )
                }
            },
        )
        .await;

        assert_eq!(history.len(), 1, "a closed channel must not inject anything");
        assert_eq!(result.expect("result").0, "natural");
        assert_eq!(entries.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn stdin_control_reader_only_accepts_steer_frames() {
        // The stdin stream carries no capability material: the sealed manifest is
        // the sole authenticated frame, so an unknown `kind` must be rejected
        // outright rather than interpreted.
        assert!(serde_json::from_str::<WorkerControlFrame>(r#"{"kind":"steer","message":"x"}"#).is_ok());
        assert!(serde_json::from_str::<WorkerControlFrame>(r#"{"kind":"grant","capability":"x"}"#).is_err());
        assert!(serde_json::from_str::<WorkerControlFrame>("not json").is_err());
    }

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

    #[test]
    fn worker_manifest_rejects_hybrid_memory_without_merge_consumer() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let mut manifest = base_manifest(tmp.path(), "capability-a");

        assert_eq!(normalized_worker_memory_strategy(&manifest).unwrap(), "shared_fabric");
        manifest.memory_strategy = Some("isolated_private".to_string());
        assert_eq!(
            normalized_worker_memory_strategy(&manifest).unwrap(),
            "isolated_private"
        );
        manifest.memory_strategy = Some("hybrid".to_string());
        manifest.memory_db_path = manifest.worker_memory_db_path.clone().unwrap();

        let serialized = serde_json::to_string(&manifest).unwrap();
        let parsed: WorkerManifest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.memory_strategy.as_deref(), Some("hybrid"));

        let expiry = cap_now() + 300;
        let (manifest, cap, expiry) = seal(parsed, expiry);
        set_expiry_env(expiry);
        let error = validate_worker_manifest_with_capability_env(&manifest, Some(&cap))
            .unwrap_err()
            .to_string();
        clear_expiry_env();
        assert_eq!(error, crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE);
    }

    #[test]
    fn worker_manifest_rejects_parent_backend_drift() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut manifest = base_manifest(tmp.path(), "capability-a");
        manifest.memory_backend = "postgres".to_string();

        let error = validate_parent_memory_backend(&manifest, "sqlite").unwrap_err();
        assert!(error.to_string().contains("manifest=postgres, config=sqlite"));
        validate_parent_memory_backend(&manifest, "postgres").unwrap();
    }

    #[test]
    fn shared_worker_uses_configured_parent_memory_factory() {
        let source = include_str!("runner.rs");
        let shared_branch = source
            .split_once(
                "let memory: Arc<dyn Memory> = if normalized_worker_memory_strategy(&manifest)? == \"shared_fabric\"",
            )
            .unwrap()
            .1
            .split_once("} else {")
            .unwrap()
            .0;
        assert!(shared_branch.contains("create_memory_with_storage_and_routes_with_acl"));
        assert!(shared_branch.contains("Some(&config.storage.provider.config)"));
        assert!(!shared_branch.contains("SqliteMemory"));
    }

    #[test]
    fn worker_manifest_validation_requires_parent_capability() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = base_manifest(tmp.path(), "capability-a");

        let error = validate_worker_manifest_with_capability_env(&manifest, None).unwrap_err();
        assert!(error.to_string().contains("parent capability env is missing"));
    }

    /// Serialize tests that mutate the process-global capability expiry env.
    static CAP_ENV_GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn cap_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Seal `manifest` with a valid HMAC for `expiry`, embedding the token into
    /// the manifest and returning `(sealed_manifest, capability, expiry)`.
    fn seal(mut manifest: WorkerManifest, expiry: u64) -> (WorkerManifest, String, u64) {
        manifest.parent_capability = None;
        let payload = manifest_signing_payload(&manifest).expect("payload");
        let cap = compute_worker_capability(&manifest.run_id, expiry, &payload);
        manifest.parent_capability = Some(cap.clone());
        (manifest, cap, expiry)
    }

    fn set_expiry_env(expiry: u64) {
        // SAFETY: tests hold `CAP_ENV_GUARD`, serializing all env mutation.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY", expiry.to_string());
        }
    }

    fn clear_expiry_env() {
        // SAFETY: tests hold `CAP_ENV_GUARD`, serializing all env mutation.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY");
        }
    }

    #[test]
    fn worker_manifest_validation_rejects_path_escape() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let mut manifest = base_manifest(tmp.path(), "");
        manifest.identity_dir = Some("../outside".to_string());
        let expiry = cap_now() + 300;
        let (manifest, cap, expiry) = seal(manifest, expiry);
        set_expiry_env(expiry);

        let error = validate_worker_manifest_with_capability_env(&manifest, Some(&cap)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("identity_dir"));
    }

    #[test]
    fn valid_sealed_capability_accepted() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        set_expiry_env(expiry);
        let result = validate_worker_capability_with_env(&manifest, Some(&cap));
        clear_expiry_env();
        result.expect("valid sealed capability must be accepted");
    }

    #[test]
    fn legacy_uuid_capability_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (mut manifest, _cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        // Replace the sealed token with a legacy UUID in both manifest and env.
        let uuid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        manifest.parent_capability = Some(uuid.clone());
        set_expiry_env(expiry);
        let error = validate_worker_capability_with_env(&manifest, Some(&uuid)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("signature mismatch"));
    }

    #[test]
    fn tampered_manifest_capability_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        // Escalate the allow-list after sealing; the embedded token now mismatches.
        manifest.allowed_tools.push("shell".to_string());
        set_expiry_env(expiry);
        let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("signature mismatch"));
    }

    #[test]
    fn tampered_route_provenance_breaks_worker_capability() {
        let _guard = CAP_ENV_GUARD.lock();
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut original = base_manifest(temp.path(), "");
        original.route_hint = Some("fast".to_string());
        original.route_source_provider = Some("openai".to_string());
        original.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::ModelRoute("fast".to_string()),
        ));
        let expiry = cap_now() + 300;
        let (sealed, capability, expiry) = seal(original, expiry);
        set_expiry_env(expiry);
        let mut source_changed = sealed.clone();
        source_changed.route_source_provider = Some("anthropic".to_string());
        assert!(
            validate_worker_capability_with_env(&source_changed, Some(&capability))
                .expect_err("source tamper refused")
                .to_string()
                .contains("signature mismatch")
        );
        let mut credential_changed = sealed;
        credential_changed.route_credential = Some(crate::providers::credentials::CredentialRef::LegacyInline(
            crate::providers::credentials::LegacySlot::Root,
        ));
        assert!(
            validate_worker_capability_with_env(&credential_changed, Some(&capability))
                .expect_err("credential tamper refused")
                .to_string()
                .contains("signature mismatch")
        );
        clear_expiry_env();
    }

    #[test]
    fn tampered_config_generation_capability_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        manifest.config_generation = "f".repeat(64);
        set_expiry_env(expiry);
        let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("signature mismatch"));
    }

    #[test]
    fn wrong_run_id_capability_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (mut manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        // Replay the token under a different run id.
        manifest.run_id = "different-run".to_string();
        set_expiry_env(expiry);
        let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("signature mismatch"));
    }

    #[test]
    fn expired_capability_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let stale = cap_now().saturating_sub(10);
        let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), stale);
        set_expiry_env(expiry);
        let error = validate_worker_capability_with_env(&manifest, Some(&cap)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn env_manifest_capability_desync_rejected() {
        let _g = CAP_ENV_GUARD.lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let expiry = cap_now() + 300;
        let (manifest, cap, expiry) = seal(base_manifest(tmp.path(), ""), expiry);
        set_expiry_env(expiry);
        // Env token differs from the manifest's sealed token.
        let mut wrong = cap;
        wrong.push('x');
        let error = validate_worker_capability_with_env(&manifest, Some(&wrong)).unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("mismatch"));
    }

    #[test]
    fn worker_cli_overrides_must_match_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = base_manifest(tmp.path(), "capability-a");

        let error = validate_worker_cli_overrides(
            &manifest,
            Some("different task"),
            Some(&manifest.workspace_dir.to_string_lossy()),
            Some(&manifest.memory_db_path.to_string_lossy()),
            Some(&manifest.allowed_tools),
            Some(&manifest.config_dir.to_string_lossy()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("task override"));
    }

    #[test]
    fn worker_cli_config_dir_must_match_sealed_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = base_manifest(tmp.path(), "capability-a");

        let error = validate_worker_cli_overrides(
            &manifest,
            Some(&manifest.task),
            Some(&manifest.workspace_dir.to_string_lossy()),
            Some(&manifest.memory_db_path.to_string_lossy()),
            Some(&manifest.allowed_tools),
            Some("/different/config"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("config-dir"));
    }

    // The `CAP_ENV_GUARD` mutex intentionally stays locked across the awaited
    // validate-then-run sequence so the capability env stays stable
    // for the whole run; this single-threaded test never contends the guard.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn malicious_manifest_rejected_before_config_memory_or_worker_dir_creation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("worker");
        let memory_db = workspace.join("memory").join("brain.db");
        let config_dir_arg = config_dir.to_string_lossy().to_string();

        let _g = CAP_ENV_GUARD.lock();
        let mut manifest = base_manifest(&workspace, "");
        manifest.run_id = "../escape".to_string();
        // Seal with a valid HMAC so validation passes the capability check and
        // reaches the run_id path-safety check we are asserting on.
        let expiry = cap_now() + 300;
        let (manifest, cap, expiry) = seal(manifest, expiry);
        set_expiry_env(expiry);

        // `run_manifest`'s order: validate, and only a valid manifest runs
        let error = async {
            validate_worker_manifest_with_capability_env(&manifest, Some(&cap))?;
            run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
        }
        .await
        .unwrap_err();
        clear_expiry_env();
        assert!(error.to_string().contains("run_id"));
        assert!(!config_dir.exists(), "invalid manifest must not initialize config dir");
        assert!(!workspace.exists(), "invalid manifest must not create worker workspace");
        assert!(
            !memory_db.exists(),
            "invalid manifest must not initialize worker memory DB"
        );
    }

    #[tokio::test]
    async fn worker_missing_config_source_fails_without_initializing_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("missing-config");
        let workspace = tmp.path().join("worker");
        let config_dir_arg = config_dir.to_string_lossy().to_string();
        let mut manifest = base_manifest(&workspace, "unused-for-direct-validation");
        manifest.config_dir = config_dir.clone();

        let result = run_validated_manifest(manifest, None, Some(&config_dir_arg)).await;

        assert!(result.is_err());
        assert!(
            !config_dir.exists(),
            "session-worker must not initialize a missing parent config source"
        );
    }

    #[tokio::test]
    async fn worker_rejects_changed_config_generation_before_workspace_side_effects() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tmp = retain_worker_probe_directory(tmp);
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("worker");
        let error = with_worker_probe_manifest(
            &tmp,
            "custom:http://127.0.0.1:1".to_owned(),
            true,
            |manifest| async move {
                // The real parent has already issued this manifest against the
                // actual config bytes. Change only those private bytes afterwards.
                let config_path = config_dir.join("config.toml");
                let mut config: crate::config::Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
                config.default_temperature = 0.6;
                std::fs::write(config_path, toml::to_string(&config)?)?;
                let config_dir_arg = config_dir.to_string_lossy().to_string();
                run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("generation mismatch"));
        assert!(
            !workspace.exists(),
            "generation mismatch must fail before worker side effects"
        );
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
                // OWNER-VIS OV-D: the feed carries the worker's own owner's
                // events only, so the parent event belongs to that owner.
                owner_id: Some("owner-a".to_string()),
                source: "gateway".into(),
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
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: Some(0),
                config_source_revision: None,
                content: "parent shared context".to_string(),
                raw_payload_json: None,
                visibility: crate::memory::MemoryVisibility::Workspace,
            })
            .await
            .unwrap();
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-hybrid".to_string(),
            task: "use context".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: parent.path().join("config"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker.path().join("brain.db"),
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(shared_db),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };
        let context = load_worker_shared_context(&manifest, &Config::default(), &shared_memory).await;

        assert!(context.contains("parent shared context"));
    }

    /// OWNER-VIS OV-D (D-019 Q-4): a process-mode sub-agent worker's prompt
    /// used to carry "[Recent shared workspace events]" from the whole
    /// workspace — another owner's spawn task text and sub-agent result
    /// included. The feed now carries the spawning owner's events only.
    ///
    /// MUTATION GUARD: drop the manifest owner from the worker's principal,
    /// or put `{owner_fragment}` back on the shared-context `workspace` arm,
    /// and this goes red.
    #[tokio::test]
    async fn process_mode_worker_prompt_carries_no_other_owners_spawn_text() {
        let parent = tempfile::TempDir::new().unwrap();
        let worker = tempfile::TempDir::new().unwrap();
        let shared_db = parent.path().join("memory").join("brain.db");
        std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();
        let shared_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db.clone(), false).unwrap();
        for (owner, session, role, content) in [
            (
                "owner-other",
                "telegram:chat-9:bob",
                "user",
                "bob private spawn task text",
            ),
            (
                "owner-other",
                "telegram:chat-9:bob",
                "event",
                "bob sub-agent result text",
            ),
            ("owner-a", "telegram:chat-2:alice", "user", "alice earlier shared event"),
        ] {
            shared_memory
                .append_message_event(crate::memory::MessageEventInput {
                    event_id: None,
                    idempotency_key: None,
                    workspace_id: parent.path().to_string_lossy().to_string(),
                    owner_id: Some(owner.to_string()),
                    source: "sessions_spawn".into(),
                    channel: Some("telegram".to_string()),
                    session_key: Some(session.to_string()),
                    parent_session_key: None,
                    run_id: Some(format!("run-{owner}")),
                    parent_run_id: None,
                    agent_id: None,
                    persona_id: None,
                    sender: None,
                    recipient: None,
                    role: role.to_string(),
                    event_type: "message.created".to_string(),
                    subject: None,
                    goal_id: None,
                    causation_event_id: None,
                    correlation_id: None,
                    attempt_id: None,
                    lease_epoch: None,
                    config_generation_id: Some(0),
                    config_source_revision: None,
                    content: content.to_string(),
                    raw_payload_json: None,
                    visibility: crate::memory::MemoryVisibility::Workspace,
                })
                .await
                .expect("test: seed a workspace event");
        }
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-hybrid".to_string(),
            task: "use context".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: parent.path().join("config"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker.path().join("brain.db"),
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(shared_db),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };
        let context = load_worker_shared_context(&manifest, &Config::default(), &shared_memory).await;

        assert!(
            context.contains("alice earlier shared event"),
            "the spawning owner's own events still reach the worker: {context}"
        );
        assert!(
            !context.contains("bob private spawn task text") && !context.contains("bob sub-agent result text"),
            "another owner's spawn and result text must not reach the worker prompt: {context}"
        );
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
            .with_owner_id("owner-a")
            .with_session_key("telegram:chat-1:alice")
            .with_run_id("run-hybrid")
            .with_parent_run_id("run-parent")
            .with_agent_id("agent-a")
            .with_persona_id("persona-a");
        let result = WorkerResult {
            completion_evidence: None,
            ordinary_completion: None,
            operator_completion: None,
            success: true,
            output: "worker draft content".to_string(),
            error: None,
            tokens_used: None,
        };
        let result_event = worker_fabric
            .record_worker_result(scope.clone(), result.output.clone(), None)
            .await
            .unwrap();
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-hybrid".to_string(),
            task: "produce draft".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: parent.path().join("config"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker_db,
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(shared_db.clone()),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
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
            &SecurityPolicy::default(),
        )
        .await;

        let drafts = worker_memory
            .list_memory_drafts_for_run(
                &crate::memory::traits::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: "workspace".to_string(),
                    agent_id: Some("system".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                "run-hybrid",
            )
            .await
            .unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = drafts.first();
        assert_eq!(draft.map(|draft| draft.status.as_str()), Some("pending"));
        assert_eq!(draft.and_then(|draft| draft.owner_id.as_deref()), Some("owner-a"));
        assert_eq!(draft.map(|draft| draft.content.as_str()), Some("worker draft content"));
        assert_eq!(
            draft.and_then(|draft| draft.source_event_id.as_deref()),
            Some(result_event.event_id.as_str())
        );

        let parent_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db, false).unwrap();
        let parent_events = parent_memory
            .list_memory_events_since(
                &crate::memory::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: parent.path().to_string_lossy().to_string(),
                    agent_id: Some("agent-a".to_string()),
                    persona_id: Some("persona-a".to_string()),
                    session_key: Some("telegram:chat-1:alice".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
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
    async fn hybrid_worker_draft_obeys_readonly_resource_gate() {
        let parent = tempfile::TempDir::new().unwrap();
        let worker = tempfile::TempDir::new().unwrap();
        let shared_db = parent.path().join("memory").join("brain.db");
        let worker_db = worker.path().join("brain.db");
        std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();

        let worker_memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(worker_db.clone(), false).unwrap());
        let worker_fabric = MemoryFabric::new(worker_memory.clone(), worker.path().to_string_lossy().to_string());
        let scope = MessageEventScope::new("session_worker", crate::memory::MemoryVisibility::Workspace)
            .with_owner_id("owner-a")
            .with_session_key("telegram:chat-1:alice")
            .with_run_id("run-hybrid");
        let result = WorkerResult {
            completion_evidence: None,
            ordinary_completion: None,
            operator_completion: None,
            success: true,
            output: "worker draft content".to_string(),
            error: None,
            tokens_used: None,
        };
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-hybrid".to_string(),
            task: "produce draft".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: parent.path().join("config"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: worker.path().to_path_buf(),
            memory_db_path: worker_db,
            memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
            memory_strategy: Some("hybrid".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(shared_db),
            worker_memory_db_path: Some(worker.path().join("brain.db")),
            agent_id: None,
            persona_id: None,
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            spawn_depth: 1,
            session_scope_key: "telegram:chat-1:alice".to_string(),
            parent_run_id: None,
            compaction_config: None,
        };
        let readonly = SecurityPolicy {
            autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };

        record_hybrid_worker_draft_if_needed(
            &manifest,
            &Config::default(),
            &worker_fabric,
            &scope,
            &result,
            None,
            &result.output,
            &readonly,
        )
        .await;

        let drafts = worker_memory
            .list_memory_drafts_for_run(
                &crate::memory::traits::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: "workspace".to_string(),
                    agent_id: Some("system".to_string()),
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                "run-hybrid",
            )
            .await
            .unwrap();
        assert!(drafts.is_empty());
    }

    #[tokio::test]
    async fn process_mode_restores_spawn_context_for_nested_runs() {
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: std::path::PathBuf::from("/tmp/openprx"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: None,
            runtime_config_source_revision: None,
            workspace_dir: std::path::PathBuf::from("/tmp/ws"),
            memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
            memory_workspace_id: Some("/tmp/ws".to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/ws/memory/brain.db")),
            worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: Some("owner-a".to_string()),
            owner_isolated: false,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            spawn_depth: 1,
            session_scope_key: "signal:group:test".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };

        let snapshot = with_manifest_spawn_context(&manifest, None, None, None, None, async {
            crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                .try_with(|ctx| {
                    (
                        ctx.run_id.clone(),
                        ctx.session_scope_key.clone(),
                        ctx.spawn_depth,
                        ctx.owner_id.clone(),
                        ctx.topic_id.clone(),
                        ctx.source_message_event_id.clone(),
                    )
                })
                .ok()
        })
        .await;
        assert_eq!(
            snapshot,
            Some((
                "run-child".to_string(),
                "signal:group:test".to_string(),
                1usize,
                Some("owner-a".to_string()),
                Some("topic-a".to_string()),
                Some("msg-a".to_string())
            ))
        );
    }

    #[test]
    fn worker_event_scope_preserves_spawn_lineage_and_owner_isolation() {
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            capability_manifest: None,
            child_source: None,
            ordinary_source: None,
            operator_source: None,
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            route_hint: None,
            route_source_provider: None,
            route_credential: None,
            route_uses_root_connection: false,
            direct_uses_root_connection: false,
            temperature: 0.7,
            config_dir: std::path::PathBuf::from("/tmp/openprx"),
            config_generation: "0".repeat(64),
            runtime_config_generation_id: Some(17),
            runtime_config_source_revision: Some("revision-17".to_string()),
            workspace_dir: std::path::PathBuf::from("/tmp/worker"),
            memory_db_path: std::path::PathBuf::from("/tmp/parent/memory/brain.db"),
            memory_workspace_id: Some("/tmp/parent".to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            memory_backend: "sqlite".to_string(),
            shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/parent/memory/brain.db")),
            worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            memory_event_recording: crate::memory::MemoryEventRecording::default(),
            allowed_tools: Vec::new(),
            channel_excluded_tools: Vec::new(),
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
            owner_isolated: true,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
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
        assert_eq!(scope.config_generation_id, Some(17));
        assert_eq!(scope.config_source_revision.as_deref(), Some("revision-17"));
        assert_eq!(scope.parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(scope.owner_id.as_deref(), Some("owner-a"));
        assert_eq!(scope.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(scope.persona_id.as_deref(), Some("persona-a"));
        assert_eq!(scope.sender.as_deref(), Some("alice"));
        assert_eq!(scope.recipient.as_deref(), Some("chat-1"));

        let envelope = worker_runtime_envelope(&manifest);
        let principal = envelope.memory_principal();
        assert_eq!(principal.workspace_id, "/tmp/parent");
        assert_eq!(principal.session_key.as_deref(), Some("telegram:chat-1:alice"));
        assert_eq!(principal.channel.as_deref(), Some("telegram"));
        assert_eq!(principal.sender.as_deref(), Some("alice"));
        assert_eq!(principal.owner_id.as_deref(), Some("owner-a"));
        assert!(principal.require_owner_match);
        assert!(envelope.owner_isolated);

        let write_context = envelope.memory_write_context("direct");
        assert_eq!(write_context.channel.as_deref(), Some("telegram"));
        assert_eq!(write_context.chat_id.as_deref(), Some("chat-1"));
        assert_eq!(write_context.raw_sender.as_deref(), Some("alice"));
        let payload = worker_lineage_payload(&manifest);
        assert_eq!(
            payload.get("owner_id").and_then(serde_json::Value::as_str),
            Some("owner-a")
        );
        assert_eq!(
            payload.get("topic_id").and_then(serde_json::Value::as_str),
            Some("topic-a")
        );
        assert_eq!(
            payload.get("parent_task_id").and_then(serde_json::Value::as_str),
            Some("run-parent")
        );
        assert_eq!(
            payload
                .get("source_message_event_id")
                .and_then(serde_json::Value::as_str),
            Some("msg-a")
        );
    }

    fn retain_worker_probe_directory(mut tmp: tempfile::TempDir) -> tempfile::TempDir {
        tmp.disable_cleanup(true);
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("worker probe retained owned scratch {}", tmp.path().display());
        });
        tmp
    }

    /// Drive the real process-worker entrypoint (`run_validated_manifest`)
    /// against `provider_name`: a real parent config source, a shared-fabric
    /// SQLite memory and the worker workspace all live under `tmp`.
    async fn worker_probe_run(tmp: &tempfile::TempDir, provider_name: String) -> WorkerResult {
        with_worker_probe_manifest(tmp, provider_name, false, |manifest| async move {
            let config_dir_arg = manifest.config_dir.to_string_lossy().to_string();
            run_validated_manifest(manifest, None, Some(&config_dir_arg)).await
        })
        .await
        .expect("test: the worker runs to its result")
    }

    async fn with_worker_probe_manifest<T, F, Fut>(
        tmp: &tempfile::TempDir,
        provider_name: String,
        generation_negative: bool,
        consume: F,
    ) -> Result<T>
    where
        F: FnOnce(WorkerManifest) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        use crate::providers::credential_probe::PROBE_MODEL;
        // Retain only the existing driver's neutral decision request/reply evidence.
        // A fresh directory under the owned scratch cannot reuse an external target.
        let diagnostics = tmp.path().join("decision-evidence");
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            directory.mode(0o700);
        }
        directory.create(&diagnostics)?;
        let metadata = std::fs::symlink_metadata(&diagnostics)?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "owned decision evidence directory required"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o777 == 0o700,
                "private decision evidence directory required"
            );
        }
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await?
            .retain_evidence_at(diagnostics);
        let endpoint = driver.endpoint().to_owned();
        driver
            .run_factory(!generation_negative, || async {
                let config_dir = tmp.path().join("config");
                let workspace = config_dir.join("workspace");
                std::fs::create_dir_all(&workspace)?;
                let mut config = crate::config::Config::default();
                config.config_path = config_dir.join("config.toml");
                config.workspace_dir = workspace.clone();
                config.default_provider = Some("ollama".into());
                config.default_model = Some("prx-local-model".into());
                config.memory.backend = "sqlite".into();
                config.autonomy.level = crate::security::AutonomyLevel::Full;
                config.sessions_spawn.default_mode = "process".into();
                config.sessions_spawn.cleanup_on_complete = false;
                config.providers.models.push(toml::from_str(
                    "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
                )?);
                config.providers.models.push(serde_json::from_value(serde_json::json!({
                    "provider":provider_name,"model":PROBE_MODEL,"operations":["chat"]
                }))?);
                config.cost.prices.insert(
                    format!("{provider_name}/{PROBE_MODEL}"),
                    crate::config::schema::ModelPricing {
                        input: 0.1,
                        output: 0.2,
                        cache_write: 0.3,
                        cache_read: 0.1,
                    },
                );
                crate::agent::agent::native_decision_fixture::configure(&mut config, &endpoint)?;
                std::fs::write(&config.config_path, toml::to_string(&config)?)?;
                let mut manifest = base_manifest(&tmp.path().join("worker"), "unused-for-direct-validation");
                manifest.config_dir = config_dir;
                manifest.provider_name = provider_name;
                manifest.model = PROBE_MODEL.to_owned();
                manifest.task = "Describe the project.".to_owned();
                manifest.allowed_tools = Vec::new();
                manifest.identity_dir = None;
                manifest.memory_db_path = workspace.join("memory/brain.db");
                manifest.shared_memory_db_path = Some(manifest.memory_db_path.clone());
                manifest.memory_workspace_id = Some(workspace.to_string_lossy().into_owned());
                manifest.memory_event_recording = config.memory.event_recording_config();
                crate::runtime::turn_decision::with_worker_probe_manifest(config, manifest, consume).await
            })
            .await
    }

    /// Event types the worker's shared-fabric memory recorded for its workspace.
    async fn worker_probe_event_types(tmp: &tempfile::TempDir) -> Vec<String> {
        let workspace = tmp.path().join("config/workspace");
        let memory = crate::memory::SqliteMemory::new(&workspace).expect("test: reopen worker memory");
        memory
            .list_message_events_recent(
                &crate::memory::MemoryPrincipal {
                    require_owner_match: false,
                    workspace_id: workspace.to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                100,
            )
            .await
            .expect("test: worker events readable")
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    /// D-053 / REDACT-TURN probe: a process-mode worker whose provider reply
    /// carries a secret runs `run_validated_manifest` through `finalize_turn`
    /// (Completed) and the `worker.result.created` event; nothing under the temp
    /// dir may keep the secret. It must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn session_worker_persists_no_secret_from_a_reply() {
        use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
        use crate::providers::credential_probe::CredentialProbe;
        let tmp = tempfile::TempDir::new().expect("test: tempdir");
        let tmp = retain_worker_probe_directory(tmp);
        let probe = CredentialProbe::start_replying(&format!("Here is the key {PROBE_SECRET} you asked for")).await;

        let result = worker_probe_run(&tmp, probe.custom_provider()).await;

        assert!(result.success, "the worker must complete: {:?}", result.error);
        assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
        assert!(
            result.output.contains(PROBE_SECRET),
            "the reply must carry the secret for the probe to mean anything: {}",
            result.output
        );
        // Config source, worker workspace (cost ledger, audit log) and the
        // shared-fabric SQLite memory are all under this temp dir. Scan before
        // reopening the database so the scan sees exactly what the run left.
        assert_nothing_under_persists_the_probe_secret(tmp.path());
        let events = worker_probe_event_types(&tmp).await;
        assert!(
            events.iter().any(|event| event == "turn.finalized"),
            "the worker must reach the terminal commit: {events:?}"
        );
        assert!(
            events.iter().any(|event| event == "worker.result.created"),
            "the worker must record its result event: {events:?}"
        );
    }

    /// D-053 / REDACT-TURN probe: a process-mode worker whose provider fails
    /// with an error carrying a secret runs `run_validated_manifest` through
    /// `finalize_turn` (Failed) and the `worker.result.created` event; nothing
    /// under the temp dir may keep the secret. It must fail before the
    /// REDACT-TURN commits.
    #[tokio::test]
    async fn session_worker_persists_no_secret_from_a_provider_error() {
        use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
        use crate::providers::credential_probe::CredentialProbe;
        let tmp = tempfile::TempDir::new().expect("test: tempdir");
        let tmp = retain_worker_probe_directory(tmp);
        let probe = CredentialProbe::start_failing(400, &format!("upstream rejected key {PROBE_SECRET}")).await;

        let result = worker_probe_run(&tmp, probe.custom_provider()).await;

        assert!(!result.success, "a failing upstream must fail the worker");
        assert!(!probe.requests().is_empty(), "the request must reach the endpoint");
        let error = result.error.unwrap_or_default();
        // The upstream's error text must reach the turn: verbatim before the
        // redactors merged (D-054), already redacted by the provider layer since.
        assert!(
            error.contains(PROBE_SECRET) || error.contains("upstream rejected key [REDACTED]"),
            "the provider error must reach the turn for the probe to mean anything: {error}"
        );
        // Config source, worker workspace (cost ledger, audit log) and the
        // shared-fabric SQLite memory are all under this temp dir. Scan before
        // reopening the database so the scan sees exactly what the run left.
        assert_nothing_under_persists_the_probe_secret(tmp.path());
        let events = worker_probe_event_types(&tmp).await;
        assert!(
            events.iter().any(|event| event == "turn.finalized"),
            "the worker must reach the terminal commit: {events:?}"
        );
        assert!(
            events.iter().any(|event| event == "worker.result.created"),
            "the worker must record its result event: {events:?}"
        );
    }
}

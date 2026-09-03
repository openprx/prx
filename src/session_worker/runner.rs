use crate::agent::loop_::{
    DocumentIngestRuntime, ScopeContext, build_context_with_shared_events_and_scope, run_tool_call_loop_traced,
};
use crate::channels::build_identity_prompt;
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

fn write_worker_result(result: &WorkerResult) -> Result<()> {
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
type SteeredSegment = (Vec<ChatMessage>, Result<(String, crate::agent::loop_::ToolLoopTrace)>);

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
    F: FnMut(Vec<ChatMessage>, CancellationToken) -> Fut,
    Fut: Future<Output = SteeredSegment>,
{
    loop {
        let cancel = CancellationToken::new();
        let segment = run_segment(history, cancel.clone());
        tokio::pin!(segment);

        let steered = tokio::select! {
            finished = &mut segment => return finished,
            steer = next_steer(steer_rx) => steer,
        };

        match steered {
            Some(message) => {
                cancel.cancel();
                let (mut advanced, _cancelled) = segment.await;
                advanced.push(ChatMessage::user(steering_instruction(&message)));
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

fn select_tools_for_worker(source: Vec<Box<dyn Tool>>, allowed_tools: &[String]) -> Result<Vec<Box<dyn Tool>>> {
    let normalized = allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() || normalized.as_slice() == ["*"] {
        return Ok(source);
    }
    if normalized.contains(&"*") {
        anyhow::bail!("Worker allowed_tools must use '*' exclusively");
    }

    let mut selected = Vec::new();
    let mut remaining = source;

    for allowed in normalized {
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
fn validate_worker_capability_with_env(manifest: &WorkerManifest, env_capability: Option<&str>) -> Result<()> {
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

    let expiry = capability_expiry_from_env()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now > expiry {
        anyhow::bail!("session-worker capability expired");
    }

    let payload = manifest_signing_payload(manifest)?;
    let expected = compute_worker_capability(&manifest.run_id, expiry, &payload);
    if !capability_constant_time_eq(env_capability.as_bytes(), expected.as_bytes()) {
        anyhow::bail!("session-worker capability signature mismatch");
    }

    Ok(())
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
fn compute_worker_capability(run_id: &str, expiry_unix: u64, manifest_json: &str) -> String {
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

    let tag = hmac::sign(&session_worker_signing_key(), &payload);
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
/// (mirror of the minting side; uses `HOME` directly, no `dirs` dependency).
fn session_secret_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENPRX_SESSION_WORKER_SECRET_PATH") {
        return Some(std::path::PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    Some(home.join(".openprx").join("keys").join("session_worker.secret"))
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

fn validate_worker_manifest_with_capability_env(manifest: &WorkerManifest, env_capability: Option<&str>) -> Result<()> {
    validate_worker_capability_with_env(manifest, env_capability)?;

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

    Ok(())
}

fn validate_worker_manifest(manifest: &WorkerManifest) -> Result<()> {
    let env_capability = std::env::var("OPENPRX_SESSION_WORKER_CAPABILITY").ok();
    validate_worker_manifest_with_capability_env(manifest, env_capability.as_deref())
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

async fn run_validated_manifest(
    manifest: WorkerManifest,
    mut steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
) -> Result<WorkerResult> {
    let explicit_config_dir = explicit_config_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker requires the parent --config-dir")?;
    if Path::new(explicit_config_dir) != manifest.config_dir {
        anyhow::bail!("session-worker config source does not match sealed manifest");
    }

    let generation_before = config_source_generation(&manifest.config_dir)?;
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

    let provider: Arc<dyn Provider> = Arc::from(crate::providers::create_resilient_provider_with_options(
        &manifest.provider_name,
        manifest.api_key.as_deref().or(config.api_key.as_deref()),
        config.api_url.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    )?);

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
            &config.embedding_routes,
            Some(&config.storage.provider.config),
            &parent_workspace,
            manifest.api_key.as_deref().or(config.api_key.as_deref()),
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
    let worker_event_scope = worker_message_event_scope(&manifest);
    SideEffectGate::new(security.as_ref())
        .authorize_resource_operation(
            "session_worker",
            &format!("session_worker:request_event:{}", manifest.run_id),
            ResourceRiskLevel::Low,
            None,
        )
        .map_err(anyhow::Error::msg)?;
    if let Err(error) = memory_fabric
        .record_inbound_user_message(
            worker_event_scope.clone(),
            manifest.task.clone(),
            Some(format!("session_worker:{}:request", manifest.run_id)),
            Some(worker_lineage_payload(&manifest).to_string()),
        )
        .await
    {
        tracing::warn!(run_id = %manifest.run_id, "failed to record session-worker request event: {error}");
    }

    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let shared_config = crate::config::new_shared(config.clone());
    let hooks = Arc::new(HookManager::new(manifest.workspace_dir.clone()));
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
    let full_tools = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        shared_config,
        &security,
        runtime,
        memory.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &manifest.workspace_dir,
        &config.agents,
        manifest.api_key.as_deref().or(config.api_key.as_deref()),
        &config,
        extensions,
    )
    .tools;

    let tools_registry = select_tools_for_worker(full_tools, &manifest.allowed_tools)?;
    let system_prompt = resolve_system_prompt(&manifest);
    let shared_context = load_worker_shared_context(&manifest, &config, memory.as_ref()).await;
    let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        manifest.provider_name.clone(),
        manifest.model.clone(),
        manifest
            .owner_id
            .clone()
            .unwrap_or_else(|| "owner:session-worker".to_string()),
        worker_event_scope
            .session_key
            .clone()
            .unwrap_or_else(|| format!("session-worker:{}", manifest.run_id)),
        manifest.source_message_event_id.clone(),
        None,
        "session_worker",
        u32::try_from(manifest.task.chars().count() / 4).unwrap_or(u32::MAX),
        !tools_registry.is_empty(),
        false,
    );
    let provider_started_at = chrono::Utc::now();

    let run_future = async {
        let user_task = if shared_context.trim().is_empty() {
            manifest.task.clone()
        } else {
            format!("{shared_context}{}", manifest.task)
        };
        let initial_history = vec![ChatMessage::system(system_prompt), ChatMessage::user(user_task)];

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
                    owner_id: manifest.owner_id.as_deref(),
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
        let routing_input_ref: &str = &manifest.task;
        let workspace_dir_ref: &Path = &manifest.workspace_dir;
        let compaction_config_ref = manifest.compaction_config.as_ref();
        let multimodal_ref = &config.multimodal;
        let tool_tiering_ref = &config.tool_tiering;
        let low_priority_tools_ref: &[String] = &config.agent.low_priority_tools;
        let temperature = manifest.temperature;
        let read_only_window = config.agent.read_only_tool_concurrency_window;
        let priority_scheduling_enabled = config.agent.priority_scheduling_enabled;
        let tools_registry = Arc::new(tools_registry);
        let tools_registry_ref = &tools_registry;
        let (history, loop_result) =
            run_segments_with_steering(initial_history, &mut steer_rx, |mut segment_history, cancel| {
                let tools_registry = Arc::clone(tools_registry_ref);
                let memory = Arc::clone(memory_ref);
                async move {
                    let loop_result = run_tool_call_loop_traced(
                        provider_ref,
                        &mut segment_history,
                        tools_registry,
                        observer_ref,
                        hooks_ref,
                        provider_name_ref,
                        model_ref,
                        temperature,
                        true,
                        None,
                        "session-worker",
                        multimodal_ref,
                        read_only_window,
                        priority_scheduling_enabled,
                        low_priority_tools_ref.to_vec(),
                        compaction_config_ref,
                        // Steering cancels the in-flight segment; without a token the
                        // worker could only append after the loop finished on its own.
                        Some(cancel),
                        None,
                        scope_ctx_ref,
                        None,
                        Some(tool_tiering_ref),
                        // The ledger comes from `memory` directly: a session worker without a
                        // resolved ingest scope must still be able to run side-effecting tools.
                        crate::agent::loop_::ToolLoopMemory::new(
                            &memory,
                            workspace_dir_ref,
                            scope_ctx_ref.map(|ctx| DocumentIngestRuntime::from_scope(Arc::clone(&memory), ctx)),
                        )
                        .with_routing_input(routing_input_ref),
                        crate::agent::loop_::ChatMode::default(),
                    )
                    .await;
                    (segment_history, loop_result)
                }
            })
            .await;
        (loop_result, history.len())
    };

    let run_future = with_manifest_spawn_context(&manifest, run_future);

    let result = run_future.await;

    let (loop_result, history_commit_len) = result;
    let (worker_result, provider_outcome, terminal_status) = match loop_result {
        Ok((output, trace)) => {
            let tokens_used = trace.tokens_used.clone();
            (
                WorkerResult {
                    success: true,
                    output: if output.trim().is_empty() {
                        "[Sub-agent produced no output]".to_string()
                    } else {
                        output
                    },
                    error: None,
                    tokens_used: tokens_used.has_any_tokens().then_some(tokens_used),
                },
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, trace),
                crate::agent::terminal::TurnTerminalStatus::Completed,
            )
        }
        Err(error) => (
            WorkerResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
                tokens_used: None,
            },
            crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &error,
            ),
            crate::agent::terminal::TurnTerminalStatus::Failed,
        ),
    };

    if let Err(error) = crate::agent::terminal::finalize_turn(
        &memory_fabric,
        crate::agent::terminal::TurnTerminalCommit {
            terminal_id: manifest.run_id.clone(),
            scope: worker_event_scope.clone(),
            status: terminal_status,
            history: worker_result
                .success
                .then(|| crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: worker_result.output.clone(),
                    history_commit_len,
                }),
            history_scope: None,
            provider_outcome: Some(provider_outcome),
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
        &config.cost,
        &config.workspace_dir,
    )
    .await
    {
        tracing::warn!(run_id = %manifest.run_id, error = %error, "failed to commit session-worker terminal event");
    }

    let event_content = if worker_result.output.trim().is_empty() {
        worker_result
            .error
            .clone()
            .unwrap_or_else(|| "[session-worker produced no output]".to_string())
    } else {
        worker_result.output.clone()
    };
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
            event_content.clone(),
            Some(
                serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
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
        security.as_ref(),
    )
    .await;

    Ok(worker_result)
}

async fn run_manifest_with_capability_env(
    manifest: WorkerManifest,
    env_capability: Option<&str>,
    explicit_config_dir: Option<&str>,
) -> Result<WorkerResult> {
    validate_worker_manifest_with_capability_env(&manifest, env_capability)?;
    run_validated_manifest(manifest, None, explicit_config_dir).await
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
    validation?;
    let steer_rx = read_stdin_control_frames.then(|| {
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
        spawn_stdin_control_reader(steer_tx);
        steer_rx
    });
    run_validated_manifest(manifest, steer_rx, explicit_config_dir).await
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
                serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "merge_policy": "parent_decides",
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
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

    build_context_with_shared_events_and_scope(
        shared_memory,
        runtime_envelope.memory_principal(),
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
                    owner_id: manifest.owner_id.clone(),
                    topic_id: manifest.topic_id.clone(),
                    source_message_event_id: manifest.source_message_event_id.clone(),
                    // A resumed spawn-run process (not a turn root): children
                    // compute spawn_depth + 1, preserving the manifest depth chain.
                    is_turn_root: false,
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
    explicit_config_dir: Option<String>,
) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .read_line(&mut raw)
        .context("read worker manifest from stdin")?;

    let manifest: WorkerManifest = match serde_json::from_str(raw.trim()) {
        Ok(value) => value,
        Err(error) => {
            let result = WorkerResult {
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
        Err(error) => WorkerResult {
            success: false,
            output: String::new(),
            error: Some(error.to_string()),
            tokens_used: None,
        },
    };

    write_worker_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_manifest(workspace: &Path, capability: &str) -> WorkerManifest {
        WorkerManifest {
            parent_capability: Some(capability.to_string()),
            run_id: "run-worker".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: Some("identity/worker".to_string()),
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: None,
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
    fn worker_wildcard_inherits_complete_tool_registry() {
        let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
        let expected = source.len();
        let selected = select_tools_for_worker(source, &["*".to_string()]).expect("wildcard selection");
        assert_eq!(selected.len(), expected);
    }

    #[test]
    fn worker_wildcard_must_be_exclusive() {
        let source = crate::tools::default_tools(Arc::new(crate::security::SecurityPolicy::default()));
        let error = match select_tools_for_worker(source, &["*".to_string(), "shell".to_string()]) {
            Ok(_) => panic!("mixed wildcard must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exclusively"));
    }

    /// A fake agent-loop segment: parks until cancelled for its first
    /// `natural_completion_on - 1` entries, then completes on its own. This
    /// exercises the steering race — cancel, unwind, re-enter, finish — without
    /// a provider, config, or memory backend.
    fn segment_cancellable_until(
        entries: Arc<std::sync::atomic::AtomicUsize>,
        natural_completion_on: usize,
    ) -> impl FnMut(Vec<ChatMessage>, CancellationToken) -> std::pin::Pin<Box<dyn Future<Output = SteeredSegment> + Send>>
    {
        move |history, cancel| {
            let entries = entries.clone();
            Box::pin(async move {
                let entry = entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if entry < natural_completion_on {
                    cancel.cancelled().await;
                }
                let output = format!("finished on entry {entry}");
                (history, Ok((output, crate::agent::loop_::ToolLoopTrace::default())))
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
        let (finished, result) = run_segments_with_steering(history_in, &mut steer_rx, |history, cancel| {
            let entries = entries.clone();
            async move {
                entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                cancel.cancel();
                cancel.cancelled().await;
                (
                    history,
                    Ok(("done".to_string(), crate::agent::loop_::ToolLoopTrace::default())),
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

        let (history, result) =
            run_segments_with_steering(vec![ChatMessage::user("task")], &mut steer_rx, |history, cancel| {
                let entries = entries.clone();
                async move {
                    entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // A closed channel resolves immediately; the segment must
                    // still be awaited to its own end rather than abandoned.
                    tokio::task::yield_now().await;
                    cancel.cancel();
                    (
                        history,
                        Ok(("natural".to_string(), crate::agent::loop_::ToolLoopTrace::default())),
                    )
                }
            })
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
    // `run_manifest_with_capability_env` call so the capability env stays stable
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

        let error = run_manifest_with_capability_env(manifest, Some(&cap), Some(&config_dir_arg))
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
        let config_dir = tmp.path().join("config");
        let workspace = tmp.path().join("worker");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), "default_temperature = 0.7\n").unwrap();
        let config_dir_arg = config_dir.to_string_lossy().to_string();
        let mut manifest = base_manifest(&workspace, "unused-for-direct-validation");
        manifest.config_dir = config_dir;
        manifest.config_generation = "0".repeat(64);

        let error = run_validated_manifest(manifest, None, Some(&config_dir_arg))
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
                owner_id: None,
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
            run_id: "run-hybrid".to_string(),
            task: "use context".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
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
            run_id: "run-hybrid".to_string(),
            task: "produce draft".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
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
            success: true,
            output: "worker draft content".to_string(),
            error: None,
            tokens_used: None,
        };
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            run_id: "run-hybrid".to_string(),
            task: "produce draft".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
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
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: Some("owner-a".to_string()),
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("run-parent".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            spawn_depth: 1,
            session_scope_key: "signal:group:test".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            compaction_config: None,
        };

        let snapshot = with_manifest_spawn_context(&manifest, async {
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
    fn worker_event_scope_preserves_spawn_lineage() {
        let manifest = WorkerManifest {
            parent_capability: Some("capability".to_string()),
            run_id: "run-child".to_string(),
            task: "noop".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
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
            system_prompt: None,
            identity_dir: None,
            scope_sender: Some("alice".to_string()),
            scope_channel: Some("telegram".to_string()),
            scope_chat_type: Some("direct".to_string()),
            scope_chat_id: Some("chat-1".to_string()),
            owner_id: Some("owner-a".to_string()),
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
}

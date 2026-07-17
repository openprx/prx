use crate::config::NodeServerConfig;
use crate::nodes::protocol::{
    AsyncTaskAccepted, CancelParams, ExecShellParams, ExecShellResult, JsonRpcRequest, JsonRpcResponse, MetricsResult,
    PingResult, ReadFileParams, ReadFileResult, TaskListItem, TaskListResult, TaskStatusParams, TaskStatusResult,
    WriteFileParams, WriteFileResult,
};
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get, routing::post};
use axum_server::tls_rustls::RustlsConfig;
use chrono::Utc;
use constant_time_eq::constant_time_eq;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

type HmacSha256 = Hmac<Sha256>;
const MUTATION_REPLAY_CAPACITY: usize = 128;
const HMAC_FRESHNESS_WINDOW_SECS: i64 = 300;
const MUTATION_REPLAY_MIN_TTL: Duration = Duration::from_secs((HMAC_FRESHNESS_WINDOW_SECS as u64).saturating_mul(2));
const MAX_RPC_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const NODE_SAFE_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const NODE_CALLER_ENV_ALLOWLIST: &[&str] = &["LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ"];

#[derive(Clone)]
enum CachedRpcOutcome {
    Success(Value),
    Failure(String),
}

impl CachedRpcOutcome {
    fn into_result(self) -> Result<Value> {
        match self {
            Self::Success(value) => Ok(value),
            Self::Failure(message) => Err(anyhow!(message)),
        }
    }
}

struct MutationReplay {
    fingerprint: String,
    outcome: OnceCell<CachedRpcOutcome>,
    completed_at: std::sync::OnceLock<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Running,
    Completed,
    Cancelled,
}

impl TaskState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
struct TaskResult {
    task_id: String,
    status: TaskState,
    started_at: Instant,
    completed_at: Option<Instant>,
    duration_ms: u64,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
}

impl TaskResult {
    fn running(task_id: String) -> Self {
        Self {
            task_id,
            status: TaskState::Running,
            started_at: Instant::now(),
            completed_at: None,
            duration_ms: 0,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            cancelled: false,
        }
    }

    fn current_duration_ms(&self, now: Instant) -> u64 {
        if self.status == TaskState::Running {
            elapsed_ms_u64(now.saturating_duration_since(self.started_at))
        } else {
            self.duration_ms
        }
    }

    fn apply_exec_result(&mut self, result: &ExecShellResult, completed_at: Instant) {
        self.status = if result.cancelled {
            TaskState::Cancelled
        } else {
            TaskState::Completed
        };
        self.completed_at = Some(completed_at);
        self.duration_ms = result.duration_ms;
        self.exit_code = result.exit_code;
        self.stdout = result.stdout.clone();
        self.stderr = result.stderr.clone();
        self.timed_out = result.timed_out;
        self.cancelled = result.cancelled;
    }

    fn to_status_result(&self, now: Instant) -> TaskStatusResult {
        if self.status == TaskState::Running {
            return TaskStatusResult {
                task_id: self.task_id.clone(),
                status: self.status.as_str().to_string(),
                duration_ms: self.current_duration_ms(now),
                exit_code: None,
                stdout: None,
                stderr: None,
                timed_out: None,
                cancelled: None,
            };
        }

        TaskStatusResult {
            task_id: self.task_id.clone(),
            status: self.status.as_str().to_string(),
            duration_ms: self.current_duration_ms(now),
            exit_code: self.exit_code,
            stdout: Some(self.stdout.clone()),
            stderr: Some(self.stderr.clone()),
            timed_out: Some(self.timed_out),
            cancelled: Some(self.cancelled),
        }
    }

    fn to_list_item(&self, now: Instant) -> TaskListItem {
        TaskListItem {
            task_id: self.task_id.clone(),
            status: self.status.as_str().to_string(),
            duration_ms: self.current_duration_ms(now),
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<NodeServerConfig>,
    sandbox_root: Arc<SandboxRoot>,
    running_tasks: Arc<RwLock<HashMap<String, CancellationToken>>>,
    task_results: Arc<RwLock<HashMap<String, TaskResult>>>,
    mutation_replays: Arc<Mutex<HashMap<String, Arc<MutationReplay>>>>,
    async_task_slots: Arc<Semaphore>,
}

struct SandboxRoot {
    path: PathBuf,
    #[cfg(unix)]
    directory: Arc<std::fs::File>,
}

pub async fn run_node_server(config: NodeServerConfig) -> Result<()> {
    let mut config = config;
    config.max_concurrent_tasks = config.max_concurrent_tasks.max(1);
    config.task_result_ttl_ms = config.task_result_ttl_ms.max(1);

    validate_tls_requirements(&config)?;
    let sandbox_root = prepare_sandbox_root(&config.sandbox_root)?;
    let state = AppState {
        config: Arc::new(config.clone()),
        sandbox_root: Arc::new(sandbox_root),
        running_tasks: Arc::new(RwLock::new(HashMap::new())),
        task_results: Arc::new(RwLock::new(HashMap::new())),
        mutation_replays: Arc::new(Mutex::new(HashMap::new())),
        async_task_slots: Arc::new(Semaphore::new(config.max_concurrent_tasks)),
    };

    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/health", get(handle_health))
        .layer(DefaultBodyLimit::max(MAX_RPC_REQUEST_BYTES))
        .with_state(state);

    if let Some(tls_config) = load_tls_config(&config).await? {
        let addr = parse_listen_addr(&config.listen_addr)?;
        tracing::info!(
            "prx-node listening with TLS on {}, sandbox_root={}",
            config.listen_addr,
            config.sandbox_root
        );
        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("node TLS server exited with error")
    } else {
        let listener = tokio::net::TcpListener::bind(&config.listen_addr)
            .await
            .with_context(|| format!("failed to bind {}", config.listen_addr))?;

        tracing::info!(
            "prx-node listening without TLS on {}, sandbox_root={}",
            config.listen_addr,
            config.sandbox_root
        );

        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .context("node TCP server exited with error")
    }
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

async fn handle_rpc(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    if let Err(error) = validate_auth(&headers, &state.config) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::failure("0".into(), -32001, error.to_string())),
        );
    }

    if let Err(error) = validate_hmac(&headers, &body, &state.config) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::failure("0".into(), -32002, error.to_string())),
        );
    }

    let request: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(req) => req,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(JsonRpcResponse::failure(
                    "0".into(),
                    -32700,
                    format!("invalid JSON payload: {error}"),
                )),
            );
        }
    };

    let source_ip = addr.ip().to_string();

    let id = request.id.clone();
    let result = if is_mutating_method(&request.method) {
        dispatch_idempotent_mutation(&state, request, &source_ip).await
    } else {
        dispatch_rpc(&state, request, &source_ip).await
    };

    match result {
        Ok(payload) => (StatusCode::OK, Json(JsonRpcResponse::success(id, payload))),
        Err(error) => (
            StatusCode::OK,
            Json(JsonRpcResponse::failure(id, -32000, error.to_string())),
        ),
    }
}

fn is_mutating_method(method: &str) -> bool {
    matches!(method, "node.exec_shell" | "node.write_file" | "node.cancel")
}

fn mutation_fingerprint(request: &JsonRpcRequest) -> Result<String> {
    use sha2::Digest;

    let mut digest = Sha256::new();
    digest.update(request.method.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(&request.params)?);
    Ok(hex::encode(digest.finalize()))
}

async fn dispatch_idempotent_mutation(state: &AppState, request: JsonRpcRequest, source_ip: &str) -> Result<Value> {
    if request.id.trim().is_empty() || request.id.len() > 128 {
        bail!("JSON-RPC mutation id must contain 1 to 128 bytes");
    }
    let fingerprint = mutation_fingerprint(&request)?;
    let request_id = request.id.clone();
    let ttl = MUTATION_REPLAY_MIN_TTL;
    let replay = {
        let mut replays = state.mutation_replays.lock().await;
        let now = Instant::now();
        replays.retain(|_, replay| {
            replay.outcome.get().is_none()
                || replay
                    .completed_at
                    .get()
                    .is_some_and(|completed_at| now.saturating_duration_since(*completed_at) <= ttl)
        });

        if let Some(existing) = replays.get(&request_id) {
            if existing.fingerprint != fingerprint {
                bail!("JSON-RPC mutation id reused with different method or params");
            }
            Arc::clone(existing)
        } else {
            if replays.len() >= MUTATION_REPLAY_CAPACITY {
                bail!("node mutation replay cache capacity reached");
            }
            let replay = Arc::new(MutationReplay {
                fingerprint,
                outcome: OnceCell::new(),
                completed_at: std::sync::OnceLock::new(),
            });
            replays.insert(request_id, Arc::clone(&replay));
            replay
        }
    };

    replay
        .outcome
        .get_or_init(|| async {
            let outcome = match dispatch_rpc(state, request, source_ip).await {
                Ok(value) => CachedRpcOutcome::Success(value),
                Err(error) => CachedRpcOutcome::Failure(error.to_string()),
            };
            let _ = replay.completed_at.set(Instant::now());
            outcome
        })
        .await
        .clone()
        .into_result()
}

async fn dispatch_rpc(state: &AppState, request: JsonRpcRequest, source_ip: &str) -> Result<Value> {
    match request.method.as_str() {
        "node.ping" => Ok(serde_json::to_value(PingResult {
            message: "pong".into(),
            timestamp: Utc::now(),
        })?),
        "node.exec_shell" => {
            let params: ExecShellParams = serde_json::from_value(request.params)?;
            handle_exec_shell(state, params, source_ip).await
        }
        "node.read_file" => {
            let params: ReadFileParams = serde_json::from_value(request.params)?;
            let output = handle_read_file(state, params).await?;
            Ok(serde_json::to_value(output)?)
        }
        "node.write_file" => {
            let params: WriteFileParams = serde_json::from_value(request.params)?;
            let output = handle_write_file(state, params).await?;
            Ok(serde_json::to_value(output)?)
        }
        "node.cancel" => {
            let params: CancelParams = serde_json::from_value(request.params)?;
            handle_cancel(state, params, source_ip).await?;
            Ok(json!({"ok": true}))
        }
        "node.task_status" => {
            let params: TaskStatusParams = serde_json::from_value(request.params)?;
            let result = handle_task_status(state, params).await?;
            Ok(serde_json::to_value(result)?)
        }
        "node.task_list" => {
            let result = handle_task_list(state).await?;
            Ok(serde_json::to_value(result)?)
        }
        "node.metrics" => Ok(serde_json::to_value(read_metrics()?)?),
        method => bail!("unsupported method: {method}"),
    }
}

async fn handle_exec_shell(state: &AppState, params: ExecShellParams, source_ip: &str) -> Result<Value> {
    cleanup_expired_task_results(state).await;

    let command_text = params.cmd.trim();
    if command_text.is_empty() {
        bail!("cmd cannot be empty");
    }

    let parsed_command = parse_command(command_text)?;
    validate_command(
        &parsed_command,
        &state.config.allowed_commands,
        &state.config.blocked_commands,
    )?;

    let cwd = if let Some(cwd) = params.cwd.as_deref() {
        Some(resolve_existing_sandbox_path(&state.sandbox_root.path, cwd)?)
    } else {
        None
    };

    let callback_url = validate_callback_url(params.callback_url.as_deref())?;
    let timeout_ms = params.timeout_ms.unwrap_or(state.config.exec_timeout_ms);
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let async_exec = params.async_exec.unwrap_or(false);
    let env = validate_node_environment(params.env)?;
    let async_permit = async_exec
        .then(|| Arc::clone(&state.async_task_slots).try_acquire_owned())
        .transpose()
        .map_err(|_| {
            anyhow!(
                "max concurrent async tasks reached ({})",
                state.config.max_concurrent_tasks
            )
        })?;

    let task_id = uuid::Uuid::new_v4().to_string();
    let cancellation = CancellationToken::new();
    register_task(state, &task_id, cancellation.clone()).await;

    if async_exec {
        let state_clone = state.clone();
        let parsed_clone = parsed_command.clone();
        let cwd_clone = cwd.clone();
        let callback_url_clone = callback_url.clone();
        let source_ip_owned = source_ip.to_string();
        let task_id_clone = task_id.clone();

        tokio::spawn(async move {
            let _permit = async_permit;
            let result = execute_shell_command(
                &state_clone,
                &parsed_clone,
                cwd_clone,
                env,
                timeout,
                &task_id_clone,
                cancellation,
            )
            .await;

            let callback_payload = finalize_task(state_clone.clone(), &result).await;
            log_exec_shell_event(&parsed_clone, &result, &source_ip_owned);

            if let Some(url) = callback_url_clone {
                send_callback(url, callback_payload).await;
            }
        });

        let accepted = AsyncTaskAccepted {
            task_id,
            status: "running".to_string(),
        };
        return Ok(serde_json::to_value(accepted)?);
    }

    let result = execute_shell_command(state, &parsed_command, cwd, env, timeout, &task_id, cancellation).await;

    let callback_payload = finalize_task(state.clone(), &result).await;
    log_exec_shell_event(&parsed_command, &result, source_ip);

    if let Some(url) = callback_url {
        tokio::spawn(async move {
            send_callback(url, callback_payload).await;
        });
    }

    Ok(serde_json::to_value(result)?)
}

async fn execute_shell_command(
    state: &AppState,
    parsed_command: &ParsedCommand,
    cwd: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    timeout: Duration,
    task_id: &str,
    cancellation: CancellationToken,
) -> ExecShellResult {
    let started = Instant::now();
    let mut cmd = Command::new(&parsed_command.program);
    cmd.args(&parsed_command.args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.env_clear();
    for variable in crate::runtime::shell_process::SAFE_ENV_VARS {
        if *variable != "PATH"
            && let Ok(value) = std::env::var(variable)
        {
            cmd.env(variable, value);
        }
    }
    cmd.env("PATH", NODE_SAFE_PATH);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = env {
        cmd.envs(env);
    }

    let mut child = match crate::runtime::shell_process::spawn_managed_shell_child(cmd) {
        Ok(child) => child,
        Err(error) => {
            return ExecShellResult {
                task_id: task_id.to_string(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
                duration_ms: elapsed_ms_u64(started.elapsed()),
                timed_out: false,
                cancelled: false,
            };
        }
    };

    let output_budget = Arc::new(AtomicUsize::new(state.config.max_output_bytes.max(1)));
    let output_truncated = Arc::new(AtomicBool::new(false));
    let stdout_reader = child.take_stdout().map(|stdout| {
        tokio::spawn(drain_bounded_output(
            stdout,
            Arc::clone(&output_budget),
            Arc::clone(&output_truncated),
        ))
    });
    let stderr_reader = child.take_stderr().map(|stderr| {
        tokio::spawn(drain_bounded_output(
            stderr,
            Arc::clone(&output_budget),
            Arc::clone(&output_truncated),
        ))
    });

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let (exit_code, timed_out, cancelled, lifecycle_error) = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => (status.code(), false, false, None),
            Err(error) => (None, false, false, Some(error.to_string())),
        },
        _ = &mut deadline => {
            let reaped = child.terminate_and_reap().await;
            let error = (!reaped).then(|| "failed to reap command process tree".to_string());
            (None, true, false, error.or_else(|| Some("command execution timed out".to_string())))
        },
        _ = cancellation.cancelled() => {
            let reaped = child.terminate_and_reap().await;
            let error = (!reaped).then(|| "failed to reap command process tree".to_string());
            (None, false, true, error.or_else(|| Some("command cancelled".to_string())))
        }
    };

    let ((stdout, stdout_incomplete), (mut stderr, stderr_incomplete)) = tokio::join!(
        collect_bounded_output(stdout_reader),
        collect_bounded_output(stderr_reader)
    );
    if let Some(error) = lifecycle_error {
        append_diagnostic(&mut stderr, &error);
    }
    if output_truncated.load(Ordering::Relaxed) {
        append_diagnostic(&mut stderr, "[output truncated by node max_output_bytes]");
    }
    if stdout_incomplete || stderr_incomplete {
        let _ = child.terminate_and_reap().await;
        append_diagnostic(&mut stderr, "[output drain stopped after bounded grace period]");
    }
    child.mark_complete();

    ExecShellResult {
        task_id: task_id.to_string(),
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        duration_ms: elapsed_ms_u64(started.elapsed()),
        timed_out,
        cancelled,
    }
}

fn validate_node_environment(env: Option<HashMap<String, String>>) -> Result<Option<HashMap<String, String>>> {
    let Some(env) = env else {
        return Ok(None);
    };
    for key in env.keys() {
        if !NODE_CALLER_ENV_ALLOWLIST.contains(&key.as_str()) {
            bail!("node environment variable is not allowed: {key}");
        }
    }
    Ok(Some(env))
}

async fn drain_bounded_output<R>(
    mut reader: R,
    budget: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let available = budget
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(read))
            })
            .unwrap_or(0);
        let keep = available.min(read);
        retained.extend_from_slice(chunk.get(..keep).unwrap_or_default());
        if keep < read {
            truncated.store(true, Ordering::Relaxed);
        }
    }
    Ok(retained)
}

async fn collect_bounded_output(reader: Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>) -> (Vec<u8>, bool) {
    match reader {
        Some(mut reader) => match tokio::time::timeout(OUTPUT_DRAIN_GRACE, &mut reader).await {
            Ok(result) => (result.ok().and_then(Result::ok).unwrap_or_default(), false),
            Err(_) => {
                reader.abort();
                let _ = reader.await;
                (Vec::new(), true)
            }
        },
        None => (Vec::new(), false),
    }
}

fn append_diagnostic(output: &mut Vec<u8>, diagnostic: &str) {
    if !output.is_empty() {
        output.push(b'\n');
    }
    output.extend_from_slice(diagnostic.as_bytes());
}

fn log_exec_shell_event(command: &ParsedCommand, result: &ExecShellResult, source_ip: &str) {
    tracing::info!(
        target: "security_audit",
        event = "node.exec_shell",
        source_ip = source_ip,
        command = %command.program,
        arg_count = command.args.len(),
        exit_code = ?result.exit_code,
        duration_ms = result.duration_ms,
        timed_out = result.timed_out,
        cancelled = result.cancelled
    );
}

async fn register_task(state: &AppState, task_id: &str, cancellation: CancellationToken) {
    {
        let mut running_tasks = state.running_tasks.write().await;
        running_tasks.insert(task_id.to_string(), cancellation);
    }

    let mut task_results = state.task_results.write().await;
    task_results.insert(task_id.to_string(), TaskResult::running(task_id.to_string()));
}

async fn finalize_task(state: AppState, result: &ExecShellResult) -> Value {
    let now = Instant::now();
    {
        let mut running_tasks = state.running_tasks.write().await;
        running_tasks.remove(&result.task_id);
    }

    let status_result = {
        let mut task_results = state.task_results.write().await;
        let entry = task_results
            .entry(result.task_id.clone())
            .or_insert_with(|| TaskResult::running(result.task_id.clone()));
        entry.apply_exec_result(result, now);
        entry.to_status_result(now)
    };

    cleanup_expired_task_results(&state).await;
    serde_json::to_value(status_result).unwrap_or_else(|_| json!({ "task_id": result.task_id }))
}

async fn cleanup_expired_task_results(state: &AppState) {
    let ttl = Duration::from_millis(state.config.task_result_ttl_ms.max(1));
    let now = Instant::now();
    let mut task_results = state.task_results.write().await;
    task_results.retain(|_, task| {
        if task.status == TaskState::Running {
            return true;
        }
        task.completed_at
            .map(|completed_at| now.saturating_duration_since(completed_at) <= ttl)
            .unwrap_or(false)
    });
}

async fn handle_task_status(state: &AppState, params: TaskStatusParams) -> Result<TaskStatusResult> {
    cleanup_expired_task_results(state).await;
    let task_results = state.task_results.read().await;
    let now = Instant::now();
    let Some(task) = task_results.get(&params.task_id) else {
        bail!("task not found")
    };
    Ok(task.to_status_result(now))
}

async fn handle_task_list(state: &AppState) -> Result<TaskListResult> {
    cleanup_expired_task_results(state).await;
    let task_results = state.task_results.read().await;
    let now = Instant::now();
    let tasks = task_results
        .values()
        .map(|task| task.to_list_item(now))
        .collect::<Vec<_>>();
    Ok(TaskListResult { tasks })
}

fn validate_callback_url(callback_url: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = callback_url else {
        return Ok(None);
    };
    let value = raw.trim();
    if value.is_empty() {
        bail!("callback_url cannot be empty");
    }
    let url = reqwest::Url::parse(value).context("invalid callback_url")?;
    if url.scheme() != "https" {
        bail!("callback_url must use https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("callback_url must not contain credentials");
    }
    let host = url.host_str().ok_or_else(|| anyhow!("callback_url missing host"))?;
    if callback_host_is_statically_blocked(host) {
        bail!("callback_url resolves to a private or local address");
    }
    Ok(Some(url.to_string()))
}

fn callback_host_is_statically_blocked(host: &str) -> bool {
    let normalized = host.trim_matches(['[', ']']).to_ascii_lowercase();
    normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
        || normalized
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| crate::tools::http_request::is_private_or_local_host(&ip.to_string()))
}

async fn send_callback(callback_url: String, payload: Value) {
    let callback_host = callback_log_host(&callback_url);
    let target = match prepare_callback_target(&callback_url).await {
        Ok(target) => target,
        Err(error) => {
            tracing::warn!(callback_host = %callback_host, error = %error, "node callback blocked by SSRF policy");
            return;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(target.host_str().unwrap_or_default(), &target.addrs)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(callback_host = %callback_host, error = %error, "failed to build node callback client");
            return;
        }
    };
    let response = client.post(target.url).json(&payload).send().await;

    match response {
        Ok(response) => {
            if !response.status().is_success() {
                tracing::warn!(callback_host = %callback_host, status = %response.status(), "node callback failed");
            }
        }
        Err(error) => {
            tracing::warn!(callback_host = %callback_host, error = %error, "node callback failed");
        }
    }
}

fn callback_log_host(raw: &str) -> String {
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "invalid".to_string())
}

struct CallbackTarget {
    url: reqwest::Url,
    addrs: Vec<SocketAddr>,
}

impl CallbackTarget {
    fn host_str(&self) -> Option<&str> {
        self.url.host_str()
    }
}

async fn prepare_callback_target(raw: &str) -> Result<CallbackTarget> {
    let validated = validate_callback_url(Some(raw))?.ok_or_else(|| anyhow!("callback_url missing"))?;
    let url = reqwest::Url::parse(&validated).context("invalid callback_url")?;
    let host = url.host_str().ok_or_else(|| anyhow!("callback_url missing host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("callback_url missing port"))?;
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .context("callback_url DNS resolution failed")?
        .collect::<Vec<_>>();
    validate_callback_addrs(&addrs)?;
    Ok(CallbackTarget { url, addrs })
}

fn validate_callback_addrs(addrs: &[SocketAddr]) -> Result<()> {
    if addrs.is_empty() {
        bail!("callback_url resolved to no addresses");
    }
    if addrs
        .iter()
        .any(|addr| crate::tools::http_request::is_private_or_local_host(&addr.ip().to_string()))
    {
        bail!("callback_url resolves to a private or local address");
    }
    Ok(())
}

async fn handle_read_file(state: &AppState, params: ReadFileParams) -> Result<ReadFileResult> {
    let sandbox = Arc::clone(&state.sandbox_root);
    let requested_path = params.path;
    let offset = params.offset.unwrap_or(0);
    let configured_limit = u64::try_from(state.config.max_output_bytes.max(1)).unwrap_or(u64::MAX);
    let limit = params.limit.unwrap_or(64 * 1024).min(configured_limit).max(1);
    tokio::task::spawn_blocking(move || read_sandbox_file(&sandbox, &requested_path, offset, limit))
        .await
        .context("node file read task failed")?
}

async fn handle_write_file(state: &AppState, params: WriteFileParams) -> Result<WriteFileResult> {
    if params.content.len() > state.config.max_output_bytes.max(1) {
        bail!("write content exceeds node max_output_bytes");
    }
    let sandbox = Arc::clone(&state.sandbox_root);
    tokio::task::spawn_blocking(move || {
        write_sandbox_file(&sandbox, &params.path, params.content.as_bytes(), params.create_dirs)
    })
    .await
    .context("node file write task failed")?
}

async fn handle_cancel(state: &AppState, params: CancelParams, source_ip: &str) -> Result<()> {
    let tasks = state.running_tasks.read().await;
    let Some(token) = tasks.get(&params.task_id) else {
        bail!("task not found")
    };
    token.cancel();
    tracing::info!(
        target: "security_audit",
        event = "node.cancel",
        source_ip = source_ip,
        task_id = %params.task_id
    );
    Ok(())
}

fn validate_auth(headers: &HeaderMap, config: &NodeServerConfig) -> Result<()> {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing Authorization header"))?;

    let provided = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| anyhow!("Authorization must use Bearer token"))?;

    if !constant_time_eq(provided.as_bytes(), config.bearer_token.as_bytes()) {
        bail!("invalid bearer token");
    }

    Ok(())
}

fn validate_hmac(headers: &HeaderMap, body: &str, config: &NodeServerConfig) -> Result<()> {
    let Some(secret) = config.hmac_secret.as_deref() else {
        return Ok(());
    };

    let timestamp_raw = headers
        .get("x-openprx-timestamp")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing X-OpenPRX-Timestamp"))?;

    let timestamp = timestamp_raw.parse::<i64>().context("invalid X-OpenPRX-Timestamp")?;

    let now = Utc::now().timestamp();
    if (now - timestamp).abs() > HMAC_FRESHNESS_WINDOW_SECS {
        bail!("stale request timestamp");
    }

    let signature = headers
        .get("x-openprx-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing X-OpenPRX-Signature"))?;

    let payload = format!("{timestamp}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid hmac key length"))?;
    mac.update(payload.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());

    if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        bail!("invalid hmac signature");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    None,
    Single,
    Double,
}

#[derive(Debug, Clone)]
struct ParsedCommand {
    program: String,
    args: Vec<String>,
}

fn parse_command(command: &str) -> Result<ParsedCommand> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = QuoteState::None;
    let mut escaped = false;

    for ch in command.chars() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                } else {
                    current.push(ch);
                }
            }
            QuoteState::Double => {
                if escaped {
                    current.push(ch);
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => escaped = true,
                    '"' => quote = QuoteState::None,
                    _ => current.push(ch),
                }
            }
            QuoteState::None => {
                if escaped {
                    current.push(ch);
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => escaped = true,
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    c if c.is_whitespace() => {
                        if !current.is_empty() {
                            tokens.push(std::mem::take(&mut current));
                        }
                    }
                    '|' | '&' | ';' | '<' | '>' | '`' => {
                        bail!("shell operators are not allowed in node.exec_shell");
                    }
                    _ => current.push(ch),
                }
            }
        }
    }

    if escaped {
        bail!("trailing escape in command");
    }
    if quote != QuoteState::None {
        bail!("unterminated quote in command");
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    if tokens.is_empty() {
        bail!("cmd cannot be empty");
    }

    // SAFETY: tokens is non-empty (checked above)
    #[allow(clippy::indexing_slicing)]
    let program = tokens[0].clone();
    let args = tokens.into_iter().skip(1).collect::<Vec<_>>();
    Ok(ParsedCommand { program, args })
}

fn command_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program)
        .to_ascii_lowercase()
}

fn validate_command(command: &ParsedCommand, allowed: &[String], blocked: &[String]) -> Result<()> {
    let name = command_name(&command.program);

    if blocked.iter().any(|item| item.eq_ignore_ascii_case(&name)) {
        bail!("command blocked by policy: {name}");
    }

    let allow_any = allowed.is_empty() || allowed.iter().any(|item| item == "*");
    if allow_any {
        return Ok(());
    }

    if allowed.iter().any(|item| item.eq_ignore_ascii_case(&name)) {
        Ok(())
    } else {
        bail!("command not allowed by policy: {name}");
    }
}

fn elapsed_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn bind_host(bind: &str) -> &str {
    if let Some(rest) = bind.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once("]:") {
            return host;
        }
    }
    bind.rsplit_once(':').map_or(bind, |(host, _)| host)
}

fn is_loopback_bind(bind: &str) -> bool {
    let host = bind_host(bind).trim().to_ascii_lowercase();
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn validate_tls_requirements(config: &NodeServerConfig) -> Result<()> {
    let cert_ok = config
        .tls_cert
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let key_ok = config
        .tls_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());

    if cert_ok != key_ok {
        bail!("node server TLS requires both nodes.server.tls_cert and nodes.server.tls_key");
    }

    if cert_ok && key_ok {
        return Ok(());
    }

    if !config.tls_required || is_loopback_bind(&config.listen_addr) {
        return Ok(());
    }

    bail!(
        "node server refuses non-loopback bind without TLS material: set nodes.server.tls_cert and nodes.server.tls_key, or bind to 127.0.0.1/localhost"
    );
}

fn configured_tls_paths(config: &NodeServerConfig) -> Result<Option<(PathBuf, PathBuf)>> {
    let cert = config
        .tls_cert
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let key = config
        .tls_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (cert, key) {
        (Some(cert), Some(key)) => Ok(Some((PathBuf::from(cert), PathBuf::from(key)))),
        (None, None) => Ok(None),
        _ => bail!("node server TLS requires both nodes.server.tls_cert and nodes.server.tls_key"),
    }
}

async fn load_tls_config(config: &NodeServerConfig) -> Result<Option<RustlsConfig>> {
    let Some((cert, key)) = configured_tls_paths(config)? else {
        return Ok(None);
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    match RustlsConfig::from_pem_file(&cert, &key).await {
        Ok(tls_config) => Ok(Some(tls_config)),
        Err(error) => {
            tracing::error!(error = %error, cert = %cert.display(), key = %key.display(), "TLS configured but cert/key load failed");
            Err(error).with_context(|| {
                format!(
                    "TLS configured but cert/key load failed: cert={}, key={}",
                    cert.display(),
                    key.display()
                )
            })
        }
    }
}

fn parse_listen_addr(listen_addr: &str) -> Result<SocketAddr> {
    listen_addr
        .parse::<SocketAddr>()
        .with_context(|| format!("failed to parse listen address {listen_addr}"))
}

fn prepare_sandbox_root(path: &str) -> Result<SandboxRoot> {
    let root = PathBuf::from(path);
    std::fs::create_dir_all(&root).with_context(|| format!("failed to create sandbox root {}", root.display()))?;
    let path = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize sandbox root {}", root.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let directory = options
            .open(&path)
            .with_context(|| format!("failed to open sandbox root {}", path.display()))?;
        Ok(SandboxRoot {
            path,
            directory: Arc::new(directory),
        })
    }

    #[cfg(not(unix))]
    {
        Ok(SandboxRoot { path })
    }
}

fn read_sandbox_file(sandbox: &SandboxRoot, requested_path: &str, offset: u64, limit: u64) -> Result<ReadFileResult> {
    #[cfg(unix)]
    {
        use std::io::{Read, Seek};

        let (mut file, display_path) = open_sandbox_file_for_read(sandbox, requested_path)?;
        let metadata = file.metadata().context("failed to inspect sandbox file")?;
        if !metadata.is_file() {
            bail!("sandbox read target must be a regular file");
        }
        if offset > metadata.len() {
            bail!("offset out of range");
        }
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        let eof = u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limit;
        if !eof {
            bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        return Ok(ReadFileResult {
            path: display_path.to_string_lossy().to_string(),
            content: String::from_utf8_lossy(&bytes).to_string(),
            bytes_read: bytes.len(),
            offset,
            eof,
        });
    }

    #[cfg(not(unix))]
    {
        let _ = (sandbox, requested_path, offset, limit);
        bail!("path-safe node file reads are unsupported on this platform")
    }
}

fn write_sandbox_file(
    sandbox: &SandboxRoot,
    requested_path: &str,
    content: &[u8],
    create_dirs: bool,
) -> Result<WriteFileResult> {
    #[cfg(unix)]
    {
        use std::io::Write;

        let (mut file, display_path, created_dirs) = open_sandbox_file_for_write(sandbox, requested_path, create_dirs)?;
        let metadata = file.metadata().context("failed to inspect sandbox file")?;
        if !metadata.is_file() {
            bail!("sandbox write target must be a regular file");
        }
        file.write_all(content)?;
        file.flush()?;
        return Ok(WriteFileResult {
            path: display_path.to_string_lossy().to_string(),
            bytes_written: content.len(),
            created_dirs,
        });
    }

    #[cfg(not(unix))]
    {
        let _ = (sandbox, requested_path, content, create_dirs);
        bail!("path-safe node file writes are unsupported on this platform")
    }
}

#[cfg(unix)]
fn open_sandbox_file_for_read(sandbox: &SandboxRoot, requested_path: &str) -> Result<(std::fs::File, PathBuf)> {
    let (components, relative_path) = sandbox_relative_components(&sandbox.path, requested_path)?;
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| anyhow!("sandbox path must name a file"))?;
    let (parent, _) = open_sandbox_parent(sandbox, parents, false)?;
    let file = openat_component(
        &parent,
        file_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .with_context(|| format!("failed to open sandbox file {}", relative_path.display()))?;
    Ok((file, sandbox.path.join(relative_path)))
}

#[cfg(unix)]
fn open_sandbox_file_for_write(
    sandbox: &SandboxRoot,
    requested_path: &str,
    create_dirs: bool,
) -> Result<(std::fs::File, PathBuf, bool)> {
    let (components, relative_path) = sandbox_relative_components(&sandbox.path, requested_path)?;
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| anyhow!("sandbox path must name a file"))?;
    let (parent, created_dirs) = open_sandbox_parent(sandbox, parents, create_dirs)?;
    let file = openat_component(
        &parent,
        file_name,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::TRUNC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .with_context(|| format!("failed to open sandbox file {}", relative_path.display()))?;
    Ok((file, sandbox.path.join(relative_path), created_dirs))
}

#[cfg(unix)]
fn sandbox_relative_components(root: &Path, requested_path: &str) -> Result<(Vec<std::ffi::OsString>, PathBuf)> {
    let requested = Path::new(requested_path);
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(root)
            .map_err(|_| anyhow!("path escapes sandbox root"))?
    } else {
        requested
    };
    let mut components = Vec::new();
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                components.push(value.to_os_string());
                normalized.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => bail!("path escapes sandbox root"),
        }
    }
    if components.is_empty() {
        bail!("sandbox path must name a file");
    }
    Ok((components, normalized))
}

#[cfg(unix)]
fn open_sandbox_parent(
    sandbox: &SandboxRoot,
    parents: &[std::ffi::OsString],
    create_dirs: bool,
) -> Result<(std::fs::File, bool)> {
    let mut current = sandbox.directory.try_clone()?;
    let mut created_any = false;
    for component in parents {
        match openat_component(
            &current,
            component,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        ) {
            Ok(next) => current = next,
            Err(error) if create_dirs && error.raw_os_error() == Some(libc::ENOENT) => {
                mkdirat_component(&current, component, rustix::fs::Mode::RWXU)?;
                created_any = true;
                current = openat_component(
                    &current,
                    component,
                    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok((current, created_any))
}

#[cfg(unix)]
fn openat_component(
    parent: &std::fs::File,
    component: &OsStr,
    flags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> std::io::Result<std::fs::File> {
    rustix::fs::openat(
        parent,
        component,
        flags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        mode,
    )
    .map(std::fs::File::from)
    .map_err(std::io::Error::from)
}

#[cfg(unix)]
fn mkdirat_component(parent: &std::fs::File, component: &OsStr, mode: rustix::fs::Mode) -> std::io::Result<()> {
    rustix::fs::mkdirat(parent, component, mode).map_err(std::io::Error::from)
}

fn resolve_requested_path(sandbox_root: &Path, requested_path: &str) -> Result<PathBuf> {
    let requested = PathBuf::from(requested_path);
    let candidate = if requested.is_absolute() {
        requested
    } else {
        sandbox_root.join(requested)
    };

    let normalized = normalize_path(&candidate);
    if !normalized.starts_with(sandbox_root) {
        bail!("path escapes sandbox root");
    }

    Ok(normalized)
}

fn ensure_within_sandbox_root(sandbox_root: &Path, path: &Path) -> Result<()> {
    if path.starts_with(sandbox_root) {
        Ok(())
    } else {
        bail!("path escapes sandbox root")
    }
}

fn canonicalize_existing_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn resolve_existing_sandbox_path(sandbox_root: &Path, requested_path: &str) -> Result<PathBuf> {
    let normalized = resolve_requested_path(sandbox_root, requested_path)?;
    let canonical = canonicalize_existing_path(&normalized)?;
    ensure_within_sandbox_root(sandbox_root, &canonical)?;
    Ok(canonical)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn read_metrics() -> Result<MetricsResult> {
    Ok(MetricsResult {
        timestamp: Utc::now(),
        cpu_cores: std::thread::available_parallelism().map_or(1, |value| value.get()),
        load_avg_1m: read_load_avg_1m(),
        mem_total_kb: read_meminfo_value("MemTotal"),
        mem_available_kb: read_meminfo_value("MemAvailable"),
        uptime_seconds: read_uptime_seconds(),
    })
}

fn read_load_avg_1m() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse::<f64>().ok()
}

fn read_uptime_seconds() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime = text.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(uptime as u64)
}

fn read_meminfo_value(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn test_state(temp: &TempDir) -> AppState {
        let mut config = NodeServerConfig::default();
        config.sandbox_root = temp.path().to_string_lossy().to_string();
        config.allowed_commands = vec!["sh".to_string()];
        config.max_output_bytes = 64;
        config.exec_timeout_ms = 5_000;
        let max_concurrent_tasks = config.max_concurrent_tasks;
        let sandbox_root = prepare_sandbox_root(&config.sandbox_root).unwrap();
        AppState {
            config: Arc::new(config),
            sandbox_root: Arc::new(sandbox_root),
            running_tasks: Arc::new(RwLock::new(HashMap::new())),
            task_results: Arc::new(RwLock::new(HashMap::new())),
            mutation_replays: Arc::new(Mutex::new(HashMap::new())),
            async_task_slots: Arc::new(Semaphore::new(max_concurrent_tasks)),
        }
    }

    #[test]
    fn sandbox_prevents_escape() {
        let temp = TempDir::new().unwrap();
        let root = prepare_sandbox_root(temp.path().to_string_lossy().as_ref()).unwrap();

        let escaped = read_sandbox_file(&root, "../etc/passwd", 0, 1);
        assert!(escaped.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_read_rejects_symlink_escape() {
        let sandbox = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = prepare_sandbox_root(sandbox.path().to_string_lossy().as_ref()).unwrap();

        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "outside").unwrap();
        symlink(outside.path(), root.path.join("link-out")).unwrap();

        let escaped = read_sandbox_file(&root, "link-out/secret.txt", 0, 64);
        assert!(escaped.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_write_rejects_symlink_escape() {
        let sandbox = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = prepare_sandbox_root(sandbox.path().to_string_lossy().as_ref()).unwrap();

        symlink(outside.path(), root.path.join("link-out")).unwrap();

        let escaped = write_sandbox_file(&root, "link-out/new.txt", b"blocked", true);
        assert!(escaped.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_file_io_uses_bounded_descriptor_relative_access() {
        let sandbox = TempDir::new().unwrap();
        let root = prepare_sandbox_root(sandbox.path().to_string_lossy().as_ref()).unwrap();
        let written = write_sandbox_file(&root, "nested/data.txt", b"0123456789", true).unwrap();
        assert_eq!(written.bytes_written, 10);
        assert!(written.created_dirs);

        let first = read_sandbox_file(&root, "nested/data.txt", 2, 4).unwrap();
        assert_eq!(first.content, "2345");
        assert_eq!(first.bytes_read, 4);
        assert!(!first.eof);

        let tail = read_sandbox_file(&root, "nested/data.txt", 8, 4).unwrap();
        assert_eq!(tail.content, "89");
        assert!(tail.eof);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_output_is_drained_while_retention_stays_bounded() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let command = parse_command("sh -c 'yes x | head -c 100000'").unwrap();
        let result = execute_shell_command(
            &state,
            &command,
            Some(state.sandbox_root.path.clone()),
            None,
            Duration::from_secs(5),
            "bounded-output",
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.len() <= state.config.max_output_bytes);
        assert!(result.stderr.contains("output truncated"));
    }

    #[test]
    fn exec_shell_rejects_runtime_loader_environment() {
        let mut env = HashMap::new();
        env.insert("BASH_ENV".to_string(), "/tmp/attacker-profile".to_string());

        let error = validate_node_environment(Some(env)).unwrap_err();

        assert!(error.to_string().contains("BASH_ENV"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_async_exec_never_exceeds_configured_cap() {
        let temp = TempDir::new().unwrap();
        let mut state = test_state(&temp);
        Arc::make_mut(&mut state.config).max_concurrent_tasks = 1;
        state.async_task_slots = Arc::new(Semaphore::new(1));
        let params = || ExecShellParams {
            cmd: "sh -c 'sleep 30'".to_string(),
            timeout_ms: None,
            cwd: Some(state.sandbox_root.path.to_string_lossy().to_string()),
            env: None,
            async_exec: Some(true),
            callback_url: None,
        };

        let (first, second) = tokio::join!(
            handle_exec_shell(&state, params(), "127.0.0.1"),
            handle_exec_shell(&state, params(), "127.0.0.1")
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let rejected = first.err().or_else(|| second.err()).unwrap();
        assert!(rejected.to_string().contains("max concurrent async tasks"));

        for cancellation in state.running_tasks.read().await.values() {
            cancellation.cancel();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.async_task_slots.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(handle_exec_shell(&state, params(), "127.0.0.1").await.is_ok());
        for cancellation in state.running_tasks.read().await.values() {
            cancellation.cancel();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_descendant_process_group() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let cancellation = CancellationToken::new();
        let cancellation_for_task = cancellation.clone();
        let state_for_task = state.clone();
        let task = tokio::spawn(async move {
            let command = parse_command("sh -c 'sleep 30 & echo $! > descendant.pid; wait'").unwrap();
            execute_shell_command(
                &state_for_task,
                &command,
                Some(state_for_task.sandbox_root.path.clone()),
                None,
                Duration::from_secs(30),
                "process-tree",
                cancellation_for_task,
            )
            .await
        });
        let pid_path = state.sandbox_root.path.join("descendant.pid");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let descendant_pid = std::fs::read_to_string(&pid_path).unwrap();
        let descendant_pid = descendant_pid.trim();

        cancellation.cancel();
        let result = task.await.unwrap();

        assert!(result.cancelled);
        tokio::time::timeout(Duration::from_secs(5), async {
            while Path::new("/proc").join(descendant_pid).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mutation_request_id_replays_result_without_reexecuting() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let request = JsonRpcRequest::new(
            "stable-mutation-id".to_string(),
            "node.exec_shell",
            json!({
                "cmd": "sh -c 'printf x >> mutation-marker'",
                "cwd": state.sandbox_root.path.to_string_lossy().to_string(),
            }),
        );

        let first = dispatch_idempotent_mutation(&state, request.clone(), "127.0.0.1")
            .await
            .unwrap();
        let replay = dispatch_idempotent_mutation(&state, request, "127.0.0.1")
            .await
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("mutation-marker")).unwrap(),
            "x"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_mutation_retries_single_flight_one_execution() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let request = JsonRpcRequest::new(
            "concurrent-mutation-id".to_string(),
            "node.exec_shell",
            json!({
                "cmd": "sh -c 'printf x >> concurrent-marker'",
                "cwd": state.sandbox_root.path.to_string_lossy().to_string(),
            }),
        );

        let (first, retry) = tokio::join!(
            dispatch_idempotent_mutation(&state, request.clone(), "127.0.0.1"),
            dispatch_idempotent_mutation(&state, request, "127.0.0.1")
        );
        assert_eq!(first.unwrap(), retry.unwrap());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("concurrent-marker")).unwrap(),
            "x"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mutation_request_id_cannot_be_rebound_to_different_params() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let first = JsonRpcRequest::new(
            "reused-id".to_string(),
            "node.write_file",
            json!({"path": "one.txt", "content": "one"}),
        );
        dispatch_idempotent_mutation(&state, first, "127.0.0.1").await.unwrap();
        let changed = JsonRpcRequest::new(
            "reused-id".to_string(),
            "node.write_file",
            json!({"path": "two.txt", "content": "two"}),
        );
        let error = dispatch_idempotent_mutation(&state, changed, "127.0.0.1")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reused with different"));
        assert!(!temp.path().join("two.txt").exists());
    }

    #[tokio::test]
    async fn mutation_request_id_must_be_nonempty_and_bounded() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        for id in [String::new(), "x".repeat(129)] {
            let request = JsonRpcRequest::new(
                id,
                "node.write_file",
                json!({"path": "blocked.txt", "content": "blocked"}),
            );
            let error = dispatch_idempotent_mutation(&state, request, "127.0.0.1")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("1 to 128 bytes"));
        }
        assert!(!temp.path().join("blocked.txt").exists());
    }

    #[tokio::test]
    async fn write_payload_is_rejected_before_open_when_over_bound() {
        let temp = TempDir::new().unwrap();
        let mut state = test_state(&temp);
        Arc::make_mut(&mut state.config).max_output_bytes = 4;
        let error = handle_write_file(
            &state,
            WriteFileParams {
                path: "too-large.txt".to_string(),
                content: "12345".to_string(),
                create_dirs: false,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds node max_output_bytes"));
        assert!(!temp.path().join("too-large.txt").exists());
    }

    #[test]
    fn callback_policy_requires_public_https_without_credentials() {
        assert!(validate_callback_url(Some("https://example.com/node-callback")).is_ok());
        assert!(validate_callback_url(Some("http://example.com/node-callback")).is_err());
        assert!(validate_callback_url(Some("https://127.0.0.1/node-callback")).is_err());
        assert!(validate_callback_url(Some("https://user:pass@example.com/node-callback")).is_err());
    }

    #[test]
    fn callback_policy_rejects_any_private_dns_answer() {
        let mixed = [
            "93.184.216.34:443".parse::<SocketAddr>().unwrap(),
            "169.254.169.254:443".parse::<SocketAddr>().unwrap(),
        ];
        assert!(validate_callback_addrs(&mixed).is_err());
        let public = ["93.184.216.34:443".parse::<SocketAddr>().unwrap()];
        assert!(validate_callback_addrs(&public).is_ok());
    }

    #[test]
    fn command_blocklist_rejects_first_token() {
        let blocked = vec!["rm".to_string()];
        let parsed = parse_command("rm -rf /").unwrap();
        let result = validate_command(&parsed, &["echo".to_string()], &blocked);
        assert!(result.is_err());
    }

    #[test]
    fn command_allowlist_blocks_unknown_command() {
        let parsed = parse_command("ls -la").unwrap();
        let result = validate_command(&parsed, &["echo".to_string()], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn command_allow_all_accepts_when_star_present() {
        let parsed = parse_command("python3 -V").unwrap();
        let result = validate_command(&parsed, &["*".to_string()], &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_command_rejects_shell_operators() {
        let parsed = parse_command("echo hi && id");
        assert!(parsed.is_err());
    }

    #[test]
    fn tls_required_rejects_non_loopback_without_certs() {
        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "0.0.0.0:8787".to_string();
        cfg.tls_required = true;
        cfg.tls_cert = None;
        cfg.tls_key = None;
        assert!(validate_tls_requirements(&cfg).is_err());
    }

    #[test]
    fn tls_required_allows_loopback_without_certs() {
        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "127.0.0.1:8787".to_string();
        cfg.tls_required = true;
        cfg.tls_cert = None;
        cfg.tls_key = None;
        assert!(validate_tls_requirements(&cfg).is_ok());
    }

    #[test]
    fn tls_partial_material_fails_validation_even_on_loopback() {
        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "127.0.0.1:8787".to_string();
        cfg.tls_required = false;
        cfg.tls_cert = Some("cert.pem".to_string());
        cfg.tls_key = None;
        assert!(validate_tls_requirements(&cfg).is_err());
    }

    #[tokio::test]
    async fn loopback_allows_tcp_without_tls() {
        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "127.0.0.1:8787".to_string();
        cfg.tls_required = true;
        cfg.tls_cert = None;
        cfg.tls_key = None;

        assert!(validate_tls_requirements(&cfg).is_ok());
        assert!(
            load_tls_config(&cfg)
                .await
                .expect("no tls config should be ok")
                .is_none()
        );
    }

    #[tokio::test]
    async fn tls_load_failure_aborts_startup() {
        let temp = tempfile::tempdir().unwrap();
        let cert_path = temp.path().join("bad-cert.pem");
        let key_path = temp.path().join("bad-key.pem");
        std::fs::write(&cert_path, "not a certificate").unwrap();
        std::fs::write(&key_path, "not a private key").unwrap();

        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "127.0.0.1:8787".to_string();
        cfg.tls_required = true;
        cfg.tls_cert = Some(cert_path.display().to_string());
        cfg.tls_key = Some(key_path.display().to_string());

        let err = load_tls_config(&cfg)
            .await
            .expect_err("corrupt TLS material must fail closed");
        assert!(
            err.to_string().contains("TLS configured but cert/key load failed"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn tls_config_loads_pem_file_pair() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cert_path = temp.path().join("cert.pem");
        let key_path = temp.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        let mut cfg = NodeServerConfig::default();
        cfg.listen_addr = "127.0.0.1:8787".to_string();
        cfg.tls_required = true;
        cfg.tls_cert = Some(cert_path.display().to_string());
        cfg.tls_key = Some(key_path.display().to_string());

        assert!(
            load_tls_config(&cfg)
                .await
                .expect("valid TLS material should load")
                .is_some()
        );
    }
}

use crate::config::NodeServerConfig;
use crate::nodes::protocol::{
    CancelParams, ExecShellParams, ExecShellResult, JsonRpcRequest, JsonRpcResponse, MetricsResult,
    PingResult, ReadFileParams, ReadFileResult, WriteFileParams, WriteFileResult,
};
use anyhow::{anyhow, bail, Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, routing::post, Json, Router};
use chrono::Utc;
use constant_time_eq::constant_time_eq;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    config: Arc<NodeServerConfig>,
    sandbox_root: Arc<PathBuf>,
    tasks: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

pub async fn run_node_server(config: NodeServerConfig) -> Result<()> {
    validate_tls_requirements(&config)?;
    let sandbox_root = prepare_sandbox_root(&config.sandbox_root)?;
    let state = AppState {
        config: Arc::new(config.clone()),
        sandbox_root: Arc::new(sandbox_root),
        tasks: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/health", get(handle_health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    tracing::info!(
        "prx-node listening on {}, sandbox_root={}",
        config.listen_addr,
        config.sandbox_root
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("node server exited with error")
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

async fn handle_rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    if let Err(error) = validate_auth(&headers, &state.config) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::failure(
                "0".into(),
                -32001,
                error.to_string(),
            )),
        );
    }

    if let Err(error) = validate_hmac(&headers, &body, &state.config) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::failure(
                "0".into(),
                -32002,
                error.to_string(),
            )),
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
            )
        }
    };

    let id = request.id.clone();
    let result = dispatch_rpc(&state, request).await;

    match result {
        Ok(payload) => (StatusCode::OK, Json(JsonRpcResponse::success(id, payload))),
        Err(error) => (
            StatusCode::OK,
            Json(JsonRpcResponse::failure(id, -32000, error.to_string())),
        ),
    }
}

async fn dispatch_rpc(state: &AppState, request: JsonRpcRequest) -> Result<Value> {
    match request.method.as_str() {
        "node.ping" => Ok(serde_json::to_value(PingResult {
            message: "pong".into(),
            timestamp: Utc::now(),
        })?),
        "node.exec_shell" => {
            let params: ExecShellParams = serde_json::from_value(request.params)?;
            let output = handle_exec_shell(state, params).await?;
            Ok(serde_json::to_value(output)?)
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
            handle_cancel(state, params).await?;
            Ok(json!({"ok": true}))
        }
        "node.metrics" => Ok(serde_json::to_value(read_metrics()?)?),
        method => bail!("unsupported method: {method}"),
    }
}

async fn handle_exec_shell(state: &AppState, params: ExecShellParams) -> Result<ExecShellResult> {
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
        Some(resolve_existing_sandbox_path(&state.sandbox_root, cwd)?)
    } else {
        None
    };

    let timeout_ms = params.timeout_ms.unwrap_or(state.config.exec_timeout_ms);
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let task_id = uuid::Uuid::new_v4().to_string();
    let cancellation = CancellationToken::new();

    {
        let mut tasks = state.tasks.write().await;
        tasks.insert(task_id.clone(), cancellation.clone());
    }

    let started = Instant::now();
    let mut cmd = Command::new(&parsed_command.program);
    cmd.args(&parsed_command.args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let child = cmd.output();

    let mut result = tokio::select! {
        output = tokio::time::timeout(timeout, child) => {
            match output {
                Ok(Ok(output)) => {
                    let (stdout, mut stderr, truncated) = cap_output(
                        &output.stdout,
                        &output.stderr,
                        state.config.max_output_bytes.max(1),
                    );
                    if truncated {
                        if stderr.is_empty() {
                            stderr = "[output truncated by node max_output_bytes]".to_string();
                        } else {
                            stderr.push_str("\n[output truncated by node max_output_bytes]");
                        }
                    }
                    ExecShellResult {
                    task_id: task_id.clone(),
                    exit_code: output.status.code(),
                    stdout,
                    stderr,
                    duration_ms: elapsed_ms_u64(started.elapsed()),
                    timed_out: false,
                    cancelled: false,
                }
                }
                Ok(Err(error)) => ExecShellResult {
                    task_id: task_id.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: error.to_string(),
                    duration_ms: elapsed_ms_u64(started.elapsed()),
                    timed_out: false,
                    cancelled: false,
                },
                Err(_) => ExecShellResult {
                    task_id: task_id.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: "command execution timed out".into(),
                    duration_ms: elapsed_ms_u64(started.elapsed()),
                    timed_out: true,
                    cancelled: false,
                },
            }
        }
        _ = cancellation.cancelled() => ExecShellResult {
            task_id: task_id.clone(),
            exit_code: None,
            stdout: String::new(),
            stderr: "command cancelled".into(),
            duration_ms: elapsed_ms_u64(started.elapsed()),
            timed_out: false,
            cancelled: true,
        }
    };
    result.duration_ms = elapsed_ms_u64(started.elapsed());

    {
        let mut tasks = state.tasks.write().await;
        tasks.remove(&task_id);
    }

    tracing::info!(
        target: "security_audit",
        event = "node.exec_shell",
        command = %parsed_command.program,
        args = %parsed_command.args.join(" "),
        exit_code = ?result.exit_code,
        duration_ms = result.duration_ms,
        timed_out = result.timed_out,
        cancelled = result.cancelled
    );

    Ok(result)
}

async fn handle_read_file(state: &AppState, params: ReadFileParams) -> Result<ReadFileResult> {
    let path = resolve_existing_sandbox_path(&state.sandbox_root, &params.path)?;
    let data = tokio::fs::read(&path)
        .await
        .with_context(|| format!("failed to read file {}", path.display()))?;

    let offset = params.offset.unwrap_or(0) as usize;
    if offset > data.len() {
        bail!("offset out of range");
    }

    let limit = params.limit.unwrap_or(64 * 1024) as usize;
    let end = offset.saturating_add(limit).min(data.len());
    let slice = &data[offset..end];

    Ok(ReadFileResult {
        path: path.to_string_lossy().to_string(),
        content: String::from_utf8_lossy(slice).to_string(),
        bytes_read: slice.len(),
        offset: offset as u64,
        eof: end >= data.len(),
    })
}

async fn handle_write_file(state: &AppState, params: WriteFileParams) -> Result<WriteFileResult> {
    let path = resolve_write_sandbox_path(&state.sandbox_root, &params.path)?;
    let mut created_dirs = false;

    if params.create_dirs {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
            created_dirs = true;
        }
    }

    tokio::fs::write(&path, params.content.as_bytes())
        .await
        .with_context(|| format!("failed to write file {}", path.display()))?;

    Ok(WriteFileResult {
        path: path.to_string_lossy().to_string(),
        bytes_written: params.content.len(),
        created_dirs,
    })
}

async fn handle_cancel(state: &AppState, params: CancelParams) -> Result<()> {
    let tasks = state.tasks.read().await;
    let Some(token) = tasks.get(&params.task_id) else {
        bail!("task not found")
    };
    token.cancel();
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
        .get("x-zeroclaw-timestamp")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing X-ZeroClaw-Timestamp"))?;

    let timestamp = timestamp_raw
        .parse::<i64>()
        .context("invalid X-ZeroClaw-Timestamp")?;

    let now = Utc::now().timestamp();
    if (now - timestamp).abs() > 300 {
        bail!("stale request timestamp");
    }

    let signature = headers
        .get("x-zeroclaw-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing X-ZeroClaw-Signature"))?;

    let payload = format!("{timestamp}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow!("invalid hmac key length"))?;
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

fn cap_output(stdout: &[u8], stderr: &[u8], max_bytes: usize) -> (String, String, bool) {
    if max_bytes == 0 {
        return (
            String::new(),
            String::new(),
            !(stdout.is_empty() && stderr.is_empty()),
        );
    }

    let stdout_take = stdout.len().min(max_bytes);
    let remaining = max_bytes - stdout_take;
    let stderr_take = stderr.len().min(remaining);
    let truncated = stdout_take < stdout.len() || stderr_take < stderr.len();

    (
        String::from_utf8_lossy(&stdout[..stdout_take]).to_string(),
        String::from_utf8_lossy(&stderr[..stderr_take]).to_string(),
        truncated,
    )
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
    if !config.tls_required || is_loopback_bind(&config.listen_addr) {
        return Ok(());
    }

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
    if cert_ok && key_ok {
        return Ok(());
    }

    bail!(
        "node server refuses non-loopback bind without TLS material: set nodes.server.tls_cert and nodes.server.tls_key, or bind to 127.0.0.1/localhost"
    );
}

fn prepare_sandbox_root(path: &str) -> Result<PathBuf> {
    let root = PathBuf::from(path);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create sandbox root {}", root.display()))?;

    root.canonicalize()
        .with_context(|| format!("failed to canonicalize sandbox root {}", root.display()))
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

fn resolve_write_sandbox_path(sandbox_root: &Path, requested_path: &str) -> Result<PathBuf> {
    let normalized = resolve_requested_path(sandbox_root, requested_path)?;

    if normalized.exists() {
        let canonical = canonicalize_existing_path(&normalized)?;
        ensure_within_sandbox_root(sandbox_root, &canonical)?;
        return Ok(canonical);
    }

    let parent = normalized
        .parent()
        .ok_or_else(|| anyhow!("write path must have a parent directory"))?;
    let canonical_parent = canonicalize_existing_ancestor(parent)?;
    ensure_within_sandbox_root(sandbox_root, &canonical_parent)?;
    Ok(normalized)
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.exists() {
            return canonicalize_existing_path(ancestor);
        }
    }
    bail!("failed to find existing parent directory for sandbox path")
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

    #[test]
    fn sandbox_prevents_escape() {
        let temp = TempDir::new().unwrap();
        let root = prepare_sandbox_root(temp.path().to_string_lossy().as_ref()).unwrap();

        let escaped = resolve_existing_sandbox_path(&root, "../etc/passwd");
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
        symlink(outside.path(), root.join("link-out")).unwrap();

        let escaped = resolve_existing_sandbox_path(&root, "link-out/secret.txt");
        assert!(escaped.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_write_rejects_symlink_escape() {
        let sandbox = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = prepare_sandbox_root(sandbox.path().to_string_lossy().as_ref()).unwrap();

        symlink(outside.path(), root.join("link-out")).unwrap();

        let escaped = resolve_write_sandbox_path(&root, "link-out/new.txt");
        assert!(escaped.is_err());
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
}

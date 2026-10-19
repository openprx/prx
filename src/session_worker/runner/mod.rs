mod capability;
mod context;
mod operator_source;
mod ordinary_source;
mod paths;
mod prompt;
mod provider;
mod run;
mod steering;
#[cfg(test)]
mod tests;
mod tools;
mod validate;

pub(crate) use self::capability::AuthenticatedWorkerTransport;
use self::capability::{capability_expiry_from_env, session_worker_signing_key};
use self::paths::parse_tools_override;
#[cfg(test)]
pub(crate) use self::provider::build_worker_provider_selection;
use self::provider::worker_credentials;
use self::run::run_authenticated_manifest;
use self::steering::spawn_stdin_control_reader;
use self::validate::{validate_worker_cli_overrides, validate_worker_manifest_with_capability_env};
use crate::config::Config;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::tools::sessions_spawn::STEER_CHANNEL_CAPACITY;
use anyhow::{Context, Result};
use std::io::Write;
use std::sync::Arc;

fn write_worker_result(result: &impl serde::Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let json = serde_json::to_string(result).context("serialize worker result")?;
    stdout.write_all(json.as_bytes()).context("write worker result")?;
    stdout.write_all(b"\n").context("write worker newline")?;
    stdout.flush().context("flush worker stdout")?;
    Ok(())
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

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing
)]
//! Regression coverage for the **real** `prx session-worker` stdin protocol.
//!
//! Every other process-mode test in the repository substitutes `sh`/`sleep` for
//! the worker, so nothing exercised the binary's own entrypoint. These tests
//! drive `env!("CARGO_BIN_EXE_prx")` itself over real pipes and pin the four
//! properties the process-mode steering channel rests on:
//!
//! 1. The worker finishes even though the parent keeps the **stdin write end
//!    open** for the whole run. This is the single largest risk the steering
//!    design took on: the post-manifest control-frame reader must never be a
//!    `tokio::io::stdin` read on the blocking pool, because runtime drop joins
//!    that pool and the process would then wait for an EOF the parent
//!    deliberately never sends.
//! 2. Worker stdout stays **exactly one line of JSON** — the `WorkerResult` IPC
//!    contract asserted by `src/main.rs`'s "session-worker must stay
//!    stdout-clean for IPC JSON" comment.
//! 3. Authentication still gates the run: no parent capability in the
//!    environment means no run, regardless of what the manifest claims.
//! 4. A malformed manifest followed by control frames fails cleanly, still on
//!    exactly one stdout line.
//!
//! **No test here sleeps or measures wall-clock time.** "The worker finished"
//! is asserted by `Child::wait()` returning at all: a worker that hung waiting
//! for stdin EOF would never return from it. That discriminator is itself
//! proved by `holding_the_stdin_write_end_keeps_an_eof_waiting_child_alive`,
//! which runs the identical harness against a child that *does* wait for EOF
//! and shows it is still running.
//!
//! Deliberately out of scope: a full steer round-trip through a *valid* sealed
//! capability into a running agent loop. That needs the parent's in-process
//! signing key over a live provider, which no hermetic test can supply. These
//! runs therefore stop at the first post-authentication check (the config
//! source generation), which is exactly far enough to prove the control-frame
//! reader was started and did not hold the process open.

use base64::Engine as _;
use openprx::memory::MemoryEventRecording;
use openprx::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult};
use ring::hmac;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Shared HMAC secret handed to the worker through `SESSION_WORKER_SECRET`, so
/// the test can mint a capability the worker will accept without reaching for
/// any state the developer's machine owns.
const TEST_SECRET: &str = "t6-session-worker-stdin-protocol-secret";

/// The first check the worker performs *after* authentication has passed and
/// the stdin control-frame reader has been started. Seeing this error is the
/// proof that the reader thread exists in the run under test.
const POST_AUTH_FAILURE: &str = "session-worker config source is missing";

struct WorkerRun {
    status: std::process::ExitStatus,
    stdout: String,
}

/// Build a manifest that authenticates but cannot run: its config directory is
/// real (so the CLI/manifest cross-check passes) but holds no `config.toml`, so
/// the run stops at `config_source_generation`.
fn manifest_for(config_dir: &Path, workspace_dir: &Path) -> WorkerManifest {
    WorkerManifest {
        parent_capability: None,
        run_id: "t6-run".to_string(),
        task: "noop".to_string(),
        provider_name: "mock".to_string(),
        model: "mock".to_string(),
        api_key: None,
        temperature: 0.7,
        config_dir: config_dir.to_path_buf(),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: workspace_dir.to_path_buf(),
        memory_db_path: workspace_dir.join("brain.db"),
        memory_workspace_id: Some(workspace_dir.to_string_lossy().to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(workspace_dir.join("brain.db")),
        worker_memory_db_path: Some(workspace_dir.join("worker.db")),
        agent_id: None,
        persona_id: None,
        memory_event_recording: MemoryEventRecording::default(),
        allowed_tools: vec!["file_read".to_string()],
        timeout_seconds: 30,
        max_iterations: 1,
        system_prompt: None,
        identity_dir: None,
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

/// Recompute the sealed capability the worker will verify:
/// `base64url(HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest)))`,
/// where the hashed manifest is the canonical JSON with `parent_capability`
/// blanked. Written out here rather than reused from the crate on purpose — the
/// test re-derives the wire contract independently of the code under test.
fn worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> String {
    let mut value = serde_json::to_value(manifest).expect("manifest serializes");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    let payload = serde_json::to_string(&value).expect("manifest reserializes");

    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    let manifest_hex = hex::encode(hasher.finalize());

    let mut signed = Vec::new();
    signed.extend_from_slice(manifest.run_id.as_bytes());
    signed.push(0);
    signed.extend_from_slice(expiry_unix.to_string().as_bytes());
    signed.push(0);
    signed.extend_from_slice(manifest_hex.as_bytes());

    let key = hmac::Key::new(hmac::HMAC_SHA256, TEST_SECRET.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hmac::sign(&key, &signed).as_ref())
}

/// An absolute expiry far enough ahead that no plausible machine clock skew or
/// build delay can make it stale, and no wall-clock wait is implied by it.
fn far_future_expiry() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
        + 86_400
}

/// Spawn the real `prx session-worker`, with the capability environment either
/// supplied or explicitly absent.
fn spawn_worker(config_dir: &Path, home: &Path, capability_env: Option<(&str, u64)>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(config_dir)
        .arg("session-worker")
        // Keep the run off the developer's real state directory: nothing here
        // may read or mint `~/.openprx/keys/session_worker.secret`.
        .env("HOME", home)
        .env("SESSION_WORKER_SECRET", TEST_SECRET)
        .env("OPENPRX_SESSION_WORKER_SECRET_PATH", home.join("session_worker.secret"))
        .env_remove("RUST_LOG")
        .env_remove("OPENPRX_SESSION_WORKER_CAPABILITY")
        .env_remove("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some((capability, expiry)) = capability_env {
        command
            .env("OPENPRX_SESSION_WORKER_CAPABILITY", capability)
            .env("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY", expiry.to_string());
    }
    command.spawn().expect("session-worker binary spawns")
}

/// Take the child's stdin so `Child::wait()` cannot close it behind our back —
/// `wait` drops `self.stdin` before waiting, which would silently turn every
/// "write end stays open" test into a "write end was closed" test.
const fn take_stdin(child: &mut Child) -> ChildStdin {
    child.stdin.take().expect("worker stdin is piped")
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    stdin.write_all(line.as_bytes()).expect("write worker stdin line");
    stdin.write_all(b"\n").expect("write worker stdin newline");
    stdin.flush().expect("flush worker stdin");
}

fn steer_frame(message: &str) -> String {
    serde_json::to_string(&WorkerControlFrame::Steer {
        message: message.to_string(),
    })
    .expect("steer frame serializes")
}

/// Drain stdout to EOF and reap the child. `held_stdin` is dropped only *after*
/// the wait returns, so the write end is genuinely open for the whole run when
/// a test passes one in.
fn finish(mut child: Child, held_stdin: Option<ChildStdin>) -> WorkerRun {
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("worker stdout is piped")
        .read_to_string(&mut stdout)
        .expect("read worker stdout");
    let status = child.wait().expect("worker is reaped");
    drop(held_stdin);
    WorkerRun { status, stdout }
}

/// Assert the one hard IPC invariant — stdout is exactly one newline-terminated
/// JSON line — and return the parsed result.
fn assert_single_json_line(stdout: &str) -> WorkerResult {
    assert!(
        stdout.ends_with('\n'),
        "session-worker stdout must be newline-terminated, got {stdout:?}"
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "session-worker stdout must be exactly one JSON line, got {stdout:?}"
    );
    serde_json::from_str::<WorkerResult>(lines.first().copied().unwrap_or_default())
        .expect("session-worker stdout line parses as a WorkerResult")
}

fn worker_dirs() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = tempfile::tempdir().expect("temp dir");
    let config_dir = root.path().join("config");
    let workspace_dir = root.path().join("workspace");
    let home = root.path().join("home");
    for dir in [&config_dir, &workspace_dir, &home] {
        std::fs::create_dir_all(dir).expect("create worker dir");
    }
    (root, config_dir, workspace_dir, home)
}

/// The property the whole process-mode steering design hangs on: the parent
/// holds the stdin write end open for the entire run (that is how later steer
/// frames stay possible), and the worker must still terminate on its own.
///
/// The run authenticates, so the post-manifest control-frame reader really is
/// started — `POST_AUTH_FAILURE` in the result is what proves it — and three
/// steer frames really are on the pipe behind the manifest. Nothing closes the
/// pipe; the assertion is simply that `wait()` returns.
#[test]
fn worker_finishes_while_the_parent_still_holds_the_stdin_write_end() {
    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let mut manifest = manifest_for(&config_dir, &workspace_dir);
    let expiry = far_future_expiry();
    let capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(capability.clone());

    let mut child = spawn_worker(&config_dir, &home, Some((capability.as_str(), expiry)));
    let mut stdin = take_stdin(&mut child);
    write_line(
        &mut stdin,
        &serde_json::to_string(&manifest).expect("manifest serializes"),
    );
    for message in ["first steer", "second steer", "third steer"] {
        write_line(&mut stdin, &steer_frame(message));
    }

    // `stdin` is still alive here and is only dropped inside `finish`, after the
    // child has been reaped.
    let run = finish(child, Some(stdin));

    assert!(
        run.status.success(),
        "worker must exit cleanly with the stdin write end still open, got {:?}",
        run.status
    );
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success, "the sealed run cannot succeed without a config source");
    let error = result.error.unwrap_or_default();
    assert!(
        error.contains(POST_AUTH_FAILURE),
        "the run must have passed authentication and started the stdin reader, got {error:?}"
    );
}

/// The contrast case for the test above: closing the write end immediately after
/// the manifest must reach the same outcome. Together the two pin that worker
/// termination is independent of when — or whether — the parent closes stdin.
#[test]
fn worker_finishes_when_the_parent_closes_stdin_right_after_the_manifest() {
    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let mut manifest = manifest_for(&config_dir, &workspace_dir);
    let expiry = far_future_expiry();
    let capability = worker_capability(&manifest, expiry);
    manifest.parent_capability = Some(capability.clone());

    let mut child = spawn_worker(&config_dir, &home, Some((capability.as_str(), expiry)));
    let mut stdin = take_stdin(&mut child);
    write_line(
        &mut stdin,
        &serde_json::to_string(&manifest).expect("manifest serializes"),
    );
    drop(stdin);

    let run = finish(child, None);

    assert!(
        run.status.success(),
        "worker must exit cleanly after an early stdin close, got {:?}",
        run.status
    );
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert!(
        result.error.unwrap_or_default().contains(POST_AUTH_FAILURE),
        "the early-close run must fail at the same post-authentication point"
    );
}

/// Steering is strictly downstream of authentication: a manifest carrying a
/// perfectly well-formed capability is still refused when the environment does
/// not corroborate it, and the refusal itself respects the one-line contract.
#[test]
fn worker_refuses_to_run_without_the_parent_capability_env() {
    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let mut manifest = manifest_for(&config_dir, &workspace_dir);
    let expiry = far_future_expiry();
    manifest.parent_capability = Some(worker_capability(&manifest, expiry));

    let mut child = spawn_worker(&config_dir, &home, None);
    let mut stdin = take_stdin(&mut child);
    write_line(
        &mut stdin,
        &serde_json::to_string(&manifest).expect("manifest serializes"),
    );
    write_line(&mut stdin, &steer_frame("steering must not be reachable"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "an unauthenticated run still reports on stdout");
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("session-worker parent capability env is missing"),
        "authentication must fail before anything else in the run"
    );
}

/// A manifest that is not JSON at all, with a control frame queued behind it:
/// the worker must reject it, stay on one stdout line, and not be held open by
/// the frame still sitting unread in the pipe.
#[test]
fn invalid_manifest_followed_by_a_steer_frame_still_writes_exactly_one_line() {
    let (_root, config_dir, _workspace_dir, home) = worker_dirs();

    let mut child = spawn_worker(&config_dir, &home, None);
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, "not json{");
    write_line(&mut stdin, &steer_frame("frame behind a broken manifest"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "a malformed manifest is reported, not crashed on");
    let result = assert_single_json_line(&run.stdout);
    assert!(!result.success);
    assert!(
        result
            .error
            .unwrap_or_default()
            .contains("Invalid worker manifest JSON"),
        "the malformed manifest must be named in the result"
    );
}

/// Negative control for the discriminator every test above relies on.
///
/// Run the same harness — take the stdin handle, write, hold the write end —
/// against a child that genuinely blocks until stdin EOF. It is still running,
/// which is exactly the state a `tokio::io::stdin`-based control-frame reader
/// would have left the worker in; only closing the write end lets it finish.
/// Without this, "`wait()` returned" would be an assertion with nothing to
/// distinguish it from.
///
/// `try_wait` here is not a race: a child blocked on a pipe whose only write end
/// this process holds open cannot have exited, so the observation is stable and
/// needs no sleeping.
#[cfg(unix)]
#[test]
fn holding_the_stdin_write_end_keeps_an_eof_waiting_child_alive() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > /dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("stand-in EOF-waiting child spawns");
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, &steer_frame("this child never stops reading"));

    assert!(
        child.try_wait().expect("try_wait on the stand-in child").is_none(),
        "a child waiting for stdin EOF must still be running while the parent holds the write end"
    );

    drop(stdin);
    let status = child.wait().expect("stand-in child is reaped");
    assert!(status.success(), "the stand-in child exits once stdin reaches EOF");
}

/// Structural guard for the half of the risk the behavioural tests provably
/// cannot reach.
///
/// Moving the control-frame reader onto `tokio::io::stdin` or the blocking pool
/// does not make the worker hang — `RUNTIME_SHUTDOWN_TIMEOUT` in `src/main.rs`
/// caps runtime drop at two seconds — it makes every process-mode sub-agent pay
/// a silent two-second exit tax. No assertion on `Child::wait()` can see that
/// difference, and the only assertion that could is a wall-clock deadline, which
/// this repository does not allow anywhere. So the invariant is pinned at the
/// source instead, in the same style as `tests/architecture_boundaries.rs`: the
/// reader is a detached OS thread that the runtime neither owns nor joins.
#[test]
fn the_stdin_control_reader_stays_a_detached_os_thread() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("session_worker")
            .join("runner.rs"),
    )
    .expect("read src/session_worker/runner.rs");
    let body = function_body(&source, "spawn_stdin_control_reader");

    assert!(
        body.contains("std::thread::spawn"),
        "the stdin control reader must run on a detached OS thread"
    );
    for forbidden in [
        "tokio::io::stdin",
        "spawn_blocking",
        "tokio::task::spawn",
        "tokio::spawn",
        ".join()",
    ] {
        assert!(
            !body.contains(forbidden),
            "spawn_stdin_control_reader reintroduced runtime-owned stdin reading via {forbidden}: \
             the worker would then pay the runtime-drop wait on every exit"
        );
    }
}

/// Return the source text of `fn <name>`, from its signature to the closing
/// brace at the same indentation.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}");
    let start = source.find(&marker).unwrap_or_else(|| panic!("{name} must exist"));
    let rest = source.get(start..).unwrap_or_default();
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} must have a closing brace"));
    rest.get(..end).unwrap_or_default()
}

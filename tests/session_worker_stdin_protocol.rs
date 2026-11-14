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
//! 2. With no mailbox binding, stdout stays exactly one `WorkerResult` JSON
//!    line. The opt-in mailbox fixture below exercises live Data/Ack lines
//!    followed by that same terminal result, with no non-protocol output.
//! 3. Authentication still gates the run: no parent capability in the
//!    environment means no run, regardless of what the manifest claims.
//! 4. A malformed manifest followed by control frames fails cleanly, still on
//!    exactly one stdout line.
//!
//! Natural worker exit while stdin remains open is the protocol assertion.
//! A fixture-only watchdog bounds regressions, and any forced cleanup fails;
//! it does not change the runtime's decision budgets. The negative control
//! uses a real EOF-waiting child and proves it remains alive until stdin closes.
//!
//! The baseline steering tests stop at config-generation validation.

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
const TEST_SECRET: &str = "t6-stdin-protocol-secret-32bytes";

// The first check the worker performs *after* authentication has passed and
// the stdin control-frame reader has been started. Seeing this error is the
// proof that the reader thread exists in the run under test.

struct WorkerRun {
    status: std::process::ExitStatus,
    stdout: String,
}

/// Build a manifest that authenticates but cannot run: its config directory is
/// real (so the CLI/manifest cross-check passes). Without an authenticated
/// decision source, the run stops immediately after transport verification.
fn manifest_for(config_dir: &Path, workspace_dir: &Path) -> WorkerManifest {
    WorkerManifest {
        parent_capability: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "t6-run".to_string(),
        task: "noop".to_string(),
        provider_name: "mock".to_string(),
        model: "mock".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
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
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
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
        inherited_provenance: None,
        session_addr: None,
        compaction_config: None,
    }
}

/// Recompute the sealed capability the worker will verify:
/// `base64url(HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest)))`,
/// where the hashed manifest is the canonical JSON with `parent_capability`
/// blanked. Written out here rather than reused from the crate on purpose — the
/// test re-derives the wire contract independently of the code under test.
fn worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> String {
    let mut value = serde_json::to_value(manifest).expect("test: manifest serializes");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    let payload = serde_json::to_string(&value).expect("test: manifest reserializes");

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
fn spawn_worker(config_dir: &Path, home: &Path, capability_env: Option<(&str, u64)>) -> OwnedWorker {
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
    let capture = tempfile::NamedTempFile::new_in(home).expect("test: private worker stdout capture");
    command.stdout(Stdio::from(
        capture.as_file().try_clone().expect("test: capture writer"),
    ));
    OwnedWorker::new(
        command.spawn().expect("test: session-worker binary spawns"),
        Some(capture),
    )
}

/// Take the child's stdin so `Child::wait()` cannot close it behind our back —
/// `wait` drops `self.stdin` before waiting, which would silently turn every
/// "write end stays open" test into a "write end was closed" test.
const fn take_stdin(child: &mut OwnedWorker) -> ChildStdin {
    child.child.stdin.take().expect("test: worker stdin is piped")
}

fn write_line(stdin: &mut ChildStdin, line: &str) {
    // Fixed local cases send at most one manifest plus three <=256-byte
    // steer frames: <=3840 bytes in the initially empty macOS stdin pipe.
    assert!(line.len() < 3072, "fixture manifest line exceeds stdin precondition");
    stdin.write_all(line.as_bytes()).expect("test: write worker stdin line");
    stdin.write_all(b"\n").expect("test: write worker stdin newline");
    stdin.flush().expect("test: flush worker stdin");
}

fn steer_frame(message: &str) -> String {
    let frame = serde_json::to_string(&WorkerControlFrame::Steer {
        message: message.to_string(),
    })
    .expect("test: steer frame serializes");
    assert!(frame.len() < 256, "fixture steer line exceeds stdin precondition");
    frame
}

/// Confirm child reap, then read bounded private stdout capture. `held_stdin`
/// stays open until natural completion; watchdog/cleanup failure cannot pass.
fn finish(child: OwnedWorker, held_stdin: Option<ChildStdin>) -> WorkerRun {
    let result = child.finish();
    drop(held_stdin);
    result
}

// Test watchdog only: the runtime's decision/cleanup budgets are unchanged.
const WORKER_WATCHDOG: std::time::Duration = std::time::Duration::from_mins(3);

struct OwnedWorker {
    child: Child,
    reaped: bool,
    capture: Option<tempfile::NamedTempFile>,
}

impl OwnedWorker {
    const fn new(child: Child, capture: Option<tempfile::NamedTempFile>) -> Self {
        Self {
            child,
            reaped: false,
            capture,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    fn finish(mut self) -> WorkerRun {
        use std::io::{Seek as _, SeekFrom};
        let deadline = std::time::Instant::now() + WORKER_WATCHDOG;
        let status = loop {
            if let Some(status) = self.try_wait().expect("test: worker exit status") {
                break status;
            }
            assert!(std::time::Instant::now() < deadline, "worker fixture watchdog expired");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // Only inspect output after the direct child has actually been reaped.
        // This does not authorize the separately blocked nested-worker case.
        let mut stdout = String::new();
        if let Some(capture) = &mut self.capture {
            const LIMIT: u64 = 1024 * 1024;
            let file = capture.as_file_mut();
            let before = file.metadata().expect("test: capture metadata");
            assert!(
                before.is_file() && before.len() <= LIMIT,
                "worker stdout exceeded fixture limit"
            );
            file.seek(SeekFrom::Start(0)).expect("test: rewind capture");
            file.take(LIMIT + 1)
                .read_to_string(&mut stdout)
                .expect("test: complete UTF-8 worker stdout");
            let after = capture.as_file().metadata().expect("test: capture metadata after read");
            assert!(stdout.len() as u64 <= LIMIT, "worker stdout exceeded fixture limit");
            assert_eq!(before.len(), after.len(), "capture changed after direct-child reap");
            assert_eq!(after.len(), stdout.len() as u64, "capture was not read completely");
        }
        WorkerRun { status, stdout }
    }
}

impl Drop for OwnedWorker {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Child ownership prevents a reused PID being signalled before reap.
        let _ = self.child.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match self.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        if let Some(capture) = self.capture.take() {
            std::mem::forget(capture);
        }
        let _ = writeln!(
            std::io::stderr(),
            "worker cleanup UNKNOWN; retained scratch; direct child PID {}",
            self.child.id()
        );
        assert!(
            std::thread::panicking(),
            "worker cleanup did not confirm direct-child reap"
        );
    }
}

// Preserve fixture state on every failed unwind, including unknown child reap.
struct WorkerScratch(Option<tempfile::TempDir>);
impl Drop for WorkerScratch {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(root) = self.0.take()
        {
            let _ = writeln!(
                std::io::stderr(),
                "worker failure scratch retained: {}",
                root.keep().display()
            );
        }
    }
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
        .expect("test: session-worker stdout line parses as a WorkerResult")
}

fn worker_dirs() -> (
    WorkerScratch,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = tempfile::tempdir().expect("test: temp dir");
    let config_dir = root.path().join("config");
    let workspace_dir = root.path().join("workspace");
    let home = root.path().join("home");
    for dir in [&config_dir, &workspace_dir, &home] {
        std::fs::create_dir_all(dir).expect("test: create worker dir");
    }
    (WorkerScratch(Some(root)), config_dir, workspace_dir, home)
}

// The property the whole process-mode steering design hangs on: the parent
// holds the stdin write end open for the entire run (that is how later steer
// frames stay possible), and the worker must still terminate on its own.
//
// The run authenticates, so the post-manifest control-frame reader really is
// started — `POST_AUTH_FAILURE` in the result is what proves it — and three
// steer frames really are on the pipe behind the manifest. Nothing closes the
// pipe; the assertion is simply that `wait()` returns.
// Relocated intact to private LIB worker_stdin_protocol tests.

// The contrast case for the test above: closing the write end immediately after
// the manifest must reach the same outcome. Together the two pin that worker
// termination is independent of when — or whether — the parent closes stdin.
// Relocated intact to private LIB worker_stdin_protocol tests.

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
        &serde_json::to_string(&manifest).expect("test: manifest serializes"),
    );
    write_line(&mut stdin, &steer_frame("steering must not be reachable"));

    let run = finish(child, Some(stdin));

    assert!(run.status.success(), "an unauthenticated run still reports on stdout");
    let result = assert_single_json_line(&run.stdout);
    assert!(
        !result.success,
        "worker refuses to run without the parent capability env: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        result.error.as_deref(),
        Some("session-worker parent capability env is missing"),
        "authentication must fail before anything else in the run"
    );
}

/// Real `prx session-worker` over pipes, then FOUNDATION relaunch: same
/// [`openprx::messaging::SessionAddr`], new run.
#[tokio::test]
async fn relaunch_real_worker_keeps_session_addr() {
    use openprx::chat::sessions::child::{ChildSession, ChildSessionRegistry, RelaunchRecipe};
    use openprx::messaging::{CausalChain, InheritedProvenance, RemotePeerAttr, SessionAddr};
    use std::sync::Arc;

    let (_root, config_dir, workspace_dir, home) = worker_dirs();
    let addr = SessionAddr::new("prx-default", "owner-a", "child-a", Some("main"));
    let provenance = InheritedProvenance {
        remote: RemotePeerAttr {
            issuer: "spiffe://prx".into(),
            subject: "spiffe://prx/peer".into(),
            message_id: uuid::Uuid::from_u128(42),
        },
        chain: CausalChain {
            trigger: uuid::Uuid::from_u128(42),
            root: uuid::Uuid::from_u128(42),
            depth: 2,
            ancestry: Vec::new(),
        },
    };
    let registry = ChildSessionRegistry::new(Arc::new(
        openprx::memory::SqliteMemory::new(&workspace_dir.join("registry")).expect("test: durable registry"),
    ))
    .with_workspace("prx_workspace".to_owned());
    registry
        .insert(&ChildSession::new(
            addr.clone(),
            None,
            Some(RelaunchRecipe {
                addr: addr.clone(),
                last_run: None,
                inherited_provenance: Some(provenance.clone()),
            }),
        ))
        .await
        .expect("test: persist relaunch recipe");
    let expiry = far_future_expiry();
    let mut previous_run = None;
    for _ in 0..2 {
        let mut base = manifest_for(&config_dir, &workspace_dir);
        base.owner_id = Some(addr.owner.clone());
        let mut manifest = registry
            .relaunch_worker(&addr, base)
            .await
            .expect("test: relaunch manifest");
        assert_eq!(
            manifest.session_addr.as_ref(),
            Some(&addr),
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        assert_eq!(
            manifest.inherited_provenance.as_ref(),
            Some(&provenance),
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        assert_ne!(
            previous_run.as_deref(),
            Some(manifest.run_id.as_str()),
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        previous_run = Some(manifest.run_id.clone());
        let restored = registry.reconstruct(&addr).await.expect("test: durable recipe");
        assert_eq!(
            restored.recipe().and_then(|r| r.last_run.as_deref()),
            Some(manifest.run_id.as_str()),
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        let capability = worker_capability(&manifest, expiry);
        manifest.parent_capability = Some(capability.clone());
        let mut child = spawn_worker(&config_dir, &home, Some((&capability, expiry)));
        let mut stdin = take_stdin(&mut child);
        write_line(
            &mut stdin,
            &serde_json::to_string(&manifest).expect("test: manifest wire bytes"),
        );
        let run = finish(child, Some(stdin));
        assert!(
            run.status.success(),
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        let result = assert_single_json_line(&run.stdout);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(
            !result.success,
            "relaunch real worker keeps session addr: IPC identity or delivery state changed"
        );
        assert!(
            error.ends_with("worker requires an authenticated decision source"),
            "worker must verify the complete manifest before the decision-source guard: {error}"
        );
        // Each field must actually reach the worker's verified manifest: the
        // unchanged HMAC must reject a changed address or stripped provenance.
        for tamper_address in [false, true] {
            let mut tampered = manifest.clone();
            if tamper_address {
                tampered.session_addr = None;
            } else {
                tampered.inherited_provenance = None;
            }
            let mut child = spawn_worker(&config_dir, &home, Some((&capability, expiry)));
            let mut stdin = take_stdin(&mut child);
            write_line(
                &mut stdin,
                &serde_json::to_string(&tampered).expect("test: tampered wire bytes"),
            );
            let run = finish(child, Some(stdin));
            let result = assert_single_json_line(&run.stdout);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("capability signature mismatch"),
                "{result:?}"
            );
        }
    }
}

#[tokio::test]
async fn relaunch_refuses_manifest_identity_mismatch_before_record_update() {
    use openprx::chat::sessions::child::{ChildSession, ChildSessionRegistry, RelaunchIdentityError};
    let (_root, config, workspace, _home) = worker_dirs();
    let registry = ChildSessionRegistry::new(std::sync::Arc::new(
        openprx::memory::SqliteMemory::new(&workspace).expect("test: sqlite"),
    ))
    .with_workspace("prx_workspace".to_owned());
    let addr = openprx::messaging::SessionAddr::new("prx-default", "prx-owner", "prx-session", None);
    registry
        .insert(&ChildSession::new(addr.clone(), None, None))
        .await
        .expect("test: insert");
    for (owner, agent, expected) in [
        (None, None, RelaunchIdentityError::Owner),
        (Some("prx-other-owner"), None, RelaunchIdentityError::Owner),
        (Some("prx-owner"), Some("prx_other_agent"), RelaunchIdentityError::Agent),
    ] {
        let mut base = manifest_for(&config, &workspace);
        base.owner_id = owner.map(str::to_owned);
        base.agent_id = agent.map(str::to_owned);
        let error = registry
            .relaunch_worker(&addr, base)
            .await
            .expect_err("test: relaunch refuses a mismatched identity");
        assert_eq!(
            error.downcast_ref::<RelaunchIdentityError>(),
            Some(&expected),
            "relaunch refuses manifest identity mismatch before record update: replay identity or receipt stability changed"
        );
        assert!(
            registry
                .reconstruct(&addr)
                .await
                .expect("test: unchanged child")
                .recipe()
                .expect("test: recipe")
                .last_run
                .is_none(),
            "relaunch refuses manifest identity mismatch before record update: replay identity or receipt stability changed"
        );
    }
}

#[tokio::test]
async fn relaunch_scope_key_separates_session_and_agent_and_is_stable() {
    use openprx::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let (_root, config, workspace, _home) = worker_dirs();
    let memory: std::sync::Arc<dyn openprx::memory::Memory> =
        std::sync::Arc::new(openprx::memory::SqliteMemory::new(&workspace).expect("test: sqlite"));
    memory
        .agent_registry()
        .expect("test: registry")
        .resolve_incarnation("prx_workspace", "prx-owner", "n:prx_agent", &"1".repeat(64), &|| Ok(()))
        .expect("test: resolve")
        .expect("test: configured named agent");
    let registry = ChildSessionRegistry::new(memory).with_workspace("prx_workspace".to_owned());
    let mut keys = std::collections::HashSet::new();
    for (session, agent) in [
        ("prx-session", None),
        ("prx-session", Some("prx_agent")),
        ("prx-other-session", None),
    ] {
        let addr = openprx::messaging::SessionAddr::new("prx-default", "prx-owner", session, agent);
        registry
            .insert(&ChildSession::new(addr.clone(), None, None))
            .await
            .expect("test: insert");
        let mut previous = None;
        for _ in 0..2 {
            let mut base = manifest_for(&config, &workspace);
            base.owner_id = Some(addr.owner.clone());
            base.agent_id = agent.map(str::to_owned);
            let manifest = registry
                .relaunch_worker(&addr, base)
                .await
                .expect("test: matching identity");
            assert_eq!(
                manifest.session_addr.as_ref(),
                Some(&addr),
                "relaunch scope key separates session and agent and is stable: recipient authority or scope isolation changed"
            );
            if let Some(previous) = previous {
                assert_eq!(
                    manifest.session_scope_key, previous,
                    "relaunch scope key separates session and agent and is stable: recipient authority or scope isolation changed"
                );
            }
            previous = Some(manifest.session_scope_key);
        }
        assert!(
            keys.insert(previous.expect("test: scope key")),
            "session and agent must both distinguish scopes"
        );
    }
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
    assert!(
        !result.success,
        "invalid manifest followed by a steer frame still writes exactly one line: IPC identity or delivery state changed"
    );
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
    let child = Command::new("sh")
        .arg("-c")
        .arg("exec cat > /dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("test: stand-in EOF-waiting child spawns");
    let mut child = OwnedWorker::new(child, None);
    let mut stdin = take_stdin(&mut child);
    write_line(&mut stdin, &steer_frame("this child never stops reading"));

    assert!(
        child
            .try_wait()
            .expect("test: try_wait on the stand-in child")
            .is_none(),
        "a child waiting for stdin EOF must still be running while the parent holds the write end"
    );

    drop(stdin);
    let status = finish(child, None).status;
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
/// source instead, in the same style as `tests/architecture_boundaries/`: the
/// reader is a detached OS thread that the runtime neither owns nor joins.
#[test]
fn the_stdin_control_reader_stays_a_detached_os_thread() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("session_worker")
            .join("runner")
            .join("steering.rs"),
    )
    .expect("test: read src/session_worker/runner/steering.rs");
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

#[cfg(feature = "test-mock")]
async fn assert_mailbox_worker_frames(turns: usize) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    use tokio::io::AsyncReadExt as _;
    const MAX_MAILBOX_OUTPUT: u64 = 1024 * 1024;
    let (_root, config_dir, _unused_workspace, home) = worker_dirs();
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let mut config = openprx::config::Config {
        workspace_dir: workspace.clone(),
        config_path: config_dir.join("config.toml"),
        default_provider: Some("mock".into()),
        default_model: Some("prx-mailbox-model".into()),
        ..openprx::config::Config::default()
    };
    config.secrets.encrypt = false;
    config.autonomy.level = openprx::security::policy::AutonomyLevel::Full;
    config.sessions_spawn.max_recursive_depth = 8;
    config.sessions_spawn.cleanup_on_complete = false;
    config.sessions_spawn.worker_workspace_root = Some(workspace.join("workers").to_string_lossy().into_owned());
    config.tool_tiering.always_include.push("sessions_spawn".into());
    config.providers.models.push(serde_json::from_value(
        serde_json::json!({"provider":"mock","model":"prx-mailbox-model","operations":["chat"],"native_tools":true}),
    )?);
    config.cost.prices.insert(
        "mock/prx-mailbox-model".into(),
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.save().await?;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .arg("--config-dir")
        .arg(&config_dir)
        .args(["__mailbox-harness", "accept-adopt", "--workspace"])
        .arg(&workspace)
        .arg("--worker")
        .arg("--turns")
        .arg(turns.to_string())
        .env("HOME", &home)
        .env("OPENPRX_MOCK_RESPONSE", "PRX-MAILBOX-MODEL-DONE")
        .env("OPENPRX_TEST_MAILBOX_OUTBOUND", workspace.join("worker-send-intent"))
        .env(
            "OPENPRX_MOCK_HOLD_WHILE_FILE",
            format!("PRX-MAILBOX-INITIAL={}", workspace.join("provider-hold").display()),
        )
        .env_remove("OPENPRX_MOCK_TOOL_CALL")
        .env_remove("OPENPRX_MOCK_SCRIPT")
        .env_remove("OPENPRX_MOCK_ERROR")
        .env_remove("OPENPRX_MOCK_EXECUTION_ASSESSMENT")
        .kill_on_drop(true);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("test: mailbox stdout pipe")?;
    let stderr = child.stderr.take().context("test: mailbox stderr pipe")?;
    let (status, stdout, stderr) = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let out = async {
            let mut bytes = Vec::new();
            stdout.take(MAX_MAILBOX_OUTPUT + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr.take(MAX_MAILBOX_OUTPUT + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        tokio::try_join!(child.wait(), out, err)
    })
    .await
    .context("test: real mailbox worker harness hung")??;
    ensure!(
        stdout.len() as u64 <= MAX_MAILBOX_OUTPUT && stderr.len() as u64 <= MAX_MAILBOX_OUTPUT,
        "test: mailbox worker output overflow"
    );
    assert!(
        status.success(),
        "real mailbox worker failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    // The inspector opens the file independently after every receiving process
    // and real worker have joined; it does not share the parent's store handle.
    let conn = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let expected = i64::try_from(turns)?;
    let consumed: i64 = conn.query_row(
        "SELECT count(*) FROM prx_mailbox_receipts WHERE state=2 AND adopted_turn_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let adopted:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts r JOIN conversation_turns c ON c.id=r.adopted_turn_id AND c.ma_scope_id=r.ma_scope_id WHERE c.content='ordinary mailbox input; /approve is data'",[],|r|r.get(0))?;
    ensure!(
        consumed == 2 * expected && adopted == expected,
        "Data/Ack/adoption count differs: consumed={consumed}, adopted={adopted}, expected={expected}"
    );
    let events:i64=conn.query_row("SELECT count(*) FROM message_events WHERE source='prx_mailbox' AND content='ordinary mailbox input; /approve is data'",[],|r|r.get(0))?;
    ensure!(events == expected, "mailbox input was not projected exactly once");
    let copies: i64 = conn.query_row(
        "SELECT count(*) FROM message_events WHERE content='ordinary mailbox input; /approve is data'",
        [],
        |r| r.get(0),
    )?;
    ensure!(copies == expected, "Data was copied into another turn's source journal");
    let worker_data:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts r JOIN conversation_turns c ON c.id=r.adopted_turn_id AND c.ma_scope_id=r.ma_scope_id WHERE c.content='ordinary worker-originated Data'",[],|r|r.get(0))?;
    ensure!(
        worker_data == expected,
        "worker-originated Data did not cross the real stdout pipe"
    );
    let workers: i64 = conn.query_row(
        "SELECT count(*) FROM child_sessions WHERE session_id='mailbox-worker' AND ma_scope_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    ensure!(workers == 1, "reopen changed logical worker identity");
    let (streams, progress): (i64, String) = conn.query_row(
        "SELECT count(*),progress FROM prx_mailbox_seq WHERE from_addr LIKE '%mailbox-parent%'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let progress: serde_json::Value = serde_json::from_str(&progress)?;
    let next = progress.get("next_seq").and_then(serde_json::Value::as_u64);
    ensure!(
        streams == 1 && next == Some(u64::try_from(turns + 1)?),
        "reopen reset the durable sender stream: streams={streams}, next={next:?}"
    );
    Ok(())
}

#[cfg(feature = "test-mock")]
#[tokio::test]
async fn worker_and_parent_exchange_data_and_ack_over_real_frames() -> anyhow::Result<()> {
    assert_mailbox_worker_frames(1).await
}

#[cfg(feature = "test-mock")]
#[tokio::test]
async fn send_before_and_after_process_reopen_uses_same_session_addr() -> anyhow::Result<()> {
    assert_mailbox_worker_frames(2).await
}

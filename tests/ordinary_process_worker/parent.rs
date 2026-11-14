//! A real `prx chat --plain` parent with a process-local `HOME`.
//!
//! The parent environment is cleared and rebuilt: no `SESSION_WORKER_SECRET`,
//! and `OPENPRX_SESSION_WORKER_SECRET_PATH=<home>/session_worker.secret`.
//! Process workers inherit this environment (the production launcher does
//! not clear it), and a restarted parent is another parent with the same
//! `home`, so the production `TransportSecret::for_launch`/`existing` run
//! unchanged against one file.

use openprx::memory::Memory as _;
use openprx::session_worker::protocol::{WorkerManifest, WorkerResult};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Most bytes kept per parent output stream; more fails the test.
pub const MAX_PARENT_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Longest a parent may run, from spawn.
pub const PARENT_DEADLINE: Duration = Duration::from_secs(180);
/// Exit polling interval of the parent waiter.
const PARENT_POLL: Duration = Duration::from_millis(20);
/// Bounded kill-and-reap during cleanup.
const REAP_BOUND: Duration = Duration::from_secs(5);

/// The fixture's private tree: `config/` (with `workspace/`), `workers/`,
/// and `home/` (0700, holding the transport secret file). Dropped last.
pub struct ParentFixture {
    root: tempfile::TempDir,
    pub config_dir: PathBuf,
    pub parent_workspace: PathBuf,
    pub worker_root: PathBuf,
    pub home: PathBuf,
}

impl ParentFixture {
    pub fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt as _;
        let root = tempfile::tempdir().expect("test: fixture root");
        let config_dir = root.path().join("config");
        let parent_workspace = config_dir.join("workspace");
        let worker_root = root.path().join("workers");
        let home = root.path().join("home");
        for dir in [&config_dir, &parent_workspace, &worker_root] {
            std::fs::create_dir_all(dir).expect("test: fixture directory");
        }
        // The secret directory must be private to the user, as production
        // requires of `~/.openprx/keys`.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .expect("test: private home");
        Self {
            root,
            config_dir,
            parent_workspace,
            worker_root,
            home,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The transport secret file the parents and workers share.
    pub fn secret_path(&self) -> PathBuf {
        self.home.join("session_worker.secret")
    }

    /// The 32 bytes `T` the first parent launch wrote, for tests that forge a
    /// worker capability.
    pub fn transport_secret(&self) -> Vec<u8> {
        let file = std::fs::File::open(self.secret_path()).expect("test: transport secret written by the parent");
        let mut secret = Vec::new();
        file.take(33)
            .read_to_end(&mut secret)
            .expect("test: read transport secret");
        assert_eq!(secret.len(), 32, "transport secret must be exactly 32 bytes");
        secret
    }
}

/// The parent's environment, applied to a cleared command.
pub fn parent_environment(command: &mut Command, home: &Path) {
    let xdg = [
        ("XDG_DATA_HOME", home.join("xdg-data")),
        ("XDG_CACHE_HOME", home.join("xdg-cache")),
        ("XDG_STATE_HOME", home.join("xdg-state")),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
    ];
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home);
    for (name, path) in xdg {
        std::fs::create_dir_all(&path).expect("test: isolated XDG directory");
        command.env(name, path);
    }
    command
        .env("PRX_TUI", "0")
        .env("NO_COLOR", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("RUST_LOG", "off")
        .env("OPENPRX_SESSION_WORKER_SECRET_PATH", home.join("session_worker.secret"));
}

/// The user line that routes a parent turn to `marker`.
pub fn spawn_request(marker: &str) -> String {
    format!("Use sessions_spawn in process mode for {marker}.")
}

pub struct ParentOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl ParentOutput {
    pub fn assert_success(&self, what: &str) {
        assert!(self.status.success(), "{what} parent failed; stderr: {}", self.stderr);
    }
}

type Drain = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;

/// A running parent. Dropping it kills and reaps the process (bounded) and
/// joins the drain threads (bounded; a stuck thread is detached and reported).
pub struct OwnedParent {
    child: Child,
    pub(crate) reaped: bool,
    stdout: Option<Drain>,
    stderr: Option<Drain>,
    started: Instant,
}

fn drain(stream: impl std::io::Read + Send + 'static) -> Drain {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.take(MAX_PARENT_OUTPUT_BYTES + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

/// Spawn `prx --config-dir <dir> chat --plain`, send `line` then `/exit`, and
/// return it unwaited. stdout and stderr are drained concurrently.
pub fn spawn_parent(config_dir: &Path, home: &Path, line: &str) -> OwnedParent {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command.arg("--config-dir").arg(config_dir).arg("chat").arg("--plain");
    parent_environment(&mut command, home);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("test: prx parent binary");
    let started = Instant::now();
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let mut parent = OwnedParent {
        child,
        reaped: false,
        stdout,
        stderr,
        started,
    };
    let mut stdin = parent.child.stdin.take().expect("test: parent stdin");
    stdin
        .write_all(format!("{line}\n/exit\n").as_bytes())
        .expect("test: parent script");
    drop(stdin);
    parent
}

/// Run a parent to completion within `PARENT_DEADLINE`.
pub fn run_parent(config_dir: &Path, home: &Path, line: &str) -> ParentOutput {
    spawn_parent(config_dir, home, line).wait()
}

impl OwnedParent {
    /// Wait for natural exit, polling every 20 ms up to `PARENT_DEADLINE`.
    /// On expiry the parent is killed and reaped and the test fails.
    pub fn wait(mut self) -> ParentOutput {
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("test: parent exit status") {
                self.reaped = true;
                break status;
            }
            assert!(
                self.started.elapsed() < PARENT_DEADLINE,
                "parent exceeded PARENT_DEADLINE"
            );
            std::thread::sleep(PARENT_POLL);
        };
        let stdout = self.collect_stream(true);
        let stderr = self.collect_stream(false);
        ParentOutput { status, stdout, stderr }
    }

    fn collect_stream(&mut self, stdout: bool) -> String {
        let drain = if stdout { self.stdout.take() } else { self.stderr.take() };
        let bytes = drain
            .and_then(|thread| super::provider::join_bounded(thread, "parent output drain"))
            .expect("test: parent output drain joined")
            .expect("test: parent output read");
        assert!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_PARENT_OUTPUT_BYTES,
            "parent output exceeded cap"
        );
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub(crate) fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + REAP_BOUND;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    return;
                }
                Ok(None) => std::thread::sleep(PARENT_POLL),
                Err(_) => break,
            }
        }
        let _ = writeln!(
            std::io::stderr(),
            "parent cleanup could not confirm reap of PID {}",
            self.child.id()
        );
    }
}

impl Drop for OwnedParent {
    fn drop(&mut self) {
        self.kill_and_reap();
        for drain in [self.stdout.take(), self.stderr.take()].into_iter().flatten() {
            let _ = super::provider::join_bounded(drain, "parent output drain");
        }
    }
}

/// A current-thread runtime for reading the parent's memory.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: current-thread runtime")
}

pub fn owner(manifest: &WorkerManifest) -> &str {
    manifest.owner_id.as_deref().expect("test: worker owner")
}

/// The ordinary completion journal record of a worker run, if written.
pub fn completion_event(workspace: &Path, manifest: &WorkerManifest) -> Option<openprx::memory::MessageEvent> {
    let memory = openprx::memory::SqliteMemory::new(workspace).expect("test: parent memory");
    runtime()
        .block_on(memory.find_owned_message_event_by_idempotency_key(
            &workspace.to_string_lossy(),
            owner(manifest),
            &format!("worker:{}:ordinary-completion", manifest.run_id),
        ))
        .expect("test: completion journal lookup")
}

/// The ordinary completion journal holds a successful result for the run.
pub fn assert_completion_journal(workspace: &Path, manifest: &WorkerManifest) {
    let source = manifest.ordinary_source.as_ref().expect("test: ordinary source");
    assert_eq!(
        Path::new(source.parent_constraints.workspace.as_str()),
        workspace,
        "journal workspace lineage"
    );
    let event = completion_event(workspace, manifest).expect("test: ordinary completion journal");
    assert_eq!(event.event_type, "worker.ordinary.completion.confirmed");
    assert_eq!(event.run_id.as_deref(), Some(manifest.run_id.as_str()));
    let result: WorkerResult =
        serde_json::from_str(event.raw_payload_json.as_deref().expect("test: completion payload"))
            .expect("test: completion result");
    assert!(result.success, "ordinary completion result: {:?}", result.error);
    assert_eq!(
        result.ordinary_completion.as_ref().map(|value| value.run_id.as_str()),
        Some(manifest.run_id.as_str()),
        "ordinary completion evidence names the run"
    );
}

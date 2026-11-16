//! Run the real `prx` binary as a bounded child process.
//!
//! The environment is cleared and rebuilt around a private `HOME`; stdout and
//! stderr are drained concurrently with a byte cap; the child is killed and
//! reaped when it outlives its deadline or the handle is dropped.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Most bytes kept per output stream; more fails the test.
pub const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Longest a child may run, from spawn.
pub const CHILD_DEADLINE: Duration = Duration::from_secs(180);
/// Exit polling interval.
const POLL: Duration = Duration::from_millis(20);
/// Bound on the kill-and-reap during cleanup.
const REAP_BOUND: Duration = Duration::from_secs(5);
/// Longest an output drain may outlive its child. A grandchild that inherited
/// the pipe keeps it open after the child exits, so the join must be bounded.
const DRAIN_BOUND: Duration = Duration::from_secs(10);

/// A private tree: `config/` (holding `workspace/`) and `home/`.
pub struct Sandbox {
    root: tempfile::TempDir,
    pub config_dir: PathBuf,
    pub workspace: PathBuf,
    pub home: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("test: sandbox root");
        let config_dir = root.path().join("config");
        let workspace = config_dir.join("workspace");
        let home = root.path().join("home");
        for dir in [&config_dir, &workspace, &home] {
            std::fs::create_dir_all(dir).expect("test: sandbox directory");
        }
        Self {
            root,
            config_dir,
            workspace,
            home,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// `prx --config-dir <config> <args…>` with the rebuilt environment.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
        command.arg("--config-dir").arg(&self.config_dir).args(args);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join("xdg-data"))
            .env("XDG_CACHE_HOME", self.home.join("xdg-cache"))
            .env("XDG_STATE_HOME", self.home.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.home.join("xdg-config"))
            .env("PRX_TUI", "0")
            .env("NO_COLOR", "1")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("RUST_LOG", "off");
        command
    }
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

type Drain = JoinHandle<std::io::Result<Vec<u8>>>;

fn drain(stream: impl std::io::Read + Send + 'static) -> Drain {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

/// A running child. Dropping it kills and reaps the process (bounded).
pub struct Running {
    child: Child,
    reaped: bool,
    stdout: Option<Drain>,
    stderr: Option<Drain>,
    started: Instant,
    pub stdin: Option<ChildStdin>,
}

impl Running {
    /// Spawn `command` with piped stdin/stdout/stderr.
    pub fn spawn(mut command: Command) -> Self {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("test: spawn prx");
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let stdin = child.stdin.take();
        Self {
            child,
            reaped: false,
            stdout,
            stderr,
            started: Instant::now(),
            stdin,
        }
    }

    /// Write one line to stdin.
    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("test: open stdin");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .expect("test: write stdin");
        stdin.flush().expect("test: flush stdin");
    }

    /// Send the child one SIGINT, as a single Ctrl-C at its terminal would.
    pub fn interrupt(&self) {
        let status = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .status()
            .expect("test: run kill");
        assert!(status.success(), "test: SIGINT delivered to prx");
    }

    /// Close stdin and wait for exit within `CHILD_DEADLINE`.
    pub fn wait(mut self) -> Output {
        drop(self.stdin.take());
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("test: child status") {
                self.reaped = true;
                break status;
            }
            assert!(self.started.elapsed() < CHILD_DEADLINE, "prx exceeded CHILD_DEADLINE");
            std::thread::sleep(POLL);
        };
        let stdout = collect(self.stdout.take());
        let stderr = collect(self.stderr.take());
        Output { status, stdout, stderr }
    }
}

/// Join a drain without waiting forever: poll `is_finished` until `DRAIN_BOUND`.
fn join_bounded(drain: Drain) -> Vec<u8> {
    let deadline = Instant::now() + DRAIN_BOUND;
    while !drain.is_finished() {
        assert!(
            Instant::now() < deadline,
            "output pipe still open {DRAIN_BOUND:?} after the child exited (inherited by a grandchild?)"
        );
        std::thread::sleep(POLL);
    }
    drain.join().expect("test: drain thread").expect("test: read output")
}

fn collect(drain: Option<Drain>) -> String {
    let bytes = join_bounded(drain.expect("test: drain"));
    assert!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_OUTPUT_BYTES,
        "output exceeded MAX_OUTPUT_BYTES"
    );
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + REAP_BOUND;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Run `prx chat --plain`, feeding `lines` and then `/exit`.
pub fn run_plain(sandbox: &Sandbox, lines: &[&str]) -> Output {
    let mut running = Running::spawn(sandbox.command(&["chat", "--plain"]));
    for line in lines {
        running.send(line);
    }
    running.send("/exit");
    running.wait()
}

/// Install the committed `pipeline-middleware` WASM fixture into the
/// workspace and trust it through the real `prx plugin` CLI.
pub fn install_pipeline_middleware(sandbox: &Sandbox) {
    let plugin_dir = sandbox.workspace.join("plugins/pipeline-middleware");
    std::fs::create_dir_all(&plugin_dir).expect("test: plugin dir");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
    for file in ["plugin.toml", "plugin.wasm"] {
        std::fs::copy(fixture.join(file), plugin_dir.join(file)).expect("test: copy plugin fixture");
    }
    let source = plugin_dir.to_str().expect("test: UTF-8 plugin path");
    let inspected = Running::spawn(sandbox.command(&["plugin", "inspect", source])).wait();
    assert!(inspected.status.success(), "plugin inspect: {}", inspected.stderr);
    let start = inspected.stdout.find('{').expect("test: inspect JSON");
    let identity: serde_json::Value =
        serde_json::from_str(inspected.stdout[start..].trim()).expect("test: inspect output is JSON");
    let sha = identity["sha256"].as_str().expect("test: wasm sha256");
    let manifest = identity["manifest_sha256"].as_str().expect("test: manifest sha256");
    let trusted = Running::spawn(sandbox.command(&[
        "plugin",
        "trust",
        source,
        "--sha256",
        sha,
        "--manifest-sha256",
        manifest,
        "--yes",
    ]))
    .wait();
    assert!(trusted.status.success(), "plugin trust: {}", trusted.stderr);
}

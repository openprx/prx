//! Per-run capture of a process worker's stderr and tracing output.
//!
//! The worker writes its tracing events to stderr (stdout is its IPC channel).
//! The parent reads that pipe and also keeps it, line by line and redacted, in
//! `<config_dir>/logs/workers/<run_id>.log`, so an operator can read what a
//! worker said while it ran, and what it printed when it died at start. The
//! file is private (mode 0600 in a 0700 directory), bounded in size, and the
//! directory keeps only the newest runs.

use crate::security::redaction::redact_secrets;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt as _;

/// The most one run's log file holds; later output is dropped after a marker.
const WORKER_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// The newest log files kept in the directory (the file being created included).
const WORKER_LOG_KEEP_FILES: usize = 64;
/// A line longer than this without a newline is written out as if it ended.
const WORKER_LOG_MAX_LINE_BYTES: usize = 64 * 1024;
const TRUNCATED_MARKER: &str = "[worker log truncated at 4 MiB]\n";

/// The directory of the per-run worker logs under the state directory.
pub(super) fn worker_log_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("workers")
}

/// A run id is already a path-safe segment; keep the file name safe even so.
fn log_file_name(run_id: &str) -> String {
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.log")
}

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|error| anyhow::anyhow!("create worker log directory {}: {error}", dir.display()))
}

fn open_private_file(path: &Path) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A planted symlink at the log path is refused by the open.
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|error| anyhow::anyhow!("open worker log {}: {error}", path.display()))
}

/// Remove the oldest logs so that, with the one about to be created, at most
/// [`WORKER_LOG_KEEP_FILES`] remain. Best effort: a failure keeps the files.
fn prune_old_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_file()
                .then(|| (metadata.modified().unwrap_or(std::time::UNIX_EPOCH), entry.path()))
        })
        .collect();
    if logs.len() < WORKER_LOG_KEEP_FILES {
        return;
    }
    logs.sort();
    let excess = logs.len() + 1 - WORKER_LOG_KEEP_FILES;
    for (_, path) in logs.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(error = %error.kind(), "failed to prune an old worker log");
        }
    }
}

/// The open log of one worker run. Moved into the task that drains the
/// worker's stderr, which is its only writer.
pub(super) struct WorkerLog {
    path: PathBuf,
    file: tokio::fs::File,
    pending: Vec<u8>,
    written: u64,
    closed: bool,
}

impl WorkerLog {
    /// Create (or append to) the run's log file. The file exists, empty, from
    /// here on, so a worker that dies before it says anything still has one.
    pub(super) fn create(config_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let dir = worker_log_dir(config_dir);
        create_private_dir(&dir)?;
        prune_old_logs(&dir);
        let path = dir.join(log_file_name(run_id));
        let file = open_private_file(&path)?;
        let written = file.metadata().map_or(0, |metadata| metadata.len());
        Ok(Self {
            path,
            file: tokio::fs::File::from_std(file),
            pending: Vec::new(),
            written,
            closed: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Add bytes read from the worker's stderr; complete lines are written.
    pub(super) async fn push(&mut self, bytes: &[u8]) {
        if self.closed {
            return;
        }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.write_line(&line).await;
        }
        if self.pending.len() > WORKER_LOG_MAX_LINE_BYTES {
            let line = std::mem::take(&mut self.pending);
            self.write_line(&line).await;
        }
        self.flush().await;
    }

    /// Write out what is left at the end of the stream.
    pub(super) async fn finish(&mut self) {
        if !self.closed && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.write_line(&line).await;
        }
        self.flush().await;
    }

    async fn write_line(&mut self, line: &[u8]) {
        if self.closed {
            return;
        }
        let text = String::from_utf8_lossy(line);
        let mut redacted = redact_secrets(text.trim_end_matches(['\n', '\r']));
        redacted.push('\n');
        let size = u64::try_from(redacted.len()).unwrap_or(u64::MAX);
        let out = if self.written.saturating_add(size) > WORKER_LOG_MAX_BYTES {
            // Past the bound: leave one marker and drop everything after it.
            self.closed = true;
            TRUNCATED_MARKER
        } else {
            redacted.as_str()
        };
        if let Err(error) = self.file.write_all(out.as_bytes()).await {
            tracing::warn!(error = %error.kind(), "failed to write the worker log; capture stops");
            self.closed = true;
            return;
        }
        self.written = self.written.saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
    }

    async fn flush(&mut self) {
        if let Err(error) = self.file.flush().await {
            tracing::warn!(error = %error.kind(), "failed to flush the worker log; capture stops");
            self.closed = true;
        }
    }
}

//! Unix socket lifecycle for the ramflux bridge.
//!
//! The socket is bound and then chmod'd to `0600`. Startup checks the
//! directory entry: owner uid must be this process, mode must be `0600`, and
//! the path must not be a symlink. `fstat` confirms the listener fd is a
//! socket. A second concurrent connection is rejected. Frames are
//! newline-delimited and capped.

use super::protocol::{self, RpcErrorBody, RpcId};
use super::{Inner, inbound};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const PING_FRESH_MS: u64 = 30_000;

/// Whether a ping timestamp is still inside the liveness window.
#[must_use]
pub const fn ping_is_fresh(last_ping_ms: u64, now_ms: u64) -> bool {
    last_ping_ms != 0 && now_ms.saturating_sub(last_ping_ms) <= PING_FRESH_MS
}

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("socket path is a symlink: {0}")]
    Symlink(PathBuf),
    #[error("socket mode is {mode:o} (directory entry {path_mode:o}), expected 0600: {path}")]
    Mode { path: PathBuf, mode: u32, path_mode: u32 },
    #[error("socket owner uid {owner} does not match process uid {own}: {path}")]
    Owner { path: PathBuf, owner: u32, own: u32 },
    #[error("socket path is not a unix socket: {0}")]
    NotSocket(PathBuf),
    #[error("frame exceeds {max} bytes")]
    FrameTooLarge { max: usize },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What `prx doctor` can say about the socket path before the listener starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionCheck {
    Absent,
    Ok,
    Symlink,
    BadMode(u32),
    BadOwner { owner: u32, own: u32 },
    NotSocket,
    Io(String),
}

impl PermissionCheck {
    #[must_use]
    pub fn report(&self) -> String {
        match self {
            Self::Absent => "absent (will be created as 0600)".to_string(),
            Self::Ok => "ok (0600, owner matches)".to_string(),
            Self::Symlink => "refused: symlink".to_string(),
            Self::BadMode(mode) => format!("refused: mode {mode:o}"),
            Self::BadOwner { owner, own } => format!("refused: owner uid {owner}, process uid {own}"),
            Self::NotSocket => "refused: not a socket".to_string(),
            Self::Io(error) => format!("refused: {error}"),
        }
    }
}

/// Inspect a path without creating it.
#[must_use]
pub fn inspect(path: &Path) -> PermissionCheck {
    #[cfg(unix)]
    {
        inspect_unix(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        PermissionCheck::Io("ramflux channel requires a unix domain socket".to_string())
    }
}

/// Bind the listener or refuse a path that fails the ownership checks.
///
/// # Errors
///
/// Returns [`SocketError`] when the path is a symlink, has the wrong mode or
/// owner, or the bind fails.
pub fn bind(path: &Path) -> Result<tokio::net::UnixListener, SocketError> {
    #[cfg(unix)]
    {
        bind_unix(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(SocketError::Invalid(
            "ramflux channel requires a unix domain socket".to_string(),
        ))
    }
}

/// Accept one connection at a time until shutdown.
pub async fn accept_loop(inner: Arc<Inner>, listener: tokio::net::UnixListener) {
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        if !inner.try_claim_connection() {
                            reject_second(stream).await;
                            continue;
                        }
                        let session = Arc::clone(&inner);
                        tokio::spawn(async move {
                            if let Err(error) = connection(Arc::clone(&session), stream).await {
                                tracing::warn!(error = %error, "ramflux connection ended");
                            }
                            session.release_connection();
                            session.on_bridge_lost();
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "ramflux accept failed");
                        if inner.shutdown.is_cancelled() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn reject_second(mut stream: UnixStream) {
    let error = RpcErrorBody::new(-32004, "second bridge connection rejected");
    let id = RpcId::Number(0);
    if let Ok(body) = protocol::encode_error(&id, &error) {
        let _ = stream.write_all(&body).await;
        let _ = stream.write_all(b"\n").await;
    }
    tracing::warn!("rejected a second ramflux bridge connection");
}

async fn connection(inner: Arc<Inner>, stream: UnixStream) -> Result<(), SocketError> {
    let (read, mut write) = stream.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    inner.rpc.set_writer(tx);
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write.write_all(&frame).await.is_err()
                || write.write_all(b"\n").await.is_err()
                || write.flush().await.is_err()
            {
                break;
            }
        }
    });
    let mut reader = BufReader::new(read);
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => break,
            frame = read_frame(&mut reader, protocol::MAX_FRAME_BYTES) => {
                match frame {
                    Ok(Some(bytes)) => {
                        if let Err(error) = inbound::on_frame(&inner, &bytes).await {
                            tracing::debug!(error = %error, bytes = bytes.len(), "ramflux frame was not applied");
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(error = %error, "ramflux frame read failed");
                        break;
                    }
                }
            }
        }
    }
    inner.rpc.clear_writer();
    inner.writer_ready.store(false, Ordering::Release);
    writer.abort();
    Ok(())
}

async fn read_frame<R>(read: &mut R, max: usize) -> Result<Option<Vec<u8>>, SocketError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    loop {
        let available = read.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(SocketError::FrameTooLarge { max });
        }
        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            let line = available
                .get(..pos)
                .ok_or_else(|| SocketError::Invalid("frame split failed".to_string()))?;
            if buf.len().saturating_add(line.len()) > max {
                return Err(SocketError::FrameTooLarge { max });
            }
            buf.extend_from_slice(line);
            read.consume(pos.saturating_add(1));
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(buf));
        }
        if buf.len().saturating_add(available.len()) > max {
            return Err(SocketError::FrameTooLarge { max });
        }
        let n = available.len();
        buf.extend_from_slice(available);
        read.consume(n);
    }
}

#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(unix)]
fn inspect_unix(path: &Path) -> PermissionCheck {
    match classify_existing(path) {
        Ok(Existing::Absent) => PermissionCheck::Absent,
        Ok(Existing::Reusable) => PermissionCheck::Ok,
        Err(SocketError::Symlink(_)) => PermissionCheck::Symlink,
        Err(SocketError::Mode { mode, path_mode, .. }) => {
            PermissionCheck::BadMode(if path_mode == 0 { mode } else { path_mode })
        }
        Err(SocketError::Owner { owner, own, .. }) => PermissionCheck::BadOwner { owner, own },
        Err(SocketError::NotSocket(_)) => PermissionCheck::NotSocket,
        Err(error) => PermissionCheck::Io(error.to_string()),
    }
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener, SocketError> {
    prepare_parent(path)?;
    match classify_existing(path)? {
        Existing::Absent => {}
        Existing::Reusable => {
            std::fs::remove_file(path)?;
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    // macOS creates socket inodes as 0666 and rejects fchmod on them. chmod on the
    // path sets 0600. verify_listener reads that directory-entry mode.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    verify_listener(&listener, path)?;
    Ok(listener)
}

#[cfg(unix)]
enum Existing {
    Absent,
    Reusable,
}

#[cfg(unix)]
fn classify_existing(path: &Path) -> Result<Existing, SocketError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Existing::Absent),
        Err(error) => return Err(SocketError::Io(error)),
    };
    if meta.file_type().is_symlink() {
        return Err(SocketError::Symlink(path.to_path_buf()));
    }
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    if !meta.file_type().is_socket() {
        return Err(SocketError::NotSocket(path.to_path_buf()));
    }
    let mode = meta.mode() & 0o777;
    if mode != 0o600 {
        return Err(SocketError::Mode {
            path: path.to_path_buf(),
            mode,
            path_mode: mode,
        });
    }
    let own = own_uid();
    if meta.uid() != own {
        return Err(SocketError::Owner {
            path: path.to_path_buf(),
            owner: meta.uid(),
            own,
        });
    }
    Ok(Existing::Reusable)
}

#[cfg(unix)]
fn prepare_parent(path: &Path) -> Result<(), SocketError> {
    let Some(parent) = path.parent() else {
        return Err(SocketError::Invalid("socket path has no parent".to_string()));
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let existed = parent.exists();
    std::fs::create_dir_all(parent)?;
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn verify_listener(listener: &tokio::net::UnixListener, path: &Path) -> Result<(), SocketError> {
    use std::os::unix::fs::MetadataExt;
    let stat = rustix::fs::fstat(listener).map_err(std::io::Error::from)?;
    let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if kind != rustix::fs::FileType::Socket {
        return Err(SocketError::NotSocket(path.to_path_buf()));
    }
    // `fstat` confirms this fd is the socket inode. On macOS that call keeps the
    // mode from bind (0666) even after chmod; the directory entry is what the
    // kernel checks on connect, so the mode gate uses it once the inode matches.
    let path_meta = std::fs::symlink_metadata(path)?;
    if path_meta.file_type().is_symlink() {
        return Err(SocketError::Symlink(path.to_path_buf()));
    }
    let path_mode = path_meta.mode() & 0o777;
    if path_mode != 0o600 {
        return Err(SocketError::Mode {
            path: path.to_path_buf(),
            mode: permission_bits(stat.st_mode),
            path_mode,
        });
    }
    let own = own_uid();
    let owner = uid_to_u32(stat.st_uid);
    if owner != own {
        return Err(SocketError::Owner {
            path: path.to_path_buf(),
            owner,
            own,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn permission_bits(st_mode: rustix::fs::RawMode) -> u32 {
    let mode = rustix::fs::Mode::from_raw_mode(st_mode);
    u32::from(mode.bits()) & 0o777
}

#[cfg(unix)]
fn own_uid() -> u32 {
    uid_to_u32(rustix::process::getuid().as_raw())
}

#[cfg(unix)]
fn uid_to_u32(raw: impl Into<u64>) -> u32 {
    u32::try_from(raw.into()).unwrap_or(u32::MAX)
}

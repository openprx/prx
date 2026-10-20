//! Shared on-disk transport signing material for minting, verification and recovery.
use std::io;
use std::path::Path;

pub(crate) fn read_existing(path: &Path) -> io::Result<[u8; 32]> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut bytes = Vec::with_capacity(33);
    file.by_ref().take(33).read_to_end(&mut bytes)?;
    let secret = bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "worker transport key has invalid length"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(secret)
}

pub(crate) fn load_or_create(path: &Path, make_secret: impl FnOnce() -> [u8; 32]) -> io::Result<[u8; 32]> {
    use std::io::Write as _;
    match read_existing(path) {
        Ok(existing) => return Ok(existing),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    let secret = make_secret();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    pending.write_all(&secret)?;
    pending.as_file().sync_all()?;
    // Publish only complete bytes, without replacing another process's key.
    publication_result(pending.persist_noclobber(path), path, parent, secret)
}

fn publication_result(
    result: Result<std::fs::File, tempfile::PersistError>,
    path: &Path,
    parent: &Path,
    secret: [u8; 32],
) -> io::Result<[u8; 32]> {
    match result {
        Ok(_) => {
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(secret)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read_existing(path),
        Err(error) => {
            tracing::warn!(error = %error.error, "cannot publish shared worker transport key; caller will use process-local fallback");
            Err(error.error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{load_or_create, publication_result, read_existing};

    #[cfg(unix)]
    #[test]
    fn an_existing_publicly_readable_valid_key_is_tightened_without_rotation() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("transport.key");
        std::fs::write(&path, [7; 32])?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        assert_eq!(load_or_create(&path, || [8; 32])?, [7; 32]);
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path)?, vec![7; 32]);
        Ok(())
    }

    #[test]
    fn failed_key_publication_logs_the_cause_and_returns_the_fallback_error() {
        use std::io::Write as _;
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transport.key");
        let mut pending = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        pending.write_all(&[b'Q'; 32]).unwrap();
        let sink = Sink::default();
        let writer = sink.clone();
        let dispatch = crate::config::secret_paths::fixture::capture_dispatch(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let result = tracing::dispatcher::with_default(&dispatch, || {
            publication_result(
                Err(tempfile::PersistError {
                    error: std::io::Error::new(std::io::ErrorKind::Unsupported, "fixture hard links unavailable"),
                    file: pending,
                }),
                &path,
                dir.path(),
                [b'Q'; 32],
            )
        });
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        assert!(!path.exists());
        let logs = String::from_utf8(sink.0.lock().clone()).unwrap();
        assert!(logs.contains("fixture hard links unavailable"), "{logs}");
        assert!(logs.contains("process-local fallback"), "{logs}");
        assert!(!logs.contains(&"Q".repeat(32)), "{logs}");
    }

    #[test]
    fn concurrent_transport_key_creators_return_the_same_persisted_material() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("transport.key");
        let ready = std::sync::Barrier::new(2);
        let (first, second) = std::thread::scope(|scope| -> anyhow::Result<_> {
            let first = scope.spawn(|| {
                load_or_create(&path, || {
                    ready.wait();
                    [1; 32]
                })
            });
            let second = scope.spawn(|| {
                load_or_create(&path, || {
                    ready.wait();
                    [2; 32]
                })
            });
            Ok((
                first.join().map_err(|_| anyhow::anyhow!("first creator panicked"))??,
                second
                    .join()
                    .map_err(|_| anyhow::anyhow!("second creator panicked"))??,
            ))
        })?;
        assert_eq!(first, second);
        assert_eq!(read_existing(&path)?, first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o077, 0);
        }
        Ok(())
    }
    #[test]
    fn an_invalid_existing_transport_key_is_never_replaced() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("transport.key");
        std::fs::write(&path, b"invalid")?;
        let generated = std::sync::atomic::AtomicBool::new(false);
        let result = load_or_create(&path, || {
            generated.store(true, std::sync::atomic::Ordering::SeqCst);
            [1; 32]
        });
        assert!(result.is_err());
        assert!(!generated.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read(&path)?, b"invalid");
        Ok(())
    }
}

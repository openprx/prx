//! Shared on-disk transport signing material for minting, verification and recovery.
use std::io;
use std::path::Path;

pub(crate) fn read_existing(path: &Path) -> io::Result<[u8; 32]> {
    let bytes = std::fs::read(path)?;
    bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "worker transport key has invalid length"))
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
    match pending.persist_noclobber(path) {
        Ok(_) => {
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(secret)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read_existing(path),
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::{load_or_create, read_existing};

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

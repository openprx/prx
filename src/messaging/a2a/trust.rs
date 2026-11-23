//! File-generation cache; refresh runs only on the blocking pool.
use super::{identity, peers};
use crate::config::{A2aConfig, Config};
use anyhow::{Result, ensure};
use std::{path::PathBuf, sync::Arc, time::SystemTime};
#[derive(Clone)]
pub(crate) struct Trust {
    pub config: Arc<Config>,
    snapshot: Arc<parking_lot::RwLock<Result<Snapshot, ()>>>,
}
struct Snapshot {
    config: Arc<Config>,
    identity: Option<Arc<identity::Identity>>,
    stamp: Vec<FileGeneration>,
}
#[derive(PartialEq, Eq)]
struct FileGeneration {
    path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    mode: u32,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinnedA2aTrust")
    }
}
impl Trust {
    pub(crate) fn new(config: &Config) -> Self {
        let config = Arc::new(config.clone());
        Self {
            snapshot: Arc::new(parking_lot::RwLock::new(Ok(Snapshot {
                config: Arc::clone(&config),
                identity: None,
                stamp: Vec::new(),
            }))),
            config,
        }
    }
    fn stamp(&self) -> Result<Vec<FileGeneration>> {
        let mut paths = vec![self.config.config_path.clone()];
        paths.extend(crate::config::files::list_config_fragment_paths(
            &self.config.config_path,
        )?);
        let dir = self.config.workspace_dir.join("a2a");
        paths.push(dir.clone());
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "identity.toml" || name.ends_with(".key.pem") || name.ends_with(".der") {
                paths.push(entry.path());
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let metadata = std::fs::symlink_metadata(&path)?;
                ensure!(!metadata.file_type().is_symlink(), "A2A source cannot be a symlink");
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode()
                };
                #[cfg(not(unix))]
                let mode = 0;
                Ok(FileGeneration {
                    path,
                    length: metadata.len(),
                    modified: metadata.modified().ok(),
                    mode,
                    #[cfg(unix)]
                    inode: {
                        use std::os::unix::fs::MetadataExt;
                        metadata.ino()
                    },
                    #[cfg(unix)]
                    changed: {
                        use std::os::unix::fs::MetadataExt;
                        (metadata.ctime(), metadata.ctime_nsec())
                    },
                })
            })
            .collect()
    }
    pub(crate) fn refresh(&self) -> Result<()> {
        let result = (|| {
            let stamp = self.stamp()?;
            if self
                .snapshot
                .read()
                .as_ref()
                .is_ok_and(|snapshot| snapshot.stamp == stamp)
            {
                return Ok(());
            }
            let merged = crate::config::files::read_merged_toml_with_gate(&self.config.config_path)?;
            let mut config: Config = merged.try_into()?;
            config.config_path.clone_from(&self.config.config_path);
            config.workspace_dir.clone_from(&self.config.workspace_dir);
            let identity = identity::load(&config)?;
            peers::validate_local(&config.a2a, &identity)?;
            ensure!(stamp == self.stamp()?, "A2A configuration changed during refresh");
            *self.snapshot.write() = Ok(Snapshot {
                config: Arc::new(config),
                identity: Some(Arc::new(identity)),
                stamp,
            });
            Ok(())
        })();
        if result.is_err() {
            *self.snapshot.write() = Err(());
        }
        result
    }
    pub(crate) async fn refresh_async(&self) -> Result<()> {
        let trust = self.clone();
        crate::runtime::blocking::spawn_blocking(move || trust.refresh()).await?
    }
    pub(crate) fn effective(&self) -> Result<Config> {
        self.snapshot
            .read()
            .as_ref()
            .map(|snapshot| (*snapshot.config).clone())
            .map_err(|()| anyhow::anyhow!("A2A configuration unavailable"))
    }
    pub(crate) fn current(&self) -> Result<A2aConfig> {
        let config = self.effective()?.a2a;
        ensure!(config.messaging_enabled, "A2A messaging disabled");
        Ok(config)
    }
    pub(crate) fn identity(&self) -> Result<Arc<identity::Identity>> {
        self.snapshot
            .read()
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.identity.clone())
            .ok_or_else(|| anyhow::anyhow!("A2A identity unavailable"))
    }
}

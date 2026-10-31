//! Private application evidence under the existing store owner and lock.
//! These records are not grants and never enter semantic memory or its outbox.
use super::generation::ensure_dir_synced;
use super::store::{check_private, read_record, record_bytes, sync_dir, write_private_file};
use super::{LifecycleError, SkillLifecycleStore};
use sha2::{Digest, Sha256};
use std::time::Duration;

impl SkillLifecycleStore {
    fn identity_evidence_path(&self, principal: &str) -> Result<std::path::PathBuf, LifecycleError> {
        if !principal.starts_with("api:") || principal.len() > 128 || principal.chars().any(char::is_control) {
            return Err(LifecycleError::InvalidRecord {
                path: self.root().into(),
                reason: "invalid authenticated identity selector".into(),
            });
        }
        let root = self.root().join("application-evidence");
        let directory = root.join("authenticated-api-identity-v1");
        Ok(directory.join(format!("{}.json", hex::encode(Sha256::digest(principal.as_bytes())))))
    }
    pub(crate) fn write_authenticated_identity_evidence(
        &self,
        principal: &str,
        value: &serde_json::Value,
    ) -> Result<(), LifecycleError> {
        let _owner = self.lock_store(false, Duration::from_secs(10))?;
        let path = self.identity_evidence_path(principal)?;
        let root = self.root().join("application-evidence");
        ensure_dir_synced(self.root(), &root)?;
        let parent = path.parent().ok_or_else(|| LifecycleError::InvalidRecord {
            path: path.clone(),
            reason: "identity evidence parent missing".into(),
        })?;
        ensure_dir_synced(&root, parent)?;
        let bytes = record_bytes(value, &path)?;
        if bytes.len() > 16 * 1024 {
            return Err(LifecycleError::InvalidRecord {
                path,
                reason: "identity evidence exceeds its bound".into(),
            });
        }
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                check_private(&path)?;
                let stored: serde_json::Value = read_record(&path)?;
                if &stored != value {
                    return Err(LifecycleError::InvalidRecord {
                        path,
                        reason: "existing authenticated identity evidence differs".into(),
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Immutable create-new. A partial crash is invalid evidence;
                // retry never overwrites it or constructs a replacement grant.
                write_private_file(&path, &bytes, false)?;
                sync_dir(parent)?;
            }
            Err(error) => return Err(LifecycleError::io(&path, error)),
        }
        Ok(())
    }
    pub(crate) fn read_authenticated_identity_evidence(
        &self,
        principal: &str,
    ) -> Result<serde_json::Value, LifecycleError> {
        let _owner = self.lock_store(true, Duration::from_secs(10))?;
        let path = self.identity_evidence_path(principal)?;
        check_private(&self.root().join("application-evidence"))?;
        check_private(path.parent().ok_or_else(|| LifecycleError::InvalidRecord {
            path: path.clone(),
            reason: "identity evidence parent missing".into(),
        })?)?;
        check_private(&path)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| LifecycleError::io(&path, error))?;
        if metadata.len() > 16 * 1024 {
            return Err(LifecycleError::InvalidRecord {
                path,
                reason: "identity evidence exceeds its bound".into(),
            });
        }
        read_record(&path)
    }
}

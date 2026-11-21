//! Private application evidence under the existing store owner and lock.
//! These records are not grants and never enter semantic memory or its outbox.
use super::generation::ensure_dir_synced;
use super::store::{check_private, read_record, record_bytes, sync_dir, write_private_file};
use super::{LifecycleError, SkillLifecycleStore};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Domain of the identity evidence file names.
const IDENTITY_EVIDENCE_DOMAIN: &str = "prx-identity-evidence-v2";

impl SkillLifecycleStore {
    /// One record per (principal, paired credential verifier), so every
    /// paired bearer of the one install owner records its own descriptor
    /// (HOST-1a V8). The name is a digest, so neither value appears in it.
    fn identity_evidence_path(&self, principal: &str, verifier: &str) -> Result<std::path::PathBuf, LifecycleError> {
        if !principal.starts_with("api:")
            || principal.len() > 128
            || principal.chars().any(char::is_control)
            || !crate::security::pairing::is_token_hash(verifier)
        {
            return Err(LifecycleError::InvalidRecord {
                path: self.root().into(),
                reason: "invalid authenticated identity selector".into(),
            });
        }
        let root = self.root().join("application-evidence");
        let directory = root.join("authenticated-api-identity-v2");
        let mut name = Sha256::new();
        for part in [IDENTITY_EVIDENCE_DOMAIN, principal, verifier] {
            name.update(part.as_bytes());
            name.update([0_u8]);
        }
        Ok(directory.join(format!("{}.json", hex::encode(name.finalize()))))
    }
    pub(crate) fn write_authenticated_identity_evidence(
        &self,
        principal: &str,
        verifier: &str,
        value: &serde_json::Value,
    ) -> Result<(), LifecycleError> {
        let _owner = self.lock_store(false, Duration::from_secs(10))?;
        let path = self.identity_evidence_path(principal, verifier)?;
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
    /// The record of (principal, verifier); `None` when none was recorded.
    pub(crate) fn read_authenticated_identity_evidence(
        &self,
        principal: &str,
        verifier: &str,
    ) -> Result<Option<serde_json::Value>, LifecycleError> {
        let _owner = self.lock_store(true, Duration::from_secs(10))?;
        let path = self.identity_evidence_path(principal, verifier)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LifecycleError::io(&path, error)),
        }
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
        read_record(&path).map(Some)
    }
}

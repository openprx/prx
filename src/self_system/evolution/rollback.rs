use crate::self_system::evolution::record::EvolutionLayer;
use crate::self_system::evolution::safety_utils::{atomic_write, validate_path_in_workspace};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

/// Snapshot metadata for versioned rollback entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionSnapshot {
    pub version_id: String,
    pub created_at: String,
    pub content: String,
}

/// Configuration rollback manager with bounded version retention.
#[derive(Debug, Clone)]
pub struct RollbackManager {
    target_path: PathBuf,
    versions_dir: PathBuf,
    max_versions: usize,
}

impl RollbackManager {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        target_path: impl AsRef<Path>,
        versions_dir: impl AsRef<Path>,
        max_versions: usize,
    ) -> Result<Self> {
        let target_path = normalize_path_in_workspace(workspace_root.as_ref(), target_path.as_ref())?;
        let versions_dir = normalize_path_in_workspace(workspace_root.as_ref(), versions_dir.as_ref())?;
        Ok(Self {
            target_path,
            versions_dir,
            max_versions: max_versions.max(1),
        })
    }

    /// Backup current target content before any persistent mutation.
    pub async fn backup_current_version(&self) -> Result<Option<VersionSnapshot>> {
        let current = match fs::read_to_string(&self.target_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).context("failed reading rollback target"),
        };

        fs::create_dir_all(&self.versions_dir)
            .await
            .context("failed creating versions dir")?;
        let version = VersionSnapshot {
            version_id: Uuid::now_v7().to_string(),
            created_at: Utc::now().to_rfc3339(),
            content: current,
        };
        let path = self.version_path(&version.version_id)?;
        let payload = serde_json::to_vec_pretty(&version)?;
        atomic_write(&self.versions_dir, &path, &payload)
            .await
            .context("failed writing rollback snapshot")?;
        self.prune_versions().await?;
        Ok(Some(version))
    }

    /// Restore a specific version content into target file.
    pub async fn rollback_to_version(&self, version_id: &str) -> Result<()> {
        let snapshot = self.read_version(version_id).await?;
        if let Some(parent) = self.target_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await?;
            }
        }
        let workspace_root = self.target_path.parent().unwrap_or_else(|| Path::new("."));
        atomic_write(workspace_root, &self.target_path, snapshot.content.as_bytes())
            .await
            .context("failed writing rollback target")?;
        Ok(())
    }

    /// Restore most recent version.
    pub async fn rollback_latest(&self) -> Result<()> {
        let mut versions = self.list_versions().await?;
        versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let Some(latest) = versions.first() else {
            bail!("no rollback versions available");
        };
        self.rollback_to_version(&latest.version_id).await
    }

    pub async fn list_versions(&self) -> Result<Vec<VersionSnapshot>> {
        if fs::metadata(&self.versions_dir).await.is_err() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut dir = fs::read_dir(&self.versions_dir).await?;
        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let path = entry.path();
            let raw = fs::read_to_string(&path).await?;
            match serde_json::from_str::<VersionSnapshot>(&raw) {
                Ok(snapshot) => out.push(snapshot),
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping malformed rollback snapshot file"
                    );
                }
            };
        }
        Ok(out)
    }

    fn version_path(&self, version_id: &str) -> Result<PathBuf> {
        let id = sanitize_version_id(version_id)?;
        Ok(self.versions_dir.join(format!("{id}.json")))
    }

    async fn read_version(&self, version_id: &str) -> Result<VersionSnapshot> {
        let path = self.version_path(version_id)?;
        let raw = fs::read_to_string(&path)
            .await
            .with_context(|| format!("rollback version not found: {}", path.display()))?;
        let snapshot = serde_json::from_str::<VersionSnapshot>(&raw)
            .with_context(|| format!("invalid rollback snapshot: {}", path.display()))?;
        Ok(snapshot)
    }

    async fn prune_versions(&self) -> Result<()> {
        let mut versions = self.list_versions().await?;
        if versions.len() <= self.max_versions {
            return Ok(());
        }
        versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        for stale in versions.into_iter().skip(self.max_versions) {
            match self.version_path(&stale.version_id) {
                Ok(path) => {
                    if let Err(err) = fs::remove_file(&path).await {
                        tracing::warn!(
                            error = %err,
                            path = %path.display(),
                            "failed to prune rollback version"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        version_id = %stale.version_id,
                        error = %err,
                        "failed to resolve rollback version path during prune"
                    );
                }
            }
        }
        Ok(())
    }
}

fn normalize_path_in_workspace(workspace_root: &Path, path: &Path) -> Result<PathBuf> {
    let rel = if path.is_absolute() {
        path.strip_prefix(workspace_root).with_context(|| {
            format!(
                "path is outside workspace root: path={}, workspace_root={}",
                path.display(),
                workspace_root.display()
            )
        })?
    } else {
        path
    };

    validate_path_in_workspace(workspace_root, rel)
}

fn sanitize_version_id(version_id: &str) -> Result<String> {
    if version_id.contains('/') || version_id.contains('\\') || version_id.contains("..") {
        bail!("invalid version_id path tokens")
    }
    let parsed = Uuid::parse_str(version_id).context("version_id must be UUID format")?;
    Ok(parsed.to_string())
}

/// Circuit breaker state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CircuitBreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Runtime circuit breaker for evolution execution.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    failures: u32,
    state: CircuitBreakerState,
    opened_at: Option<DateTime<Utc>>,
    frozen_layers: HashSet<EvolutionLayer>,
    evaluating_layer: Option<EvolutionLayer>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown_hours: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown: Duration::hours(cooldown_hours.max(1) as i64),
            failures: 0,
            state: CircuitBreakerState::Closed,
            opened_at: None,
            frozen_layers: HashSet::new(),
            evaluating_layer: None,
        }
    }

    pub fn state(&self) -> CircuitBreakerState {
        self.state.clone()
    }

    pub fn record_failure(&mut self, now: DateTime<Utc>) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.threshold {
            self.state = CircuitBreakerState::Open;
            self.opened_at = Some(now);
        }
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
        self.state = CircuitBreakerState::Closed;
        self.opened_at = None;
    }

    pub fn can_execute(&mut self, now: DateTime<Utc>) -> bool {
        match self.state {
            CircuitBreakerState::Closed => true,
            CircuitBreakerState::HalfOpen => true,
            CircuitBreakerState::Open => {
                let Some(opened_at) = self.opened_at else {
                    return false;
                };
                if now - opened_at >= self.cooldown {
                    self.state = CircuitBreakerState::HalfOpen;
                    return true;
                }
                false
            }
        }
    }

    pub fn freeze_layer(&mut self, layer: EvolutionLayer) {
        self.frozen_layers.insert(layer);
    }

    pub fn unfreeze_layer(&mut self, layer: EvolutionLayer) {
        self.frozen_layers.remove(&layer);
    }

    /// Mark one layer as being evaluated, effectively freezing others.
    pub fn begin_layer_evaluation(&mut self, layer: EvolutionLayer) {
        self.evaluating_layer = Some(layer);
    }

    pub fn end_layer_evaluation(&mut self) {
        self.evaluating_layer = None;
    }

    pub fn can_mutate_layer(&self, layer: EvolutionLayer) -> bool {
        if self.frozen_layers.contains(&layer) {
            return false;
        }
        if let Some(active) = self.evaluating_layer.as_ref() {
            return active == &layer;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn rollback_manager_can_restore_latest_snapshot() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("cfg.toml");
        let versions = dir.path().join(".evolution/rollback/versions");
        fs::write(&target, "value=1").await.unwrap();

        let manager = RollbackManager::new(dir.path(), &target, &versions, 3).unwrap();
        let snap = manager.backup_current_version().await.unwrap().unwrap();
        fs::write(&target, "value=2").await.unwrap();

        manager.rollback_to_version(&snap.version_id).await.unwrap();
        let restored = fs::read_to_string(&target).await.unwrap();
        assert_eq!(restored, "value=1");
    }

    #[tokio::test]
    async fn rollback_manager_rejects_version_traversal_id() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("cfg.toml");
        let versions = dir.path().join(".evolution/rollback/versions");
        fs::write(&target, "value=1").await.unwrap();

        let manager = RollbackManager::new(dir.path(), &target, &versions, 3).unwrap();
        let err = manager.rollback_to_version("../escape").await.unwrap_err();
        assert!(err.to_string().contains("invalid version_id"));
    }

    #[test]
    fn circuit_breaker_opens_and_half_opens_after_cooldown() {
        let mut breaker = CircuitBreaker::new(2, 1);
        let now = Utc::now();
        breaker.record_failure(now);
        assert_eq!(breaker.state(), CircuitBreakerState::Closed);
        breaker.record_failure(now);
        assert_eq!(breaker.state(), CircuitBreakerState::Open);
        assert!(!breaker.can_execute(now));

        assert!(breaker.can_execute(now + Duration::hours(1)));
        assert_eq!(breaker.state(), CircuitBreakerState::HalfOpen);
    }

    #[test]
    fn layer_freeze_window_blocks_other_layers() {
        let mut breaker = CircuitBreaker::new(5, 24);
        breaker.begin_layer_evaluation(EvolutionLayer::Memory);
        assert!(breaker.can_mutate_layer(EvolutionLayer::Memory));
        assert!(!breaker.can_mutate_layer(EvolutionLayer::Prompt));
        breaker.end_layer_evaluation();
        assert!(breaker.can_mutate_layer(EvolutionLayer::Prompt));
    }
}

use crate::self_system::evolution::safety_utils::atomic_write;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Task-level anti-pattern record used to prevent repeated mistakes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AntiPattern {
    pub id: String,
    pub trigger: String,
    pub wrong_action: String,
    pub correct_action: String,
    pub severity: u8,
    pub occurrences: u32,
    pub last_occurred: String,
}

/// JSONL-backed anti-pattern repository.
#[derive(Debug, Clone)]
pub struct AntiPatternStore {
    path: PathBuf,
}

impl AntiPatternStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub async fn create(&self, pattern: AntiPattern) -> Result<()> {
        let mut all = self.list().await?;
        if all.iter().any(|v| v.id == pattern.id) {
            bail!("anti-pattern already exists: {}", pattern.id);
        }
        all.push(pattern);
        self.write_all(&all).await
    }

    pub async fn list(&self) -> Result<Vec<AntiPattern>> {
        if fs::metadata(&self.path).await.is_err() {
            return Ok(Vec::new());
        }

        let raw = fs::read_to_string(&self.path).await?;
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<AntiPattern>(line)
                    .with_context(|| "failed to parse anti-pattern JSONL line")
            })
            .collect()
    }

    pub async fn get(&self, id: &str) -> Result<Option<AntiPattern>> {
        let all = self.list().await?;
        Ok(all.into_iter().find(|item| item.id == id))
    }

    pub async fn update(&self, pattern: AntiPattern) -> Result<()> {
        let mut all = self.list().await?;
        let Some(slot) = all.iter_mut().find(|item| item.id == pattern.id) else {
            bail!("anti-pattern not found: {}", pattern.id);
        };
        *slot = pattern;
        self.write_all(&all).await
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let mut all = self.list().await?;
        let before = all.len();
        all.retain(|item| item.id != id);
        if all.len() == before {
            return Ok(false);
        }
        self.write_all(&all).await?;
        Ok(true)
    }

    pub async fn record_occurrence(&self, id: &str) -> Result<()> {
        let mut all = self.list().await?;
        let Some(pattern) = all.iter_mut().find(|item| item.id == id) else {
            bail!("anti-pattern not found: {id}");
        };
        pattern.occurrences = pattern.occurrences.saturating_add(1);
        pattern.last_occurred = Utc::now().to_rfc3339();
        self.write_all(&all).await
    }

    /// Match anti-patterns by trigger phrase against task description.
    pub async fn match_task(&self, task_description: &str) -> Result<Vec<AntiPattern>> {
        let all = self.list().await?;
        let lower = task_description.to_ascii_lowercase();
        Ok(all
            .into_iter()
            .filter(|pattern| lower.contains(&pattern.trigger.to_ascii_lowercase()))
            .collect())
    }

    async fn write_all(&self, patterns: &[AntiPattern]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut lines = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            lines.push(serde_json::to_string(pattern)?);
        }
        let payload = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        let workspace_root = self.path.parent().unwrap_or_else(|| Path::new("."));
        atomic_write(workspace_root, &self.path, payload.as_bytes()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pattern(id: &str, trigger: &str) -> AntiPattern {
        AntiPattern {
            id: id.to_string(),
            trigger: trigger.to_string(),
            wrong_action: "wrong".to_string(),
            correct_action: "correct".to_string(),
            severity: 2,
            occurrences: 0,
            last_occurred: "2026-02-24T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn crud_and_match_flow_works() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("anti_pattern.jsonl");
        let store = AntiPatternStore::new(&path);

        store.create(pattern("p1", "deploy")).await.unwrap();
        store.create(pattern("p2", "migration")).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 2);

        store.record_occurrence("p1").await.unwrap();
        let p1 = store.get("p1").await.unwrap().unwrap();
        assert_eq!(p1.occurrences, 1);

        let matched = store.match_task("prepare deploy plan").await.unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "p1");

        assert!(store.delete("p2").await.unwrap());
        assert!(!store.delete("p-not-found").await.unwrap());
        assert_eq!(store.list().await.unwrap().len(), 1);
    }
}

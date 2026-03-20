use super::traits::{Memory, MemoryCategory, MemoryEntry, validate_memory_write_target};
use async_trait::async_trait;
use chrono::Local;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Markdown-based memory — plain files as source of truth
///
/// Layout:
///   workspace/MEMORY.md          — curated long-term memory (core)
///   workspace/memory/YYYY-MM-DD.md — daily logs (append-only)
pub struct MarkdownMemory {
    workspace_dir: PathBuf,
}

impl MarkdownMemory {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
        }
    }

    fn memory_dir(&self) -> PathBuf {
        self.workspace_dir.join("memory")
    }

    fn core_path(&self) -> PathBuf {
        self.workspace_dir.join("MEMORY.md")
    }

    fn daily_path(&self) -> PathBuf {
        let date = Local::now().format("%Y-%m-%d").to_string();
        self.memory_dir().join(format!("{date}.md"))
    }

    async fn ensure_dirs(&self) -> anyhow::Result<()> {
        fs::create_dir_all(self.memory_dir()).await?;
        Ok(())
    }

    async fn append_to_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.ensure_dirs().await?;

        let needs_header = !path.exists()
            || fs::metadata(path)
                .await
                .map(|m| m.len() == 0)
                .unwrap_or(true);

        if needs_header {
            let header = if path == self.core_path() {
                "# Long-Term Memory\n\n"
            } else {
                let date = Local::now().format("%Y-%m-%d").to_string();
                &format!("# Daily Log — {date}\n\n")
            };
            fs::write(path, format!("{header}{content}\n")).await?;
        } else {
            // Use append mode to avoid read-modify-write race conditions.
            // Multiple concurrent writers will serialize at the OS level.
            let path = path.to_path_buf();
            let data = format!("\n{content}\n");
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
                write!(file, "{data}")?;
                Ok(())
            })
            .await??;
        }

        Ok(())
    }

    fn parse_entries_from_file(
        path: &Path,
        content: &str,
        category: &MemoryCategory,
    ) -> Vec<MemoryEntry> {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .enumerate()
            .map(|(i, line)| {
                let trimmed = line.trim();
                let clean = trimmed.strip_prefix("- ").unwrap_or(trimmed);
                let (key, parsed_content) = parse_markdown_entry(clean)
                    .unwrap_or_else(|| (format!("{filename}:{i}"), clean.to_string()));
                let timestamp = normalized_entry_timestamp(filename, &parsed_content);
                MemoryEntry {
                    id: format!("{filename}:{i}"),
                    key,
                    content: parsed_content,
                    category: category.clone(),
                    timestamp,
                    session_id: None,
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                }
            })
            .collect()
    }

    async fn read_all_entries(&self) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();

        // Read MEMORY.md (core)
        let core_path = self.core_path();
        if core_path.exists() {
            let content = fs::read_to_string(&core_path).await?;
            entries.extend(Self::parse_entries_from_file(
                &core_path,
                &content,
                &MemoryCategory::Core,
            ));
        }

        // Read daily logs
        let mem_dir = self.memory_dir();
        if mem_dir.exists() {
            let mut dir = fs::read_dir(&mem_dir).await?;
            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let content = fs::read_to_string(&path).await?;
                    entries.extend(Self::parse_entries_from_file(
                        &path,
                        &content,
                        &MemoryCategory::Daily,
                    ));
                }
            }
        }

        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(entries)
    }
}

fn parse_markdown_entry(line: &str) -> Option<(String, String)> {
    let stripped = line.strip_prefix("**")?;
    let delimiter = stripped.find("**:")?;
    let key = stripped[..delimiter].trim();
    if key.is_empty() {
        return None;
    }

    let content = stripped[delimiter + 3..].trim_start();
    Some((key.to_string(), content.to_string()))
}

fn normalized_entry_timestamp(filename: &str, content: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        for field in ["logged_at", "timestamp", "created_at", "updated_at"] {
            if let Some(raw) = value.get(field).and_then(Value::as_str) {
                return raw.to_string();
            }
        }
    }

    if filename.len() == 10
        && filename.as_bytes().get(4) == Some(&b'-')
        && filename.as_bytes().get(7) == Some(&b'-')
    {
        return format!("{filename}T00:00:00Z");
    }

    filename.to_string()
}

#[async_trait]
impl Memory for MarkdownMemory {
    fn name(&self) -> &str {
        "markdown"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_memory_write_target(key, session_id)?;
        let entry = format!("- **{key}**: {content}");
        let path = match category {
            MemoryCategory::Core => self.core_path(),
            _ => self.daily_path(),
        };
        self.append_to_file(&path, &entry).await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<MemoryEntry> = all
            .into_iter()
            .filter_map(|mut entry| {
                let content_lower = entry.content.to_lowercase();
                let matched = keywords
                    .iter()
                    .filter(|kw| content_lower.contains(**kw))
                    .count();
                if matched > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let score = matched as f64 / keywords.len() as f64;
                    entry.score = Some(score);
                    Some(entry)
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        Ok(all
            .into_iter()
            .find(|e| e.key == key || e.content.contains(key)))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        match category {
            Some(cat) => Ok(all.into_iter().filter(|e| &e.category == cat).collect()),
            None => Ok(all),
        }
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        // Markdown memory is append-only by design (audit trail)
        // Return false to indicate the entry wasn't removed
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let all = self.read_all_entries().await?;
        Ok(all.len())
    }

    async fn increment_useful_count(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.workspace_dir.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_workspace() -> (TempDir, MarkdownMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = MarkdownMemory::new(tmp.path());
        (tmp, mem)
    }

    #[tokio::test]
    async fn markdown_name() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.name(), "markdown");
    }

    #[tokio::test]
    async fn markdown_health_check() {
        let (_tmp, mem) = temp_workspace();
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn markdown_store_core() {
        let (_tmp, mem) = temp_workspace();
        mem.store("pref", "User likes Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let content = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(content.contains("User likes Rust"));
    }

    #[tokio::test]
    async fn markdown_store_daily() {
        let (_tmp, mem) = temp_workspace();
        mem.store("note", "Finished tests", MemoryCategory::Daily, None)
            .await
            .unwrap();
        let path = mem.daily_path();
        let content = fs::read_to_string(path).await.unwrap();
        assert!(content.contains("Finished tests"));
    }

    #[tokio::test]
    async fn markdown_recall_keyword() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is fast", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Python is slow", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("c", "Rust and safety", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("Rust", 10, None).await.unwrap();
        assert!(results.len() >= 2);
        assert!(
            results
                .iter()
                .all(|r| r.content.to_lowercase().contains("rust"))
        );
    }

    #[tokio::test]
    async fn markdown_recall_no_match() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is great", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("javascript", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_count() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "first", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "second", MemoryCategory::Core, None)
            .await
            .unwrap();
        let count = mem.count().await.unwrap();
        assert!(count >= 2);
    }

    #[tokio::test]
    async fn markdown_list_by_category() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "core fact", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "daily note", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(core.iter().all(|e| e.category == MemoryCategory::Core));

        let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
        assert!(daily.iter().all(|e| e.category == MemoryCategory::Daily));
    }

    #[tokio::test]
    async fn markdown_forget_is_noop() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "permanent", MemoryCategory::Core, None)
            .await
            .unwrap();
        let removed = mem.forget("a").await.unwrap();
        assert!(!removed, "Markdown memory is append-only");
    }

    #[tokio::test]
    async fn markdown_empty_recall() {
        let (_tmp, mem) = temp_workspace();
        let results = mem.recall("anything", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_empty_count() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn markdown_restores_structured_key_and_content() {
        let (_tmp, mem) = temp_workspace();
        mem.store(
            "self/fitness/daily/2026-03-10",
            r#"{"score":0.9,"note":"legacy"}"#,
            MemoryCategory::Core,
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
        )
        .await
        .unwrap();

        let entries = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        let entry = entries
            .into_iter()
            .find(|entry| entry.key == "self/fitness/daily/2026-03-10")
            .expect("structured entry");

        assert_eq!(entry.content, r#"{"score":0.9,"note":"legacy"}"#);
        assert!(
            mem.get("self/fitness/daily/2026-03-10")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn markdown_normalizes_structured_timestamps() {
        let (_tmp, mem) = temp_workspace();
        mem.store(
            "self/decisions/2026-03-10/proposal_1",
            r#"{"logged_at":"2026-03-10T12:34:56Z","key":"router"}"#,
            MemoryCategory::Core,
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
        )
        .await
        .unwrap();

        let entry = mem
            .get("self/decisions/2026-03-10/proposal_1")
            .await
            .unwrap()
            .expect("decision entry");
        assert_eq!(entry.timestamp, "2026-03-10T12:34:56Z");
    }
}

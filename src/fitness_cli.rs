#![allow(clippy::print_stdout)]

use crate::FitnessCommands;
use crate::config::Config;
use crate::memory::{Memory, SqliteMemory};
use crate::self_system::fitness_store::FitnessStore;
use anyhow::{Context, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

const LEGACY_PREFIX: &str = "self/fitness/daily/";

#[derive(Debug, Serialize)]
struct MigrationReport {
    mode: &'static str,
    legacy_rows: usize,
    exported_reports: usize,
    deleted_rows: usize,
    memory_projection_entries: usize,
    snapshot_projection_entries: usize,
    backup_dir: Option<String>,
    verified_clean: bool,
}

pub async fn handle_command(command: FitnessCommands, json: bool, config: &Config) -> Result<()> {
    match command {
        FitnessCommands::Status => {
            let store = FitnessStore::new(&config.workspace_dir);
            let output = serde_json::json!({
                "store": store.root(),
                "schedule": config.self_system.fitness,
                "latest": store.latest()?,
            });
            print_value(json, &output)
        }
        FitnessCommands::Run => {
            let report = crate::self_system::fitness::run_fitness_report_with_config(config).await?;
            print_value(json, &report)
        }
        FitnessCommands::History { limit } => {
            let reports = FitnessStore::new(&config.workspace_dir).history(limit)?;
            print_value(json, &reports)
        }
        FitnessCommands::MigrateLegacy { apply } => migrate_legacy(config, apply, json).await,
    }
}

fn print_value<T: Serialize + std::fmt::Debug>(json: bool, value: &T) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

async fn migrate_legacy(config: &Config, apply: bool, json: bool) -> Result<()> {
    let effective =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    anyhow::ensure!(
        effective == "sqlite",
        "fitness legacy migration currently requires the sqlite memory backend; found {effective}"
    );
    let rows = SqliteMemory::read_key_prefix_read_only(&config.workspace_dir, LEGACY_PREFIX)
        .await?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let memory_path = config.workspace_dir.join("MEMORY.md");
    let snapshot_path = config.workspace_dir.join(crate::memory::snapshot::SNAPSHOT_FILENAME);
    let memory_projection_entries = count_core_projection_entries(&memory_path, LEGACY_PREFIX)?;
    let snapshot_projection_entries = count_snapshot_entries(&snapshot_path, LEGACY_PREFIX)?;

    if !apply {
        let report = MigrationReport {
            mode: "dry_run",
            legacy_rows: rows.len(),
            exported_reports: 0,
            deleted_rows: 0,
            memory_projection_entries,
            snapshot_projection_entries,
            backup_dir: None,
            verified_clean: rows.is_empty() && memory_projection_entries == 0 && snapshot_projection_entries == 0,
        };
        return print_value(json, &report);
    }

    let memory = SqliteMemory::new(&config.workspace_dir)?;
    let backup_dir = config.workspace_dir.join("self/fitness/migration").join(format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        Uuid::new_v4()
    ));
    fs::create_dir_all(&backup_dir)?;
    memory
        .backup_database_to(&backup_dir.join("brain.db"))
        .await
        .context("failed to create online SQLite backup")?;
    copy_if_exists(&memory_path, &backup_dir.join("MEMORY.md"))?;
    copy_if_exists(&snapshot_path, &backup_dir.join("MEMORY_SNAPSHOT.md"))?;

    let store = FitnessStore::new(&config.workspace_dir);
    let mut exported_reports = 0_usize;
    for (key, content) in &rows {
        let day = key
            .strip_prefix(LEGACY_PREFIX)
            .ok_or_else(|| anyhow::anyhow!("unexpected legacy fitness key {key}"))?
            .parse()
            .with_context(|| format!("invalid date in legacy fitness key {key}"))?;
        store.store_legacy(day, content)?;
        exported_reports += 1;
    }
    anyhow::ensure!(exported_reports == rows.len(), "not all legacy reports were exported");

    let deleted_rows = memory.delete_key_prefix_transactional(LEGACY_PREFIX).await?;
    anyhow::ensure!(deleted_rows == rows.len(), "legacy row count changed during migration");
    rewrite_core_projection(&memory_path, LEGACY_PREFIX)?;
    rewrite_snapshot_projection(&snapshot_path, LEGACY_PREFIX)?;
    let _ = crate::memory::snapshot::export_snapshot(&config.workspace_dir)?;

    let remaining_rows = memory
        .list(None, None)
        .await?
        .into_iter()
        .filter(|entry| entry.key.starts_with(LEGACY_PREFIX))
        .count();
    let remaining_memory = count_core_projection_entries(&memory_path, LEGACY_PREFIX)?;
    let remaining_snapshot = count_snapshot_entries(&snapshot_path, LEGACY_PREFIX)?;
    let verified_clean = remaining_rows == 0 && remaining_memory == 0 && remaining_snapshot == 0;
    anyhow::ensure!(
        verified_clean,
        "legacy fitness cleanup verification failed; backups remain available"
    );

    let report = MigrationReport {
        mode: "apply",
        legacy_rows: rows.len(),
        exported_reports,
        deleted_rows,
        memory_projection_entries,
        snapshot_projection_entries,
        backup_dir: Some(backup_dir.display().to_string()),
        verified_clean,
    };
    atomic_write_json(&backup_dir.join("migration-report.json"), &report)?;
    print_value(json, &report)
}

fn copy_if_exists(source: &Path, target: &Path) -> Result<()> {
    if source.exists() {
        fs::copy(source, target).with_context(|| format!("failed to back up {}", source.display()))?;
    }
    Ok(())
}

fn count_core_projection_entries(path: &Path, prefix: &str) -> Result<usize> {
    let content = read_optional(path)?;
    Ok(content
        .lines()
        .filter_map(generated_backup_entry_key)
        .filter(|key| key.starts_with(prefix))
        .count())
}

fn count_snapshot_entries(path: &Path, prefix: &str) -> Result<usize> {
    let content = read_optional(path)?;
    Ok(content
        .lines()
        .filter_map(|line| line.strip_prefix("### 🔑 `").and_then(|rest| rest.strip_suffix('`')))
        .filter(|key| key.starts_with(prefix))
        .count())
}

fn rewrite_core_projection(path: &Path, prefix: &str) -> Result<()> {
    rewrite_locked(path, |existing| {
        let mut blocks: Vec<(Option<String>, String)> = Vec::new();
        for line in existing.split_inclusive('\n') {
            let key = generated_backup_entry_key(line).map(str::to_string);
            if key.is_some() || blocks.is_empty() {
                blocks.push((key, line.to_string()));
            } else if let Some((_, block)) = blocks.last_mut() {
                block.push_str(line);
            }
        }
        blocks
            .into_iter()
            .filter(|(key, _)| key.as_deref().is_none_or(|key| !key.starts_with(prefix)))
            .map(|(_, block)| block)
            .collect()
    })
}

fn rewrite_snapshot_projection(path: &Path, prefix: &str) -> Result<()> {
    rewrite_locked(path, |existing| {
        let mut output = String::new();
        let mut skip = false;
        for line in existing.split_inclusive('\n') {
            if let Some(key) = line
                .strip_prefix("### 🔑 `")
                .and_then(|rest| rest.trim_end().strip_suffix('`'))
            {
                skip = key.starts_with(prefix);
            }
            if !skip {
                output.push_str(line);
            }
            if skip && line.trim() == "---" {
                skip = false;
            }
        }
        output
    })
}

fn rewrite_locked(path: &Path, transform: impl FnOnce(&str) -> String) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let lock_path = path.with_extension("md.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let existing = fs::read_to_string(path)?;
    let rewritten = transform(&existing);
    let result = atomic_write(path, rewritten.as_bytes());
    lock.unlock()?;
    result
}

fn generated_backup_entry_key(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- [")?;
    let (_, rest) = rest.split_once("] **")?;
    let (key, _) = rest.split_once("**:")?;
    (!key.is_empty()).then_some(key)
}

fn read_optional(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".fitness-migration-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&staged, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::MemoryCategory;
    use crate::self_system::SELF_SYSTEM_SESSION_ID;

    #[test]
    fn core_projection_rewrite_removes_only_fitness_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("MEMORY.md");
        fs::write(
            &path,
            "# Long-Term Memory\n\n- [t] **keep/key**: keep\ncontinued\n- [t] **self/fitness/daily/2026-09-10**: {\n  x\n}\n",
        )
        .unwrap();
        rewrite_core_projection(&path, LEGACY_PREFIX).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("keep/key"));
        assert!(!content.contains(LEGACY_PREFIX));
    }

    #[test]
    fn snapshot_rewrite_removes_only_fitness_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("MEMORY_SNAPSHOT.md");
        fs::write(
            &path,
            "head\n### 🔑 `self/fitness/daily/2026-09-10`\n\nold\n\n---\n\n### 🔑 `keep/key`\n\nkeep\n",
        )
        .unwrap();
        rewrite_snapshot_projection(&path, LEGACY_PREFIX).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("keep/key"));
        assert!(!content.contains(LEGACY_PREFIX));
    }

    #[tokio::test]
    async fn migration_exports_then_removes_legacy_rows_and_projections() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            workspace_dir: tmp.path().to_path_buf(),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        let memory = SqliteMemory::new(&config.workspace_dir).unwrap();
        memory
            .store(
                "self/fitness/daily/2026-09-10",
                r#"{"version":"p0-1","final_score":0.5}"#,
                MemoryCategory::Core,
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap();
        crate::memory::snapshot::export_snapshot(&config.workspace_dir).unwrap();

        migrate_legacy(&config, true, true).await.unwrap();

        assert!(memory.get("self/fitness/daily/2026-09-10").await.unwrap().is_none());
        assert!(
            config
                .workspace_dir
                .join("self/fitness/legacy/daily/2026-09-10.json")
                .exists()
        );
        assert!(
            !read_optional(&config.workspace_dir.join("MEMORY.md"))
                .unwrap()
                .contains(LEGACY_PREFIX)
        );
        assert!(
            !read_optional(&config.workspace_dir.join("MEMORY_SNAPSHOT.md"))
                .unwrap()
                .contains(LEGACY_PREFIX)
        );
    }
}

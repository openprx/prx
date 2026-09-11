use super::fitness::FitnessReport;
use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate, Utc};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Versioned file store for fitness reports.
///
/// Fitness is operational telemetry, not conversational memory. Keeping it in
/// a dedicated store prevents reports from entering recall, `MEMORY.md`, or the
/// cold-start memory snapshot.
#[derive(Debug, Clone)]
pub struct FitnessStore {
    root: PathBuf,
}

impl FitnessStore {
    #[must_use]
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            root: workspace_dir.join("self/fitness"),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn daily_dir(&self) -> PathBuf {
        self.root.join("daily")
    }

    fn daily_path(&self, day: NaiveDate) -> PathBuf {
        self.daily_dir().join(format!("{day}.json"))
    }

    fn legacy_daily_dir(&self) -> PathBuf {
        self.root.join("legacy/daily")
    }

    pub fn load(&self, day: NaiveDate) -> Result<Option<FitnessReport>> {
        read_report(&self.daily_path(day))
    }

    pub fn latest(&self) -> Result<Option<FitnessReport>> {
        read_report(&self.root.join("latest.json"))
    }

    /// Load newest daily reports first. Non-date files are ignored.
    pub fn history(&self, limit: usize) -> Result<Vec<FitnessReport>> {
        let entries = match fs::read_dir(self.daily_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut dated_paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(day) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| value.parse::<NaiveDate>().ok())
            else {
                continue;
            };
            dated_paths.push((day, path));
        }
        dated_paths.sort_by_key(|item| std::cmp::Reverse(item.0));
        dated_paths
            .into_iter()
            .take(limit)
            .map(|(_, path)| {
                read_report(&path)?.ok_or_else(|| anyhow::anyhow!("fitness report disappeared: {}", path.display()))
            })
            .collect()
    }

    /// Atomically store a report under its calendar date and update `latest`.
    pub fn store(&self, report: &FitnessReport, retention_days: u32) -> Result<()> {
        let day = report
            .window
            .date
            .parse::<NaiveDate>()
            .with_context(|| format!("invalid fitness report date {}", report.window.date))?;
        fs::create_dir_all(self.daily_dir())
            .with_context(|| format!("failed to create fitness store {}", self.root.display()))?;
        let bytes = serde_json::to_vec_pretty(report)?;
        atomic_write(&self.daily_path(day), &bytes)?;
        atomic_write(&self.root.join("latest.json"), &bytes)?;
        self.prune(day, retention_days)
    }

    /// Preserve one pre-v2 report byte-for-byte during migration. Legacy
    /// reports are intentionally isolated from the v2 trend reader because the
    /// old scores contain fallback constants and health proxies.
    pub fn store_legacy(&self, day: NaiveDate, content: &str) -> Result<PathBuf> {
        fs::create_dir_all(self.legacy_daily_dir())?;
        let path = self.legacy_daily_dir().join(format!("{day}.json"));
        atomic_write(&path, content.as_bytes())?;
        Ok(path)
    }

    fn prune(&self, newest_day: NaiveDate, retention_days: u32) -> Result<()> {
        if retention_days == 0 {
            return Ok(());
        }
        // The newest report counts as day one, so a 180-day policy retains
        // exactly the inclusive range [newest - 179 days, newest].
        let cutoff = newest_day - Duration::days(i64::from(retention_days.saturating_sub(1)));
        let entries = match fs::read_dir(self.daily_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let Some(day) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| value.parse::<NaiveDate>().ok())
            else {
                continue;
            };
            if day < cutoff {
                fs::remove_file(&path).with_context(|| format!("failed to prune fitness report {}", path.display()))?;
            }
        }
        Ok(())
    }
}

fn read_report(path: &Path) -> Result<Option<FitnessReport>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid fitness report {}", path.display()))
        .map(Some)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fitness path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(
        ".fitness-{}-{}.tmp",
        Utc::now().timestamp_nanos_opt().unwrap_or(0),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create staged fitness report {}", temp_path.display()))?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to activate fitness report {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::fitness::{
        FitnessEvidence, FitnessMetricStatus, FitnessReportStatus, FitnessSubscores, FitnessWeights, FitnessWindow,
    };

    fn report(day: &str) -> FitnessReport {
        FitnessReport {
            version: "2".to_string(),
            status: FitnessReportStatus::Final,
            window: FitnessWindow {
                date: day.to_string(),
                timezone: "UTC".to_string(),
                start: format!("{day}T00:00:00+00:00"),
                end: format!("{day}T23:59:59+00:00"),
            },
            subscores: FitnessSubscores::default(),
            metric_status: FitnessMetricStatus::default(),
            weights: FitnessWeights::default(),
            final_score: Some(0.75),
            confidence: 0.8,
            coverage: 0.7,
            evidence: FitnessEvidence::default(),
            generated_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn report_is_stored_outside_memory_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FitnessStore::new(tmp.path());
        let report = report("2026-09-10");
        store.store(&report, 180).unwrap();

        assert_eq!(
            store.load("2026-09-10".parse().unwrap()).unwrap().unwrap().final_score,
            Some(0.75)
        );
        assert_eq!(store.latest().unwrap().unwrap().window.date, "2026-09-10");
        assert!(!tmp.path().join("MEMORY.md").exists());
        assert!(!tmp.path().join("MEMORY_SNAPSHOT.md").exists());
    }

    #[test]
    fn retention_prunes_only_expired_daily_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FitnessStore::new(tmp.path());
        store.store(&report("2026-09-05"), 5).unwrap();
        store.store(&report("2026-09-06"), 5).unwrap();
        store.store(&report("2026-09-10"), 5).unwrap();
        assert!(store.load("2026-09-05".parse().unwrap()).unwrap().is_none());
        assert!(store.load("2026-09-06".parse().unwrap()).unwrap().is_some());
        assert!(store.load("2026-09-10".parse().unwrap()).unwrap().is_some());
    }
}

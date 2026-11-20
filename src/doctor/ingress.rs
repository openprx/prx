//! `prx doctor ingress`: every durable ingress row that is not reconciled.
//!
//! Read-only: the store is opened read-only and nothing is created, repaired
//! or settled. Rows carry identifiers only, never message content. A row that
//! is `pending` while its lifetime is no longer live (its lock is present but
//! not held, or missing) is an error; the report then exits nonzero.
use super::{Config, DiagItem, DoctorReport};

const CATEGORY: &str = "ingress";

pub fn run_ingress(config: &Config) -> anyhow::Result<()> {
    let report = diagnose_ingress(config);
    report.print();
    report.ensure_success()
}

pub fn diagnose_ingress(config: &Config) -> DoctorReport {
    let mut items = Vec::new();
    check_ingress(config, &mut items);
    DoctorReport::new("OpenPRX Doctor - Ingress", items)
}

fn check_ingress(config: &Config, items: &mut Vec<DiagItem>) {
    let backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    match crate::memory::classify_memory_backend(&backend) {
        crate::memory::MemoryBackendKind::Sqlite | crate::memory::MemoryBackendKind::Lucid => {
            sqlite_rows(&crate::schema_migration::memory_db_path(config), items);
        }
        crate::memory::MemoryBackendKind::Postgres => items.push(DiagItem::warn(
            CATEGORY,
            "PostgreSQL ingress rows are not inspected by this check",
        )),
        crate::memory::MemoryBackendKind::Markdown | crate::memory::MemoryBackendKind::None => {
            items.push(DiagItem::ok(CATEGORY, "this memory backend records no durable ingress"));
        }
        crate::memory::MemoryBackendKind::Unknown => items.push(DiagItem::error(
            CATEGORY,
            format!("unsupported configured memory backend '{backend}'"),
        )),
    }
}

#[cfg(unix)]
fn sqlite_rows(database: &std::path::Path, items: &mut Vec<DiagItem>) {
    let inspection = match crate::memory::sqlite::inspect_unsettled_ingress(database) {
        Ok(inspection) => inspection,
        Err(error) => {
            items.push(DiagItem::error(
                CATEGORY,
                format!("read-only ingress inspection failed: {error}"),
            ));
            return;
        }
    };
    if inspection.rows.is_empty() {
        items.push(DiagItem::healthy(CATEGORY, "no unsettled ingress rows"));
        return;
    }
    let now = chrono::Utc::now();
    for row in &inspection.rows {
        items.push(row_item(row, now));
    }
    if inspection.truncated {
        items.push(DiagItem::warn(
            CATEGORY,
            format!(
                "only the first {} unsettled rows are listed",
                crate::memory::sqlite::MAX_INSPECTED_ROWS
            ),
        ));
    }
}

/// One row: identifiers, state, age and lock reachability; never content.
#[cfg(unix)]
fn row_item(row: &crate::memory::sqlite::UnsettledIngressRow, now: chrono::DateTime<chrono::Utc>) -> DiagItem {
    use crate::memory::ingress_recovery::IngressRecoveryState;
    use crate::memory::sqlite::IngressLockReach;
    let state = match row.state {
        IngressRecoveryState::Pending => "pending",
        IngressRecoveryState::Unconfirmed => "unconfirmed",
        IngressRecoveryState::Reconciled => "reconciled",
    };
    let age = row
        .received_at
        .map_or_else(|| "unknown".to_owned(), |received| age_text(now - received));
    let cause = row
        .cause
        .map(|cause| format!(" cause={}", cause.as_str()))
        .unwrap_or_default();
    let message = format!(
        "owner={} source={} run={} state={state}{cause} age={age} lock={}",
        row.owner_id,
        row.source,
        row.run_id,
        row.lock.as_str()
    );
    // Pending while no live lifetime holds it: nothing will settle it.
    if row.state == IngressRecoveryState::Pending && row.lock != IngressLockReach::Held {
        DiagItem::error(CATEGORY, message)
    } else {
        DiagItem::warn(CATEGORY, message)
    }
}

/// Other systems have no SQLite ingress locks, so no ingress row exists there.
#[cfg(not(unix))]
fn sqlite_rows(_database: &std::path::Path, items: &mut Vec<DiagItem>) {
    items.push(DiagItem::ok(
        CATEGORY,
        "SQLite ingress recovery requires Unix file locks; no ingress rows are recorded on this system",
    ));
}

#[cfg(unix)]
fn age_text(age: chrono::TimeDelta) -> String {
    let seconds = age.num_seconds().max(0);
    let (days, hours, minutes) = (seconds / 86_400, seconds % 86_400 / 3_600, seconds % 3_600 / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m{}s", seconds % 60)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::doctor::Severity;
    use crate::memory::{Memory, SqliteMemory};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const CONTENT: &str = "PRX-DOCTOR-PRIVATE-CONTENT";
    /// Bound on the files the state snapshot reads.
    const MAX_TREE_ENTRIES: usize = 256;
    const MAX_TREE_BYTES: usize = 64 * 1024 * 1024;

    fn input(workspace: &Path, run: &str) -> crate::memory::MessageEventInput {
        serde_json::from_value(serde_json::json!({
            "event_id": null, "idempotency_key": format!("turn:{run}:input"),
            "workspace_id": workspace.to_string_lossy(), "owner_id": "prx_user",
            "source": "agent", "channel": "agent_api", "session_key": "prx_session",
            "run_id": run, "role": "user", "event_type": "message.created",
            "content": CONTENT, "raw_payload_json": null, "visibility": "session"
        }))
        .expect("test: input")
    }

    /// Record one input under a fresh lifetime of `memory`.
    async fn record(
        memory: Arc<dyn Memory>,
        config: &Config,
        run: &str,
    ) -> crate::runtime::turn_cleanup::CleanupTicket {
        let tracker = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let ticket = tracker
            .register(tokio_util::sync::CancellationToken::new())
            .expect("test: ticket");
        let recorder = crate::runtime::ingress_recovery::IngressRecorder::prepare(memory, config, &ticket)
            .await
            .expect("test: recorder")
            .expect("test: recording on");
        recorder
            .append(input(&config.workspace_dir, run))
            .await
            .expect("test: input recorded");
        ticket
    }

    /// Every path under `root` with the bytes of each file. SQLite's
    /// shared-memory index records reader marks, so only its presence counts.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        let mut bytes = 0;
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("test: read dir") {
                let path = entry.expect("test: dir entry").path();
                assert!(entries.len() < MAX_TREE_ENTRIES, "test: bounded tree");
                if path.is_dir() {
                    pending.push(path.clone());
                    entries.insert(path, None);
                } else if path.to_string_lossy().ends_with("-shm") {
                    entries.insert(path, Some(Vec::new()));
                } else {
                    let content = std::fs::read(&path).expect("test: read file");
                    bytes += content.len();
                    assert!(bytes <= MAX_TREE_BYTES, "test: bounded tree bytes");
                    entries.insert(path, Some(content));
                }
            }
        }
        entries
    }

    #[tokio::test]
    async fn doctor_ingress_lists_stale_rows_without_content_and_creates_no_state() {
        let dir = tempfile::tempdir().expect("test: workspace");
        let mut config = Config::default();
        config.workspace_dir = dir.path().to_owned();
        // The live row first: the ended lifetime's own admission skips it and
        // leaves it untouched, and nothing later recovers the ended row.
        let live: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: live store"));
        let live_ticket = record(Arc::clone(&live), &config, "prx-live-run").await;
        {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: ended store"));
            record(memory, &config, "prx-ended-run").await.finish(true);
        }

        let before = tree(dir.path());
        let report = diagnose_ingress(&config);
        assert_eq!(before, tree(dir.path()), "doctor ingress creates and changes nothing");

        let item = |run: &str| {
            report
                .items
                .iter()
                .find(|item| item.message.contains(&format!("run={run} ")))
                .unwrap_or_else(|| panic!("test: {run} is listed: {:?}", report.items))
        };
        let ended = item("prx-ended-run");
        assert_eq!(ended.severity, Severity::Error, "a dead pending row is an error");
        for expected in [
            "owner=prx_user",
            "source=agent",
            "state=pending",
            "lock=present",
            "age=",
        ] {
            assert!(ended.message.contains(expected), "{expected}: {}", ended.message);
        }
        let held = item("prx-live-run");
        assert_eq!(held.severity, Severity::Warn, "a live row is not an error");
        assert!(held.message.contains("lock=held"), "{}", held.message);
        assert!(report.has_errors(), "the dead pending row fails the report");
        assert!(
            report.items.iter().all(|item| !item.message.contains(CONTENT)),
            "no message content: {:?}",
            report.items
        );
        live_ticket.finish(true);
    }
}

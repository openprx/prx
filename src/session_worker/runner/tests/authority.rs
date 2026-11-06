#![cfg(unix)]
use super::probe::with_worker_probe_manifest;
use crate::runtime::child_source::authority_journal;
use crate::session_worker::runner::run::{run_validated_manifest, verify_worker_parent_authority};
use anyhow::Result;

#[tokio::test]
async fn worker_verifies_parent_from_authority_journal_without_decision_store() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_worker_probe_manifest(&tmp, "ollama".into(), false, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let db = crate::causal_tree::store::db_path(workspace);
        std::fs::rename(&db, db.with_extension("unavailable"))?;
        assert!(verify_worker_parent_authority(&worker, workspace)?.is_some());
        assert!(!db.exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn forged_parent_authority_seal_is_refused_after_restart() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_worker_probe_manifest(&tmp, "ollama".into(), false, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
        let entry: String = conn.query_row("SELECT entry FROM authority WHERE run=?1", [&worker.run_id], |row| {
            row.get(0)
        })?;
        let mut entry: serde_json::Value = serde_json::from_str(&entry)?;
        let old = entry["seal"][0].as_u64().unwrap();
        entry["seal"][0] = serde_json::json!(old ^ 1);
        conn.execute(
            "UPDATE authority SET entry=?1 WHERE run=?2",
            (serde_json::to_string(&entry)?, &worker.run_id),
        )?;
        drop(conn);
        let error = verify_worker_parent_authority(&worker, workspace)
            .err()
            .expect("forged launch must fail");
        assert!(error.to_string().contains("journal seal is invalid"), "{error:#}");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn worker_child_cannot_widen_parent_capabilities() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_worker_probe_manifest(&tmp, "ollama".into(), false, |mut worker| async move {
        let workspace = std::path::PathBuf::from(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        worker.capability_manifest.as_mut().unwrap().allowed =
            worker.child_source.as_ref().unwrap().parent_constraints.allowed.clone();
        assert!(verify_worker_parent_authority(&worker, &workspace).is_err());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn pre_upgrade_child_verifies_through_causal_store_fallback() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_worker_probe_manifest(&tmp, "ollama".into(), false, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        std::fs::remove_file(authority_journal::path(workspace))?;
        assert!(verify_worker_parent_authority(&worker, workspace)?.is_some());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn worker_segment_admission_still_runs_the_gate() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let tmp = tempfile::tempdir()?;
    let provider = crate::providers::credential_probe::CredentialProbe::start_replying("prx child reply").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let spy = Arc::clone(&calls);
    with_worker_probe_manifest(&tmp, provider.custom_provider(), false, |worker| async move {
        let config = worker.config_dir.to_string_lossy().to_string();
        let result = crate::runtime::child_turn::SEGMENT_GATE_SPY
            .scope(spy, run_validated_manifest(worker, None, Some(&config)))
            .await?;
        assert!(result.success, "{:?}", result.error);
        Ok(())
    })
    .await?;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one real gate call for the one segment"
    );
    Ok(())
}

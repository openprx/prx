#![cfg(unix)]
use super::probe::{with_prepared_worker_manifest, with_worker_probe_manifest};
use crate::runtime::child_source::authority_journal;
use crate::session_worker::runner::run::{run_validated_manifest, verify_worker_parent_authority};
use anyhow::Result;

#[tokio::test]
async fn worker_verifies_parent_from_authority_journal_without_decision_store() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
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
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
        let entry: String = conn.query_row("SELECT entry FROM authority WHERE run=?1", [&worker.run_id], |row| {
            row.get(0)
        })?;
        let mut entry: serde_json::Value = serde_json::from_str(&entry)?;
        let seal_byte = entry
            .get_mut("seal")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|seal| seal.first_mut())
            .expect("authority entry has a first seal byte");
        let old = seal_byte.as_u64().expect("seal byte is an unsigned integer");
        *seal_byte = serde_json::json!(old ^ 1);
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
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |mut worker| async move {
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
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |mut worker| async move {
        let workspace = worker
            .child_source
            .as_ref()
            .unwrap()
            .parent_constraints
            .workspace
            .clone();
        let workspace = std::path::Path::new(&workspace);
        // A pre-upgrade child carries no journal version, so it has no journal authority.
        worker
            .child_source
            .as_mut()
            .expect("test: child source")
            .journal_version = None;
        std::fs::remove_file(authority_journal::path(workspace))?;
        assert!(verify_worker_parent_authority(&worker, workspace)?.is_some());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn launch_v3_refuses_missing_parent_row() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        assert_eq!(
            worker.child_source.as_ref().and_then(|source| source.journal_version),
            Some(3)
        );
        // The causal store is left intact, so only the missing row can refuse the child.
        let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
        let deleted = conn.execute("DELETE FROM authority WHERE run=?1", [&worker.run_id])?;
        drop(conn);
        assert_eq!(deleted, 1, "test: the authority row existed before deletion");
        let error = verify_worker_parent_authority(&worker, workspace)
            .err()
            .expect("a version-3 child without its journal row must be refused");
        assert!(error.to_string().contains("journal row is missing"), "{error:#}");
        Ok(())
    })
    .await
}

/// Raw journal rows and the store file, to prove a refused startup wrote nothing.
/// Replaces one sealed `launch_parent` row so a test can reseal it for another run.
fn reseal_launch_parent(
    workspace: &std::path::Path,
    key: &crate::decision::DecisionKey,
    key_run: &str,
    launch: &authority_journal::LaunchParentRow,
) -> Result<()> {
    // Mirrors the journal's sealing: HMAC over the domain and the payload.
    let payload = serde_json::to_string(launch)?;
    let seal = key.hmac(
        crate::decision::key::KeyDomain::StateFingerprint,
        &[b"child-launch-parent-authority-v1", payload.as_bytes()],
    );
    let entry = serde_json::to_string(&serde_json::json!({ "payload": payload, "seal": seal }))?;
    let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
    let updated = conn.execute("UPDATE launch_parent SET entry=?1 WHERE run=?2", (entry, key_run))?;
    anyhow::ensure!(updated == 1, "test: the launch parent row existed before resealing");
    Ok(())
}

fn startup_footprint(
    workspace: &std::path::Path,
) -> Result<(Vec<String>, Vec<String>, Option<(u64, std::time::SystemTime)>)> {
    let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
    let rows = |sql: &str| -> Result<Vec<String>> {
        let mut statement = conn.prepare(sql)?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?)
    };
    let authority = rows("SELECT entry FROM authority ORDER BY run")?;
    let launch = rows("SELECT entry FROM launch_parent ORDER BY run")?;
    let store = std::fs::metadata(crate::causal_tree::store::db_path(workspace))
        .ok()
        .map(|meta| meta.modified().map(|modified| (meta.len(), modified)))
        .transpose()?;
    Ok((authority, launch, store))
}

#[tokio::test]
async fn launch_v3_refuses_deleted_launch_parent_row() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
        let deleted = conn.execute("DELETE FROM launch_parent WHERE run=?1", [&worker.run_id])?;
        drop(conn);
        assert_eq!(deleted, 1, "test: the launch parent row existed before deletion");
        let before = startup_footprint(workspace)?;
        assert_eq!(before.0.len(), 1, "test: the valid authority row is kept");
        let error = verify_worker_parent_authority(&worker, workspace)
            .err()
            .expect("a version-3 child without its launch parent row must be refused");
        assert!(
            error.to_string().contains("launch parent authority row is missing"),
            "{error:#}"
        );
        assert_eq!(
            startup_footprint(workspace)?,
            before,
            "a refused startup leaves no side effect"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn launch_v3_refuses_corrupted_launch_parent_row() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let conn = rusqlite::Connection::open(authority_journal::path(workspace))?;
        let entry: String = conn.query_row(
            "SELECT entry FROM launch_parent WHERE run=?1",
            [&worker.run_id],
            |row| row.get(0),
        )?;
        let mut entry: serde_json::Value = serde_json::from_str(&entry)?;
        let seal_byte = entry
            .get_mut("seal")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|seal| seal.first_mut())
            .expect("launch parent entry has a first seal byte");
        let old = seal_byte.as_u64().expect("seal byte is an unsigned integer");
        *seal_byte = serde_json::json!(old ^ 1);
        conn.execute(
            "UPDATE launch_parent SET entry=?1 WHERE run=?2",
            (serde_json::to_string(&entry)?, &worker.run_id),
        )?;
        drop(conn);
        let before = startup_footprint(workspace)?;
        let error = verify_worker_parent_authority(&worker, workspace)
            .err()
            .expect("a corrupt launch parent row must be refused");
        assert!(
            error
                .to_string()
                .contains("launch parent authority journal seal is invalid"),
            "{error:#}"
        );
        assert_eq!(
            startup_footprint(workspace)?,
            before,
            "a refused startup leaves no side effect"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn launch_v3_refuses_launch_parent_sealed_for_another_run() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        let workspace = std::path::Path::new(&worker.child_source.as_ref().unwrap().parent_constraints.workspace);
        let key = crate::decision::DecisionKey::load_existing(workspace)?;
        let original =
            authority_journal::read_launch_parent(workspace, &worker.run_id, &key)?.expect("test: launch parent row");
        let mut other_run = original.clone();
        other_run.run = "another-run".to_owned();
        let mut other_lineage = original.clone();
        other_lineage.session_key = Some("another-session".to_owned());
        for (label, forged) in [("identity", other_run), ("lineage", other_lineage)] {
            assert_ne!(forged, original, "test: the {label} mutation changes the row");
            // The forged row is validly sealed, so only the comparison can refuse it.
            reseal_launch_parent(workspace, &key, &worker.run_id, &forged)?;
            let before = startup_footprint(workspace)?;
            let error = verify_worker_parent_authority(&worker, workspace)
                .err()
                .expect("a launch parent sealed for another run must be refused");
            assert!(
                error
                    .to_string()
                    .contains("launch parent authority differs from the signed manifest"),
                "{label}: {error:#}"
            );
            assert_eq!(
                startup_footprint(workspace)?,
                before,
                "{label}: a refused startup leaves no side effect"
            );
        }
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

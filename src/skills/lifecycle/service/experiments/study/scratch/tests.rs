//! Lifecycle regressions for A's parent owner. No provider/model is involved.
use super::*;
use crate::runtime::turn_cleanup::CleanupTracker;
use std::sync::Arc;
use std::time::Duration;

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_millis(100)
}

#[tokio::test]
async fn no_cleanup_owner_or_closed_owner_cannot_leave_a_new_parent_directory() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let cancel = CancellationToken::new();
    assert!(OwnedStudyScratch::new(workspace.path(), &cancel).is_err());
    assert!(std::fs::read_dir(workspace.path())?.next().is_none());
    let tracker = Arc::new(CleanupTracker::default());
    tracker.drain(deadline()).await?;
    assert!(
        tracker
            .scope(async { OwnedStudyScratch::new(workspace.path(), &cancel) })
            .await
            .is_err()
    );
    assert!(std::fs::read_dir(workspace.path())?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn constructor_error_before_case_poll_cleans_parent_and_settles_original_owner() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = Arc::new(CleanupTracker::default());
    let result: Result<()> = tracker
        .scope(async {
            let _parent = OwnedStudyScratch::new(workspace.path(), &CancellationToken::new())?;
            anyhow::bail!("fixture constructor refused before any case was polled")
        })
        .await;
    assert!(result.is_err());
    assert!(std::fs::read_dir(workspace.path())?.next().is_none());
    tracker.drain(deadline()).await?;
    Ok(())
}

#[tokio::test]
async fn dropping_started_parent_retains_a_real_late_writer_instead_of_recursively_removing_it() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = Arc::new(CleanupTracker::default());
    let (path, release, worker) = tracker
        .scope(async {
            let mut parent = OwnedStudyScratch::new(workspace.path(), &CancellationToken::new())?;
            parent.start()?;
            let path = parent.path().to_path_buf();
            let target = path.join("late-effect.bin");
            let (release, wait) = tokio::sync::oneshot::channel();
            let worker = tokio::spawn(async move {
                wait.await?;
                std::fs::write(&target, b"late owned work")?;
                Ok::<_, anyhow::Error>(())
            });
            drop(parent);
            Ok::<_, anyhow::Error>((path, release, worker))
        })
        .await?;
    assert!(path.is_dir(), "unknown case work must keep its parent alive");
    release
        .send(())
        .map_err(|()| anyhow::anyhow!("fixture late worker disappeared"))?;
    worker.await??;
    assert_eq!(std::fs::read(path.join("late-effect.bin"))?, b"late owned work");
    assert!(
        tracker.drain(deadline()).await.is_err(),
        "a retained uncertain owner cannot report confirmed cleanup"
    );
    // The test has explicitly joined its synthetic writer before TempDir
    // teardown. Production Drop is never granted that unproved knowledge.
    Ok(())
}

#[tokio::test]
async fn an_empty_directory_does_not_prove_backend_cleanup() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = Arc::new(CleanupTracker::default());
    let path = tracker
        .scope(async {
            let mut parent = OwnedStudyScratch::new(workspace.path(), &CancellationToken::new())?;
            parent.start()?;
            let path = parent.path().to_path_buf();
            assert!(std::fs::read_dir(&path)?.next().is_none());
            assert!(parent.confirm_case_removed(CleanupState::Failed).is_err());
            drop(parent);
            Ok::<_, anyhow::Error>(path)
        })
        .await?;
    assert!(path.exists());
    assert!(tracker.drain(deadline()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn backend_removed_does_not_authorize_deleting_a_nonempty_parent() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = Arc::new(CleanupTracker::default());
    let path = tracker
        .scope(async {
            let mut parent = OwnedStudyScratch::new(workspace.path(), &CancellationToken::new())?;
            parent.start()?;
            let path = parent.path().to_path_buf();
            std::fs::write(path.join("retained-child.bin"), b"owned residue")?;
            assert!(parent.confirm_case_removed(CleanupState::Removed).is_err());
            drop(parent);
            Ok::<_, anyhow::Error>(path)
        })
        .await?;
    assert_eq!(std::fs::read(path.join("retained-child.bin"))?, b"owned residue");
    assert!(tracker.drain(deadline()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn actual_removed_and_empty_parent_settles_once_without_a_cleanup_failure() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let tracker = Arc::new(CleanupTracker::default());
    let path = tracker
        .scope(async {
            let mut parent = OwnedStudyScratch::new(workspace.path(), &CancellationToken::new())?;
            let path = parent.path().to_path_buf();
            parent.start()?;
            assert!(
                parent.start().is_err(),
                "one parent cannot create a second start handoff"
            );
            parent.confirm_case_removed(CleanupState::Removed)?;
            assert!(!path.exists());
            drop(parent);
            Ok::<_, anyhow::Error>(path)
        })
        .await?;
    assert!(!path.exists());
    tracker.drain(deadline()).await?;
    Ok(())
}

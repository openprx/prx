//! Exercise the real cleanup owner across a cancelled blocking-I/O waiter.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod physical_owner;

#[tokio::test]
async fn cancelled_delivery_waiter_keeps_io_owned_until_blocking_completion() -> Result<()> {
    let cleanup = Arc::new(CleanupTracker::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let observed_cancel = Arc::clone(&cancelled);
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let owner = Arc::clone(&cleanup);
    let task = tokio::spawn(async move {
        owned_persistence(owner, move |cancel| {
            entered
                .send(())
                .map_err(|()| anyhow::anyhow!("fixture waiter disappeared"))?;
            resume.recv()?;
            observed_cancel.store(cancel.is_cancelled(), Ordering::SeqCst);
            Ok(())
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), started).await??;
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => anyhow::bail!("fixture persistence waiter was not cancelled"),
    }
    // The actual I/O closure is still blocked. Draining cannot falsely report
    // completion merely because its awaiting task was dropped.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(25),
            cleanup.drain(tokio::time::Instant::now() + Duration::from_secs(5)),
        )
        .await
        .is_err()
    );
    release.send(())?;
    cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await?;
    assert!(cancelled.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn uncertain_delivery_write_marks_the_existing_cleanup_owner_failed() -> Result<()> {
    let cleanup = Arc::new(CleanupTracker::default());
    let error = owned_persistence(Arc::clone(&cleanup), |_| {
        Err(crate::skills::lifecycle::LessonDeliveryWriteUnconfirmed.into())
    })
    .await
    .err()
    .context("fixture uncertain write unexpectedly succeeded")?;
    assert!(error.is::<crate::skills::lifecycle::LessonDeliveryWriteUnconfirmed>());
    assert!(
        cleanup
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .is_err()
    );
    assert!(cleanup.register(CancellationToken::new()).is_err());
    Ok(())
}

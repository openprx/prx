//! Optional historical latency estimates must not delay turn admission.
use std::collections::BTreeMap;
use std::time::Duration;

pub(super) async fn read<E: std::fmt::Debug + Send + 'static>(
    lookup: impl FnOnce() -> Result<BTreeMap<String, u32>, E> + Send + 'static,
) -> BTreeMap<String, u32> {
    finish(crate::runtime::blocking::spawn_blocking(lookup)).await
}

async fn finish<E: std::fmt::Debug + Send + 'static>(
    mut task: tokio::task::JoinHandle<Result<BTreeMap<String, u32>, E>>,
) -> BTreeMap<String, u32> {
    match tokio::time::timeout(Duration::from_millis(10), &mut task).await {
        Ok(Ok(Ok(samples))) => samples,
        result => {
            // Cancel a lookup still queued in the pool. A running read owns
            // its store reference and may finish without changing turn state.
            task.abort();
            tracing::warn!(error = ?result, "causal outcome latency snapshot unavailable; using pinned defaults");
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_completed_latency_snapshot_preserves_the_scoped_samples() {
        let samples = BTreeMap::from([("prx_lookup".into(), 7)]);
        let task = crate::runtime::blocking::spawn_blocking(move || Ok::<_, ()>(samples));
        // Complete the pool work before testing the successful join. The
        // optional production read may legitimately fall back under load.
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        assert_eq!(finish(task).await.get("prx_lookup"), Some(&7));
        let task = crate::runtime::blocking::spawn_blocking(|| Err::<BTreeMap<String, u32>, _>("unavailable"));
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(finish(task).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_latency_snapshot_falls_back_without_waiting_for_the_read() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        // A live blocking task prevents Tokio's automatic clock advance.
        // Poll the read once to install its timer, then advance explicitly.
        let result = read::<()>(move || {
            let _ = blocked.recv();
            Ok(BTreeMap::from([("prx_lookup".into(), 7)]))
        });
        tokio::pin!(result);
        assert!(futures::poll!(&mut result).is_pending());
        tokio::time::advance(Duration::from_millis(10)).await;
        let result = result.await;
        assert!(result.is_empty());
        // Release even a task already running; runtime shutdown must not wait
        // for a fixture-owned blocking operation.
        drop(release);
    }
}

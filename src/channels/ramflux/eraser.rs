//! Indexed erasure off the socket reader, including its blocking SQLite IO.
use super::Inner;
use crate::memory::erasure::{ErasureOrigin, SharedErasureStore};
use tokio::sync::{mpsc, oneshot};

pub(super) const QUEUE_CAP: usize = 64;

pub(super) struct EraseJob {
    pub account: String,
    pub conversation: String,
    pub message_ids: Vec<String>,
    pub done: oneshot::Sender<Result<(), String>>,
}

pub(super) async fn run(inner: std::sync::Arc<Inner>, mut jobs: mpsc::Receiver<EraseJob>) {
    while let Some(job) = jobs.recv().await {
        let result = perform(&inner, &job).await;
        let _ = job.done.send(result);
    }
}

pub(super) async fn storage<T, F>(store: SharedErasureStore, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(SharedErasureStore) -> anyhow::Result<T> + Send + 'static,
{
    crate::runtime::blocking::spawn_blocking(move || operation(store))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

pub(super) async fn fence_runs(inner: &Inner, store: SharedErasureStore, origin: ErasureOrigin) -> Result<(), String> {
    let runs = storage(store, move |store| store.run_ids(&origin)).await?;
    inner.rpc.fence_turns(&runs);
    Ok(())
}

pub(super) async fn perform(inner: &Inner, job: &EraseJob) -> Result<(), String> {
    let backend = inner.store.backend().map_err(|error| error.to_string())?;
    let attached = inner
        .attached_memory
        .lock()
        .clone()
        .ok_or_else(|| "memory is not attached".to_string())?;
    for message_id in &job.message_ids {
        let origin = ErasureOrigin::new(&job.account, &job.conversation, message_id);
        let begin = origin.clone();
        storage(backend.clone(), move |store| store.begin_delete(&begin)).await?;
        inner.cancel_message(&job.conversation, message_id);
        fence_runs(inner, backend.clone(), origin.clone()).await?;
        inner.wait_message(&job.conversation, message_id).await;
        fence_runs(inner, backend.clone(), origin.clone()).await?;
        let erase = origin.clone();
        storage(backend.clone(), move |store| store.erase_rows(&erase)).await?;
        super::hooks::erase_local(inner, &job.account, &job.conversation, message_id).await?;
        attached.checkpoint_wal().await.map_err(|error| error.to_string())?;
        // Content-free retry metadata is removed only after the data checkpoint.
        super::erase_index::remove(&inner.store, &job.account, &job.conversation, message_id)
            .map_err(|error| error.to_string())?;
        storage(backend.clone(), move |store| store.finish_delete(&origin)).await?;
        inner.resume_delete(&job.conversation, message_id);
    }
    Ok(())
}

//! Message-event erase off the socket reader.
//!
//! The reader enqueues an [`EraseJob`] and keeps reading. A later task sends
//! the JSON-RPC result when the oneshot fires. The eraser task is the one
//! that takes the sqlite writer, so an ingress turn can still finish its ack.

use super::Inner;
use tokio::sync::{mpsc, oneshot};

pub(super) const QUEUE_CAP: usize = 64;

pub(super) struct EraseJob {
    pub account: String,
    pub conversation: String,
    pub message_ids: Vec<String>,
    pub run_ids: Vec<String>,
    pub done: oneshot::Sender<Result<(), String>>,
}

pub(super) async fn run(inner: std::sync::Arc<Inner>, mut jobs: mpsc::Receiver<EraseJob>) {
    while let Some(job) = jobs.recv().await {
        let result = perform(&inner, &job).await;
        let _ = job.done.send(result);
    }
}

async fn perform(inner: &Inner, job: &EraseJob) -> Result<(), String> {
    let mut memory_keys = Vec::new();
    for message_id in &job.message_ids {
        if let Some(entry) =
            super::erase_index::load(&inner.store, &inner.crypto, &job.account, &job.conversation, message_id)
        {
            memory_keys.extend(entry.memory_keys);
        }
        super::hooks::erase_local(inner, &job.account, &job.conversation, message_id).await?;
    }
    let attached = inner.attached_memory.lock().clone();
    let Some(attached) = attached else {
        return Ok(());
    };
    let scoped = inner.scoped_memory(std::sync::Arc::clone(&attached), &job.account, &job.conversation);
    for key in &memory_keys {
        scoped.forget(key).await.map_err(|error| error.to_string())?;
    }
    for message_id in &job.message_ids {
        attached
            .forget_message_idempotency(&format!("channel:ramflux:{message_id}"))
            .await
            .map_err(|error| error.to_string())?;
    }
    for run_id in &job.run_ids {
        for suffix in ["assistant", "final"] {
            attached
                .forget_message_idempotency(&format!("turn:{run_id}:{suffix}"))
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    attached.checkpoint_wal().await.map_err(|error| error.to_string())?;
    Ok(())
}

/// Run ids recorded as `turn:{run}:assistant` or `turn:{run}:final`.
pub(super) fn run_ids_from_keys(keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        let Some(rest) = key.strip_prefix("turn:") else {
            continue;
        };
        let Some((run, suffix)) = rest.rsplit_once(':') else {
            continue;
        };
        if suffix == "assistant" || suffix == "final" {
            if !out.iter().any(|existing| existing == run) {
                out.push(run.to_string());
            }
        }
    }
    out
}

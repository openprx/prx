//! Own the durable router training commit independently of its caller.
use super::{Arc, CancellationToken, Context, Instant, Memory};
use crate::memory::RouterHistoryWriteEvidence;

pub(crate) async fn commit_training_owned(
    memory: Arc<dyn Memory>,
    evidence: RouterHistoryWriteEvidence,
    deadline: Instant,
    owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    cancellation: &CancellationToken,
) -> anyhow::Result<crate::memory::RouterHistoryCommitReceipt> {
    let ticket = owner.register(cancellation.clone())?;

    {
        let work = async move {
            let result = memory.commit_router_history(&evidence, deadline).await;
            // An error during PostgreSQL COMMIT may leave its outcome unknown.
            // The backend's real commit receipt is the only success acknowledgement.
            ticket.finish(result.is_ok());
            result
        };

        tokio::spawn(work)
    }
    .await
    .context("Router history write owner task failed")?
}

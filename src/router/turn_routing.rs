//! Turn-owned Router bindings: the provider identity binding retained for
//! historical Router replay and the durable Router training commit, owned
//! independently of its caller.
#[cfg(feature = "llm-router")]
use crate::memory::{Memory, RouterHistoryWriteEvidence};
#[cfg(feature = "llm-router")]
use anyhow::Context;
#[cfg(feature = "llm-router")]
use std::sync::Arc;
#[cfg(feature = "llm-router")]
use std::time::Instant;
#[cfg(feature = "llm-router")]
use tokio_util::sync::CancellationToken;

#[cfg(feature = "llm-router")]
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

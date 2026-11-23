//! Mailbox-triggered run ownership and erase-time transcript cache removal.
use super::{HistoryEntry, SubAgentRun, SubAgentStatus};
use std::sync::Arc;
use tokio::sync::RwLock;
pub(super) fn retain(
    config: &crate::config::Config,
    input: Option<&crate::messaging::runtime::MessageInput>,
    runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    stop: Arc<dyn Fn() + Send + Sync>,
) -> anyhow::Result<Option<crate::messaging::runtime::MessageTurnGuard>> {
    let Some(input) = input else {
        return Ok(None);
    };
    let runtime = crate::messaging::runtime::MessagingRuntime::for_config(config)?;
    let weak = Arc::downgrade(runs);
    let alive = weak.clone();
    let session = input.message.to.session.clone();
    let handle = tokio::runtime::Handle::try_current()?;
    runtime.register_cache(
        input,
        format!("{:p}:{session}", Arc::as_ptr(runs)),
        Arc::new(move || alive.strong_count() > 0),
        Arc::new(move |lease, joined| {
            if let Some(runs) = weak.upgrade() {
                let session = session.clone();
                handle.spawn(async move {
                    let _lease = lease;
                    for completed in joined {
                        completed.cancelled().await;
                    }
                    let histories: Vec<Arc<RwLock<Vec<HistoryEntry>>>> = {
                        let mut runs = runs.write().await;
                        runs.iter_mut()
                            .filter(|run| run.session_scope_key == session)
                            .map(|run| {
                                run.task.clear();
                                run.status = SubAgentStatus::Failed("history_erased".into());
                                Arc::clone(&run.history)
                            })
                            .collect()
                    };
                    for history in histories {
                        history.write().await.clear();
                    }
                });
            }
        }),
    )?;
    Ok(Some(runtime.hold_turn(input, stop)?))
}

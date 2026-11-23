//! Display confirmed segment usage while a logical child task remains live.
use super::SubAgentRun;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub(super) fn observe(
    collector: Arc<crate::runtime::turn_terminal::TerminalUsageCollector>,
    runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run: String,
) -> tokio_util::sync::DropGuard {
    let cancellation = CancellationToken::new();
    let guard = cancellation.clone().drop_guard();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! {
                ()=cancellation.cancelled()=>break,
                _=interval.tick()=>{
                    let usage=collector.session_snapshot();
                    let mut runs=runs.write().await;
                    let Some(run)=runs.iter_mut().find(|row|row.id==run) else {break;};
                    for usage in usage.into_iter().filter(|usage|usage.settlement_class.is_some()) {
                        let Some(id)=usage.settlement_id.as_ref() else {continue;};
                        if !run.token_usage_records.iter().any(|known|known.settlement_id.as_ref()==Some(id)) {
                            run.token_usage_records.push(usage);
                        }
                    }
                }
            }
        }
    });
    guard
}

#![cfg(test)]

use super::super::DelegationExecutionTask;
use crate::tools::{ToolExecutionOutcome, ToolExecutionStatus};
use std::{io, sync::Arc};

#[derive(Clone, Default)]
struct LogSink(Arc<parking_lot::Mutex<Vec<u8>>>);

impl io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn emit_task_marker() {
    tracing::debug!("owned delegation task debug marker");
}

#[tokio::test]
async fn delegation_task_retains_the_source_subscriber_after_its_scope_ends() -> anyhow::Result<()> {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let subscriber = crate::config::secret_paths::fixture::capture_dispatch(subscriber);
    // The callsite first registers on a sibling with no default subscriber,
    // after the capture exists. It must still retain the capture's interest.
    std::thread::spawn(emit_task_marker)
        .join()
        .expect("sibling callsite registration");
    let task = tracing::dispatcher::with_default(&subscriber, || {
        DelegationExecutionTask::spawn(Box::pin(async {
            emit_task_marker();
            ToolExecutionOutcome {
                operation_id: "prx_observed_delegation".to_owned(),
                descriptor: None,
                decision: None,
                status: ToolExecutionStatus::Cancelled,
                model_content: String::new(),
                result: None,
                error: None,
                preparation: None,
                duration_ms: 0,
                replayed: false,
            }
        }))
    });
    assert_eq!(task.join().await?.status, ToolExecutionStatus::Cancelled);
    let logs = String::from_utf8_lossy(&sink.0.lock()).into_owned();
    assert!(logs.contains("owned delegation task debug marker"), "{logs}");
    Ok(())
}

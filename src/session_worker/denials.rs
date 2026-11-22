//! Content-free denial observations returned under the worker completion seal.
use crate::agent::{live_frames::LiveFrameSink, turn_contracts::ToolLoopEvent};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DENIALS: usize = 256;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDenial {
    pub tool: String,
    pub cause: crate::tools::ApprovalDenialCause,
}
#[derive(Default)]
pub(crate) struct DenialSink {
    denials: Mutex<Vec<WorkerDenial>>,
    overflow: AtomicBool,
}
impl DenialSink {
    pub(crate) fn take(&self) -> anyhow::Result<Vec<WorkerDenial>> {
        anyhow::ensure!(
            !self.overflow.load(Ordering::Relaxed),
            "worker denial inventory exceeded its bounded completion frame"
        );
        Ok(std::mem::take(&mut *self.denials.lock()))
    }
}
impl LiveFrameSink for DenialSink {
    fn thinking(&self) {}
    fn child_denial(&self, tool: &str, _child_run_id: &str) {
        self.record(tool);
    }
    fn emit(&self, event: &ToolLoopEvent) {
        if let ToolLoopEvent::ToolFinished {
            name,
            outcome:
                crate::agent::tool_evidence::ToolCallOutcome::Denied {
                    cause: Some(crate::tools::ApprovalDenialCause::NoResolver),
                },
            ..
        } = event
        {
            self.record(name);
        }
    }
}
impl DenialSink {
    fn record(&self, tool: &str) {
        let mut denials = self.denials.lock();
        if denials.len() == MAX_DENIALS || tool.len() > 64 {
            self.overflow.store(true, Ordering::Relaxed);
            return;
        }
        denials.push(WorkerDenial {
            tool: tool.to_owned(),
            cause: crate::tools::ApprovalDenialCause::NoResolver,
        });
    }
}

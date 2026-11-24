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
    pub cause: WorkerDenialCause,
}
/// Content-free outcomes only; this never carries an approval request or grant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkerDenialCause {
    Approval(crate::tools::ApprovalDenialCause),
    Refusal(crate::tools::execution::ToolCallRefusalReason),
}
impl WorkerDenial {
    pub(crate) const fn valid(&self) -> bool {
        self.tool.len() <= 64
    }
    pub(crate) fn rf_cause(&self) -> crate::security::approval_ledger::run_call::RfDenialCause {
        match self.cause {
            WorkerDenialCause::Approval(cause) => cause.into(),
            WorkerDenialCause::Refusal(cause) => cause.into(),
        }
    }
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
    // The parent emits the live awaiting-approval status of a relayed request;
    // only typed denials enter completion.
    fn awaiting_approval(&self, _tool: &str) {}
    fn child_denial(
        &self,
        tool: &str,
        _child_run_id: &str,
        cause: crate::security::approval_ledger::run_call::RfDenialCause,
    ) {
        self.record(tool, cause);
    }
    fn emit(&self, event: &ToolLoopEvent) {
        use crate::agent::tool_evidence::ToolCallOutcome;
        use crate::tools::execution::ToolCallRefusalReason;
        if let ToolLoopEvent::ToolFinished {
            name,
            outcome,
            refusal_cause,
            ..
        } = event
        {
            let cause = match outcome {
                ToolCallOutcome::Denied { cause: Some(cause) } => Some((*cause).into()),
                ToolCallOutcome::Refused => Some(refusal_cause.unwrap_or(ToolCallRefusalReason::PolicyDenied).into()),
                ToolCallOutcome::PlanOnly => Some(ToolCallRefusalReason::PlanOnly.into()),
                ToolCallOutcome::Error => Some(refusal_cause.unwrap_or(ToolCallRefusalReason::Failed).into()),
                ToolCallOutcome::Unknown => Some(refusal_cause.unwrap_or(ToolCallRefusalReason::Indeterminate).into()),
                _ => None,
            };
            if let Some(cause) = cause {
                self.record(name, cause);
            }
        }
    }
}

impl DenialSink {
    fn record(&self, tool: &str, cause: crate::security::approval_ledger::run_call::RfDenialCause) {
        let mut denials = self.denials.lock();
        if denials.len() == MAX_DENIALS || tool.len() > 64 {
            self.overflow.store(true, Ordering::Relaxed);
            return;
        }
        denials.push(WorkerDenial {
            tool: tool.to_owned(),
            cause: match cause {
                crate::security::approval_ledger::run_call::RfDenialCause::Approval(cause) => {
                    WorkerDenialCause::Approval(cause)
                }
                crate::security::approval_ledger::run_call::RfDenialCause::Refusal(cause) => {
                    WorkerDenialCause::Refusal(cause)
                }
            },
        });
    }
}

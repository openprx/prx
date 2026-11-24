//! Parent side of the approval relay for one process worker run.
//!
//! Nothing from the worker is trusted except `tool`, `tool_call_id` and
//! `arguments`. Everything else comes from what the parent sealed at launch:
//! the child execution context, the approval strategy and the surfaces.
use super::{
    ApprovalDecisionPayload, ApprovalRequestPayload, MAX_OUTSTANDING, WorkerApprovalOutcome,
};
use crate::agent::live_frames::LiveTurnRef;
use crate::session_worker::protocol::WorkerControlFrame;
use crate::tools::{
    ApprovalDenialCause, ToolApprovalDecision, ToolApprovalRequest, ToolDescriptor, ToolExecutionCommand,
    ToolExecutionContext, ToolExecutionService,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Everything the live relay needs, fixed at launch.
pub(crate) struct RelayParts {
    pub(crate) run_id: String,
    /// The sealed tool allowance; empty means the run inherits every tool.
    pub(crate) allowed_tools: Vec<String>,
    pub(crate) execution: ToolExecutionContext,
    pub(crate) service: Arc<ToolExecutionService>,
    pub(crate) live_turn: Option<LiveTurnRef>,
    pub(crate) controls: mpsc::Sender<WorkerControlFrame>,
    pub(crate) progress: Arc<crate::agent::idle::ProgressBeat>,
    /// Ends every wait of the run: pipe end, kill or termination request.
    pub(crate) cancel: CancellationToken,
}

struct Outstanding {
    cancel: CancellationToken,
    withdrawn: bool,
}

#[derive(Default)]
struct State {
    last_id: u64,
    outstanding: HashMap<u64, Outstanding>,
}

enum Admission {
    Accepted(ToolDescriptor, CancellationToken),
    Refused(ApprovalDenialCause, &'static str),
    /// A protocol violation; nothing is answered.
    Dropped,
}

pub(crate) struct ProcessApprovalRelay {
    parts: RelayParts,
    state: Mutex<State>,
    tasks: Mutex<JoinSet<()>>,
}

fn outcome_from(decision: ToolApprovalDecision) -> WorkerApprovalOutcome {
    match decision {
        ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant,
        } => WorkerApprovalOutcome::Approved { grant: runtime_grant },
        ToolApprovalDecision::Approved { .. } => WorkerApprovalOutcome::Denied {
            cause: ApprovalDenialCause::LedgerRefused,
            reason: "Approval did not grant the call (ledger_refused). Do not retry this call".to_owned(),
        },
        ToolApprovalDecision::Denied { cause, reason } => WorkerApprovalOutcome::Denied { cause, reason },
        ToolApprovalDecision::Cancelled { reason } => WorkerApprovalOutcome::Cancelled { reason },
    }
}

fn refusal(cause: ApprovalDenialCause, why: &str) -> WorkerApprovalOutcome {
    WorkerApprovalOutcome::Denied {
        cause,
        reason: cause.fixed_reason().map_or_else(
            || format!("Approval request was refused ({why}). Do not retry this call"),
            str::to_owned,
        ),
    }
}

impl ProcessApprovalRelay {
    pub(crate) fn new(parts: RelayParts) -> Arc<Self> {
        Arc::new(Self {
            parts,
            state: Mutex::new(State::default()),
            tasks: Mutex::new(JoinSet::new()),
        })
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock();
        // Reap finished request tasks so the set stays bounded by the outstanding limit.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
    }

    fn admit(&self, payload: &ApprovalRequestPayload) -> Admission {
        let mut state = self.state.lock();
        if payload.request_id <= state.last_id {
            return Admission::Dropped;
        }
        state.last_id = payload.request_id;
        if payload.validate().is_err() {
            return Admission::Refused(ApprovalDenialCause::LedgerRefused, "ledger_refused: invalid request");
        }
        if state.outstanding.len() >= MAX_OUTSTANDING {
            return Admission::Refused(ApprovalDenialCause::ConcurrentPending, "concurrent_pending");
        }
        let allowed = &self.parts.allowed_tools;
        if !allowed.is_empty() && !allowed.iter().any(|name| name == &payload.tool) {
            return Admission::Refused(ApprovalDenialCause::LedgerRefused, "ledger_refused: tool not allowed");
        }
        let service = &self.parts.service;
        let Some(descriptor) = service.current_descriptor(&payload.tool) else {
            return Admission::Refused(ApprovalDenialCause::LedgerRefused, "ledger_refused: unknown tool");
        };
        // The worker's own check is not evidence: only a call the parent's
        // policy still holds for approval is relayed.
        if !service.relayed_call_asks(&descriptor, &self.parts.execution) {
            return Admission::Refused(ApprovalDenialCause::LedgerRefused, "ledger_refused: no approval required");
        }
        let cancel = self.parts.cancel.child_token();
        state.outstanding.insert(
            payload.request_id,
            Outstanding {
                cancel: cancel.clone(),
                withdrawn: false,
            },
        );
        Admission::Accepted(descriptor, cancel)
    }

    /// One authenticated `ApprovalRequest` from the worker.
    pub(crate) fn request(self: &Arc<Self>, payload: ApprovalRequestPayload) {
        let request_id = payload.request_id;
        let input_sha256 = payload.input_sha256.clone();
        match self.admit(&payload) {
            Admission::Dropped => {
                tracing::warn!("session-worker approval request out of sequence; dropped");
            }
            Admission::Refused(cause, why) => {
                let relay = Arc::clone(self);
                self.spawn(async move {
                    relay
                        .send(request_id, input_sha256, refusal(cause, why))
                        .await;
                });
            }
            Admission::Accepted(descriptor, cancel) => {
                let relay = Arc::clone(self);
                self.spawn(async move { relay.run(payload, descriptor, cancel).await });
            }
        }
    }

    async fn run(self: Arc<Self>, payload: ApprovalRequestPayload, descriptor: ToolDescriptor, cancel: CancellationToken) {
        let ApprovalRequestPayload {
            request_id,
            tool_call_id,
            tool,
            arguments,
            input_sha256,
        } = payload;
        let request = ToolApprovalRequest {
            command: ToolExecutionCommand::new(tool, arguments).with_operation_id(tool_call_id),
            descriptor,
            context: self.parts.execution.clone(),
        };
        // The run's beat is parented on its caller's: while this holds the
        // pause, neither the child nor any waiting ancestor looks silent.
        let outcome = crate::agent::idle::scope_beat(Some(Arc::clone(&self.parts.progress)), async {
            let _pause = crate::agent::idle::pause_for_approval();
            tokio::select! {
                biased;
                () = cancel.cancelled() => WorkerApprovalOutcome::Cancelled {
                    reason: "approval cancelled".to_owned(),
                },
                decision = self.parts.service.resolve_relayed_approval(request) => outcome_from(decision),
            }
        })
        .await;
        let withdrawn = self
            .state
            .lock()
            .outstanding
            .remove(&request_id)
            .is_some_and(|entry| entry.withdrawn);
        if !withdrawn {
            self.send(request_id, input_sha256, outcome).await;
        }
    }

    async fn send(&self, request_id: u64, input_sha256: String, outcome: WorkerApprovalOutcome) {
        let frame = WorkerControlFrame::ApprovalDecision(ApprovalDecisionPayload {
            request_id,
            input_sha256,
            approval_id: None,
            outcome,
        });
        if self.parts.controls.send(frame).await.is_err() {
            tracing::debug!("session-worker control pipe closed before an approval decision");
        }
    }

    /// The worker stopped waiting. A withdraw only happens when a whole segment
    /// was cancelled, and pipe order puts it ahead of any request of the next
    /// segment, so it closes the run's approvals, not one call.
    pub(crate) fn withdraw(&self, request_id: u64) {
        tracing::debug!(request_id, "session-worker withdrew its approval requests");
        for entry in self.state.lock().outstanding.values_mut() {
            entry.withdrawn = true;
            entry.cancel.cancel();
        }
        self.close_run();
    }

    fn close_run(&self) {
        if let Some(live) = &self.parts.live_turn {
            live.0.close_run(&self.parts.run_id);
        }
    }

    /// The pipe ended, or the child is being cancelled or killed: every wait of
    /// the run ends as cancelled. The durable rows close with the run terminal.
    pub(crate) fn cancel_all(&self) {
        self.parts.cancel.cancel();
        self.close_run();
    }

    /// Run `main`, ending every approval wait as soon as `termination` fires.
    pub(crate) async fn guard<F: Future>(&self, termination: impl Future<Output = String>, main: F) -> F::Output {
        tokio::pin!(main);
        tokio::pin!(termination);
        let mut watching = true;
        loop {
            tokio::select! {
                output = &mut main => return output,
                _reason = &mut termination, if watching => {
                    watching = false;
                    self.cancel_all();
                }
            }
        }
    }
}

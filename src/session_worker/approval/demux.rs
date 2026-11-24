//! Parent-side split of the worker's output frames.
//!
//! Approval frames go to the relay. Every other frame goes on, unchanged, to
//! the mailbox actor that was already reading them, and that actor still
//! rejects any frame it does not know.
use super::relay::ProcessApprovalRelay;
use crate::session_worker::frames::WorkerFrameChannels;
use crate::session_worker::protocol::WorkerOutputFrame;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Capacity of the forwarding queue, the same as the mailbox output queue.
const FORWARD_CAPACITY: usize = 64;

/// Put the split in front of `frames.outputs`. The returned sender replaces the
/// original in the frames; the original moves into the forwarder.
///
/// The forwarder never waits on the relay: a request is handed to a task of its
/// own, so a long approval does not hold up mailbox frames behind it. When the
/// pipe ends (the reader dropped its sender, whether the worker finished, was
/// killed or the read failed) every approval of the run ends as cancelled.
pub(crate) fn attach(relay: &Arc<ProcessApprovalRelay>, frames: &mut WorkerFrameChannels) {
    let (forward, mut incoming) = mpsc::channel::<WorkerOutputFrame>(FORWARD_CAPACITY);
    let downstream = std::mem::replace(&mut frames.outputs, forward);
    let relay = Arc::clone(relay);
    tokio::spawn(async move {
        while let Some(frame) = incoming.recv().await {
            match frame {
                WorkerOutputFrame::ApprovalRequest(payload) => relay.request(payload),
                WorkerOutputFrame::ApprovalWithdraw { request_id } => relay.withdraw(request_id),
                other => {
                    if downstream.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
        relay.cancel_all();
    });
}

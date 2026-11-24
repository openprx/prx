//! Worker-side registry of approvals that wait for a parent decision.
//!
//! One process-wide registry, in the same shape as the mailbox publication
//! waiters: the stdin reader thread resolves a waiter, the approval client
//! awaits it. A decision is accepted only for an outstanding request id whose
//! digest matches the request it answers.
use super::WorkerApprovalOutcome;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::oneshot;

struct Waiter {
    input_sha256: String,
    reply: oneshot::Sender<WorkerApprovalOutcome>,
}

#[derive(Default)]
struct Registry {
    last_id: u64,
    closed: bool,
    pending: HashMap<u64, Waiter>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

/// Why a decision did not reach a waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// No outstanding request has that id.
    Unknown,
    /// The digest differs from the request's.
    DigestMismatch,
}

/// Register a waiter for a new request and return its id. `None` once the
/// parent pipe has ended: nothing can answer any more.
pub(crate) fn register(input_sha256: &str) -> Option<(u64, oneshot::Receiver<WorkerApprovalOutcome>)> {
    let mut registry = REGISTRY.lock();
    if registry.closed {
        return None;
    }
    let request_id = registry.last_id.checked_add(1)?;
    registry.last_id = request_id;
    let (reply, outcome) = oneshot::channel();
    registry.pending.insert(
        request_id,
        Waiter {
            input_sha256: input_sha256.to_owned(),
            reply,
        },
    );
    Some((request_id, outcome))
}

/// Deliver a verified decision. Anything but an outstanding id with a matching
/// digest is refused and leaves the waiter in place.
pub(crate) fn resolve(request_id: u64, input_sha256: &str, outcome: WorkerApprovalOutcome) -> Result<(), Rejected> {
    let mut registry = REGISTRY.lock();
    let waiter = registry.pending.get(&request_id).ok_or(Rejected::Unknown)?;
    if waiter.input_sha256 != input_sha256 {
        return Err(Rejected::DigestMismatch);
    }
    if let Some(waiter) = registry.pending.remove(&request_id) {
        // A closed receiver means the waiting future was dropped; its guard
        // already withdrew the request.
        let _ = waiter.reply.send(outcome);
    }
    Ok(())
}

/// Forget a request that nobody waits for any more. True when it was still
/// outstanding.
pub(crate) fn forget(request_id: u64) -> bool {
    REGISTRY.lock().pending.remove(&request_id).is_some()
}

/// End of input: the parent is gone or closed the pipe. Every waiter resolves
/// as cancelled, and later requests are refused at registration.
pub(crate) fn close_all() {
    let drained: Vec<Waiter> = {
        let mut registry = REGISTRY.lock();
        registry.closed = true;
        registry.pending.drain().map(|(_, waiter)| waiter).collect()
    };
    for waiter in drained {
        let _ = waiter.reply.send(WorkerApprovalOutcome::Cancelled {
            reason: "approval channel closed".to_owned(),
        });
    }
}

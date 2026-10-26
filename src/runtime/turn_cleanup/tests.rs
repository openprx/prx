#![cfg(test)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{CleanupScope, CleanupTicket, CleanupTracker, current_tracker};

#[derive(Clone, Copy)]
enum Exit {
    Success,
    Error,
    PreCancelled,
    PendingCancelled,
    ForcedDrop,
    UnpolledDrop,
}

struct DroppedWork {
    ticket: CleanupTicket,
    task_local_owner: Option<Arc<CleanupTracker>>,
    drain_started: bool,
    polls: usize,
}

/// The real ticket stays owned by the work until its destructor transfers it
/// to the test's explicit acknowledgement point. No worker/task is simulated.
struct TicketWork {
    ticket: Option<CleanupTicket>,
    dropped: Option<oneshot::Sender<DroppedWork>>,
    exit: Exit,
    polls: usize,
}

impl Future for TicketWork {
    type Output = anyhow::Result<&'static str>;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls += 1;
        let owner = self.ticket.as_ref().expect("owned work ticket").owner();
        assert!(current_tracker().is_some_and(|current| Arc::ptr_eq(&current, &owner)));
        match self.exit {
            Exit::Success => Poll::Ready(Ok("work completed")),
            Exit::Error => Poll::Ready(Err(anyhow::anyhow!("work failed"))),
            _ => Poll::Pending,
        }
    }
}

impl Drop for TicketWork {
    fn drop(&mut self) {
        let ticket = self.ticket.take().expect("work releases its ticket exactly once");
        let drain_started = ticket.owner().state.lock().closed;
        let event = DroppedWork {
            ticket,
            task_local_owner: current_tracker(),
            drain_started,
            polls: self.polls,
        };
        // A receiver dropped by a failed assertion releases the real ticket as
        // unconfirmed, preserving the production failure semantics.
        let _ = self.dropped.take().expect("single destructor event").send(event);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

async fn lifecycle(exit: Exit) {
    assert!(current_tracker().is_none());
    let parent = Arc::new(CleanupTracker::default());
    let cancellation = CancellationToken::new();
    let (ack_tx, mut ack_rx) = oneshot::channel();
    let scope = CleanupScope::new(Some(&parent), cancellation.clone())
        .expect("child scope")
        .with_acknowledgement(Some(ack_tx));
    let child = Arc::clone(&scope.tracker);
    let work_ticket = child
        .register(CancellationToken::new())
        .expect("real work registration");
    let (drop_tx, mut drop_rx) = oneshot::channel();
    let work = TicketWork {
        ticket: Some(work_ticket),
        dropped: Some(drop_tx),
        exit,
        polls: 0,
    };
    assert_eq!(parent.state.lock().pending.len(), 1);
    if matches!(exit, Exit::PreCancelled) {
        cancellation.cancel();
    }
    let mut running = Box::pin(scope.run(work));
    if !matches!(exit, Exit::UnpolledDrop) {
        assert!(poll_once(running.as_mut()).is_pending());
        assert!(
            current_tracker().is_none(),
            "scope must restore caller task-local state"
        );
    }
    if matches!(exit, Exit::PendingCancelled) {
        assert!(matches!(drop_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        cancellation.cancel();
        assert!(poll_once(running.as_mut()).is_pending());
    }
    let forced = matches!(exit, Exit::ForcedDrop | Exit::UnpolledDrop);
    let running = if forced {
        if matches!(exit, Exit::ForcedDrop) {
            assert!(matches!(drop_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        }
        drop(running);
        None
    } else {
        Some(running)
    };
    let dropped = drop_rx
        .try_recv()
        .expect("work must drop before waiting for its ticket");
    assert!(
        !dropped.drain_started,
        "work destructor must precede child drain admission close"
    );
    if matches!(exit, Exit::UnpolledDrop) {
        assert!(
            dropped.task_local_owner.is_none(),
            "unpolled run never constructed TaskLocalFuture"
        );
        assert_eq!(dropped.polls, 0);
    } else {
        assert!(
            dropped
                .task_local_owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &child)),
            "owned TaskLocalFuture must drop work under the actual child tracker"
        );
        assert_eq!(dropped.polls, usize::from(!matches!(exit, Exit::PreCancelled)));
    }
    assert!(current_tracker().is_none());
    assert!(matches!(ack_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    assert_eq!(
        parent.state.lock().pending.len(),
        1,
        "parent cannot confirm before child ticket acknowledgement"
    );
    assert!(!parent.state.lock().failed);

    // Drive the real parent drain too: it must remain pending until the
    // destructor-transferred work ticket has actually been acknowledged.
    let mut parent_drain = Box::pin(parent.drain(tokio::time::Instant::now() + Duration::from_secs(5)));
    assert!(poll_once(parent_drain.as_mut()).is_pending());
    dropped.ticket.finish(true);
    if let Some(running) = running {
        let result = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("scope completion cannot hang after acknowledgement");
        match exit {
            Exit::Success => assert_eq!(result.expect("successful work"), "work completed"),
            Exit::Error => assert_eq!(result.expect_err("work error retained").to_string(), "work failed"),
            Exit::PreCancelled | Exit::PendingCancelled => {
                assert_eq!(
                    result.expect_err("cancelled work").to_string(),
                    "runtime work cancelled"
                );
            }
            _ => panic!("forced drop must not retain a running future"),
        }
    }
    tokio::time::timeout(Duration::from_secs(5), ack_rx)
        .await
        .expect("cleanup acknowledgement cannot hang")
        .expect("acknowledgement sender retained")
        .expect("acknowledged ticket confirms cleanup even when work failed");
    tokio::time::timeout(Duration::from_secs(5), parent_drain)
        .await
        .expect("parent drain cannot hang")
        .expect("parent receives confirmed cleanup");
    assert!(parent.state.lock().pending.is_empty());
    assert!(!parent.state.lock().failed);
    assert!(child.state.lock().pending.is_empty());
    assert!(child.state.lock().closed);
    assert!(current_tracker().is_none());
}

#[tokio::test]
async fn successful_work_drops_before_drain_and_parent_acknowledgement() {
    lifecycle(Exit::Success).await;
}

#[tokio::test]
async fn failed_work_retains_error_but_confirms_acknowledged_cleanup() {
    lifecycle(Exit::Error).await;
}

#[tokio::test]
async fn pre_cancelled_work_drops_unpolled_under_child_task_local() {
    lifecycle(Exit::PreCancelled).await;
}

#[tokio::test]
async fn pending_work_cancellation_drops_under_child_before_acknowledgement() {
    lifecycle(Exit::PendingCancelled).await;
}

#[tokio::test]
async fn forced_drop_after_first_poll_keeps_parent_pending_until_real_ticket_finishes() {
    lifecycle(Exit::ForcedDrop).await;
}

#[tokio::test]
async fn unpolled_scope_drop_has_no_task_local_and_still_confirms_parent_after_drain() {
    lifecycle(Exit::UnpolledDrop).await;
}

#[tokio::test]
async fn admission_owner_remains_open_for_later_child_work_and_final_ack() {
    let parent = Arc::new(CleanupTracker::default());
    let main_cancel = CancellationToken::new();
    let (ack_tx, mut ack_rx) = oneshot::channel();
    let scope = CleanupScope::new(Some(&parent), main_cancel.child_token())
        .expect("real owner")
        .with_acknowledgement(Some(ack_tx));
    let retained = scope
        .scope_admission(async { Ok(current_tracker().expect("admission owner installed")) })
        .await
        .expect("admission succeeds");
    assert!(!retained.state.lock().closed);
    assert!(current_tracker().is_none());
    let (ticket_tx, mut ticket_rx) = oneshot::channel();
    let mut running = Box::pin(scope.run(async {
        let current = current_tracker().expect("later work owner");
        assert!(Arc::ptr_eq(&current, &retained));
        // A real later child must still register beneath the retained owner;
        // closing a separate admission scope would reject this operation.
        let child =
            CleanupScope::new(Some(&retained), main_cancel.child_token()).expect("later child admission remains open");
        let ticket = child
            .scope_admission(async {
                current_tracker()
                    .expect("child owner")
                    .register(CancellationToken::new())
            })
            .await
            .expect("real later child work ticket");
        assert!(ticket_tx.send(ticket).is_ok());
        child.run(async { Ok(()) }).await
    }));
    assert!(poll_once(running.as_mut()).is_pending());
    let ticket = ticket_rx.try_recv().expect("later child really ran");
    assert!(matches!(ack_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    assert!(!parent.state.lock().pending.is_empty());
    ticket.finish(true);
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("bounded real child drain")
        .expect("completed work");
    tokio::time::timeout(Duration::from_secs(5), ack_rx)
        .await
        .expect("bounded ack")
        .expect("sender retained")
        .expect("confirmed final cleanup");
    assert!(
        !main_cancel.is_cancelled(),
        "child cleanup must not cancel the main token"
    );
    parent
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect("parent drained");
}

#[tokio::test]
async fn cancelled_admission_retains_failed_ticket_until_owner_drop_ack() {
    let parent = Arc::new(CleanupTracker::default());
    let cancel = CancellationToken::new();
    let (ack_tx, mut ack_rx) = oneshot::channel();
    let scope = CleanupScope::new(Some(&parent), cancel.child_token())
        .expect("real owner")
        .with_acknowledgement(Some(ack_tx));
    let retained = scope
        .scope_admission(async { Ok(current_tracker().expect("admission owner")) })
        .await
        .expect("capture actual owner");
    let ticket = retained.register(CancellationToken::new()).expect("real pending work");
    let (drop_tx, mut drop_rx) = oneshot::channel();
    let mut admission = Box::pin(scope.scope_admission(TicketWork {
        ticket: Some(ticket),
        dropped: Some(drop_tx),
        exit: Exit::PendingCancelled,
        polls: 0,
    }));
    assert!(poll_once(admission.as_mut()).is_pending());
    cancel.cancel();
    assert!(matches!(poll_once(admission.as_mut()), Poll::Ready(Err(_))));
    let dropped = drop_rx.try_recv().expect("cancel dropped actual work before drain");
    assert_eq!(dropped.polls, 1);
    assert!(!dropped.drain_started);
    assert!(!retained.state.lock().closed, "borrowed stage cannot close the owner");
    drop(admission);
    drop(scope);
    assert!(matches!(ack_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    assert!(!parent.state.lock().pending.is_empty());
    dropped.ticket.finish(false);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), ack_rx)
            .await
            .expect("bounded failed drain ack")
            .expect("sender retained")
            .is_err()
    );
    assert!(
        parent
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .is_err()
    );
    assert!(current_tracker().is_none());
}

#[tokio::test]
async fn incomplete_cleanup_keeps_the_original_work_failure() {
    let scope = CleanupScope::new(None, CancellationToken::new()).unwrap();
    let error = scope
        .run::<()>(async {
            let ticket = current_tracker().unwrap().register(CancellationToken::new()).unwrap();
            ticket.finish(false);
            anyhow::bail!("specific ingress failure")
        })
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<super::CleanupIncomplete>().is_some());
    let chain = format!("{error:#}");
    assert!(chain.contains("specific ingress failure"));
    assert!(chain.contains("runtime terminal cleanup has unconfirmed failures"));
}

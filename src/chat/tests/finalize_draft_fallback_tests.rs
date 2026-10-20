#![cfg(test)]

//! Tests for the P1-4 idempotency contract: when `finalize_draft` fails
//! for an active draft, the chat loop must NOT re-send the full response
//! (the streamed deltas already delivered it). The "no draft" path is a
//! genuine first-send and is still allowed to call `send`.
use super::*;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Pure decision test: with an active draft, finalize failure must NOT trigger
/// a resend (would duplicate what the user already saw via stream).
#[test]
fn finalize_failure_with_active_draft_does_not_resend() {
    assert!(
        !should_resend_on_finalize_failure(true),
        "active-draft finalize failure must be idempotent (no resend)"
    );
}

/// Pure decision test: with no active draft, the caller still uses `send`
/// directly — that path is the first send, not a fallback resend.
/// `should_resend_on_finalize_failure(false)` returning `true` simply
/// documents that the "no draft" branch's normal send is allowed.
#[test]
fn no_active_draft_path_allows_send() {
    assert!(
        should_resend_on_finalize_failure(false),
        "no-draft path must allow the normal send (this is a first send, not a duplicate)"
    );
}

/// Mock channel that lets us script `finalize_draft` to fail and counts
/// every method call so we can assert no fallback resend occurs.
struct MockChannel {
    finalize_should_fail: bool,
    send_calls: AtomicUsize,
    finalize_calls: AtomicUsize,
}

impl MockChannel {
    fn new(finalize_should_fail: bool) -> Self {
        Self {
            finalize_should_fail,
            send_calls: AtomicUsize::new(0),
            finalize_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Channel for MockChannel {
    fn name(&self) -> &str {
        "mock"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        self.send_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    fn supports_draft_updates(&self) -> bool {
        true
    }

    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(Some("draft-1".to_string()))
    }

    async fn finalize_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
        self.finalize_calls.fetch_add(1, Ordering::SeqCst);
        if self.finalize_should_fail {
            Err(anyhow::anyhow!("simulated finalize failure"))
        } else {
            Ok(())
        }
    }
}

/// Replicates the production fallback control flow against a mock channel.
/// On finalize failure with an active draft, `send` must NOT be invoked.
async fn run_finalize_path(channel: Arc<MockChannel>, draft_id: Option<String>, display_response: &str) {
    if let Some(ref d_id) = draft_id {
        if let Err(e) = channel.finalize_draft("user", d_id, display_response).await {
            if should_resend_on_finalize_failure(true) {
                let _ = channel.send(&SendMessage::new(display_response, "user")).await;
            } else {
                tracing::warn!(error = %e, "finalize_draft failed; suppressing resend");
            }
        }
    } else {
        // First-send path (no draft was ever created)
        let _ = channel.send(&SendMessage::new(display_response, "user")).await;
    }
}

#[tokio::test]
async fn finalize_failure_with_active_draft_suppresses_send() {
    let ch = Arc::new(MockChannel::new(true));
    run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

    assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1, "finalize must run once");
    assert_eq!(
        ch.send_calls.load(Ordering::SeqCst),
        0,
        "finalize failure must NOT trigger a resend when a draft was active"
    );
}

#[tokio::test]
async fn finalize_success_does_not_resend() {
    let ch = Arc::new(MockChannel::new(false));
    run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

    assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ch.send_calls.load(Ordering::SeqCst),
        0,
        "successful finalize must not produce any extra send"
    );
}

#[tokio::test]
async fn no_draft_path_sends_once() {
    let ch = Arc::new(MockChannel::new(false));
    run_finalize_path(Arc::clone(&ch), None, "hello world").await;

    assert_eq!(
        ch.finalize_calls.load(Ordering::SeqCst),
        0,
        "no-draft path must not call finalize"
    );
    assert_eq!(
        ch.send_calls.load(Ordering::SeqCst),
        1,
        "no-draft path must send the response exactly once"
    );
}

/// Regression guard: two consecutive turns where finalize fails on both
/// must produce zero extra `send` calls (the previous buggy behavior
/// resulted in 2 duplicate messages).
#[tokio::test]
async fn repeated_finalize_failures_never_resend() {
    let ch = Arc::new(MockChannel::new(true));
    run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "turn 1").await;
    run_finalize_path(Arc::clone(&ch), Some("draft-2".to_string()), "turn 2").await;

    assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        ch.send_calls.load(Ordering::SeqCst),
        0,
        "repeated finalize failures must remain idempotent (no resends)"
    );
}

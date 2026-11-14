//! Hand-over of the next terminal input line to a pending approval prompt.
//!
//! The line-mode chat reads stdin on a dedicated thread for its whole life.
//! When stdin is the terminal, an approval prompt reading the same terminal
//! would race that thread for the operator's answer. Instead the prompt claims
//! the next line here and the input thread delivers it to the prompt rather
//! than to the chat.

use parking_lot::Mutex;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

static CLAIM: LazyLock<Mutex<Option<tokio::sync::oneshot::Sender<String>>>> = LazyLock::new(|| Mutex::new(None));
static TTY_INPUT_LOOP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while the chat input thread reads lines from a terminal stdin.
pub(crate) fn tty_input_loop_active() -> bool {
    TTY_INPUT_LOOP_ACTIVE.load(Ordering::Acquire)
}

/// Marks the terminal input thread as running for its lifetime.
pub(super) struct TtyInputLoopGuard;

impl TtyInputLoopGuard {
    pub(super) fn enter() -> Self {
        TTY_INPUT_LOOP_ACTIVE.store(true, Ordering::Release);
        Self
    }
}

impl Drop for TtyInputLoopGuard {
    fn drop(&mut self) {
        TTY_INPUT_LOOP_ACTIVE.store(false, Ordering::Release);
        // A prompt still waiting gets EOF (its sender is dropped).
        CLAIM.lock().take();
    }
}

/// The next input line, claimed by one prompt. Dropping it releases the claim.
pub(crate) struct ClaimedLine {
    receiver: tokio::sync::oneshot::Receiver<String>,
}

impl ClaimedLine {
    /// Wait for the line; `None` when the input thread ended (EOF).
    pub(crate) async fn line(&mut self) -> Option<String> {
        (&mut self.receiver).await.ok()
    }
}

impl Drop for ClaimedLine {
    fn drop(&mut self) {
        // Close first so the slot's sender reports closed for this claim.
        self.receiver.close();
        let mut claim = CLAIM.lock();
        if claim.as_ref().is_some_and(tokio::sync::oneshot::Sender::is_closed) {
            claim.take();
        }
    }
}

/// Claim the next line typed on the terminal stdin. `None` when another
/// prompt already holds the claim.
pub(crate) fn claim_next_line() -> Option<ClaimedLine> {
    let mut claim = CLAIM.lock();
    if claim.as_ref().is_some_and(|sender| !sender.is_closed()) {
        return None;
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    *claim = Some(sender);
    Some(ClaimedLine { receiver })
}

/// Called by the input thread for every line it reads: returns the line back
/// when no live prompt claimed it, so the chat receives it.
pub(super) fn deliver(line: String) -> Option<String> {
    let sender = CLAIM.lock().take();
    match sender {
        Some(sender) => sender.send(line).err(),
        None => Some(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process-global slot: the cases run in sequence in one test.
    #[tokio::test]
    async fn claimed_line_goes_to_the_prompt_and_unclaimed_lines_to_the_chat() {
        assert_eq!(deliver("hello".to_owned()).as_deref(), Some("hello"));

        let mut claimed = claim_next_line().expect("first claim");
        assert!(
            claim_next_line().is_none(),
            "a second prompt cannot claim the same line"
        );
        assert_eq!(deliver("y".to_owned()), None, "the claimed line is consumed");
        assert_eq!(claimed.line().await.as_deref(), Some("y"));
        drop(claimed);

        let abandoned = claim_next_line().expect("claim after release");
        drop(abandoned);
        assert_eq!(
            deliver("next".to_owned()).as_deref(),
            Some("next"),
            "an abandoned claim returns the line to the chat"
        );
    }
}

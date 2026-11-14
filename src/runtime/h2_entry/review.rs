//! Local control authority of a directly launched controlled Gateway.
//!
//! A Gateway lifecycle mutation (publish, annotate, rollback, retire) runs
//! only after the original OS terminal owner reviews that exact call. The
//! review is an in-process request/answer between the executing call and the
//! local control reader; it is independent of any decision approval.
use super::authority::EntryCall;
use super::{ControlledH2Profile, VerifiedEntryOrigin};
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

/// How long a lifecycle call waits for the local owner's answer.
const REVIEW_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
/// Bound on concurrently waiting reviews of one controlled launch.
const MAX_PENDING_REVIEWS: usize = 16;

/// A call waiting for the local owner, keyed by its review id in the profile.
pub(super) struct PendingLocalReview {
    call_digest: String,
    reply: tokio::sync::oneshot::Sender<bool>,
}

pub(crate) struct LocalControlAuthority {
    profile: Arc<ControlledH2Profile>,
}
impl LocalControlAuthority {
    pub(super) fn from_producer(profile: Arc<ControlledH2Profile>) -> Result<Self> {
        ensure!(
            profile.kind == super::EntryKind::Gateway,
            "local review requires direct Gateway launch"
        );
        profile.ensure_live()?;
        Ok(Self { profile })
    }
    pub(crate) const fn profile(&self) -> &Arc<ControlledH2Profile> {
        &self.profile
    }
    /// Handles one bounded review command typed in the original terminal:
    /// `approve REVIEW CALL_DIGEST` or `deny REVIEW`.
    pub(crate) fn review_command(&self, fields: &[&str]) -> Result<String> {
        match fields {
            ["approve", id, digest] => self.answer(id, Some(digest), true),
            ["deny", id] => self.answer(id, None, false),
            _ => anyhow::bail!("invalid bounded local-control command"),
        }
    }
    fn answer(&self, id: &str, digest: Option<&str>, approved: bool) -> Result<String> {
        self.profile.ensure_live()?;
        let pending = {
            let mut reviews = self.profile.reviews.lock();
            let entry = reviews.get(id).context("no such pending local review")?;
            if let Some(digest) = digest {
                ensure!(
                    entry.call_digest == digest,
                    "local review digest differs from the waiting call"
                );
            }
            reviews.remove(id).context("no such pending local review")?
        };
        pending
            .reply
            .send(approved)
            .map_err(|_| anyhow::anyhow!("the reviewed call is no longer waiting"))?;
        Ok(format!(
            "local review {id} {}",
            if approved { "approved" } else { "denied" }
        ))
    }
    /// Ends every waiting review as not approved (reader exit or failure).
    pub(crate) fn cancel_reviews(&self) {
        self.profile.reviews.lock().clear();
    }
}

/// Removes an unanswered review when the waiting call ends for any reason.
struct ReviewTicket<'a> {
    profile: &'a ControlledH2Profile,
    id: String,
}
impl Drop for ReviewTicket<'_> {
    fn drop(&mut self) {
        self.profile.reviews.lock().remove(&self.id);
    }
}

/// The local owner's approval of one exact executing call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalOwnerReview {
    origin: Arc<VerifiedEntryOrigin>,
    args_sha256: String,
    call_digest: String,
    capability_generation: String,
    catalog_generation: String,
}
impl LocalOwnerReview {
    /// Asks the original terminal owner about `call` and waits for the answer.
    /// A denial, timeout, reader exit or revoked launch refuses the call.
    pub(super) async fn request(origin: &Arc<VerifiedEntryOrigin>, call: &EntryCall) -> Result<Self> {
        origin.ensure_live()?;
        let profile = origin.profile();
        ensure!(
            profile.kind == super::EntryKind::Gateway && profile.has_reader(),
            "local lifecycle owner review requires the original Gateway terminal"
        );
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let (reply, answer) = tokio::sync::oneshot::channel();
        {
            let mut reviews = profile.reviews.lock();
            ensure!(
                reviews.len() < MAX_PENDING_REVIEWS,
                "too many pending local lifecycle owner reviews"
            );
            reviews.insert(
                id.clone(),
                PendingLocalReview {
                    call_digest: call.call_digest.clone(),
                    reply,
                },
            );
        }
        let _ticket = ReviewTicket {
            profile: profile.as_ref(),
            id: id.clone(),
        };
        {
            use std::io::Write as _;
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "Local owner review {id}: {:?} session {} run {} args {}; approve {id} {} | deny {id}",
                call.action, call.session, call.run, call.args_sha256, call.call_digest
            )
            .and_then(|()| stdout.flush())
            .context("local lifecycle owner review prompt could not be shown")?;
        }
        let approved = tokio::time::timeout(REVIEW_WINDOW, answer)
            .await
            .map_err(|_| anyhow::anyhow!("local lifecycle owner review timed out"))?
            .map_err(|_| anyhow::anyhow!("local lifecycle owner review ended without an answer"))?;
        ensure!(approved, "local lifecycle owner denied this call");
        origin.ensure_live()?;
        Ok(Self {
            origin: Arc::clone(origin),
            args_sha256: call.args_sha256.clone(),
            call_digest: call.call_digest.clone(),
            capability_generation: call.capability_generation.clone(),
            catalog_generation: call.catalog_generation.clone(),
        })
    }
    pub(super) fn verify(&self, origin: &VerifiedEntryOrigin, call: &EntryCall) -> Result<()> {
        origin.ensure_live()?;
        ensure!(
            self.origin.as_ref() == origin
                && self.args_sha256 == call.args_sha256
                && self.call_digest == call.call_digest
                && self.capability_generation == call.capability_generation
                && self.catalog_generation == call.catalog_generation,
            "local-owner review differs from the executing call"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;

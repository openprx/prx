//! Non-executing, source-bound argument collection. No proof grants dispatch.
use super::branch::CapabilityRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalReviewAdapter {
    ChatOwnerReview,
    ConsoleOwnerReview,
}

/// Issued only after exact-source eligibility and all three gate passes.
/// This evidence cannot be deserialized into live authority.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DeferredArgumentProof {
    capability: CapabilityRef,
    adapter: ApprovalReviewAdapter,
    state_fingerprint: String,
}
impl std::fmt::Debug for DeferredArgumentProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredArgumentProof")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}
impl DeferredArgumentProof {
    pub(super) const fn issue(
        capability: CapabilityRef,
        adapter: ApprovalReviewAdapter,
        state_fingerprint: String,
    ) -> Self {
        Self {
            capability,
            adapter,
            state_fingerprint,
        }
    }
    pub(crate) const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }
    pub(crate) const fn adapter(&self) -> ApprovalReviewAdapter {
        self.adapter
    }
    pub(crate) fn state_fingerprint(&self) -> &str {
        &self.state_fingerprint
    }
}

/// Validation of persisted evidence is not an authority constructor.
pub(crate) fn valid_projection(gates: &serde_json::Value, state: &str, generation: &str) -> bool {
    let Some(proof) = gates.get("approval_collection") else {
        return false;
    };
    let hex32 = |value: Option<&str>| {
        value.is_some_and(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    proof.get("version").and_then(serde_json::Value::as_u64) == Some(1)
        && proof.get("state_fingerprint").and_then(serde_json::Value::as_str) == Some(state)
        && matches!(
            proof.get("adapter").and_then(serde_json::Value::as_str),
            Some("chat_owner_review" | "console_owner_review")
        )
        && proof.get("capability").is_some_and(|capability| {
            capability
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty())
                && hex32(capability.get("schema").and_then(serde_json::Value::as_str))
                && capability.get("generation").and_then(serde_json::Value::as_str) == Some(generation)
        })
}

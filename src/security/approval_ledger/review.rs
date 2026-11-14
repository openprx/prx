// Non-executing, source-bound argument collection. No proof grants dispatch
// (moved from `causal_tree::approval_collection`, R15).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalReviewAdapter {
    ChatOwnerReview,
    ConsoleOwnerReview,
}

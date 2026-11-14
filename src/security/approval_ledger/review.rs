// Non-executing, source-bound argument collection. No proof grants dispatch
// (R15).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalReviewAdapter {
    ChatOwnerReview,
    ConsoleOwnerReview,
}

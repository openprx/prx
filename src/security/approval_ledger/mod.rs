//! Durable run-keyed approval lifecycle over the existing MemoryFabric event
//! ledger. Runtime admission checks remain in the adapter; persisted
//! authentication, review arbitration and metadata reads belong here.
pub(crate) mod keys;
mod review;
pub(crate) mod run_call;
pub(crate) mod strategy;
pub(crate) mod view;
pub(crate) use review::ApprovalReviewAdapter;
#[cfg(test)]
mod tests;

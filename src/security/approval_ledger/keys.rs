//! Key namespaces are independent of the persisted payloads.
use crate::causal_tree::ids::DecisionId;
use sha2::{Digest, Sha256};

pub(crate) trait ApprovalAuthority {
    fn namespace(&self) -> String;
    fn pending_key(&self) -> String {
        format!("{}:pending", self.namespace())
    }
    fn review_key(&self) -> String {
        format!("{}:review", self.namespace())
    }
    fn issued_key(&self, grant_id: &str) -> String;
    fn resolution_key(&self, op_id: &str) -> String {
        let operation = hex::encode(Sha256::digest(op_id.as_bytes()));
        format!("{}:{operation}:resolution", self.namespace())
    }
    fn resume_key(&self, source: &str) -> String {
        format!("{}:resume:{source}", self.namespace())
    }
}

pub(crate) struct DecisionAuthority(pub(crate) DecisionId);
impl ApprovalAuthority for DecisionAuthority {
    fn namespace(&self) -> String {
        format!("decision-approval:{}", self.0)
    }
    fn issued_key(&self, grant_id: &str) -> String {
        format!("decision-approval:{grant_id}:issued")
    }
}

pub(crate) fn decision_resolution_key(
    binding: &crate::acl::approval_grant::DecisionApprovalBinding,
    op_id: &str,
) -> String {
    let operation = hex::encode(Sha256::digest(op_id.as_bytes()));
    format!("decision-approval:{}:{operation}:resolution", binding.decision_id)
}

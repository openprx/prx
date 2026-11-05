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

/// This free function and `DecisionAuthority::resolution_key` must agree.
pub(crate) fn decision_resolution_key(
    binding: &crate::acl::approval_grant::DecisionApprovalBinding,
    op_id: &str,
) -> String {
    let operation = hex::encode(Sha256::digest(op_id.as_bytes()));
    format!("decision-approval:{}:{operation}:resolution", binding.decision_id)
}

/// Identity of one exact call, independent of any causal decision.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunCallAuthority {
    pub(crate) run_id: String,
    pub(crate) call_digest: [u8; 32],
}

impl ApprovalAuthority for RunCallAuthority {
    fn namespace(&self) -> String {
        // Hash the run component so delimiters in an untrusted run id cannot
        // cross the fixed key boundaries (including resume suffixes).
        let run = hex::encode(Sha256::digest(self.run_id.as_bytes()));
        format!("call-approval:{run}:{}", hex::encode(self.call_digest))
    }
    fn issued_key(&self, _grant_id: &str) -> String {
        format!("{}:issued", self.namespace())
    }
    fn resolution_key(&self, _op_id: &str) -> String {
        format!("{}:resolution", self.namespace())
    }
}

impl RunCallAuthority {
    pub(crate) fn call_id(&self) -> String {
        format!(
            "call.{}.{}",
            hex::encode(self.run_id.as_bytes()),
            hex::encode(self.call_digest)
        )
    }
    pub(crate) fn parse_call_id(id: &str) -> anyhow::Result<Self> {
        let mut parts = id.split('.');
        anyhow::ensure!(parts.next() == Some("call"), "invalid call id");
        let run_id = String::from_utf8(hex::decode(
            parts.next().ok_or_else(|| anyhow::anyhow!("invalid call id"))?,
        )?)?;
        let call_digest = hex::decode(parts.next().ok_or_else(|| anyhow::anyhow!("invalid call id"))?)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid call digest"))?;
        anyhow::ensure!(!run_id.is_empty() && parts.next().is_none(), "invalid call id");
        Ok(Self { run_id, call_digest })
    }
}

use super::keys::{ApprovalAuthority, DecisionAuthority};
use crate::causal_tree::ids::DecisionId;
use sha2::{Digest, Sha256};

#[test]
fn decision_keys_are_byte_identical_to_pre_extraction() {
    let id = DecisionId::parse("00000000-0000-0000-0000-000000000001").unwrap();
    let authority = DecisionAuthority(id);
    assert_eq!(authority.pending_key(), format!("decision-approval:{id}:pending"));
    assert_eq!(authority.review_key(), format!("decision-approval:{id}:review"));
    assert_eq!(
        authority.issued_key("grant_fixture"),
        "decision-approval:grant_fixture:issued"
    );
    assert_eq!(
        authority.resolution_key("fixture operation"),
        format!(
            "decision-approval:{id}:{}:resolution",
            hex::encode(Sha256::digest(b"fixture operation"))
        )
    );
    assert_eq!(
        authority.resume_key("source_fixture"),
        format!("decision-approval:{id}:resume:source_fixture")
    );
}

// Captured baseline field order, optional-field omission and payload shapes.
// These fixtures carry no execution authority or real identity data.
#[test]
fn pending_and_review_payloads_keep_pre_extraction_bytes() {
    const PENDING: &str = r#"{"record":{"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"call_digest":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"source_event_id":"prx_source","source_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"run_id":"prx_run","decision_digests":[["00000000-0000-0000-0000-000000000001","prx_digest"]],"policy_version":"prx_policy","capabilities":[["prx_capability",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]]],"op_ids":["prx_operation"],"effect":"local_write","expires_at":"2030-01-01T00:00:00Z"},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    const REVIEW: &str = r#"{"record":{"pending_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"approved":false,"grants":[]},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    let pending: super::pending::SignedPending = serde_json::from_str(PENDING).unwrap();
    assert_eq!(serde_json::to_string(&pending).unwrap(), PENDING);
    let review: super::review::SignedReview = serde_json::from_str(REVIEW).unwrap();
    assert_eq!(serde_json::to_string(&review).unwrap(), REVIEW);
}

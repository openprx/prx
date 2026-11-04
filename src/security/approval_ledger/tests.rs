use super::keys::{ApprovalAuthority, DecisionAuthority};
use crate::causal_tree::ids::DecisionId;

#[test]
fn decision_keys_are_byte_identical_to_pre_extraction() {
    let id = DecisionId::parse("00000000-0000-0000-0000-000000000001").unwrap();
    let authority = DecisionAuthority(id);
    assert_eq!(
        authority.pending_key(),
        "decision-approval:00000000-0000-0000-0000-000000000001:pending"
    );
    assert_eq!(
        authority.review_key(),
        "decision-approval:00000000-0000-0000-0000-000000000001:review"
    );
    assert_eq!(
        authority.issued_key("grant_fixture"),
        "decision-approval:grant_fixture:issued"
    );
    assert_eq!(
        authority.resolution_key("fixture operation"),
        "decision-approval:00000000-0000-0000-0000-000000000001:ad3f377ae32bf3fca6b60175d758bb069396189058c30fbee26b15b9036e2f2a:resolution"
    );
    assert_eq!(
        authority.resume_key("source_fixture"),
        "decision-approval:00000000-0000-0000-0000-000000000001:resume:source_fixture"
    );
}

// Captured baseline field order, optional-field omission and payload shapes.
// These fixtures carry no execution authority or real identity data.
#[test]
fn pending_and_review_payloads_keep_pre_extraction_bytes() {
    const PENDING: &str = r#"{"record":{"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"call_digest":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"source_event_id":"prx_source","source_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"run_id":"prx_run","decision_digests":[["00000000-0000-0000-0000-000000000001","prx_digest"]],"policy_version":"prx_policy","capabilities":[["prx_capability",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]]],"op_ids":["prx_operation"],"effect":"local_write","expires_at":"2030-01-01T00:00:00Z"},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    const REVIEW: &str = r#"{"record":{"pending_mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"approved":false,"grants":[]},"mac":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
    // Synthetic populated wire fixtures pin optional fields and nonzero MAC bytes.
    // They are serialization data, never authenticated approval authority.
    const POPULATED_PENDING: &str = r#"{"record":{"h2_origin":{"version":1,"authority_expires_at":"2030-01-01T00:00:00Z","kind":"gateway","boot":"prx_boot","profile":"prx_profile","actor":"prx_owner","target_owner":"prx_owner","workspace":"prx_workspace","session":"prx_session","run":"prx_run","event":"prx_source","input":"prx_input","routing":"prx_routing","sequence":7},"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]},"source_event_id":"prx_source","source_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"run_id":"prx_run","decision_digests":[["00000000-0000-0000-0000-000000000001","prx_digest"]],"policy_version":"prx_policy","capabilities":[["prx_capability",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]]],"op_ids":["prx_operation"],"effect":"local_write","risk":"High","schema_dialect":"gemini","sealed_call":{"version":1,"nonce":[1,2,3,4,5,6,7,8,9,10,11,12],"ciphertext":"01020304"},"expires_at":"2030-01-01T00:00:00Z"},"mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32]}"#;
    const POPULATED_REVIEW: &str = r#"{"record":{"local_owner":{"body":{"version":1,"boot":"prx_boot","profile":"prx_profile","original":{"version":1,"authority_expires_at":"2030-01-01T00:00:00Z","kind":"gateway","boot":"prx_boot","profile":"prx_profile","actor":"prx_owner","target_owner":"prx_owner","workspace":"prx_workspace","session":"prx_session","run":"prx_run","event":"prx_source","input":"prx_input","routing":"prx_routing","sequence":7},"pending_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"pending_id":"00000000-0000-0000-0000-000000000001","call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31],"args_sha256":"prx_arguments","capability_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"expires":"2030-01-01T00:00:00Z"},"mac":[32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1]},"pending_mac":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32],"approved":true,"grants":[{"grant_id":"prx_grant","version":2,"subject":{"agent_id":"prx_agent","principal_id":"prx_principal","owner_id":"prx_owner","workspace_id":"prx_workspace","session_key":"prx_session"},"issuer":{"authority":"HumanReview","authority_id":"prx_reviewer","public_key_id":"prx_witness"},"capability":{"op_id":"prx_operation","op_id_match":"Exact","risk_level":"High"},"resource_constraints":{"abs_path_prefix":"/prx_workspace","url_host_allowlist":["example.com"],"recipient_allowlist":["prx_recipient"],"max_payload_bytes":1024,"max_concurrent_calls":1},"issued_at":"2029-12-31T23:55:00Z","not_before":"2029-12-31T23:55:00Z","expires_at":"2030-01-01T00:00:00Z","max_uses":1,"uses_consumed":0,"witness_signature":{"alg":"Ed25519","signature_b64":"cHJ4X2ZpeHR1cmU=","signed_payload_sha256":"prx_signed_payload"},"related_task_id":"prx_task","related_message_event_id":9,"revoked_at":"2030-01-01T00:01:00Z","revocation_reason":"prx_fixture","decision_binding":{"decision_id":"00000000-0000-0000-0000-000000000001","catalog_generation":[32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63],"call_digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]}}]},"mac":[32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1]}"#;
    for expected in [PENDING, POPULATED_PENDING] {
        let pending: super::pending::SignedPending = serde_json::from_str(expected).unwrap();
        assert_eq!(serde_json::to_string(&pending).unwrap(), expected);
    }
    for expected in [REVIEW, POPULATED_REVIEW] {
        let review: super::review::SignedReview = serde_json::from_str(expected).unwrap();
        assert_eq!(serde_json::to_string(&review).unwrap(), expected);
    }
}

#[tokio::test]
async fn pre_extraction_pending_and_issued_events_are_resolved_by_the_ledger() {
    Box::pin(crate::runtime::turn_decision::historical_approval_events()).await;
}

#[tokio::test]
async fn review_denial_blocks_issue() {
    Box::pin(crate::runtime::turn_decision::exact_file_collection_for_review(false)).await;
}

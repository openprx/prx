use super::*;
use crate::runtime::h2_entry::{EntryKind, profile::tests::Fixture};
fn signed(fixture: &Fixture) -> LocalOwnerReviewStamp {
    let authority = LocalControlAuthority::from_producer(fixture.profile.clone()).unwrap();
    authority
        .attest(
            fixture.origin().record(),
            [1; 32],
            "actual-pending",
            [2; 32],
            &serde_json::json!({"action":"lifecycle_experiment_publish","request":{"expected":"actual"}}),
            [3; 32],
            [4; 32],
            chrono::Utc::now() + chrono::Duration::minutes(1),
        )
        .unwrap()
}
#[test]
fn local_stamp_binds_pending_sealed_call_and_original_boot() {
    let fixture = Fixture::new(EntryKind::Gateway);
    let stamp = signed(&fixture);
    stamp
        .verify_pending([1; 32], fixture.origin().record(), "actual-pending", [2; 32])
        .unwrap();
    assert!(
        stamp
            .verify_pending([9; 32], fixture.origin().record(), "actual-pending", [2; 32])
            .is_err()
    );
    assert!(
        stamp
            .verify_pending([1; 32], fixture.origin().record(), "other-pending", [2; 32])
            .is_err()
    );
    assert!(
        stamp
            .verify_pending([1; 32], fixture.origin().record(), "actual-pending", [9; 32])
            .is_err()
    );
    LocalOwnerReview::from_signed(fixture.origin(), stamp.clone()).unwrap();
    let mut changed = stamp.clone();
    changed.body.args_sha256 = "f".repeat(64);
    assert!(LocalOwnerReview::from_signed(fixture.origin(), changed).is_err());
    let mut changed = stamp.clone();
    changed.body.boot = "old-boot".into();
    assert!(LocalOwnerReview::from_signed(fixture.origin(), changed).is_err());
    let mut changed = stamp;
    changed.body.capability_generation = [8; 32];
    assert!(LocalOwnerReview::from_signed(fixture.origin(), changed).is_err());
}
#[test]
fn verifying_stamp_without_key_never_creates_replacement() {
    let fixture = Fixture::new(EntryKind::Gateway);
    let stamp = signed(&fixture);
    let key = fixture.config.workspace_dir.join("self/causal_tree/.decision_key");
    std::fs::remove_file(&key).unwrap();
    assert!(LocalOwnerReview::from_signed(fixture.origin(), stamp).is_err());
    assert!(!key.exists());
}
#[cfg(unix)]
#[test]
fn verifying_stamp_with_open_key_permissions_refuses_without_repair() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(EntryKind::Gateway);
    let stamp = signed(&fixture);
    let key = fixture.config.workspace_dir.join("self/causal_tree/.decision_key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(LocalOwnerReview::from_signed(fixture.origin(), stamp).is_err());
    assert_eq!(std::fs::metadata(key).unwrap().permissions().mode() & 0o777, 0o644);
}
#[test]
fn revoked_authority_cannot_sign_or_resume_even_unchanged_stamp() {
    let fixture = Fixture::new(EntryKind::Gateway);
    let stamp = signed(&fixture);
    let authority = LocalControlAuthority::from_producer(fixture.profile.clone()).unwrap();
    fixture.profile.revoke();
    assert!(authority.verify_source(fixture.origin().record()).is_err());
    assert!(LocalOwnerReview::from_signed(fixture.origin(), stamp).is_err());
}

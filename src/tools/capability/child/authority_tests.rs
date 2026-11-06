use super::*;
use crate::causal_tree::branch::{CapabilityId, CapabilityTarget, SchemaFingerprint};

fn parent() -> CapabilityConstraints {
    let key = DecisionKey::ephemeral();
    let generation = CatalogGeneration([7; 32]);
    let allowed = ["prx_read", "prx_write"]
        .into_iter()
        .map(|name| CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "native", name).unwrap(),
            fingerprint: SchemaFingerprint([1; 32]),
            generation,
        })
        .collect();
    CapabilityConstraints {
        parent_authority_id: ParentAuthorityId::for_run("prx_run", &key),
        allowed,
        authority: AuthorityEnvelope {
            owner: "prx_user".into(),
            workspace: "prx_workspace".into(),
            level: AutonomyLevel::Supervised,
            max_cost_cents: 100,
            remaining_depth: 2,
            delegation: None,
        },
        catalog_generation: generation,
        key,
    }
}

fn child(parent: &CapabilityConstraints) -> ChildCapabilityManifest {
    let requested = parent.allowed.iter().next().unwrap().clone();
    ChildCapabilityManifest::derive_strict_subset(parent, &[requested]).unwrap()
}

#[test]
fn run_rooted_parent_derives_sealed_strict_subset() {
    let parent = parent();
    assert_eq!(
        parent.parent_authority_id,
        ParentAuthorityId::for_run("prx_run", &parent.key)
    );
    assert_ne!(
        parent.parent_authority_id,
        ParentAuthorityId::for_run("prx_other_run", &parent.key)
    );
    assert_ne!(
        parent.parent_authority_id,
        ParentAuthorityId::for_run("prx_run", &DecisionKey::ephemeral())
    );
    let child = child(&parent);
    assert_eq!(child.parent_authority_id, parent.parent_authority_id);
    assert!(child.allowed.is_subset(&parent.allowed));
    assert!(child.allowed.len() < parent.allowed.len());
    assert_eq!(child.authority.remaining_depth, 1);
    VerifiedChildScope::new(child, &parent)
        .unwrap()
        .validate_runtime(&parent.key, "prx_user", "prx_workspace", parent.catalog_generation)
        .unwrap();
}

#[test]
fn decision_rooted_parent_id_bytes_are_unchanged() {
    let mut parent = parent();
    let decision = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap();
    parent.parent_authority_id = decision.into();
    let child = child(&parent);
    assert_eq!(
        child.parent_authority_id.to_string().as_bytes(),
        decision.to_string().as_bytes()
    );
    // Independent pre-migration encoding: decision UUID text first, then all existing fields.
    let mut encoded = Vec::new();
    append(&mut encoded, decision.to_string().as_bytes());
    append(&mut encoded, &parent.catalog_generation.0);
    append(&mut encoded, b"prx_user");
    append(&mut encoded, b"prx_workspace");
    encoded.push(1);
    encoded.extend(100_u32.to_be_bytes());
    encoded.push(1);
    for reference in &child.allowed {
        append(&mut encoded, reference.id.as_str().as_bytes());
        append(&mut encoded, &reference.fingerprint.0);
        append(&mut encoded, &reference.generation.0);
    }
    assert_eq!(
        child.seal,
        parent.key.hmac(
            KeyDomain::StateFingerprint,
            &[b"child-capability-manifest-v1", &encoded]
        )
    );
    let wire = child.to_worker_manifest();
    assert_eq!(wire.parent_decision_id, decision.to_string());
    assert_eq!(
        ChildCapabilityManifest::from_worker_manifest(&wire, &parent).unwrap(),
        child
    );
}

#[test]
fn spawn_at_depth_zero_is_refused_before_launch() {
    let mut parent = parent();
    parent.authority.remaining_depth = 0;
    assert!(matches!(
        ChildCapabilityManifest::derive_strict_subset(&parent, &[]),
        Err(ChildManifestError::DepthExhausted)
    ));
}

#[test]
fn child_cost_above_parent_is_authority_widened() {
    let parent = parent();
    let mut child = child(&parent);
    child.authority.max_cost_cents += 1;
    child.seal = child.compute_seal(&parent.key);
    assert!(matches!(
        child.verify(&parent),
        Err(ChildManifestError::AuthorityWidened)
    ));
}

#[test]
fn readonly_parent_cannot_derive_supervised_child() {
    let mut parent = parent();
    parent.authority.level = AutonomyLevel::ReadOnly;
    let mut child = child(&parent);
    child.authority.level = AutonomyLevel::Supervised;
    child.seal = child.compute_seal(&parent.key);
    assert!(matches!(
        child.verify(&parent),
        Err(ChildManifestError::AuthorityWidened)
    ));
}

#[test]
fn foreign_owner_or_workspace_child_fails_validate_runtime() {
    let parent = parent();
    let scope = VerifiedChildScope::new(child(&parent), &parent).unwrap();
    for (owner, workspace) in [("prx_other_user", "prx_workspace"), ("prx_user", "prx_other_workspace")] {
        assert!(matches!(
            scope.validate_runtime(&parent.key, owner, workspace, parent.catalog_generation),
            Err(ChildManifestError::StaleReference)
        ));
    }
}

#[test]
fn stale_generation_child_fails_validate_runtime() {
    let parent = parent();
    let scope = VerifiedChildScope::new(child(&parent), &parent).unwrap();
    assert!(matches!(
        scope.validate_runtime(&parent.key, "prx_user", "prx_workspace", CatalogGeneration([8; 32])),
        Err(ChildManifestError::StaleReference)
    ));
}

#[test]
fn forged_seal_is_refused() {
    let parent = parent();
    let mut child = child(&parent);
    child.seal[0] ^= 1;
    assert!(matches!(
        VerifiedChildScope::new(child, &parent),
        Err(ChildManifestError::InvalidSeal)
    ));
}

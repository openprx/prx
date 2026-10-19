#![cfg(test)]

use super::*;
use crate::causal_tree::branch::{CapabilityId, CapabilityTarget, SchemaFingerprint};

fn parent(workspace: &std::path::Path) -> (CapabilityConstraints, CapabilityRef, CapabilityRef) {
    let generation = CatalogGeneration([7; 32]);
    let reference = |name: &str, fingerprint| CapabilityRef {
        id: CapabilityId::new(CapabilityTarget::NativeTool, "native", name).unwrap(),
        fingerprint: SchemaFingerprint([fingerprint; 32]),
        generation,
    };
    let spawn = reference("sessions_spawn", 1);
    let read = reference("file_read", 2);
    let other = reference("delegate", 3);
    (
        CapabilityConstraints {
            decision_id: DecisionId::legacy(&[9; 32]),
            allowed: BTreeSet::from([spawn.clone(), read.clone(), other]),
            authority: AuthorityEnvelope {
                owner: "prx_user".to_owned(),
                workspace: "prx_workspace".to_owned(),
                level: AutonomyLevel::Full,
                max_cost_cents: 7,
                remaining_depth: 9,
                delegation: None,
            },
            catalog_generation: generation,
            key: DecisionKey::load_or_create(workspace).unwrap(),
        },
        spawn,
        read,
    )
}

#[test]
fn recursive_manifest_seals_the_request_pins_and_configured_depth() {
    let temp = tempfile::tempdir().unwrap();
    let (parent, spawn, _) = parent(temp.path());
    let manifest = ChildCapabilityManifest::for_recursive_spawn(
        &parent,
        std::slice::from_ref(&spawn),
        "ollama",
        "prx-local-model",
        1,
    )
    .unwrap();
    manifest.verify(&parent).unwrap();
    assert_eq!(manifest.allowed, BTreeSet::from([spawn]));
    assert_eq!(manifest.authority.remaining_depth, 1);
    assert_eq!(manifest.authority.owner, parent.authority.owner);
    assert_eq!(manifest.authority.workspace, parent.authority.workspace);
    assert_eq!(manifest.authority.max_cost_cents, parent.authority.max_cost_cents);
    assert_eq!(manifest.authority.level, parent.authority.level);
    let pins = manifest.authority.delegation.as_ref().unwrap();
    assert!(pins.requested);
    assert_eq!(pins.provider, "ollama");
    assert_eq!(pins.model, "prx-local-model");
    let mut wire = manifest.to_worker_manifest();
    ChildCapabilityManifest::from_worker_manifest(&wire, &parent).unwrap();
    wire.delegation.as_mut().unwrap().requested = false;
    assert!(matches!(
        ChildCapabilityManifest::from_worker_manifest(&wire, &parent),
        Err(ChildManifestError::InvalidSeal)
    ));
    wire = manifest.to_worker_manifest();
    wire.delegation.as_mut().unwrap().model = "prx-other-model".to_owned();
    assert!(matches!(
        ChildCapabilityManifest::from_worker_manifest(&wire, &parent),
        Err(ChildManifestError::InvalidSeal)
    ));
    wire = manifest.to_worker_manifest();
    wire.delegation = None;
    assert!(matches!(
        ChildCapabilityManifest::from_worker_manifest(&wire, &parent),
        Err(ChildManifestError::InvalidSeal)
    ));
    wire = manifest.to_worker_manifest();
    wire.remaining_depth = 2;
    assert!(matches!(
        ChildCapabilityManifest::from_worker_manifest(&wire, &parent),
        Err(ChildManifestError::InvalidSeal)
    ));
}

#[test]
fn descendant_manifest_keeps_model_pins_and_cannot_replenish_depth_or_tools() {
    let temp = tempfile::tempdir().unwrap();
    let (root, spawn, read) = parent(temp.path());
    let child =
        ChildCapabilityManifest::for_recursive_spawn(&root, &[spawn.clone(), read], "ollama", "prx-local-model", 2)
            .unwrap();
    let parent = CapabilityConstraints {
        decision_id: DecisionId::legacy(&[8; 32]),
        allowed: child.allowed,
        authority: child.authority,
        catalog_generation: root.catalog_generation,
        key: Arc::clone(&root.key),
    };
    for (provider, model) in [("openai", "prx-local-model"), ("ollama", "prx-other-model")] {
        assert!(matches!(
            ChildCapabilityManifest::for_recursive_spawn(&parent, std::slice::from_ref(&spawn), provider, model, 2,),
            Err(ChildManifestError::AuthorityWidened)
        ));
    }
    let grandchild = ChildCapabilityManifest::for_recursive_spawn(
        &parent,
        std::slice::from_ref(&spawn),
        "ollama",
        "prx-local-model",
        9,
    )
    .unwrap();
    assert_eq!(
        grandchild.authority.remaining_depth, 1,
        "config cannot refill inherited depth"
    );
    assert_eq!(grandchild.authority.max_cost_cents, 7);
    let leaf_parent = CapabilityConstraints {
        decision_id: DecisionId::legacy(&[6; 32]),
        allowed: grandchild.allowed,
        authority: grandchild.authority,
        catalog_generation: parent.catalog_generation,
        key: Arc::clone(&parent.key),
    };
    assert!(matches!(
        ChildCapabilityManifest::for_recursive_spawn(
            &leaf_parent,
            std::slice::from_ref(&spawn),
            "ollama",
            "prx-local-model",
            9,
        ),
        Err(ChildManifestError::NotStrictSubset)
    ));
    let leaf = ChildCapabilityManifest::derive_strict_subset(&leaf_parent, &[]).unwrap();
    assert_eq!(leaf.authority.remaining_depth, 0);
    let pins = leaf.authority.delegation.as_ref().unwrap();
    assert!(!pins.requested, "each new spawn requires its own opt-in");
    assert_eq!(pins.model, "prx-local-model");
    let exhausted = CapabilityConstraints {
        decision_id: DecisionId::legacy(&[5; 32]),
        allowed: leaf.allowed,
        authority: leaf.authority,
        catalog_generation: parent.catalog_generation,
        key: Arc::clone(&parent.key),
    };
    assert!(matches!(
        ChildCapabilityManifest::derive_strict_subset(&exhausted, &[]),
        Err(ChildManifestError::DepthExhausted)
    ));
}

#[test]
fn process_narrowing_preserves_issued_pins_budget_and_depth() {
    let temp = tempfile::tempdir().unwrap();
    let (root, spawn, read) = parent(temp.path());
    let manifest = ChildCapabilityManifest::for_recursive_spawn(
        &root,
        &[spawn.clone(), read.clone()],
        "ollama",
        "prx-local-model",
        1,
    )
    .unwrap();
    let narrowed = manifest.narrow_allowed(&root, std::slice::from_ref(&spawn)).unwrap();
    assert_eq!(narrowed.authority, manifest.authority);
    assert_eq!(narrowed.allowed, BTreeSet::from([spawn]));
    narrowed.verify(&root).unwrap();
    assert!(matches!(
        narrowed.narrow_allowed(&root, &[read]),
        Err(ChildManifestError::NotStrictSubset)
    ));
    let constraints = CapabilityConstraints {
        authority: narrowed.authority,
        ..root.clone()
    };
    let wire = constraints.to_worker_constraints().unwrap();
    let restored = CapabilityConstraints::from_worker_constraints(&wire, Arc::clone(&root.key)).unwrap();
    assert_eq!(restored.authority, constraints.authority);
    let mut forged = wire;
    forged.delegation.as_mut().unwrap().provider = "openai".to_owned();
    assert!(matches!(
        CapabilityConstraints::from_worker_constraints(&forged, Arc::clone(&root.key)),
        Err(ChildManifestError::InvalidSeal)
    ));
}

#[test]
fn ordinary_manifest_retains_legacy_transport_and_parent_signature() {
    let temp = tempfile::tempdir().unwrap();
    let (root, _, _) = parent(temp.path());
    let child = ChildCapabilityManifest::derive_strict_subset(&root, &[]).unwrap();
    assert!(child.authority.delegation.is_none());
    assert!(
        serde_json::to_value(child.to_worker_manifest())
            .unwrap()
            .get("delegation")
            .is_none()
    );
    let wire = root.to_worker_constraints().unwrap();
    assert!(serde_json::to_value(&wire).unwrap().get("delegation").is_none());
    let legacy = serde_json::to_vec(&(
        &wire.decision_id,
        &wire.allowed,
        &wire.owner,
        &wire.workspace,
        wire.level,
        wire.max_cost_cents,
        wire.remaining_depth,
        wire.catalog_generation,
    ))
    .unwrap();
    assert!(root.key.verify_hmac(
        KeyDomain::StateFingerprint,
        &[b"worker-parent-constraints-v1", &legacy],
        &wire.seal
    ));
}

#[test]
fn delegation_transport_keeps_authority_labels_but_redacts_debug_credentials() {
    let provider = ["custom:http://", "prx_user", ":", "fixture-password", "@example.com/v1"].concat();
    let pins = wire::WorkerDelegationAuthority {
        requested: true,
        provider: provider.clone(),
        model: "prx-model".to_owned(),
    };
    let encoded = serde_json::to_string(&pins).unwrap();
    let decoded: wire::WorkerDelegationAuthority = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.provider, provider);
    let debug = format!("{decoded:?}");
    assert!(!debug.contains("fixture-password"));
    assert!(!debug.contains("prx_user"));
    assert!(debug.contains("example.com"));
}

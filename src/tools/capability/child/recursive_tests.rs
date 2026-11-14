#![cfg(test)]

use super::*;
use crate::tools::capability::contract::{CapabilityId, CapabilityTarget, SchemaFingerprint};

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
            parent_authority_id: DecisionId::parse("0192f0c4-0000-7000-8000-000000000009")
                .expect("test: id")
                .into(),
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

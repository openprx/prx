use super::*;
use crate::gateway::api::test_support::{TOKEN, serve_config};
async fn fixture() -> super::super::test_support::Served {
    let served = serve_config(|config| {
        config.skills_lifecycle.enabled = true;
        config.skills_lifecycle.owner_scope = Some("owner:prx:h2:identity".into());
        config.causal_tree.enabled = true;
        config.memory.events.record_user_messages = true;
        config.runtime.kind = "native".into();
        config.memory.backend = "sqlite".into();
        config.tunnel.provider = "none".into();
        config.plugins.enabled = false;
        config.gateway.require_pairing = true;
        config.gateway.paired_tokens = vec![TOKEN.into()];
        config.sessions_spawn.allow_recursive_delegation = false;
    })
    .await;
    let config = served.state.config.load_full();
    crate::skills::lifecycle::SkillLifecycleStore::open(
        &config.workspace_dir,
        config.skills_lifecycle.owner().unwrap().unwrap(),
    )
    .unwrap();
    served
}
#[tokio::test]
async fn unauthenticated_identity_has_no_evidence_sideeffect_and_response_has_no_secret() {
    let served = fixture().await;
    let config = served.state.config.load_full();
    let store = existing_store(&config).unwrap();
    let (status, _) = served.call_anonymous(reqwest::Method::GET, "/api/identity", None).await;
    assert_eq!(status, 401);
    assert!(!store.root().join("application-evidence").exists());
    let (status, body) = served.call(reqwest::Method::GET, "/api/identity", None).await;
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value.get("principal_id").unwrap(),
        ApiPrincipal::of_token(TOKEN).owner_id
    );
    assert!(!body.contains(TOKEN));
    assert!(!body.contains(&crate::security::pairing::hash_token(TOKEN)));
    for forbidden in ["credential_verifier", "owner_mac", "mac", "key"] {
        assert!(value.get(forbidden).is_none());
    }
    assert_eq!(value.as_object().unwrap().len(), 4);
    verify_startup_identity(&config, &ApiPrincipal::of_token(TOKEN).owner_id)
        .await
        .unwrap();
    let mut changed = (*config).clone();
    changed.gateway.paired_tokens = vec!["different-existing-token".into()];
    assert!(
        verify_startup_identity(&changed, &ApiPrincipal::of_token(TOKEN).owner_id)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn startup_identity_requires_existing_private_mac_and_never_repairs_missing_key() {
    let served = fixture().await;
    let (status, body) = served.call(reqwest::Method::GET, "/api/identity", None).await;
    assert_eq!(status, 200, "{body}");
    let config = served.state.config.load_full();
    let principal = ApiPrincipal::of_token(TOKEN).owner_id;
    let key = config.workspace_dir.join("self/causal_tree/.decision_key");
    std::fs::remove_file(&key).unwrap();
    assert!(verify_startup_identity(&config, &principal).await.is_err());
    assert!(!key.exists());
}

#[tokio::test]
async fn tampered_descriptor_cannot_select_another_owner_or_principal() {
    use sha2::{Digest, Sha256};
    let served = fixture().await;
    assert_eq!(served.call(reqwest::Method::GET, "/api/identity", None).await.0, 200);
    let config = served.state.config.load_full();
    let principal = ApiPrincipal::of_token(TOKEN).owner_id;
    let store = existing_store(&config).unwrap();
    let path = store
        .root()
        .join("application-evidence/authenticated-api-identity-v1")
        .join(format!("{}.json", hex::encode(Sha256::digest(principal.as_bytes()))));
    let original = std::fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    *value.get_mut("body").unwrap().get_mut("store_owner").unwrap() = serde_json::json!("owner:other");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(verify_startup_identity(&config, &principal).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), serde_json::to_vec(&value).unwrap());
}
#[tokio::test]
async fn frozen_config_denies_raw_write_and_reload_before_disk_or_generation_change() {
    let served = fixture().await;
    let before = served.disk();
    let generation = served.state.config.active_generation_id();
    let revision = served.state.config.desired().source_revision.fingerprint_sha256.clone();
    served.state.config.freeze_runtime_writes();
    let (status, _) = served
        .call_if_match(
            reqwest::Method::PUT,
            "/api/config/files/config.toml",
            &revision,
            Some(serde_json::json!({"content":format!("{before}\n# controlled mutation must be refused\n")})),
        )
        .await;
    assert!(status >= 400);
    assert_eq!(served.disk(), before);
    let (status, _) = served.call(reqwest::Method::POST, "/api/config/reload", None).await;
    assert!(status >= 400);
    assert_eq!(served.disk(), before);
    assert_eq!(served.state.config.active_generation_id(), generation);
}

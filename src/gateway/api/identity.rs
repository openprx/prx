//! Authenticated identity evidence uses the existing private lifecycle store.
//! It never enters semantic memory, message events, an outbox or remote sync.
use super::*;
use crate::decision::{DecisionKey, KeyDomain};
use crate::skills::lifecycle::SkillLifecycleStore;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IdentityBody {
    version: u32,
    workspace: String,
    store_owner: String,
    principal: String,
    credential_verifier: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedIdentity {
    body: IdentityBody,
    mac: [u8; 32],
}

/// GET records one immutable private application descriptor. The response never
/// includes the token, verifier or MAC. It creates no owner, store or grant.
pub(super) async fn get_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<ApiPrincipal>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = async {
        let config = state.config.load_full();
        crate::runtime::h2_entry::ControlledH2Profile::validate_config(&config)?;
        ensure!(
            state.pairing.require_pairing(),
            "identity evidence requires existing paired authentication"
        );
        let token = extract_resource_auth_token(&headers);
        ensure!(
            !token.is_empty() && state.pairing.is_authenticated(&token) && principal == ApiPrincipal::of_token(&token),
            "authenticated identity changed"
        );
        let store = existing_store(&config)?;
        let workspace = config.workspace_dir.canonicalize()?.to_string_lossy().into_owned();
        let body = IdentityBody {
            version: 1,
            workspace: workspace.clone(),
            store_owner: store.owner_scope().as_str().into(),
            principal: principal.owner_id.clone(),
            credential_verifier: crate::security::pairing::hash_token(&token),
        };
        let key = DecisionKey::load_or_create(&config.workspace_dir)?;
        let signed = SignedIdentity {
            mac: key.hmac(
                KeyDomain::ApprovalState,
                &[b"gateway-authenticated-identity-v1", &serde_json::to_vec(&body)?],
            ),
            body,
        };
        let selector = principal.owner_id.clone();
        let value = serde_json::to_value(signed)?;
        tokio::task::spawn_blocking(move || store.write_authenticated_identity_evidence(&selector, &value)).await??;
        verify_startup_identity(&config, &principal.owner_id).await?;
        Ok::<_, anyhow::Error>(Json(serde_json::json!({"principal_id":principal.owner_id,
            "workspace":workspace,"descriptor_recorded":true,
            "provenance":"existing authenticated bearer; private application identity evidence only"})))
    }
    .await;
    result.map_err(|error| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":error.to_string()})),
        )
    })
}
fn existing_store(config: &Config) -> anyhow::Result<SkillLifecycleStore> {
    let owner = config
        .skills_lifecycle
        .owner()?
        .context("existing lifecycle owner is required")?;
    SkillLifecycleStore::open_existing(&config.workspace_dir, owner)?.context("lifecycle store must already exist")
}
pub(in crate::gateway) async fn verify_startup_identity(config: &Config, principal: &str) -> anyhow::Result<()> {
    crate::runtime::h2_entry::ControlledH2Profile::validate_config(config)?;
    let store = existing_store(config)?;
    let owner = store.owner_scope().as_str().to_owned();
    let selector = principal.to_owned();
    let value = tokio::task::spawn_blocking(move || store.read_authenticated_identity_evidence(&selector)).await??;
    let signed: SignedIdentity = serde_json::from_value(value)?;
    let workspace = config.workspace_dir.canonicalize()?.to_string_lossy().into_owned();
    let key = DecisionKey::load_or_create(&config.workspace_dir)?;
    ensure!(
        signed.body.version == 1
            && signed.body.workspace == workspace
            && signed.body.store_owner == owner
            && signed.body.principal == principal
            && key.verify_hmac(
                KeyDomain::ApprovalState,
                &[b"gateway-authenticated-identity-v1", &serde_json::to_vec(&signed.body)?],
                &signed.mac
            ),
        "existing identity descriptor is unauthenticated"
    );
    ensure!(
        config.gateway.require_pairing
            && config.gateway.paired_tokens.iter().any(|value| {
                let verifier = if crate::security::pairing::is_token_hash(value) {
                    value.clone()
                } else {
                    crate::security::pairing::hash_token(value)
                };
                crate::security::pairing::constant_time_eq(&verifier, &signed.body.credential_verifier)
            }),
        "identity credential is absent from the startup paired snapshot"
    );
    Ok(())
}

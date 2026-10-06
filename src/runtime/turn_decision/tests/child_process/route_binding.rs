#![cfg(test)]
//! Reduced source/route authentication component case. No provider-wire claim.
use super::*;

pub(super) const MODEL_HINT: &str = "hint:prx-reduced-route";
const ROUTE: &str = "prx-reduced-route";
const ROOT_KEY: &str = "PRX-REDUCED-ROOT-KEY";
const ROUTE_KEY: &str = "PRX-REDUCED-ROUTE-KEY";

pub(super) fn configure(config: &mut crate::config::Config) {
    config.default_model = Some(MODEL_HINT.into());
    config.api_key = Some(ROOT_KEY.into());
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: ROUTE.into(),
        provider: "ollama".into(),
        model: "prx-local-model".into(),
        api_key: Some(ROUTE_KEY.into()),
        credential_ref: None,
    });
}

pub(super) fn verify_capture(
    captured: &parking_lot::Mutex<crate::tools::sessions_spawn::TestWorkerRouteCapture>,
    proof: &AgentTurnProof,
    completed_run: &str,
) -> anyhow::Result<()> {
    use crate::providers::credentials::{CredentialRef, LegacySlot};
    let capture = captured.lock();
    assert_eq!(capture.total, 1, "exactly one actual manifest was produced");
    let observed = capture.first.as_ref().context("actual manifest observation missing")?;
    assert_eq!(observed.run_id, completed_run);
    assert_eq!(observed.parent_run_id.as_deref(), Some(proof.run_id()));
    assert_eq!(observed.owner_id.as_deref(), Some(proof.owner_id()));
    assert_eq!(observed.source_event_id.as_deref(), proof.source_event_id());
    assert_eq!(observed.provider, "ollama");
    assert_eq!(observed.model, "prx-local-model");
    assert_eq!(observed.route_hint.as_deref(), Some(ROUTE));
    assert_eq!(observed.route_source_provider.as_deref(), Some("ollama"));
    assert_eq!(
        observed.route_credential,
        Some(CredentialRef::LegacyInline(LegacySlot::ModelRoute(ROUTE.into())))
    );
    let route_digest: [u8; 32] = Sha256::digest(ROUTE_KEY.as_bytes()).into();
    let root_digest: [u8; 32] = Sha256::digest(ROOT_KEY.as_bytes()).into();
    assert_eq!(observed.key_sha256, Some(route_digest));
    assert_ne!(observed.key_sha256, Some(root_digest));
    assert!(!observed.route_uses_root_connection && !observed.direct_uses_root_connection);
    assert!(observed.child_source_present);
    assert_eq!(
        observed.child_allowed_count,
        Some(0),
        "no recursive child tool authority"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root-attested worker artifact and outer guard; reduced route/source close only, not wire or recursive E2E"]
async fn actual_parent_issued_single_worker_non_root_route_closes_without_wire() -> anyhow::Result<()> {
    boxed_parent_process_case(true).await
}

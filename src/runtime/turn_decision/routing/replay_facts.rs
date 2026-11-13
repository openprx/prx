//! Provider identity bindings retained for historical Router replay.
/// Same identity normalization as actual persisted provider outcomes. Only
/// keyed bytes enter the Router replay projection, never a private URL.
pub(crate) fn provider_binding(key: &crate::security::runtime_key::DecisionKey, provider: &str) -> [u8; 32] {
    key.hmac(
        crate::security::runtime_key::KeyDomain::StateFingerprint,
        &[
            b"router-readmission-provider-v1",
            crate::providers::redact_endpoint_userinfo(provider).as_bytes(),
        ],
    )
}

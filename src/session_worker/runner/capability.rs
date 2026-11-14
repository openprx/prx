use crate::session_worker::protocol::WorkerManifest;
use anyhow::{Context, Result};

/// Validate the sealed session-worker capability (FIX-P0-36).
///
/// The capability is no longer an opaque UUID compared by string equality. It
/// is an `HMAC_SHA256(secret, run_id \0 expiry \0 sha256(manifest))` token
/// (base64url). Validation:
/// 1. Both the manifest-embedded token and the env-supplied token must be
///    present and identical (defends against env/manifest desync).
/// 2. The capability must not be past its absolute expiry.
/// 3. The token must equal the HMAC recomputed from the *received* manifest
///    (capability field blanked), the run id, and the expiry — compared in
///    constant time. A legacy UUID, a forged token, a tampered manifest, or a
///    replay under a different run id all fail here.
///
/// The expiry is read from `OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY`.
/// Created only after verifying this exact manifest and its transport expiry.
/// Not cloneable or serializable; environment secrets can be scrubbed at boot.
pub(crate) struct AuthenticatedWorkerTransport {
    pub(super) key: ring::hmac::Key,
    pub(super) manifest_digest: [u8; 32],
}

impl std::fmt::Debug for AuthenticatedWorkerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedWorkerTransport").finish_non_exhaustive()
    }
}

impl AuthenticatedWorkerTransport {
    pub(crate) fn verify_manifest(&self, manifest: &WorkerManifest) -> Result<()> {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(manifest_signing_payload(manifest)?.as_bytes()).into();
        anyhow::ensure!(
            digest == self.manifest_digest,
            "worker manifest changed after transport authentication"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn validate_worker_capability_with_env(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
) -> Result<()> {
    authenticate_worker_capability_with_env(manifest, env_capability).map(|_| ())
}

pub(super) fn authenticate_worker_capability_with_env(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
) -> Result<AuthenticatedWorkerTransport> {
    matching_worker_capability(manifest, env_capability)?;
    let expiry = capability_expiry_from_env()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    anyhow::ensure!(now <= expiry, "session-worker capability expired");
    let key = crate::session_worker::transport_key::TransportSecret::existing()?.hmac_key();
    authenticate_worker_capability_with_key(manifest, env_capability, expiry, now, key)
}

fn matching_worker_capability<'a>(manifest: &'a WorkerManifest, env_capability: Option<&str>) -> Result<&'a str> {
    let manifest_capability = manifest
        .parent_capability
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker manifest is missing parent capability")?;
    let env_capability = env_capability
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker parent capability env is missing")?;

    if !capability_constant_time_eq(manifest_capability.as_bytes(), env_capability.as_bytes()) {
        anyhow::bail!("session-worker parent capability mismatch");
    }

    Ok(manifest_capability)
}

pub(super) fn authenticate_worker_capability_with_key(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
    expiry: u64,
    now: u64,
    key: ring::hmac::Key,
) -> Result<AuthenticatedWorkerTransport> {
    let manifest_capability = matching_worker_capability(manifest, env_capability)?;

    if now > expiry {
        anyhow::bail!("session-worker capability expired");
    }

    let payload = manifest_signing_payload(manifest)?;
    let expected = compute_worker_capability_with_key(&key, &manifest.run_id, expiry, &payload);
    if !capability_constant_time_eq(manifest_capability.as_bytes(), expected.as_bytes()) {
        anyhow::bail!("session-worker capability signature mismatch");
    }

    use sha2::{Digest, Sha256};
    Ok(AuthenticatedWorkerTransport {
        key,
        manifest_digest: Sha256::digest(payload.as_bytes()).into(),
    })
}

/// Read the capability absolute expiry (unix seconds) from the environment.
pub(super) fn capability_expiry_from_env() -> Result<u64> {
    let raw = std::env::var("OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY")
        .context("session-worker capability expiry env is missing")?;
    raw.trim()
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("invalid session-worker capability expiry: {e}"))
}

/// Serialize a manifest with an empty `parent_capability` field, returning the
/// canonical JSON payload (alphabetical key order via `serde_json::Value`) — the
/// exact input the parent signed in `sessions_spawn::manifest_signing_payload`.
pub(super) fn manifest_signing_payload(manifest: &WorkerManifest) -> Result<String> {
    let mut value = serde_json::to_value(manifest).context("serialize worker manifest for capability")?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    serde_json::to_string(&value).context("reserialize worker manifest for capability")
}

/// Recompute `HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest))` as
/// base64url (no padding). Mirror of `sessions_spawn::compute_worker_capability`.
#[cfg(test)]
pub(super) fn compute_worker_capability(run_id: &str, expiry_unix: u64, manifest_json: &str) -> Result<String> {
    let key = crate::session_worker::transport_key::TransportSecret::for_launch()?.hmac_key();
    Ok(compute_worker_capability_with_key(
        &key,
        run_id,
        expiry_unix,
        manifest_json,
    ))
}

pub(super) fn compute_worker_capability_with_key(
    key: &ring::hmac::Key,
    run_id: &str,
    expiry_unix: u64,
    manifest_json: &str,
) -> String {
    use base64::Engine as _;
    use ring::hmac;
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(manifest_json.as_bytes());
    let manifest_hex = capability_hex_encode(&hasher.finalize());

    let mut payload = Vec::with_capacity(run_id.len() + manifest_hex.len() + 32);
    payload.extend_from_slice(run_id.as_bytes());
    payload.push(0);
    payload.extend_from_slice(expiry_unix.to_string().as_bytes());
    payload.push(0);
    payload.extend_from_slice(manifest_hex.as_bytes());

    let tag = hmac::sign(key, &payload);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref())
}

/// Lowercase hex-encode a byte slice.
fn capability_hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        // `b >> 4` and `b & 0x0f` are both in `0..16`, always valid indices into
        // the 16-byte `HEX` table; `.get().copied()` keeps the lookup panic-free.
        out.push(HEX.get((b >> 4) as usize).copied().unwrap_or(b'0') as char);
        out.push(HEX.get((b & 0x0f) as usize).copied().unwrap_or(b'0') as char);
    }
    out
}

/// Constant-time byte-slice equality.
fn capability_constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

use crate::session_worker::protocol::WorkerManifest;

/// Environment variable carrying the sealed session-worker capability to the
/// child process.
pub(crate) const SESSION_WORKER_CAP_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY";
/// Environment variable carrying the capability's absolute expiry (unix secs),
/// which is bound into the capability HMAC.
pub(crate) const SESSION_WORKER_CAP_EXPIRY_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY";
/// Capability time-to-live in seconds.
pub(super) const SESSION_WORKER_CAP_TTL_SECS: u64 = 300;

/// Current unix time in seconds (saturating to 0 on clock errors).
pub(super) fn capability_now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serialize a manifest with an empty `parent_capability` field, returning the
/// canonical JSON payload (alphabetical key order via `serde_json::Value`).
pub(super) fn manifest_signing_payload(manifest: &WorkerManifest) -> anyhow::Result<String> {
    let mut value =
        serde_json::to_value(manifest).map_err(|e| anyhow::anyhow!("serialize worker manifest for capability: {e}"))?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    serde_json::to_string(&value).map_err(|e| anyhow::anyhow!("reserialize worker manifest for capability: {e}"))
}

/// Compute `HMAC_SHA256(T, run_id \0 expiry \0 sha256_hex(manifest))` as
/// base64url (no padding), where `T` is the strict transport secret
/// (`session_worker::transport_key`).
///
/// FIX-P0-36: the manifest is serialized via `serde_json::Value` (alphabetical
/// key order) so that the parent and the validating worker produce byte-identical
/// inputs. The worker-side mirror is `session_worker::runner::capability`.
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
    let manifest_hex = hmac_hex_encode(&hasher.finalize());

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
fn hmac_hex_encode(bytes: &[u8]) -> String {
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

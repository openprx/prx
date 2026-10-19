use crate::session_worker::protocol::WorkerManifest;
use anyhow::Context as _;
use std::sync::OnceLock;

/// Environment variable carrying the sealed session-worker capability to the
/// child process.
pub(super) const SESSION_WORKER_CAP_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY";
/// Environment variable carrying the capability's absolute expiry (unix secs),
/// which is bound into the capability HMAC.
pub(super) const SESSION_WORKER_CAP_EXPIRY_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY";
/// Capability time-to-live in seconds.
pub(super) const SESSION_WORKER_CAP_TTL_SECS: u64 = 300;

/// Current unix time in seconds (saturating to 0 on clock errors).
pub(super) fn capability_now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compute the sealed capability HMAC for a manifest and absolute expiry.
///
/// FIX-P0-36: the signed payload is
/// `run_id \0 expiry_unix \0 sha256_hex(manifest_json_with_empty_capability)`,
/// signed with `HMAC_SHA256(secret, payload)` and encoded as base64url (no
/// padding). The manifest is serialized via `serde_json::Value` (alphabetical
/// key order) so that the parent (this side) and the validating worker — which
/// reconstructs the payload from the transmitted JSON — produce byte-identical
/// inputs regardless of struct field declaration order.
///
/// NOTE: the equivalent recomputation lives in `session_worker::runner`
/// (`expected_worker_capability`). The two must stay in lockstep; they are kept
/// in separate modules deliberately (parent mints, child validates) and share
/// the same payload construction documented here.
pub(super) fn seal_worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> anyhow::Result<String> {
    let payload = manifest_signing_payload(manifest)?;
    Ok(compute_worker_capability(&manifest.run_id, expiry_unix, &payload))
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

/// Compute `HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest))` as
/// base64url (no padding).
fn compute_worker_capability(run_id: &str, expiry_unix: u64, manifest_json: &str) -> String {
    compute_worker_capability_with_key(&session_worker_signing_key(), run_id, expiry_unix, manifest_json)
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

/// Process-level fallback secret, minted once if neither `SESSION_WORKER_SECRET`
/// nor a persisted secret file is available.
static SESSION_WORKER_FALLBACK_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

/// Return the shared HMAC signing key for session-worker capabilities.
///
/// Resolution order (parent and child run the same binary on the same host, so
/// all three sources are deterministic across the process boundary):
/// 1. `SESSION_WORKER_SECRET` environment variable (explicit configuration).
/// 2. A 32-byte secret persisted under the OpenPRX state dir (auto-generated on
///    first use, mirroring `WitnessKeyring`), so parent and child derive the
///    same key without transporting the secret alongside the capability.
/// 3. A per-process random fallback (only consistent within a single process;
///    used in tests / when no filesystem state dir is available).
pub(super) fn session_worker_signing_key() -> ring::hmac::Key {
    use ring::hmac;
    if let Ok(secret) = std::env::var("SESSION_WORKER_SECRET") {
        if !secret.is_empty() {
            return hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        }
    }
    if let Some(bytes) = load_or_create_persisted_session_secret() {
        return hmac::Key::new(hmac::HMAC_SHA256, &bytes);
    }
    let bytes = SESSION_WORKER_FALLBACK_SECRET.get_or_init(generate_session_secret);
    hmac::Key::new(hmac::HMAC_SHA256, bytes)
}

/// Recovery must never create or rotate the transport key, or substitute a
/// process-local fallback when the original signing material is unavailable.
pub(super) fn existing_session_worker_signing_key() -> anyhow::Result<ring::hmac::Key> {
    if let Ok(secret) = std::env::var("SESSION_WORKER_SECRET") {
        if !secret.is_empty() {
            return Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()));
        }
    }
    let path = session_secret_path().context("worker recovery key location unavailable")?;
    let bytes = std::fs::read(path).context("existing worker recovery key unavailable")?;
    anyhow::ensure!(bytes.len() == 32, "existing worker recovery key has invalid length");
    Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
}

/// Generate 32 random bytes, falling back to a time-derived seed (never panics)
/// when the system RNG is unavailable.
fn generate_session_secret() -> [u8; 32] {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    let mut buf = [0u8; 32];
    if rng.fill(&mut buf).is_err() {
        let now = capability_now_unix().to_le_bytes();
        for (i, b) in buf.iter_mut().enumerate() {
            // `i % now.len()` is always within `now` (non-empty fixed-size array);
            // `.get().copied()` keeps the seed fill panic-free.
            *b = now.get(i % now.len()).copied().unwrap_or(0);
        }
    }
    buf
}

/// Path to the persisted session-worker secret under the OpenPRX state dir.
///
/// Mirrors `WitnessKeyring`'s convention: an explicit override env, else the
/// resolved config directory, else `$HOME/.openprx` (no `dirs` dependency).
pub(super) fn session_secret_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENPRX_SESSION_WORKER_SECRET_PATH") {
        return Some(std::path::PathBuf::from(explicit));
    }
    // Follows the resolved config directory so the secret minted by a
    // `--config-dir` run is the one its workers read back, and no run writes
    // key material into a configuration it was told not to touch.
    if let Some(config_dir) = crate::config::process_config_dir() {
        return Some(config_dir.join("keys").join("session_worker.secret"));
    }
    crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret"))
}

/// Load the persisted 32-byte secret, creating it on first use. Returns `None`
/// if no state dir is resolvable or any filesystem operation fails (callers then
/// fall back to the per-process random secret).
fn load_or_create_persisted_session_secret() -> Option<[u8; 32]> {
    let path = session_secret_path()?;
    if let Ok(existing) = std::fs::read(&path) {
        if existing.len() == 32 {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&existing);
            return Some(buf);
        }
        // Wrong length → fall through and regenerate.
    }
    let secret = generate_session_secret();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    match std::fs::write(&path, secret) {
        Ok(()) => Some(secret),
        Err(error) => {
            tracing::warn!("failed to persist session-worker secret: {error}");
            None
        }
    }
}

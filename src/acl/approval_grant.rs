use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalGrantV2 {
    pub grant_id: String,
    pub version: u8,
    pub subject: Subject,
    pub issuer: Issuer,
    pub capability: Capability,
    pub resource_constraints: ResourceConstraints,
    pub issued_at: DateTime<Utc>,
    pub not_before: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub max_uses: u32,
    pub uses_consumed: u32,
    pub witness_signature: WitnessSignature,
    pub related_task_id: Option<String>,
    pub related_message_event_id: Option<i64>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revocation_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    pub agent_id: String,
    pub principal_id: String,
    pub owner_id: String,
    pub workspace_id: String,
    pub session_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IssuerAuthority {
    RuntimeAutomatic,
    HumanReview,
    ExternalOAuth,
    Delegated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issuer {
    pub authority: IssuerAuthority,
    pub authority_id: String,
    pub public_key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capability {
    pub op_id: String,
    pub op_id_match: OpIdMatch,
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpIdMatch {
    Exact,
    GlobPattern(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low = 0,
    Medium = 1,
    High = 2,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceConstraints {
    pub abs_path_prefix: Option<String>,
    pub url_host_allowlist: Option<Vec<String>>,
    pub recipient_allowlist: Option<Vec<String>>,
    pub max_payload_bytes: Option<u64>,
    pub max_concurrent_calls: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WitnessSignature {
    pub alg: String,
    pub signature_b64: String,
    pub signed_payload_sha256: String,
}

/// On-disk persisted form of the Ed25519 witness key.
///
/// Stored as JSON at `~/.openprx/keys/runtime_witness.key` with `0600`
/// permissions. The secret is the PKCS#8 v2 DER (base64, standard alphabet);
/// the public key is the raw 32-byte Ed25519 point (base64).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWitnessKey {
    kid: String,
    alg: String,
    secret_b64: String,
    public_b64: String,
    created_at: String,
}

#[derive(Debug, Clone)]
pub struct WitnessKeyring {
    kid: String,
    pkcs8: Vec<u8>,
    public_key: Vec<u8>,
}

/// Process-global witness keyring, lazily initialised from disk on first use.
static GLOBAL_WITNESS_KEYRING: OnceLock<WitnessKeyring> = OnceLock::new();

/// Default on-disk location for the runtime witness key, relative to `$HOME`.
/// Mirrors the PRX config dir convention (`.openprx`).
#[must_use]
fn default_witness_key_path() -> Option<PathBuf> {
    // `OPENPRX_WITNESS_KEY_PATH` lets tests / operators pin a deterministic
    // location without depending on `$HOME`.
    if let Some(explicit) = std::env::var_os("OPENPRX_WITNESS_KEY_PATH") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".openprx").join("keys").join("runtime_witness.key"))
}

impl WitnessKeyring {
    /// Generate an ephemeral, in-memory-only keyring. Test/helper use only:
    /// the key is never persisted, so grants it signs cannot survive a restart.
    pub fn generate_for_tests() -> Result<Self> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("generate Ed25519 keypair"))?;
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow::anyhow!("load generated Ed25519 keypair"))?;
        Ok(Self {
            kid: format!("wit-{}", Uuid::new_v4()),
            pkcs8: pkcs8.as_ref().to_vec(),
            public_key: key_pair.public_key().as_ref().to_vec(),
        })
    }

    /// Load the witness keyring from `path`, or generate a fresh Ed25519 keypair
    /// and persist it (creating parent directories, `0600` on the key file) when
    /// the file does not yet exist.
    ///
    /// Production code must use this (or [`WitnessKeyring::global`]) rather than
    /// [`WitnessKeyring::generate_for_tests`], so that signatures remain
    /// verifiable across process restarts.
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load_from(path);
        }
        let keyring = Self::generate_persistent()?;
        keyring
            .persist_to(path)
            .with_context(|| format!("persist witness key to {}", path.display()))?;
        tracing::info!(kid = %keyring.kid, path = %path.display(), "generated new witness signing key");
        Ok(keyring)
    }

    /// Return the process-global keyring, initialising it from the default path
    /// on first use. Subsequent calls return the cached instance.
    pub fn global() -> Result<&'static Self> {
        if let Some(existing) = GLOBAL_WITNESS_KEYRING.get() {
            return Ok(existing);
        }
        let path = default_witness_key_path()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve witness key path: HOME is unset"))?;
        let keyring = Self::load_or_generate(&path)?;
        // Tolerate a concurrent initialiser winning the race: either way the
        // installed value is a valid keyring for this process.
        let _ = GLOBAL_WITNESS_KEYRING.set(keyring);
        GLOBAL_WITNESS_KEYRING
            .get()
            .ok_or_else(|| anyhow::anyhow!("witness keyring failed to initialise"))
    }

    /// Install an in-memory keyring as the process-global one, for tests that
    /// exercise the production gate path (which calls [`Self::global`]) without
    /// touching `$HOME` or the filesystem. Idempotent: if the global keyring is
    /// already set (by an earlier test in the same binary), that one is returned.
    #[cfg(test)]
    pub fn global_for_tests() -> &'static Self {
        if let Some(existing) = GLOBAL_WITNESS_KEYRING.get() {
            return existing;
        }
        let keyring = Self::generate_for_tests().expect("test: generate witness keyring");
        let _ = GLOBAL_WITNESS_KEYRING.set(keyring);
        GLOBAL_WITNESS_KEYRING
            .get()
            .expect("test: global witness keyring installed")
    }

    fn generate_persistent() -> Result<Self> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("generate Ed25519 keypair"))?;
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow::anyhow!("load generated Ed25519 keypair"))?;
        Ok(Self {
            kid: format!("wit-{}", Uuid::now_v7()),
            pkcs8: pkcs8.as_ref().to_vec(),
            public_key: key_pair.public_key().as_ref().to_vec(),
        })
    }

    fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read witness key {}", path.display()))?;
        let persisted: PersistedWitnessKey =
            serde_json::from_str(&raw).with_context(|| format!("parse witness key {}", path.display()))?;
        if persisted.alg != "Ed25519" {
            bail!("unsupported witness key alg: {}", persisted.alg);
        }
        let pkcs8 = BASE64_STD
            .decode(persisted.secret_b64.as_bytes())
            .context("decode witness secret key")?;
        let public_key = BASE64_STD
            .decode(persisted.public_b64.as_bytes())
            .context("decode witness public key")?;
        // Validate the secret is actually loadable and matches the stored public
        // key before trusting it.
        let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow::anyhow!("load persisted Ed25519 key"))?;
        if key_pair.public_key().as_ref() != public_key.as_slice() {
            bail!("witness key public/secret mismatch in {}", path.display());
        }
        Ok(Self {
            kid: persisted.kid,
            pkcs8,
            public_key,
        })
    }

    fn persist_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create key dir {}", parent.display()))?;
        }
        let persisted = PersistedWitnessKey {
            kid: self.kid.clone(),
            alg: "Ed25519".to_string(),
            secret_b64: BASE64_STD.encode(&self.pkcs8),
            public_b64: BASE64_STD.encode(&self.public_key),
            created_at: Utc::now().to_rfc3339(),
        };
        let json = serde_json::to_string_pretty(&persisted).context("serialize witness key")?;
        write_private_file(path, json.as_bytes())?;
        Ok(())
    }

    pub fn current_kid(&self) -> &str {
        &self.kid
    }

    fn key_pair(&self) -> Result<Ed25519KeyPair> {
        Ed25519KeyPair::from_pkcs8(&self.pkcs8).map_err(|_| anyhow::anyhow!("load Ed25519 keypair"))
    }

    fn verify_key(&self, kid: &str) -> Option<UnparsedPublicKey<&[u8]>> {
        (kid == self.kid).then_some(UnparsedPublicKey::new(&ED25519, self.public_key.as_slice()))
    }
}

/// Write `bytes` to `path`, ensuring the file is created with owner-only (`0600`)
/// permissions on Unix before any secret material is written.
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("open witness key for writing {}", path.display()))?;
    // On Unix the 0600 mode above only applies when the file is freshly created;
    // tighten an existing file too so a re-persist never leaves it world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        file.set_permissions(perms)
            .with_context(|| format!("set 0600 on witness key {}", path.display()))?;
    }
    file.write_all(bytes)
        .with_context(|| format!("write witness key {}", path.display()))?;
    Ok(())
}

impl ApprovalGrantV2 {
    pub const VERSION: u8 = 2;

    pub fn issue_one_shot(
        keyring: &WitnessKeyring,
        subject: Subject,
        issuer_authority: IssuerAuthority,
        op_id: impl Into<String>,
        risk_level: RiskLevel,
    ) -> Result<Self> {
        Self::issue_one_shot_match(keyring, subject, issuer_authority, op_id, OpIdMatch::Exact, risk_level)
    }

    /// Like [`Self::issue_one_shot`] but with an explicit op-id match mode. Use
    /// [`OpIdMatch::GlobPattern`] for tools whose gate op-id is derived from
    /// runtime-resolved state (the loop can only bind a `{tool}:{verb}:*` scope).
    ///
    /// A `GlobPattern` is validated by [`conservative_glob_match`] at gate time:
    /// it must contain a `:`-bearing prefix of length ≥3 and exactly one trailing
    /// `*`, so a bare `*` / tool-name wildcard can never be authorized.
    pub fn issue_one_shot_match(
        keyring: &WitnessKeyring,
        subject: Subject,
        issuer_authority: IssuerAuthority,
        op_id: impl Into<String>,
        op_id_match: OpIdMatch,
        risk_level: RiskLevel,
    ) -> Result<Self> {
        let now = Utc::now();
        let mut grant = Self {
            grant_id: format!("grant-{}", Uuid::now_v7()),
            version: Self::VERSION,
            subject,
            issuer: Issuer {
                authority: issuer_authority,
                authority_id: "runtime".to_string(),
                public_key_id: keyring.current_kid().to_string(),
            },
            capability: Capability {
                op_id: op_id.into(),
                op_id_match,
                risk_level,
            },
            resource_constraints: ResourceConstraints::default(),
            issued_at: now,
            not_before: now,
            expires_at: now + Duration::seconds(60),
            max_uses: 1,
            uses_consumed: 0,
            witness_signature: WitnessSignature::default(),
            related_task_id: None,
            related_message_event_id: None,
            revoked_at: None,
            revocation_reason: None,
        };
        sign_grant(keyring, &mut grant)?;
        Ok(grant)
    }

    #[must_use]
    pub fn is_time_valid(&self, now: DateTime<Utc>) -> bool {
        self.not_before <= now && now < self.expires_at
    }

    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    #[must_use]
    pub const fn has_uses_remaining(&self) -> bool {
        self.max_uses == 0 || self.uses_consumed < self.max_uses
    }

    #[must_use]
    pub fn matches_operation(&self, op_id: &str, risk_level: RiskLevel) -> bool {
        self.capability.risk_level >= risk_level
            && op_id_matches(&self.capability.op_id, &self.capability.op_id_match, op_id)
    }

    pub fn verify_for_operation(
        &self,
        keyring: &WitnessKeyring,
        op_id: &str,
        risk_level: RiskLevel,
        now: DateTime<Utc>,
    ) -> Result<()> {
        verify_grant(keyring, self)?;
        if self.is_revoked() {
            bail!("approval grant is revoked");
        }
        if !self.is_time_valid(now) {
            bail!("approval grant is outside its validity window");
        }
        if !self.has_uses_remaining() {
            bail!("approval grant has no remaining uses");
        }
        if !self.matches_operation(op_id, risk_level) {
            bail!("approval grant does not match requested operation");
        }
        Ok(())
    }

    /// Full v2 verification used on the production gate path.
    ///
    /// In addition to the checks in [`Self::verify_for_operation`] this binds the
    /// grant to the calling principal (threat M4: cross-tenant grant reuse).
    /// `caller_principal_id` is the trusted principal derived from the runtime
    /// scope at the gate; when present it must exactly match the subject the
    /// grant was issued for. A `None` caller means no principal context is
    /// available (e.g. background runners) and only the cryptographic / temporal
    /// checks apply.
    pub fn verify_for_operation_bound(
        &self,
        keyring: &WitnessKeyring,
        op_id: &str,
        risk_level: RiskLevel,
        caller_principal_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.verify_for_operation(keyring, op_id, risk_level, now)?;
        // M4 cross-tenant defense, fail-closed: a v2 grant MUST be bound to a
        // concrete, non-empty caller principal. A `None` caller, an empty caller
        // string, or an empty grant subject is denied outright. There is NO
        // "None skips" or "empty == empty passes" branch — without a trusted
        // principal context the v2 grant cannot be honoured.
        let Some(caller) = caller_principal_id else {
            bail!("approval grant requires a caller principal (none provided)");
        };
        if caller.is_empty() {
            bail!("approval grant requires a non-empty caller principal");
        }
        if self.subject.principal_id.is_empty() {
            bail!("approval grant subject principal is empty");
        }
        if self.subject.principal_id != caller {
            bail!("approval grant principal mismatch: grant subject does not match caller principal");
        }
        Ok(())
    }

    pub fn revoke(&mut self, reason: impl Into<String>, revoked_at: DateTime<Utc>) {
        self.revoked_at = Some(revoked_at);
        self.revocation_reason = Some(reason.into());
    }
}

fn op_id_matches(granted_op_id: &str, match_mode: &OpIdMatch, requested_op_id: &str) -> bool {
    match match_mode {
        OpIdMatch::Exact => granted_op_id == requested_op_id,
        OpIdMatch::GlobPattern(pattern) => conservative_glob_match(pattern, requested_op_id),
    }
}

fn conservative_glob_match(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return false;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    if pattern.matches('*').count() != 1 || !pattern.ends_with('*') {
        return false;
    }
    let prefix = pattern.trim_end_matches('*');
    prefix.len() >= 3 && prefix.contains(':') && value.starts_with(prefix)
}

pub fn sign_grant(keyring: &WitnessKeyring, grant: &mut ApprovalGrantV2) -> Result<()> {
    grant.version = ApprovalGrantV2::VERSION;
    grant.issuer.public_key_id = keyring.current_kid().to_string();
    grant.witness_signature = WitnessSignature::default();
    let canonical = canonical_grant_bytes(grant)?;
    let payload_sha = sha256_hex(&canonical);
    let key_pair = keyring.key_pair()?;
    let signature = key_pair.sign(&canonical);
    grant.witness_signature = WitnessSignature {
        alg: "Ed25519".to_string(),
        signature_b64: URL_SAFE_NO_PAD.encode(signature.as_ref()),
        signed_payload_sha256: payload_sha,
    };
    Ok(())
}

pub fn verify_grant(keyring: &WitnessKeyring, grant: &ApprovalGrantV2) -> Result<()> {
    if grant.version != ApprovalGrantV2::VERSION {
        bail!("unsupported approval grant version: {}", grant.version);
    }
    if grant.witness_signature.alg != "Ed25519" {
        bail!(
            "unsupported approval grant signature alg: {}",
            grant.witness_signature.alg
        );
    }
    let verify_key = keyring
        .verify_key(&grant.issuer.public_key_id)
        .ok_or_else(|| anyhow::anyhow!("unknown witness key id: {}", grant.issuer.public_key_id))?;
    let mut unsigned = grant.clone();
    unsigned.witness_signature = WitnessSignature::default();
    let canonical = canonical_grant_bytes(&unsigned)?;
    let payload_sha = sha256_hex(&canonical);
    if payload_sha != grant.witness_signature.signed_payload_sha256 {
        bail!("signed_payload_sha256 mismatch");
    }
    let signature = URL_SAFE_NO_PAD
        .decode(grant.witness_signature.signature_b64.as_bytes())
        .context("decode approval grant signature")?;
    verify_key
        .verify(&canonical, &signature)
        .map_err(|_| anyhow::anyhow!("approval grant signature verification failed"))?;
    Ok(())
}

/// Produce the byte payload that is signed/verified for a grant.
///
/// `ApprovalGrantV2` contains only scalar fields and `Option<scalar>` /
/// `Option<Vec<scalar>>` members (no `HashMap`), so `serde_json` emits a
/// deterministic, struct-declaration-ordered encoding. This is sufficient for
/// the in-process sign-then-verify path used by the runtime gate, where the same
/// binary produces and checks the signature.
///
/// NOTE: this is *not* RFC 8785 JCS. External / cross-implementation verifiers
/// (e.g. SPIFFE tooling) are out of scope for the production gate wiring and are
/// deferred per design d08 §15 #1. If such interop is added later, swap this for
/// a JCS canonicaliser; the signature scheme itself (Ed25519 over these bytes)
/// stays the same.
fn canonical_grant_bytes(grant: &ApprovalGrantV2) -> Result<Vec<u8>> {
    serde_json::to_vec(grant).context("serialize approval grant canonical payload")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_subject() -> Subject {
        Subject {
            agent_id: "prx:test:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:workspace:alice".to_string(),
            workspace_id: "workspace".to_string(),
            session_key: Some("session-a".to_string()),
        }
    }

    #[test]
    fn approval_grant_ed25519_signs_and_verifies() -> Result<()> {
        let keyring = WitnessKeyring::generate_for_tests()?;
        let grant = ApprovalGrantV2::issue_one_shot(
            &keyring,
            test_subject(),
            IssuerAuthority::HumanReview,
            "file_write:write:abc",
            RiskLevel::Medium,
        )?;

        assert_eq!(grant.version, 2);
        assert_eq!(grant.witness_signature.alg, "Ed25519");
        verify_grant(&keyring, &grant)
    }

    #[test]
    fn approval_grant_tamper_detect_rejects_modified_payload() -> Result<()> {
        let keyring = WitnessKeyring::generate_for_tests()?;
        let mut grant = ApprovalGrantV2::issue_one_shot(
            &keyring,
            test_subject(),
            IssuerAuthority::HumanReview,
            "file_write:write:abc",
            RiskLevel::Medium,
        )?;
        grant.capability.op_id = "file_write:write:def".to_string();

        let error = verify_grant(&keyring, &grant).expect_err("tampered grant must fail verification");
        assert!(error.to_string().contains("signed_payload_sha256 mismatch"));
        Ok(())
    }

    #[test]
    fn approval_grant_verify_for_operation_checks_time_revocation_use_and_op() -> Result<()> {
        let keyring = WitnessKeyring::generate_for_tests()?;
        let mut grant = ApprovalGrantV2::issue_one_shot(
            &keyring,
            test_subject(),
            IssuerAuthority::HumanReview,
            "nodes:exec:n1",
            RiskLevel::High,
        )?;
        let now = Utc::now();

        grant.verify_for_operation(&keyring, "nodes:exec:n1", RiskLevel::High, now)?;

        let wrong_op = grant
            .verify_for_operation(&keyring, "nodes:exec:n2", RiskLevel::High, now)
            .expect_err("wrong operation should fail");
        assert!(wrong_op.to_string().contains("does not match"));

        grant.uses_consumed = grant.max_uses;
        sign_grant(&keyring, &mut grant)?;
        let no_uses = grant
            .verify_for_operation(&keyring, "nodes:exec:n1", RiskLevel::High, now)
            .expect_err("consumed grant should fail");
        assert!(no_uses.to_string().contains("no remaining uses"));

        grant.uses_consumed = 0;
        grant.revoke("operator revoked", now);
        sign_grant(&keyring, &mut grant)?;
        let revoked = grant
            .verify_for_operation(&keyring, "nodes:exec:n1", RiskLevel::High, now)
            .expect_err("revoked grant should fail");
        assert!(revoked.to_string().contains("revoked"));
        Ok(())
    }

    #[test]
    fn witness_keyring_load_or_generate_persists_and_reloads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("keys").join("runtime_witness.key");

        let first = WitnessKeyring::load_or_generate(&path)?;
        assert!(path.exists(), "key file must be persisted");

        // Signature made by the first instance must verify under a freshly
        // loaded instance (proves the secret survived the disk round-trip).
        let grant = ApprovalGrantV2::issue_one_shot(
            &first,
            test_subject(),
            IssuerAuthority::RuntimeAutomatic,
            "file_write:write:abc",
            RiskLevel::Medium,
        )?;

        let reloaded = WitnessKeyring::load_or_generate(&path)?;
        assert_eq!(reloaded.current_kid(), first.current_kid());
        verify_grant(&reloaded, &grant)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "witness key must be 0600");
        }
        Ok(())
    }

    #[test]
    fn verify_for_operation_bound_rejects_cross_tenant_principal() -> Result<()> {
        let keyring = WitnessKeyring::generate_for_tests()?;
        let grant = ApprovalGrantV2::issue_one_shot(
            &keyring,
            test_subject(), // principal_id = "telegram:alice"
            IssuerAuthority::HumanReview,
            "file_write:write:abc",
            RiskLevel::Medium,
        )?;
        let now = Utc::now();

        // Same principal → ok.
        grant.verify_for_operation_bound(
            &keyring,
            "file_write:write:abc",
            RiskLevel::Medium,
            Some("telegram:alice"),
            now,
        )?;

        // Different principal → cross-tenant reuse rejected.
        let err = grant
            .verify_for_operation_bound(
                &keyring,
                "file_write:write:abc",
                RiskLevel::Medium,
                Some("telegram:mallory"),
                now,
            )
            .expect_err("cross-tenant grant must be rejected");
        assert!(err.to_string().contains("principal mismatch"));

        // No caller context → fail-closed (M4): a v2 grant is never honoured
        // without a trusted, non-empty caller principal.
        let none_caller = grant
            .verify_for_operation_bound(&keyring, "file_write:write:abc", RiskLevel::Medium, None, now)
            .expect_err("None caller principal must be rejected (fail-closed)");
        assert!(none_caller.to_string().contains("caller principal"));

        // Empty caller string → also rejected.
        let empty_caller = grant
            .verify_for_operation_bound(&keyring, "file_write:write:abc", RiskLevel::Medium, Some(""), now)
            .expect_err("empty caller principal must be rejected");
        assert!(empty_caller.to_string().contains("caller principal"));
        Ok(())
    }

    #[test]
    fn approval_grant_conservative_glob_requires_scoped_prefix() {
        assert!(conservative_glob_match("memory_forget:*", "memory_forget:owner:alice"));
        assert!(!conservative_glob_match("*", "memory_forget:owner:alice"));
        assert!(!conservative_glob_match("memory*", "memory_forget:owner:alice"));
        assert!(!conservative_glob_match(
            "memory_forget:*:alice",
            "memory_forget:owner:alice"
        ));
    }
}

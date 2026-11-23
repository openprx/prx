//! Atomic, private, persisted Ed25519 instance identities.
use anyhow::{Context, Result, ensure};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    instance_id: uuid::Uuid,
    generation: uuid::Uuid,
    spiffe_id: String,
}
pub struct Identity {
    pub instance_id: uuid::Uuid,
    pub spiffe_id: String,
    pub fingerprint: String,
    pub expires_at: i64,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}
fn directory(config: &crate::config::Config) -> PathBuf {
    config.workspace_dir.join("a2a")
}
fn read(path: &Path, private: bool) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= 65536 && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "invalid A2A identity file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private {
            ensure!(
                meta.permissions().mode().trailing_zeros() >= 7,
                "A2A private file permissions exceed 0600"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "A2A identity exceeds file bound");
    Ok(bytes)
}
fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temp = dir.join(format!(".{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(name))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
pub(crate) fn optional(config: &crate::config::Config) -> Result<Option<Identity>> {
    if directory(config).join("identity.toml").try_exists()? {
        Ok(Some(load(config)?))
    } else {
        Ok(None)
    }
}
pub fn load(config: &crate::config::Config) -> Result<Identity> {
    load_named(config, "identity.toml")
}
fn load_named(config: &crate::config::Config, name: &str) -> Result<Identity> {
    let dir = directory(config);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(&dir)?.permissions().mode().trailing_zeros() >= 6,
            "A2A directory permissions exceed 0700"
        );
    }
    ensure!(
        !fs::symlink_metadata(&dir)?.file_type().is_symlink(),
        "A2A directory cannot be a symlink"
    );
    let record: Record = toml::from_str(std::str::from_utf8(&read(&dir.join(name), true)?)?)?;
    ensure!(
        super::peers::instance(&record.spiffe_id)? == record.instance_id,
        "A2A identity instance mismatch"
    );
    let cert = read(&dir.join(format!("{}.der", record.generation)), false)?;
    let key = read(&dir.join(format!("{}.key.pem", record.generation)), true)?;
    let (_, parsed) =
        x509_parser::parse_x509_certificate(&cert).map_err(|_| anyhow::anyhow!("invalid A2A certificate"))?;
    let expires_at = parsed.validity().not_after.timestamp();
    let fingerprint = fingerprint(&cert);
    let cert = CertificateDer::from(cert);
    use rustls::pki_types::pem::PemObject as _;
    let key = PrivateKeyDer::from_pem_slice(&key).context("invalid A2A private key")?;
    let identity = Identity {
        instance_id: record.instance_id,
        spiffe_id: record.spiffe_id,
        fingerprint,
        expires_at,
        cert,
        key,
    };
    let _ = rustls::sign::CertifiedKey::from_der(
        vec![identity.cert.clone()],
        identity.key.clone_key(),
        &rustls::crypto::ring::default_provider(),
    )?;
    Ok(identity)
}
pub fn init(config: &crate::config::Config, rotate: bool) -> Result<Identity> {
    let dir = directory(config);
    if dir.join("identity.toml").exists() && !rotate {
        return load(config);
    }
    let old = if dir.join("identity.toml").exists() {
        Some(load(config)?)
    } else {
        None
    };
    if rotate && dir.join("pending.toml").exists() {
        let pending = load_named(config, "pending.toml")?;
        let current = load(config)?;
        ensure!(
            pending.instance_id == current.instance_id && pending.spiffe_id == current.spiffe_id,
            "staged identity differs from current instance"
        );
        super::tls::check_certificate(&pending.cert, &pending.spiffe_id, rustls::pki_types::UnixTime::now())?;
        ensure!(
            !config
                .a2a
                .peers
                .values()
                .any(|peer| peer.spiffe_id == pending.spiffe_id || peer.fingerprints.contains(&pending.fingerprint)),
            "local identity cannot be its own peer"
        );
        fs::rename(dir.join("pending.toml"), dir.join("identity.toml"))?;
        fs::File::open(&dir)?.sync_all()?;
        return load(config);
    }
    generate(config, "identity.toml", old)
}
pub fn prepare_rotation(config: &crate::config::Config) -> Result<Identity> {
    let current = load(config)?;
    if directory(config).join("pending.toml").exists() {
        return load_named(config, "pending.toml");
    }
    generate(config, "pending.toml", Some(current))
}
fn generate(config: &crate::config::Config, name: &str, old: Option<Identity>) -> Result<Identity> {
    let dir = directory(config);
    fs::create_dir_all(&config.workspace_dir)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(&dir)?.permissions().mode().trailing_zeros() >= 6,
            "A2A directory permissions exceed 0700"
        );
    }
    ensure!(
        !fs::symlink_metadata(&dir)?.file_type().is_symlink(),
        "A2A directory cannot be a symlink"
    );
    let instance_id = old
        .as_ref()
        .map_or_else(uuid::Uuid::new_v4, |identity| identity.instance_id);
    let spiffe_id = old.map_or_else(
        || format!("spiffe://{}/prx/{instance_id}", config.a2a.trust_domain),
        |identity| identity.spiffe_id,
    );
    super::peers::instance(&spiffe_id)?;
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![rcgen::SanType::URI(spiffe_id.clone().try_into()?)];
    params.is_ca = rcgen::IsCa::NoCa;
    let time = chrono::Utc::now();
    let now = u64::try_from(time.timestamp())?;
    let expires = u64::try_from(
        time.checked_add_months(chrono::Months::new(60))
            .context("certificate expiry overflow")?
            .timestamp(),
    )?;
    params.not_before = rcgen::date_time_ymd(1970, 1, 1) + std::time::Duration::from_secs(now.saturating_sub(300));
    params.not_after = rcgen::date_time_ymd(1970, 1, 1) + std::time::Duration::from_secs(expires);
    let cert = params.self_signed(&key)?;
    let generation = uuid::Uuid::new_v4();
    write(&dir, &format!("{generation}.key.pem"), key.serialize_pem().as_bytes())?;
    write(&dir, &format!("{generation}.der"), cert.der())?;
    write(
        &dir,
        name,
        toml::to_string(&Record {
            instance_id,
            generation,
            spiffe_id,
        })?
        .as_bytes(),
    )?;
    load_named(config, name)
}
pub(crate) fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

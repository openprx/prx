//! One-use local transport of a complete, already sealed worker manifest.
//! A reference never grants authority; ordinary manifest verification follows.
use super::protocol::WorkerManifest;
use anyhow::{Context, Result};
use ring::hmac;
use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REFERENCE_BYTES: usize = 3071;
const SNAPSHOT: &str = "manifest.json";
const CLAIM: &str = "claimed";
const DOMAIN: &[u8] = b"prx-worker-manifest-reference-v1\0";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceBody {
    version: u8,
    directory: PathBuf,
    nonce: String,
    run_id: String,
    config_generation: String,
    bytes: u64,
    digest: [u8; 32],
    capability: String,
    expiry: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    body: ReferenceBody,
    seal: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    worker_manifest_reference: Reference,
}

fn signing_bytes(body: &ReferenceBody) -> Result<Vec<u8>> {
    let mut bytes = DOMAIN.to_vec();
    serde_json::to_writer(&mut bytes, body)?;
    Ok(bytes)
}

/// Retained by the parent until the owned child lifecycle has completed.
/// TempDir also removes the private snapshot on pre-spawn failure or unwind.
pub(crate) struct PreparedManifest {
    directory: Option<tempfile::TempDir>,
    launched: bool,
    line: String,
}

impl PreparedManifest {
    pub(crate) fn prepare(manifest: &WorkerManifest, key: &hmac::Key, expiry: u64) -> Result<Self> {
        let bytes = serde_json::to_vec(manifest)?;
        anyhow::ensure!(
            u64::try_from(bytes.len())? <= MAX_SNAPSHOT_BYTES,
            "worker snapshot exceeds byte bound"
        );
        // Resolve the operator's workspace once, then reject symbolic links in
        // every component when opening the actual private snapshot directory.
        let workspace = std::fs::canonicalize(&manifest.workspace_dir)?;
        let directory = tempfile::Builder::new()
            .prefix(".prx-worker-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(&workspace)?;
        let dir = open_private_directory(directory.path())?;
        let fd = rustix::fs::openat(
            &dir,
            SNAPSHOT,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        file.write_all(&bytes)?;
        file.sync_all()?;
        file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
        file.sync_all()?;
        drop(file); // No writable handle survives publication.
        let nonce = directory
            .path()
            .file_name()
            .context("worker snapshot directory has no name")?
            .to_str()
            .context("worker snapshot directory name is not UTF-8")?
            .to_owned();
        let body = ReferenceBody {
            version: 1,
            directory: directory.path().to_path_buf(),
            nonce,
            run_id: manifest.run_id.clone(),
            config_generation: manifest.config_generation.clone(),
            bytes: u64::try_from(bytes.len())?,
            digest: Sha256::digest(&bytes).into(),
            capability: manifest
                .parent_capability
                .clone()
                .context("worker snapshot capability missing")?,
            expiry,
        };
        let seal = hmac::sign(key, &signing_bytes(&body)?).as_ref().to_vec();
        let line = serde_json::to_string(&Envelope {
            worker_manifest_reference: Reference { body, seal },
        })?;
        anyhow::ensure!(
            line.len() <= MAX_REFERENCE_BYTES,
            "worker reference exceeds stdin byte bound"
        );
        Ok(Self {
            directory: Some(directory),
            launched: false,
            line,
        })
    }

    pub(crate) fn line(&self) -> &str {
        &self.line
    }

    pub(crate) const fn mark_spawned(&mut self) {
        self.launched = true;
    }

    /// Call only after independently confirmed child reap.
    pub(crate) fn close(&mut self) -> Result<()> {
        if let Some(directory) = self.directory.take() {
            directory.close().context("remove owned worker manifest snapshot")?;
        }
        Ok(())
    }
}

impl Drop for PreparedManifest {
    fn drop(&mut self) {
        if self.launched
            && let Some(directory) = self.directory.take()
        {
            // An error/cancellation with unconfirmed child termination must not
            // invalidate transport still needed by that owned child. Workspace
            // recovery owns this retained private artifact; never log its data.
            let _retained = directory.keep();
        }
    }
}

fn open_private_directory(path: &Path) -> Result<OwnedFd> {
    anyhow::ensure!(path.is_absolute(), "worker snapshot directory must be absolute");
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = rustix::fs::open("/", flags, Mode::empty())?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => dir = rustix::fs::openat(&dir, name, flags, Mode::empty())?,
            _ => anyhow::bail!("worker snapshot directory is not a clean absolute path"),
        }
    }
    let stat = rustix::fs::fstat(&dir)?;
    anyhow::ensure!(
        stat.st_uid == rustix::process::geteuid().as_raw() && stat.st_mode & 0o777 == 0o700,
        "worker snapshot directory is not private and owned"
    );
    Ok(dir)
}

pub(crate) fn restore(
    value: serde_json::Value,
    key: &hmac::Key,
    expected_capability: &str,
    expected_expiry: u64,
    now: u64,
) -> Result<WorkerManifest> {
    let envelope: Envelope = serde_json::from_value(value)?;
    let reference = envelope.worker_manifest_reference;
    let body = &reference.body;
    anyhow::ensure!(body.version == 1, "worker manifest reference version is unsupported");
    hmac::verify(key, &signing_bytes(body)?, &reference.seal)
        .map_err(|_| anyhow::anyhow!("worker manifest reference signature is invalid"))?;
    // The existing process channel binds this reference to this launch. No
    // path is opened before HMAC, expiry and capability checks pass.
    anyhow::ensure!(
        body.capability == expected_capability && body.expiry == expected_expiry && now <= body.expiry,
        "worker manifest reference differs from active transport"
    );
    anyhow::ensure!(
        body.bytes > 0 && body.bytes <= MAX_SNAPSHOT_BYTES,
        "worker snapshot byte bound is invalid"
    );
    anyhow::ensure!(
        body.directory.file_name().and_then(|name| name.to_str()) == Some(body.nonce.as_str())
            && body.nonce.starts_with(".prx-worker-"),
        "worker snapshot nonce differs"
    );
    let dir = open_private_directory(&body.directory)?;
    // Atomic exclusive creation makes this launch reference single-use even
    // if two workers race to consume it. Retain the marker until parent cleanup.
    let claim = rustix::fs::openat(
        &dir,
        CLAIM,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .context("worker manifest reference was already claimed or cannot be claimed")?;
    drop(claim);
    let fd = rustix::fs::openat(
        &dir,
        SNAPSHOT,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut file = File::from(fd);
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file()
            && before.uid() == rustix::process::geteuid().as_raw()
            && before.nlink() == 1
            && before.mode() & 0o777 == 0o400
            && before.len() == body.bytes,
        "worker snapshot is not an immutable owned regular file of the expected size"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(body.bytes)?);
    (&mut file).take(body.bytes.saturating_add(1)).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? == body.bytes
            && after.len() == before.len()
            && after.nlink() == 1
            && after.mode() == before.mode()
            && <[u8; 32]>::from(Sha256::digest(&bytes)) == body.digest,
        "worker manifest snapshot content differs"
    );
    let manifest: WorkerManifest = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        manifest.run_id == body.run_id
            && manifest.config_generation == body.config_generation
            && manifest.parent_capability.as_deref() == Some(body.capability.as_str()),
        "worker snapshot manifest identity differs"
    );
    Ok(manifest)
}

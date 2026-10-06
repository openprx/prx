//! Conservative, read-only process identity checks for signed launch recovery.
use crate::decision::{DecisionKey, key::KeyDomain};

pub(in crate::tools::sessions_spawn::process_terminal) fn context_mac(
    key: &DecisionKey,
) -> anyhow::Result<Option<[u8; 32]>> {
    Ok(context()?.map(|context| key.hmac(KeyDomain::StateFingerprint, &[b"worker-process-context-v1", &context])))
}

#[cfg(target_os = "linux")]
fn context() -> anyhow::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::MetadataExt;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot = uuid::Uuid::parse_str(boot.trim())?;
    anyhow::ensure!(!boot.is_nil(), "worker recovery boot identity is empty");
    // Follow the namespace link: its inode distinguishes containers sharing a
    // kernel boot. A process in another PID namespace is never probed as ours.
    let namespace = std::fs::metadata("/proc/self/ns/pid")?;
    Ok(Some(serde_json::to_vec(&(
        "linux",
        boot.to_string(),
        namespace.dev(),
        namespace.ino(),
    ))?))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn context() -> anyhow::Result<Option<Vec<u8>>> {
    let mut buffer = [0_u8; 64];
    let mut length = buffer.len();
    // SAFETY: the name is NUL terminated; buffer and length are writable for
    // their declared sizes. Null newp with zero newlen performs only a read.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    anyhow::ensure!(
        length > 1 && length <= buffer.len(),
        "worker recovery boot identity has invalid length"
    );
    let text = std::ffi::CStr::from_bytes_with_nul(
        buffer
            .get(..length)
            .ok_or_else(|| anyhow::anyhow!("process identity exceeds buffer"))?,
    )?
    .to_str()?;
    let boot = uuid::Uuid::parse_str(text)?;
    anyhow::ensure!(!boot.is_nil(), "worker recovery boot identity is empty");
    Ok(Some(serde_json::to_vec(&("macos", boot.to_string()))?))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn context() -> anyhow::Result<Option<Vec<u8>>> {
    Ok(None)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
pub(in crate::tools::sessions_spawn::process_terminal) fn absent(pid: u32) -> anyhow::Result<bool> {
    let pid = libc::pid_t::try_from(pid)?;
    anyhow::ensure!(pid > 0, "worker recovery requires a positive process ID");
    // SAFETY: signal zero checks a single positive PID without delivering a
    // signal or modifying the process. ESRCH alone establishes absence.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(true),
        Some(libc::EPERM) => Ok(false),
        _ => Err(error.into()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(in crate::tools::sessions_spawn::process_terminal) fn absent(_pid: u32) -> anyhow::Result<bool> {
    Ok(false)
}

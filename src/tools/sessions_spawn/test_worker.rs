#[cfg(test)]
use super::ProcessFinalization;
#[cfg(test)]
use crate::providers;
#[cfg(test)]
use anyhow::Context as _;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
pub(crate) struct TestWorkerRecoveryResult {
    pub(crate) launch_event_id: i64,
    pub(crate) result: anyhow::Result<Option<bool>>,
}

/// Read-only fixture observation of the actual built worker manifest. No key,
/// transport seal or grant bytes leave the launch producer through this seam.
#[cfg(test)]
pub(crate) struct TestWorkerRouteObservation {
    pub(crate) run_id: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) owner_id: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) route_hint: Option<String>,
    pub(crate) route_source_provider: Option<String>,
    pub(crate) route_credential: Option<providers::credentials::CredentialRef>,
    pub(crate) key_sha256: Option<[u8; 32]>,
    pub(crate) route_uses_root_connection: bool,
    pub(crate) direct_uses_root_connection: bool,
    pub(crate) child_source_present: bool,
    pub(crate) child_allowed_count: Option<usize>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestWorkerRouteCapture {
    pub(crate) total: usize,
    pub(crate) first: Option<TestWorkerRouteObservation>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestWorkerProtocolCase {
    HoldAfterThreeSteers,
    CloseAfterManifest,
}

#[cfg(test)]
pub(crate) struct TestWorkerProtocolControl {
    pub(super) case: TestWorkerProtocolCase,
    claimed: std::sync::atomic::AtomicBool,
    pub(super) termination_calls: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) terminal: parking_lot::Mutex<Option<TestWorkerProtocolTerminal>>,
    pub(super) diagnostic: Arc<parking_lot::Mutex<TestWorkerProtocolDiagnostic>>,
}

#[cfg(test)]
#[derive(Default, serde::Serialize, Debug)]
pub(crate) struct TestWorkerProtocolDiagnostic {
    manifest_bytes: Option<usize>,
    component_bytes: std::collections::BTreeMap<String, usize>,
    precondition_passed: Option<bool>,
    pub(super) spawned: bool,
    pub(super) pid: Option<u32>,
    pub(super) actual_pgid: Option<i32>,
    pub(super) start_identity: Option<(u64, u64)>,
    pub(super) identity_class: Option<&'static str>,
    pub(super) registry_id: Option<u64>,
    pub(super) launch_persisted: bool,
    pub(super) reap_confirmed: bool,
    pub(super) exit_code: Option<i32>,
    pub(super) exit_signal: Option<i32>,
    pub(super) registration_marked_reaped: bool,
    pub(super) lifecycle_class: Option<&'static str>,
    pub(super) panic_cleanup_class: Option<&'static str>,
    pub(super) finalization: Option<&'static str>,
    fixture_cleanup_class: Option<&'static str>,
    process_cleanup_class: Option<&'static str>,
    owner_cleanup_class: Option<&'static str>,
}

#[cfg(test)]
pub(super) fn test_worker_cleanup_error_class(error: &anyhow::Error) -> &'static str {
    // Inspect locally; retain only a closed class, never rendered error text.
    let text = error.to_string();
    if text.contains("failed to kill session-worker process group") {
        "group_signal_failed"
    } else if text.contains("direct-child") || text.contains("direct child") {
        "direct_child_cleanup_failed"
    } else if text.contains("wait") || text.contains("reap") {
        "wait_or_reap_failed"
    } else {
        "other_cleanup_error"
    }
}

#[cfg(all(test, unix))]
#[allow(unsafe_code)]
pub(super) fn test_worker_process_identity(pid: u32) -> (Option<i32>, Option<(u64, u64)>, &'static str) {
    let Ok(pid) = i32::try_from(pid) else {
        return (None, None, "invalid_pid");
    };
    // SAFETY: getpgid observes one actual positive child PID without signalling.
    let pgid = unsafe { libc::getpgid(pid) };
    let pgid = (pgid > 0).then_some(pgid);
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain C output buffer has the exact PROC_PIDTBSDINFO size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: pid is the positive spawned child; info is writable for exactly
        // size bytes of PROC_PIDTBSDINFO. libproc validates pid and writes the buffer.
        let received = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if received == size {
            return (
                pgid,
                Some((info.pbi_start_tvsec, info.pbi_start_tvusec)),
                "macos_start_time",
            );
        }
        (pgid, None, "start_unavailable")
    }
    #[cfg(target_os = "linux")]
    {
        let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| {
                let tail = text.get(text.rfind(')')? + 1..)?;
                tail.split_whitespace().nth(19)?.parse::<u64>().ok()
            });
        (
            pgid,
            start.map(|ticks| (ticks, 0)),
            if start.is_some() {
                "linux_start_ticks"
            } else {
                "start_unavailable"
            },
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (pgid, None, "start_unsupported")
    }
}

#[cfg(test)]
pub(crate) struct TestWorkerProtocolTerminal {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdin_held_through_reap: bool,
}

#[cfg(test)]
impl TestWorkerProtocolControl {
    pub(crate) fn new(case: TestWorkerProtocolCase) -> Self {
        Self {
            case,
            claimed: std::sync::atomic::AtomicBool::new(false),
            termination_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            terminal: parking_lot::Mutex::new(None),
            diagnostic: Arc::new(parking_lot::Mutex::new(TestWorkerProtocolDiagnostic::default())),
        }
    }

    pub(super) fn claim(&self) -> anyhow::Result<()> {
        self.claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .map_err(|_| anyhow::anyhow!("worker protocol control already claimed"))?;
        Ok(())
    }

    pub(super) fn check_serialized_precondition(&self, payload: &str) -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let fields = value.as_object().context("serialized worker manifest object missing")?;
        anyhow::ensure!(fields.len() <= 128, "manifest diagnostic field bound");
        let mut lengths = std::collections::BTreeMap::new();
        for (name, value) in fields {
            anyhow::ensure!(name.len() <= 128, "manifest diagnostic name bound");
            lengths.insert(name.clone(), serde_json::to_vec(value)?.len());
        }
        if let Some(source) = fields.get("operator_source").and_then(serde_json::Value::as_object) {
            anyhow::ensure!(source.len() <= 32, "operator source diagnostic field bound");
            for (name, value) in source {
                anyhow::ensure!(name.len() <= 128, "operator diagnostic name bound");
                lengths.insert(format!("operator_source.{name}"), serde_json::to_vec(value)?.len());
            }
        }
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.manifest_bytes = Some(payload.len());
        diagnostic.component_bytes = lengths;
        diagnostic.precondition_passed = Some(payload.len() < 3072);
        anyhow::ensure!(
            payload.len() < 3072,
            "worker stdin fixture precondition failed before spawn: manifest_bytes={}",
            payload.len()
        );
        Ok(())
    }

    pub(crate) fn serialized_precondition_failed(&self) -> bool {
        self.diagnostic.lock().precondition_passed == Some(false)
    }

    pub(crate) fn diagnostic_json(&self) -> anyhow::Result<Vec<u8>> {
        let mut value = serde_json::to_value(&*self.diagnostic.lock())?;
        let spawned = matches!(value.get("spawned").and_then(serde_json::Value::as_bool), Some(true));
        let object = value.as_object_mut().context("diagnostic object missing")?;
        let mut child = serde_json::Map::new();
        for name in [
            "pid",
            "actual_pgid",
            "start_identity",
            "identity_class",
            "registry_id",
            "launch_persisted",
            "reap_confirmed",
            "exit_code",
            "exit_signal",
            "registration_marked_reaped",
            "lifecycle_class",
            "panic_cleanup_class",
            "finalization",
        ] {
            child.insert(
                name.to_owned(),
                object.remove(name).context("diagnostic field missing")?,
            );
        }
        child.insert(
            "signal_attempts".to_owned(),
            serde_json::json!(self.termination_calls()),
        );
        object.insert(
            "child".to_owned(),
            if spawned {
                serde_json::Value::Object(child)
            } else {
                serde_json::Value::Null
            },
        );
        serde_json::to_vec_pretty(&value).map_err(Into::into)
    }

    pub(super) fn observe_reap_status(&self, status: std::process::ExitStatus) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.reap_confirmed = true;
        diagnostic.exit_code = status.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            diagnostic.exit_signal = status.signal();
        }
    }

    pub(crate) fn observe_cleanup_parts(&self, process: &'static str, owner: &'static str) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.process_cleanup_class = Some(process);
        diagnostic.owner_cleanup_class = Some(owner);
    }

    pub(crate) fn observe_fixture_cleanup(&self, finalization: Option<ProcessFinalization>, class: &'static str) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.finalization = finalization.map(|value| match value {
            ProcessFinalization::Natural => "natural",
            ProcessFinalization::Terminated => "terminated",
            ProcessFinalization::TerminationFailed => "termination_failed",
            ProcessFinalization::Unconfirmed => "unconfirmed",
        });
        diagnostic.fixture_cleanup_class = Some(class);
    }

    pub(crate) fn termination_calls(&self) -> usize {
        self.termination_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn take_terminal(&self) -> anyhow::Result<TestWorkerProtocolTerminal> {
        self.terminal
            .lock()
            .take()
            .context("actual worker terminal observation missing")
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(super) struct TestWorkerExecutable {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) private_home: PathBuf,
    pub(super) route_observer: Option<Arc<parking_lot::Mutex<TestWorkerRouteCapture>>>,
    pub(super) protocol: Option<Arc<TestWorkerProtocolControl>>,
}

#[cfg(test)]
impl TestWorkerExecutable {
    pub(super) fn verify(&self) -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Read as _;
        anyhow::ensure!(self.path.is_absolute(), "worker artifact path must be absolute");
        anyhow::ensure!(
            std::fs::symlink_metadata(&self.path)?.file_type().is_file(),
            "worker artifact must be a regular file, not a symlink"
        );
        let mut file = std::fs::File::open(&self.path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(buffer.get(..count).context("worker artifact read exceeded buffer")?);
        }
        anyhow::ensure!(
            hex::encode(hash.finalize()) == self.sha256,
            "worker artifact hash changed"
        );
        anyhow::ensure!(
            self.private_home.is_absolute() && std::fs::symlink_metadata(&self.private_home)?.file_type().is_dir(),
            "worker HOME must be an existing private directory, not a symlink"
        );
        Ok(())
    }
}

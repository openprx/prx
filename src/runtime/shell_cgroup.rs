//! Opt-in Linux cgroup v2 containment for joined executable skill runs.
use std::{
    io,
    path::{Path, PathBuf},
};

pub(super) struct ShellCgroup {
    path: PathBuf,
}

impl ShellCgroup {
    #[cfg(not(target_os = "linux"))]
    pub(super) fn create(_root: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed_cgroup_root requires Linux cgroup v2",
        ))
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub(super) fn create(root: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cgroup root must be absolute",
            ));
        }
        let root = root.canonicalize()?;
        let directory = std::fs::File::open(&root)?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the directory owns this fd, and stat points to writable storage.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatfs initialized the structure.
        if unsafe { stat.assume_init() }.f_type != libc::CGROUP2_SUPER_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cgroup root is not on cgroup v2",
            ));
        }
        let path = root.join(format!("prx-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        let group = Self { path };
        // Fail before spawning when kill is unavailable or delegation is incomplete.
        std::fs::OpenOptions::new()
            .write(true)
            .open(group.path.join("cgroup.kill"))?;
        std::fs::write(group.path.join("cgroup.max.descendants"), "0")?;
        Ok(group)
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub(super) fn attach(&self, command: &mut tokio::process::Command) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let procs = std::fs::OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))?;
        // SAFETY: only async-signal-safe syscalls and stack arithmetic run after fork.
        // Joining before exec prevents the shell from forking outside the group.
        unsafe {
            command.pre_exec(move || {
                let mut pid = u32::try_from(libc::getpid()).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                let mut bytes = [0u8; 16];
                let mut offset = bytes.len();
                while pid > 0 {
                    offset -= 1;
                    let digit = u8::try_from(pid % 10).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                    *bytes.get_mut(offset).ok_or(io::ErrorKind::InvalidData)? = b'0' + digit;
                    pid /= 10;
                }
                let length = bytes.len() - offset;
                let written = libc::write(procs.as_raw_fd(), bytes.as_ptr().add(offset).cast(), length);
                if usize::try_from(written).ok() != Some(length) {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn attach(&self, _command: &mut tokio::process::Command) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "cgroup v2 is unavailable"))
    }

    pub(super) fn kill(&self) -> io::Result<()> {
        std::fs::write(self.path.join("cgroup.kill"), "1")
    }

    pub(super) async fn finish(&self) -> io::Result<()> {
        self.kill()?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::remove_dir(&self.path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    }
}

impl Drop for ShellCgroup {
    fn drop(&mut self) {
        // Abort/panic still kills escaped session leaders synchronously. Directory
        // cleanup is best effort on drop; normal completion joins finish above.
        let _ = self.kill();
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_directory_is_refused_before_any_spawn() {
        let temp = tempfile::tempdir().unwrap();
        assert!(ShellCgroup::create(temp.path()).is_err());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires PRX_TEST_CGROUP_ROOT pointing to an empty delegated cgroup v2"]
    async fn setsid_descendant_is_killed_and_the_managed_group_is_removed() {
        let root = std::env::var_os("PRX_TEST_CGROUP_ROOT").expect("delegated cgroup root");
        let group = ShellCgroup::create(Path::new(&root)).unwrap();
        let path = group.path.clone();
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("child.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                r#"setsid sh -c 'echo $$ > "$1"; sleep 300' proof "$1" & wait"#,
                "proof",
            ])
            .arg(&marker);
        group.attach(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(value) = std::fs::read_to_string(&marker)
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline, "descendant did not start");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        assert!(membership.contains(path.file_name().unwrap().to_str().unwrap()));
        group.finish().await.unwrap();
        assert!(!child.wait().await.unwrap().success());
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        assert!(state.is_err() || state.as_ref().is_ok_and(|s| s.contains(") Z ")));
        assert!(!path.exists());
    }
}

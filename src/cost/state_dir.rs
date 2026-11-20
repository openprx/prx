//! Handle-based authority checks shared by all cost state access paths.
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(super) enum StateDirectoryError {
    #[error("cost state directory {path} has unsafe mode {mode:#o}; run chmod 700 {path}")]
    Mode { path: PathBuf, mode: rustix::fs::RawMode },
    #[error(
        "cost state directory {path} is owned by uid {owner}, expected effective uid {effective}; run chown {effective} {path} or run as the owner"
    )]
    Owner { path: PathBuf, owner: u32, effective: u32 },
}

pub(super) fn validate(path: &Path, stat: &rustix::fs::Stat) -> Result<(), StateDirectoryError> {
    validate_for_uid(path, stat, rustix::process::geteuid().as_raw())
}

fn validate_for_uid(path: &Path, stat: &rustix::fs::Stat, effective: u32) -> Result<(), StateDirectoryError> {
    if stat.st_uid != effective {
        return Err(StateDirectoryError::Owner {
            path: path.to_path_buf(),
            owner: stat.st_uid,
            effective,
        });
    }
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory || stat.st_mode & 0o022 != 0
    {
        return Err(StateDirectoryError::Mode {
            path: path.to_path_buf(),
            mode: stat.st_mode & 0o7777,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_state_dir_foreign_owner_is_refused_including_root() {
        let temp = tempfile::tempdir().expect("test: workspace");
        let file = std::fs::File::open(temp.path()).expect("test: open directory");
        let mut stat = rustix::fs::fstat(&file).expect("test: real directory stat");
        stat.st_uid = 1;
        for effective in [0, 2] {
            let error =
                validate_for_uid(temp.path(), &stat, effective).expect_err("test: foreign owner must be refused");
            assert!(
                matches!(error, StateDirectoryError::Owner { owner: 1, .. }),
                "owner predicate must apply even to root"
            );
            let message = error.to_string();
            assert!(
                message.contains(&format!("chown {effective}")) && message.contains(&temp.path().display().to_string()),
                "owner refusal must explain remediation and path: {message}"
            );
        }
        stat.st_uid = 2;
        stat.st_mode = 0o42700;
        assert!(
            validate_for_uid(temp.path(), &stat, 2).is_ok(),
            "inherited setgid must be accepted"
        );
    }
}

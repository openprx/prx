//! Real-CLI coverage for `prx init --force` reporting.
//!
//! `init` mutates the configuration generation before it scaffolds the
//! workspace. A failure in those later steps must not swallow the report of the
//! mutation that already committed: the operator has to be told that their
//! settings were reset and where the archive of the previous configuration is,
//! even when the same run then fails.
//!
//! These cases drive the built binary rather than the library, so the
//! assertions are about what an operator actually sees on stdout/stderr and
//! about the exit status a script observes.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;

/// Distinctive value written into the fragment the run is expected to archive.
const SYNTHETIC_TOKEN: &str = "synthetic-operator-token-r09";

/// Prefix every archive directory carries.
const ARCHIVE_PREFIX: &str = "config.d.pre-";

/// A channels fragment this build cannot load: `allowed_users` is required, so
/// the preserved settings fail staged validation and `--force` falls back to
/// the shipped templates.
fn synthetic_channels_fragment() -> String {
    format!("[channels_config]\ncli = true\n\n[channels_config.telegram]\nbot_token = \"{SYNTHETIC_TOKEN}\"\n")
}

/// Run the real `prx init` against an isolated home and config directory.
fn run_init(home: &Path, config_dir: &Path, extra: &[&str]) -> io::Result<Output> {
    std::process::Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(config_dir)
        .arg("init")
        .arg("--spec")
        .arg("full")
        .args(extra)
        .env("HOME", home)
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .output()
}

/// Archive directories currently next to the configuration, sorted by name.
fn archive_dirs(config_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(config_dir)? {
        let path = entry?.path();
        let is_archive = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(ARCHIVE_PREFIX));
        if is_archive && path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Both streams as one string, for assertions that do not care which one an
/// operator read the line on.
fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Replace `workspace/sessions` with a regular file so the scaffolding that
/// runs after the configuration mutation fails.
fn obstruct_sessions(config_dir: &Path) -> io::Result<()> {
    let sessions = config_dir.join("workspace").join("sessions");
    if sessions.is_dir() {
        std::fs::remove_dir_all(&sessions)?;
    }
    std::fs::write(&sessions, "not a directory")
}

/// The baseline from the audit: an operator fragment that cannot validate forces
/// the reset, an obstructed `workspace/sessions` fails the run after the reset
/// has already committed, and the operator must still be told both things.
#[test]
fn a_reset_that_commits_is_reported_even_when_the_run_then_fails() {
    let temp = tempfile::tempdir().expect("test: create tempdir");
    let home = temp.path().join("home");
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&home).expect("test: create the isolated home");

    let first = run_init(&home, &config_dir, &[]).expect("test: run the first init");
    assert!(
        first.status.success(),
        "test fixture: the first init must succeed: {}",
        combined(&first)
    );

    let channels = config_dir.join("config.d").join("channels.toml");
    std::fs::write(&channels, synthetic_channels_fragment()).expect("test: write the synthetic fragment");
    obstruct_sessions(&config_dir).expect("test: obstruct workspace/sessions");

    let forced = run_init(&home, &config_dir, &["--force"]).expect("test: run the forced init");
    let stdout = String::from_utf8_lossy(&forced.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&forced.stderr).into_owned();

    assert!(
        !forced.status.success(),
        "a reset plus a failed scaffold must exit non-zero: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("RESET TO THE SHIPPED TEMPLATES"),
        "the reset warning must survive the later failure: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("workspace/sessions"),
        "the failure that stopped the run must still be reported: stderr={stderr}"
    );
    assert!(
        stdout.contains(&config_dir.display().to_string()),
        "the committed configuration directory must be named on stdout: stdout={stdout}"
    );

    let archives = archive_dirs(&config_dir).expect("test: list the archives");
    assert_eq!(
        archives.len(),
        1,
        "exactly one archive must exist after one forced run: {archives:?}"
    );
    let archive = archives.first().expect("test: the archive must exist");
    assert!(
        stderr.contains(&archive.display().to_string()),
        "the exact archive path must be printed: archive={} stderr={stderr}",
        archive.display()
    );
    assert!(
        std::fs::read_to_string(archive.join("channels.toml"))
            .expect("test: read the archived fragment")
            .contains(SYNTHETIC_TOKEN),
        "the archive must hold the operator's original fragment"
    );
    assert!(
        !std::fs::read_to_string(&channels)
            .expect("test: read the regenerated fragment")
            .contains(SYNTHETIC_TOKEN),
        "the reset the report describes must actually have committed"
    );
}

/// The archive named by the report is the one that run wrote: a second forced
/// run gets its own directory and cannot overwrite the first one's contents.
#[test]
fn a_second_forced_run_archives_into_its_own_directory() {
    let temp = tempfile::tempdir().expect("test: create tempdir");
    let home = temp.path().join("home");
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&home).expect("test: create the isolated home");

    let first_init = run_init(&home, &config_dir, &[]).expect("test: run the first init");
    assert!(
        first_init.status.success(),
        "test fixture: the first init must succeed: {}",
        combined(&first_init)
    );
    let channels = config_dir.join("config.d").join("channels.toml");
    std::fs::write(&channels, synthetic_channels_fragment()).expect("test: write the synthetic fragment");
    obstruct_sessions(&config_dir).expect("test: obstruct workspace/sessions");

    let first = run_init(&home, &config_dir, &["--force"]).expect("test: run the first forced init");
    assert!(
        !first.status.success(),
        "test fixture: the forced run must fail: {}",
        combined(&first)
    );
    let after_first = archive_dirs(&config_dir).expect("test: list the archives");
    assert_eq!(
        after_first.len(),
        1,
        "one archive after one forced run: {after_first:?}"
    );

    let second = run_init(&home, &config_dir, &["--force"]).expect("test: run the second forced init");
    let second_output = combined(&second);
    assert!(
        !second.status.success(),
        "the obstruction is still there, so the second run must exit non-zero too: {second_output}"
    );

    let after_second = archive_dirs(&config_dir).expect("test: list the archives");
    assert_eq!(
        after_second.len(),
        2,
        "the second run must archive into its own directory: {after_second:?}"
    );
    let original = after_first.first().expect("test: the first archive must exist");
    assert!(
        std::fs::read_to_string(original.join("channels.toml"))
            .expect("test: read the first archived fragment")
            .contains(SYNTHETIC_TOKEN),
        "the first archive must keep the operator's original fragment"
    );
    let named = after_second
        .iter()
        .filter(|path| second_output.contains(&path.display().to_string()))
        .count();
    assert_eq!(
        named, 1,
        "the second run must name exactly its own archive: {after_second:?} output={second_output}"
    );
}

//! Exposes the git commit the binary is built from as the compile-time
//! variable `PRX_BUILD_COMMIT` (the `commit` of `GET /api/capabilities`).
//!
//! Order: a builder-supplied `PRX_BUILD_COMMIT` (for a tree copied without
//! `.git`, such as an rsync build mirror), then `git rev-parse HEAD`, then the
//! literal `unknown`. Only a full 40-hex SHA is accepted. Local only: no
//! network, no panic.

use std::path::Path;
use std::process::{Command, Stdio};

const UNKNOWN: &str = "unknown";

fn main() {
    println!("cargo:rerun-if-env-changed=PRX_BUILD_COMMIT");
    // Rebuild when HEAD moves: HEAD itself, the branch it points at, and
    // packed refs. Only existing files are named; a missing one would make
    // Cargo rerun this script on every build.
    let mut watched = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(reference);
    }
    for name in &watched {
        if let Some(path) = git(&["rev-parse", "--git-path", name]).filter(|path| Path::new(path).exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = std::env::var("PRX_BUILD_COMMIT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| is_full_sha(value))
        .or_else(|| git(&["rev-parse", "HEAD"]).filter(|value| is_full_sha(value)))
        .unwrap_or_else(|| UNKNOWN.to_owned());
    println!("cargo:rustc-env=PRX_BUILD_COMMIT={commit}");
}

/// The trimmed stdout of a successful `git` command, if any.
fn git(args: &[&str]) -> Option<String> {
    // git's diagnostics go to this script's stderr, which Cargo keeps.
    let output = Command::new("git").args(args).stderr(Stdio::inherit()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#![cfg(test)]

//! SL09: the raw store mutations are not a second way to change a skill. The
//! policy owner is `SkillsLifecycleService`; code outside `src/skills` may not
//! reference the raw transitions, apart from the named test fixtures that
//! publish an operator revision.

use std::fs;
use std::path::{Path, PathBuf};

/// Call patterns nobody outside `src/skills` may use at all.
const FORBIDDEN_OUTSIDE: [&str; 5] = [
    ".authorize_publication(",
    ".publish_authorized(",
    ".gate_proposal(",
    ".cancel_proposal(",
    ".install_candidate(",
];

/// Patterns only the named test fixtures may use, and only inside their
/// `#[cfg(test)] mod tests`.
const FIXTURE_ONLY: [&str; 2] = [".write_revision(", "lifecycle::PublishRequest"];
const FIXTURE_FILES: [&str; 2] = ["src/tools/skills.rs", "src/agent/terminal.rs"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn raw_store_mutations_are_referenced_only_from_src_skills_and_named_test_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(files.len() > 100, "the source tree was not found");
    let mut violations = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("src/skills/") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for pattern in FORBIDDEN_OUTSIDE {
            if text.contains(pattern) {
                violations.push(format!("{relative} uses {pattern}"));
            }
        }
        // A rollback request of the skill lifecycle (the config module has an
        // unrelated type of the same name).
        if text.contains("RollbackRequest") && text.contains("skills::lifecycle") {
            violations.push(format!("{relative} builds a lifecycle RollbackRequest"));
        }
        let fixture_use = FIXTURE_ONLY
            .iter()
            .filter_map(|pattern| text.find(pattern))
            .chain(
                text.contains("skills::lifecycle")
                    .then(|| text.find("PublishRequest"))
                    .flatten(),
            )
            .min();
        if let Some(first) = fixture_use {
            let allowed = FIXTURE_FILES.contains(&relative.as_str())
                && text
                    .get(..first)
                    .is_some_and(|before| before.contains("#[cfg(test)]\nmod tests"));
            if !allowed {
                violations.push(format!(
                    "{relative} uses a raw store write outside a named test fixture"
                ));
            }
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

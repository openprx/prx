//! Removal stays complete while exact compatibility surfaces remain reviewable.
use super::helpers::{read_source, relative_path, repository_root, rust_source_and_test_files, rust_source_files};
use super::scanner_probes::scanner_fails_with;
use std::path::{Path, PathBuf};

fn keyword() -> String {
    ["auto", "mix"].concat()
}

// Exact paths and their sole accepted purposes. No new-turn implementation is
// permitted in the config adapter, legacy-input test, or historical reader.
const COMPATIBILITY: &[(&str, &str)] = &[
    ("src/config/deprecations.rs", "retired operator section detection"),
    (
        "src/config/schema/tests/retired_router.rs",
        "legacy loader warning regression",
    ),
    (
        "src/agent/agent/retired_escalation_tests.rs",
        "embedded legacy TOML fixture",
    ),
    ("src/agent/terminal/served_auxiliary.rs", "read-only v1 event identity"),
    (
        "tests/fixtures/served-reply-v1.json",
        "immutable pre-removal committed records",
    ),
];

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read contract tree") {
        let path = entry.expect("contract entry").path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn reference_allowed(path: &str) -> bool {
    path == "CHANGELOG.md"
        || COMPATIBILITY
            .iter()
            .any(|(allowed, purpose)| *allowed == path && !purpose.is_empty())
}

#[test]
fn removed_reply_feature_has_only_explicit_compatibility_references() {
    let root = repository_root();
    let mut paths = rust_source_and_test_files();
    paths.extend(rust_source_files());
    for dir in ["docs", "benches", "tests/fixtures"] {
        collect(&root.join(dir), &mut paths);
    }
    paths.extend(["Cargo.toml", "README.md", "CHANGELOG.md"].map(|path| root.join(path)));
    for path in paths {
        let relative = relative_path(&path);
        if !reference_allowed(&relative) {
            assert!(
                !relative.to_lowercase().contains(&keyword()),
                "removed feature path: {relative}"
            );
            let bytes = std::fs::read(&path).unwrap_or_default();
            let text = if path.extension().is_some_and(|ext| ext == "rs") {
                Ok(read_source(&path))
            } else {
                String::from_utf8(bytes)
            };
            if let Ok(text) = text {
                if relative == "README.md" || relative.starts_with("docs/") {
                    assert!(
                        !text.contains("low-confidence upgrade") && !text.contains("Injected-router escalation"),
                        "removed feature claim: {relative}"
                    );
                }
                assert!(
                    !text.to_lowercase().contains(&keyword()),
                    "removed feature reference: {relative}"
                );
            }
        }
    }
    let schema = serde_json::to_string(&schemars::schema_for!(openprx::config::Config)).expect("generated schema");
    assert!(!schema.to_lowercase().contains(&keyword()));
}

fn docs_contract(readme: &str, changelog: &str) -> bool {
    let unreleased = changelog
        .split("## [Unreleased]")
        .nth(1)
        .unwrap_or("")
        .split("\n## [")
        .next()
        .unwrap_or("");
    !readme.to_lowercase().contains(&keyword())
        && !readme.contains("low-confidence upgrade")
        && !readme.contains("Injected-router escalation")
        && unreleased.contains("BREAKING (operators)")
        && unreleased
            .to_lowercase()
            .contains(&format!("{} reply escalation was removed", keyword()))
        && unreleased.contains("ignored with one warning")
}

#[test]
fn removal_docs_contract_and_mutations() {
    let root = repository_root();
    let readme = read_source(&root.join("README.md"));
    let changelog = read_source(&root.join("CHANGELOG.md"));
    assert!(docs_contract(&readme, &changelog));
    assert!(!docs_contract(
        &readme,
        &changelog.replace("BREAKING (operators)", "Changed")
    ));
    assert!(!docs_contract(
        &format!("{readme}\n{} escalation is available", keyword()),
        &changelog
    ));
    assert!(!docs_contract(
        &format!("{readme}\nlow-confidence upgrade activates"),
        &changelog
    ));
    assert!(!reference_allowed("src/router/retired_reply.rs"));
    let planted = format!("fn {}_escalate() {{}}", keyword());
    assert!(planted.contains(&keyword()) && !reference_allowed("src/router/retired_reply.rs"));
}

#[test]
fn source_absence_guard_rejects_restored_executable_reference() {
    let restored = format!("pub fn {}_escalate() {{}}", keyword());
    assert!(scanner_fails_with(
        removed_reply_feature_has_only_explicit_compatibility_references,
        vec![("src/router/retired_reply.rs".into(), restored)],
    ));
}

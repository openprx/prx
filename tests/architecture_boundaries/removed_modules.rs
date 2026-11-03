//! Guard the removal of an unused integration pipeline and its metadata.
use super::helpers::{read_source, repository_root};
use std::path::Path;
fn needle() -> String {
    ["skill", "forge"].concat()
}
fn collect(dir: &Path, paths: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            collect(&path, paths);
        } else if entry.file_type().unwrap().is_file() {
            paths.push(path);
        }
    }
}
#[test]
fn removed_modules_directory_and_exports_are_absent() {
    let root = repository_root();
    let name = needle();
    assert!(
        !root.join("src").join(&name).exists(),
        "removed module directory exists"
    );
    let lib = read_source(&root.join("src/lib.rs"));
    assert!(
        !lib.contains(&format!("pub mod {name};")),
        "removed public export exists"
    );
    assert!(
        !lib.contains(&format!("\"mod {name};\"")),
        "removed binary graph entry exists"
    );
}
#[test]
fn removed_modules_text_references_are_allowlisted() {
    let root = repository_root();
    let name = needle();
    let mut files = Vec::new();
    for directory in ["src", "tests", "docs", ".github", "console/src"] {
        collect(&root.join(directory), &mut files);
    }
    files.extend(["README.md", "Cargo.toml", "CHANGELOG.md"].map(|path| root.join(path)));
    let mut discovery_lines = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if !line.to_ascii_lowercase().contains(&name) {
                continue;
            }
            let relative = file.strip_prefix(&root).unwrap();
            if relative == Path::new("CHANGELOG.md") {
                continue;
            }
            if relative == Path::new("src/skills/discovery.rs")
                && line.contains(&["OpenPRX-Skill", "Forge/0.1"].concat())
            {
                discovery_lines.push(index + 1);
                continue;
            }
            panic!("removed module reference at {}:{}", relative.display(), index + 1);
        }
    }
    assert_eq!(
        discovery_lines.len(),
        1,
        "exactly one legacy HTTP identity line is allowed"
    );
}
#[test]
fn removed_modules_security_document_has_no_reference() {
    assert_no_reference("docs/skills-catalog-security.md");
}
#[test]
fn removed_modules_labeler_has_no_reference() {
    assert_no_reference(".github/labeler.yml");
}
#[test]
fn removed_modules_pull_request_template_has_no_reference() {
    assert_no_reference(".github/pull_request_template.md");
}
fn assert_no_reference(path: &str) {
    assert!(
        !read_source(&repository_root().join(path))
            .to_ascii_lowercase()
            .contains(&needle()),
        "removed module in {path}"
    );
}
#[test]
fn removed_modules_changelog_records_removal() {
    let text = read_source(&repository_root().join("CHANGELOG.md"));
    let unreleased = text
        .split("## [Unreleased]")
        .nth(1)
        .unwrap()
        .split("\n## [")
        .next()
        .unwrap();
    let removed = unreleased
        .split("### Removed\n")
        .nth(1)
        .unwrap()
        .split("\n### ")
        .next()
        .unwrap();
    assert!(
        removed.contains(
            &[
                "Skill",
                "Forge (unused auto-integration pipeline); skill discovery moved to skills::discovery"
            ]
            .concat()
        )
    );
}

use super::{entry, md};
use crate::skills::lifecycle::{Bundle, ManifestError, validate_manifest};

fn manifest_of(
    entries: Vec<crate::skills::lifecycle::BundleEntry>,
) -> Result<crate::skills::lifecycle::ManifestInfo, ManifestError> {
    validate_manifest(&Bundle::from_entries(entries).expect("test: valid bundle"))
}

fn toml_for(name: &str, description: &str) -> String {
    format!("[skill]\nname = \"{name}\"\ndescription = \"{description}\"\nversion = \"1.2.3\"\n")
}

#[test]
fn frontmatter_only_manifest_is_accepted() {
    let info = manifest_of(vec![entry("SKILL.md", md("Deploy", "Does deploys").as_bytes())]).expect("test: valid");
    assert_eq!(info.name, "Deploy");
    assert_eq!(info.description, "Does deploys");
    assert_eq!(info.semantic_version, None);
}

#[test]
fn matching_toml_is_accepted_and_supplies_the_version() {
    let info = manifest_of(vec![
        entry("SKILL.md", md("Deploy", "Does deploys").as_bytes()),
        entry("SKILL.toml", toml_for("Deploy", "Does deploys").as_bytes()),
    ])
    .expect("test: valid");
    assert_eq!(info.semantic_version.as_deref(), Some("1.2.3"));
}

#[test]
fn conflicting_markdown_and_toml_are_rejected() {
    let err = manifest_of(vec![
        entry("SKILL.md", md("Deploy", "Does deploys").as_bytes()),
        entry("SKILL.toml", toml_for("Other", "Does deploys").as_bytes()),
    ])
    .expect_err("test: name conflict");
    assert!(matches!(err, ManifestError::Conflict { field: "name", .. }), "{err}");

    let err = manifest_of(vec![
        entry("SKILL.md", md("Deploy", "Does deploys").as_bytes()),
        entry("SKILL.toml", toml_for("Deploy", "Different text").as_bytes()),
    ])
    .expect_err("test: description conflict");
    assert!(
        matches!(
            err,
            ManifestError::Conflict {
                field: "description",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn missing_or_malformed_manifests_are_rejected() {
    let err = manifest_of(vec![entry("README.md", b"x")]).expect_err("test: no SKILL.md");
    assert!(matches!(err, ManifestError::MissingSkillMd), "{err}");

    let err = manifest_of(vec![entry("SKILL.md", b"# No frontmatter\n")]).expect_err("test: no frontmatter");
    assert!(matches!(err, ManifestError::MissingFrontmatter), "{err}");

    let err =
        manifest_of(vec![entry("SKILL.md", b"---\nname: only-name\n---\nbody")]).expect_err("test: no description");
    assert!(matches!(err, ManifestError::MissingFrontmatter), "{err}");

    let err = manifest_of(vec![entry("SKILL.md", md("", "d").as_bytes())]).expect_err("test: empty name");
    assert!(
        matches!(err, ManifestError::InvalidField { field: "name", .. }),
        "{err}"
    );

    let err = manifest_of(vec![entry("SKILL.md", &[0xff, 0xfe, b'-'])]).expect_err("test: invalid utf8");
    assert!(matches!(err, ManifestError::NotUtf8 { .. }), "{err}");

    let err = manifest_of(vec![
        entry("SKILL.md", md("Deploy", "d").as_bytes()),
        entry("SKILL.toml", b"[skill\nname ="),
    ])
    .expect_err("test: bad toml");
    assert!(matches!(err, ManifestError::InvalidToml(_)), "{err}");
}

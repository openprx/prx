#![cfg(test)]

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

#[test]
fn duplicate_and_overriding_frontmatter_keys_are_rejected() {
    let dup = b"---\nname: A\nname: B\ndescription: d\n---\nbody";
    let err = manifest_of(vec![entry("SKILL.md", dup)]).expect_err("test: duplicate name");
    assert!(matches!(err, ManifestError::DuplicateKey { key: "name" }), "{err}");
    let dup = b"---\nname: A\ndescription: d\ndescription: e\n---\nbody";
    let err = manifest_of(vec![entry("SKILL.md", dup)]).expect_err("test: duplicate description");
    assert!(
        matches!(err, ManifestError::DuplicateKey { key: "description" }),
        "{err}"
    );
    let nested = b"---\nname: A\ndescription: d\nmetadata:\n  name: Evil\n---\nbody";
    let err = manifest_of(vec![entry("SKILL.md", nested)]).expect_err("test: nested override");
    assert!(matches!(err, ManifestError::AmbiguousFrontmatter), "{err}");
}

fn md_with_tools(line: &str) -> String {
    format!("---\nname: Deploy\ndescription: Does deploys\n{line}\n---\n\nBody.\n")
}

#[test]
fn required_tools_are_read_from_the_frontmatter_list() {
    let info = manifest_of(vec![entry(
        "SKILL.md",
        md_with_tools("required-tools: [lint, \"deploy_check\", lint]").as_bytes(),
    )])
    .expect("test: valid");
    assert_eq!(
        info.required_tools,
        vec!["deploy_check".to_string(), "lint".to_string()]
    );
    let none = manifest_of(vec![entry("SKILL.md", md("Deploy", "Does deploys").as_bytes())]).expect("test: valid");
    assert!(none.required_tools.is_empty(), "nothing declared is nothing required");
    let empty =
        manifest_of(vec![entry("SKILL.md", md_with_tools("required-tools: []").as_bytes())]).expect("test: valid");
    assert!(empty.required_tools.is_empty(), "an empty list declares nothing");
    // An indented key belongs to another mapping and is not read.
    let nested = manifest_of(vec![entry(
        "SKILL.md",
        md_with_tools("metadata:\n  required-tools: [x]").as_bytes(),
    )])
    .expect("test: valid");
    assert!(nested.required_tools.is_empty(), "an indented key is not ours");
}

#[test]
fn malformed_or_repeated_required_tools_are_rejected() {
    for line in [
        "required-tools: lint",
        "required-tools: [a b]",
        "required-tools: [a;b]",
        "required-tools: [a, b",
        "required-tools: [a]\nrequired-tools: [b]",
    ] {
        assert!(
            manifest_of(vec![entry("SKILL.md", md_with_tools(line).as_bytes())]).is_err(),
            "{line}"
        );
    }
}

#[test]
fn skill_md_and_skill_toml_must_agree_on_required_tools() {
    let toml = |tools: &str| format!("{}{tools}", toml_for("Deploy", "Does deploys"));
    let both = |md_line: &str, toml_tools: &str| {
        manifest_of(vec![
            entry("SKILL.md", md_with_tools(md_line).as_bytes()),
            entry("SKILL.toml", toml(toml_tools).as_bytes()),
        ])
    };
    // Equal lists, whatever their order, agree; one side alone is enough.
    let agreed = both("required-tools: [b, a]", "required-tools = [\"a\", \"b\"]\n").expect("test: agree");
    assert_eq!(agreed.required_tools, vec!["a".to_string(), "b".to_string()]);
    let toml_only = both("", "required-tools = [\"a\"]\n").expect("test: toml only");
    assert_eq!(toml_only.required_tools, vec!["a".to_string()]);
    let markdown_only = both("required-tools: [a]", "").expect("test: markdown only");
    assert_eq!(markdown_only.required_tools, vec!["a".to_string()]);
    // A disagreement is refused; neither parser wins.
    let err = both("required-tools: [a]", "required-tools = [\"b\"]\n").expect_err("test: conflict");
    assert!(
        matches!(
            err,
            ManifestError::Conflict {
                field: "required-tools",
                ..
            }
        ),
        "{err}"
    );
    assert!(both("required-tools: [a]", "required-tools = \"a\"\n").is_err());
    assert!(both("required-tools: [a]", "required-tools = [1]\n").is_err());
}

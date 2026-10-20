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
        md_with_tools(
            "required-tools: [native_tool:native:lint, \"native_tool:native:deploy_check\", native_tool:native:lint]",
        )
        .as_bytes(),
    )])
    .expect("test: valid");
    assert_eq!(
        info.required_tools,
        vec![
            "native_tool:native:deploy_check".to_string(),
            "native_tool:native:lint".to_string()
        ]
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

const A: &str = "native_tool:native:a";
const B: &str = "mcp:notes:search";

/// Every form the key does not allow: the real ids of the lifecycle have the
/// shape `family:namespace:name` with a known family.
const BAD_LINES: [&str; 12] = [
    "required-tools: lint",
    "required-tools: [lint]",
    "required-tools: [deploy_check]",
    "required-tools: [shell:native]",
    "required-tools: [shell:native:]",
    "required-tools: [shell::shell]",
    "required-tools: [bogus:native:x]",
    "required-tools: [none:native:x]",
    "required-tools: [shell:native:shell:more]",
    "required-tools: [a b]",
    "required-tools: [shell:native:shell, ",
    "required-tools: {a: 1}",
];

#[test]
fn a_legacy_or_malformed_required_tools_line_reads_as_undeclared() {
    // A stored revision may already carry such a line in another form: it must
    // stay readable (and rewritable) and simply declares nothing.
    for line in BAD_LINES
        .iter()
        .copied()
        .chain(["required-tools: [native_tool:native:a]\nrequired-tools: [mcp:notes:search]"])
    {
        let info = manifest_of(vec![entry("SKILL.md", md_with_tools(line).as_bytes())])
            .unwrap_or_else(|e| panic!("test: {line} must stay readable: {e}"));
        assert!(info.required_tools.is_empty(), "{line}");
    }
}

#[test]
fn a_new_revision_with_a_malformed_required_tools_line_is_refused_with_a_typed_error() {
    use crate::skills::lifecycle::validate_manifest_strict;
    let strict = |line: &str| {
        validate_manifest_strict(
            &Bundle::from_entries(vec![entry("SKILL.md", md_with_tools(line).as_bytes())]).expect("test: bundle"),
        )
    };
    for line in BAD_LINES
        .iter()
        .copied()
        .chain(["required-tools: [native_tool:native:a]\nrequired-tools: [mcp:notes:search]"])
    {
        let err = strict(line).expect_err(line);
        assert!(
            matches!(
                err,
                ManifestError::InvalidField {
                    field: "required-tools",
                    ..
                } | ManifestError::DuplicateKey { key: "required-tools" }
            ),
            "{line}: {err}"
        );
    }
    let ok = strict("required-tools: [shell:native:shell, filesystem:native:file_read, mcp:notes:search, skill:native:skill__a__b, native_tool:native:x]")
        .expect("test: every known shape");
    assert_eq!(ok.required_tools.len(), 5);
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
    let md_list = format!("required-tools: [{B}, {A}]");
    // Equal lists, whatever their order, agree; one side alone is enough.
    let agreed = both(&md_list, &format!("required-tools = [\"{A}\", \"{B}\"]\n")).expect("test: agree");
    assert_eq!(agreed.required_tools, vec![B.to_string(), A.to_string()], "sorted");
    let toml_only = both("", &format!("required-tools = [\"{A}\"]\n")).expect("test: toml only");
    assert_eq!(toml_only.required_tools, vec![A.to_string()]);
    let markdown_only = both(&format!("required-tools: [{A}]"), "").expect("test: markdown only");
    assert_eq!(markdown_only.required_tools, vec![A.to_string()]);
    // A disagreement is refused; neither parser wins.
    let err = both(
        &format!("required-tools: [{A}]"),
        &format!("required-tools = [\"{B}\"]\n"),
    )
    .expect_err("test: conflict");
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
    // A malformed TOML value is undeclared, not a conflict.
    let lenient = both(&format!("required-tools: [{A}]"), "required-tools = [1]\n").expect("test: lenient");
    assert_eq!(lenient.required_tools, vec![A.to_string()]);
}

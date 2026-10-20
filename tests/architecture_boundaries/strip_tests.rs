use super::helpers::{blank_comments_and_literals, is_test_only_file, production_part, strip_cfg_test_items};
use super::tool_scanner::scan_tool_executions;
/// The configuration-write guard's production part drops every test
/// *module*, wherever it is, and keeps what follows it.
#[test]
fn the_production_part_drops_every_test_module_and_keeps_what_follows() {
    let source = "fn a() {}\n#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\n#[allow(clippy::x)]\nmod tests {}\n";
    let production = production_part(source);
    assert!(production.contains("fn helper()"));
    assert!(!production.contains("mod tests"));

    // A brace-less test module first (providers/mod.rs:37 shape) and an inline
    // one in the middle of the file (chat/mod.rs:1577 shape): the production
    // code after either is still part of the file.
    let source = "#[cfg(test)]\npub(crate) mod credential_probe;\npub fn first(cfg: Config) {\n    cfg.save();\n}\n\
                  #[cfg(test)]\nmod tests {\n    fn t() { cfg.save(); }\n}\npub fn second(cfg: Config) {\n    \
                  cfg.apply_config();\n}\n";
    let production = production_part(source);
    assert!(production.contains("cfg.save();\n}"), "{production}");
    assert!(production.contains("cfg.apply_config()"), "{production}");
    assert!(!production.contains("credential_probe"), "{production}");
    assert!(!production.contains("fn t()"), "{production}");
}

/// One case of `a_cfg_test_construct_ends_where_it_ends`: the source, text
/// that must survive stripping, and text that must not.
type StripCase = (&'static str, &'static [&'static str], &'static [&'static str]);

/// Every shape a `#[cfg(test)]` attribute takes in this crate, each followed
/// by production text that must stay visible: the construct ends at its own
/// `;` or `,`, at the close of its block, or just before the closer of the
/// enclosing construct, which always survives.
const STRIP_CASES: &[StripCase] = &[
    // A block-bodied test arm, then an arm opening with a range pattern: `..`
    // begins the next arm, it does not continue the block expression.
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => {}\n        ..=5 => \
         call(MiddlewareStage::Outbound),\n        _ => 0,\n    }\n}\n",
        &["..=5 => call(MiddlewareStage::Outbound),", "_ => 0,"],
        &["0 => {}"],
    ),
    // An inline `const` block is an expression statement, not an item: it has
    // no `;` and ends with its block.
    (
        "fn f() {\n    #[cfg(test)]\n    const { assert!(true) }\n    run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["assert!(true)"],
    ),
    // A method call or `.await` after a test block still continues it.
    (
        "async fn f() {\n    #[cfg(test)]\n    async { probe() }.await;\n    run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["probe()"],
    ),
    // Raw byte and raw C strings ending in a backslash, and an escaped quote
    // char directly followed by another char, inside test code: the literal
    // ends where rustc ends it, so nothing after it is taken for a literal.
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = br\"C:\\\";\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = cr\"C:\\\";\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = ['\\'','('];\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nconst LIMIT: usize = 3;\n#[cfg(test)]\n#[path = \"probe_tests.rs\"]\nmod probe_tests;\n\
         #[cfg(test)]\nuse crate::probe::{A, B};\n#[cfg(test)]\nconst TABLE: [u8; 2] = [\n    1, 2,\n];\npub fn \
         production() {\n    let _ = MiddlewareStage::Outbound;\n}\n#[cfg(test)]\nfn helper() -> Result<(), \
         Error> {\n    Ok(())\n}\npub fn after_helper() {}\n#[cfg(test)]\nmod tests {\n    fn inside() {}\n}\n",
        &[
            "pub fn production()",
            "MiddlewareStage::Outbound",
            "pub fn after_helper()",
        ],
        &[
            "LIMIT",
            "probe_tests",
            "crate::probe",
            "TABLE",
            "1, 2,",
            "fn helper",
            "Ok(())",
            "mod tests",
            "inside",
        ],
    ),
    (
        "pub struct Runtime {\n    #[cfg(test)]\n    probe: u8,\n    pub stage: Stage,\n}\n",
        &["pub stage: Stage,", "pub struct Runtime {"],
        &["probe"],
    ),
    (
        "pub struct Runtime {\n    pub stage: Stage,\n    #[cfg(test)]\n    probe: u8\n}\npub fn later() -> \
         Stage {\n    MiddlewareStage::Outbound\n}\n",
        &["pub stage: Stage,", "pub fn later()", "MiddlewareStage::Outbound"],
        &["probe"],
    ),
    (
        "fn build(\n    #[cfg(test)] hook: Hook,\n    real: u8,\n) -> anyhow::Result<u8> {\n    \
         run(MiddlewareStage::Outbound)\n}\n",
        &["real: u8,", ") -> anyhow::Result<u8> {", "MiddlewareStage::Outbound"],
        &["hook"],
    ),
    (
        "fn f() {\n    call(\n        #[cfg(test)]\n        probe(),\n        run(MiddlewareStage::Outbound),\n    );\n}\n",
        &["run(MiddlewareStage::Outbound),", "    );"],
        &["probe()"],
    ),
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => 1,\n        _ => \
         call(MiddlewareStage::Outbound),\n    }\n}\n",
        &["_ => call(MiddlewareStage::Outbound),"],
        &["0 => 1"],
    ),
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => {\n            probe()\n        \
         }\n        _ => call(MiddlewareStage::Outbound),\n    }\n}\n",
        &["_ => call(MiddlewareStage::Outbound),"],
        &["probe()"],
    ),
    (
        "fn f() -> Runtime {\n    Runtime {\n        #[cfg(test)]\n        probe: 1,\n        stage: \
         run(MiddlewareStage::Outbound),\n    }\n}\n",
        &["stage: run(MiddlewareStage::Outbound),"],
        &["probe: 1"],
    ),
    (
        "fn generation() -> Arc<Generation> {\n    #[cfg(test)]\n    Arc::new(Generation { probe: 1 })\n}\nfn \
         later() {\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &[
            "fn generation() -> Arc<Generation> {",
            "fn later()",
            "MiddlewareStage::Outbound",
        ],
        &["Arc::new", "probe"],
    ),
    (
        "fn f() {\n    #[cfg(test)]\n    if armed { probe() } else { other() }\n    \
         run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["armed", "probe()", "other()"],
    ),
    (
        "fn f() {\n    #[cfg(test)]\n    hook();\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &["let _ = MiddlewareStage::Outbound;"],
        &["hook()"],
    ),
    // Brackets inside literals and comments of a test item move nothing.
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = \"(\";\n    let _ = '[';\n    // (\n}\nconst OWNER: &str = \
         \"[context_handoff]\";\nconst EVENT: &str = \"llm.request.context.compiled\";\n",
        &["[context_handoff]", "llm.request.context.compiled"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nmod tests {\n    const OPEN: &str = \"{ ( [\";\n    /* } ) ] */\n}\npub fn after() {\n    \
         let _ = MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["OPEN"],
    ),
    // An attribute spelled inside a literal or comment is not one.
    (
        "const DOC: &str = \"#[cfg(test)]\";\n// #[cfg(test)]\npub fn f() {\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &["pub fn f()", "MiddlewareStage::Outbound"],
        &[],
    ),
];

/// Braces of the production text a stripping kept, after blanking literals:
/// never below zero and zero at the end.
fn braces_balance(text: &str) -> bool {
    let mut depth = 0i64;
    for character in blank_comments_and_literals(text).chars() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// A `#[cfg(test)]` construct ends where it ends (R-LOOPD0 R1-R3): extents are
/// computed on blanked text for raw and blanked callers alike, a brace-less or
/// comma-terminated construct does not swallow what follows, and the closer
/// of the enclosing construct stays.
#[test]
fn a_cfg_test_construct_ends_where_it_ends() {
    for (source, kept, stripped) in STRIP_CASES {
        for (mode, input) in [
            ("raw", (*source).to_string()),
            ("blanked", blank_comments_and_literals(source)),
        ] {
            let production = strip_cfg_test_items(&input);
            for text in *kept {
                // Blanked input has no literal contents left to keep.
                if input.contains(text) {
                    assert!(
                        production.contains(text),
                        "{mode}: `{text}` was stripped from:\n{source}\n---\n{production}"
                    );
                }
            }
            for text in *stripped {
                assert!(
                    !production.contains(text),
                    "{mode}: `{text}` was kept from:\n{source}\n---\n{production}"
                );
            }
            assert!(
                braces_balance(&production),
                "{mode}: stripping unbalanced the braces of:\n{source}\n---\n{production}"
            );
            assert_eq!(
                production.lines().count(),
                source.lines().count(),
                "{mode}: stripping must keep every line"
            );
        }
    }
}

/// A file whose first line of code is `#![cfg(test)]` is test code for every
/// scanner that separates production from tests, and nothing else is.
///
/// This is what lets a test module live in its own file (a `#[path]` child of
/// the item it tests) without a path exemption: the marker travels with the
/// file and the compiler enforces it. The negative half is the mutation: the
/// same production text without the marker, with the marker after the first
/// item, or with a composite predicate is still scanned.
#[test]
fn a_file_level_cfg_test_marker_makes_the_whole_file_test_code() {
    let body = "pub async fn handler() {\n    let _ = MiddlewareStage::Outbound;\n    cfg.save();\n    let _ = \
                tool.execute(args).await;\n}\n";
    for marked in [
        format!("//! Tests of a production module.\n\n#![cfg(test)]\n#![allow(clippy::unwrap_used)]\n\n{body}"),
        format!("/* leading block comment */\n#![cfg(test)] // trailing comment\n{body}"),
    ] {
        assert!(is_test_only_file(&marked), "{marked}");
        assert!(strip_cfg_test_items(&marked).is_empty(), "{marked}");
        assert!(
            strip_cfg_test_items(&blank_comments_and_literals(&marked)).is_empty(),
            "{marked}"
        );
        assert!(production_part(&marked).is_empty(), "{marked}");
        assert!(
            scan_tool_executions("src/agent/turn_tests/probe.rs", &marked)
                .sites
                .is_empty(),
            "{marked}"
        );
    }

    for unmarked in [
        body.to_string(),
        format!("use super::*;\n#![cfg(test)]\n{body}"),
        format!("#![allow(clippy::unwrap_used)]\n#![cfg(test)]\n{body}"),
        format!("#![cfg(any(test, feature = \"probe\"))]\n{body}"),
        format!("#![cfg(not(test))]\n{body}"),
        format!("#![cfg(all(test, feature = \"probe\"))]\n{body}"),
        format!("/* a comment that mentions\n#![cfg(test)]\n*/\n{body}"),
        format!("{body}#[cfg(test)]\nmod tests {{}}\n"),
    ] {
        assert!(!is_test_only_file(&unmarked), "{unmarked}");
        let stripped = strip_cfg_test_items(&blank_comments_and_literals(&unmarked));
        assert!(stripped.contains("MiddlewareStage::Outbound"), "{unmarked}");
        assert!(production_part(&unmarked).contains("cfg.save()"), "{unmarked}");
        assert_eq!(
            scan_tool_executions("src/agent/turn_tests/probe.rs", &unmarked)
                .sites
                .len(),
            1,
            "{unmarked}"
        );
    }
}

use super::helpers::architecture_guard_source;
use super::helpers::{blank_comments_and_literals, repository_root};
use super::scanner_probes::{
    MUTATION_PROBED_TREE_SCANNERS, PROBED_TREE_SCANNERS, TEST_INCLUSIVE_TREE_SCANNERS, scanner_fails_with,
};
use std::collections::BTreeSet;
use std::fs;
/// Calls that walk or read the source tree. A function whose body makes one
/// walks the tree, and so does every function that calls a walking function
/// (found to a fixpoint), whatever the helper is called.
const TREE_WALKS: &[&str] = &[
    "rust_source_files(",
    "rust_source_and_test_files(",
    "collect_rust_files(",
    "read_dir(",
    "read_source(",
];

/// Every function of `source` (comment- and literal-blanked text), at any
/// indentation: its name, whether `#[test]` or `#[tokio::test]` marks it
/// (also on the same line), and its body.
pub(super) fn source_functions(source: &str) -> Vec<(String, bool, String)> {
    let bytes = source.as_bytes();
    let mut functions = Vec::new();
    let mut search = 0usize;
    while let Some(found) = source[search..].find("fn ") {
        let at = search + found;
        search = at + 3;
        if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_') {
            continue;
        }
        let name: String = source[at + 3..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let rest = &source[at + 3 + name.len()..];
        let (Some(open), semicolon) = (rest.find('{'), rest.find(';')) else {
            continue;
        };
        if semicolon.is_some_and(|semicolon| semicolon < open) {
            continue;
        }
        let open = at + 3 + name.len() + open;
        let mut depth = 0usize;
        let mut close = source.len();
        for (offset, character) in source[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        // The attributes: the text before `fn` on its line, and the attribute
        // lines directly above it.
        let line_start = source[..at].rfind('\n').map_or(0, |offset| offset + 1);
        let mut head = source[line_start..at].to_string();
        let mut above = source[..line_start].lines().rev();
        while let Some(line) = above.next().filter(|line| line.trim_start().starts_with("#[")) {
            head.push_str(line);
        }
        let is_test = head.contains("#[test]") || head.contains("#[tokio::test");
        functions.push((name, is_test, source[open..close].to_string()));
    }
    functions
}

/// The `#[test]` functions of `source` (comment- and literal-blanked text)
/// that scan the source tree, directly or through any walking helper.
fn tree_scanners(source: &str) -> Vec<String> {
    let functions = source_functions(source);
    let calls = |body: &str, callee: &str| {
        let needle = format!("{callee}(");
        body.match_indices(&needle).any(|(offset, _)| {
            offset == 0 || {
                let before = body.as_bytes()[offset - 1];
                !(before.is_ascii_alphanumeric() || before == b'_')
            }
        })
    };
    let mut walkers: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = walkers.len();
        for (name, _is_test, body) in &functions {
            if walkers.contains(name) {
                continue;
            }
            let walks = TREE_WALKS.iter().any(|walk| calls(body, walk.trim_end_matches('(')))
                || walkers.iter().any(|walker| calls(body, walker));
            if walks {
                walkers.insert(name.clone());
            }
        }
        if walkers.len() == before {
            break;
        }
    }
    let mut scanners: Vec<String> = functions
        .into_iter()
        .filter(|(name, is_test, _)| *is_test && walkers.contains(name))
        .map(|(name, _, _)| name)
        .collect();
    scanners.sort();
    scanners.dedup();
    scanners
}

/// String methods that cut or search source text; applied to the
/// `#[cfg(test)]` spelling they are hand-written test stripping.
const STRIPPING_CALLS: &[&str] = &[
    "split",
    "split_once",
    "rsplit",
    "rsplit_once",
    "split_inclusive",
    "find",
    "rfind",
    "match_indices",
    "matches",
    "starts_with",
    "strip_prefix",
];

/// Hand-written test stripping in `text`: a string method applied to the
/// `#[cfg(test)]` spelling instead of `strip_cfg_test_items`.
fn hand_stripping_patterns(text: &str) -> Vec<String> {
    let marker = ["\"#[cfg", "(test)]"].concat();
    STRIPPING_CALLS
        .iter()
        .map(|call| format!("{call}({marker}"))
        .filter(|pattern| text.contains(pattern.as_str()))
        .collect()
}

/// Discovery finds a tree scanner in every shape one can be written: inside
/// a module, on one line, as `#[tokio::test]`, and through a walking helper
/// that is not one of the known ones.
#[test]
fn tree_scanner_discovery_sees_every_shape() {
    let source = "fn helper_walk() -> usize {\n    rust_source_files().len()\n}\n\nmod nested {\n    #[test]\n    fn \
                  indented_scanner() {\n        for path in rust_source_files() {}\n    }\n}\n\n#[test] fn \
                  one_line_scanner() { let _ = fs::read_dir(\"src\"); }\n\n#[test]\nfn via_a_new_helper() {\n    \
                  let _ = helper_walk();\n}\n\n#[tokio::test]\nasync fn async_scanner() {\n    let _ = \
                  read_source(path);\n}\n\n#[test]\nfn not_a_scanner() {\n    let _ = 1;\n}\n";
    let mut found = tree_scanners(&blank_comments_and_literals(source));
    found.sort();
    assert_eq!(
        found,
        vec![
            "async_scanner",
            "indented_scanner",
            "one_line_scanner",
            "via_a_new_helper"
        ]
    );
}

/// The ban on hand-written stripping catches every string method that cuts
/// or searches text at the `#[cfg(test)]` spelling.
#[test]
fn hand_stripping_ban_covers_every_split_form() {
    let marker = ["\"#[cfg", "(test)]\""].concat();
    for call in [
        "split",
        "split_once",
        "rsplit",
        "rsplit_once",
        "find",
        "rfind",
        "starts_with",
        "strip_prefix",
    ] {
        let text = format!("let parts = source.{call}({marker});");
        assert!(!hand_stripping_patterns(&text).is_empty(), "`{call}` is not caught");
    }
    assert!(
        hand_stripping_patterns("let production = strip_cfg_test_items(&source);").is_empty(),
        "expected an empty value"
    );
}

#[test]
fn every_tree_scanner_is_probed_or_declared_test_inclusive() {
    let raw = architecture_guard_source();
    let source = blank_comments_and_literals(&raw);
    let scanners = tree_scanners(&source);
    let mut unproven = scanners
        .iter()
        .filter(|name| {
            !PROBED_TREE_SCANNERS.contains(&name.as_str())
                && !MUTATION_PROBED_TREE_SCANNERS
                    .iter()
                    .any(|(listed, _, _)| listed == name)
                && !TEST_INCLUSIVE_TREE_SCANNERS.iter().any(|(listed, _)| listed == name)
        })
        .collect::<Vec<_>>();
    unproven.sort();
    assert!(
        unproven.is_empty(),
        "these tree scanners have no planted-violation probe and are not declared test-inclusive: {unproven:?}"
    );
    for listed in PROBED_TREE_SCANNERS
        .iter()
        .chain(MUTATION_PROBED_TREE_SCANNERS.iter().map(|(name, _, _)| name))
        .chain(TEST_INCLUSIVE_TREE_SCANNERS.iter().map(|(name, _)| name))
    {
        assert!(
            scanners.iter().any(|name| name == listed),
            "{listed} is listed but scans no tree"
        );
    }

    let unique_mutation_scanners: BTreeSet<_> =
        MUTATION_PROBED_TREE_SCANNERS.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(unique_mutation_scanners.len(), MUTATION_PROBED_TREE_SCANNERS.len());
    for (name, scanner, proof) in MUTATION_PROBED_TREE_SCANNERS {
        assert!(
            !PROBED_TREE_SCANNERS.contains(name),
            "duplicate standard probe registration: {name}"
        );
        assert!(
            !TEST_INCLUSIVE_TREE_SCANNERS.iter().any(|(listed, _)| listed == name),
            "mutation-probed scanner cannot use a test-inclusive exemption: {name}"
        );
        assert!(
            !scanner_fails_with(*scanner, Vec::new()),
            "registered scanner fails on the tree: {name}"
        );
        proof();
    }

    let bootstrap = fs::read_to_string(repository_root().join("src/runtime/bootstrap.rs")).expect("read bootstrap");
    for (file, text) in [
        ("tests/architecture_boundaries/", &raw),
        ("src/runtime/bootstrap.rs", &bootstrap),
    ] {
        let found = hand_stripping_patterns(text);
        assert!(
            found.is_empty(),
            "{file} strips test code by hand with {found:?}; use strip_cfg_test_items"
        );
    }
}

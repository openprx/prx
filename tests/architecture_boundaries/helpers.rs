use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
include!("../support/cfg_test_scan.rs");

pub(super) fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display())) {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

thread_local! {
    /// Source files a guard-of-guards probe plants for the current test thread
    /// only, as `(relative path, full text)`: a planted path is walked by
    /// `rust_source_files` and read by `read_source` instead of the disk file.
    /// Empty outside `every_tree_scanner_catches_a_planted_violation`.
    pub(super) static PLANTED_SOURCES: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn rust_source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(&repository_root().join("src"), &mut files);
    PLANTED_SOURCES.with_borrow(|planted| {
        for (relative, _) in planted {
            let path = repository_root().join(relative);
            if !files.contains(&path) {
                files.push(path);
            }
        }
    });
    files.sort();
    files
}

/// The text of a source file the scanners walk: the planted text for a probe
/// path on this thread, otherwise the file on disk.
pub(super) fn read_source(path: &Path) -> String {
    let relative = relative_path(path);
    let planted = PLANTED_SOURCES.with_borrow(|planted| {
        planted
            .iter()
            .find(|(planted_path, _)| *planted_path == relative)
            .map(|(_, text)| text.clone())
    });
    planted.unwrap_or_else(|| fs::read_to_string(path).unwrap_or_else(|error| panic!("read {relative}: {error}")))
}

/// Every Rust file the repository owns: the crate sources and the integration
/// tests.
pub(super) fn rust_source_and_test_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(&repository_root().join("src"), &mut files);
    collect_rust_files(&repository_root().join("tests"), &mut files);
    files.sort();
    files
}

pub(super) fn relative_path(path: &Path) -> String {
    path.strip_prefix(repository_root())
        .expect("source path must be inside repository")
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn normalize_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn enclosing_function(lines: &[&str], line_index: usize) -> String {
    for line in lines[..=line_index].iter().rev() {
        let Some(fn_offset) = line.find("fn ") else {
            continue;
        };
        let name = line[fn_offset + 3..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect::<String>();
        if !name.is_empty() {
            return name;
        }
    }
    "<module>".to_string()
}

pub(super) fn function_body(source: &str, name: &str) -> String {
    let needle = format!("fn {name}(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("missing production function {name}"));
    let body_start = source[start..].find('{').map_or_else(
        || panic!("missing body for production function {name}"),
        |offset| start + offset,
    );
    let mut depth = 0usize;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1).expect("function brace depth underflow");
                if depth == 0 {
                    return source[start..=body_start + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated production function {name}")
}

pub(super) fn assert_inventory(boundary: &str, actual: Vec<String>, expected: &[&str]) {
    let mut actual = actual;
    actual.sort();
    let mut expected = expected.iter().map(ToString::to_string).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        actual, expected,
        "{boundary} changed. New entries must be routed through the approved architecture; intentional exceptions require architecture review and an explicit allowlist update"
    );
}

pub(super) fn top_level_modules(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            if line != line.trim_start() {
                return None;
            }
            let line = line.trim();
            let module = line
                .strip_prefix("mod ")
                .or_else(|| line.strip_prefix("pub mod "))
                .or_else(|| line.strip_prefix("pub(crate) mod "))?;
            module
                .strip_suffix(';')
                .filter(|name| {
                    name.chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                })
                .map(ToString::to_string)
        })
        .collect()
}

/// The production part of a source file for the configuration-write guard:
/// every `#[cfg(test)]` *module* blanked wherever it is (an inline
/// `mod tests { .. }`, a brace-less `mod x;` declaration, one in the middle of
/// the file), and nothing for a test-only file. Other `#[cfg(test)]` items
/// still count, because the pinned write counts below were taken that way.
/// Positions and lines are kept, so nothing after a test module is lost.
pub(super) fn production_part(source: &str) -> String {
    if is_test_only_file(source) {
        return String::new();
    }
    let masked: Vec<char> = blank_comments_and_literals(source).chars().collect();
    let modules = cfg_test_ranges(source)
        .into_iter()
        .filter(|range| {
            // Past the attributes and the visibility, the construct is `mod`.
            let mut cursor = range.start;
            loop {
                while cursor < masked.len() && masked[cursor].is_whitespace() {
                    cursor += 1;
                }
                if cursor + 1 < masked.len() && masked[cursor] == '#' && masked[cursor + 1] == '[' {
                    cursor = close_of_group(&masked, cursor + 1);
                } else {
                    break;
                }
            }
            let (mut word, mut after) = masked_word_at(&masked, cursor);
            if word == "pub" {
                while after < masked.len() && masked[after].is_whitespace() {
                    after += 1;
                }
                if after < masked.len() && masked[after] == '(' {
                    after = close_of_group(&masked, after);
                }
                (word, _) = masked_word_at(&masked, after);
            }
            word == "mod"
        })
        .collect::<Vec<_>>();
    blank_ranges(source, &modules)
}

/// Read the integration target and every module declared by its thin root.
/// Keeping the declarations as the inventory makes a newly wired guard part of
/// the metaguard without maintaining a second list of architecture files.
pub(super) fn architecture_guard_source() -> String {
    let root = repository_root().join("tests/architecture_boundaries/main.rs");
    let raw = fs::read_to_string(root).expect("read tests/architecture_boundaries/main.rs");
    assert!(!raw.trim().is_empty(), "architecture test root must not be empty");
    let modules = top_level_modules(&raw);
    assert!(
        !modules.is_empty(),
        "architecture test root must declare its guard modules"
    );
    let mut source = raw;
    for module in modules {
        let relative = format!("tests/architecture_boundaries/{module}.rs");
        let child = fs::read_to_string(repository_root().join(&relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            !child.trim().is_empty(),
            "architecture guard module {relative} must not be empty"
        );
        source.push('\n');
        source.push_str(&child);
    }
    source
}

/// The memo in front of the blanker and the stripper returns exactly what the
/// computation returns, for every file of the tree, on a miss and on a hit.
#[test]
fn the_strip_memo_returns_the_exact_computation() {
    for path in rust_source_files() {
        let source = read_source(&path);
        let blanked = blank_comments_and_literals_uncached(&source);
        let stripped = if is_test_only_file(&source) {
            String::new()
        } else {
            blank_ranges(&source, &cfg_test_ranges(&source))
        };
        for _ in 0..2 {
            assert_eq!(
                blank_comments_and_literals(&source),
                blanked,
                "{}",
                relative_path(&path)
            );
            assert_eq!(strip_cfg_test_items(&source), stripped, "{}", relative_path(&path));
        }
    }
    let edited = "fn a() {}\n#[cfg(test)]\nfn b() {}\n";
    assert_ne!(
        strip_cfg_test_items(edited),
        strip_cfg_test_items("fn a() {}\nfn b() {}\n")
    );
}

// ---------------------------------------------------------------------------
// Open Causal Decision Plane: decision protocol guards (CDP-01; SP-CDP-C 3.4)
// ---------------------------------------------------------------------------

/// Occurrences of `Name {` in `production` that build a value of `name`: a
/// struct literal, not the type's own `struct`/`impl` header or a function's
/// `-> Name {` return type.
pub(super) fn struct_literal_count(production: &str, name: &str) -> usize {
    let needle = format!("{name} {{");
    production
        .match_indices(&needle)
        .filter(|(start, _)| {
            let before = &production[..*start];
            if before
                .chars()
                .next_back()
                .is_some_and(|character| is_ident_char(character) || character == ':')
            {
                return false;
            }
            let previous = before.trim_end();
            !["struct", "impl", "enum", "for", "trait", "->", "dyn", "<", "&"]
                .iter()
                .any(|token| previous.ends_with(token))
        })
        .count()
}

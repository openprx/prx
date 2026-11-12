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

/// Locate a top-level method inside its exact impl, ignoring literal/comment braces.
pub(super) fn impl_method_body(source: &str, implementation: &str, name: &str) -> String {
    let masked = blank_comments_and_literals(source);
    let start_byte = masked
        .match_indices(implementation)
        .find_map(|(offset, _)| {
            masked[offset + implementation.len()..]
                .trim_start()
                .starts_with('{')
                .then_some(offset)
        })
        .unwrap_or_else(|| panic!("missing {implementation}"));
    let chars: Vec<char> = masked.chars().collect();
    let start = masked[..start_byte].chars().count();
    let open = (start..chars.len())
        .find(|index| chars[*index] == '{')
        .expect("impl body");
    let close = close_of_group(&chars, open);
    let needle: Vec<char> = format!("fn {name}(").chars().collect();
    let mut depth = 0usize;
    let method_start = (open..close)
        .find(|index| {
            match chars[*index] {
                '{' => depth += 1,
                '}' => depth = depth.checked_sub(1).expect("impl brace depth"),
                _ => {}
            }
            depth == 1
                && chars
                    .get(*index..*index + needle.len())
                    .is_some_and(|text| text == needle.as_slice())
        })
        .unwrap_or_else(|| panic!("missing {implementation}::{name}"));
    let method_open = (method_start..close)
        .find(|index| chars[*index] == '{')
        .expect("method body");
    let method_close = close_of_group(&chars, method_open);
    let original: Vec<char> = source.chars().collect();
    original[method_start..method_close].iter().collect()
}

#[test]
fn impl_method_lookup_stays_inside_the_exact_impl_and_method_scope() {
    let source = r#"
impl PreparedInputOther { fn new() { prefix_constructor(); } }
impl PreparedInput {
    fn before() { fn new() { nested_constructor(); } }
    fn new() { let brace = "}"; /* { */ intended_constructor(); }
}
"#;
    let method = impl_method_body(source, "impl PreparedInput", "new");
    assert!(method.contains("intended_constructor()"), "{method}");
    assert!(!method.contains("prefix_constructor()"), "{method}");
    assert!(!method.contains("nested_constructor()"), "{method}");
}

/// The source files that are test code because every `mod` declaration that
/// loads them is `#[cfg(test)]` (INT-2B a48). A file whose own first line is
/// `#![cfg(test)]` is test code by [`is_test_only_file`]; this covers the
/// test modules that carry no marker, such as `#[cfg(test)] mod x_tests;`
/// and `#[cfg(test)] #[path = "turn_tests/x.rs"] mod x;`. It replaces a
/// file-name rule (`ends_with("tests.rs")`), which also skipped production
/// files such as `contests.rs`. A file no declaration loads is production.
pub(super) fn test_module_files() -> BTreeSet<String> {
    let files: Vec<(String, String)> = rust_source_files()
        .iter()
        .map(|path| (relative_path(path), read_source(path)))
        .collect();
    test_module_files_of(&files)
}

/// [`test_module_files`] over explicit `(relative path, source)` pairs.
///
/// All offsets are character indices: `blank_comments_and_literals` and
/// `strip_cfg_test_items` keep character positions, not byte positions, so
/// the raw text (for the `#[path]` literal), the masked text and the
/// production text are read at the same character index (INT-2B B4f L-4).
pub(super) fn test_module_files_of(files: &[(String, String)]) -> BTreeSet<String> {
    // Keep loader edges until classification: an ordinary declaration in a
    // test-only parent cannot turn its child into production. Conversely, a
    // second production loader must keep a shared child in the inventory.
    let mut edges = Vec::new();
    let mut test_only = BTreeSet::new();
    for (relative, raw_text) in files {
        if is_test_only_file(raw_text) {
            test_only.insert(relative.clone());
        }
        let raw: Vec<char> = raw_text.chars().collect();
        let masked_text = blank_comments_and_literals(raw_text);
        let masked: Vec<char> = masked_text.chars().collect();
        let production: Vec<char> = strip_cfg_test_items(&masked_text).chars().collect();
        let dir = relative.rsplit_once('/').map_or("", |(dir, _)| dir);
        let file = relative.rsplit('/').next().unwrap_or_default();
        let module_dir = if matches!(file, "mod.rs" | "lib.rs" | "main.rs") {
            dir.to_string()
        } else {
            format!("{dir}/{}", file.trim_end_matches(".rs"))
        };
        collect_module_loaders(
            relative,
            &raw,
            &masked,
            &production,
            0,
            masked.len(),
            &module_dir,
            dir,
            &mut edges,
        );
    }
    loop {
        let before = test_only.clone();
        let mut production_loaded = BTreeSet::new();
        let mut test_loaded = BTreeSet::new();
        for (parent, child, locally_test_only) in &edges {
            if *locally_test_only || test_only.contains(parent) {
                test_loaded.insert(child.clone());
            } else {
                production_loaded.insert(child.clone());
            }
        }
        test_only.extend(test_loaded.difference(&production_loaded).cloned());
        if test_only == before {
            return test_only;
        }
    }
}

/// Resolve declarations in their actual inline module namespace. In
/// `host.rs`, `mod tests { mod child; }` loads `host/tests/child.rs`, not
/// `host/child.rs`. A #[path] at file scope is relative to the containing
/// file; inside an inline module it is relative to that module's directory.
#[allow(clippy::too_many_arguments)]
fn collect_module_loaders(
    parent: &str,
    raw: &[char],
    masked: &[char],
    production: &[char],
    begin: usize,
    end: usize,
    module_dir: &str,
    path_dir: &str,
    edges: &mut Vec<(String, String, bool)>,
) {
    let keyword = ['m', 'o', 'd'];
    let mut start = begin;
    while start < end {
        if !masked[start..end].starts_with(&keyword)
            || (start > 0 && is_ident_char(masked[start - 1]))
            || !masked
                .get(start + keyword.len())
                .is_some_and(|character| character.is_whitespace())
        {
            start += 1;
            continue;
        }
        let mut cursor = start + keyword.len();
        while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
            cursor += 1;
        }
        // Rust raw identifiers name the same module/file as the identifier
        // without `r#`; omitting a production raw loader could hide a child.
        if masked.get(cursor..cursor + 2) == Some(&['r', '#']) {
            cursor += 2;
        }
        let name_start = cursor;
        while masked.get(cursor).copied().is_some_and(is_ident_char) {
            cursor += 1;
        }
        let name: String = masked[name_start..cursor].iter().collect();
        while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
            cursor += 1;
        }
        assert!(
            !name.is_empty() && matches!(masked.get(cursor), Some(';' | '{')),
            "{parent}: unsupported module identifier or declaration; cannot prove test-only reachability"
        );
        let attributes_start = masked[..start]
            .iter()
            .rposition(|character| matches!(character, ';' | '}' | '{'))
            .map_or(0, |index| index + 1);
        let explicit_path = module_path_attribute(raw, masked, attributes_start, start);
        if masked.get(cursor) == Some(&'{') {
            let close = close_of_group(masked, cursor).min(end);
            let inline_dir = match &explicit_path {
                Ok(Some(path)) => resolve_module_path(parent, path_dir, path),
                Ok(None) => format!("{module_dir}/{name}"),
                Err(()) => panic!("{parent}: unsupported inline module path; cannot prove test-only reachability"),
            };
            collect_module_loaders(
                parent,
                raw,
                masked,
                production,
                cursor + 1,
                close.saturating_sub(1),
                &inline_dir,
                &inline_dir,
                edges,
            );
            start = close;
            continue;
        }
        if masked.get(cursor) != Some(&';') {
            start += 1;
            continue;
        }
        let target = match explicit_path {
            Ok(Some(path)) => vec![resolve_module_path(parent, path_dir, &path)],
            Ok(None) => vec![format!("{module_dir}/{name}.rs"), format!("{module_dir}/{name}/mod.rs")],
            // A skipped unknown production loader could hide a mixed-load
            // child. Require review instead of granting any exemption.
            Err(()) => panic!("{parent}: unsupported module path; cannot prove test-only reachability"),
        };
        let locally_test_only = production.get(start..start + keyword.len()) != Some(&keyword[..]);
        for candidate in target {
            edges.push((parent.to_string(), candidate, locally_test_only));
        }
        start = cursor + 1;
    }
}

/// Only repository-relative module paths can establish an exemption. Reject
/// absolute paths and parent traversal beyond the repository; do not silently
/// clip an external loader into an unrelated repository path.
fn resolve_module_path(parent: &str, base: &str, path: &str) -> String {
    assert!(
        !std::path::Path::new(path).is_absolute() && !path.contains('\\'),
        "{parent}: unsupported absolute module path; cannot prove test-only reachability"
    );
    let candidate = format!("{base}/{path}");
    let mut components = Vec::new();
    for component in candidate.split('/') {
        match component {
            "." | "" => {}
            ".." => assert!(
                components.pop().is_some(),
                "{parent}: module path escapes repository; cannot prove test-only reachability"
            ),
            _ => components.push(component),
        }
    }
    components.join("/")
}

/// Read only real path attributes; a matching string or comment is blanked
/// in `masked` and cannot redirect module classification. Unknown attribute
/// path syntax stops the classifier rather than exempting another file.
fn module_path_attribute(raw: &[char], masked: &[char], begin: usize, end: usize) -> Result<Option<String>, ()> {
    let mut cursor = begin;
    let mut explicit_path = None;
    while cursor + 1 < end {
        if masked[cursor] != '#' || masked[cursor + 1] != '[' {
            cursor += 1;
            continue;
        }
        let close = close_of_group(masked, cursor + 1).min(end);
        let (word, mut value) = masked_word_at(masked, cursor + 2);
        if word == "cfg_attr" {
            // A conditional path can load different files in production.
            let attribute: String = masked[cursor..close].iter().collect();
            if attribute.contains("path") {
                return Err(());
            }
        }
        if word == "path" {
            while value < close && masked[value].is_whitespace() {
                value += 1;
            }
            if masked.get(value) != Some(&'=') {
                return Err(());
            }
            value += 1;
            // The literal's characters were blanked, so locate its quote in
            // raw text only after proving the attribute name on masked text.
            while value < close && raw[value].is_whitespace() {
                value += 1;
            }
            if raw.get(value) != Some(&'"') {
                return Err(());
            }
            value += 1;
            let start = value;
            while value < close && raw[value] != '"' {
                if raw[value] == '\\' {
                    return Err(());
                }
                value += 1;
            }
            if value == close {
                return Err(());
            }
            if explicit_path.is_some() {
                return Err(());
            }
            explicit_path = Some(raw[start..value].iter().collect());
        }
        cursor = close;
    }
    Ok(explicit_path)
}

/// Whether the file at `relative` is test code: its own `#![cfg(test)]`
/// marker, or it is loaded only by `#[cfg(test)]` module declarations.
pub(super) fn is_test_code_file(relative: &str, source: &str, test_modules: &BTreeSet<String>) -> bool {
    is_test_only_file(source) || test_modules.contains(relative)
}

//! Decision engine boundaries (R18). The engine may depend on the neutral
//! homes of the items it shares with the rest of the crate; those homes never
//! depend on the engine.

use super::helpers::{
    blank_comments_and_literals, compact_rust_path_text, is_ident_char, is_test_code_file, module_resolved_paths,
    module_segments, read_source, relative_path, rust_source_files, strip_cfg_test_items, test_module_files,
};

/// The neutral homes (R15-R17, R52, OV-14). There is no exception for
/// `src/config`: the engine configuration schema lives there and the
/// dependency is engine -> config only.
const NEUTRAL_HOMES: [&str; 9] = [
    "src/tools/",
    "src/security/",
    "src/providers/",
    "src/memory/",
    "src/observability/",
    "src/session_worker/",
    "src/config/",
    "src/cost/",
    "src/runtime/decision_journal",
];

/// Whether `compact` (whitespace removed) names the engine module from the
/// crate root: a path (`crate::decision_engine::..`), a bare module
/// (`use crate::decision_engine;`, `.. as engine`) or an entry of a grouped
/// `crate::{..}` import.
fn names_the_engine(compact: &str) -> bool {
    const ENGINE: &str = "decision_engine";
    let mut search = 0;
    while let Some(found) = compact[search..].find("crate::") {
        let start = search + found + "crate::".len();
        search = start;
        let rest = &compact[start..];
        if let Some(after) = rest.strip_prefix(ENGINE) {
            if !after.starts_with(is_ident_char) {
                return true;
            }
            continue;
        }
        let Some(group) = rest.strip_prefix('{') else {
            continue;
        };
        let mut depth = 1_usize;
        let mut end = group.len();
        for (offset, character) in group.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        let names_it = group[..end].split([',', '{']).any(|entry| {
            entry
                .strip_prefix(ENGINE)
                .is_some_and(|after| !after.starts_with(is_ident_char))
        });
        if names_it {
            return true;
        }
    }
    false
}

/// R18 (P1-I8): no production file under the neutral homes imports
/// `crate::decision_engine`. `#[cfg(test)]` items and test-only files are
/// skipped (the `providers/selection.rs` `access_spy` call is a
/// `#[cfg(test)]` statement).
#[test]
pub(super) fn shared_types_are_not_engine_owned() {
    let test_modules = test_module_files();
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !NEUTRAL_HOMES.iter().any(|home| relative.starts_with(home)) {
            continue;
        }
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let compact: String = production.split_whitespace().collect();
        if names_the_engine(&compact) {
            violations.push(relative);
        }
    }
    assert!(scanned > 0, "the neutral homes hold production files");
    assert!(
        violations.is_empty(),
        "neutral homes never import the decision engine (R18): {violations:?}"
    );
}

/// Roots the engine never imports (R41, P1-I9). Matching is on whole path
/// segments, so `crate::tools::execution_binding` is not `crate::tools::execution`.
const FORBIDDEN_ENGINE_IMPORTS: [&str; 8] = [
    "agent",
    "chat",
    "channels",
    "gateway",
    "runtime",
    "tools::execution",
    "tools::sessions_spawn",
    "tools::delegate",
];

/// Whether a crate-rooted `path` is (or is inside) one of the forbidden roots.
fn is_forbidden_engine_import(path: &str) -> bool {
    FORBIDDEN_ENGINE_IMPORTS.iter().any(|root| {
        path.strip_prefix(root)
            .is_some_and(|after| after.is_empty() || after.starts_with("::"))
    })
}

/// R41 (P1-I9): no production file under `src/decision_engine/` imports
/// `crate::{agent, chat, channels, gateway, runtime}` or
/// `crate::tools::{execution, sessions_spawn, delegate}`. `#[cfg(test)]`
/// items, `tests.rs` and `*_tests.rs` files are skipped.
#[test]
pub(super) fn decision_engine_never_imports_agent_or_tool_execution() {
    let test_modules = test_module_files();
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision_engine/") {
            continue;
        }
        let file_name = relative.rsplit('/').next().unwrap_or(&relative);
        if file_name == "tests.rs" || file_name.ends_with("_tests.rs") {
            continue;
        }
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let compact = compact_rust_path_text(&production);
        let module = module_segments(&relative);
        for named in module_resolved_paths(&compact, &module) {
            if is_forbidden_engine_import(&named) {
                violations.push(format!("{relative}: crate::{named}"));
            }
        }
    }
    assert!(scanned > 0, "the decision engine holds production files");
    assert!(
        violations.is_empty(),
        "the decision engine never imports agent, chat, channels, gateway, runtime or tool execution (R41): {violations:?}"
    );
}

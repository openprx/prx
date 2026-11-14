//! AG-28: trust-bearing turn state has exactly the reviewed production
//! constructors.

use super::helpers::{
    blank_comments_and_literals, identifier_count, is_ident_char, read_source, relative_path, rust_source_files,
    strip_cfg_test_items,
};

/// The only production files that may build trust-bearing turn state
/// (D-066 N-3): the turn opening, which writes the `TrustedParts` literal,
/// and the segment observation, which records the previous segment's
/// verified observation. Keep exact files; no other runtime module may
/// construct state.
const TRUST_BEARING_CALLERS: &[&str] = &[
    "src/runtime/turn_context/opening.rs",
    "src/runtime/child_turn/segment_observation.rs",
];

/// The trust-bearing constructors, named by identifiers no other code uses,
/// with the file that defines each once (`from_runtime` moved to the
/// neutral turn scope, R52).
const TRUST_BEARING_CONSTRUCTORS: [&str; 3] = ["from_runtime", "from_runtime_records", "recorded_by_runtime"];
const TRUST_BEARING_DEFINITIONS: [(&str, &str); 3] = [
    ("from_runtime", "src/tools/capability/turn_state.rs"),
    ("from_runtime_records", "src/decision_engine/state.rs"),
    ("recorded_by_runtime", "src/decision_engine/state.rs"),
];

/// Production files that may name one trust-bearing constructor exactly once
/// without becoming a [`TRUST_BEARING_CALLERS`] entry: every other AG-28 rule
/// (literals, field writes, further constructors) still applies to them.
/// Strip P1 R9: `decision_advisory::build_state` reuses
/// `TrustedState::from_runtime` over the trusted parts the turn opening
/// pinned in `TurnRuntimeContext`; it builds no new trust.
const TRUST_BEARING_REUSERS: [(&str, &str); 1] = [("from_runtime", "src/runtime/decision_advisory.rs")];

/// AG-28 (CDP-PLAN 1.9, scope amended by D-066 N-3 and its amendment).
/// Trusted and observed state carry authority: a trusted part grants or
/// excludes capabilities, and a failed step marks a candidate
/// `failed_before`. Only the turn opening may build them. Outside
/// [`TRUST_BEARING_CALLERS`], no production code
///
/// - names `TrustedState::from_runtime`, `ObservedState::from_runtime_records`
///   or `ObservedStep::recorded_by_runtime` at all: called, passed as a
///   function value (`.map(TrustedState::from_runtime)`) or through an alias
///   (the identifiers are unique to these constructors, so the type path is
///   not needed); the defining file names each exactly once, in its `fn`;
/// - writes a `TrustedParts { .. }` literal, borrowed or not;
/// - assigns a turn state's `.trusted` or `.observed`, or borrows either
///   mutably (`std::mem::replace`, `swap` and `take` need `&mut`).
///
/// Comments and literals are blanked first, and the scan runs over the
/// whitespace-normalized token text, so formatting cannot hide a use.
#[test]
pub(super) fn trust_bearing_turn_state_is_built_only_by_the_turn_opening() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if TRUST_BEARING_CALLERS.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        for constructor in TRUST_BEARING_CONSTRUCTORS {
            let allowed = usize::from(
                TRUST_BEARING_DEFINITIONS
                    .iter()
                    .any(|(name, file)| *name == constructor && relative == *file),
            );
            let reused = usize::from(
                TRUST_BEARING_REUSERS
                    .iter()
                    .any(|(name, file)| *name == constructor && relative == *file),
            );
            let named = identifier_count(&compact, constructor);
            let defined = identifier_count(&compact, &format!("fn {constructor}"));
            if named > allowed + reused || defined > allowed {
                violations.push(format!("{relative}: names {constructor}"));
            }
        }
        let mut search = 0;
        while let Some(found) = compact[search..].find("TrustedParts {") {
            let start = search + found;
            search = start + "TrustedParts {".len();
            // Not a literal: the definition, an impl header, or a return type
            // (owned or borrowed) right before a function body. A borrowed
            // literal (`&TrustedParts { .. }` in an expression) is one.
            let before = compact[..start].trim_end();
            let before = before.strip_suffix("mut").map_or(before, str::trim_end);
            let before = before.strip_suffix('&').map_or(before, str::trim_end);
            let is_type_position = ["struct", "for", "impl", "->"]
                .iter()
                .any(|token| before.ends_with(token));
            if !is_type_position {
                violations.push(format!("{relative}: TrustedParts literal"));
            }
        }
        for field in [".trusted", ".observed"] {
            let mut search = 0;
            while let Some(found) = compact[search..].find(field) {
                let start = search + found;
                search = start + field.len();
                if compact[search..].chars().next().is_some_and(is_ident_char) {
                    continue;
                }
                let assigned = compact[search..].starts_with(" =") && !compact[search..].starts_with(" ==");
                // A mutable borrow of the field: `&mut <path>.field` with a
                // path of identifiers, dots and calls before it.
                let path_start = compact[..start]
                    .rfind(|character: char| !(is_ident_char(character) || matches!(character, '.' | '(' | ')')))
                    .map_or(0, |index| index + 1);
                let borrowed =
                    compact[..path_start].trim_end().ends_with("&mut") || compact[..path_start].ends_with("&mut ");
                if assigned || borrowed {
                    violations.push(format!("{relative}: writes {field}"));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "trusted and observed turn state is built only by the turn opening (AG-28): {violations:?}"
    );
}

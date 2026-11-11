use super::helpers::struct_literal_count;
use super::helpers::{
    blank_comments_and_literals, function_body, is_ident_char, normalize_line, read_source, relative_path,
    repository_root, rust_source_files, strip_cfg_test_items,
};
use super::tool_scanner::contains_identifier;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
/// The only production files that may build trust-bearing turn state
/// (D-066 N-3). CDP-21 creates the turn decision service; until then no
/// production file builds any.
// Service-owned child modules consume only privately issued source/terminal
// evidence. Keep exact files; no other runtime module may construct state.
const TRUST_BEARING_CALLERS: &[&str] = &[
    "src/runtime/turn_decision.rs",
    "src/runtime/turn_decision/routing.rs",
    "src/runtime/turn_decision/segment_observation.rs",
];

/// The trust-bearing constructors, named by identifiers no other code uses,
/// with the file that defines each once.
const TRUST_BEARING_CONSTRUCTORS: [&str; 3] = ["from_runtime", "from_runtime_records", "recorded_by_runtime"];
const TRUST_BEARING_DEFINITIONS: &str = "src/causal_tree/state.rs";

/// How often `name` occurs in `text` as a whole identifier.
pub(super) fn identifier_count(text: &str, name: &str) -> usize {
    let mut count = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find(name) {
        let start = search + found;
        search = start + name.len();
        let before = text[..start].chars().next_back();
        let after = text[search..].chars().next();
        if !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char) {
            count += 1;
        }
    }
    count
}

/// AG-28 (CDP-PLAN 1.9, scope amended by D-066 N-3 and its amendment).
/// Trusted and observed state carry authority: a trusted part grants or
/// excludes capabilities, and a failed step marks a candidate
/// `failed_before`. Only the turn decision service may build them. Outside
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
pub(super) fn trust_bearing_turn_state_is_built_only_by_the_turn_decision_service() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if TRUST_BEARING_CALLERS.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        for constructor in TRUST_BEARING_CONSTRUCTORS {
            let allowed = usize::from(relative == TRUST_BEARING_DEFINITIONS);
            let named = identifier_count(&compact, constructor);
            let defined = identifier_count(&compact, &format!("fn {constructor}"));
            if named > allowed || defined > allowed {
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
        "trusted and observed turn state is built only by the turn decision service (AG-28): {violations:?}"
    );
}

/// The text of the brace-delimited block that opens at the first `{` at or
/// after `from`, braces included.
pub(super) fn braced_block(text: &str, from: usize) -> Option<&str> {
    let open = from + text[from..].find('{')?;
    let mut depth = 0usize;
    for (offset, character) in text[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open..=open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `impl` header in `production` (whitespace-normalized, up to its
/// `{`) that names `name`, with the block it opens.
fn impl_blocks_naming<'a>(production: &'a str, name: &str) -> Vec<(String, &'a str)> {
    let mut blocks = Vec::new();
    for (start, _) in production.match_indices("impl") {
        let before_ok = start == 0 || !is_ident_char(production[..start].chars().next_back().unwrap_or(' '));
        let after_ok = production[start + 4..]
            .chars()
            .next()
            .is_some_and(|character| character == '<' || character.is_whitespace());
        if !(before_ok && after_ok) {
            continue;
        }
        let Some(open) = production[start..].find('{').map(|offset| start + offset) else {
            continue;
        };
        let header = normalize_line(&production[start..open]);
        if header.contains(';') || !contains_identifier(&header, name) {
            continue;
        }
        if let Some(block) = braced_block(production, open) {
            blocks.push((header, block));
        }
    }
    blocks
}

/// AG-29. Every production request constructor is a reviewed redacted
/// projection or a neutral diagnostic request. A new call site must receive
/// an explicit privacy review before it can reach a decision backend.
fn decision_seal_sites(source: &str) -> (usize, usize, usize) {
    let production = strip_cfg_test_items(&blank_comments_and_literals(source));
    (
        production.matches("DecisionRequest::seal(").count(),
        production
            .matches("DecisionRequest::seal_with_projection_audit(")
            .count(),
        production.matches("DecisionRequest::seal_recovering_parts(").count(),
    )
}

#[test]
pub(super) fn only_reviewed_production_sites_seal_decision_requests() {
    let mut found = BTreeMap::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let counts = decision_seal_sites(&read_source(&path));
        if counts != (0, 0, 0) {
            found.insert(relative, counts);
        }
    }
    assert_eq!(
        found,
        BTreeMap::from([
            ("src/causal_tree/admin.rs".to_string(), (1, 0, 0)),
            ("src/causal_tree/questions.rs".to_string(), (0, 0, 1)),
            ("src/decision/factory.rs".to_string(), (1, 0, 0)),
        ]),
        "AG-29 decision request constructors changed; review redacted or neutral state provenance"
    );
}

#[test]
fn probe_decision_seal_inventory_rejects_an_extra_caller() {
    let planted = "fn unreviewed() { let _ = DecisionRequest::seal(parts, key, expiry, limits); }";
    assert_eq!(decision_seal_sites(planted), (1, 0, 0));
    assert_ne!(decision_seal_sites(planted), (0, 0, 0));
    let recovered = "fn unreviewed() { DecisionRequest::seal_recovering_parts(parts, audit, key, expiry, limits); }";
    assert_eq!(decision_seal_sites(recovered), (0, 0, 1));
    let hidden = format!("#[cfg(test)] mod tests {{ {recovered} }}");
    assert_eq!(decision_seal_sites(&hidden), (0, 0, 0));
}

/// AG-19 and AG-20 apply to the compiler's production body. Runtime
/// injection tests cover values; this source guard catches a new construction
/// path that starts reading untrusted names or schema metadata directly.
fn decision_compiler_source_errors(source: &str) -> Vec<&'static str> {
    let production = strip_cfg_test_items(source);
    let mut errors = Vec::new();
    if production.matches("redacted_projection(state)?").count() != 1
        || production.matches("state: projection,").count() != 1
        || production.matches("parts.state =").count() != 1
        || !production.contains("parts.state = shrink_oldest_artifact_summary(&parts.state)?.ok_or(CompileError::Budget)?;")
        || production.matches("parts = *recovered;").count() != 1
        || production.matches("seal_recovering_parts(").count() != 1
        || !production.contains("seal_recovering_parts(\n                parts,")
        || !production.contains("Err((DecisionError::InvalidRequest(RequestRule::RequestTooLarge), recovered)) => {\n                    parts = *recovered;")
        || production.contains("projection.clone()")
        || production.contains("parts.clone()")
    {
        errors.push("request state must move from redacted projection and recover only for checked size shrinking");
    }
    let allowed_option_calls = [
        "OptionId::code(candidate.action().as_str())",
        "OptionId::code(action)",
        "OptionId::code(\"other\")",
        "OptionId::code(candidate.target().as_str())",
        "OptionId::code(target.as_str())",
        "OptionId::code(target)",
        "OptionId::code(\"none\")",
    ];
    if production.matches("OptionId::code(").count() != 10
        || allowed_option_calls.iter().any(|call| {
            let expected = match *call {
                "OptionId::code(target)"
                | "OptionId::code(candidate.action().as_str())"
                | "OptionId::code(candidate.target().as_str())" => 2,
                _ => 1,
            };
            production.matches(call).count() != expected
        })
    {
        errors.push("option ids must be derived from typed action or target values");
    }
    let allowed_formats = [
        "format!(\"$.candidates[{index}]\")",
        "format!(\"Does {path} apply to this state?\")",
        "format!(\"Rate the risk of {path}.\")",
        "format!(\"Would {path} succeed?\")",
        "format!(\"Rate the task gain from {path}.\")",
    ];
    if production.matches("format!(").count() != allowed_formats.len()
        || allowed_formats.iter().any(|call| production.matches(call).count() != 1)
    {
        errors.push("question paths and text must use only the typed projected candidate index");
    }
    for forbidden in [
        "ToolSpec",
        "CapabilityCatalog",
        "inputSchema",
        ".schema",
        "QuestionId::from",
        "QuestionId::parse",
        "CandidateId::from",
        "CandidateId::parse",
        "OptionId::code(state",
        "OptionId::code(user",
    ] {
        if production.contains(forbidden) {
            errors.push("compiler reads schema/catalog or constructs an id from untyped input");
        }
    }
    errors
}

#[test]
pub(super) fn decision_compiler_uses_typed_ids_and_only_redacted_state() {
    let source = read_source(&repository_root().join("src/causal_tree/questions.rs"));
    assert_eq!(decision_compiler_source_errors(&source), Vec::<&str>::new());
    let planted_id = format!("{source}\nfn planted() {{ let _ = QuestionId::parse(user_text); }}");
    assert!(
        !decision_compiler_source_errors(&planted_id).is_empty(),
        "expected a non-empty value"
    );
    let planted_schema = format!("{source}\nfn planted() {{ let _ = entry.schema; }}");
    assert!(
        !decision_compiler_source_errors(&planted_schema).is_empty(),
        "expected a non-empty value"
    );
    let planted_option = format!("{source}\nfn planted() {{ let _ = OptionId::code(state.routing_input.as_str()); }}");
    assert!(
        !decision_compiler_source_errors(&planted_option).is_empty(),
        "expected a non-empty value"
    );
    let planted_path =
        format!("{source}\nfn planted() {{ let _ = format!(\"$.candidates[{{}}]\", state.observed.retry_count()); }}");
    assert!(
        !decision_compiler_source_errors(&planted_path).is_empty(),
        "expected a non-empty value"
    );
    let planted_state = format!("{source}\nfn planted() {{ parts.state = unredacted_state; }}");
    assert!(
        !decision_compiler_source_errors(&planted_state).is_empty(),
        "expected a non-empty value"
    );
    for (old, new) in [
        ("state: projection,", "state: unredacted_state,"),
        (
            "shrink_oldest_artifact_summary(&parts.state)?",
            "shrink_oldest_artifact_summary(&unrelated_state)?",
        ),
        (
            "RequestRule::RequestTooLarge), recovered",
            "RequestRule::ModelInvalid), recovered",
        ),
        (
            "DecisionRequest::seal_recovering_parts(",
            "DecisionRequest::unchecked_seal(",
        ),
    ] {
        assert!(source.contains(old));
        assert!(
            !decision_compiler_source_errors(&source.replacen(old, new, 1)).is_empty(),
            "expected a non-empty value"
        );
    }
}

/// AG-23. Validation always runs against the sealed request: the validator
/// takes the `DecisionRequest` and reads its manifest; a `ValidatedDecision`
/// is built only in the validator; the manifest is built only in
/// `protocol.rs` (by `DecisionRequest::seal`); and no code builds a
/// `DecisionRequest` literal by name (`seal` builds it as `Self`), so every
/// manifest the validator reads matches the request content.
#[test]
pub(super) fn validation_requires_the_sealed_request_manifest() {
    let owners: [(&str, &[&str]); 3] = [
        ("ValidatedDecision", &["src/decision/validate.rs"]),
        ("DecisionRequest", &[]),
        ("DecisionRequestManifest", &["src/decision/protocol.rs"]),
    ];
    let mut sites = BTreeMap::<&str, Vec<String>>::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (name, _) in owners {
            for _ in 0..struct_literal_count(&production, name) {
                sites.entry(name).or_default().push(relative.clone());
            }
        }
    }
    for (name, owner) in owners {
        assert_eq!(
            sites.get(name).cloned().unwrap_or_default(),
            owner.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "`{name}` struct literals (AG-23)"
        );
    }

    let validator = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/validate.rs"),
    )));
    let body = function_body(&validator, "validate");
    let header: String = body[..body.find('{').expect("validate has a body")]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        header.contains("request: &DecisionRequest,") && header.contains("response: DecisionResponse,"),
        "`validate` takes the sealed request and the response: {header}"
    );
    assert!(
        body.contains("request.manifest"),
        "`validate` reads the manifest of the request it validates against"
    );

    // D-057 M-1: a sealed request cannot be changed. Its fields and the
    // manifest's are private, every `impl` of either type lives in
    // `protocol.rs` and takes no `&mut self`, and no production code holds a
    // `&mut` to one.
    let protocol = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/protocol.rs"),
    )));
    for name in ["validate_parts", "from_validated_parts"] {
        let declaration = format!("fn {name}(");
        let declarations: Vec<_> = protocol
            .lines()
            .filter(|line| line.contains(&declaration))
            .map(str::trim)
            .collect();
        assert_eq!(
            declarations,
            vec![declaration.as_str()],
            "checked validation/construction helpers stay protocol-private"
        );
    }
    assert_eq!(protocol.matches("Self::validate_parts(").count(), 2);
    assert_eq!(protocol.matches("Self::from_validated_parts(").count(), 2);
    let validation = function_body(&protocol, "validate_parts");
    let construction = function_body(&protocol, "from_validated_parts");
    assert!(!validation.contains("Uuid::now_v7"));
    assert!(validation.contains("super::wire::salted_question_ids("));
    assert_eq!(protocol.matches("Uuid::now_v7()").count(), 1);
    assert_eq!(construction.matches("Uuid::now_v7()").count(), 1);
    for name in ["DecisionRequest", "DecisionRequestManifest"] {
        let declaration = format!("pub struct {name} ");
        let start = protocol
            .find(&declaration)
            .unwrap_or_else(|| panic!("protocol.rs declares {name}"));
        let body = braced_block(&protocol, start).expect("struct body");
        let public_fields: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("pub "))
            .collect();
        assert!(
            public_fields.is_empty(),
            "`{name}` must keep its fields private (AG-23): {public_fields:?}"
        );
    }
    let mut impls = BTreeSet::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for name in ["DecisionRequest", "DecisionRequestManifest"] {
            for (header, block) in impl_blocks_naming(&production, name) {
                assert!(
                    !block.contains("&mut self"),
                    "{relative}: `{header}` takes `&mut self` on a sealed request (AG-23)"
                );
                impls.insert(format!("{relative}: {header}"));
            }
            let compact: String = production.split_whitespace().collect();
            assert!(
                !compact.contains(&format!("&mut{name}")),
                "{relative}: holds `&mut {name}`; a sealed request is immutable (AG-23)"
            );
        }
    }
    let expected: BTreeSet<String> = [
        "src/decision/protocol.rs: impl DecisionRequest",
        "src/decision/protocol.rs: impl DecisionRequestManifest",
        "src/decision/protocol.rs: impl Eq for DecisionRequest",
        "src/decision/protocol.rs: impl PartialEq for DecisionRequest",
        "src/decision/protocol.rs: impl fmt::Debug for DecisionRequest",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(impls, expected, "the `impl` blocks of the sealed request types (AG-23)");
}

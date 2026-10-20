use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, function_body, is_test_only_file, read_source,
    relative_path, repository_root, rust_source_files, strip_cfg_test_items,
};
use super::loop_prompt::REVIEWED_DECISION_HANDOFFS;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
pub(super) fn compact_rust_tokens(expression: &str) -> String {
    expression
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
pub(super) fn every_loop_engine_call_receives_a_turn_decision_context() {
    let mut actual = BTreeMap::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (_, request) in engine_turn_requests(&production) {
            assert!(
                request_field(&request, "routing").is_none(),
                "{relative}: removed routing field returned to the shared request"
            );
            let decision = request_field(&request, "decision")
                .unwrap_or_else(|| panic!("{relative}: shared engine request has no decision"));
            let (_, _, expected) = REVIEWED_DECISION_HANDOFFS
                .iter()
                .find(|(file, _, _)| *file == relative)
                .unwrap_or_else(|| panic!("{relative}: unreviewed shared engine entrypoint"));
            assert_eq!(
                compact_rust_tokens(&decision),
                *expected,
                "{relative}: decision handoff changed; review its service, context and source provenance"
            );
            *actual.entry(relative.clone()).or_insert(0usize) += 1;
        }
        if relative == "src/agent/embedded_turn.rs" {
            let body = compact_rust_tokens(&function_body(&production, "run_work"));
            assert_eq!(
                body.matches("letsource_owner=source;letsource=source_owner.as_ref();")
                    .count(),
                1
            );
            assert_eq!(
                body.matches(".record_guarded(input,&owner,cancellation,defer_model_selection)")
                    .count(),
                1
            );
        }
        if relative == "src/gateway/mod.rs" {
            let body = compact_rust_tokens(&function_body(&production, "run_gateway_chat_with_multimodal"));
            for binding in [
                "let(source_receipt,gateway_user_event,mutadmission_guard)=pending_input.record(message.to_owned(),fabric_ctx.idempotency_key.clone()).await?;",
                "letgateway_proof=source_receipt.bind(&turn_runtime,fabric_ctx)?;",
                "letservice=turn_runtime.decision_slot.publish(&turn_runtime,state).await?;",
                "letcontext=service.decide_agent_initial(&gateway_proof,crate::runtime::turn_decision::AgentTurnKind::SingleShot,&cancellation,).await?;",
            ] {
                assert_eq!(
                    body.matches(binding).count(),
                    1,
                    "Gateway source/service admission provenance changed"
                );
            }
        }
        if relative == "src/chat/dispatcher.rs" {
            assert!(
                compact_rust_tokens(&production).contains("letSome(routing)=binding.decision_routing()else{"),
                "Chat dispatcher must unwrap its admitted binding before the engine handoff"
            );
        }
        if relative == "src/agent/turn_request.rs" {
            assert!(
                compact_rust_tokens(&production).contains("pub(crate)decision:TurnDecision<'a>,"),
                "the shared request must require a non-optional decision context"
            );
        }
    }
    let expected: BTreeMap<_, _> = REVIEWED_DECISION_HANDOFFS
        .iter()
        .map(|(path, count, _)| ((*path).to_owned(), *count))
        .collect();
    assert_eq!(
        actual, expected,
        "shared engine inventory changed; reconcile all EP owners, including future websocket turns"
    );
}

/// The one loop-engine entry, by the name a call site spells.
const ENGINE_ENTRY: &str = "run_turn";

/// Every `run_turn` call in already-stripped production source, as the call's
/// byte offset paired with the text of its `TurnRequest` argument (the second
/// of its four arguments).
pub(super) fn engine_turn_requests(production: &str) -> Vec<(usize, String)> {
    let bytes = production.as_bytes();
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(found) = production[search..].find(ENGINE_ENTRY) {
        let start = search + found;
        search = start + ENGINE_ENTRY.len();
        if start > 0 && is_identifier_byte(bytes[start - 1]) {
            continue;
        }
        let end = start + ENGINE_ENTRY.len();
        if end < bytes.len() && is_identifier_byte(bytes[end]) {
            continue;
        }
        if production[..start].trim_end().ends_with("fn") {
            continue;
        }
        let mut cursor = end;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            continue;
        }
        let Some(arguments) = top_level_arguments(production, cursor) else {
            continue;
        };
        assert_eq!(
            arguments.len(),
            4,
            "a `run_turn` call with {} arguments does not match `run_turn(history, request, services, delivery)`",
            arguments.len()
        );
        calls.push((start, arguments[1].clone()));
    }
    calls
}

/// The value of field `name` in the first struct literal of `expression`
/// (`Type { a: x, b }`); the shorthand `b` yields `b`. `None` when absent.
pub(super) fn request_field(expression: &str, name: &str) -> Option<String> {
    let open = expression.find('{')?;
    let bytes = expression.as_bytes();
    let mut depth = 0i32;
    let mut field_start = open + 1;
    let mut fields = Vec::new();
    for index in open..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    fields.push(&expression[field_start..index]);
                    break;
                }
            }
            b',' if depth == 1 => {
                fields.push(&expression[field_start..index]);
                field_start = index + 1;
            }
            _ => {}
        }
    }
    fields.into_iter().find_map(|field| {
        let field = field.trim();
        match field.split_once(':') {
            Some((key, value)) if key.trim() == name && !value.starts_with(':') => Some(value.trim().to_string()),
            None if field == name => Some(field.to_string()),
            _ => None,
        }
    })
}

pub(super) const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Split the call that opens at `open` into its top-level argument expressions.
pub(super) fn top_level_arguments(production: &str, open: usize) -> Option<Vec<String>> {
    let bytes = production.as_bytes();
    let mut depth = 0i32;
    let mut arguments: Vec<String> = Vec::new();
    let mut argument_start = open + 1;
    for index in open..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let last = production[argument_start..index].to_string();
                    if !last.trim().is_empty() {
                        arguments.push(last);
                    }
                    return Some(arguments);
                }
            }
            b',' if depth == 1 => {
                arguments.push(production[argument_start..index].to_string());
                argument_start = index + 1;
            }
            _ => {}
        }
    }
    None
}

pub(super) fn leading_identifier(expression: &str) -> Option<String> {
    let trimmed = expression.trim_start_matches(|character: char| {
        character.is_whitespace() || character == '&' || character == '*' || character == '('
    });
    let identifier: String = trimmed
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    (!identifier.is_empty()).then_some(identifier)
}

/// The initializer of the last `let <name> =` that appears before `before`.
pub(super) fn nearest_let_initializer(production: &str, name: &str, before: usize) -> Option<String> {
    let limit = before.min(production.len());
    let mut best: Option<usize> = None;
    for pattern in [format!("let {name} ="), format!("let mut {name} =")] {
        let mut search = 0usize;
        while search < limit {
            let Some(found) = production[search..limit].find(&pattern) else {
                break;
            };
            let start = search + found;
            search = start + pattern.len();
            best = Some(best.map_or(start, |current: usize| current.max(start)));
        }
    }
    let equals = production[best?..].find('=')? + best?;
    let bytes = production.as_bytes();
    let mut depth = 0i32;
    for index in equals + 1..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => return Some(production[equals + 1..index].to_string()),
            _ => {}
        }
    }
    None
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
fn test_module_files_of(files: &[(String, String)]) -> BTreeSet<String> {
    const fn is_identifier_char(character: char) -> bool {
        character.is_ascii_alphanumeric() || character == '_'
    }
    let mut production_loaded = BTreeSet::new();
    let mut test_loaded = BTreeSet::new();
    for (relative, raw_text) in files {
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
        let keyword = ['m', 'o', 'd', ' '];
        for start in 0..masked.len() {
            if !masked[start..].starts_with(&keyword) || (start > 0 && is_identifier_char(masked[start - 1])) {
                continue;
            }
            let mut cursor = start + keyword.len();
            while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
                cursor += 1;
            }
            let name_start = cursor;
            while masked.get(cursor).copied().is_some_and(is_identifier_char) {
                cursor += 1;
            }
            let name: String = masked[name_start..cursor].iter().collect();
            while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
                cursor += 1;
            }
            if name.is_empty() || masked.get(cursor) != Some(&';') {
                continue;
            }
            // The attributes right above the declaration, delimited on the
            // masked text; the `#[path]` literal is read from the raw text
            // at the same character positions.
            let attributes_start = masked[..start]
                .iter()
                .rposition(|character| matches!(character, ';' | '}' | '{'))
                .map_or(0, |index| index + 1);
            let attributes: String = raw[attributes_start..start].iter().collect();
            let target = attributes
                .find("#[path = \"")
                .and_then(|at| {
                    let value = &attributes[at + "#[path = \"".len()..];
                    value.find('"').map(|end| format!("{dir}/{}", &value[..end]))
                })
                .map_or_else(
                    || vec![format!("{module_dir}/{name}.rs"), format!("{module_dir}/{name}/mod.rs")],
                    |path| vec![path],
                );
            let in_production = production.get(start..start + keyword.len()) == Some(&keyword[..]);
            for candidate in target {
                if in_production {
                    production_loaded.insert(candidate);
                } else {
                    test_loaded.insert(candidate);
                }
            }
        }
    }
    test_loaded.difference(&production_loaded).cloned().collect()
}

/// Whether the file at `relative` is test code: its own `#![cfg(test)]`
/// marker, or it is loaded only by `#[cfg(test)]` module declarations.
pub(super) fn is_test_code_file(relative: &str, source: &str, test_modules: &BTreeSet<String>) -> bool {
    is_test_only_file(source) || test_modules.contains(relative)
}

/// Whether a `compaction` argument is `None` in any spelling (INT-2B a48):
/// `None`, `Option::None` under any path, with generic arguments anywhere
/// (`None::<T>`, `Option::<T>::None`), or a plain name the nearest
/// `let <name> =` before the call binds to one of those. Known gaps: a typed
/// binding (`let c: Option<_> = None;`), a binding more than one hop away, a
/// `None` returned from a function, and `Default::default()` (which is
/// `None` for an `Option`) are not followed.
pub(super) fn compaction_argument_is_none(production: &str, argument: &str, call_start: usize) -> bool {
    fn spelled_none(text: &str) -> bool {
        let compact: String = text.split_whitespace().collect();
        let compact = compact.trim_end_matches(';');
        // Drop every `::<…>` generic argument list (balanced), wherever it is.
        let mut path = String::with_capacity(compact.len());
        let mut depth = 0usize;
        let mut rest = compact;
        while let Some(character) = rest.chars().next() {
            if depth == 0 && rest.starts_with("::<") {
                depth = 1;
                rest = &rest[3..];
                continue;
            }
            if depth > 0 {
                match character {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    _ => {}
                }
            } else {
                path.push(character);
            }
            rest = &rest[character.len_utf8()..];
        }
        path == "None" || path.ends_with("Option::None")
    }
    if spelled_none(argument) {
        return true;
    }
    let trimmed = argument.trim();
    leading_identifier(trimmed)
        .filter(|name| name == trimmed)
        .and_then(|name| nearest_let_initializer(production, &name, call_start))
        .is_some_and(|initializer| spelled_none(&initializer))
}

/// INT-2B a48, RED before (the guards compared the literal text `None` and
/// skipped every file named `*tests.rs`): every spelling of `None` counts,
/// and a file is test code only when it is loaded only under `#[cfg(test)]`.
#[test]
fn compaction_none_spellings_and_test_module_files_are_exact() {
    let production = "fn f() {\n    let budget = None;\n    let kept = Some(&config.agent.compaction);\n}\n";
    let call = production.len();
    for none in [
        "None",
        " None ",
        "Option::None",
        "std::option::Option::None",
        "None::<&AgentCompactionConfig>",
        // B4f L-5
        "Option::<&AgentCompactionConfig>::None",
        "std::option::Option::<Option<u8>>::None",
        "budget",
    ] {
        assert!(compaction_argument_is_none(production, none, call), "{none:?} is None");
    }
    for some in [
        "Some(&config.agent.compaction)",
        "kept",
        "compaction_config",
        "Nonesuch",
    ] {
        assert!(
            !compaction_argument_is_none(production, some, call),
            "{some:?} is not None"
        );
    }

    let modules = test_module_files();
    for test_module in [
        "src/providers/label_redaction_tests.rs",
        "src/providers/tool_dialect_config_tests.rs",
        "src/providers/catalog/resolver_tests.rs",
    ] {
        assert!(
            modules.contains(test_module),
            "{test_module} is loaded only under cfg(test)"
        );
    }
    // L-4: `#[path]` modules declared after non-ASCII comment text resolve
    // to their real files (character offsets, not byte offsets).
    for engine_module in [
        "src/agent/turn_tests/engine_surface.rs",
        "src/agent/turn_tests/engine_budget.rs",
        "src/agent/turn_tests/engine_stay_silent.rs",
    ] {
        assert!(
            modules.contains(engine_module),
            "{engine_module} is loaded only under cfg(test)"
        );
    }
    let planted = vec![(
        "src/x/host.rs".to_string(),
        "// é ü — multibyte text before the declaration\n#[cfg(test)]\n#[path = \"host_cases.rs\"]\nmod cases;\n// ß\nmod plain;\n"
            .to_string(),
    )];
    let planted_modules = test_module_files_of(&planted);
    assert_eq!(
        planted_modules,
        BTreeSet::from(["src/x/host_cases.rs".to_string()]),
        "a #[path] literal after multibyte comments is read from the right place"
    );
    // Loaded by a production `#[path]` declaration, but test code by its own
    // `#![cfg(test)]` marker.
    let marked = "src/agent/turn_tests/context_paging.rs";
    assert!(is_test_code_file(
        marked,
        &read_source(&repository_root().join(marked)),
        &modules
    ));
    for production_file in [
        "src/agent/context_paging.rs",
        "src/providers/mod.rs",
        "src/agent/contests.rs",
    ] {
        assert!(
            !modules.contains(production_file),
            "{production_file} is not a test module"
        );
    }
}

/// Every production turn hands the loop engine a context-budget config.
///
/// `compaction_config = None` switches off the whole bounded-request contract
/// for that turn: no pre-turn flush or paging, no preflight before each
/// provider call (so the compiled-request event reports `budgeted_*: null`),
/// and no reserved mid-turn trim. The Gateway webhook turn shipped that way
/// (OQ-23), which left `POST /webhook` and the WhatsApp, Linq and Nextcloud
/// Talk webhooks unbudgeted while every other surface was budgeted.
///
/// A `compaction: None` in a `TurnRequest`, in any spelling (see
/// [`compaction_argument_is_none`]), is recorded by its enclosing function
/// (test modules excluded by [`is_test_code_file`]). The
/// engine has no pass-through calls of its own. There is no allowed site: the
/// last one, `agent_turn`, was removed with the library-only `process_message`
/// (E2).
///
/// MUTATION GUARD: put `None` back as the webhook's compaction argument and the
/// inventory gains `src/gateway/mod.rs::run_gateway_chat_with_multimodal`.
#[test]
pub(super) fn production_turns_pass_a_context_budget_to_the_loop_engine() {
    let mut unbudgeted = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        // Test modules (loaded only under `#[cfg(test)]`) drive the engine
        // without a budget on purpose. They are not execution surfaces.
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        for (call_start, request) in engine_turn_requests(&production) {
            let compaction = request_field(&request, "compaction")
                .unwrap_or_else(|| panic!("{relative}: a `run_turn` request without a `compaction` field"));
            if !compaction_argument_is_none(&production, &compaction, call_start) {
                continue;
            }
            let line_index = production[..call_start].matches('\n').count();
            unbudgeted.push(format!("{relative}::{}", enclosing_function(&lines, line_index)));
        }
    }

    assert_inventory("production loop engine calls without a context budget", unbudgeted, &[]);
}

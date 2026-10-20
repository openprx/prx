use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, function_body, is_test_only_file,
    normalize_line, read_source, relative_path, repository_root, rust_source_and_test_files, rust_source_files,
    strip_cfg_test_items,
};
use super::loop_requests::{compact_rust_tokens, is_identifier_byte, top_level_arguments};
use super::scanner_meta::source_functions;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
/// The chat route-decision log names the count it carries for what it is.
///
/// `tools_registry` in `chat::run` is the sealed base registry, so its length
/// is the number of registry **root** tools before routing, refresh and alias
/// expansion. Logging it as `tools_count` read as the number of tools sent to
/// the model (SPLASH-F4: 48 logged against 71 on the wire). The wire count is
/// the compiled-request event's `tool_snapshot.count`.
///
/// MUTATION GUARD: rename the field back to `tools_count` and this fails.
#[test]
pub(super) fn chat_route_decision_names_the_registry_root_count() {
    let chat_files = rust_source_files()
        .into_iter()
        .filter(|path| relative_path(path).starts_with("src/chat/"));
    let mut misleading = Vec::new();
    let mut named = 0usize;
    for path in chat_files {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let normalized = normalize_line(&production);
        if normalized.contains("tools_count = tools_registry.len()") {
            misleading.push(relative.clone());
        }
        named += normalized.matches("registry_root_tools = tools_registry.len()").count();
    }
    assert!(
        misleading.is_empty(),
        "a chat log labels the registry root count as `tools_count`; name it `registry_root_tools`: {misleading:?}"
    );
    assert_eq!(
        named, 1,
        "the chat route decision must report the registry root count as `registry_root_tools`"
    );
}

#[test]
pub(super) fn context_handoff_has_one_production_owner_and_exact_lookup_is_registered() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            // The whole text of every file that is not test-only by its
            // `#![cfg(test)]` marker: a test that asserts on the marker in a
            // production file makes that file an owner, as it always did, so
            // no stripping error can hide a second producer.
            (!is_test_only_file(&source) && source.contains("[context_handoff]")).then_some(relative)
        })
        .collect::<Vec<_>>();
    assert_eq!(owners, vec!["src/agent/compaction.rs"]);

    let tools = fs::read_to_string(repository_root().join("src/tools/mod.rs")).expect("read src/tools/mod.rs");
    assert!(tools.contains("TranscriptHistoryLookupTool::new(memory.clone())"));
    let policy =
        fs::read_to_string(repository_root().join("src/security/policy.rs")).expect("read src/security/policy.rs");
    assert!(policy.contains("\"transcript_history_lookup\""));

    for relative in [
        "src/agent/turn_engine.rs",
        "src/session_worker/runner/tests/tool_selection.rs",
        "src/tools/delegate.rs",
        "src/tools/sessions_spawn/tool_proxy.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains("TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME"),
            "{relative} must preserve the hard-switch recovery dependency"
        );
    }
    let loop_source =
        fs::read_to_string(repository_root().join("src/agent/turn_engine.rs")).expect("read src/agent/turn_engine.rs");
    assert!(loop_source.contains("context_mode_uses_os_paging"));
    assert!(loop_source.contains("tool_specs.push(required)"));
}

/// The agent loop module `src/agent/loop_.rs` stays deleted (LOOP D4).
///
/// Its items live in owner modules (`turn_engine`, `entry`, `tool_scheduler`,
/// ...). Nothing may bring the old module back under its name or as a
/// compatibility alias: the file is absent, `src/agent/` has no `compat.rs` or
/// `loop.rs`, and no file under `src/`, `tests/`, `benches/`, `examples/` or
/// `scripts/` spells a `mod` declaration of it, an `agent::` path into it or a
/// path through it. Comments count too: a comment that names the old path
/// points a reader at a module that does not exist.
#[test]
fn the_agent_loop_module_stays_deleted() {
    fn collect_all_files(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("read directory entry").path();
            if path.is_dir() {
                collect_all_files(&path, files);
            } else {
                files.push(path);
            }
        }
    }

    let root = repository_root();
    let old_module = ["loop", "_"].concat();
    assert!(
        !root.join("src/agent").join(format!("{old_module}.rs")).exists(),
        "src/agent/{old_module}.rs must stay deleted"
    );
    for name in ["compat.rs", "loop.rs"] {
        assert!(
            !root.join("src/agent").join(name).exists(),
            "src/agent/{name} must not reintroduce the agent loop module"
        );
    }

    let mut files = Vec::new();
    for dir in ["src", "tests", "benches", "examples", "scripts"] {
        collect_all_files(&root.join(dir), &mut files);
    }
    files.sort();
    let mut offenders = Vec::new();
    for path in files {
        let bytes = fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let text = String::from_utf8_lossy(&bytes);
        for mention in deleted_loop_module_mentions(&text) {
            offenders.push(format!("{}: {mention}", relative_path(&path)));
        }
    }
    assert!(
        offenders.is_empty(),
        "the deleted agent loop module is named again: {offenders:?}"
    );
}

/// Every way `text` names the deleted agent loop module (the old name is
/// built at run time so this file never spells it): its `mod` declaration, an
/// `agent::` path into it, a path through it, an alias that brings the name
/// back (`as <name>`, `pub use .. as <name>`), and the raw-identifier spelling
/// (`r#<name>`). Word-bounded, so longer names that merely contain it (a
/// `tool_` prefix, an `x` suffix) are other names.
fn deleted_loop_module_mentions(text: &str) -> Vec<String> {
    let old_module = ["loop", "_"].concat();
    let banned = [
        format!("mod {old_module}"),
        format!("agent::{old_module}"),
        format!("{old_module}::"),
        format!("as {old_module}"),
        format!("r#{old_module}"),
    ];
    let mut mentions = Vec::new();
    for pattern in &banned {
        let mut search = 0usize;
        while let Some(found) = text[search..].find(pattern.as_str()) {
            let start = search + found;
            search = start + pattern.len();
            let before = text[..start].chars().next_back();
            if before.is_some_and(|character| character.is_alphanumeric() || character == '_') {
                continue;
            }
            let after = text[start + pattern.len()..].chars().next();
            if !pattern.ends_with("::")
                && after.is_some_and(|character| character.is_alphanumeric() || character == '_')
            {
                continue;
            }
            mentions.push(pattern.clone());
        }
    }
    mentions
}

/// The scan behind `the_agent_loop_module_stays_deleted` catches every
/// spelling that could bring the module back, not only the file: planted
/// declarations, paths and aliases are reported, look-alike names are not.
#[test]
fn the_deleted_loop_module_scan_catches_every_spelling() {
    let old_module = ["loop", "_"].concat();
    for planted in [
        format!("mod {old_module};"),
        format!("pub mod {old_module} {{}}"),
        format!("use crate::agent::{old_module}::run_turn;"),
        format!("let _ = {old_module}::run_turn;"),
        format!("use crate::agent::turn_engine as {old_module};"),
        format!("pub use crate::agent::turn_engine as {old_module};"),
        format!("mod r#{old_module};"),
        format!("use crate::agent::r#{old_module}::X;"),
    ] {
        assert!(
            !deleted_loop_module_mentions(&planted).is_empty(),
            "not reported: {planted}"
        );
    }
    for look_alike in [
        "run_tool_call_loop_outcome".to_string(),
        format!("tool_{old_module}::x"),
        format!("mod {old_module}x;"),
        format!("use a as {old_module}x;"),
        format!("the former `agent/{old_module}.rs`"),
    ] {
        assert!(
            deleted_loop_module_mentions(&look_alike).is_empty(),
            "reported a look-alike: {look_alike}"
        );
    }
}

/// The loop engine has one typed entry (LOOP D5).
///
/// `run_turn(history, request, services, delivery)` replaced three positional
/// entries of 23 to 26 arguments, several of them `Option<&str>` or `bool`
/// neighbours a call site could swap without a type error. So: no production
/// function named after the old entries (`run_tool_call_loop*`, `agent_turn`)
/// anywhere in `src/`; `run_turn` is the only function of
/// `src/agent/turn_engine.rs` visible outside it; and no function there takes
/// more than `MAX_ENGINE_PARAMETERS` parameters.
///
/// MUTATION GUARD: add back a `pub(crate) async fn run_tool_call_loop_outcome`
/// with its positional parameters and all three assertions fail.
#[test]
pub(super) fn the_loop_engine_has_one_typed_entry() {
    const MAX_ENGINE_PARAMETERS: usize = 6;
    let old_entries = ["run_tool_call_loop", "agent_turn"];
    let mut revived = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        for name in source_functions(&production)
            .into_iter()
            .map(|(name, _test, _body)| name)
        {
            if old_entries.iter().any(|old| name.starts_with(old)) {
                revived.push(format!("{relative}::{name}"));
            }
        }
    }
    assert_inventory(
        "production functions named after the removed engine entries",
        revived,
        &[],
    );

    let engine = strip_cfg_test_items(&blank_comments_and_literals(
        &fs::read_to_string(repository_root().join("src/agent/turn_engine.rs")).expect("read src/agent/turn_engine.rs"),
    ));
    let mut visible = Vec::new();
    let mut wide = Vec::new();
    let mut search = 0usize;
    while let Some(found) = engine[search..].find("fn ") {
        let at = search + found;
        search = at + 3;
        if at > 0 && is_identifier_byte(engine.as_bytes()[at - 1]) {
            continue;
        }
        let name: String = engine[at + 3..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        let Some(open) = engine[at..].find('(').map(|offset| at + offset) else {
            continue;
        };
        let parameters = top_level_arguments(&engine, open).unwrap_or_default();
        if parameters.len() > MAX_ENGINE_PARAMETERS {
            wide.push(format!("{name}: {} parameters", parameters.len()));
        }
        let line_start = engine[..at].rfind('\n').map_or(0, |index| index + 1);
        if engine[line_start..at].contains("pub") {
            visible.push(name);
        }
    }
    assert_eq!(
        visible,
        vec!["run_turn"],
        "run_turn must be the only engine function visible outside turn_engine.rs"
    );
    assert!(
        wide.is_empty(),
        "engine functions take at most {MAX_ENGINE_PARAMETERS} parameters: {wide:?}"
    );
}

/// User-authored text enters a loop history only neutralised (LOOP D5b).
///
/// The loop reads a `user` message that opens with one of its reserved
/// markers (`[Tool results]`, `[Post-compaction context refresh]`,
/// `[Recalled context from earlier in this session]`) as its own bookkeeping.
/// So every ingress builds user messages through
/// `turn_contracts::inbound_user_message` / `inbound_user_message_owned`
/// (fresh input) or
/// `turn_contracts::stored_turn_message` (stored turns rebuilt into a
/// history), which defuse those markers. Every other production construction of
/// a `ChatMessage` is listed here with the reason it cannot carry user text
/// into a loop history. `src/providers/` is skipped as a whole: provider
/// adapters convert one call's input into their wire format and build no loop
/// history.
///
/// MUTATION GUARD: build a channel or console turn with `ChatMessage::user(..)`
/// instead of `inbound_user_message(..)` and the inventory gains that function.
#[test]
pub(super) fn user_text_enters_histories_only_neutralized() {
    let mut sites = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        // test files are exempt by their `#![cfg(test)]` marker (the stripper
        // empties them), never by their name
        if relative.starts_with("src/providers/") || relative == "src/agent/turn_contracts.rs" {
            continue;
        }
        let source = read_source(&path);
        let production = without_feature_gated_test_items(&strip_cfg_test_items(&blank_comments_and_literals(&source)));
        let lines: Vec<&str> = production.lines().collect();
        // `ChatMessage::user` without its `(`: a path used as a function
        // (`.map(ChatMessage::user)`) builds user messages as well (R-LOOPD5 G-2)
        for pattern in ["ChatMessage::user", "ChatMessage {"] {
            let mut search = 0usize;
            while let Some(found) = production[search..].find(pattern) {
                let start = search + found;
                search = start + pattern.len();
                if start > 0 && is_identifier_byte(production.as_bytes()[start - 1]) {
                    continue;
                }
                if pattern == "ChatMessage::user"
                    && production
                        .as_bytes()
                        .get(start + pattern.len())
                        .is_some_and(|byte| is_identifier_byte(*byte))
                {
                    continue;
                }
                let before = production[..start].trim_end();
                if before.ends_with("struct")
                    || before.ends_with("impl")
                    || before.ends_with("for")
                    || before.ends_with("->")
                {
                    continue;
                }
                let line_index = production[..start].matches('\n').count();
                sites.push(format!("{relative}::{}", enclosing_function(&lines, line_index)));
            }
        }
    }
    sites.sort();
    assert_inventory(
        "production ChatMessage constructions outside inbound helpers / stored_turn_message",
        sites,
        USER_MESSAGE_CONSTRUCTION_SITES,
    );
}

/// `text` with every item behind `#[cfg(all(test, ..))]` blanked (lines kept):
/// the shared stripper removes `#[cfg(test)]` items only.
fn without_feature_gated_test_items(text: &str) -> String {
    let mut out = text.to_string();
    let mut search = 0usize;
    while let Some(found) = out[search..].find("#[cfg(all(test") {
        let start = search + found;
        search = start + 1;
        let Some(open) = out[start..].find('{').map(|offset| start + offset) else {
            break;
        };
        // a braceless item (`#[cfg(all(test, ..))] use a::B;`) ends before any `{`
        if out[start..open].contains(';') {
            continue;
        }
        let bytes = out.as_bytes();
        let mut depth = 0usize;
        let mut close = None;
        for (index, byte) in bytes.iter().enumerate().skip(open) {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        let blanked: String = out[start..=close]
            .chars()
            .map(|character| if character == '\n' { '\n' } else { ' ' })
            .collect();
        out.replace_range(start..=close, &blanked);
    }
    out
}

/// `ChatMessage` constructions that are not user-authored text entering a loop
/// history, one entry per construction, with the reason.
const USER_MESSAGE_CONSTRUCTION_SITES: &[&str] = &[
    // Loop-written notes: the reserved markers themselves, and the corrective aside.
    "src/agent/compaction.rs::post_compaction_refresh_aside",
    "src/agent/context_paging.rs::recall_context_pages",
    "src/agent/response_guard.rs::append_corrective_aside",
    "src/agent/tool_result_recording.rs::drop",
    "src/agent/turn_engine.rs::run_turn_unguarded",
    // The legacy `Agent` dispatcher's own tool-result block and its conversion of
    // an already-built conversation (the user turns in it came through
    // `inbound_user_message` in `agent.rs`).
    "src/agent/dispatcher.rs::to_provider_messages",
    // Copies of messages already in a history (content truncated or normalised,
    // role kept): compaction and audit projections, multimodal normalisation.
    "src/agent/compaction.rs::bounded_compaction_projection",
    "src/chat/compaction.rs::bounded_legacy_chat_compaction_audit_source",
    "src/multimodal.rs::prepare_messages_for_provider",
    // One-shot provider calls that build no loop history.
    "src/plugins/capabilities/provider.rs::chat_with_system",
    "src/plugins/runtime.rs::chat_provider",
    "src/tools/image.rs::execute",
    // `run_sub_agent_task`: the operator steering turn, which opens with `[Steering instruction from
    // operator]` and so can never open with a reserved marker.
    "src/tools/sessions_spawn/task_mode.rs::run_sub_agent_task",
    // The session worker's steering turn, the same wrapper.
    "src/session_worker/runner/steering.rs::run_segments_with_steering",
];

/// The names the loop-note guards scan for cannot be hidden (R-LOOPD5 F-1).
///
/// `user_text_enters_histories_only_neutralized` spots user messages by the
/// spelling `ChatMessage::user(` / `ChatMessage {`, and
/// the removed `RoutingInput` fallback remains forbidden while the shared
/// request requires a source-owned decision context. An alias
/// (`ChatMessage as Msg`, `RoutingInput as RI`, `type Msg = ChatMessage;`) or
/// an import of the variants (`use ..RoutingInput::*;`,
/// `use ..RoutingInput::{LastUserMessage};`) would take a construction out of
/// their sight, as an alias of the deleted loop module would have. Production
/// code may therefore not spell any of these; provider adapters, which build
/// no loop history, are exempt as they are from the ingress guard.
///
/// MUTATION GUARD: add `use crate::agent::turn_request::RoutingInput as RI;`
/// to a production file and this fails.
#[test]
pub(super) fn loop_note_names_are_never_aliased_or_variant_imported() {
    let mut offenders = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative.starts_with("src/providers/") {
            continue;
        }
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        for mention in loop_note_name_aliases(&production) {
            offenders.push(format!("{relative}: {mention}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "production code hides a name the loop-note guards scan for: {offenders:?}"
    );
}

/// Every alias or variant import of `ChatMessage` / `RoutingInput` in `text`.
fn loop_note_name_aliases(text: &str) -> Vec<String> {
    let normalized = normalize_line(text);
    let mut found = Vec::new();
    for name in ["ChatMessage", "RoutingInput"] {
        for pattern in [
            format!("{name} as "),
            format!("= {name};"),
            format!("{name}::*"),
            format!("{name}::{{"),
        ] {
            let mut search = 0usize;
            while let Some(offset) = normalized[search..].find(pattern.as_str()) {
                let start = search + offset;
                search = start + pattern.len();
                let before = normalized[..start].chars().next_back();
                if before.is_some_and(|character| character.is_alphanumeric() || character == '_') {
                    continue;
                }
                found.push(pattern.clone());
            }
        }
    }
    // `use ..RoutingInput::LastUserMessage;` imports a variant by name, and
    // `type Msg = crate::providers::ChatMessage;` aliases through a path
    // (R-LOOPD5 G-2)
    for statement in normalized.split(';') {
        let mut statement = statement.trim();
        // attributes before the item (`#[cfg(..)] use ..`, `#![allow(..)] type ..`)
        while statement.starts_with('#')
            && let Some(close) = statement.find(']')
        {
            statement = statement[close + 1..].trim_start();
        }
        if statement.starts_with("use ") && statement.contains("RoutingInput::") {
            found.push(format!("variant import `{statement}`"));
        }
        let declaration = statement
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub(super) ")
            .trim_start_matches("pub ");
        if declaration.starts_with("type ")
            && let Some((_, target)) = declaration.split_once('=')
        {
            let target = target.trim();
            for name in ["ChatMessage", "RoutingInput"] {
                if target == name || target.ends_with(&format!("::{name}")) {
                    found.push(format!("type alias `{statement}`"));
                }
            }
        }
    }
    // inside `impl RoutingInput`, `Self::LastUserMessage` names the history
    // fallback without the spelling the routing-pin inventory counts
    if normalized.contains("Self::LastUserMessage") {
        found.push("Self::LastUserMessage".to_string());
    }
    found
}

/// A history rebuilt from a middleware's message list is neutralised
/// (R-LOOPD5 F-2 wiring, G-3).
///
/// The inbound WASM middleware may return a replacement message list; a user
/// message it adds or rewrites must not pose as one of the loop's own notes, so
/// the rebuilt history is assigned through `neutralize_middleware_user_messages`.
/// The guard reads every production `from_value::<Vec<ChatMessage>>(` rebuild
/// and the first assignment that follows it: the provider request copy
/// (`prepared_messages.messages`) may take the candidate as is; any other
/// target must be assigned through the neutraliser, and exactly one such
/// history rebuild exists.
///
/// MUTATION GUARD: change `*history = neutralize_middleware_user_messages(history,
/// candidate);` in `turn_engine.rs` to `*history = candidate;` and this fails.
#[test]
pub(super) fn middleware_history_rebuilds_neutralize_user_messages() {
    const REBUILD: &str = "from_value::<Vec<ChatMessage>>(";
    let mut neutralised = Vec::new();
    let mut offenders = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let mut search = 0usize;
        while let Some(found) = production[search..].find(REBUILD) {
            let start = search + found;
            search = start + REBUILD.len();
            let assignment = production[start..]
                .lines()
                .skip(1)
                .map(str::trim)
                .find(|line| line.contains(" = ") && line.ends_with(';'))
                .unwrap_or_default();
            let (target, value) = assignment.split_once(" = ").unwrap_or_default();
            if target == "prepared_messages.messages" {
                continue;
            }
            if value.starts_with("neutralize_middleware_user_messages(") {
                neutralised.push(format!("{relative}: {target}"));
            } else {
                offenders.push(format!("{relative}: `{assignment}`"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a middleware rebuild assigns its candidate without neutralising user messages: {offenders:?}"
    );
    assert_eq!(
        neutralised,
        vec!["src/agent/turn_engine.rs: *history"],
        "the inbound middleware history rebuild is the one neutralised rebuild"
    );
}

/// The `ChatMessage` constructors are pinned (R-LOOPD5 G-2).
///
/// The ingress guard exempts `src/providers/`, where `ChatMessage` is defined,
/// and spots user messages by `ChatMessage::user`. A new constructor there that
/// builds a `user` message (`ChatMessage::user_text`, a `From<String>` impl, a
/// free `fn .. -> ChatMessage`) would let every other module build one out of
/// the guard's sight. So `impl ChatMessage` may define exactly the four role
/// constructors, and nothing under `src/providers/` may add a `From` impl for
/// it or a free function returning it.
///
/// MUTATION GUARD: add `pub fn user_text(content: String) -> Self` to
/// `impl ChatMessage` and this fails.
#[test]
pub(super) fn chat_message_constructors_are_pinned() {
    let mut constructors = Vec::new();
    let mut other_builders = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/providers/") {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let normalized = normalize_line(&production);
        let mut search = 0usize;
        while let Some(found) = normalized[search..].find("impl ChatMessage {") {
            let open = search + found + "impl ChatMessage ".len();
            search = open + 1;
            let mut depth = 0usize;
            let mut close = normalized.len();
            for (index, byte) in normalized.bytes().enumerate().skip(open) {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            close = index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let body = &normalized[open..close];
            for signature in body.split("fn ").skip(1) {
                let name: String = signature
                    .chars()
                    .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                    .collect();
                let header = signature.split('{').next().unwrap_or_default();
                if header.contains("-> Self") || header.contains("-> ChatMessage") {
                    constructors.push(format!("{relative}::{name}"));
                }
            }
        }
        for pattern in ["for ChatMessage {", "-> ChatMessage {"] {
            if normalized.contains(pattern) {
                other_builders.push(format!("{relative}: `{pattern}`"));
            }
        }
    }
    constructors.sort();
    assert_eq!(
        constructors,
        vec![
            "src/providers/traits.rs::assistant",
            "src/providers/traits.rs::system",
            "src/providers/traits.rs::tool",
            "src/providers/traits.rs::user",
        ],
        "ChatMessage gained or lost a constructor; one that builds a user message must be added to the \
         ingress guard's patterns"
    );
    assert!(
        other_builders.is_empty(),
        "a ChatMessage builder outside `impl ChatMessage`: {other_builders:?}"
    );
}

/// The alias scan catches every hiding spelling and leaves plain uses alone.
#[test]
fn the_loop_note_alias_scan_catches_every_spelling() {
    for planted in [
        "use crate::providers::ChatMessage as Msg;",
        "use crate::providers::{ChatMessage as Msg, Provider};",
        "type Msg = ChatMessage;",
        "use crate::agent::turn_request::RoutingInput as RI;",
        "use crate::agent::turn_request::RoutingInput::*;",
        "use crate::agent::turn_request::RoutingInput::{LastUserMessage, Pinned};",
        "use crate::agent::turn_request::RoutingInput::LastUserMessage;",
        "type Msg = crate::providers::ChatMessage;",
        "pub(crate) type Input = super::turn_request::RoutingInput;",
        "match self { Self::LastUserMessage => None, Self::Pinned(text) => Some(text) }",
        "#![allow(unused)] #![cfg(test)] pub type Msg = crate::providers::ChatMessage;",
        "#[cfg(feature = \"x\")] use crate::agent::turn_request::RoutingInput::LastUserMessage;",
    ] {
        assert!(!loop_note_name_aliases(planted).is_empty(), "not reported: {planted}");
    }
    for plain in [
        "use crate::providers::{ChatMessage, Provider};",
        "let message = ChatMessage::user(text);",
        "input: crate::agent::turn_request::RoutingInput::Pinned(text),",
        "use crate::providers::ChatMessageKind as Kind;",
        "type Messages = Vec<ChatMessage>;",
        "type Kind = crate::providers::ChatMessageKind;",
        "let pinned = Self::Pinned(text);",
    ] {
        assert!(
            loop_note_name_aliases(plain).is_empty(),
            "reported a plain use: {plain}"
        );
    }
}

/// The source tree is English-only: no Rust file under `src/` or `tests/` may
/// carry a CJK character. That covers behavioural keyword tables, user-visible
/// copy, test fixtures, assertion messages and comments alike — a keyword table
/// that reads one natural language routes that language's users differently
/// from everyone else, and a comment nobody on the project can read is a
/// comment that stops being maintained.
///
/// The guard reads `\u{...}` escapes as well as literal characters, because the
/// two produce the very same string at run time; without that, the rule would
/// be one escape away from being bypassed.
///
/// Language-neutral substitutes exist for every legitimate use: `\u{20ac}` for a
/// three-byte code point, a Hangul syllable for an East-Asian-wide column, or
/// Cyrillic/Greek text for "some non-ASCII prose".
#[test]
fn src_and_tests_carry_no_cjk_characters() {
    const CJK_RANGES: &[(u32, u32)] = &[(0x3000, 0x30ff), (0x3400, 0x9fff), (0xff00, 0xffef)];

    fn is_cjk(value: u32) -> bool {
        CJK_RANGES.iter().any(|(start, end)| value >= *start && value <= *end)
    }

    let mut offenders: Vec<String> = Vec::new();
    for path in rust_source_and_test_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        for (index, line) in source.lines().enumerate() {
            let number = index + 1;
            if let Some(found) = line.chars().find(|ch| is_cjk(*ch as u32)) {
                offenders.push(format!("{relative}:{number}: literal U+{:04X}", found as u32));
            }
            let mut rest = line;
            while let Some(start) = rest.find("\\u{") {
                rest = &rest[start + 3..];
                let Some(end) = rest.find('}') else { break };
                if let Ok(value) = u32::from_str_radix(&rest[..end], 16)
                    && is_cjk(value)
                {
                    offenders.push(format!("{relative}:{number}: escaped U+{value:04X}"));
                }
                rest = &rest[end + 1..];
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "source and tests must be free of CJK characters, found {}:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The published tool schema is a runtime gate, not documentation.
///
/// `validate_tool_arguments` used to be reachable only from `#[cfg(test)]`
/// mirrors, while the production pipeline checked nothing but the root
/// `required` list. This guard fails if that regresses: the shipped
/// `ToolExecutionService::execute` must still validate every call against the
/// descriptor's own schema, before approval and before any backend runs.
#[test]
fn tool_arguments_are_schema_validated_on_the_production_execution_path() {
    let root = repository_root();
    let execution = fs::read_to_string(root.join("src/tools/execution.rs")).expect("read src/tools/execution.rs");
    let production = strip_cfg_test_items(&execution);
    let production = production.as_str();

    let validator = function_body(production, "enforce_command_contract");
    assert!(
        validator.contains("validate_tool_arguments("),
        "the production argument gate no longer calls the canonical schema validator; \
         a tool call that violates its published contract would reach its executor"
    );
    assert!(
        validator.contains("descriptor.parameters"),
        "the production argument gate must validate against the descriptor's published schema, \
         not a second hand-written rule set"
    );
    assert!(
        validator.contains("normalize_integral_numbers("),
        "the production argument gate no longer normalizes integral floats; a whole number a \
         provider spelled as a float would pass the gate and then read as a default in the executor"
    );

    let execute = function_body(production, "execute");
    assert!(
        {
            // The canonical pipeline is boxed so callers carry a pointer, not the
            // whole state machine; the boxed and the bare await are both the one
            // call into `execute_inner`.
            let compact = compact_rust_tokens(&execute);
            compact.contains("self.execute_inner(command,context,cancellation).await")
                || compact.contains("Box::pin(self.execute_inner(command,context,cancellation)).await")
        } && execute.matches("execute_inner(").count() == 1
            && function_body(production, "execute_inner")
                .matches("enforce_command_contract(")
                .count()
                == 1,
        "ToolExecutionService::execute no longer validates arguments; every entry point funnels \
         through it, so removing the call disables schema enforcement everywhere"
    );

    let schema = fs::read_to_string(root.join("src/tools/schema.rs")).expect("read src/tools/schema.rs");
    let schema_production = strip_cfg_test_items(&schema);
    assert!(
        schema_production.contains("pub fn compiled_tool_schema("),
        "the per-tool compiled-schema cache is gone; every production call would recompile its contract"
    );
}

/// A webhook turn must run its tool loop inside its own spawn boundary.
///
/// `gateway::tests::tool_surface::the_webhook_turns_tool_loop_runs_inside_the_seeded_spawn_context`
/// pins what `within_gateway_turn_spawn_context` does; this pins that the
/// shipped webhook turn still goes through it. Without this guard the seeding
/// can be deleted from `run_gateway_chat_with_multimodal` -- or replaced by a
/// hand-rolled `SPAWN_EXECUTION_CONTEXT.scope` over a bare context -- and every
/// test stays green while `sessions_spawn` becomes a way back to the tools the
/// operator hid from the gateway.
#[test]
fn the_gateway_webhook_turn_seeds_its_spawn_boundary() {
    let root = repository_root();
    let gateway = fs::read_to_string(root.join("src/gateway/mod.rs")).expect("read src/gateway/mod.rs");
    let production = strip_cfg_test_items(&gateway);
    let production = production.as_str();

    let turn = function_body(production, "run_gateway_chat_with_multimodal");
    assert!(
        turn.contains("within_gateway_turn_spawn_context("),
        "the webhook turn no longer seeds its spawn boundary; a sub-agent it starts would run with no          inherited exclusions and could call the tools the operator hid from the gateway"
    );
    assert!(
        !turn.contains("SPAWN_EXECUTION_CONTEXT"),
        "the webhook turn scopes a spawn context of its own instead of the seeded one; the seeding          must stay in within_gateway_turn_spawn_context, where it is under test"
    );

    let seeder = {
        let start = production
            .find("async fn within_gateway_turn_spawn_context")
            .expect("missing production function within_gateway_turn_spawn_context");
        let end = production[start..]
            .find("\n}\n")
            .expect("unterminated production function within_gateway_turn_spawn_context");
        &production[start..start + end]
    };
    assert!(
        seeder.contains("gateway_turn_spawn_context(") && seeder.contains("SPAWN_EXECUTION_CONTEXT"),
        "the gateway spawn seam must still build the turn context and scope the tool loop inside it"
    );
    assert!(
        !seeder.contains("with_channel_excluded_tools(Vec::new())"),
        "the gateway spawn seam empties the inherited exclusion list; that is the escape hatch this          boundary exists to close"
    );
}

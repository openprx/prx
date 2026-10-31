use super::decision_seals::braced_block;
use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, function_body, read_source, relative_path,
    repository_root, rust_source_files, strip_cfg_test_items,
};
use super::loop_requests::{
    compact_rust_tokens, compaction_argument_is_none, engine_turn_requests, is_identifier_byte, is_test_code_file,
    leading_identifier, nearest_let_initializer, request_field, test_module_files,
};
/// Where a loop-engine call's compaction config comes from, as far as the
/// source text shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactionSource {
    /// The call reaches a reviewed effective-window resolver or owner:
    /// the served model's effective window.
    Effective,
    /// The call reaches the raw `[agent.compaction]` table (or a default one)
    /// without passing through a resolver: the base window.
    Base,
    /// Neither is visible here; the value arrives as a parameter or a field
    /// resolved by the caller.
    Forwarded,
}

/// Text that proves the value went through the effective-window resolution.
const EFFECTIVE_COMPACTION_MARKERS: &[&str] = &[
    "resolve_effective_compaction_config(",
    "resolve_effective_compaction_config_with_catalog(",
    "admitted_binding.effective_compaction()",
    "self.effective_compaction(",
    "decided.service.effective_compaction(",
    "decision.service.effective_compaction(",
    "service.effective_compaction(",
];

/// Text that names the base window directly.
const BASE_COMPACTION_MARKERS: &[&str] = &[
    ".agent.compaction",
    "agent_compaction",
    "AgentCompactionConfig::default()",
];

/// Follow a compaction argument through at most four hops (the nearest `let`
/// binding of its leading identifier before the call, or a same-file `fn`,
/// including a `self.` method) until it names a resolver or the base table.
pub(super) fn classify_compaction_argument(production: &str, expression: &str, call_start: usize) -> CompactionSource {
    let mut text = expression.to_string();
    for _ in 0..4 {
        let compact = compact_rust_tokens(&text);
        let pinned_provider_binding = compact.starts_with("provider_binding.as_ref().ok_or_else(")
            && compact.ends_with(")?.effective_compaction()");
        if pinned_provider_binding
            || EFFECTIVE_COMPACTION_MARKERS.iter().any(|marker| {
                compact.match_indices(marker).any(|(start, _)| {
                    compact[..start]
                        .chars()
                        .next_back()
                        .is_none_or(|previous| !previous.is_alphanumeric() && previous != '_' && previous != '.')
                })
            })
        {
            return CompactionSource::Effective;
        }
        if BASE_COMPACTION_MARKERS.iter().any(|marker| text.contains(marker)) {
            return CompactionSource::Base;
        }
        let trimmed = text.trim_start();
        let unwrapped = trimmed.strip_prefix("Some(").unwrap_or(trimmed);
        let unwrapped = unwrapped.trim_start_matches(|character: char| character.is_whitespace() || character == '&');
        let method_target = unwrapped.strip_prefix("self.");
        let Some(name) = leading_identifier(method_target.unwrap_or(unwrapped)) else {
            return CompactionSource::Forwarded;
        };
        let same_file_fn = || {
            production
                .contains(&format!("fn {name}("))
                .then(|| function_body(production, &name))
        };
        let next = if method_target.is_some() {
            same_file_fn()
        } else {
            nearest_let_initializer(production, &name, call_start).or_else(same_file_fn)
        };
        let Some(next) = next else {
            return CompactionSource::Forwarded;
        };
        text = next;
    }
    CompactionSource::Forwarded
}

/// Loop-engine calls whose compaction config is resolved by their caller.
///
/// Each forwards a value that an upstream owner already resolved to the
/// effective window; the reason is recorded next to the entry.
const FORWARDED_COMPACTION_SITES: &[&str] = &[
    // Source-bound decision snapshots reserve the actual served model's window.
    "src/agent/embedded_turn.rs::run_work",
    "src/agent/entry.rs::run_with_runtime_envelope",
    "src/agent/entry.rs::run_with_runtime_envelope",
    "src/channels/turn.rs::run_channel_turn",
    // The reducer action carries its explicitly resolved compaction config.
    "src/chat/dispatcher.rs::drive_start_turn_stream",
    // Workers prefer admitted child context, retaining the signed manifest fallback.
    "src/session_worker/runner/run.rs::run_owned_manifest",
    "src/tools/delegate.rs::execute_tool_loop",
    "src/tools/sessions_spawn/task_mode.rs::run_sub_agent_task",
];

/// The sub-agent tool types and their constructors (INT-2B a46).
const SUB_AGENT_TOOL_CONSTRUCTORS: &[(&str, &[&str])] = &[
    ("DelegateTool", &["new", "with_depth"]),
    ("SessionsSpawnTool", &["new", "new_with_registry"]),
];

/// The local names `type_name` goes by in `production`: the name itself,
/// every `use … type_name as Alias` (grouped imports too) and every
/// `type Alias = …type_name;` (INT-2B B4f, the D-065 L-6 bar).
fn type_names(production: &str, type_name: &str) -> Vec<String> {
    let last_segment = |path: &str| {
        path.trim()
            .trim_start_matches('&')
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mut names = vec![type_name.to_string()];
    for statement in production.split(';') {
        let tokens: Vec<&str> = statement
            .split(|character: char| character.is_whitespace() || matches!(character, ',' | '{' | '}'))
            .filter(|token| !token.is_empty())
            .collect();
        if let Some(use_at) = tokens.iter().position(|token| *token == "use") {
            for window in tokens[use_at..].windows(3) {
                if last_segment(window[0]) == type_name && window[1] == "as" {
                    names.push(window[2].to_string());
                }
            }
        }
        if let Some(type_at) = tokens.iter().position(|token| *token == "type")
            && let (Some(alias), Some("="), Some(value)) = (
                tokens.get(type_at + 1),
                tokens.get(type_at + 2).copied(),
                tokens.get(type_at + 3),
            )
            && last_segment(value) == type_name
        {
            names.push(alias.split('<').next().unwrap_or_default().to_string());
        }
    }
    names
}

/// The type an `impl` block containing `offset` is for, if any: the name
/// after `for`, or after `impl` and its generics.
fn enclosing_impl_type(production: &str, offset: usize) -> Option<String> {
    let before = &production[..offset];
    let header_start = before.rfind("\nimpl")? + 1;
    let open = header_start + production[header_start..].find('{')?;
    let mut depth = 0usize;
    let mut end = production.len();
    for (index, character) in production[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + index;
                    break;
                }
            }
            _ => {}
        }
    }
    if offset > end {
        return None;
    }
    let header = &production[header_start..open];
    let target = header.split(" for ").nth(1).unwrap_or_else(|| {
        let after = header.trim_start_matches("impl");
        if after.starts_with('<') {
            after.split_once('>').map_or(after, |(_, rest)| rest)
        } else {
            after
        }
    });
    Some(
        target
            .trim()
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .split(['<', ' '])
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

/// Offsets of every call of one of `type_name`'s `functions` in
/// `production`, under any local name of the type, and as `Self::…` inside
/// the type's own `impl` outside the listed functions themselves (a
/// constructor delegating to another is not a construction site).
fn sub_agent_calls(production: &str, lines: &[&str], type_name: &str, functions: &[&str]) -> Vec<usize> {
    let bytes = production.as_bytes();
    let mut calls = Vec::new();
    let mut prefixes = type_names(production, type_name);
    prefixes.push("Self".to_string());
    for prefix in &prefixes {
        for function in functions {
            let needle = format!("{prefix}::{function}(");
            let mut search = 0usize;
            while let Some(found) = production[search..].find(&needle) {
                let at = search + found;
                search = at + needle.len();
                if at > 0 && (is_identifier_byte(bytes[at - 1]) || bytes[at - 1] == b':') && prefix != type_name {
                    continue;
                }
                if at > 0 && is_identifier_byte(bytes[at - 1]) {
                    continue;
                }
                if prefix == "Self" {
                    let line_index = production[..at].matches('\n').count();
                    let enclosing = enclosing_function(lines, line_index);
                    if enclosing_impl_type(production, at).as_deref() != Some(type_name)
                        || functions.contains(&enclosing.as_str())
                    {
                        continue;
                    }
                }
                calls.push(at);
            }
        }
    }
    calls.sort_unstable();
    calls
}

/// INT-2B B4f (D-065 L-6 bar): the sub-agent constructor scan sees the tool
/// types under an imported alias, a `type` alias and `Self::`, and does not
/// count a constructor delegating to another.
#[test]
fn sub_agent_constructor_scan_sees_aliases_and_self() {
    let production = "use crate::tools::DelegateTool as D;\nuse crate::tools::{SessionsSpawnTool as S, Other};\ntype T = crate::tools::DelegateTool;\nimpl DelegateTool {\n    pub fn new(a: A) -> Self {\n        Self::new_with_options(a)\n    }\n    pub fn child(&self) -> Self {\n        Self::new(x)\n    }\n}\nimpl Other {\n    fn default() -> Self {\n        Self::new(y)\n    }\n}\nfn f() {\n    let a = D::new(x);\n    let b = S::new_with_registry(y);\n    let c = T::with_depth(z);\n    let d = DelegateTool::new(w);\n}\n";
    let lines: Vec<&str> = production.lines().collect();
    let delegate = sub_agent_calls(
        production,
        &lines,
        "DelegateTool",
        &["new", "new_with_options", "with_depth", "with_depth_and_options"],
    );
    let spawn = sub_agent_calls(production, &lines, "SessionsSpawnTool", &["new", "new_with_registry"]);
    let text = |offsets: &[usize]| {
        offsets
            .iter()
            .map(|at| production[*at..].split('(').next().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text(&delegate),
        vec!["Self::new", "D::new", "T::with_depth", "DelegateTool::new"]
    );
    assert_eq!(text(&spawn), vec!["S::new_with_registry"]);
}

/// Every production sub-agent tool resolves its children's compaction window
/// from the child's admitted configuration generation (INT-2B a46).
///
/// A startup `CompactionResolver` is stale after configuration reload and its
/// `from_base` constructor also ignores `[[router.models]]` and model routes.
/// The production tools therefore resolve at child admission, using the
/// admitted generation's whole configuration and catalog. The constructor
/// inventory pins the reviewed creation sites so a new site cannot bypass
/// that internal admission path.
///
/// MUTATION GUARD: add an unreviewed `DelegateTool` or `SessionsSpawnTool`
/// construction and the constructor inventory gains that site.
#[test]
pub(super) fn sub_agent_tools_resolve_compaction_from_the_whole_configuration() {
    let mut from_base = Vec::new();
    let mut constructor_sites = Vec::new();
    let mut admitted_resolution_sites = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        let site = |at: usize| {
            let line_index = production[..at].matches('\n').count();
            format!("{relative}::{}", enclosing_function(&lines, line_index))
        };
        for at in sub_agent_calls(&production, &lines, "CompactionResolver", &["from_base"]) {
            from_base.push(site(at));
        }
        for (tool, constructors) in SUB_AGENT_TOOL_CONSTRUCTORS {
            for at in sub_agent_calls(&production, &lines, tool, constructors) {
                constructor_sites.push(site(at));
            }
        }
        if matches!(
            relative.as_str(),
            "src/tools/delegate.rs" | "src/tools/sessions_spawn/spawn.rs"
        ) {
            for (at, _) in production.match_indices("resolve_effective_compaction_config_with_catalog(") {
                admitted_resolution_sites.push(site(at));
            }
        }
    }

    assert_inventory("production CompactionResolver::from_base calls", from_base, &[]);
    assert_inventory(
        "reviewed production sub-agent tool constructions",
        constructor_sites,
        &[
            "src/channels/startup.rs::start_channels_with_config",
            "src/chat/run/startup.rs::session_tools",
            "src/gateway/mod.rs::register_gateway_turn_tools",
            "src/session_worker/runner/provider.rs::worker_spawn_tool",
            "src/tools/mod.rs::all_tools_with_runtime_ext_and_extensions",
        ],
    );
    assert_inventory(
        "admitted-generation sub-agent compaction resolution sites",
        admitted_resolution_sites,
        &[
            "src/tools/delegate.rs::resolve_pinned_child_compaction",
            "src/tools/sessions_spawn/spawn.rs::resolve_pinned_child_compaction",
        ],
    );
}

/// The producers behind `FORWARDED_COMPACTION_SITES`: where each forwarded
/// value is filled, as `(file, function, anchor, field)`. With a `field`, the
/// value is that field of the struct literal that starts at `anchor`; without
/// one, `anchor` is the argument expression itself.
const FORWARDED_COMPACTION_PRODUCERS: &[(&str, &str, &str, Option<&str>)] = &[
    // `Effect::StartTurn.compaction_config`, carried from this action.
    (
        "src/chat/run.rs",
        "run_internal",
        "Action::StartLLMTurn {",
        Some("compaction_config"),
    ),
    // `WorkerManifest.compaction_config` (process branch).
    (
        "src/tools/sessions_spawn/spawn.rs",
        "execute_spawn_with_id",
        "&process_compaction_config",
        None,
    ),
];

/// The values `FORWARDED_COMPACTION_SITES` receive are resolved to the
/// effective window where they are produced (INT-2B a47): each producer's
/// value is classified like a direct engine call and must be `Effective`.
///
/// MUTATION GUARD: fill `Action::StartLLMTurn.compaction_config` in
/// `chat::run` from `config.agent.compaction` and this guard reports the chat
/// producer as `Base`, while the forwarded-site inventory stays green.
#[test]
pub(super) fn forwarded_compaction_values_are_produced_from_the_effective_window() {
    // The checked owner moved behind an entry wrapper. Pin that edge so an
    // empty public entry or a dropped input/authority cannot evade the owner.
    let entry = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/chat/run/entry.rs"),
    )));
    let ordinary = compact_rust_tokens(&function_body(&entry, "run"));
    assert_eq!(
        braced_block(&ordinary, ordinary.find('{').expect("chat entry body")),
        Some("{super::run_internal(config,provider,model,temperature,plain,session,list,shutdown,None,).await}"),
        "ordinary chat must forward every input to the budgeted private owner"
    );
    let controlled = compact_rust_tokens(&function_body(&entry, "run_controlled"));
    assert_eq!(controlled.matches("super::run_internal(").count(), 1);
    assert!(controlled.ends_with("letresult=super::run_internal(config,provider,model,temperature,plain,session,false,shutdown,Some(producer),).await;letcleanup=profile.close_reader().await;cleanup?;result}"),
        "controlled chat must forward its verified producer and retain reader cleanup");
    let owner = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/chat/run.rs"),
    )));
    assert_eq!(
        owner
            .lines()
            .find(|line| line.contains("async fn run_internal("))
            .map(str::trim),
        Some("async fn run_internal("),
        "chat owner must remain private"
    );
    assert_eq!(
        compact_rust_tokens(&function_body(&owner, "run_internal"))
            .matches("entry::build_context(config,list_sessions,terminal_producer).await?")
            .count(),
        1,
        "the private owner must pass its config, mode and producer to bootstrap"
    );
    let mut classified = Vec::new();
    for (relative, function, anchor, field) in FORWARDED_COMPACTION_PRODUCERS {
        let source = read_source(&repository_root().join(relative));
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        // Exactly one such function and exactly one producer in it (B4f): a
        // second producer, or a second function of the same name, fails by
        // name instead of hiding behind the first match.
        let signature = format!("fn {function}(");
        assert_eq!(
            production.matches(&signature).count(),
            1,
            "{relative}: expected exactly one production `{signature}`"
        );
        let start = production
            .find(&signature)
            .unwrap_or_else(|| panic!("{relative}: no production fn {function}"));
        let body_len = function_body(&production, function).len();
        let body = &production[start..(start + body_len).min(production.len())];
        assert_eq!(
            body.matches(*anchor).count(),
            1,
            "{relative}::{function} must contain `{anchor}` exactly once; list every producer"
        );
        let found = body
            .find(anchor)
            .unwrap_or_else(|| panic!("{relative}::{function} no longer contains `{anchor}`; re-locate the producer"));
        let at = start + found;
        let expression = field.map_or_else(
            || anchor.trim_end_matches(',').to_string(),
            |field| {
                request_field(&production[at..], field)
                    .unwrap_or_else(|| panic!("{relative}::{function}: `{anchor}` has no `{field}` field"))
            },
        );
        let source_kind = classify_compaction_argument(&production, &expression, at);
        classified.push(format!("{relative}::{function}::{anchor} -> {source_kind:?}"));
    }
    // Decision-owned windows have two concrete producers: initial admission
    // and the purpose-granted Router readmission. Neither may substitute base
    // configuration for the resolved model's window or omit its reservation.
    for (relative, function, resolver) in [
        (
            "src/runtime/turn_decision.rs",
            "agent_budget_snapshot",
            compact_rust_tokens(
                "let effective = self.effective_compaction(proof.provider_name(), proof.model_name());",
            ),
        ),
        (
            "src/runtime/turn_decision/routing.rs",
            "router_history_hop_inner",
            compact_rust_tokens(
                "let effective = crate::router::context::resolve_effective_compaction_config_with_catalog(\
                    &config.agent.compaction, admitted.provider_name(), admitted.model_name(), &config.router, \
                    &config.model_routes, &self.provider_catalog,);",
            ),
        ),
    ] {
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
            &repository_root().join(relative),
        )));
        let body = compact_rust_tokens(&function_body(&production, function));
        assert_eq!(
            body.matches(&resolver).count(),
            1,
            "{relative}: resolved producer changed"
        );
        assert_eq!(
            body.matches("letreservation=DecisionTokenReservation::from_effective_compaction(&effective.config,extra_token_ratio_limit)?;")
                .count(),
            1
        );
        assert_eq!(
            body.matches(
                "letmain_compaction=reservation.reserved_compaction(&effective.config,extra_token_ratio_limit)?;"
            )
            .count(),
            1
        );
    }
    for (relative, function, expected) in [
        (
            "src/runtime/turn_decision.rs",
            "main_compaction",
            "&self.main_compaction",
        ),
        (
            "src/channels/decision_turn.rs",
            "compaction",
            "self.context.main_compaction()",
        ),
        (
            "src/runtime/child_turn.rs",
            "compaction",
            "self.decision.main_compaction()",
        ),
    ] {
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
            &repository_root().join(relative),
        )));
        let body = compact_rust_tokens(&function_body(&production, function));
        assert_eq!(
            braced_block(&body, body.find('{').expect("accessor body")),
            Some(format!("{{{expected}}}").as_str()),
            "{relative}: decision compaction accessor changed"
        );
    }
    let expected: Vec<String> = FORWARDED_COMPACTION_PRODUCERS
        .iter()
        .map(|(relative, function, anchor, _)| format!("{relative}::{function}::{anchor} -> Effective"))
        .collect();
    assert_eq!(
        classified, expected,
        "a forwarded compaction value is no longer produced from the effective window"
    );
}

/// Every production turn budgets on the served model's effective window
/// (OQ-23 / R5).
///
/// `resolve_effective_compaction_config` applies the `[[router.models]]` or
/// built-in `max_context` and `reserved_output_tokens` of the model a turn
/// actually serves. A turn that hands the loop the raw `[agent.compaction]`
/// table instead budgets on the 128,000-token default whatever the model's
/// window, so two surfaces of one process trim the same request differently.
/// Console sessions and IM channels shipped that way while the CLI, TUI and
/// webhook resolved the window.
///
/// This is a textual guard. A call is `Base` when its compaction argument, or
/// a `let` / same-file `fn` it names, spells the raw table before any
/// resolver; `Base` is never allowed. A call that shows neither must be listed
/// in `FORWARDED_COMPACTION_SITES` with the upstream owner that resolved it.
/// `nearest_let_initializer` searches the file, not the function scope, so a
/// shadowed or out-of-scope binding can mislead it; the inventories below make
/// every change to the classification visible.
///
/// MUTATION GUARD: pass `Some(&config_snapshot.agent.compaction)` from the
/// Console turn, or `Some(&message_runtime.agent_compaction)` from the IM
/// turn, and the base inventory gains that function.
#[test]
pub(super) fn production_turns_budget_on_the_effective_context_window() {
    let mut effective = Vec::new();
    let mut base = Vec::new();
    let mut forwarded = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        for (call_start, request) in engine_turn_requests(&production) {
            let compaction = request_field(&request, "compaction")
                .unwrap_or_else(|| panic!("{relative}: a `run_turn` request without a `compaction` field"));
            let argument = &compaction;
            if compaction_argument_is_none(&production, argument, call_start) {
                // `production_turns_pass_a_context_budget_to_the_loop_engine`
                // owns this case.
                continue;
            }
            let line_index = production[..call_start].matches('\n').count();
            let site = format!("{relative}::{}", enclosing_function(&lines, line_index));
            match classify_compaction_argument(&production, argument, call_start) {
                CompactionSource::Effective => effective.push(site),
                CompactionSource::Base => base.push(site),
                CompactionSource::Forwarded => {
                    let expected = match site.as_str() {
                        "src/agent/embedded_turn.rs::run_work" => "Some(decision.main_compaction())",
                        "src/agent/entry.rs::run_with_runtime_envelope" => "Some(decision_context.main_compaction())",
                        "src/channels/turn.rs::run_channel_turn" => "Some(decision_turn.compaction())",
                        "src/chat/dispatcher.rs::drive_start_turn_stream" => "compaction_config.as_ref()",
                        "src/session_worker/runner/run.rs::run_owned_manifest" => {
                            "child_turn.as_ref().map(|turn|turn.compaction()).or(compaction_config_ref)"
                        }
                        "src/tools/delegate.rs::execute_tool_loop"
                        | "src/tools/sessions_spawn/task_mode.rs::run_sub_agent_task" => {
                            "Some(child_turn.compaction())"
                        }
                        _ => panic!("unreviewed forwarded compaction site: {site}"),
                    };
                    assert_eq!(
                        compact_rust_tokens(argument),
                        expected,
                        "{site}: forwarded compaction provenance changed"
                    );
                    forwarded.push(site);
                }
            }
        }
    }

    assert_inventory("production loop engine calls budgeted on the base window", base, &[]);
    assert_inventory(
        "production loop engine calls whose compaction config is forwarded",
        forwarded,
        FORWARDED_COMPACTION_SITES,
    );
    assert_inventory(
        "production loop engine calls budgeted on the effective window",
        effective,
        &[
            "src/chat/run.rs::run_internal",
            "src/gateway/api/sessions.rs::run_console_runtime_turn",
            "src/gateway/mod.rs::run_gateway_chat_with_multimodal",
        ],
    );
}

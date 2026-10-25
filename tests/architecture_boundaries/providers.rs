use super::helpers::{
    blank_comments_and_literals, enclosing_function, function_body, production_part, read_source, relative_path,
    repository_root, rust_source_files, strip_cfg_test_items,
};
use super::loop_requests::{compact_rust_tokens, is_identifier_byte, top_level_arguments};
use std::collections::BTreeSet;
use std::fs;
/// Every provider-factory call in already-stripped production source, as the
/// call's byte offset, the factory name and its top-level argument expressions.
///
/// A factory is the low-level `create_provider` family or one of the typed
/// `create_provider_for` / `create_routed_provider_for` entrypoints, called as
/// a free function (a method call such as `.build_provider(` is not a factory).
fn provider_factory_calls(production: &str) -> Vec<(usize, String, Vec<String>)> {
    let bytes = production.as_bytes();
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(found) = production[search..].find("create_") {
        let start = search + found;
        search = start + "create_".len();
        if start > 0 && (is_identifier_byte(bytes[start - 1]) || bytes[start - 1] == b'.') {
            continue;
        }
        let mut end = start;
        while end < bytes.len() && is_identifier_byte(bytes[end]) {
            end += 1;
        }
        let name = &production[start..end];
        let is_factory = name == "create_provider"
            || name == "create_provider_for"
            || name == "create_routed_provider_for"
            || name
                .strip_prefix("create_provider_with")
                .is_some_and(|rest| rest.starts_with('_'));
        if !is_factory || production[..start].trim_end().ends_with("fn") {
            continue;
        }
        let mut cursor = end;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            continue;
        }
        if let Some(arguments) = top_level_arguments(production, cursor) {
            calls.push((start, name.to_string(), arguments));
        }
    }
    calls
}

/// P0-B. The root credential, base URL and rotation keys belong to the
/// configured default provider. In `src/agent/**` they reach a provider factory
/// only through `agent_provider_selection`, which scopes the selection
/// `resolve_agent_model_choice` resolved and releases them on a
/// provider-identity match: no factory call there may name
/// the root `api_key`, `api_url` or `reliability` directly, and the only
/// factory call is the one inside `AgentProviderSelection::build_provider`.
#[test]
pub(super) fn agent_entries_reach_provider_factories_only_through_the_scoped_selection() {
    let mut sites = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/agent/") {
            continue;
        }
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        for (start, name, arguments) in provider_factory_calls(&production) {
            let line_index = production[..start].matches('\n').count();
            let function = enclosing_function(&lines, line_index);
            for argument in &arguments {
                let normalized: String = argument.split_whitespace().collect();
                for root in ["config.api_key", "config.api_url", "config.reliability"] {
                    assert!(
                        !normalized.contains(root),
                        "{relative} (fn {function}): `{name}` receives `{root}` directly; route the selection \
                         through `agent_provider_selection` so it is scoped to the provider it was configured for"
                    );
                }
            }
            sites.push(format!("{relative}:{function}:{name}"));
        }
    }
    assert_eq!(
        sites,
        vec!["src/agent/entry.rs:build_provider:create_routed_provider_for".to_string()],
        "agent entries must build their provider through `AgentProviderSelection::build_provider`"
    );
}

/// Whether a `let` line destructures a config struct: the left-hand side
/// (the pattern before the first `=`) names `…Config {` or one of `aliases`
/// with `{` (INT-2B a34). A struct *literal* on the right-hand side
/// (`let r = ReliabilityConfig { .. };`) is not a destructuring.
///
/// Known gaps (none occurs under `src/session_worker/` today; the guard's
/// `.api_key` / `.api_url` receiver check still catches a field read through
/// any of them):
/// - a pattern split over several lines is seen only on its first line;
/// - a tuple-struct or nested pattern of another type holding a config field;
/// - patterns outside a `let`: `if let` / `while let` / `let … else`
///   patterns are covered only when the line starts with `let`, and `match`
///   arms, closure parameters and `fn` parameters
///   (`fn f(Config { api_key, .. }: Config)`) are not looked at;
/// - a `type Cfg = Config;` alias (only `use … as` aliases are collected);
/// - `Self { .. }` inside an `impl …Config` block.
fn destructures_config(line: &str, aliases: &[String]) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("let ") else {
        return false;
    };
    let pattern: String = rest
        .split_once('=')
        .map_or(rest, |(pattern, _)| pattern)
        .split_whitespace()
        .collect();
    pattern.contains("Config{") || aliases.iter().any(|alias| pattern.contains(&format!("{alias}{{")))
}

/// The local names a config type is imported under (`use …::Config as Cfg;`,
/// also inside a `{ … }` group), so a destructuring through the alias is seen.
fn config_type_aliases(production: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    for statement in production.split(';') {
        let tokens: Vec<&str> = statement
            .split(|character: char| character.is_whitespace() || matches!(character, ',' | '{' | '}'))
            .filter(|token| !token.is_empty())
            .collect();
        let Some(use_at) = tokens.iter().position(|token| *token == "use") else {
            continue;
        };
        for window in tokens[use_at..].windows(3) {
            if window[0]
                .rsplit("::")
                .next()
                .is_some_and(|name| name.ends_with("Config"))
                && window[1] == "as"
            {
                aliases.push(window[2].to_string());
            }
        }
    }
    aliases
}

/// INT-2B a34, RED before (the guard matched any `let` line holding
/// `Config{` and `=`): only a left-hand-side pattern counts, through an
/// imported alias too.
#[test]
fn config_destructuring_is_recognised_on_the_left_hand_side_only() {
    let aliases = config_type_aliases(
        "use crate::config::{DelegateAgentConfig as Delegate, Other};\nuse crate::config::Config as Cfg;\n",
    );
    assert_eq!(aliases, vec!["Delegate".to_string(), "Cfg".to_string()]);
    for destructuring in [
        "let Config { api_key, .. } = config;",
        "let ReliabilityConfig { api_keys, .. } = config.reliability.clone();",
        "let mut Config { api_url, .. } = config;",
        "let Cfg { api_key, .. } = config;",
        "let Delegate { api_key, .. } = agent;",
    ] {
        assert!(destructures_config(destructuring, &aliases), "{destructuring}");
    }
    for literal in [
        "let r = ReliabilityConfig { ..ReliabilityConfig::default() };",
        "let config = Config { api_key: None, ..Config::default() };",
        "letters.push(Config { x: 1 });",
        "let d = Delegate::default();",
    ] {
        assert!(!destructures_config(literal, &aliases), "{literal}");
    }
}

/// C0-C, the process-mode two-hop. A session worker may run on a provider the
/// parent took from a model's per-call `provider` argument. Every credential it
/// hands out is decided once, in `worker_credentials`, which pairs each with the
/// provider it was issued for: no other production code under
/// `src/session_worker/**` may read the manifest's agent key or the root
/// `api_key`/`api_url`, and the worker's provider chain is built only by
/// `worker_provider`.
#[test]
pub(super) fn session_worker_credentials_are_decided_in_one_scoped_place() {
    let mut factory_sites = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/session_worker/") {
            continue;
        }
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        if relative == "src/session_worker/runner/ordinary_source.rs" {
            let restored = compact_rust_tokens(&function_body(&production, "restore"));
            assert_eq!(
                restored
                    .matches("letcredentials=super::worker_credentials(manifest,&generation.effective);")
                    .count(),
                1
            );
            assert_eq!(
                restored
                    .matches("resolved.credential().api_key()==credentials.sealed_key")
                    .count(),
                1
            );
        }
        let config_aliases = config_type_aliases(&production);
        // Names the root config is rebound to (`let cfg = &config;`,
        // `let mut worker_config = config.clone();`, ...).
        let config_rebindings: Vec<String> = lines
            .iter()
            .filter_map(|line| {
                let normalized: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
                let rest = normalized.strip_prefix("let ")?;
                let rest = rest.strip_prefix("mut ").unwrap_or(rest);
                let (name, value) = rest.split_once('=')?;
                let name = name.split(':').next()?.trim();
                let value: String = value.split_whitespace().collect();
                let is_root = [
                    "&config;",
                    "config;",
                    "&*config;",
                    "config.clone();",
                    "config.as_ref();",
                ]
                .contains(&value.as_str());
                (is_root
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_'))
                .then(|| name.to_string())
            })
            .collect();
        for (index, line) in lines.iter().enumerate() {
            let normalized: String = line.split_whitespace().collect();
            let function = enclosing_function(&lines, index);
            if function == "worker_credentials" {
                continue;
            }
            for credential in [
                "manifest.api_key",
                "config.api_key",
                "config.api_url",
                "config.reliability",
            ] {
                assert!(
                    !normalized.contains(credential),
                    "{relative} (fn {function}) reads `{credential}` directly; take it from \
                     `worker_credentials`, which scopes it to the provider it was issued for"
                );
            }
            // R-C0 F-2: a rebinding of the root config reads the same fields
            // under another name, so its `reliability` is held to the same rule
            // (its `api_key`/`api_url` fall under the receiver check below).
            for name in &config_rebindings {
                let token = format!("{name}.reliability");
                assert!(
                    !normalized.contains(&token),
                    "{relative} (fn {function}) reads `{token}` from a rebinding of the root config; take \
                     it from `worker_credentials`"
                );
            }
            assert!(
                !destructures_config(line, &config_aliases),
                "{relative} (fn {function}) destructures a config (`{}`); read credentials only through \
                 `worker_credentials`",
                line.trim()
            );
            // Any other `.api_key` / `.api_url` read must be of the scoped
            // credentials, or of a non-provider secret (Composio).
            for field in [".api_key", ".api_url"] {
                let mut search = 0usize;
                while let Some(found) = normalized[search..].find(field) {
                    let at = search + found;
                    search = at + field.len();
                    let receiver: String = normalized[..at]
                        .chars()
                        .rev()
                        .take_while(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let owner_checked_selection = match function.as_str() {
                        "validate_worker_route" => ["selected.credential()", "resolved.credential()"],
                        "build_worker_provider_selection" => ["resolved.credential()", "sealed_connection"],
                        _ => ["", ""],
                    }
                    .iter()
                    .any(|source| !source.is_empty() && normalized[..at].ends_with(source));
                    let owner_checked_agent = matches!(function.as_str(), "validate_worker_route" | "worker_provider")
                        && receiver.ends_with("agent")
                        && field == ".api_key";
                    assert!(
                        matches!(receiver.as_str(), "credentials.provider" | "config.composio")
                            || owner_checked_agent
                            || owner_checked_selection
                            || (relative == "src/session_worker/runner/ordinary_source.rs"
                                && function == "restore"
                                && field == ".api_key"
                                && normalized == "&&resolved.credential().api_key()==credentials.sealed_key"),
                        "{relative} (fn {function}) reads `{receiver}{field}`; outside `worker_credentials` only \
                         the scoped `credentials.provider` may be read"
                    );
                }
            }
        }
        for (start, name, _arguments) in provider_factory_calls(&production) {
            let line_index = production[..start].matches('\n').count();
            factory_sites.push(format!("{relative}:{}:{name}", enclosing_function(&lines, line_index)));
        }
    }
    assert_eq!(
        factory_sites,
        vec!["src/session_worker/runner/provider.rs:build_worker_provider_selection:create_provider_for".to_string()],
        "a session worker builds its provider only through `worker_provider`"
    );
}

/// The factory stems a provider chain is built through.
const PROVIDER_FACTORY_STEMS: [&str; 2] = ["create_provider", "create_routed_provider_for"];

/// Concrete provider and chain constructors. Outside `src/providers/**` a
/// provider is built only through the factories, which apply the credential
/// gate; a constructor named directly would bypass every credential scope.
const PROVIDER_CONSTRUCTOR_TYPES: [&str; 11] = [
    "ReliableProvider",
    "RouterProvider",
    "OpenAiCompatibleProvider",
    "AnthropicProvider",
    "OpenAiProvider",
    "OpenRouterProvider",
    "OllamaProvider",
    "GeminiProvider",
    "BedrockProvider",
    "CopilotProvider",
    "OpenAiCodexProvider",
];

/// R-C0 F-2. The guards above match factory calls by name inside one
/// directory. Three shapes would slip past them, and this guard closes each
/// as far as a textual check can:
///
/// - an import alias (`use …::create_provider_for as build;`)
///   anywhere in production code;
/// - a concrete provider or chain constructor (`ReliableProvider::new`,
///   `OpenAiCompatibleProvider::new_optional_credential`, …) outside
///   `src/providers/**`;
/// - a helper outside `src/agent/**` or `src/session_worker/**` that builds a
///   provider for them: every production factory call outside
///   `src/providers/**` is pinned by file and function, so a new call site
///   anywhere fails here and must be reviewed for credential scoping.
#[test]
pub(super) fn provider_construction_outside_the_provider_module_is_pinned() {
    let mut factory_sites = BTreeSet::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));

        for statement in production.split(';') {
            let statement = statement.trim();
            let is_use = statement.starts_with("use ")
                || statement.starts_with("pub use ")
                || statement.starts_with("pub(crate) use ");
            if !is_use {
                continue;
            }
            let normalized: String = statement.split_whitespace().collect::<Vec<_>>().join(" ");
            for stem in PROVIDER_FACTORY_STEMS {
                let mut search = 0usize;
                while let Some(found) = normalized[search..].find(stem) {
                    let start = search + found;
                    let rest = &normalized[start..];
                    let name_end = rest
                        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                        .unwrap_or(rest.len());
                    assert!(
                        !rest[name_end..].trim_start().starts_with("as "),
                        "{relative}: `{}` is imported under an alias; call a provider factory by its own name \
                         so the credential guards can see it",
                        &rest[..name_end]
                    );
                    search = start + stem.len();
                }
            }
        }

        if relative.starts_with("src/providers/") {
            continue;
        }
        for type_name in PROVIDER_CONSTRUCTOR_TYPES {
            let needle = format!("{type_name}::");
            let mut search = 0usize;
            while let Some(found) = production[search..].find(&needle) {
                let start = search + found;
                search = start + needle.len();
                if start > 0 && is_identifier_byte(production.as_bytes()[start - 1]) {
                    continue;
                }
                let after = &production[search..];
                let method: String = after
                    .chars()
                    .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                    .collect();
                let is_constructor = method.starts_with("new") || method.starts_with("with_");
                let is_call = after[method.len()..].trim_start().starts_with('(');
                assert!(
                    !(is_constructor && is_call),
                    "{relative}: `{type_name}::{method}` builds a provider outside `src/providers/**`; go through \
                     a provider factory, which applies the credential gate"
                );
            }
        }

        let lines: Vec<&str> = production.lines().collect();
        for (start, name, _arguments) in provider_factory_calls(&production) {
            let line_index = production[..start].matches('\n').count();
            factory_sites.insert(format!("{relative}:{}:{name}", enclosing_function(&lines, line_index)));
        }
    }

    // Each site's credential source, as reviewed in C0 / R-C0:
    let expected: BTreeSet<String> = [
        "src/agent/entry.rs:build_provider:create_routed_provider_for",
        "src/channels/decision_turn.rs:build_provider:create_provider_for",
        "src/channels/runtime_commands.rs:build_channel_hint_provider:create_provider_for",
        "src/channels/runtime_commands.rs:build_channel_startup_provider:create_routed_provider_for",
        "src/chat/bootstrap.rs:chat_provider_chain:create_routed_provider_for",
        "src/doctor/mod.rs:provider_validation_error:create_provider_for",
        "src/gateway/mod.rs:build_gateway_provider_selection:create_routed_provider_for",
        "src/runtime/child_source.rs:from_selection:create_provider_for",
        // Proposal and judge calls share the resolved, credential-scoped factory.
        "src/runtime/evolution_model.rs:from_config:create_provider_for",
        "src/runtime/turn_decision/auxiliary.rs:auxiliary_accounting:create_provider_for",
        "src/runtime/turn_decision/auxiliary.rs:smart_group_accounting:create_provider_for",
        "src/session_worker/runner/provider.rs:build_worker_provider_selection:create_provider_for",
    ]
    .iter()
    .map(|site| (*site).to_string())
    .collect();
    assert_eq!(
        factory_sites, expected,
        "the set of provider-factory call sites outside `src/providers/**` changed; a new site must pass only \
         a credential scoped to the provider it builds (see `configured_connection_for`), then be added here"
    );
}

/// Resolver entry points a model selection can go through: the selection
/// helpers, the catalog resolver itself, and the label-level validator.
const RESOLVER_NEEDLES: &[&str] = &[
    "selection::check",
    "catalog::resolve",
    "resolver::resolve",
    "validate_provider_model",
];

/// The `use` declarations that import resolver items under any name: an
/// aliased import would otherwise hide a call from a substring check. Each
/// declaration starts on a line of its own and runs to its `;`, so a
/// multi-line `use crate::providers::{…}` block counts once.
fn resolver_imports(production: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut offset = 0;
    for line in production.split_inclusive('\n') {
        let body = line.trim_start();
        let declaration = body
            .strip_prefix("pub(crate) ")
            .or_else(|| body.strip_prefix("pub "))
            .unwrap_or(body);
        if declaration.starts_with("use ") {
            let start = offset + (line.len() - body.len());
            let end = production[start..].find(';').map_or(production.len(), |at| start + at);
            let statement = &production[start..end];
            if statement.contains("providers")
                && (statement.contains("selection")
                    || statement.contains("catalog")
                    || statement.contains("resolver")
                    || statement.contains("validate_provider_model"))
            {
                imports.push(statement.to_string());
            }
        }
        offset += line.len();
    }
    imports
}

/// PMM-C-L / D-065: the agent loop's entries (`prx agent`, cron and xin agent
/// jobs, `prx go --message`) resolve their provider and model exactly once,
/// in `agent::entry::resolve_agent_model_choice`, and before the runtime
/// bootstrap. The only later re-resolution is the explicit credential-source
/// attestation immediately before construction. Each configured route is then
/// resolved once, from its own credential, inside the typed routed factory;
/// callers do not resolve either selection again under any name.
#[test]
pub(super) fn agent_loop_entries_resolve_the_model_once() {
    let entry = production_part(&read_source(&repository_root().join("src/agent/entry.rs")));
    let resolution = function_body(&entry, "resolve_agent_model_choice");
    assert_eq!(
        resolution.matches("providers::selection::check_with(").count(),
        1,
        "resolve_agent_model_choice resolves the selection once"
    );
    let providers = production_part(&read_source(&repository_root().join("src/providers/mod.rs")));
    let routed_factory = function_body(&providers, "create_routed_provider_for");
    assert_eq!(
        routed_factory.matches("selection::resolve_route(").count(),
        1,
        "create_routed_provider_for resolves every configured route through the typed resolver"
    );
    let attestation = function_body(&entry, "factory_credential_source_matches");
    assert_eq!(
        attestation.matches("providers::selection::check_with(").count(),
        1,
        "the construction-time credential-source attestation re-resolves exactly once"
    );
    let outside = entry.replacen(&resolution, "", 1).replacen(&attestation, "", 1);
    for needle in RESOLVER_NEEDLES {
        assert!(
            !outside.contains(needle),
            "the agent entry resolves outside resolve_agent_model_choice: {needle}"
        );
    }
    assert!(
        resolver_imports(&outside).is_empty(),
        "the agent entry imports resolver items: {:?}",
        resolver_imports(&outside)
    );
    let run = function_body(&entry, "run_with_runtime_envelope");
    let resolved_at = run
        .find("resolve_agent_model_choice(")
        .expect("run_with_runtime_envelope resolves the selection");
    let bootstrap_at = run
        .find("RuntimeBootstrap::build_agent(")
        .expect("run_with_runtime_envelope builds the runtime");
    assert!(
        resolved_at < bootstrap_at,
        "the selection is resolved before the runtime bootstrap"
    );
    for relative in ["src/cron/scheduler.rs", "src/xin/runner.rs", "src/main.rs"] {
        let production = production_part(&read_source(&repository_root().join(relative)));
        for needle in RESOLVER_NEEDLES
            .iter()
            .chain(&["resolve_agent_model_choice(", "agent_provider_selection("])
        {
            assert!(
                !production.contains(needle),
                "{relative} resolves a model the agent entry (or chat start-up) resolves: {needle}"
            );
        }
        assert!(
            resolver_imports(&production).is_empty(),
            "{relative} imports resolver items: {:?}",
            resolver_imports(&production)
        );
    }
}

/// PMM-C-L: tool-argument validation takes its dialect from the provider
/// object for the model and member that serve (`tool_schema_dialect_for`,
/// `tool_schema_dialect_of_member`). No label-keyed dialect map, and no
/// `widest`/`across_chain` fold across a chain, remains in production code.
#[test]
pub(super) fn no_label_keyed_or_folded_tool_schema_dialect_remains() {
    let mut hits = Vec::new();
    for path in rust_source_files() {
        let production = production_part(&read_source(&path));
        // `::for_provider(` also catches a call through an aliased import
        // (`use crate::tools::ToolSchemaDialect as D; D::for_provider(…)`).
        for needle in [
            "::for_provider(",
            "fn for_provider(",
            "fn widest(",
            "::widest(",
            ".widest(",
            "across_chain(",
        ] {
            if production.contains(needle) {
                hits.push(format!("{}: {needle}", relative_path(&path)));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "tool schema dialects come from the provider object for the serving model and member: {hits:?}"
    );
}

/// Every provider source file is compiled (INT-2B B3f, D-068 (1)).
///
/// `src/providers/glm.rs` sat next to the providers for months without a
/// `mod glm;`: it was never compiled, so its error paths (raw upstream bodies
/// in `bail!`) and its tests looked covered by the provider review while
/// nothing ran. Each `src/providers/*.rs` must be declared as a module of
/// `src/providers/mod.rs`.
#[test]
fn every_provider_source_file_is_in_the_module_tree() {
    let root = repository_root();
    let providers = root.join("src/providers");
    let module = fs::read_to_string(providers.join("mod.rs")).expect("read src/providers/mod.rs");
    let mut orphans = Vec::new();
    for entry in fs::read_dir(&providers).expect("list src/providers") {
        let path = entry.expect("read a src/providers entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem == "mod" {
            continue;
        }
        let declared = module.lines().any(|line| {
            let line = line.trim_start();
            let line = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub(super) "))
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            line == format!("mod {stem};")
        });
        if !declared {
            orphans.push(stem.to_string());
        }
    }
    orphans.sort();
    assert!(
        orphans.is_empty(),
        "src/providers files outside the module tree (never compiled): {orphans:?}"
    );
}

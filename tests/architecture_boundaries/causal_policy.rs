use super::causal_domain::crate_rooted_paths;
use super::helpers::{
    blank_comments_and_literals, is_ident_char, read_source, relative_path, rust_source_files, strip_cfg_test_items,
};
use super::loop_requests::{is_test_code_file, test_module_files};
use super::tool_scanner::contains_identifier;
/// The only production files that may write the decision-plane
/// configuration (CDP-04, SP-CDP-A 5.4): its own module, for the legacy
/// migration and the derived legacy view. CDP-12 adds the typed mutation
/// service (`src/config/manage/mutation/causal.rs`).
const CAUSAL_POLICY_WRITERS: &[&str] = &[
    "src/causal_tree/config.rs",
    // CDP-12: the one decision-plane mutation service (AG-13).
    "src/config/manage/mutation/causal.rs",
];

/// Methods that only read a value, and so may be called on a
/// `.causal_tree` chain outside [`CAUSAL_POLICY_WRITERS`].
const CAUSAL_READ_ONLY_METHODS: &[&str] = &[
    "clone",
    "as_str",
    "as_ref",
    "as_deref",
    "trim",
    "is_empty",
    "len",
    "iter",
    "get",
    "contains",
    "to_string",
    "to_owned",
    "is_some",
    "is_none",
    "map",
    "map_or",
    "copied",
    "cloned",
    "validate",
    "eq",
    "ne",
    "first",
    "last",
    "keys",
    "values",
    "unwrap_or",
    "unwrap_or_default",
];

/// The typed sections of `[causal_tree]`; a literal of one outside the
/// writers builds a policy nobody reviewed.
const CAUSAL_CONFIG_TYPES: [&str; 9] = [
    "CausalTreeConfig",
    "CausalPolicy",
    "DecisionBackendConfig",
    "CircuitBreakerConfig",
    "RehearsalConfig",
    "ScoringConfig",
    "WeightOverrides",
    "DecisionStoreConfig",
    "EvaluationConfig",
];

/// CDP-04 (SP-CDP-C V8.05 `runtime_never_rewrites_thresholds`, CDP 15 "no
/// auto-threshold rewrite"): no production code outside
/// [`CAUSAL_POLICY_WRITERS`] writes decision policy. It never
///
/// - assigns (`=`, or a compound `+=` and the like) to `.causal_tree` or to
///   any field under it;
/// - borrows `.causal_tree` or a field under it mutably (`&mut`, which
///   `std::mem::replace`, `swap` and `take` need);
/// - writes a literal of a [`CAUSAL_CONFIG_TYPES`] type;
/// - fills a `causal_tree:` field with anything but the section's default.
///
/// A whole `Config` still changes through the transaction layer (AG-13),
/// whose plan classifies a decision-plane change (`RiskReason`).
#[test]
pub(super) fn causal_policy_is_written_only_by_the_mutation_service() {
    let mut violations = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if CAUSAL_POLICY_WRITERS.contains(&relative.as_str()) {
            continue;
        }
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        // Whitespace-normalized, with a method or field chain split over
        // lines (`config\n    .causal_tree\n    .backend`) joined back.
        let compact = production
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" .", ".");
        let mut search = 0;
        while let Some(found) = compact[search..].find(".causal_tree") {
            let start = search + found;
            search = start + ".causal_tree".len();
            if compact[search..].chars().next().is_some_and(is_ident_char) {
                continue;
            }
            // The field chain under the section: `.policy.commit_threshold`,
            // including index segments (`.weights[0]`, D-082 N-10).
            let mut chain_end = search;
            loop {
                chain_end += compact[chain_end..]
                    .find(|character: char| !(is_ident_char(character) || character == '.'))
                    .unwrap_or(compact.len() - chain_end);
                if !compact[chain_end..].starts_with('[') {
                    break;
                }
                match compact[chain_end..].find(']') {
                    Some(close) => chain_end += close + 1,
                    None => break,
                }
            }
            let rest = compact[chain_end..].trim_start();
            // A turbofish method call (`.get_or_insert_with::<T>(..)`) is a
            // method call too.
            let rest = rest
                .strip_prefix("::<")
                .and_then(|generic| generic.find(">("))
                .map_or(rest, |close| &rest[3 + close + 1..]);
            let assigned = (rest.starts_with('=') && !rest.starts_with("==") && !rest.starts_with("=>"))
                || ["+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<=", ">>="]
                    .iter()
                    .any(|operator| rest.starts_with(operator));
            let path_start = compact[..start]
                .rfind(|character: char| !(is_ident_char(character) || matches!(character, '.' | '(' | ')')))
                .map_or(0, |index| index + 1);
            let borrowed = compact[..path_start].trim_end().ends_with("&mut");
            // A method called on the chain (`.causal_tree.normalize()`,
            // `.backend.endpoint.push_str(..)`) may take `&mut self`: only
            // read-only methods are allowed.
            let method = rest.starts_with('(')
                && compact[search..chain_end]
                    .rsplit('.')
                    .next()
                    .is_some_and(|name| !CAUSAL_READ_ONLY_METHODS.contains(&name));
            if assigned || borrowed || method {
                violations.push(format!(
                    "{relative}: writes .causal_tree{}",
                    &compact[search..chain_end]
                ));
            }
        }
        for name in CAUSAL_CONFIG_TYPES {
            let literal = format!("{name} {{");
            let mut search = 0;
            while let Some(found) = compact[search..].find(&literal) {
                let start = search + found;
                search = start + literal.len();
                if compact[..start].chars().next_back().is_some_and(is_ident_char) {
                    continue;
                }
                let before = compact[..start].trim_end();
                let before = before.strip_suffix("mut").map_or(before, str::trim_end);
                let before = before.strip_suffix('&').map_or(before, str::trim_end);
                let is_type_position = ["struct", "for", "impl", "->", "enum"]
                    .iter()
                    .any(|token| before.ends_with(token));
                if !is_type_position {
                    violations.push(format!("{relative}: {name} literal"));
                }
            }
        }
        // Field shorthand in a `Config` pattern or literal
        // (`let Config { causal_tree, .. } = &mut config;`,
        // `Config { causal_tree, ..x }`) binds or fills the section by name.
        let mut search = 0;
        while let Some(found) = compact[search..].find("causal_tree") {
            let start = search + found;
            search = start + "causal_tree".len();
            let preceding = compact[..start].chars().next_back();
            if preceding.is_some_and(|character| is_ident_char(character) || character == '.' || character == ':') {
                continue;
            }
            let following = compact[search..].trim_start();
            if following.starts_with(',') || following.starts_with('}') {
                violations.push(format!("{relative}: binds or fills causal_tree by shorthand"));
            }
        }
        let mut search = 0;
        while let Some(found) = compact[search..].find("causal_tree:") {
            let start = search + found;
            search = start + "causal_tree:".len();
            // Not a field value: a path (`crate::causal_tree::`), or the
            // field's declaration (`pub causal_tree: CausalTreeConfig`). An
            // attribute before a field value (`#[cfg(x)] causal_tree: v`) is
            // no declaration.
            let before = compact[..start].trim_end();
            let declared = ["pub", "pub(crate)", "pub(super)", "pub(self)"]
                .iter()
                .any(|visibility| before.ends_with(visibility));
            if compact[..start].chars().next_back().is_some_and(is_ident_char)
                || compact[search..].starts_with(':')
                || declared
            {
                continue;
            }
            let value = compact[search..].trim_start();
            let value = &value[..value.find([',', '}']).unwrap_or(value.len())];
            if !(value.trim_end().ends_with("CausalTreeConfig::default()") || value.trim() == "Default::default()") {
                violations.push(format!("{relative}: fills causal_tree with `{}`", value.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "decision policy is written only by the decision-plane configuration module (CDP-04): {violations:?}"
    );
}

/// The `crate::providers` items decision code may name (AG-03, D-079
/// L-10): the provider identity, the catalog's selection types, the
/// credential reference and the strict decision backend check. A chat
/// provider, its trait and its factories are none of them.
const DECISION_PROVIDER_PATHS: &[&str] = &[
    "providers::ProviderId",
    "providers::catalog::CatalogView",
    "providers::catalog::ResolutionError",
    "providers::catalog::ResolvedModel",
    "providers::credentials::CredentialRef",
    "providers::selection::check_decision_backend",
    // The configuration-only half of that check, for a disabled plane
    // (D-082 N-9).
    "providers::selection::check_decision_model",
    // The compatible HTTP backend (CDP-07): its released credential, the
    // endpoint and error-text redaction, and the retry-after parser.
    "providers::catalog::ResolvedCredential",
    "providers::redact_endpoint_userinfo",
    "providers::sanitize_api_error",
    "providers::traits::retry_after_ms_from_headers",
];

/// AG-03 (CDP-PLAN 1.5, SP-CDP-A 5.6; D-079 L-10): no file under
/// `src/decision/` uses a chat provider. Every path it names under
/// `crate::providers` is in [`DECISION_PROVIDER_PATHS`], through `use` trees,
/// aliases and qualified paths alike; a glob or the bare module is refused.
/// The `Provider` identifier, the chat provider factories and
/// `OpenRouterProvider` are also refused by name, and no decision
/// adapter names a chat completions route (CDP-13: the hosted adapters).
#[test]
pub(super) fn decision_code_never_uses_a_chat_provider() {
    const CHAT_PROVIDER_NAMES: [&str; 10] = [
        "Provider",
        "OpenRouterProvider",
        "create_provider",
        "create_provider_with_options",
        "create_provider_with_url",
        "create_provider_for",
        "create_resilient_provider",
        "create_resilient_provider_with_options",
        "create_routed_provider",
        "create_routed_provider_with_options",
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (_, named) in crate_rooted_paths(&production, relative == "src/decision/mod.rs") {
            if named != "providers" && !named.starts_with("providers::") {
                continue;
            }
            let allowed = DECISION_PROVIDER_PATHS
                .iter()
                .any(|entry| named == *entry || named.starts_with(&format!("{entry}::")));
            if !allowed {
                violations.push(format!("{relative}: crate::{named}"));
            }
        }
        for name in CHAT_PROVIDER_NAMES {
            if contains_identifier(&production, name) {
                violations.push(format!("{relative}: {name}"));
            }
        }
        // Routes are string literals: read the code lines, not the comments.
        let code = strip_cfg_test_items(&read_source(&path));
        if code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| line.contains("chat/completions") || line.contains("/v1/chat"))
        {
            violations.push(format!("{relative}: a chat completions route"));
        }
    }
    assert!(
        violations.is_empty(),
        "decision code never uses a chat provider (AG-03): {violations:?}"
    );
}

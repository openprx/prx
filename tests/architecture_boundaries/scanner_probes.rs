use super::causal_domain::{
    decision_answers_never_become_chat_messages, decision_domain_has_no_constant_timeout_or_approval_grant_constructor,
    mock_decision_backend_is_compiled_only_for_tests, the_causal_domain_imports_no_backend_and_no_network,
    the_decision_validator_never_repairs_a_value,
};
use super::causal_policy::{
    causal_policy_is_written_only_by_the_mutation_service, decision_code_never_uses_a_chat_provider,
};
use super::compaction::{
    CompactionSource, classify_compaction_argument, forwarded_compaction_values_are_produced_from_the_effective_window,
    production_turns_budget_on_the_effective_context_window,
    sub_agent_tools_resolve_compaction_from_the_whole_configuration,
};
use super::config::{
    config_writes_route_through_the_transaction_layer, the_home_directory_is_resolved_only_by_the_config_resolver,
};
use super::decision_backend::{
    decision_http_clients_come_only_from_the_pinned_builder, decision_metric_labels_come_from_bounded_allowlists,
    loopback_is_decided_only_by_the_shared_predicate,
    only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
    the_decision_store_database_is_opened_only_by_the_store,
};
use super::decision_seals::{
    decision_compiler_uses_typed_ids_and_only_redacted_state, only_reviewed_production_sites_seal_decision_requests,
    trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
    validation_requires_the_sealed_request_manifest,
};
use super::helpers::{PLANTED_SOURCES, repository_root};
use super::ingress::{
    embedded_user_input_cannot_bypass_reserved_marker_neutralization,
    ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates,
};
use super::labels::{
    provider_label_sinks_redact_url_userinfo, provider_labels_never_reach_a_log_field_raw,
    terminal_provider_label_guard_rejects_bypass_missing_calls_and_literal_decoys,
};
use super::loop_contracts::{
    chat_message_constructors_are_pinned, chat_route_decision_names_the_registry_root_count,
    context_handoff_has_one_production_owner_and_exact_lookup_is_registered,
    loop_note_names_are_never_aliased_or_variant_imported, middleware_history_rebuilds_neutralize_user_messages,
    the_loop_engine_has_one_typed_entry, user_text_enters_histories_only_neutralized,
};
use super::loop_prompt::{
    compiled_request_context_event_has_one_production_owner, outbound_middleware_has_one_production_owner,
    prompt_guided_tool_protocol_has_one_production_owner, runtime_prompt_has_no_parallel_tool_description_catalog,
};
use super::loop_requests::{
    every_loop_engine_call_receives_a_turn_decision_context, production_turns_pass_a_context_budget_to_the_loop_engine,
};
use super::memory::{
    every_memory_read_principal_built_from_a_context_carries_the_owner_scope,
    the_owner_isolation_bit_is_never_asserted_by_memory_production_code,
};
use super::process::{
    every_production_process_command_is_inventoried, git_is_named_only_in_the_reviewed_places,
    git_runs_only_through_the_hardened_launch, process_command_types_are_never_aliased,
};
use super::providers::{
    agent_entries_reach_provider_factories_only_through_the_scoped_selection,
    agent_loop_entries_resolve_the_model_once, no_label_keyed_or_folded_tool_schema_dialect_remains,
    provider_construction_outside_the_provider_module_is_pinned,
    session_worker_credentials_are_decided_in_one_scoped_place,
};
use super::tool_tables::both_gateway_launch_paths_register_their_tools_from_one_body;
use super::tool_tests::{
    direct_tool_invocation_allowlist_counts_match_the_tree,
    direct_tool_invocations_outside_the_execution_service_are_allowlisted,
    every_gateway_tool_execution_documents_its_alternative_gate,
    every_non_tool_execute_call_site_shows_its_type_evidence, non_tool_closure_providers_match_their_definitions,
    non_tool_execute_call_sites_match_their_declared_occurrences,
};
use std::fs;
/// Tree scanners that deliberately inventory test code as well, with the
/// reason. Every other function that walks the source tree must separate
/// production from tests through a marker-aware helper.
pub(super) const TEST_INCLUSIVE_TREE_SCANNERS: &[(&str, &str)] = &[
    (
        "runtime_and_message_events_preserve_typed_config_generation_columns",
        "it requires column names to be present in the whole PostgreSQL adapter directory, tests included; it prohibits nothing, so there is no violation to plant",
    ),
    (
        "brain_db_is_not_opened_outside_the_sqlite_repository",
        "its allowlist names test functions: a test that opens brain.db directly is a reviewed exception too",
    ),
    (
        "raw_child_process_spawns_are_explicitly_allowlisted",
        "its allowlist names test functions: a test that spawns a raw child is a reviewed exception too",
    ),
    (
        "persisted_event_tables_are_explicitly_allowlisted",
        "its allowlist includes a test fixture table (the pre-v16 memory_events)",
    ),
    (
        "config_generation_manager_is_the_only_runtime_config_publisher",
        "an independent config publisher is forbidden in test code as well",
    ),
    (
        "the_strip_memo_returns_the_exact_computation",
        "not a guard: it checks the stripper memo against the computation on every file, test code included",
    ),
    (
        "src_and_tests_carry_no_cjk_characters",
        "the English-only rule covers tests and fixtures",
    ),
    (
        "compaction_none_spellings_and_test_module_files_are_exact",
        "not a guard: it checks the test-module classifier against the real tree",
    ),
    (
        "the_agent_loop_module_stays_deleted",
        "it bans the old module path in test code too: a test that names the old path would keep an alias alive",
    ),
    (
        "every_provider_source_file_is_in_the_module_tree",
        "it checks that each provider file is declared, whatever the file holds: a test-only file outside the \
         module tree is never compiled either",
    ),
];

/// Where a guard-of-guards probe plants its violation.
#[derive(Clone, Copy)]
enum Planted {
    /// A new file at this path: plain, after brace-less and bracket-bearing
    /// test items, inside a test module, behind the file marker, behind a
    /// late marker.
    NewFile,
    /// Appended to this existing file (its real text first).
    AppendedTo,
}

/// Which text the scanner reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// Production text: a violation inside a test module is not one.
    ProductionText,
    /// The whole text of every file that is not test-only by its marker.
    WholeTextOfUnmarkedFiles,
}

/// Test items that precede a violation in the "after test code" scenario:
/// brace-less items and brackets inside literals and comments, each of which
/// once blinded a stripper to the production text that follows.
const PLANTED_TEST_PREFIX: &str = "#[cfg(test)]\nconst D0_PROBE_LIMIT: usize = 1;\n#[cfg(test)]\nmod d0_probe_tests;\n\
                                   #[cfg(test)]\nfn d0_probe_helper() {\n    let _ = \"(\";\n    let _ = '[';\n    \
                                   // (\n}\n#[cfg(test)]\nfn d0_probe_raw() {\n    let _ = br\"C:\\\";\n    let _ = \
                                   cr\"C:\\\";\n    let _ = ['\\'','('];\n}\n";

pub(super) fn scanner_fails_with(scanner: fn(), planted: Vec<(String, String)>) -> bool {
    // Nested negative probes temporarily replace, then restore, their caller's mutation.
    let previous = PLANTED_SOURCES.with_borrow_mut(|sources| std::mem::replace(sources, planted));
    let failed = std::panic::catch_unwind(scanner).is_err();
    PLANTED_SOURCES.with_borrow_mut(|sources| *sources = previous);
    failed
}

/// Plant `violation` for `scanner` in every placement and check the scanner
/// catches it exactly where it is production code.
fn run_planted_probe(name: &str, scanner: fn(), path: &str, planted: Planted, reads: Reads, violation: &str) {
    assert!(
        !scanner_fails_with(scanner, Vec::new()),
        "{name} fails on the tree itself"
    );
    let base = match planted {
        Planted::NewFile => String::new(),
        Planted::AppendedTo => {
            fs::read_to_string(repository_root().join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
                + "\n"
        }
    };
    let wrapped = format!("#[cfg(test)]\nmod d0_planted_tests {{\n{violation}}}\n");
    let mut scenarios = vec![
        ("plain", format!("{base}{violation}"), true),
        (
            "after test code",
            format!("{base}{PLANTED_TEST_PREFIX}{violation}"),
            true,
        ),
        (
            "inside a test module",
            format!("{base}{wrapped}"),
            reads == Reads::WholeTextOfUnmarkedFiles,
        ),
    ];
    if matches!(planted, Planted::NewFile) {
        scenarios.push((
            "file marker",
            format!("//! probe\n\n#![cfg(test)]\n\n{violation}"),
            false,
        ));
        scenarios.push((
            "late marker",
            format!("#![allow(unused)]\n#![cfg(test)]\n\n{violation}"),
            true,
        ));
    }
    for (scenario, text, must_fail) in scenarios {
        let failed = scanner_fails_with(scanner, vec![(path.to_string(), text)]);
        assert_eq!(
            failed,
            must_fail,
            "{name}: a violation planted in {path} ({scenario}) was {}",
            if failed {
                "reported, but it is test code"
            } else {
                "NOT reported"
            }
        );
    }
}

/// A `run_turn` call for the routing and budget probes: the request carries
/// the compaction config and the routing the probes read; the other fields are
/// placeholders (the guards read production text, never compile it).
macro_rules! planted_engine_call {
    ($compaction:literal, $routing:literal) => {
        concat!(
            "pub async fn d0_planted_turn() {\n    let _ = run_turn(\n        &mut history,\n        \
             TurnRequest {\n            channel_name: a0,\n            scope: a1,\n            compaction: ",
            $compaction,
            ",\n            routing: ",
            $routing,
            ",\n        },\n        services,\n        delivery,\n    )\n    .await;\n}\n"
        )
    };
}

/// One planted-violation test per production-only tree scanner.
macro_rules! planted_violation_probes {
    ($($probe:ident: $scanner:ident, $path:literal, $planted:expr, $reads:expr, $violation:expr;)*) => {
        /// The scanners the probes below cover.
        pub(super) const PROBED_TREE_SCANNERS: &[&str] = &[$(stringify!($scanner)),*];
        $(
            #[test]
            fn $probe() {
                run_planted_probe(stringify!($scanner), $scanner, $path, $planted, $reads, $violation);
            }
        )*
    };
}

planted_violation_probes! {
    probe_config_writes: config_writes_route_through_the_transaction_layer,
        "src/providers/mod.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub async fn d0_planted_save(cfg: Config) {\n    cfg.save().await;\n}\n";
    probe_prompt_protocol: prompt_guided_tool_protocol_has_one_production_owner,
        "src/chat/mod.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_protocol() -> &'static str {\n    \"## Tool Use Protocol\"\n}\n";
    probe_runtime_prompt_catalog: runtime_prompt_has_no_parallel_tool_description_catalog,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_catalog(tool_descs: &[u8]) -> usize {\n    tool_descs.len()\n}\n";
    probe_compiled_request_owner: compiled_request_context_event_has_one_production_owner,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub const D0_PLANTED_EVENT: &str = \"llm.request.context.compiled\";\n";
    probe_outbound_owner: outbound_middleware_has_one_production_owner,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_outbound() {\n    let _ = MiddlewareStage::Outbound;\n}\n";
    probe_decision_required: every_loop_engine_call_receives_a_turn_decision_context,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!(
            "a15",
            "TurnRouting::Routed { tiering: &config.tool_tiering, input: RoutingInput::LastUserMessage, unrouted: a2 }"
        );
    probe_decision_none: every_loop_engine_call_receives_a_turn_decision_context,
        "src/agent/entry.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub async fn d0_planted_turn() { run_turn(&mut history, TurnRequest { decision: None }, services, delivery).await; }";
    probe_decision_unreviewed_entry: every_loop_engine_call_receives_a_turn_decision_context,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_turn() { run_turn(&mut history, TurnRequest { decision: admitted_turn.routing() }, services, delivery).await; }";
    probe_budget_present: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("None", "TurnRouting::Unrouted");
    // INT-2B a48: every spelling of `None`, and a production file whose name
    // merely ends in `tests.rs`.
    probe_budget_option_none: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("Option::None", "TurnRouting::Unrouted");
    probe_budget_option_turbofish_none: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("Option::<&AgentCompactionConfig>::None", "TurnRouting::Unrouted");
    probe_budget_turbofish_none: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("None::<&AgentCompactionConfig>", "TurnRouting::Unrouted");
    probe_budget_let_none: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        concat!("pub async fn d0_planted_budget() {\n    let budget = None;\n}\n", planted_engine_call!("budget", "TurnRouting::Unrouted"));
    probe_budget_contests_file: production_turns_pass_a_context_budget_to_the_loop_engine,
        "src/agent/contests.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("None", "TurnRouting::Unrouted");
    probe_one_engine_entry: the_loop_engine_has_one_typed_entry,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn run_tool_call_loop_outcome() {}\n";
    probe_agent_resolve_once: agent_loop_entries_resolve_the_model_once,
        "src/xin/runner.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_recheck(config: &Config) {\n    let _ = crate::providers::selection::check_default(config, a0);\n}\n";
    probe_no_label_dialect: no_label_keyed_or_folded_tool_schema_dialect_remains,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_dialect(label: &str) -> ToolSchemaDialect {\n    ToolSchemaDialect::for_provider(label)\n}\n";
    probe_agent_resolve_once_alias: agent_loop_entries_resolve_the_model_once,
        "src/xin/runner.rs", Planted::AppendedTo, Reads::ProductionText,
        "use crate::providers::{selection as picked};\npub fn d0_planted_alias(config: &Config) {\n    let _ = picked::default_model(config);\n}\n";
    probe_agent_resolve_once_direct: agent_loop_entries_resolve_the_model_once,
        "src/cron/scheduler.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_direct(a0: &str, a1: &str) {\n    let _ = crate::providers::validate_provider_model(a0, a1);\n}\n";
    probe_no_label_dialect_alias: no_label_keyed_or_folded_tool_schema_dialect_remains,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::tools::ToolSchemaDialect as D;\npub fn d0_planted_alias(label: &str) -> D {\n    D::for_provider(label)\n}\n";
    probe_user_ingress: user_text_enters_histories_only_neutralized,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_ingress(text: &str) -> ChatMessage {\n    ChatMessage::user(text)\n}\n";
    probe_user_ingress_literal: user_text_enters_histories_only_neutralized,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_literal(content: String) -> ChatMessage {\n    ChatMessage { role: \"user\".to_string(), content }\n}\n";
    probe_loop_note_aliases: loop_note_names_are_never_aliased_or_variant_imported,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::agent::turn_request::RoutingInput as RI;\npub fn d0_planted_alias() {}\n";
    probe_loop_note_type_alias: loop_note_names_are_never_aliased_or_variant_imported,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub type D0Msg = crate::providers::ChatMessage;\n";
    probe_loop_note_self_variant: loop_note_names_are_never_aliased_or_variant_imported,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "impl D0Planted {\n    fn fallback() -> Self {\n        Self::LastUserMessage\n    }\n}\n";
    probe_user_ingress_path: user_text_enters_histories_only_neutralized,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_map(texts: Vec<String>) -> Vec<ChatMessage> {\n    texts.into_iter().map(ChatMessage::user).collect()\n}\n";
    probe_middleware_rebuild: middleware_history_rebuilds_neutralize_user_messages,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_rebuild(history: &mut Vec<ChatMessage>, messages: Value) {\n    if let Ok(candidate) = serde_json::from_value::<Vec<ChatMessage>>(messages)\n    {\n        *history = candidate;\n    }\n}\n";
    probe_chat_message_constructors: chat_message_constructors_are_pinned,
        "src/providers/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "impl ChatMessage {\n    pub fn user_text(content: String) -> Self {\n        Self::user(content)\n    }\n}\n";
    probe_budget_window: production_turns_budget_on_the_effective_context_window,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("Some(&config.agent.compaction)", "TurnRouting::Unrouted");
    probe_sub_agent_resolver_alias: sub_agent_tools_resolve_compaction_from_the_whole_configuration,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::tools::DelegateTool as D0Tool;\npub fn d0_planted_alias(agents: Agents, security: Security) -> D0Tool {\n    D0Tool::new(agents, security)\n}\n";
    probe_sub_agent_resolver_type_alias: sub_agent_tools_resolve_compaction_from_the_whole_configuration,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "type D0Spawn = crate::tools::SessionsSpawnTool;\npub fn d0_planted_type(a: A) -> D0Spawn {\n    D0Spawn::new_with_registry(a)\n}\n";
    probe_raw_label_log_field: provider_labels_never_reach_a_log_field_raw,
        "src/router/context.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_label(provider: &str) {\n    tracing::warn!(provider, \"planted\");\n}\n";
    probe_sub_agent_resolver: sub_agent_tools_resolve_compaction_from_the_whole_configuration,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_delegate(agents: Agents, security: Security) -> DelegateTool {\n    DelegateTool::new(agents, security)\n}\n";
    probe_budget_window_contests_file: production_turns_budget_on_the_effective_context_window,
        "src/agent/contests.rs", Planted::NewFile, Reads::ProductionText,
        planted_engine_call!("Some(&config.agent.compaction)", "TurnRouting::Unrouted");
    probe_chat_route_count: chat_route_decision_names_the_registry_root_count,
        "src/chat/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_route(tools_registry: &[u8]) {\n    tracing::info!(tools_count = tools_registry.len(), \"route\");\n}\n";
    probe_context_handoff_owner: context_handoff_has_one_production_owner_and_exact_lookup_is_registered,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::WholeTextOfUnmarkedFiles,
        "pub const D0_PLANTED_HANDOFF: &str = \"[context_handoff]\";\n";
    probe_direct_tool_invocations: direct_tool_invocations_outside_the_execution_service_are_allowlisted,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_call(tool: &dyn Tool) {\n    let _ = tool.execute(args).await;\n}\n";
    probe_gateway_gates: every_gateway_tool_execution_documents_its_alternative_gate,
        "src/gateway/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_call(tool: &dyn Tool) {\n    let _ = tool.execute(args).await;\n}\n";
    probe_direct_invocation_counts: direct_tool_invocation_allowlist_counts_match_the_tree,
        "src/chat/file_mention.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub async fn enrich_file_mentions_for_prompt(tool: &dyn Tool) {\n    let _ = tool.execute(args).await;\n}\n";
    probe_non_tool_occurrences: non_tool_execute_call_sites_match_their_declared_occurrences,
        "src/agent/embedded_turn.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn run_work() {\n    let _ = execution.execute(args);\n}\n";
    probe_closure_providers: non_tool_closure_providers_match_their_definitions,
        "src/memory/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "fn with_client(value: u8) -> u8 {\n    value\n}\n";
    probe_memory_principal_scope: every_memory_read_principal_built_from_a_context_carries_the_owner_scope,
        "src/memory/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_principal(context: &MemoryWriteContext) -> Principal {\n    Principal { id: 1 }\n}\n";
    probe_owner_isolation_bit: the_owner_isolation_bit_is_never_asserted_by_memory_production_code,
        "src/memory/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_isolation(mut principal: Principal) {\n    principal.require_owner_match = true;\n}\n";
    probe_agent_provider_factories: agent_entries_reach_provider_factories_only_through_the_scoped_selection,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_provider() {\n    let _ = create_provider(\"probe\", None);\n}\n";
    probe_session_worker_credentials: session_worker_credentials_are_decided_in_one_scoped_place,
        "src/session_worker/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_credential(config: &Config) -> Option<String> {\n    config.api_key.clone()\n}\n";
    probe_session_worker_alias_destructuring: session_worker_credentials_are_decided_in_one_scoped_place,
        "src/session_worker/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::config::Config as Cfg;\npub fn d0_planted_destructure(config: Cfg) {\n    let Cfg { default_temperature, .. } = config;\n}\n";
    probe_home_directory_lookup: the_home_directory_is_resolved_only_by_the_config_resolver,
        "src/memory/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_home() -> Option<std::path::PathBuf> {\n    std::env::var_os(\"HOME\").map(Into::into)\n}\n";
    probe_home_project_dirs: the_home_directory_is_resolved_only_by_the_config_resolver,
        "src/providers/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_cache() -> Option<std::path::PathBuf> {\n    directories::ProjectDirs::from(\"\", \"\", \"x\").map(|d| d.config_dir().to_path_buf())\n}\n";
    probe_home_shellexpand: the_home_directory_is_resolved_only_by_the_config_resolver,
        "src/auth/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_expand(path: &str) -> String {\n    shellexpand::tilde(path).into_owned()\n}\n";
    probe_home_userprofile_constant: the_home_directory_is_resolved_only_by_the_config_resolver,
        "src/auth/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "const D0_PROFILE_VAR: &str = \"USERPROFILE\";\npub fn d0_planted_profile() -> Option<std::ffi::OsString> {\n    std::env::var_os(D0_PROFILE_VAR)\n}\n";
    probe_home_platform_dir: the_home_directory_is_resolved_only_by_the_config_resolver,
        "src/auth/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_cache() -> Option<std::path::PathBuf> {\n    dirs::cache_dir()\n}\n";
    probe_git_literal: git_is_named_only_in_the_reviewed_places,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "const D0_PROGRAM: &str = \"git\";\n";
    probe_process_command: every_production_process_command_is_inventoried,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_run(program: &str) {\n    let _ = std::process::Command::new(program).output();\n}\n";
    probe_process_command_alias: process_command_types_are_never_aliased,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use tokio::process::Command as D0Proc;\n";
    probe_process_command_value: every_production_process_command_is_inventoried,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_value(programs: Vec<String>) {\n    let _ = programs.into_iter().map(std::process::Command::new);\n}\n";
    probe_git_literal_case: git_is_named_only_in_the_reviewed_places,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "const D0_PROGRAM: &str = \"Git\";\n";
    probe_git_launch: git_runs_only_through_the_hardened_launch,
        "src/tools/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_git() {\n    let _ = tokio::process::Command::new(\"git\");\n}\n";
    probe_provider_construction: provider_construction_outside_the_provider_module_is_pinned,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_chain() {\n    let _ = ReliableProvider::new(Vec::new(), 0, 0);\n}\n";
    probe_decision_mock: mock_decision_backend_is_compiled_only_for_tests,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_mock() {\n    let _ = MockDecisionBackend::identity();\n}\n";
    probe_decision_text: decision_answers_never_become_chat_messages,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_prompt(answer: &DecisionAnswer) -> ChatMessage {\n    ChatMessage::system(answer.text())\n}\n";
    probe_decision_repair: the_decision_validator_never_repairs_a_value,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_fix(value: f64, total: f64) -> f64 {\n    renormalize(value, total)\n}\n";
    probe_decision_mutation: validation_requires_the_sealed_request_manifest,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "impl DecisionRequest {\n    pub fn d0_planted_reseal(&mut self) {}\n}\n";
    probe_decision_manifest: validation_requires_the_sealed_request_manifest,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_decision() -> ValidatedDecision {\n    ValidatedDecision { answers: Default::default() }\n}\n";
    probe_decision_seal_new_caller: only_reviewed_production_sites_seal_decision_requests,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_request() { let _ = DecisionRequest::seal(parts, key, expiry, limits); }\n";
    probe_decision_recoverable_seal_new_caller: only_reviewed_production_sites_seal_decision_requests,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "fn planted() { DecisionRequest::seal_recovering_parts(parts, audit, key, expiry, limits); }\n";
    probe_decision_recoverable_seal_extra_allowed_caller: only_reviewed_production_sites_seal_decision_requests,
        "src/causal_tree/questions.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn planted() { DecisionRequest::seal_recovering_parts(parts, audit, key, expiry, limits); }\n";
    probe_decision_unchecked_constructor_visibility: validation_requires_the_sealed_request_manifest,
        "src/decision/protocol.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub(crate) fn from_validated_parts() {}\n";
    probe_decision_compiler_untrusted_option: decision_compiler_uses_typed_ids_and_only_redacted_state,
        "src/causal_tree/questions.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn d0_planted_option(state: &CausalState) { let _ = OptionId::code(state.routing_input.as_str()); }\n";
    probe_causal_admin_commit_neighbor: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/admin.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn planted_commit() { let _ = crate::config::files::commit_mutation_guarded; }\n";
    probe_causal_child_replay_memory_neighbor: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/store/child_outcome.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn planted_memory() { let _ = crate::memory::MemoryFabric::new; }\n";
    probe_causal_outcome_finalizer: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/outcome.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn d0_planted_finalize() { let _ = crate::agent::terminal::finalize_turn; }\n";
    probe_causal_outcome_service_builder: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/outcome.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn d0_planted_service() { let _ = crate::runtime::turn_decision::build_turn_decision_service; }\n";
    probe_causal_outcome_memory_io: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/outcome.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn d0_planted_memory() { let _ = crate::memory::MemoryFabric::new; }\n";
    probe_causal_engine_search_glob: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/decision_engine.rs", Planted::AppendedTo, Reads::ProductionText,
        "use crate::tools::capability::search::*;\n";
    probe_causal_engine_backend: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/decision_engine.rs", Planted::AppendedTo, Reads::ProductionText,
        "use crate::decision::CompatibleHttpBackend;\n";
    probe_causal_network: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_fetch() {\n    let _ = reqwest::get(url).await;\n}\n";
    probe_causal_proxy_client: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_proxy() {\n    let _ = crate::config::build_runtime_proxy_client(name);\n}\n";
    probe_causal_backend_type: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::decision::CompatibleHttpBackend;\npub fn d0_planted_backend() {}\n";
    probe_causal_gate_rejects_execution_service: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/gates.rs", Planted::AppendedTo, Reads::ProductionText,
        "use crate::tools::execution::ToolExecutionService;\n";
    probe_causal_question_rejects_http_backend: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/questions.rs", Planted::AppendedTo, Reads::ProductionText,
        "use crate::decision::compatible_http::CompatibleHttpBackend;\n";
    probe_causal_module_alias: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::decision as d;\npub fn d0_planted_alias() {\n    let _ = d::http::send();\n}\n";
    probe_causal_provider: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_provider() {\n    let _ = crate::providers::create_provider(name, None);\n}\n";
    probe_causal_socket: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_socket() {\n    let _ = tokio::net::TcpStream::connect(address).await;\n}\n";
    probe_causal_path_attribute: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "#[path = \"../net.rs\"]\nmod d0_planted_net;\n";
    probe_causal_glob: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::decision::*;\npub fn d0_planted_glob() {}\n";
    probe_causal_mod_super: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/mod.rs", Planted::AppendedTo, Reads::ProductionText,
        "use super::providers::compatible::OpenAiCompatibleProvider;\n";
    probe_causal_subprocess: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_curl() {\n    let _ = std::process::Command::new(program);\n}\n";
    probe_decision_constant_timeout: decision_domain_has_no_constant_timeout_or_approval_grant_constructor,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_timeout() { let _ = tokio::time::timeout(wait, future).await; }\n";
    probe_causal_approval_grant: decision_domain_has_no_constant_timeout_or_approval_grant_constructor,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_grant() { let _ = ApprovalGrantV2::issue_one_shot(subject); }\n";
    probe_causal_websocket: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_socket() {\n    let _ = tokio_tungstenite::connect_async(url).await;\n}\n";
    probe_causal_crate_named_like_a_module: the_causal_domain_imports_no_backend_and_no_network,
        "src/causal_tree/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::state::Anything;\npub fn d0_planted_state() {}\n";
    probe_trusted_state_caller: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_trust(parts: TrustedParts) -> TrustedState {\n    TrustedState::from_runtime(parts)\n}\n";
    probe_trusted_state_function_value: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_map(parts: Vec<TrustedParts>) -> Vec<TrustedState> {\n    parts.into_iter().map(TrustedState::from_runtime).collect()\n}\n";
    probe_observed_step_caller: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_step(id: CandidateId) -> ObservedStep {\n    ObservedStep::recorded_by_runtime(kind, status, Some(id), None)\n}\n";
    probe_observed_step_alias: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::causal_tree::state::ObservedStep as Step;\npub fn d0_planted_alias() -> Step {\n    Step::recorded_by_runtime(kind, status, None, None)\n}\n";
    probe_observed_assignment: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_observed(state: &mut CausalState, observed: ObservedState) {\n    state.observed = observed;\n}\n";
    probe_observed_mem_replace: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_replace(state: &mut CausalState, observed: ObservedState) {\n    let _ = std::mem::replace(&mut state.observed, observed);\n}\n";
    probe_trusted_mem_swap: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_swap(state: &mut CausalState, trusted: &mut TrustedState) {\n    std::mem::swap(&mut state.trusted, trusted);\n}\n";
    probe_trusted_parts_literal: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_parts() -> TrustedParts {\n    TrustedParts { workspace_id }\n}\n";
    probe_trusted_parts_borrowed_literal: trust_bearing_turn_state_is_built_only_by_the_turn_decision_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_borrow() {\n    let _ = &TrustedParts { workspace_id };\n}\n";
    probe_decision_chat_provider: decision_code_never_uses_a_chat_provider,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_chat(name: &str) {\n    let _ = crate::providers::create_provider(name, None);\n}\n";
    probe_decision_provider_trait: decision_code_never_uses_a_chat_provider,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub struct D0Planted {\n    chat: Box<dyn crate::providers::Provider>,\n}\n";
    probe_decision_validate_outside_factory: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_validate(request: &DecisionRequest, response: DecisionResponse, ctx: &ValidationContext) {\n    \
         let _ = DecisionResponseValidator::validate(request, response, ctx);\n}\n";
    probe_decision_builtin_outside_factory: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_builtin(request: &DecisionRequest) {\n    let _ = BuiltinDecisionBackend::answer(request);\n}\n";
    probe_decision_provider_reexport: decision_code_never_uses_a_chat_provider,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::agent::Provider;\npub fn d0_planted_reexport(chat: &dyn Provider) {}\n";
    probe_decision_provider_module: decision_code_never_uses_a_chat_provider,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::providers::reliable::ReliableProvider as Chat;\npub fn d0_planted_chat() {}\n";
    probe_decision_evaluate_outside_factory: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "use crate::decision::backend::DecisionBackend;\npub async fn d0_planted_live(backend: &dyn DecisionBackend) {\n    \
         let _ = backend.evaluate(request, body, deadline).await;\n}\n";
    probe_decision_parsed_host: loopback_is_decided_only_by_the_shared_predicate,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_local(url: &Url) -> bool {\n    url.host_str() == Some(name)\n}\n";
    probe_decision_address_match: loopback_is_decided_only_by_the_shared_predicate,
        "src/causal_tree/config.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_v4(ip: std::net::Ipv4Addr) -> bool {\n    ip.is_loopback()\n}\n";
    probe_causal_metric_label: decision_metric_labels_come_from_bounded_allowlists,
        "src/observability/prometheus.rs", Planted::AppendedTo, Reads::ProductionText,
        "pub fn d0_planted_metric() {\n    let _ = IntCounterVec::new(prometheus::Opts::new(\"prx_causal_d0_total\", \"probe\"), &[\"session\"]);\n}\n";
    probe_causal_metric_lineage: decision_metric_labels_come_from_bounded_allowlists,
        "src/observability/prometheus.rs", Planted::AppendedTo, Reads::ProductionText,
        "impl D0Planted {\n    fn record_causal(&self, event: &CausalEvent) {\n        let _ = &event.lineage;\n    }\n}\n";
    probe_decision_store_opened_elsewhere: the_decision_store_database_is_opened_only_by_the_store,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_open(dir: &std::path::Path) {\n    let _ = rusqlite::Connection::open(dir.join(\"decisions.db\"));\n}\n";
    probe_causal_metric_helper: decision_metric_labels_come_from_bounded_allowlists,
        "src/observability/prometheus.rs", Planted::AppendedTo, Reads::ProductionText,
        "fn d0_planted_label(event: &CausalEvent) -> &str {\n    event.lineage.workspace_id.as_ref()\n}\n";
    probe_causal_otel_attributes: decision_metric_labels_come_from_bounded_allowlists,
        "src/observability/otel.rs", Planted::AppendedTo, Reads::ProductionText,
        "impl D0Planted {\n    fn d0_planted(&self, event: &CausalEvent) {\n        self.causal_events.add(1, &[KeyValue::new(\"session\", event.kind.as_str())]);\n    }\n}\n";
    probe_decision_default_client: decision_http_clients_come_only_from_the_pinned_builder,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_default() -> reqwest::Client {\n    reqwest::Client::default()\n}\n";
    probe_decision_blocking_client: decision_http_clients_come_only_from_the_pinned_builder,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_get(url: &str) {\n    let _ = reqwest::blocking::get(url);\n}\n";
    probe_decision_other_client: decision_http_clients_come_only_from_the_pinned_builder,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_client() -> reqwest::Client {\n    reqwest::Client::new()\n}\n";
    probe_decision_insecure_client: decision_http_clients_come_only_from_the_pinned_builder,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_insecure(builder: B) -> B {\n    builder.danger_accept_invalid_certs(true)\n}\n";
    probe_decision_backend_encodes: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_encode(request: &DecisionRequest, key: &DecisionKey) {\n    let _ = SystemOneCodec::new(request, key).and_then(|codec| codec.encode_request());\n}\n";
    probe_decision_backend_guards_itself: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/decision/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_guard(bytes: Vec<u8>, secrets: &OutboundSecrets) {\n    let _ = check_outbound(bytes, u32::MAX, secrets);\n}\n";
    probe_decision_transport_receiver: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_receiver(deps: &FactoryDeps<'_>, spec: &BackendSpec<'_>) {\n    let _ = deps.transports.build(spec);\n}\n";
    probe_decision_spec_literal: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_spec(identity: BackendIdentity) {\n    let _ = BackendSpec { identity, locality, endpoint, resolved, timeout, max_response_bytes, key };\n}\n";
    probe_decision_guarded_evaluate: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub async fn d0_planted_bare(backend: &Live, body: &GuardedBody) {\n    let _ = backend.evaluate(request, body, deadline).await;\n}\n";
    probe_decision_backend_constructed: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_backend(spec: &BackendSpec<'_>) {\n    let _ = CompatibleHttpBackend::new(spec);\n}\n";
    probe_decision_transport_outside_factory: only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_build(transports: &dyn DecisionTransports, spec: &BackendSpec<'_>) {\n    \
         let _ = transports.build(spec);\n}\n";
    probe_causal_policy_assignment: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_threshold(config: &mut Config) {\n    config.causal_tree.policy.commit_threshold = 0.1;\n}\n";
    probe_causal_policy_compound: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_widen(config: &mut Config) {\n    config.causal_tree.policy.max_candidates += 1;\n}\n";
    probe_causal_policy_remote_switch: causal_policy_is_written_only_by_the_mutation_service,
        "src/runtime/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_remote(config: &mut Config) {\n    config\n        .causal_tree\n        .backend\n        .remote_enabled = true;\n}\n";
    probe_causal_policy_mut_borrow: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_take(config: &mut Config) {\n    let _ = std::mem::take(&mut config.causal_tree.policy);\n}\n";
    probe_causal_policy_literal: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_policy() -> CausalPolicy {\n    CausalPolicy { commit_threshold: 0.1, ..Default::default() }\n}\n";
    probe_causal_policy_turbofish: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_turbofish(config: &mut Config) {\n    config.causal_tree.policy.calibration_profile.get_or_insert_with::<Make>(make);\n}\n";
    probe_causal_policy_index_assignment: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_index(config: &mut Config) {\n    config.causal_tree.scoring.levels[0] = 1.0;\n}\n";
    probe_causal_policy_normalize_call: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_normalize(config: &mut Config) {\n    config.causal_tree.normalize();\n}\n";
    probe_causal_policy_push_str: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_push(config: &mut Config) {\n    config.causal_tree.backend.endpoint.push_str(value);\n}\n";
    probe_causal_policy_destructure: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_bind(config: &mut Config) {\n    let Config { causal_tree, .. } = config;\n    causal_tree.enabled = true;\n}\n";
    probe_causal_policy_shorthand_init: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_fill(causal_tree: CausalTreeConfig, base: Config) -> Config {\n    Config { causal_tree, ..base }\n}\n";
    probe_causal_policy_attributed_field: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_attr(section: CausalTreeConfig, base: Config) -> Config {\n    Config {\n        #[cfg(unix)]\n        causal_tree: section,\n        ..base\n    }\n}\n";
    probe_causal_policy_config_field: causal_policy_is_written_only_by_the_mutation_service,
        "src/agent/d0_planted_probe.rs", Planted::NewFile, Reads::ProductionText,
        "pub fn d0_planted_config(section: CausalTreeConfig) -> Config {\n    Config { causal_tree: section, ..Config::default() }\n}\n";
}

// Destructive source substitutions cannot use the appended-item probe form.
// Bind the scanner name and function from one identifier; the metaguard runs
// the actual mutation proof rather than exempting its scanner from coverage.
macro_rules! source_mutation_probes {
    ($($scanner:ident => $proof:ident),+ $(,)?) => {
        pub(super) const MUTATION_PROBED_TREE_SCANNERS: &[(&str, fn(), fn())] = &[
            $((stringify!($scanner), $scanner, $proof)),+
        ];
    };
}

source_mutation_probes! {
    provider_label_sinks_redact_url_userinfo => terminal_provider_label_guard_rejects_bypass_missing_calls_and_literal_decoys,
    embedded_user_input_cannot_bypass_reserved_marker_neutralization => mutation_proof_embedded_input_guard,
    every_non_tool_execute_call_site_shows_its_type_evidence => mutation_proof_execution_binding_guard,
    forwarded_compaction_values_are_produced_from_the_effective_window => mutation_proof_compaction_producer_guard,
    both_gateway_launch_paths_register_their_tools_from_one_body => mutation_proof_gateway_registration_guard,
    ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates => mutation_proof_credential_comparison_guard,
}

// These are mutation proofs, not additional tests or test-inclusive exceptions.
fn assert_reviewed_source_mutation(scanner: fn(), path: &str, needle: &str, replacement: &str) {
    assert!(!scanner_fails_with(scanner, Vec::new()));
    let source = fs::read_to_string(repository_root().join(path)).expect("read actual mutation source");
    assert!(source.contains(needle), "reviewed mutation site disappeared: {path}");
    assert!(scanner_fails_with(
        scanner,
        vec![(path.into(), source.replacen(needle, replacement, 1))]
    ));
}

fn mutation_proof_embedded_input_guard() {
    assert_reviewed_source_mutation(
        embedded_user_input_cannot_bypass_reserved_marker_neutralization,
        "src/agent/embedded_turn.rs",
        "crate::agent::turn_contracts::inbound_user_message(input)",
        "ChatMessage::user(input)",
    );
}
fn mutation_proof_execution_binding_guard() {
    assert_reviewed_source_mutation(
        every_non_tool_execute_call_site_shows_its_type_evidence,
        "src/agent/embedded_turn.rs",
        "service.bind_agent_execution(source, &decision, &context, &selected.specs)?",
        "service.bind_unreviewed_execution(source, &decision, &context, &selected.specs)?",
    );
}
fn mutation_proof_compaction_producer_guard() {
    for expression in [
        "unreviewed.effective_compaction()",
        "unreviewed.admitted_binding.effective_compaction()",
        "compaction_resolver.resolve(provider, model)",
    ] {
        assert_eq!(
            classify_compaction_argument("", expression, 0),
            CompactionSource::Forwarded
        );
    }
    for expression in [
        "admitted_binding.effective_compaction()?",
        "self.effective_compaction(provider, model)",
        "decided.service.effective_compaction(provider, model)",
    ] {
        assert_eq!(
            classify_compaction_argument("", expression, 0),
            CompactionSource::Effective
        );
    }
    for (path, old, new) in [
        (
            "src/chat/run/entry.rs",
            "        list,\n        shutdown,\n        None,",
            "        false,\n        shutdown,\n        None,",
        ),
        ("src/chat/run/entry.rs", "        Some(producer),", "        None,"),
        (
            "src/chat/run.rs",
            "compaction_config: Some(effective_compaction.config.clone())",
            "compaction_config: Some(config.agent.compaction.clone())",
        ),
    ] {
        assert_reviewed_source_mutation(
            forwarded_compaction_values_are_produced_from_the_effective_window,
            path,
            old,
            new,
        );
    }
    assert_reviewed_source_mutation(
        forwarded_compaction_values_are_produced_from_the_effective_window,
        "src/runtime/turn_decision.rs",
        "let main_compaction = reservation.reserved_compaction(&effective.config, extra_token_ratio_limit)?;",
        "let main_compaction = config.agent.compaction.clone();",
    );
}
fn mutation_proof_gateway_registration_guard() {
    for (path, old, new) in [
        (
            "src/gateway/mod.rs",
            "run_gateway_inner(host, port, config, shared_config, shutdown, None).await",
            "Ok(())",
        ),
        (
            "src/gateway/mod.rs",
            "let (channels_registry, spawn_tools_handle) = register_gateway_turn_tools(",
            "let (channels_registry, spawn_tools_handle) = unreviewed_gateway_turn_tools(",
        ),
        (
            "src/gateway/h2_control.rs",
            "config, None, shutdown, Some(producer)",
            "config, None, shutdown, None",
        ),
    ] {
        assert_reviewed_source_mutation(
            both_gateway_launch_paths_register_their_tools_from_one_body,
            path,
            old,
            new,
        );
    }
}
fn mutation_proof_credential_comparison_guard() {
    assert_reviewed_source_mutation(
        ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates,
        "src/session_worker/runner/ordinary_source.rs",
        "resolved.credential().api_key() == credentials.sealed_key",
        "resolved.credential().api_key() == manifest.api_key.as_deref()",
    );
}

/// Module classification must suppress only compiler-excluded fixtures, and
/// both production guards must still report the same text through a live load.
#[test]
fn out_of_line_test_ancestry_preserves_production_violation_probes() {
    for (scanner, violation) in [
        (
            causal_policy_is_written_only_by_the_mutation_service as fn(),
            "fn forbidden() { config.causal_tree.enabled = true; }",
        ),
        (
            direct_tool_invocations_outside_the_execution_service_are_allowlisted as fn(),
            "fn forbidden() { tool.execute(args); }",
        ),
    ] {
        for (parent, child) in [
            ("#[cfg(test)] mod cases { mod child; }", "src/d0_fixture/cases/child.rs"),
            ("#[cfg(test)] mod cases;", "src/d0_fixture/cases/child.rs"),
        ] {
            let mut planted = vec![
                ("src/d0_fixture.rs".into(), parent.into()),
                (child.into(), violation.into()),
            ];
            if parent.ends_with(';') {
                planted.push(("src/d0_fixture/cases.rs".into(), "mod child;".into()));
            }
            assert!(
                !scanner_fails_with(scanner, planted.clone()),
                "test-only descendant reported: {child}"
            );
            planted.push((
                "src/d0_live.rs".into(),
                "#[path = \"d0_fixture/cases/child.rs\"] mod live;".into(),
            ));
            assert!(
                scanner_fails_with(scanner, planted),
                "production loader hidden: {child}"
            );
        }
        assert!(
            scanner_fails_with(scanner, vec![("src/d0_orphan/tests.rs".into(), violation.into())]),
            "an undeclared file called tests.rs must remain production"
        );
    }
}

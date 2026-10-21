use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, is_ident_char, production_part, read_source,
    relative_path, repository_root, rust_source_files, strip_cfg_test_items,
};
use std::collections::BTreeSet;
use std::fs;
#[test]
fn config_generation_manager_is_the_only_runtime_config_publisher() {
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        if relative == "src/config/generation.rs" {
            continue;
        }
        let compact = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        for forbidden in [
            "ArcSwap<Config>",
            "ArcSwap<crate::config::Config>",
            "ArcSwap<super::Config>",
            "Arc<ArcSwap<Config>>",
            "ArcSwap<Arc<Config>>",
            "ArcSwapAny<Arc<Config>>",
        ] {
            assert!(
                !compact.contains(forbidden),
                "{relative} reintroduced an independent runtime config publisher via {forbidden}"
            );
        }
        assert!(
            !source.contains("EvolutionRuntimeConfigManager"),
            "{relative} reintroduced the retired evolution file-polling config publisher"
        );
    }
}

#[test]
fn daemon_owned_components_do_not_reload_process_config_from_disk() {
    fn contains_main_config_call(source: &str, call: &str) -> bool {
        source.match_indices(call).any(|(offset, _)| {
            source[..offset]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
        })
    }

    for relative in [
        "src/channels/mod.rs",
        "src/channels/inbound_policy.rs",
        "src/channels/prompt_context.rs",
        "src/channels/history.rs",
        "src/channels/runtime_commands.rs",
        "src/channels/turn_failure.rs",
        "src/channels/response_sanitize.rs",
        "src/channels/signal_vision.rs",
        "src/channels/supervisor.rs",
        "src/channels/turn.rs",
        "src/channels/smart_gate.rs",
        "src/channels/dispatch.rs",
        "src/channels/service.rs",
        "src/channels/doctor.rs",
        "src/channels/startup.rs",
        "src/cron/scheduler.rs",
        "src/xin/runner.rs",
        "src/heartbeat/engine.rs",
        "src/webhook/mod.rs",
        "src/self_system/fitness.rs",
        "src/self_system/evolution/config.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            !contains_main_config_call(&source, "Config::load"),
            "{relative} must receive a pinned ConfigGeneration and cannot call any main Config::load* entrypoint"
        );
        for forbidden in ["HotReloadManager", "compute_config_fingerprint_gated"] {
            assert!(
                !source.contains(forbidden),
                "{relative} must receive a pinned ConfigGeneration and cannot reload process config via {forbidden}"
            );
        }
    }
}

#[test]
fn gateway_and_channels_have_one_shared_config_owner() {
    let gateway = fs::read_to_string(repository_root().join("src/gateway/mod.rs")).expect("read src/gateway/mod.rs");
    let app_state_start = gateway.find("pub struct AppState").expect("AppState");
    let app_state = &gateway[app_state_start
        ..gateway[app_state_start..]
            .find("\n}")
            .map_or(gateway.len(), |offset| app_state_start + offset + 2)];
    assert_eq!(
        app_state.matches("SharedConfig").count(),
        1,
        "AppState must expose exactly one SharedConfig owner"
    );
    assert!(
        !app_state.contains("shared_config:"),
        "AppState must not restore the former config/shared_config split"
    );

    for relative in [
        "src/channels/mod.rs",
        "src/channels/inbound_policy.rs",
        "src/channels/prompt_context.rs",
        "src/channels/history.rs",
        "src/channels/runtime_commands.rs",
        "src/channels/turn_failure.rs",
        "src/channels/response_sanitize.rs",
        "src/channels/signal_vision.rs",
        "src/channels/supervisor.rs",
        "src/channels/turn.rs",
        "src/channels/smart_gate.rs",
        "src/channels/dispatch.rs",
        "src/channels/service.rs",
        "src/channels/doctor.rs",
        "src/channels/startup.rs",
    ] {
        let channels = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            !channels.contains("HotReloadManager") && !channels.contains("ArcSwap<Config>"),
            "{relative}: Channels must not own an independent config watcher or config publisher"
        );
    }
}

#[test]
fn every_config_reload_entrypoint_routes_through_generation_manager() {
    for (relative, required) in [
        (
            "src/config/hotreload.rs",
            "reload_from_disk(ConfigReloadTrigger::FileWatcher)",
        ),
        (
            "src/tools/config_reload.rs",
            "reload_from_disk(ConfigReloadTrigger::Tool)",
        ),
        ("src/config/manage/transaction.rs", "manager.reload_from_disk(trigger)"),
        (
            "src/gateway/mod.rs",
            "reload_from_disk(crate::config::ConfigReloadTrigger::PairingPersistence)",
        ),
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains(required),
            "{relative} must route config reload through ConfigGenerationManager"
        );
    }
}

/// Every configuration write is either the transaction layer, one of its
/// primitives, or a writer listed here as not yet migrated. The scan covers
/// all of `src/`, ignores whitespace (so a call rustfmt splits over lines is
/// still found), and matches the method form (`cfg.save()`, also when the
/// future is awaited later) and the path form (`Config::save(&cfg)`). Each
/// allowed file is pinned to the exact number of each call it holds today, so
/// a new writer added to a large allowed file fails too: route it through
/// `config::manage::ConfigTransactions` instead, or, for a primitive, update
/// the count here with a reason.
///
/// Limits of a textual guard: a writer that goes through a helper under
/// another name, or writes the file with plain `std::fs`, is not found.
#[test]
pub(super) fn config_writes_route_through_the_transaction_layer() {
    const WRITE_CALLS: &[&str] = &[
        ".save()",
        "::save(",
        "commit_mutation_atomically(",
        "commit_mutation_guarded(",
        "reload_from_disk(",
        ".apply_config(",
        "apply_runtime_config(",
        "write_toml_string_atomic_without_lock(",
        "write_split_config(",
        "merge_split_config(",
        "plan_structured_save(",
    ];
    // (file, call, count, reason). The transaction layer and its primitives
    // first, then the writers not yet migrated (see docs/configuration.md).
    const ALLOWED: &[(&str, &str, usize, &str)] = &[
        (
            "src/config/manage/transaction.rs",
            "commit_mutation_guarded(",
            2,
            "the transaction layer: commit and restore",
        ),
        (
            "src/config/manage/transaction.rs",
            "reload_from_disk(",
            1,
            "the transaction layer: activation",
        ),
        (
            "src/config/manage/transaction.rs",
            "plan_structured_save(",
            1,
            "the transaction layer: structured plan",
        ),
        (
            "src/config/files.rs",
            "commit_mutation_atomically(",
            3,
            "commit primitive and its split/merge callers",
        ),
        ("src/config/files.rs", "commit_mutation_guarded(", 1, "commit primitive"),
        (
            "src/config/init.rs",
            "commit_mutation_guarded(",
            1,
            "revision-guarded explicit causal schema marker adoption",
        ),
        (
            "src/config/files.rs",
            "write_toml_string_atomic_without_lock(",
            10,
            "journaled per-file writes",
        ),
        ("src/config/files.rs", "write_split_config(", 1, "split primitive"),
        ("src/config/files.rs", "merge_split_config(", 1, "merge primitive"),
        (
            "src/config/files.rs",
            "plan_structured_save(",
            1,
            "structured plan primitive",
        ),
        (
            "src/config/schema/lifecycle.rs",
            ".save()",
            1,
            "Config::save (first-run creation)",
        ),
        (
            "src/config/schema/lifecycle.rs",
            "commit_mutation_atomically(",
            1,
            "Config::save",
        ),
        (
            "src/config/schema/credentials.rs",
            "commit_mutation_guarded(",
            1,
            "the startup secret migration (revision-guarded, D-101 F-5)",
        ),
        (
            "src/config/schema/atomic.rs",
            "write_toml_string_atomic_without_lock(",
            2,
            "the atomic write primitive",
        ),
        (
            "src/config/schema/lifecycle.rs",
            "plan_structured_save(",
            1,
            "Config::save",
        ),
        (
            "src/config/generation.rs",
            "reload_from_disk(",
            1,
            "the generation manager",
        ),
        (
            "src/config/generation.rs",
            ".apply_config(",
            2,
            "the generation manager",
        ),
        (
            "src/config/generation.rs",
            "apply_runtime_config(",
            1,
            "the generation manager (in-memory, tests)",
        ),
        (
            "src/config/hotreload.rs",
            "reload_from_disk(",
            1,
            "the file watcher (reload only)",
        ),
        (
            "src/config/init.rs",
            "commit_mutation_atomically(",
            1,
            "first-run initialization",
        ),
        (
            "src/tools/config_reload.rs",
            "reload_from_disk(",
            1,
            "explicit reload (no write)",
        ),
        (
            "src/gateway/mod.rs",
            ".save()",
            1,
            "not yet migrated: pairing-token persistence",
        ),
        (
            "src/gateway/mod.rs",
            "reload_from_disk(",
            1,
            "not yet migrated: pairing-token persistence",
        ),
        (
            "src/channels/service.rs",
            ".save()",
            1,
            "not yet migrated: prx channel bind-telegram",
        ),
        (
            "src/channels/telegram.rs",
            ".save()",
            2,
            "not yet migrated: Telegram in-channel bind (one in a doc comment)",
        ),
        (
            "src/onboard/wizard.rs",
            ".save()",
            2,
            "offline channel repair (one occurrence is in a comment)",
        ),
        (
            "src/onboard/wizard/persistence.rs",
            ".save()",
            1,
            "offline first-run defaults",
        ),
        (
            "src/onboard/wizard.rs",
            "write_split_config(",
            1,
            "offline channel repair",
        ),
        (
            "src/onboard/wizard/persistence.rs",
            "write_split_config(",
            1,
            "safe first-run defaults",
        ),
        (
            "src/runtime/mode.rs",
            "write_split_config(",
            1,
            "not yet migrated: prx config split (offline)",
        ),
        (
            "src/runtime/mode.rs",
            "merge_split_config(",
            1,
            "not yet migrated: prx config merge (offline)",
        ),
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let compact = production_part(&source)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        for call in WRITE_CALLS {
            let found = compact.matches(call).count();
            let allowed = ALLOWED
                .iter()
                .find(|(file, allowed_call, _, _)| *file == relative && allowed_call == call)
                .map_or(0, |(_, _, count, _)| *count);
            if found != allowed {
                violations.push(format!("{relative}: {call} found {found}, allowed {allowed}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "configuration written outside the transaction layer (or an allow-list count is stale): {violations:?}"
    );

    // The migrated surfaces use the layer.
    for relative in [
        "src/gateway/api/config.rs",
        "src/tools/gateway.rs",
        "src/tools/proxy_config.rs",
        "src/config/manage/service.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            production_part(&source).contains("ConfigTransactions::new("),
            "{relative} must write configuration through ConfigTransactions"
        );
    }
}

/// The management CLI (PMM-E) calls the one management service in-process:
/// it never builds its own transaction, writes no configuration itself, and
/// renders no plan of its own.
#[test]
fn the_management_cli_goes_through_the_management_service() {
    let source = fs::read_to_string(repository_root().join("src/runtime/manage_cli.rs"))
        .unwrap_or_else(|error| panic!("read src/runtime/manage_cli.rs: {error}"));
    let production = production_part(&source);
    // The CLI does not run the configuration's runtime, so it records what
    // it applies as deferred to the daemon (D-085 F-4).
    assert!(production.contains("ManagementService::deferred("));
    assert!(!production.contains("ManagementService::new("));
    for forbidden in ["ConfigTransactions::new(", "ConfigChangeSummary::between(", ".save()"] {
        assert!(
            !production.contains(forbidden),
            "src/runtime/manage_cli.rs must not call {forbidden}; use config::manage::ManagementService"
        );
    }
}

/// The Console raw editor can confirm a high-risk save: on 428 it asks the
/// operator and retries with `confirm_high_risk=true` (source contract; every
/// Console source change ships with a rebuilt, committed `console/dist` in the
/// same change, D-038, which superseded D-018 Q2).
#[test]
fn console_raw_editor_confirms_a_high_risk_save() {
    let api = fs::read_to_string(repository_root().join("console/src/lib/api.js")).expect("read console api.js");
    assert!(
        api.contains("?confirm_high_risk=true"),
        "saveConfigFile must be able to confirm"
    );
    let page = fs::read_to_string(repository_root().join("console/src/pages/ConfigPage.svelte"))
        .expect("read ConfigPage.svelte");
    assert!(page.contains("status !== 428") && page.contains("window.confirm("));
    assert!(page.contains("confirmHighRisk: true"));
}

/// The Console binds a confirmed retry to the refused plan: it resends with
/// `If-Match` set to the revision the 428 names (G-2).
#[test]
fn console_confirmed_retry_is_bound_to_the_planned_revision() {
    let api = fs::read_to_string(repository_root().join("console/src/lib/api.js")).expect("read console api.js");
    assert!(
        api.contains("'If-Match'"),
        "saveConfigFile must be able to send If-Match"
    );
    let page = fs::read_to_string(repository_root().join("console/src/pages/ConfigPage.svelte"))
        .expect("read ConfigPage.svelte");
    assert!(
        page.contains("e.body?.revision"),
        "the retry must use the 428's revision"
    );
}

/// The Console's first save is bound to the revision the editor loaded
/// (`If-Match` from the `GET` `ETag`), so a change made after loading answers
/// 409 instead of being replaced by the stale buffer (R-PMMDh H-3).
#[test]
fn console_first_save_is_bound_to_the_loaded_revision() {
    let api = fs::read_to_string(repository_root().join("console/src/lib/api.js")).expect("read console api.js");
    assert!(
        api.contains("headers.get('etag')"),
        "getConfigFiles must return the ETag"
    );
    let page = fs::read_to_string(repository_root().join("console/src/pages/ConfigPage.svelte"))
        .expect("read ConfigPage.svelte");
    assert!(
        page.contains("revision: loadedRevision"),
        "the first save must send the loaded revision"
    );
}

/// The production functions outside the config resolver that look up the
/// user's home directory or a per-user platform directory, as
/// `file::function`. Everything else under the home (the default
/// `~/.openprx`, other tools' credential files, `~` in a path) goes through
/// the resolver, which the lib-test binary points at a scratch home.
const ALLOWED_HOME_DIRECTORY_LOOKUPS: &[&str] = &[
    // The resolver: the home directory and the platform config directory.
    "src/config/schema/paths.rs::user_home_dir",
    "src/config/schema/paths.rs::platform_config_dir",
    // Reports whether `HOME` is set at all; nothing is read under it.
    "src/doctor/mod.rs::check_environment",
    "src/runtime/shell_process.rs::<module>",
    // Sets `HOME` of a contained skill run to the run directory; nothing is read
    // under the real home.
    "src/runtime/shell_process.rs::contained_environment",
    // The stdio MCP launch allowlist (D-045) names `HOME` (and, on Windows,
    // `USERPROFILE`) as a variable the child inherits; nothing is read under
    // it here.
    "src/config/mcp_launch.rs::<module>",
];

/// Calls that look up the home directory or a per-user platform directory.
const HOME_LOOKUP_CALLS: &[&str] = &[
    "UserDirs::new",
    "BaseDirs::new",
    "ProjectDirs::from",
    "home_dir",
    "shellexpand::tilde",
    "shellexpand::full",
    "getpwuid",
    "getpwuid_r",
    "User::from_uid",
];

/// Environment variables that name the home directory.
const HOME_VARIABLES: &[&str] = &["HOME", "USERPROFILE"];

/// Whether `line` calls a `dirs::…_dir(` (or `dirs_next::…_dir(`) function,
/// such as `dirs::config_dir()` or `dirs::cache_dir()`.
fn platform_dir_call(line: &str) -> bool {
    ["dirs::", "dirs_next::"].iter().any(|prefix| {
        line.match_indices(prefix).any(|(offset, _)| {
            let before_ok = line[..offset]
                .chars()
                .next_back()
                .is_none_or(|previous| !is_ident_char(previous));
            let name: String = line[offset + prefix.len()..]
                .chars()
                .take_while(|character| is_ident_char(*character))
                .collect();
            before_ok && name.ends_with("_dir") && line[offset + prefix.len() + name.len()..].starts_with('(')
        })
    })
}

/// Whether `line` calls `name(` as a whole identifier.
fn calls_identifier(line: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    line.match_indices(&needle).any(|(offset, _)| {
        line[..offset]
            .chars()
            .next_back()
            .is_none_or(|previous| !is_ident_char(previous))
    })
}

/// D-070, D-071. The user's home is resolved in one place,
/// `crate::config::user_home_dir`, which the lib-test binary points at a
/// scratch home, so no test reads or writes the operator's real
/// configuration or another tool's credentials. Any other production lookup
/// of the home or a per-user platform directory (`UserDirs`, `BaseDirs`,
/// `ProjectDirs`, `home_dir()`, `shellexpand`, the `HOME` variable) could reach
/// the real home from a test again, so each function that makes one is
/// pinned here after review.
#[test]
pub(super) fn the_home_directory_is_resolved_only_by_the_config_resolver() {
    let mut actual = BTreeSet::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let original = source.lines().collect::<Vec<_>>();
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let directory_lookup =
                HOME_LOOKUP_CALLS.iter().any(|name| calls_identifier(line, name)) || platform_dir_call(line);
            // The variable names as string literals, wherever they appear
            // (a lookup, a constant, a wrapped call): literals are blanked in
            // the production text, so the original line is read, but only
            // where the line is production code (test code is blank).
            let variable_named = !line.trim().is_empty()
                && original
                    .get(index)
                    .is_some_and(|text| HOME_VARIABLES.iter().any(|name| text.contains(&format!("\"{name}\""))));
            if directory_lookup || variable_named {
                actual.insert(format!("{relative}::{}", enclosing_function(&lines, index)));
            }
        }
    }
    assert_inventory(
        "home directory lookups outside crate::config::user_home_dir",
        actual.into_iter().collect(),
        ALLOWED_HOME_DIRECTORY_LOOKUPS,
    );
}

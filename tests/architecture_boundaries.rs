//! Regression guards for architecture boundaries that are not expressed by Rust's type system.
//!
//! These allowlists intentionally describe the current tree. A new entry is a review event:
//! route the behavior through the owning repository/adapter/event fabric, or update the
//! allowlist with an explicit architecture decision.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED_BRAIN_DB_OPENS_OUTSIDE_SQLITE_REPOSITORY: &[&str] = &[
    "src/agent/loop_.rs::build_agent_context_includes_document_evidence_with_source_ids::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/agent/loop_.rs::configurable_compaction_records_run_and_summary_memory::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/chat/mod.rs::legacy_chat_compaction_downgrades_fidelity_when_event_provenance_is_unavailable::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/chat/mod.rs::legacy_chat_compaction_persists_run_and_summary_memory::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/cron/store.rs::add_job_persists_owner_topic_lineage_and_event::let memory_conn = Connection::open(config.workspace_dir.join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/cron/store.rs::cron_event_outbox_retries_memory_mirror_idempotently::let cron_conn = Connection::open(&cron_db).unwrap();",
    "src/cron/store.rs::cron_event_outbox_retries_memory_mirror_idempotently::let cron_conn = Connection::open(&cron_db).unwrap();",
    "src/cron/store.rs::cron_event_outbox_retries_memory_mirror_idempotently::let memory_conn = Connection::open(memory_dir.join(\"brain.db\")).unwrap();",
    "src/cron/store.rs::legacy_cron_schema_migrates_lineage_columns_and_events_table::let conn = Connection::open(&db_path).unwrap();",
    "src/cron/store.rs::with_connection::Connection::open(&db_path).with_context(|| format!(\"Failed to open cron DB: {}\", db_path.display()))?;",
    "src/doctor/mod.rs::read_only_sqlite_session_count::let conn = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    "src/gateway/compat.rs::mcp_agent_identity_binding_upserts_sqlite_row::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/gateway/compat.rs::upsert_agent_identity_binding::let conn = Connection::open(db_path)?;",
    "src/main.rs::open_approval_ledger::Connection::open(&db_path).with_context(|| format!(\"Failed to open approval ledger: {}\", db_path.display()))?;",
    "src/memory/hygiene.rs::close_stale_topics::let conn = Connection::open(db_path)?;",
    "src/memory/hygiene.rs::closes_stale_open_topics::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/hygiene.rs::closes_stale_open_topics::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/hygiene.rs::keeps_old_conversation_rows_with_high_useful_count::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/hygiene.rs::keeps_old_daily_rows_with_high_useful_count::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/hygiene.rs::prune_conversation_rows::let conn = Connection::open(db_path)?;",
    "src/memory/hygiene.rs::prune_daily_rows::let conn = Connection::open(db_path)?;",
    "src/memory/hygiene.rs::prunes_old_conversation_rows_in_sqlite_backend::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/hygiene.rs::prunes_old_daily_rows_in_sqlite_backend::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/mod.rs::upsert_acl_bootstrap::let conn = Connection::open(db_path)?;",
    "src/memory/snapshot.rs::export_and_hydrate_roundtrip::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/snapshot.rs::export_and_hydrate_roundtrip::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/snapshot.rs::export_snapshot::let conn = Connection::open(&db_path)?;",
    "src/memory/snapshot.rs::hydrate_from_snapshot::let conn = Connection::open(&db_path)?;",
    "src/memory/snapshot.rs::hydrate_sets_owner_visibility_for_file_entries::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/memory/snapshot.rs::should_hydrate_only_when_needed::let conn = Connection::open(&db_path).unwrap();",
    "src/memory/topic.rs::setup_conn::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/migration.rs::dry_run_does_not_write::let conn = Connection::open(&source_db).unwrap();",
    "src/migration.rs::migration_renames_conflicting_key::let conn = Connection::open(&source_db).unwrap();",
    "src/migration.rs::migration_skips_empty_content::let conn = Connection::open(&db_path).unwrap();",
    "src/migration.rs::read_openclaw_sqlite_entries::let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/migration.rs::sqlite_reader_supports_legacy_value_column::let conn = Connection::open(&db_path).unwrap();",
    "src/schema_migration/mod.rs::configured_sqlite_inspection_is_byte_for_byte_read_only::let conn = Connection::open(&db_path).unwrap();",
    "src/schema_migration/mod.rs::configured_inspection_isolated_from_tokio_runtime::let conn = Connection::open(&db_path).unwrap();",
    "src/schema_migration/mod.rs::inspect_sqlite_path::let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/tools/memory_get.rs::observe_mode_returns_entry_but_audits_would_deny::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/tools/memory_get.rs::open_conn::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap()",
    "src/tools/memory_search.rs::observe_mode_returns_results_while_recording_would_deny::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/tools/memory_search.rs::open_conn::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap()",
    "src/tools/memory_store.rs::store_persists_trusted_scope_metadata::let conn = rusqlite::Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::committed_ingestion_replays_after_state_restart_without_duplicate_rows::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::failed_ingestion_rolls_back_all_state_and_same_key_retries::let conn = Connection::open(&db_path).unwrap();",
    "src/webhook/mod.rs::new::let conn = Connection::open(&db_path)",
    "src/webhook/mod.rs::pending_ingestion_reports_processing_and_expired_lease_is_reclaimed::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::readonly_autonomy_blocks_persist_with_forbidden::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::same_external_id_in_different_projects_keeps_separate_topics::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::supervised_autonomy_allows_persist::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::token_auth_valid_accepts_and_persists_topic::let conn = Connection::open(db_path).unwrap();",
    "src/xin/evolution.rs::draft_evolution_scheduler_creates_draft_without_agent_run::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/xin/evolution.rs::tick::Connection::open(&db_path).with_context(|| format!(\"failed to open memory db: {}\", db_path.display()))?;",
    "src/xin/store.rs::add_task_persists_owner_topic_lineage_and_event::let memory_conn = Connection::open(config.workspace_dir.join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/xin/store.rs::event_outbox_recovers_cross_database_delivery_idempotently::let brain = Connection::open(memory_path.join(\"brain.db\")).unwrap();",
    "src/xin/store.rs::event_outbox_recovers_cross_database_delivery_idempotently::let brain = Connection::open(memory_path.join(\"brain.db\")).unwrap();",
    "src/xin/store.rs::legacy_xin_tasks_schema_migrates_lineage_columns_and_events_table::let conn = Connection::open(&db_path).unwrap();",
    "src/xin/store.rs::open_xin_test_connection::let conn = Connection::open(&db_path).unwrap();",
    "src/xin/store.rs::with_connection::let conn = Connection::open(&db_path).with_context(|| format!(\"Failed to open xin DB: {}\", db_path.display()))?;",
];

const ALLOWED_RAW_CHILD_PROCESS_SPAWNS: &[&str] = &[
    "src/channels/signal_native.rs::health_check::let Ok(_child) = cmd.spawn() else {",
    "src/channels/signal_native.rs::listen::.spawn()",
    "src/chat/mod.rs::run_git_diff_bounded::.spawn()",
    "src/chat/sessions/shell.rs::spawn_shell_with_origin::.spawn()",
    "src/chat/terminal_proto.rs::copy_to_tmux_buffer::.spawn()?;",
    "src/media/mod.rs::run_command_bounded::.spawn()",
    "src/tools/sessions_spawn.rs::assert_explicit_cleanup_signals_once::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_panic_cleanup_try_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_termination_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_wait_error_keeps_child_finalization_and_slot_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owned_child_panic_cleanup_signals_once_and_reaps_before_return::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owner_keeps_child_after_requester_timeout_until_reap::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owner_mediated_process_kill_reaps_leader_and_terminates_group::let child = command.spawn().expect(\"test child should spawn\");",
    "src/tools/sessions_spawn.rs::process_mode_parent_timeout_kills_stuck_process::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::run_sub_agent_process::let mut child = command.spawn()?;",
    "src/tools/sessions_spawn.rs::termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe::let mut child = command.spawn().expect(\"test leader should spawn\");",
    "src/runtime/shell_process.rs::spawn_managed_shell_child::let child = cmd.spawn()?;",
    "src/tunnel/cloudflare.rs::start::.spawn()?;",
    "src/tunnel/custom.rs::start::.spawn()?;",
    "src/tunnel/mod.rs::kill_shared_terminates_and_clears_child::.spawn()",
    "src/tunnel/ngrok.rs::start::.spawn()?;",
    "src/tunnel/tailscale.rs::start::.spawn()?;",
];

const ALLOWED_PERSISTED_EVENT_TABLES: &[&str] = &[
    "src/cron/postgres.rs::cron_job_events",
    "src/cron/postgres.rs::{qualified_memory_events_table}",
    "src/cron/store.rs::cron_event_outbox",
    "src/cron/store.rs::cron_job_events",
    "src/memory/postgres.rs::{qualified_memory_events_table}",
    "src/memory/postgres.rs::{qualified_message_events_table}",
    "src/memory/postgres.rs::{schema_ident}.approval_grant_events",
    "src/memory/sqlite.rs::approval_grant_events",
    "src/memory/sqlite.rs::evolution_proposal_events",
    "src/memory/sqlite.rs::memory_events",
    "src/memory/sqlite.rs::memory_events",
    "src/memory/sqlite.rs::memory_events",
    "src/memory/sqlite.rs::message_events",
    "src/memory/sqlite.rs::message_events",
    "src/memory/sqlite.rs::message_events",
    "src/xin/evolution.rs::evolution_proposal_events",
    "src/xin/store.rs::xin_event_outbox",
    "src/xin/store.rs::xin_task_events",
];

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_source_files() -> Vec<PathBuf> {
    fn visit(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display())) {
            let path = entry.expect("read directory entry").path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(&repository_root().join("src"), &mut files);
    files.sort();
    files
}

fn relative_path(path: &Path) -> String {
    path.strip_prefix(repository_root())
        .expect("source path must be inside repository")
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn enclosing_function(lines: &[&str], line_index: usize) -> String {
    for line in lines[..=line_index].iter().rev() {
        let Some(fn_offset) = line.find("fn ") else {
            continue;
        };
        let name = line[fn_offset + 3..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect::<String>();
        if !name.is_empty() {
            return name;
        }
    }
    "<module>".to_string()
}

fn function_body(source: &str, name: &str) -> String {
    let needle = format!("fn {name}(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("missing production function {name}"));
    let body_start = source[start..].find('{').map_or_else(
        || panic!("missing body for production function {name}"),
        |offset| start + offset,
    );
    let mut depth = 0usize;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1).expect("function brace depth underflow");
                if depth == 0 {
                    return source[start..=body_start + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated production function {name}")
}

fn assert_inventory(boundary: &str, actual: Vec<String>, expected: &[&str]) {
    let mut actual = actual;
    actual.sort();
    let mut expected = expected.iter().map(ToString::to_string).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        actual, expected,
        "{boundary} changed. New entries must be routed through the approved architecture; intentional exceptions require architecture review and an explicit allowlist update"
    );
}

#[test]
fn brain_db_is_not_opened_outside_the_sqlite_repository() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative == "src/memory/sqlite.rs" {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        if !source.contains("brain.db") {
            continue;
        }
        let lines = source.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if line.contains("Connection::open(") || line.contains("Connection::open_with_flags(") {
                actual.push(format!(
                    "{relative}::{}::{}",
                    enclosing_function(&lines, index),
                    normalize_line(line)
                ));
            }
        }
    }
    assert_inventory(
        "direct brain.db opens outside src/memory/sqlite.rs",
        actual,
        ALLOWED_BRAIN_DB_OPENS_OUTSIDE_SQLITE_REPOSITORY,
    );
}

#[test]
fn raw_child_process_spawns_are_explicitly_allowlisted() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let lines = source.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains(".spawn(") {
                continue;
            }
            let function_start = (0..=index)
                .rev()
                .find(|candidate| lines[*candidate].contains("fn "))
                .unwrap_or(0);
            let prefix = lines[function_start..=index].join("\n");
            let is_process_spawn = prefix.contains("Command::new")
                || prefix.contains("process::Command")
                || prefix.contains(".build_command()")
                || line.contains("cmd.spawn(")
                || line.contains("command.spawn(");
            let is_thread_spawn = prefix.contains("std::thread::Builder::new") && !prefix.contains("process::Command");
            if is_process_spawn && !is_thread_spawn {
                actual.push(format!(
                    "{relative}::{}::{}",
                    enclosing_function(&lines, index),
                    normalize_line(line)
                ));
            }
        }
    }
    assert_inventory(
        "raw child-process spawn sites",
        actual,
        ALLOWED_RAW_CHILD_PROCESS_SPAWNS,
    );
}

#[test]
fn shell_entrypoints_delegate_process_execution_to_shared_adapter() {
    for (relative, entrypoint, execution_helper) in [
        ("src/tools/shell.rs", "execute_inner", "execute_inner"),
        (
            "src/cron/scheduler.rs",
            "run_job_command_with_timeout_authorization",
            "run_job_command_with_timeout_and_adapter",
        ),
        (
            "src/xin/runner.rs",
            "run_shell_with_cancellation",
            "run_shell_with_adapter",
        ),
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let entrypoint_body = function_body(&source, entrypoint);
        let helper_body = function_body(&source, execution_helper);
        if entrypoint != execution_helper {
            let helper_call_marker = format!("{execution_helper}(");
            assert!(
                entrypoint_body.contains(&helper_call_marker),
                "{relative}::{entrypoint} must call the inspected helper {execution_helper}"
            );
        }
        assert!(
            helper_body.contains(".execute(") && helper_body.contains("ShellProcessRequest"),
            "{relative}::{execution_helper} must execute a ShellProcessRequest through ShellProcessAdapter"
        );
        let inspected_chain = format!("{entrypoint_body}\n{helper_body}");
        for forbidden in [
            "tokio::process::Command",
            "std::process::Command",
            "Command::new",
            ".spawn(",
            ".output(",
            ".status(",
        ] {
            assert!(
                !inspected_chain.contains(forbidden),
                "{relative}::{entrypoint}->{execution_helper} reintroduced raw process execution via {forbidden}"
            );
        }
    }
}

#[test]
fn persisted_event_tables_are_explicitly_allowlisted() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let lowercase = source.to_ascii_lowercase();
        let mut remainder = lowercase.as_str();
        while let Some(offset) = remainder.find("create table") {
            remainder = &remainder[offset + "create table".len()..];
            let mut tokens = remainder.split_whitespace();
            let first = tokens.next().unwrap_or_default();
            let table = if first == "if" {
                let _not = tokens.next();
                let _exists = tokens.next();
                tokens.next().unwrap_or_default()
            } else {
                first
            };
            let table =
                table.trim_matches(|character: char| matches!(character, '`' | '\'' | '"' | '(' | ')' | ',' | ';'));
            if table.contains("event") || table.contains("ledger") {
                actual.push(format!("{relative}::{table}"));
            }
        }
    }
    assert_inventory("persisted event-ledger tables", actual, ALLOWED_PERSISTED_EVENT_TABLES);
}

fn top_level_modules(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            if line != line.trim_start() {
                return None;
            }
            let line = line.trim();
            let module = line
                .strip_prefix("mod ")
                .or_else(|| line.strip_prefix("pub mod "))
                .or_else(|| line.strip_prefix("pub(crate) mod "))?;
            module
                .strip_suffix(';')
                .filter(|name| {
                    name.chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                })
                .map(ToString::to_string)
        })
        .collect()
}

#[test]
fn main_rs_does_not_duplicate_additional_library_modules() {
    let root = repository_root();
    let main = fs::read_to_string(root.join("src/main.rs")).expect("read src/main.rs");
    let library = fs::read_to_string(root.join("src/lib.rs")).expect("read src/lib.rs");
    let duplicates = top_level_modules(&main)
        .intersection(&top_level_modules(&library))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        duplicates.is_empty(),
        "src/main.rs must not redeclare library modules; found duplicates: {duplicates:?}"
    );
}

#[test]
fn config_generation_manager_is_the_only_runtime_config_publisher() {
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
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

    let channels = fs::read_to_string(repository_root().join("src/channels/mod.rs")).expect("read src/channels/mod.rs");
    assert!(
        !channels.contains("HotReloadManager") && !channels.contains("ArcSwap<Config>"),
        "Channels must not own an independent config watcher or config publisher"
    );
}

#[test]
fn runtime_and_message_events_preserve_typed_config_generation_columns() {
    let envelope =
        fs::read_to_string(repository_root().join("src/runtime/envelope.rs")).expect("read runtime envelope");
    assert!(envelope.contains("pub config_generation_id: Option<u64>"));
    assert!(envelope.contains("pub config_source_revision: Option<String>"));
    assert!(
        envelope.contains("config_generation_id: None") && envelope.contains("config_source_revision: None"),
        "unstamped envelopes must remain explicitly unknown rather than fabricating generation zero"
    );
    let message_scope = function_body(&envelope, "message_scope");
    assert!(message_scope.contains("scope.config_generation_id = self.config_generation_id"));
    assert!(message_scope.contains("scope.config_source_revision = self.config_source_revision.clone()"));

    for relative in ["src/memory/sqlite.rs", "src/memory/postgres.rs"] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        for required in [
            "config_generation_id",
            "config_source_revision",
            "idx_message_events_config_generation",
        ] {
            assert!(
                source.contains(required),
                "{relative} must persist and index {required}"
            );
        }
    }

    for relative in ["src/agent/loop_.rs", "src/chat/mod.rs"] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains("with_config_generation("),
            "{relative} must stamp RuntimeEnvelope at the turn admission boundary"
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
        (
            "src/gateway/api/config.rs",
            "reload_from_disk(crate::config::ConfigReloadTrigger::Api)",
        ),
        (
            "src/gateway/api/config.rs",
            "reload_from_disk(crate::config::ConfigReloadTrigger::ConfigFileApi)",
        ),
        (
            "src/gateway/mod.rs",
            "reload_from_disk(crate::config::ConfigReloadTrigger::PairingPersistence)",
        ),
        (
            "src/tools/proxy_config.rs",
            "reload_from_disk(ConfigReloadTrigger::Tool)",
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

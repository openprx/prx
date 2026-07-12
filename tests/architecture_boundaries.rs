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
    "src/chat/mod.rs::legacy_chat_compaction_persists_run_and_summary_memory::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/cron/store.rs::add_job_persists_owner_topic_lineage_and_event::let memory_conn = Connection::open(config.workspace_dir.join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/cron/store.rs::legacy_cron_schema_migrates_lineage_columns_and_events_table::let conn = Connection::open(&db_path).unwrap();",
    "src/cron/store.rs::with_connection::let conn = Connection::open(&db_path).with_context(|| format!(\"Failed to open cron DB: {}\", db_path.display()))?;",
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
    "src/migration.rs::dry_run_schema_migrations::Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/migration.rs::migration_renames_conflicting_key::let conn = Connection::open(&source_db).unwrap();",
    "src/migration.rs::migration_skips_empty_content::let conn = Connection::open(&db_path).unwrap();",
    "src/migration.rs::plan_schema_migrations::Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/migration.rs::read_openclaw_sqlite_entries::let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/migration.rs::sqlite_reader_supports_legacy_value_column::let conn = Connection::open(&db_path).unwrap();",
    "src/schema_migration/mod.rs::open_sqlite_memory_db::Connection::open(&db_path).with_context(|| format!(\"open memory db {}\", db_path.display()))",
    "src/tools/memory_get.rs::execute::let conn = match Connection::open(&db_path) {",
    "src/tools/memory_get.rs::observe_mode_returns_entry_but_audits_would_deny::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/tools/memory_get.rs::open_conn::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap()",
    "src/tools/memory_search.rs::execute::let conn = match Connection::open(&db_path) {",
    "src/tools/memory_search.rs::observe_mode_returns_results_while_recording_would_deny::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/tools/memory_search.rs::open_conn::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap()",
    "src/tools/memory_store.rs::store_persists_trusted_scope_metadata::let conn = rusqlite::Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::finalize_persisted_event::let conn = Connection::open(db_path).with_context(|| format!(\"failed to open webhook db {}\", db_path.display()))?;",
    "src/webhook/mod.rs::persist_event_topic::let conn = Connection::open(db_path).with_context(|| format!(\"failed to open webhook db {}\", db_path.display()))?;",
    "src/webhook/mod.rs::readonly_autonomy_blocks_persist_with_forbidden::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::same_external_id_in_different_projects_keeps_separate_topics::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::supervised_autonomy_allows_persist::let conn = Connection::open(db_path).unwrap();",
    "src/webhook/mod.rs::token_auth_valid_accepts_and_persists_topic::let conn = Connection::open(db_path).unwrap();",
    "src/xin/evolution.rs::draft_evolution_scheduler_creates_draft_without_agent_run::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/xin/evolution.rs::tick::Connection::open(&db_path).with_context(|| format!(\"failed to open memory db: {}\", db_path.display()))?;",
    "src/xin/store.rs::add_task_persists_owner_topic_lineage_and_event::let memory_conn = Connection::open(config.workspace_dir.join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/xin/store.rs::legacy_xin_tasks_schema_migrates_lineage_columns_and_events_table::let conn = Connection::open(&db_path).unwrap();",
    "src/xin/store.rs::with_connection::let conn = Connection::open(&db_path).with_context(|| format!(\"Failed to open xin DB: {}\", db_path.display()))?;",
];

const ALLOWED_RAW_CHILD_PROCESS_SPAWNS: &[&str] = &[
    "src/channels/signal_native.rs::health_check::let Ok(_child) = cmd.spawn() else {",
    "src/channels/signal_native.rs::listen::.spawn()",
    "src/chat/mod.rs::run_git_diff_bounded::.spawn()",
    "src/chat/sessions/shell.rs::spawn_shell_with_origin::.spawn()",
    "src/chat/terminal_proto.rs::copy_to_tmux_buffer::.spawn()?;",
    "src/cron/scheduler.rs::run_job_command_with_timeout::.spawn()",
    "src/hooks/mod.rs::run_action::let mut child = cmd.spawn()?;",
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
    "src/tools/shell.rs::spawn_managed_shell_child::let child = cmd.spawn()?;",
    "src/tunnel/cloudflare.rs::start::.spawn()?;",
    "src/tunnel/custom.rs::start::.spawn()?;",
    "src/tunnel/mod.rs::kill_shared_terminates_and_clears_child::.spawn()",
    "src/tunnel/ngrok.rs::start::.spawn()?;",
    "src/tunnel/tailscale.rs::start::.spawn()?;",
    "src/xin/runner.rs::run_shell::let child = match command.spawn() {",
];

const ALLOWED_PERSISTED_EVENT_TABLES: &[&str] = &[
    "src/cron/postgres.rs::cron_job_events",
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
    "src/xin/evolution.rs::evolution_proposal_events",
    "src/xin/store.rs::xin_task_events",
];

const ALLOWED_MAIN_LIBRARY_MODULE_DUPLICATES: &[&str] = &[
    "acl",
    "agent",
    "approval",
    "auth",
    "causal_tree",
    "channels",
    "config",
    "cost",
    "cron",
    "daemon",
    "gateway",
    "health",
    "heartbeat",
    "hooks",
    "identity",
    "llm",
    "media",
    "memory",
    "multimodal",
    "nodes",
    "observability",
    "onboard",
    "plugins",
    "providers",
    "recovery",
    "router",
    "runtime",
    "schema_migration",
    "security",
    "self_system",
    "skills",
    "tools",
    "tunnel",
    "util",
    "webhook",
    "xin",
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
    assert_inventory(
        "modules declared in both src/main.rs and src/lib.rs",
        duplicates,
        ALLOWED_MAIN_LIBRARY_MODULE_DUPLICATES,
    );
}

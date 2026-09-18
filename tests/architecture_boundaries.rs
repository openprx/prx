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
    // Test-only: proving the unbounded busy handler waits past the retired 5s
    // deadline requires a second, non-pooled connection to hold the write lock.
    "src/cron/store.rs::cron_write_waits_past_the_old_five_second_deadline_and_warns::let conn = Connection::open(&db_path).expect(\"test: holder connection\");",
    "src/cron/store.rs::legacy_cron_schema_migrates_lineage_columns_and_events_table::let conn = Connection::open(&db_path).unwrap();",
    "src/cron/store.rs::with_connection::Connection::open(&db_path).with_context(|| format!(\"Failed to open cron DB: {}\", db_path.display()))?;",
    // Read-only doctor probes must not initialize the memory repository merely
    // to count legacy rows in an existing database.
    "src/doctor/mod.rs::read_only_sqlite_key_prefix_count::let connection = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    "src/doctor/mod.rs::read_only_sqlite_session_count::let conn = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    // Test-only legacy rows intentionally bypass the new public write guard so
    // the migration and doctor can prove they repair pre-guard data.
    "src/doctor/mod.rs::doctor_reports_legacy_fitness_counts_and_repair_command::let connection = rusqlite::Connection::open(config.workspace_dir.join(\"memory/brain.db\")).unwrap();",
    "src/fitness_cli.rs::migration_exports_then_removes_legacy_rows_and_projections::let connection = rusqlite::Connection::open(db_path).unwrap();",
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
    // Test-only (b1): process-mode liveness is only proved against a real
    // child. One is externally SIGKILLed so the parent sees nothing but a
    // closed pipe (the OOM-kill shape); the other emits output in instalments
    // so the drain can be observed refreshing the run's progress beat while the
    // child is still alive. A managed spawn would hide both.
    "src/tools/sessions_spawn.rs::a_sigkilled_worker_is_classified_as_exited_without_result::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    // b2: a join must survive a member that is a real OS process emitting
    // nothing at all; only a real child can prove the silence is real.
    "src/tools/sessions_spawn.rs::a_join_survives_a_real_child_process_that_never_writes_anything::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    // Test-only (b3): the wording that separates `no_result` from `failed` must
    // be derived from a genuinely SIGKILLed child through the production
    // classifier, not typed into the test — a managed spawn would supply an
    // exit status the classifier never sees in the fault this models.
    "src/tools/sessions_spawn.rs::silent_death_reason_from_a_real_sigkill::.spawn()",
    "src/tools/sessions_spawn.rs::worker_output_refreshes_the_run_beat_while_it_is_still_arriving::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn.rs::assert_explicit_cleanup_signals_once::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_panic_cleanup_try_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_termination_wait_error_keeps_owner_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::injected_wait_error_keeps_child_finalization_and_slot_pending::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owned_child_panic_cleanup_signals_once_and_reaps_before_return::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owner_keeps_child_after_requester_timeout_until_reap::let child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::owner_mediated_process_kill_reaps_leader_and_terminates_group::let child = command.spawn().expect(\"test child should spawn\");",
    "src/tools/sessions_spawn.rs::process_mode_waits_for_natural_completion_without_a_deadline::let mut child = command.spawn().unwrap();",
    "src/tools/sessions_spawn.rs::run_sub_agent_process::let mut child = command.spawn()?;",
    // Test-only: the steer wire is only proved by a real pipe to a real child,
    // so these two stand in for a session-worker (one echoes stdin, one never
    // reads it) rather than mocking the very pipe under test.
    "src/tools/sessions_spawn.rs::steer_frames_reach_worker_stdin_and_land_in_history::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn.rs::steer_pump_backpressures_when_the_worker_never_reads_stdin::let mut child = command.spawn().expect(\"stand-in worker spawns\");",
    "src/tools/sessions_spawn.rs::termination_after_leader_exit_does_not_wait_forever_on_inherited_pipe::let mut child = command.spawn().expect(\"test leader should spawn\");",
    // Test-only: the orphan-reclaim test must create a child that deliberately
    // bypasses the managed path, otherwise there is no orphan to detect.
    "src/runtime/registry.rs::unreaped_child_is_reported_as_an_orphan_then_reclaimed::let mut child = command.spawn().expect(\"test: spawn detached child\");",
    // Test-only: proving the hang detector cleans up through `killpg` requires a
    // child that no RAII path can reclaim, so the managed spawn (which kills on
    // drop) would hide the very thing under test.
    "src/agent/idle.rs::hung_turn_is_terminated_and_its_child_process_group_is_killed::let child = command.spawn().unwrap();",
    "src/runtime/shell_process.rs::spawn_managed_shell_child::let child = cmd.spawn()?;",
    // MCP stdio transports must hand the raw child pipes to rmcp. Both sites
    // create a process group and register the child in the shared runtime
    // ledger; discovery closes immediately and call sessions close on reset.
    "src/tools/mcp.rs::call_stdio::let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;",
    "src/tools/mcp.rs::discover_server_tools_stdio::let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;",
    "src/tunnel/cloudflare.rs::start::.spawn()?;",
    "src/tunnel/custom.rs::start::.spawn()?;",
    "src/tunnel/mod.rs::kill_shared_terminates_and_clears_child::.spawn()",
    "src/tunnel/ngrok.rs::start::.spawn()?;",
    "src/tunnel/tailscale.rs::start::.spawn()?;",
];

const ALLOWED_PERSISTED_EVENT_TABLES: &[&str] = &[
    // Dedicated Article 73 compliance ledger: it stores immutable event hashes,
    // is owned by IncidentStore, and is intentionally separate from message/memory events.
    "src/compliance/incident.rs::serious_incident_audit_events",
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

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display())) {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn rust_source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(&repository_root().join("src"), &mut files);
    files.sort();
    files
}

/// Every Rust file the repository owns: the crate sources and the integration
/// tests.
fn rust_source_and_test_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(&repository_root().join("src"), &mut files);
    collect_rust_files(&repository_root().join("tests"), &mut files);
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
            "run_job_command_with_adapter",
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

#[test]
fn prompt_guided_tool_protocol_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
            let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
            production.contains("## Tool Use Protocol").then_some(relative)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        owners,
        vec!["src/tools/prompt.rs"],
        "the prompt-guided protocol must be rendered by one canonical production module"
    );
}

#[test]
fn channel_system_prompt_delegates_to_the_canonical_agent_builder() {
    let channels = fs::read_to_string(repository_root().join("src/channels/mod.rs")).expect("read src/channels/mod.rs");
    let body = function_body(&channels, "build_system_prompt_with_mode");

    assert!(body.contains("crate::agent::prompt::SystemPromptBuilder::with_defaults()"));
    for duplicated_section in [
        "## Your Task",
        "## Safety",
        "## Workspace",
        "## Project Context",
        "## Current Date & Time",
        "## Runtime",
    ] {
        assert!(
            !body.contains(duplicated_section),
            "channels must not rebuild canonical prompt section {duplicated_section}"
        );
    }
}

#[test]
fn runtime_prompt_has_no_parallel_tool_description_catalog() {
    for relative in [
        "src/agent/loop_.rs",
        "src/channels/mod.rs",
        "src/chat/mod.rs",
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            !source.contains("tool_descs"),
            "{relative} must derive prompt tool descriptions from the compiled ToolSpec snapshot"
        );
    }
}

#[test]
fn compiled_request_context_event_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
            source.contains("llm.request.context.compiled").then_some(relative)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        owners,
        vec!["src/agent/loop_.rs"],
        "request-context provenance must be emitted once by the shared provider loop"
    );
}

#[test]
fn production_tool_loop_memory_is_wired_to_request_context_events() {
    for relative in [
        "src/agent/loop_.rs",
        "src/channels/mod.rs",
        "src/chat/mod.rs",
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner.rs",
        "src/tools/delegate.rs",
        "src/tools/sessions_spawn.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains(".with_event_fabric("),
            "{relative} must attach request-context event persistence to its ToolLoopMemory"
        );
    }

    let dispatcher =
        fs::read_to_string(repository_root().join("src/chat/dispatcher.rs")).expect("read src/chat/dispatcher.rs");
    let redux_memory_chain = dispatcher
        .find("ToolLoopMemory::none()")
        .and_then(|start| dispatcher.get(start..))
        .and_then(|rest| rest.find(';').and_then(|end| rest.get(..end)))
        .unwrap_or_default();
    assert!(
        redux_memory_chain.contains(".with_event_fabric(request_event_fabric)"),
        "the Redux driver must attach request-context persistence to its adapter-owned memory runtime"
    );
}

#[test]
fn context_handoff_has_one_production_owner_and_exact_lookup_is_registered() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
            source.contains("[context_handoff]").then_some(relative)
        })
        .collect::<Vec<_>>();
    assert_eq!(owners, vec!["src/agent/loop_.rs"]);

    let tools = fs::read_to_string(repository_root().join("src/tools/mod.rs")).expect("read src/tools/mod.rs");
    assert!(tools.contains("TranscriptHistoryLookupTool::new(memory.clone())"));
    let policy =
        fs::read_to_string(repository_root().join("src/security/policy.rs")).expect("read src/security/policy.rs");
    assert!(policy.contains("\"transcript_history_lookup\""));

    for relative in [
        "src/agent/loop_.rs",
        "src/session_worker/runner.rs",
        "src/tools/delegate.rs",
        "src/tools/sessions_spawn.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        assert!(
            source.contains("TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME"),
            "{relative} must preserve the hard-switch recovery dependency"
        );
    }
    let loop_source =
        fs::read_to_string(repository_root().join("src/agent/loop_.rs")).expect("read src/agent/loop_.rs");
    assert!(loop_source.contains("context_mode_uses_os_paging"));
    assert!(loop_source.contains("tool_specs.push(required)"));
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
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
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
    let production = execution
        .find("#[cfg(test)]\nmod tests {")
        .map_or(execution.as_str(), |offset| &execution[..offset]);

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
        execute.contains("enforce_command_contract("),
        "ToolExecutionService::execute no longer validates arguments; every entry point funnels \
         through it, so removing the call disables schema enforcement everywhere"
    );

    let schema = fs::read_to_string(root.join("src/tools/schema.rs")).expect("read src/tools/schema.rs");
    let schema_production = schema
        .find("#[cfg(test)]\nmod tests {")
        .map_or(schema.as_str(), |offset| &schema[..offset]);
    assert!(
        schema_production.contains("pub fn compiled_tool_schema("),
        "the per-tool compiled-schema cache is gone; every production call would recompile its contract"
    );
}

/// A webhook turn must run its tool loop inside its own spawn boundary.
///
/// `gateway::tests::the_webhook_turns_tool_loop_runs_inside_the_seeded_spawn_context`
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
    let production = gateway
        .find("#[cfg(test)]\nmod tests {")
        .map_or(gateway.as_str(), |offset| &gateway[..offset]);

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

/// A session worker must select its tool surface from its own manifest.
///
/// `run_validated_manifest` used to read `manifest.allowed_tools` and
/// `manifest.channel_excluded_tools` out at the call site and hand them to
/// `select_tools_for_worker`, and nothing pinned that: the covering unit test
/// calls the filter with a list it computes itself, so replacing the exclusion
/// argument with `&[]` left the whole suite green while a worker resumed from
/// that manifest lost a layer of the channel boundary (independent audit
/// 2026-09-16 §1.4). A unit test on the binding helper does not close this --
/// the call site can simply stop using the helper.
///
/// `select_tools_for_manifest` is the one binding now, and this guard keeps it
/// the only one: the production entry point must go through it, and no other
/// production site in the module may reach the filter directly.
#[test]
fn a_session_worker_selects_its_tools_through_its_manifest() {
    let root = repository_root();
    let source =
        fs::read_to_string(root.join("src/session_worker/runner.rs")).expect("read src/session_worker/runner.rs");
    let production = strip_cfg_test_items(&source);

    let entry = function_body(&production, "run_validated_manifest");
    assert!(
        entry.contains("select_tools_for_manifest(full_tools, &manifest)"),
        "run_validated_manifest no longer selects the worker's tools from its manifest; a list that never \
         came from the sealed manifest can reach the filter and the channel boundary stops crossing the \
         process edge"
    );

    let direct_calls = production
        .matches("select_tools_for_worker(")
        .count()
        .saturating_sub(production.matches("fn select_tools_for_worker(").count());
    assert_eq!(
        direct_calls, 1,
        "select_tools_for_worker must be called from exactly one production site; any other caller picks \
         its own two lists and bypasses the manifest binding select_tools_for_manifest exists to be"
    );

    let binding = function_body(&production, "select_tools_for_manifest");
    assert!(
        binding.contains("select_tools_for_worker(source, &manifest.allowed_tools, &manifest.channel_excluded_tools)"),
        "the one call to select_tools_for_worker must pass both of the manifest's own lists; emptying \
         either of them is the escape hatch this binding exists to close"
    );
}

/// Both gateway launch paths must register their turn tools from one body.
///
/// The gateway the daemon runs (rebuilt on every hot reload) and a standalone
/// `prx gateway` each carried a symmetric copy of the same registration block,
/// and only the daemon copy had tests, so a change to one silently gave the two
/// launch modes different tool surfaces (independent audit 2026-09-16 §2.3).
/// `gateway::tests::both_gateway_launch_paths_publish_the_same_tool_surface`
/// compares what the shared registrar produces; this pins that both entry points
/// still go through it rather than growing a second registration of their own.
#[test]
fn both_gateway_launch_paths_register_their_tools_from_one_body() {
    let root = repository_root();
    let source = fs::read_to_string(root.join("src/gateway/mod.rs")).expect("read src/gateway/mod.rs");
    let production = strip_cfg_test_items(&source);

    for entry in ["build_gateway_turn_runtime", "run_gateway"] {
        let body = function_body(&production, entry);
        assert!(
            body.contains("register_gateway_turn_tools("),
            "{entry} no longer registers its turn tools through register_gateway_turn_tools; the two \
             gateway launch paths can drift into different tool surfaces again, and only one of them \
             is reachable from a test"
        );
    }

    assert_eq!(
        production.matches("SessionsSpawnTool::new(").count(),
        1,
        "sessions_spawn must be constructed at exactly one production site in the gateway; a second one \
         is a second tool surface that no test compares against the first"
    );
    let registrar = function_body(&production, "register_gateway_turn_tools");
    assert!(
        registrar.contains("SessionsSpawnTool::new("),
        "the one sessions_spawn registration must live in register_gateway_turn_tools, which is what \
         both launch paths share and what the gateway tests drive"
    );
}

/// Strip every `#[cfg(test)]` item from `source`, brace-balanced.
///
/// Line-based and deliberately conservative: it matches the shape this crate
/// actually uses (`#[cfg(test)]` immediately above a `mod tests {` or a
/// test-only item). Anything it cannot balance is dropped, which errs towards
/// scanning less rather than reporting a test as a production bypass.
fn strip_cfg_test_items(source: &str) -> String {
    let mut kept = Vec::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            let mut depth: i32 = 0;
            let mut started = false;
            for next in lines.by_ref() {
                for character in next.chars() {
                    if character == '{' {
                        depth += 1;
                        started = true;
                    } else if character == '}' {
                        depth -= 1;
                    }
                }
                if started && depth <= 0 {
                    break;
                }
            }
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// Every method that runs a `Tool`.
///
/// `Tool` declares exactly these four: `execute` and `execute_named` plus their
/// cancellation twins (`src/tools/traits.rs`). The guard below recognises all of
/// them, in every call form Rust offers, because the string the scanner misses
/// is the bypass that ships.
const TOOL_EXECUTION_METHODS: &[&str] = &[
    "execute",
    "execute_named",
    "execute_named_with_cancellation",
    "execute_with_cancellation",
];

/// Receiver identifiers whose `.execute(...)` belongs to some *other* trait, and
/// the type each one actually names.
///
/// Rust's `execute` is a popular method name: `rusqlite`, `tokio-postgres`, the
/// shell process adapter, the chat effect executor, the xin builtin registry and
/// `ToolExecutionService` itself all have one. A scanner with no type
/// information has to separate them by hand, so this list is the hand: it is the
/// *only* way a `.execute(` call escapes the inventory, every entry names the
/// type it stands for, and `non_tool_execute_receivers_are_all_still_in_use`
/// fails the moment one of them stops matching a real call site, so the list
/// cannot quietly rot into a wildcard.
///
/// Everything else is treated as a `Tool` call, including a receiver the scanner
/// cannot reduce to an identifier. The guard fails closed on purpose: a false
/// entry in the inventory costs one review, a false *absence* costs an
/// unauthorised tool execution.
const NON_TOOL_EXECUTE_RECEIVERS: &[(&str, &str)] = &[
    (
        "client",
        "tokio-postgres `Client::execute(statement, params)` and the reqwest client",
    ),
    (
        "client_mut",
        "pooled tokio-postgres client accessor in the webhook ingestion store",
    ),
    ("conn", "rusqlite / tokio-postgres connection handle"),
    (
        "executor",
        "chat `EffectExecutor::execute(effect)` — UI effects, not tools",
    ),
    (
        "process",
        "`ShellProcessAdapter::execute(ShellProcessRequest)` — the shared child-process seam",
    ),
    (
        "registry",
        "xin `BuiltinRegistry::execute(payload, config)` — internal task builtins",
    ),
    (
        "service",
        "`ToolExecutionService::execute(command)` — the approved path itself, not a bypass",
    ),
    ("shell", "`ShellProcessAdapter` held by the skills tool"),
    (
        "tool_execution",
        "the `ToolExecutionService` field on `Agent` — the approved path itself",
    ),
    ("transaction", "rusqlite / tokio-postgres transaction handle"),
    ("tx", "rusqlite / tokio-postgres transaction handle"),
];

/// Production call sites that invoke a `Tool` directly instead of going through
/// `ToolExecutionService`, each with the reason it is allowed to.
///
/// `ToolExecutionService::execute` is the only place that applies the autonomy
/// policy, the approval gate, the published-schema check and the audit record.
/// A direct `Tool` call skips all four. The gateway MCP endpoint did exactly
/// that until 0.8.126 and handed an unauthenticated-by-the-runtime, fully
/// attacker-controlled body straight to a tool; this inventory exists so the
/// next one fails a test instead of shipping.
///
/// Three shapes are legitimate:
///  * **decorators** — a `Tool` implementation forwarding to the tool it wraps.
///    The service already ran its gates on the outer call; the inner hop is one
///    tool's own implementation detail.
///  * **operator-typed chat commands** — `!cmd`, `@file`, `/spawn` and friends
///    in the terminal UI, where the human at the keyboard *is* the authority and
///    there is no model-authored argument document to validate. Converging these
///    onto the service is tracked separately; adding a new one is not.
///  * **gateway operator-plane endpoints that carry their own gate chain** —
///    listed separately in [`GATEWAY_DIRECT_TOOL_INVOCATION_GATES`], which names
///    the alternative gate for each one.
///
/// A new entry needs a reason next to it, and any new entry under `src/gateway/`
/// that is not in `GATEWAY_DIRECT_TOOL_INVOCATION_GATES` is a regression of the
/// 0.8.126 fix by construction.
const ALLOWED_DIRECT_TOOL_INVOCATIONS: &[(&str, &str)] = &[
    (
        "src/agent/agent.rs::spawn_delegate_task",
        "fire-and-forget delegate spawn from the legacy `Agent::run_single` shell; \
         `Agent::execute_tool_call` — the shell's actual tool path — was routed through the \
         service in 0.8.128 and must not come back here",
    ),
    (
        "src/chat/mod.rs::enrich_file_mentions_for_prompt",
        "operator-typed `@file` mention expansion; the argument document is built by this code, \
         not by a model",
    ),
    (
        "src/chat/mod.rs::kill_local_session",
        "operator-typed `/kill`; the human at the keyboard is the authority",
    ),
    (
        "src/chat/mod.rs::run",
        "operator-typed `/spawn`, `/steer` and `!command`; the human at the keyboard is the \
         authority",
    ),
    (
        "src/gateway/api/channels.rs::post_channel_send",
        "operator-plane send with its own gate chain — see GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/gateway/api/config.rs::post_config_reload",
        "operator-plane config reload with its own gate chain — see \
         GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/plugins/runtime.rs::dispatch_alias",
        "decorator: the WASM plugin runtime dispatches a plugin alias onto the plugin tool it \
         already resolved",
    ),
    (
        "src/plugins/runtime.rs::execute_named",
        "decorator: the WASM plugin tool forwards its named entry to the resolved plugin tool",
    ),
    (
        "src/tools/delegate.rs::execute",
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named",
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named_with_cancellation",
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_with_cancellation",
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/execution.rs::invoke",
        "the service's own legacy per-call adapter — this is `ToolExecutionService` reaching its \
         backend after every gate has run",
    ),
    (
        "src/tools/mcp.rs::execute",
        "decorator: the `mcp_call` root alias forwards to its own named entry",
    ),
    (
        "src/tools/mod.rs::execute",
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named",
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named_with_cancellation",
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_with_cancellation",
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/sessions_spawn.rs::execute",
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_named",
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_named_with_cancellation",
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_with_cancellation",
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/skills.rs::execute",
        "decorator: skill aliases forward to the skill runner",
    ),
    (
        "src/tools/skills.rs::execute_named",
        "decorator: skill aliases forward to the skill runner",
    ),
    (
        "src/tools/skills.rs::execute_resolved",
        "decorator: an `http` skill is run by handing the request this code built to the \
         `HttpRequestTool` the skill tool owns; the service already gated the outer `skill_execute`",
    ),
    (
        "src/tools/traits.rs::execute_named",
        "the `Tool` trait's own default bodies: `execute_named` falls back to `execute`",
    ),
    (
        "src/tools/traits.rs::execute_named_with_cancellation",
        "the `Tool` trait's own default bodies: the cancellation twin routes to the named entry",
    ),
    (
        "src/tools/traits.rs::execute_with_cancellation",
        "the `Tool` trait's own default body: the cancellation twin races `execute`",
    ),
];

/// The gateway is internet-adjacent, so every direct tool execution it performs
/// is listed here with the gate chain that stands in for `ToolExecutionService`.
///
/// These are *not* unprotected. Each one builds the tool's arguments itself from
/// typed request fields, so nothing the caller sends becomes a scope claim or a
/// prior-approval claim, and each one runs an authorization gate before the tool
/// does any work. What they do not get is the service's audit record and its
/// published-schema check, which is why they are pinned here and reviewed rather
/// than allowed by shape.
const GATEWAY_DIRECT_TOOL_INVOCATION_GATES: &[(&str, &str)] = &[
    (
        "src/gateway/api/channels.rs::post_channel_send",
        "gate chain: `authorize_resource_mutation(state, \"channel_send\", Medium)` on entry, then \
         `MessageSendTool::execute` itself resolves the channel, authorizes the destination \
         against the outbound scope rules as a cross-channel decision, and refuses cross-channel \
         media. The turn channel is pinned to the operator plane and the argument document is \
         built here from bounded typed fields, so the identity behind the send is always the \
         unknown operator plane",
    ),
    (
        "src/gateway/api/config.rs::post_config_reload",
        "gate chain: the authenticated `/api/config` router, then `ConfigReloadTool::execute` runs \
         `SideEffectGate::authorize_resource_operation(\"config_reload:reload\", Low)` against a \
         `SecurityPolicy` rebuilt from the sole active config generation, so a reload that \
         tightened autonomy constrains the next one. The argument document is the empty object \
         built here, so no caller-supplied approval grant reaches the gate",
    ),
];

const fn is_ident_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// Replace every comment, string literal and character literal with spaces.
///
/// Line structure and character offsets are preserved, so the result can still
/// be split into lines and searched for `fn` without a comment or a doc example
/// that merely *mentions* `.execute(` being mistaken for a call site.
fn blank_comments_and_literals(source: &str) -> String {
    let characters: Vec<char> = source.chars().collect();
    let mut output = characters.clone();
    let length = characters.len();
    let blank = |output: &mut [char], from: usize, to: usize| {
        let end = to.min(length);
        if from >= end {
            return;
        }
        for slot in &mut output[from..end] {
            if *slot != '\n' {
                *slot = ' ';
            }
        }
    };
    let mut index = 0usize;
    while index < length {
        let character = characters[index];
        if character == '/' && index + 1 < length && characters[index + 1] == '/' {
            let mut end = index;
            while end < length && characters[end] != '\n' {
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == '/' && index + 1 < length && characters[index + 1] == '*' {
            let mut depth = 0usize;
            let mut end = index;
            while end < length {
                if characters[end] == '/' && end + 1 < length && characters[end + 1] == '*' {
                    depth += 1;
                    end += 2;
                    continue;
                }
                if characters[end] == '*' && end + 1 < length && characters[end + 1] == '/' {
                    depth = depth.saturating_sub(1);
                    end += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == 'r' || character == 'b' {
            // Raw and byte strings: `r"..."`, `r#"..."#`, `br#"..."#`.
            let mut cursor = index;
            while cursor < length && (characters[cursor] == 'r' || characters[cursor] == 'b') {
                cursor += 1;
            }
            let mut hashes = 0usize;
            while cursor + hashes < length && characters[cursor + hashes] == '#' {
                hashes += 1;
            }
            let quote = cursor + hashes;
            let raw = cursor > index
                && quote < length
                && characters[quote] == '"'
                && (hashes > 0 || characters[index] == 'r');
            let leading_ident = index > 0 && is_ident_char(characters[index - 1]);
            if raw && !leading_ident {
                let mut end = quote + 1;
                loop {
                    if end >= length {
                        break;
                    }
                    if characters[end] == '"' {
                        let mut closing = 0usize;
                        while closing < hashes && end + 1 + closing < length && characters[end + 1 + closing] == '#' {
                            closing += 1;
                        }
                        if closing == hashes {
                            end += 1 + hashes;
                            break;
                        }
                    }
                    end += 1;
                }
                blank(&mut output, index, end);
                index = end;
                continue;
            }
        }
        if character == '"' {
            let mut end = index + 1;
            while end < length {
                if characters[end] == '\\' {
                    end += 2;
                    continue;
                }
                if characters[end] == '"' {
                    end += 1;
                    break;
                }
                end += 1;
            }
            blank(&mut output, index, end);
            index = end;
            continue;
        }
        if character == '\'' {
            // A character literal, not a lifetime: `'a'` or `'\n'`.
            let escaped = index + 1 < length && characters[index + 1] == '\\';
            let mut end = index + 1;
            if escaped {
                end += 1;
                while end < length && characters[end] != '\'' && characters[end] != '\n' && end - index < 12 {
                    end += 1;
                }
            } else {
                end = index + 2;
            }
            if end < length && characters[end] == '\'' {
                blank(&mut output, index, end + 1);
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    output.into_iter().collect()
}

/// The receiver identifier of a method call whose `.` sits at `dot`.
///
/// Walks back over `?`, balanced call/index brackets and whitespace, which is
/// what makes a multi-line chain (`self\n    .inner\n    .execute(args)`) and a
/// trailing accessor (`checkout.client_mut()?.execute(...)`) reduce to the same
/// identifier a single-line call would. A receiver the walk cannot reduce is
/// reported as `<expression>`, which is not in `NON_TOOL_EXECUTE_RECEIVERS` and
/// therefore counts as a tool execution.
fn receiver_identifier(characters: &[char], dot: usize) -> String {
    let mut index = dot;
    loop {
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        if index == 0 {
            return "<expression>".to_string();
        }
        let previous = characters[index - 1];
        if previous == '?' {
            index -= 1;
            continue;
        }
        if previous == ')' || previous == ']' {
            let (open, close) = if previous == ')' { ('(', ')') } else { ('[', ']') };
            let mut depth = 0usize;
            let mut scan = index - 1;
            loop {
                if characters[scan] == close {
                    depth += 1;
                } else if characters[scan] == open {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                if scan == 0 {
                    return "<expression>".to_string();
                }
                scan -= 1;
            }
            index = scan;
            continue;
        }
        if is_ident_char(previous) {
            let mut start = index - 1;
            while start > 0 && is_ident_char(characters[start - 1]) {
                start -= 1;
            }
            return characters[start..index].iter().collect();
        }
        return "<expression>".to_string();
    }
}

/// The type or trait a UFCS call names: `Tool::execute`, `MessageSendTool::execute`
/// and `<Self as Tool>::execute` all reduce to one segment.
fn qualified_path_segment(characters: &[char], colons: usize) -> String {
    let mut index = colons;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 {
        return "<path>".to_string();
    }
    let previous = characters[index - 1];
    if previous == '>' {
        let mut depth = 0usize;
        let mut scan = index - 1;
        loop {
            if characters[scan] == '>' {
                depth += 1;
            } else if characters[scan] == '<' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            if scan == 0 {
                return "<path>".to_string();
            }
            scan -= 1;
        }
        let inner: String = characters[scan + 1..index - 1].iter().collect();
        let qualified = inner.rsplit(" as ").next().unwrap_or(&inner).trim().to_string();
        return qualified.rsplit("::").next().unwrap_or(&qualified).trim().to_string();
    }
    if is_ident_char(previous) {
        let mut start = index - 1;
        while start > 0 && is_ident_char(characters[start - 1]) {
            start -= 1;
        }
        return characters[start..index].iter().collect();
    }
    "<path>".to_string()
}

/// One direct `Tool` execution the scanner found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolExecutionSite {
    /// `<file>::<enclosing function>`, the inventory key.
    entry: String,
    /// The call form, for the failure message: `tool.execute(...)`, UFCS, and so on.
    form: String,
    line: usize,
}

/// Everything one file's production source says about tool execution.
#[derive(Debug, Default)]
struct ToolExecutionScan {
    sites: Vec<ToolExecutionSite>,
    /// Receivers that matched `NON_TOOL_EXECUTE_RECEIVERS` and were skipped.
    excluded_receivers: BTreeSet<String>,
}

/// Find every direct `Tool` execution in one file's production source.
///
/// Token-level rather than regex-level: comments and literals are blanked first,
/// identifiers are matched whole (so `execute_named` never matches inside
/// `execute_named_with_cancellation`), and the receiver is recovered by walking
/// the expression backwards. That covers the method form, the two cancellation
/// twins, UFCS (`Tool::execute(&tool, args)` and `<T as Tool>::execute(...)`) and
/// call chains broken across lines.
fn scan_tool_executions(relative: &str, source: &str) -> ToolExecutionScan {
    let production = strip_cfg_test_items(&blank_comments_and_literals(source));
    let characters: Vec<char> = production.chars().collect();
    let lines: Vec<&str> = production.lines().collect();
    let newlines: Vec<usize> = characters
        .iter()
        .enumerate()
        .filter_map(|(offset, character)| (*character == '\n').then_some(offset))
        .collect();
    let mut scan = ToolExecutionScan::default();

    let mut index = 0usize;
    while index < characters.len() {
        if !is_ident_char(characters[index]) || (index > 0 && is_ident_char(characters[index - 1])) {
            index += 1;
            continue;
        }
        let mut end = index;
        while end < characters.len() && is_ident_char(characters[end]) {
            end += 1;
        }
        let name: String = characters[index..end].iter().collect();
        if !TOOL_EXECUTION_METHODS.contains(&name.as_str()) {
            index = end;
            continue;
        }

        // A call, not a definition or a path mention: the next significant
        // character has to open the argument list.
        let mut after = end;
        while after < characters.len() && characters[after].is_whitespace() {
            after += 1;
        }
        if after >= characters.len() || characters[after] != '(' {
            index = end;
            continue;
        }

        let mut before = index;
        while before > 0 && characters[before - 1].is_whitespace() {
            before -= 1;
        }
        let (receiver, form) = if before > 0 && characters[before - 1] == '.' {
            let receiver = receiver_identifier(&characters, before - 1);
            let form = format!("{receiver}.{name}(...)");
            (receiver, form)
        } else if before > 1 && characters[before - 1] == ':' && characters[before - 2] == ':' {
            let segment = qualified_path_segment(&characters, before - 2);
            let form = format!("{segment}::{name}(...)");
            // A UFCS call has no receiver to exclude: every `Path::execute(...)`
            // in this tree is a tool execution, and a new non-tool one is a
            // review event rather than something the scanner may guess about.
            (String::new(), form)
        } else {
            index = end;
            continue;
        };

        if !receiver.is_empty()
            && NON_TOOL_EXECUTE_RECEIVERS
                .iter()
                .any(|(candidate, _)| *candidate == receiver)
        {
            scan.excluded_receivers.insert(receiver);
            index = end;
            continue;
        }

        let line = newlines.partition_point(|offset| *offset < index);
        scan.sites.push(ToolExecutionSite {
            entry: format!("{relative}::{}", enclosing_function(&lines, line)),
            form,
            line: line + 1,
        });
        index = end;
    }
    scan
}

fn scan_repository_tool_executions() -> ToolExecutionScan {
    let mut combined = ToolExecutionScan::default();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let scan = scan_tool_executions(&relative, &source);
        combined.sites.extend(scan.sites);
        combined.excluded_receivers.extend(scan.excluded_receivers);
    }
    combined
}

/// The autonomy policy, the approval gate, the published-schema check and the
/// audit record all live in `ToolExecutionService::execute`. Anything that calls
/// a `Tool` directly gets none of them, so the set of direct callers is pinned —
/// in every call form, not only the two method names the 0.8.127 guard knew.
#[test]
fn direct_tool_invocations_outside_the_execution_service_are_allowlisted() {
    let scan = scan_repository_tool_executions();

    let gateway_gates: BTreeSet<&str> = GATEWAY_DIRECT_TOOL_INVOCATION_GATES
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    for site in &scan.sites {
        if site.entry.starts_with("src/gateway/") {
            assert!(
                gateway_gates.contains(site.entry.as_str()),
                "{} line {} calls a tool directly as `{}`. The gateway MCP endpoint was routed \
                 through ToolExecutionService in 0.8.126 precisely so an external caller cannot \
                 skip policy, approval, schema validation and audit; a new direct gateway call \
                 reopens that hole unless GATEWAY_DIRECT_TOOL_INVOCATION_GATES documents the gate \
                 chain that replaces it",
                site.entry,
                site.line,
                site.form
            );
        }
        assert_ne!(
            site.entry, "src/agent/agent.rs::execute_tool_call",
            "Agent::execute_tool_call was the second execution path: it advertised a tool schema \
             to the model and then enforced none of it, with no policy decision and no audit \
             record. It was routed through ToolExecutionService in 0.8.128"
        );
    }

    let mut found: Vec<String> = scan.sites.into_iter().map(|site| site.entry).collect();
    found.sort();
    found.dedup();

    let expected: Vec<&str> = ALLOWED_DIRECT_TOOL_INVOCATIONS
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    assert_inventory(
        "direct Tool execution call sites outside ToolExecutionService",
        found,
        &expected,
    );
}

/// Every gateway entry documents a gate chain, and every documented gate chain
/// still describes a call site that exists.
#[test]
fn every_gateway_tool_execution_documents_its_alternative_gate() {
    let scan = scan_repository_tool_executions();
    let gateway_sites: BTreeSet<String> = scan
        .sites
        .iter()
        .filter(|site| site.entry.starts_with("src/gateway/"))
        .map(|site| site.entry.clone())
        .collect();

    for (entry, reason) in GATEWAY_DIRECT_TOOL_INVOCATION_GATES {
        assert!(
            gateway_sites.contains(*entry),
            "{entry} is documented as a gated direct tool execution but no such call site exists \
             any more; remove the entry instead of leaving a standing exemption"
        );
        assert!(
            reason.contains("gate chain:"),
            "{entry} must name the gate chain that replaces ToolExecutionService"
        );
    }

    let documented: BTreeSet<&str> = GATEWAY_DIRECT_TOOL_INVOCATION_GATES
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    let mut undocumented: Vec<&String> = gateway_sites
        .iter()
        .filter(|entry| !documented.contains(entry.as_str()))
        .collect();
    undocumented.sort();
    assert!(
        undocumented.is_empty(),
        "gateway tool executions without a documented gate chain: {undocumented:?}"
    );
}

/// Every allowlisted entry carries a reason, and no entry is listed twice.
#[test]
fn direct_tool_invocation_allowlist_entries_carry_a_reason() {
    let mut seen = BTreeSet::new();
    for (entry, reason) in ALLOWED_DIRECT_TOOL_INVOCATIONS {
        assert!(
            seen.insert(*entry),
            "{entry} is listed twice in ALLOWED_DIRECT_TOOL_INVOCATIONS"
        );
        assert!(
            reason.len() > 20,
            "{entry} needs a real reason, not {reason:?}: the allowlist is the architecture record"
        );
    }
}

/// The non-tool receiver list is the only escape hatch in the scanner, so it is
/// held to the same standard as the allowlist: every entry must still match a
/// real call site, or it is a wildcard waiting for a tool variable to be named
/// after it.
#[test]
fn non_tool_execute_receivers_are_all_still_in_use() {
    let scan = scan_repository_tool_executions();
    let mut stale: Vec<&str> = NON_TOOL_EXECUTE_RECEIVERS
        .iter()
        .map(|(receiver, _)| *receiver)
        .filter(|receiver| !scan.excluded_receivers.contains(*receiver))
        .collect();
    stale.sort_unstable();
    assert!(
        stale.is_empty(),
        "these receivers no longer name any `.execute(` call site and must be deleted from \
         NON_TOOL_EXECUTE_RECEIVERS: {stale:?}"
    );
    for (receiver, reason) in NON_TOOL_EXECUTE_RECEIVERS {
        assert!(
            reason.len() > 20,
            "{receiver} needs a reason naming the type whose `execute` this is, not {reason:?}"
        );
    }
}

/// The forms the scanner claims to cover, proven one at a time against a
/// synthetic gateway file. Each snippet is the mutation a future bypass would
/// look like; if any of them stops being reported, the guard has a blind spot
/// and the real-file mutation runs in the receipt would pass for the wrong
/// reason.
#[test]
fn the_scanner_reports_every_supported_tool_execution_form() {
    let forms: &[(&str, &str)] = &[
        ("method", "let result = tool.execute(args).await;"),
        (
            "cancellation twin",
            "let result = tool.execute_with_cancellation(args, None).await;",
        ),
        ("named", "let result = tool.execute_named(name, args).await;"),
        (
            "named cancellation twin",
            "let result = tool.execute_named_with_cancellation(name, args, None).await;",
        ),
        (
            "UFCS type path",
            "let result = MessageSendTool::execute(&tool, args).await;",
        ),
        ("UFCS trait path", "let result = Tool::execute(&tool, args).await;"),
        (
            "UFCS qualified path",
            "let result = <MessageSendTool as Tool>::execute(&tool, args).await;",
        ),
        (
            "multi-line chain",
            "let result = state\n        .tool\n        .execute(args)\n        .await;",
        ),
        (
            "multi-line UFCS",
            "let result = <MessageSendTool as Tool>::execute(\n        &tool,\n        args,\n    )\n    .await;",
        ),
        (
            "whitespace before the argument list",
            "let result = tool.execute (args).await;",
        ),
    ];

    for (label, snippet) in forms {
        let source = format!("pub async fn post_channel_send() {{\n    {snippet}\n}}\n");
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        assert_eq!(
            scan.sites.len(),
            1,
            "the {label} form was not inventoried: {snippet:?} produced {:?}",
            scan.sites
        );
        assert_eq!(
            scan.sites[0].entry, "src/gateway/api/channels.rs::post_channel_send",
            "the {label} form was attributed to the wrong function"
        );
    }
}

/// The scanner must not invent call sites out of prose, and must not stop
/// scanning a file because a literal mentioned a brace or a quote.
#[test]
fn the_scanner_ignores_comments_literals_and_test_modules() {
    let source = r##"
/// Doc comment: this endpoint used to call tool.execute(args) directly.
pub async fn post_channel_send() {
    // tool.execute(args)
    let sql = "tool.execute(args)";
    let raw = r#"tool.execute_named("send", args)"#;
    let brace = '{';
    let _ = (sql, raw, brace);
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn direct_call_is_fine_in_a_test() {
        let _ = tool.execute(args).await;
    }
}
"##;
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    assert!(
        scan.sites.is_empty(),
        "comments, literals and `#[cfg(test)]` modules must not be inventoried: {:?}",
        scan.sites
    );

    let real = format!("{source}\npub async fn later() {{\n    let _ = tool.execute(args).await;\n}}\n");
    let scan = scan_tool_executions("src/gateway/api/channels.rs", &real);
    assert_eq!(
        scan.sites.len(),
        1,
        "a literal must not end the scan early: {:?}",
        scan.sites
    );
}

/// The escape hatch is exactly as wide as `NON_TOOL_EXECUTE_RECEIVERS` says.
#[test]
fn only_listed_non_tool_receivers_escape_the_inventory() {
    let source =
        "pub async fn handler() {\n    conn.execute(sql, params)?;\n    let _ = database.execute(sql, params)?;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    assert_eq!(
        scan.sites.len(),
        1,
        "an unlisted receiver must fail closed and appear in the inventory: {:?}",
        scan.sites
    );
    assert_eq!(scan.sites[0].form, "database.execute(...)");
    assert!(scan.excluded_receivers.contains("conn"));
}

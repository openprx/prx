//! Regression guards for architecture boundaries that are not expressed by Rust's type system.
//!
//! These allowlists intentionally describe the current tree. A new entry is a review event:
//! route the behavior through the owning repository/adapter/event fabric, or update the
//! allowlist with an explicit architecture decision.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
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
    // Fourth occurrence: the pre-v16 `memory_events` fixture in
    // `legacy_memory_events_gets_owner_id_column_on_init`, which proves the
    // C03 owner_id migration upgrades a database created before the column
    // existed. A test fixture, not a second production ledger.
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

/// Types whose `execute(...)` is not `Tool::execute`, with the crate or module
/// each one comes from.
///
/// Rust's `execute` is a popular method name: `rusqlite`, `tokio-postgres`, the
/// shell process adapter, the chat effect executor, the xin builtin registry and
/// `ToolExecutionService` itself all have one. The scanner has no type
/// information, so this table *is* the type information. It is consulted only
/// against the receiver's own binding inside the enclosing function - never
/// against a bare identifier - so naming a `Tool` variable `conn` no longer
/// hides it.
const NON_TOOL_RECEIVER_TYPES: &[(&str, &str)] = &[
    (
        "BuiltinRegistry",
        "xin `BuiltinRegistry::execute(payload, config)` - internal task builtins",
    ),
    ("Client", "tokio-postgres `Client::execute(statement, params)`"),
    ("Connection", "rusqlite `Connection::execute(sql, params)`"),
    (
        "EffectExecutor",
        "chat `EffectExecutor::execute(effect)` - terminal UI effects, not tools",
    ),
    (
        "GenericClient",
        "the tokio-postgres trait shared by `Client` and `Transaction`",
    ),
    (
        "PooledConnection",
        "a checked-out tokio-postgres client from the memory pool",
    ),
    (
        "PostgresConnectionPool",
        "the tokio-postgres pool wrapper, which forwards `execute`",
    ),
    ("ShellProcessAdapter", "the shared child-process seam"),
    ("Transaction", "rusqlite / tokio-postgres transaction handle"),
    (
        "ToolExecutionService",
        "the approved execution path itself: policy, approval, schema check and audit",
    ),
];

/// Chain methods that only attach context to an error and cannot change the
/// type of the value a producer expression yields.
const PRODUCER_INERT_TAIL_METHODS: &[&str] = &[".context(", ".with_context(", ".map_err("];

/// Initialiser expressions that produce one of [`NON_TOOL_RECEIVER_TYPES`].
///
/// A `let` binding with no type annotation carries its type in the expression
/// that built it. Each entry is the *whole* initialiser in canonical form
/// ([`producer_shape`]: whitespace removed, argument lists that carry no nested
/// call reduced to `..`, the inert error-context tail and `?` stripped), and it
/// is compared for equality, never as a substring. The receiver is part of the
/// entry for that reason: `.transaction()` on its own would have exempted
/// `let handle = self.transaction();` whatever `self.transaction()` returns,
/// which is the same defect as exempting a receiver by its name. Nothing here
/// matches a plain move of an existing value, so `let conn = some_tool;` stays
/// a reviewable tool execution.
///
/// Associated functions of a declared non-tool type - `Connection::open(..)` -
/// are recognised by [`constructor_producer_type`] instead and are not listed.
const NON_TOOL_RECEIVER_PRODUCERS: &[(&str, &str)] = &[
    (
        "checkout.client_mut()?.transaction()",
        "the webhook store's pooled tokio-postgres checkout -> Transaction",
    ),
    (
        "client.build_transaction().read_only(..).start()",
        "the memory Postgres read-only transaction builder -> Transaction",
    ),
    (
        "client.transaction()",
        "tokio-postgres `Client::transaction()` -> Transaction",
    ),
    (
        "conn.transaction()",
        "rusqlite `Connection::transaction()` -> Transaction",
    ),
    (
        "conn.transaction_with_behavior(..)",
        "rusqlite `transaction_with_behavior()` -> Transaction",
    ),
    (
        "conn.unchecked_transaction()",
        "rusqlite `unchecked_transaction()` -> Transaction",
    ),
    (
        "open_approval_ledger(..)",
        "the approval ledger opener -> rusqlite Connection",
    ),
    (
        "pool.write()",
        "the `parking_lot` write guard over the shared rusqlite Connection",
    ),
    (
        "self.client.clone()",
        "the memory Postgres pool's own tokio-postgres Client handle",
    ),
    (
        "self.conn()",
        "the xin and cron store's rusqlite connection accessor -> Connection",
    ),
    (
        "self.conn.lock()",
        "the webhook store's `parking_lot` guard over its rusqlite Connection",
    ),
    (
        "self.conn.transaction()",
        "the store's own rusqlite Connection field -> Transaction",
    ),
    (
        "reader.connection()",
        "the memory SQLite snapshot reader's rusqlite Connection accessor",
    ),
    (
        "to_store_err!(conn.transaction())",
        "the WhatsApp store's error-mapping macro around `Connection::transaction()`",
    ),
];

/// Helpers that hand a non-`Tool` handle to a closure, and the type each one
/// passes.
///
/// `with_connection(config, |conn| conn.execute(...))` leaves the closure
/// parameter untyped, so the type lives in the helper's own signature. Each
/// entry is checked against that signature by
/// `non_tool_closure_providers_match_their_definitions`, so renaming or
/// retyping a helper fails the guard instead of silently widening it.
const NON_TOOL_CLOSURE_PROVIDERS: &[(&str, &str, &str)] = &[
    (
        "with_client",
        "Client",
        "the memory and cron Postgres stores hand the pooled tokio-postgres client to a closure",
    ),
    (
        "with_connection",
        "Connection",
        "the cron and xin SQLite stores hand the rusqlite connection to a closure",
    ),
    (
        "with_immediate_connection",
        "Connection",
        "the xin SQLite store's BEGIN IMMEDIATE variant of `with_connection`",
    ),
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
///
/// Each entry carries how many direct `Tool` executions that function contains.
/// The guard compares `(entry, count)`, not the set of entries: a *second*
/// direct call added to a function that is already listed used to disappear into
/// the deduplicated entry name, which is the same blind spot as exempting a
/// receiver by its name.
const ALLOWED_DIRECT_TOOL_INVOCATIONS: &[(&str, usize, &str)] = &[
    (
        "src/agent/agent.rs::spawn_delegate_task",
        1,
        "fire-and-forget delegate spawn from the legacy `Agent::run_single` shell; \
         `Agent::execute_tool_call` — the shell's actual tool path — was routed through the \
         service in 0.8.128 and must not come back here",
    ),
    (
        "src/chat/mod.rs::enrich_file_mentions_for_prompt",
        1,
        "operator-typed `@file` mention expansion; the argument document is built by this code, \
         not by a model",
    ),
    (
        "src/chat/mod.rs::kill_local_session",
        1,
        "operator-typed `/kill`; the human at the keyboard is the authority",
    ),
    (
        "src/chat/mod.rs::run",
        4,
        "operator-typed `/spawn`, `/steer` and `!command`; the human at the keyboard is the \
         authority",
    ),
    (
        "src/gateway/api/channels.rs::post_channel_send",
        1,
        "operator-plane send with its own gate chain — see GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/gateway/api/config.rs::post_config_reload",
        1,
        "operator-plane config reload with its own gate chain — see \
         GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/plugins/runtime.rs::dispatch_alias",
        1,
        "decorator: the WASM plugin runtime dispatches a plugin alias onto the plugin tool it \
         already resolved",
    ),
    (
        "src/plugins/runtime.rs::execute_named",
        1,
        "decorator: the WASM plugin tool forwards its named entry to the resolved plugin tool",
    ),
    (
        "src/tools/delegate.rs::execute",
        2,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named",
        1,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named_with_cancellation",
        1,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_with_cancellation",
        2,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/execution.rs::invoke",
        2,
        "the service's own legacy per-call adapter — this is `ToolExecutionService` reaching its \
         backend after every gate has run",
    ),
    (
        "src/tools/mcp.rs::execute",
        1,
        "decorator: the `mcp_call` root alias forwards to its own named entry",
    ),
    (
        "src/tools/mod.rs::execute",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named_with_cancellation",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_with_cancellation",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/sessions_spawn.rs::execute",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_named",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_named_with_cancellation",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn.rs::execute_with_cancellation",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/skills.rs::execute",
        1,
        "decorator: skill aliases forward to the skill runner",
    ),
    (
        "src/tools/skills.rs::execute_named",
        1,
        "decorator: skill aliases forward to the skill runner",
    ),
    (
        "src/tools/skills.rs::execute_resolved",
        1,
        "decorator: an `http` skill is run by handing the request this code built to the \
         `HttpRequestTool` the skill tool owns; the service already gated the outer `skill_execute`",
    ),
    (
        "src/tools/traits.rs::execute_named",
        1,
        "the `Tool` trait's own default bodies: `execute_named` falls back to `execute`",
    ),
    (
        "src/tools/traits.rs::execute_named_with_cancellation",
        3,
        "the `Tool` trait's own default bodies: the cancellation twin routes to the named entry",
    ),
    (
        "src/tools/traits.rs::execute_with_cancellation",
        2,
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

/// The receiver expression as it is written: `self.tool_execution`, `state.tool`,
/// `conn`.
///
/// The bare identifier is not enough to review a call site — `conn` could be a
/// pooled database handle or a field holding a `Tool` — so the scanner keeps the
/// dotted path that produced it. Anything it cannot reduce to a dotted
/// identifier path (a call, an index, `?`) falls back to
/// [`receiver_identifier`], which collapses those to `<expression>`.
fn receiver_expression(characters: &[char], dot: usize) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut index = dot;
    loop {
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        if index == 0 || !is_ident_char(characters[index - 1]) {
            break;
        }
        let mut start = index - 1;
        while start > 0 && is_ident_char(characters[start - 1]) {
            start -= 1;
        }
        segments.push(characters[start..index].iter().collect());
        let mut joint = start;
        while joint > 0 && characters[joint - 1].is_whitespace() {
            joint -= 1;
        }
        if joint == 0 || characters[joint - 1] != '.' {
            break;
        }
        index = joint - 1;
    }
    if segments.is_empty() {
        return receiver_identifier(characters, dot);
    }
    segments.reverse();
    segments.join(".")
}

/// Whether the receiver of the call at `dot` is a plain local name - something
/// a `let`, a parameter or a closure parameter in this function could have
/// bound - rather than a field, an index or the result of another call.
///
/// `conn.execute(..)` is a local name. `self.conn.execute(..)`,
/// `state.tool.execute(..)` and `checkout.client_mut()?.execute(..)` are not:
/// their type lives on a struct or on another function's return type, so the
/// binding rule has nothing to say about them and they fall through to the
/// reviewed site exceptions.
fn receiver_is_local_binding(characters: &[char], dot: usize) -> bool {
    let mut index = dot;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 || !is_ident_char(characters[index - 1]) {
        return false;
    }
    let mut start = index - 1;
    while start > 0 && is_ident_char(characters[start - 1]) {
        start -= 1;
    }
    let mut before = start;
    while before > 0 && characters[before - 1].is_whitespace() {
        before -= 1;
    }
    if before == 0 {
        return true;
    }
    if matches!(characters[before - 1], '.' | ')' | ']' | '?') {
        return false;
    }
    // A path segment (`crate::THING.execute(..)`), not a local. A single colon
    // is a macro label or a struct field and says nothing either way.
    !(characters[before - 1] == ':' && before >= 2 && characters[before - 2] == ':')
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

/// The production text of the function that contains `line_index`.
///
/// The same backwards scan [`enclosing_function`] uses to name the function,
/// followed forwards to the closing brace, so the binding rule below reads
/// exactly the body the call site sits in.
fn enclosing_function_body(lines: &[&str], line_index: usize) -> Option<String> {
    let mut start = None;
    for (offset, line) in lines[..=line_index].iter().enumerate().rev() {
        let Some(fn_offset) = line.find("fn ") else {
            continue;
        };
        let name: String = line[fn_offset + 3..]
            .chars()
            .take_while(|character| is_ident_char(*character))
            .collect();
        if !name.is_empty() {
            start = Some(offset);
            break;
        }
    }
    let start = start?;
    let text = lines[start..].join("\n");
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, character) in text.char_indices() {
        match character {
            '{' => {
                depth += 1;
                opened = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    return Some(text[..=offset].to_string());
                }
            }
            _ => {}
        }
    }
    Some(text)
}

/// Read a type annotation starting at `from`, stopping at the first `stops`
/// character that is not nested inside `<>`, `()` or `[]`.
fn type_annotation(characters: &[char], from: usize, stops: &[char]) -> String {
    let mut depth = 0i32;
    let mut text = String::new();
    for character in characters.iter().skip(from) {
        if depth <= 0 && stops.contains(character) {
            break;
        }
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            _ => {}
        }
        text.push(*character);
        if text.len() > 200 {
            break;
        }
    }
    normalize_line(&text)
}

/// Read an initialiser expression starting at `from`, stopping at the statement
/// terminator that is not nested inside a bracket.
fn expression_text(characters: &[char], from: usize) -> String {
    let mut depth = 0i32;
    let mut text = String::new();
    for character in characters.iter().skip(from) {
        match character {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ';' if depth <= 0 => break,
            _ => {}
        }
        text.push(*character);
        if text.len() > 400 {
            break;
        }
    }
    normalize_line(&text)
}

/// Whether the identifier at `position` sits on the binding side of a `let`,
/// `if let` or `while let` - a destructuring pattern - rather than in the
/// initialiser or in an ordinary call argument.
fn is_let_pattern_position(characters: &[char], position: usize) -> bool {
    let mut index = position;
    while index > 0 {
        let character = characters[index - 1];
        if character == ';' || character == '{' || character == '}' {
            return false;
        }
        if character == '=' {
            let previous = if index >= 2 { characters[index - 2] } else { ' ' };
            let next = characters.get(index).copied().unwrap_or(' ');
            if !matches!(next, '=' | '>') && !matches!(previous, '=' | '!' | '<' | '>') {
                return false;
            }
        }
        if is_ident_char(character) {
            let mut word_start = index - 1;
            while word_start > 0 && is_ident_char(characters[word_start - 1]) {
                word_start -= 1;
            }
            let word: String = characters[word_start..index].iter().collect();
            if word == "let" {
                return true;
            }
            index = word_start;
            continue;
        }
        index -= 1;
    }
    false
}

/// The callee of the call whose argument list contains `position`.
///
/// `with_connection(config, |conn| ...)` resolves to `with_connection`, which is
/// how an untyped closure parameter recovers a type.
fn enclosing_call_name(characters: &[char], position: usize) -> Option<String> {
    let mut depth = 0i32;
    let mut index = position;
    while index > 0 {
        index -= 1;
        match characters[index] {
            ')' | ']' => depth += 1,
            '(' | '[' if depth > 0 => depth -= 1,
            '(' => {
                let mut end = index;
                while end > 0 && characters[end - 1].is_whitespace() {
                    end -= 1;
                }
                if end == 0 || !is_ident_char(characters[end - 1]) {
                    return None;
                }
                let mut start = end;
                while start > 0 && is_ident_char(characters[start - 1]) {
                    start -= 1;
                }
                return Some(characters[start..end].iter().collect());
            }
            '[' | '{' | '}' | ';' => return None,
            _ => {}
        }
    }
    None
}

/// What one occurrence of the receiver identifier says about the receiver's
/// type, or `None` when the occurrence is a use rather than a binding.
///
/// A binding form the scanner cannot read yields an `<unparsed ...>` marker that
/// no table approves, so the call site falls through to review instead of being
/// guessed at.
fn classify_receiver_binding(characters: &[char], start: usize, end: usize) -> Option<String> {
    let mut before = start;
    let mut keyword: Option<String> = None;
    loop {
        while before > 0 && characters[before - 1].is_whitespace() {
            before -= 1;
        }
        if before == 0 {
            return None;
        }
        if !is_ident_char(characters[before - 1]) {
            break;
        }
        let mut word_start = before - 1;
        while word_start > 0 && is_ident_char(characters[word_start - 1]) {
            word_start -= 1;
        }
        let candidate: String = characters[word_start..before].iter().collect();
        if candidate == "mut" || candidate == "ref" {
            before = word_start;
            continue;
        }
        keyword = Some(candidate);
        break;
    }

    let mut after = end;
    while after < characters.len() && characters[after].is_whitespace() {
        after += 1;
    }
    let annotated = characters.get(after) == Some(&':') && characters.get(after + 1) != Some(&':');

    if let Some(keyword) = keyword {
        return match keyword.as_str() {
            "let" => Some(if annotated {
                format!("let type: {}", type_annotation(characters, after + 1, &['=', ';']))
            } else if characters.get(after) == Some(&'=') {
                format!("let value: {}", expression_text(characters, after + 1))
            } else {
                "<unparsed let binding>".to_string()
            }),
            "for" => Some("<for-loop binding>".to_string()),
            _ => None,
        };
    }

    let previous = characters[before - 1];
    match previous {
        '(' | ',' if annotated => Some(format!(
            "parameter: {}",
            type_annotation(characters, after + 1, &[',', ')', '|'])
        )),
        '|' if annotated => Some(format!(
            "closure parameter: {}",
            type_annotation(characters, after + 1, &[',', '|'])
        )),
        '|' if before < 2 || characters[before - 2] != '|' => {
            Some(enclosing_call_name(characters, before - 1).map_or_else(
                || "<untyped closure parameter>".to_string(),
                |callee| format!("closure parameter of {callee}"),
            ))
        }
        '(' | ',' if is_let_pattern_position(characters, before - 1) => Some("<unparsed let binding>".to_string()),
        _ => None,
    }
}

/// Every binding of `name` the scanner can see inside one function body.
fn receiver_bindings(body: &str, name: &str) -> Vec<String> {
    let characters: Vec<char> = body.chars().collect();
    let mut bindings = Vec::new();
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
        if characters[index..end].iter().collect::<String>() == name
            && let Some(binding) = classify_receiver_binding(&characters, index, end)
        {
            bindings.push(binding);
        }
        index = end;
    }
    bindings
}

/// The bare type name a type annotation resolves to: `&mut Connection`,
/// `&rusqlite::Transaction<'_>` and `PooledConnection<'_>` all reduce to one
/// identifier, and a wrapper such as `Arc<dyn Tool>` reduces to `Arc`, which no
/// table approves.
fn base_type_identifier(annotation: &str) -> String {
    let mut text = annotation.trim();
    loop {
        let stripped = text.trim_start_matches('&').trim_start();
        let stripped = stripped.strip_prefix("mut ").unwrap_or(stripped).trim_start();
        let stripped = stripped.strip_prefix("dyn ").unwrap_or(stripped).trim_start();
        let stripped = if stripped.starts_with('\'') {
            stripped
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest)
                .trim_start()
        } else {
            stripped
        };
        if stripped == text {
            break;
        }
        text = stripped;
    }
    let head: String = text
        .chars()
        .take_while(|character| is_ident_char(*character) || *character == ':')
        .collect();
    head.rsplit("::").next().unwrap_or(&head).to_string()
}

/// One `let` initialiser in canonical form.
///
/// Whitespace is removed so a multi-line chain reads like a single-line one, and
/// every argument list is reduced to `..` unless it contains another call, in
/// which case the nested call is kept and canonicalised too. The result is the
/// *shape* of the expression: which receiver, which methods, in which order -
/// everything that decides the type, and nothing that does not.
fn producer_shape(initialiser: &str) -> String {
    let characters: Vec<char> = initialiser.chars().filter(|c| !c.is_whitespace()).collect();
    let mut shape = String::new();
    let mut index = 0usize;
    while index < characters.len() {
        if characters[index] != '(' {
            shape.push(characters[index]);
            index += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut end = index;
        let mut closed = false;
        while end < characters.len() {
            match characters[end] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        if !closed {
            // Truncated or unbalanced: nothing can match it, which is the safe
            // direction.
            shape.push_str("(<unbalanced>");
            return shape;
        }
        let inner: String = characters[index + 1..end].iter().collect();
        shape.push('(');
        if inner.contains('(') {
            shape.push_str(&producer_shape(&inner));
        } else if !inner.is_empty() {
            shape.push_str("..");
        }
        shape.push(')');
        index = end + 1;
    }
    shape
}

/// Whether `text` opens with `(` and closes on the matching `)` at its end.
fn is_one_balanced_group(text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    if characters.first() != Some(&'(') {
        return false;
    }
    let mut depth = 0usize;
    for (offset, character) in characters.iter().enumerate() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return offset + 1 == characters.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// Strip the part of a producer shape that cannot change its type: trailing `?`
/// and trailing error-context combinators.
fn strip_inert_producer_tail(shape: &str) -> String {
    let mut text = shape;
    loop {
        if let Some(trimmed) = text.strip_suffix('?') {
            text = trimmed;
            continue;
        }
        let mut cut = None;
        for method in PRODUCER_INERT_TAIL_METHODS {
            let Some(position) = text.rfind(method) else {
                continue;
            };
            let arguments = &text[position + method.len() - 1..];
            if is_one_balanced_group(arguments) {
                cut = Some(cut.map_or(position, |current: usize| current.min(position)));
            }
        }
        match cut {
            Some(position) => text = &text[..position],
            None => break,
        }
    }
    text.to_string()
}

/// The non-tool type a producer shape names when it is an associated function
/// of one: `Connection::open(..)`,
/// `crate::tools::ToolExecutionService::from_shared_boxed_registry(..)`.
///
/// The shape has to be that one call and nothing else, so
/// `MessageSendTool::new(..).with_channels(..)` and a chain that turns the
/// result into something else both fall through to review.
fn constructor_producer_type(shape: &str) -> Option<&'static str> {
    let (head, arguments) = shape.split_once('(')?;
    if head.contains('.') || !head.contains("::") {
        return None;
    }
    if !is_one_balanced_group(&format!("({arguments}")) {
        return None;
    }
    let mut segments: Vec<&str> = head.split("::").collect();
    segments.pop()?;
    let owner = segments.pop()?;
    NON_TOOL_RECEIVER_TYPES
        .iter()
        .find(|(name, _)| *name == owner)
        .map(|(name, _)| *name)
}

/// The non-`Tool` type one binding proves, if any.
fn non_tool_binding_type(binding: &str) -> Option<&'static str> {
    if let Some(annotation) = binding
        .strip_prefix("parameter: ")
        .or_else(|| binding.strip_prefix("closure parameter: "))
        .or_else(|| binding.strip_prefix("let type: "))
    {
        let base = base_type_identifier(annotation);
        return NON_TOOL_RECEIVER_TYPES
            .iter()
            .find(|(name, _)| *name == base)
            .map(|(name, _)| *name);
    }
    if let Some(callee) = binding.strip_prefix("closure parameter of ") {
        return NON_TOOL_CLOSURE_PROVIDERS
            .iter()
            .find(|(name, parameter, _)| {
                *name == callee && NON_TOOL_RECEIVER_TYPES.iter().any(|(known, _)| known == parameter)
            })
            .map(|(_, parameter, _)| *parameter);
    }
    if let Some(value) = binding.strip_prefix("let value: ") {
        let shape = strip_inert_producer_tail(&producer_shape(value));
        if let Some(owner) = constructor_producer_type(&shape) {
            return Some(owner);
        }
        return NON_TOOL_RECEIVER_PRODUCERS
            .iter()
            .find(|(pattern, _)| *pattern == shape)
            .map(|(pattern, _)| *pattern);
    }
    None
}

/// The non-`Tool` type every binding of the receiver agrees on, or `None` when
/// the receiver has no readable binding or any one of its bindings is not
/// provably a non-`Tool` type.
///
/// "Every binding" is what makes shadowing visible: a function that binds `conn`
/// to a `Connection` and later rebinds it to a `Tool` has one unprovable
/// binding, so both of its call sites return to the inventory.
fn proven_non_tool_receiver(bindings: &[String]) -> Option<String> {
    if bindings.is_empty() {
        return None;
    }
    let mut proofs: Vec<&'static str> = Vec::with_capacity(bindings.len());
    for binding in bindings {
        proofs.push(non_tool_binding_type(binding)?);
    }
    proofs.sort_unstable();
    proofs.dedup();
    Some(proofs.join(", "))
}

/// One `execute(...)` call the scanner found in production source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolExecutionSite {
    /// `<file>::<enclosing function>`, the inventory key.
    entry: String,
    /// The call form, for the failure message: `tool.execute(...)`, UFCS, and so on.
    form: String,
    /// The receiver identifier, empty for a UFCS call that has no receiver.
    receiver: String,
    /// The receiver expression as written, e.g. `self.tool_execution`.
    receiver_path: String,
    /// Every binding of the receiver identifier inside the enclosing function.
    bindings: Vec<String>,
    /// The non-`Tool` type the receiver's own bindings prove, when they do.
    non_tool_binding: Option<String>,
    line: usize,
}

/// Everything one file's production source says about tool execution.
#[derive(Debug, Default)]
struct ToolExecutionScan {
    sites: Vec<ToolExecutionSite>,
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
        let (receiver, receiver_path, form) = if before > 0 && characters[before - 1] == '.' {
            let path = receiver_expression(&characters, before - 1);
            // Only a receiver that *is* a local binding may be looked up among
            // the function's bindings. `self.conn` and `state.tool` are fields
            // and `checkout.client_mut()?` is a call: a same-named local in the
            // same function says nothing about them, and consulting one is how
            // an audit probe got a `Tool` on `self.conn` waved through by a
            // `conn: &Connection` parameter sitting beside it.
            let receiver = if receiver_is_local_binding(&characters, before - 1) {
                receiver_identifier(&characters, before - 1)
            } else {
                String::new()
            };
            let form = format!("{path}.{name}(...)");
            (receiver, path, form)
        } else if before > 1 && characters[before - 1] == ':' && characters[before - 2] == ':' {
            let segment = qualified_path_segment(&characters, before - 2);
            let form = format!("{segment}::{name}(...)");
            // A UFCS call has no receiver binding to reason about: every
            // `Path::execute(...)` in this tree is a tool execution, and a new
            // non-tool one is a review event rather than something the scanner
            // may guess about.
            (String::new(), String::new(), form)
        } else {
            index = end;
            continue;
        };

        let line = newlines.partition_point(|offset| *offset < index);
        let bindings = if receiver.is_empty() {
            Vec::new()
        } else {
            enclosing_function_body(&lines, line)
                .map(|body| receiver_bindings(&body, &receiver))
                .unwrap_or_default()
        };
        let non_tool_binding = proven_non_tool_receiver(&bindings);
        scan.sites.push(ToolExecutionSite {
            entry: format!("{relative}::{}", enclosing_function(&lines, line)),
            form,
            receiver,
            receiver_path,
            bindings,
            non_tool_binding,
            line: line + 1,
        });
        index = end;
    }
    scan
}

/// One reviewed `execute(...)` call site whose receiver is not a `Tool` and
/// whose binding the scanner cannot read on its own.
///
/// This is the whole of the escape hatch that used to be a list of bare
/// identifiers. An entry pins the file, the enclosing function, the receiver
/// expression *as written* and how many `execute*` calls on that receiver the
/// function contains, and it points at the source fragment that names the
/// receiver's real type. Nothing here covers a file or a function wholesale: a
/// second call on the same receiver, a differently spelled receiver, or the same
/// call moved to another function all fail the guard.
#[derive(Debug, Clone, Copy)]
struct NonToolCallSite {
    /// `<file>::<enclosing function>`, exactly as the scanner reports it.
    entry: &'static str,
    /// The receiver expression, exactly as the scanner reports it.
    receiver: &'static str,
    /// How many unproven `execute*` calls on that receiver the entry contains.
    occurrences: usize,
    /// The file holding the declaration that gives the receiver its type.
    evidence_file: &'static str,
    /// That declaration, verbatim. It must name a [`NON_TOOL_RECEIVER_TYPES`]
    /// type, and it must still be in the source.
    evidence: &'static str,
    reason: &'static str,
}

/// The non-`Tool` `execute(...)` call sites that survive the binding rule.
///
/// Field receivers (`self.process`) have no binding inside the function, and a
/// `let Some(x) = ... else` pattern or a generic parameter carries its type
/// somewhere the scanner does not read. Each one is pinned here instead of being
/// waved through by its name.
const NON_TOOL_EXECUTE_CALL_SITES: &[NonToolCallSite] = &[
    NonToolCallSite {
        entry: "src/agent/agent.rs::execute_tool_call",
        receiver: "self.tool_execution",
        occurrences: 1,
        evidence_file: "src/agent/agent.rs",
        evidence: "tool_execution: Arc<crate::tools::ToolExecutionService>",
        reason: "the approved path itself: `Agent::execute_tool_call` was routed through \
                 ToolExecutionService in 0.8.128 and this is that call",
    },
    NonToolCallSite {
        entry: "src/agent/loop_.rs::execute_one_unrecorded",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/agent/loop_.rs",
        evidence: "pub tool_execution_service: Option<Arc<crate::tools::ToolExecutionService>>",
        reason: "`let Some(service) = adapter.tool_execution_service.as_ref() else` - the tool \
                 loop's own handle on the approved execution service",
    },
    NonToolCallSite {
        entry: "src/chat/dispatcher.rs::execute_single_tool_call",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/chat/dispatcher.rs",
        evidence: "service: Option<&ToolExecutionService>",
        reason: "the Redux dispatcher's tool call goes through the service parameter it was \
                 handed; `let Some(service) = service else` only unwraps the Option",
    },
    NonToolCallSite {
        entry: "src/memory/postgres.rs::acquire_reset",
        receiver: "client",
        occurrences: 1,
        evidence_file: "src/memory/postgres.rs",
        evidence: "pub(crate) fn client_mut(&mut self) -> Result<&mut Client>",
        reason: "`checkout.client_mut().and_then(|client| client.execute(...))` resets the \
                 PostgreSQL session context on a pooled tokio-postgres client",
    },
    NonToolCallSite {
        entry: "src/memory/postgres.rs::apply_rls_context_raw",
        receiver: "client",
        occurrences: 4,
        evidence_file: "src/memory/postgres.rs",
        evidence: "fn apply_rls_context_raw<C: GenericClient>",
        reason: "binds the row-level-security context on a generic tokio-postgres client or \
                 transaction; the four calls are the four SET LOCAL statements",
    },
    NonToolCallSite {
        entry: "src/tools/shell.rs::execute_inner",
        receiver: "self.process",
        occurrences: 1,
        evidence_file: "src/tools/shell.rs",
        evidence: "process: ShellProcessAdapter",
        reason: "the shell tool's own child-process seam: this is the adapter call the shell \
                 tool exists to make, not a second tool execution",
    },
    NonToolCallSite {
        entry: "src/tools/skills.rs::execute_resolved",
        receiver: "self.shell",
        occurrences: 1,
        evidence_file: "src/tools/skills.rs",
        evidence: "shell: ShellProcessAdapter",
        reason: "the skills tool runs a resolved skill command through the same shared \
                 child-process adapter; its `self.http` call in the same function stays in the \
                 direct-invocation inventory",
    },
    NonToolCallSite {
        entry: "src/webhook/mod.rs::fail_blocking",
        receiver: "client_mut",
        occurrences: 1,
        evidence_file: "src/memory/postgres.rs",
        evidence: "pub(crate) fn client_mut(&mut self) -> Result<&mut Client>",
        reason: "`checkout.client_mut()?.execute(...)` marks a webhook ingestion failed on the \
                 pooled tokio-postgres client; the SQLite twin of the same function reaches its \
                 rusqlite connection through a proven binding",
    },
];

/// Whether `needle` occurs in `haystack` as a whole identifier.
fn contains_identifier(haystack: &str, needle: &str) -> bool {
    let characters: Vec<char> = haystack.chars().collect();
    let target: Vec<char> = needle.chars().collect();
    if target.is_empty() || target.len() > characters.len() {
        return false;
    }
    (0..=characters.len() - target.len()).any(|start| {
        characters[start..start + target.len()] == target[..]
            && (start == 0 || !is_ident_char(characters[start - 1]))
            && characters
                .get(start + target.len())
                .is_none_or(|character| !is_ident_char(*character))
    })
}

/// Whether one reviewed exception covers one call site: same file, same
/// enclosing function, same receiver expression.
fn exception_covers(exception: &NonToolCallSite, site: &ToolExecutionSite) -> bool {
    (exception.entry, exception.receiver) == (site.entry.as_str(), site.receiver_path.as_str())
}

/// Count, duplication and staleness checks for the reviewed non-`Tool` call
/// sites.
///
/// A pure function so the rules can be exercised against a synthetic inventory
/// as well as against the real tree.
fn non_tool_call_site_errors(sites: &[ToolExecutionSite], exceptions: &[NonToolCallSite]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for exception in exceptions {
        let label = format!("{} / {}", exception.entry, exception.receiver);
        if !seen.insert((exception.entry, exception.receiver)) {
            errors.push(format!("{label} is listed twice"));
        }
        if exception.receiver.is_empty() {
            errors.push(format!(
                "{label} has no receiver: a UFCS call has no binding to review and is never exempt"
            ));
        }
        if exception.occurrences == 0 {
            errors.push(format!(
                "{label} declares zero occurrences; delete the entry instead of leaving a \
                 standing exemption"
            ));
        }
        if exception.reason.len() <= 20 {
            errors.push(format!(
                "{label} needs a real reason naming the type whose `execute` this is, not {:?}",
                exception.reason
            ));
        }
        if exception.evidence.len() <= 10 {
            errors.push(format!(
                "{label} needs a source fragment that names the receiver's type, not {:?}",
                exception.evidence
            ));
        }
        let actual = sites
            .iter()
            .filter(|site| site.non_tool_binding.is_none() && exception_covers(exception, site))
            .count();
        if actual == 0 {
            errors.push(format!(
                "{label} no longer matches any `execute` call site; remove the entry instead of \
                 leaving a standing exemption"
            ));
        } else if actual != exception.occurrences {
            errors.push(format!(
                "{label} declares {} `execute` call(s) but the tree now has {actual}. Every call \
                 on an exempt receiver is reviewed one at a time: if the new call is another \
                 non-tool call raise the count, and if it is a tool call route it through \
                 ToolExecutionService",
                exception.occurrences
            ));
        }
    }
    errors
}

/// The direct `Tool` executions in a scan: every call whose receiver binding
/// does not prove another type, minus the reviewed non-`Tool` sites.
fn direct_tool_execution_sites<'a>(
    sites: &'a [ToolExecutionSite],
    exceptions: &[NonToolCallSite],
) -> Vec<&'a ToolExecutionSite> {
    sites
        .iter()
        .filter(|site| {
            site.non_tool_binding.is_none() && !exceptions.iter().any(|exception| exception_covers(exception, site))
        })
        .collect()
}

fn scan_repository_tool_executions() -> ToolExecutionScan {
    let mut combined = ToolExecutionScan::default();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let scan = scan_tool_executions(&relative, &source);
        combined.sites.extend(scan.sites);
    }
    combined
}

/// One inventory line per enclosing function: `<file>::<function> xN`.
///
/// The count is what stops a *second* direct tool execution inside a function
/// that is already allowlisted from disappearing into the deduplicated entry
/// name.
fn direct_tool_execution_inventory(sites: &[&ToolExecutionSite]) -> Vec<String> {
    let mut counted: BTreeMap<&str, usize> = BTreeMap::new();
    for site in sites {
        *counted.entry(site.entry.as_str()).or_default() += 1;
    }
    counted
        .into_iter()
        .map(|(entry, count)| format!("{entry} x{count}"))
        .collect()
}

/// The autonomy policy, the approval gate, the published-schema check and the
/// audit record all live in `ToolExecutionService::execute`. Anything that calls
/// a `Tool` directly gets none of them, so the set of direct callers is pinned —
/// in every call form, not only the two method names the 0.8.127 guard knew.
#[test]
fn direct_tool_invocations_outside_the_execution_service_are_allowlisted() {
    let scan = scan_repository_tool_executions();
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);

    let gateway_gates: BTreeSet<&str> = GATEWAY_DIRECT_TOOL_INVOCATION_GATES
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    for site in &direct {
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

    let found = direct_tool_execution_inventory(&direct);

    let expected: Vec<String> = ALLOWED_DIRECT_TOOL_INVOCATIONS
        .iter()
        .map(|(entry, count, _)| format!("{entry} x{count}"))
        .collect();
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_inventory(
        "direct Tool execution call sites outside ToolExecutionService (entry and call count)",
        found,
        &expected,
    );
}

/// Every gateway entry documents a gate chain, and every documented gate chain
/// still describes a call site that exists.
#[test]
fn every_gateway_tool_execution_documents_its_alternative_gate() {
    let scan = scan_repository_tool_executions();
    let gateway_sites: BTreeSet<String> = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES)
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
    for (entry, occurrences, reason) in ALLOWED_DIRECT_TOOL_INVOCATIONS {
        assert!(
            seen.insert(*entry),
            "{entry} is listed twice in ALLOWED_DIRECT_TOOL_INVOCATIONS"
        );
        assert!(
            *occurrences > 0,
            "{entry} declares zero direct tool executions; delete the entry instead of leaving a \
             standing exemption"
        );
        assert!(
            reason.len() > 20,
            "{entry} needs a real reason, not {reason:?}: the allowlist is the architecture record"
        );
    }
}

/// The names the 0.8.155 guard exempted globally, whatever they were bound to.
/// Every one of them is now only as exempt as its own binding makes it.
const FORMERLY_EXEMPT_RECEIVERS: &[&str] = &[
    "client",
    "client_mut",
    "conn",
    "executor",
    "process",
    "registry",
    "service",
    "shell",
    "tool_execution",
    "transaction",
    "tx",
];

/// One synthetic handler that holds an approved `Connection` call and a `Tool`
/// call on a receiver named `receiver`.
fn probe_handler(receiver: &str, call: &str) -> String {
    format!(
        "pub async fn post_channel_send(conn: &Connection) {{\n    \
         conn.execute(sql, params)?;\n    \
         let {receiver}: &dyn Tool = resolve();\n    \
         {call}\n}}\n"
    )
}

/// The Mac cross-audit probe, replayed for every name the old guard exempted.
///
/// An explicitly typed `Tool` receiver called `service`, `client`, `conn` or
/// `executor` used to vanish from the inventory because of its spelling alone,
/// even when it sat next to a real database call. It must now be reported.
#[test]
fn a_tool_receiver_named_like_a_non_tool_handle_is_still_inventoried() {
    for receiver in FORMERLY_EXEMPT_RECEIVERS {
        let source = probe_handler(receiver, &format!("let _ = {receiver}.execute(args).await;"));
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        assert!(
            direct.iter().any(|site| site.receiver_path == *receiver),
            "a `&dyn Tool` bound to `{receiver}` escaped the inventory; scan was {:?}",
            scan.sites
        );
        assert!(
            scan.sites
                .iter()
                .any(|site| site.receiver_path == "conn" && site.non_tool_binding.is_some())
                || *receiver == "conn",
            "the approved `&Connection` call in the same function should stay proven"
        );
    }
}

/// The same probe in every call form the scanner claims to cover, so a bypass
/// cannot be spelled around the method name.
#[test]
fn a_tool_receiver_named_like_a_non_tool_handle_is_inventoried_in_every_call_form() {
    let calls: &[(&str, &str)] = &[
        ("method", "let _ = service.execute(args).await;"),
        (
            "cancellation twin",
            "let _ = service.execute_with_cancellation(args, None).await;",
        ),
        ("named", "let _ = service.execute_named(name, args).await;"),
        (
            "named cancellation twin",
            "let _ = service.execute_named_with_cancellation(name, args, None).await;",
        ),
        (
            "multi-line chain",
            "let _ = service\n        .execute(args)\n        .await;",
        ),
    ];
    for (label, call) in calls {
        let source = probe_handler("service", call);
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        assert!(
            direct.iter().any(|site| site.receiver_path == "service"),
            "the {label} form on a `Tool` named `service` escaped the inventory: {:?}",
            scan.sites
        );
    }

    // UFCS has no receiver at all, so no binding and no exception can reach it.
    let ufcs = "pub async fn post_channel_send(conn: &Connection) {\n    \
                conn.execute(sql, params)?;\n    \
                let _ = <ShellTool as Tool>::execute(&service, args).await;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", ufcs);
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    assert!(
        direct.iter().any(|site| site.form.contains("Tool::execute")),
        "a UFCS tool execution escaped the inventory: {:?}",
        scan.sites
    );
}

/// Rebinding a proven receiver to a `Tool` makes the whole receiver unproven,
/// so the shadowed call cannot hide behind the binding that came before it.
#[test]
fn shadowing_a_proven_receiver_with_a_tool_returns_every_call_to_the_inventory() {
    let source = "pub async fn handler(conn: &Connection) {\n    \
                  conn.execute(sql, params)?;\n    \
                  let conn: &dyn Tool = resolve();\n    \
                  let _ = conn.execute(args).await;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    assert_eq!(scan.sites.len(), 2, "both calls must be seen: {:?}", scan.sites);
    assert!(
        scan.sites.iter().all(|site| site.non_tool_binding.is_none()),
        "a function that rebinds `conn` to a Tool has no proven binding left: {:?}",
        scan.sites
    );
    assert_eq!(
        direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES).len(),
        2
    );
}

/// A receiver with no readable binding fails closed: the guard reports it rather
/// than guessing that a name it has seen before means the same type here.
#[test]
fn only_a_proven_non_tool_binding_escapes_the_inventory() {
    let source = "pub async fn handler(conn: &Connection) {\n    \
                  conn.execute(sql, params)?;\n    \
                  let _ = database.execute(sql, params)?;\n    \
                  let _ = tx.execute(sql, params)?;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    let proven: Vec<&str> = scan
        .sites
        .iter()
        .filter(|site| site.non_tool_binding.is_some())
        .map(|site| site.receiver_path.as_str())
        .collect();
    assert_eq!(
        proven,
        vec!["conn"],
        "only the typed parameter is proven: {:?}",
        scan.sites
    );
    let direct: Vec<&str> = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES)
        .iter()
        .map(|site| site.receiver_path.as_str())
        .collect();
    assert_eq!(direct, vec!["database", "tx"]);
}

/// The binding rule reads the forms the tree actually uses, and nothing else.
#[test]
fn the_binding_rule_proves_only_the_declared_non_tool_types() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            "typed parameter",
            "pub fn f(conn: &Connection) {\n    conn.execute(sql, params)?;\n}\n",
            Some("Connection"),
        ),
        (
            "typed closure parameter",
            "pub fn f() {\n    with_write(move |conn: &mut Connection| {\n        conn.execute(sql, params)\n    })\n}\n",
            Some("Connection"),
        ),
        (
            "untyped closure parameter of a declared provider",
            "pub fn f() {\n    with_connection(config, |conn| {\n        conn.execute(sql, params)\n    })\n}\n",
            Some("Connection"),
        ),
        (
            "untyped closure parameter of an undeclared helper",
            "pub fn f() {\n    with_tools(config, |conn| {\n        conn.execute(sql, params)\n    })\n}\n",
            None,
        ),
        (
            "declared producer expression",
            "pub fn f() {\n    let tx = conn.transaction()?;\n    tx.execute(sql, params)?;\n}\n",
            Some("conn.transaction()"),
        ),
        (
            "plain move of an existing value",
            "pub fn f() {\n    let tx = chosen_tool;\n    tx.execute(args).await;\n}\n",
            None,
        ),
        (
            "wrapper type around a Tool",
            "pub fn f() {\n    let conn: Arc<dyn Tool> = resolve();\n    conn.execute(args).await;\n}\n",
            None,
        ),
        (
            "field receiver",
            "pub fn f(&self) {\n    self.conn.execute(sql, params)?;\n}\n",
            None,
        ),
    ];
    for (label, source, expected) in cases {
        let scan = scan_tool_executions("src/example.rs", source);
        assert_eq!(scan.sites.len(), 1, "{label}: {:?}", scan.sites);
        assert_eq!(
            scan.sites[0].non_tool_binding.as_deref(),
            *expected,
            "{label} resolved to the wrong binding: {:?}",
            scan.sites[0]
        );
    }
}

/// Independent audit B01-M3: a `Tool` reached through a field is not excused by
/// a same-named local of a proven type sitting beside it.
///
/// `self.conn.execute(args).await` reduces to the identifier `conn`, and the
/// function also takes `conn: &Connection`. A field has no binding in the
/// function, so its type is not the local's type and the call must fall through
/// to the reviewed site exceptions, which is where `self.tool_execution`,
/// `self.process` and `self.shell` already are.
#[test]
fn a_tool_on_a_non_local_receiver_is_not_excused_by_a_same_named_local() {
    let cases: &[(&str, &str, &str)] = &[
        ("field of self", "self.conn", "let _ = self.conn.execute(args).await;"),
        (
            "field of a local",
            "state.conn",
            "let _ = state.conn.execute(args).await;",
        ),
        ("accessor call", "conn", "let _ = checkout.conn()?.execute(args).await;"),
        (
            "indexed element",
            "conn",
            "let _ = registry.conn[0].execute(args).await;",
        ),
    ];
    for (label, expected_receiver, call) in cases {
        let source = format!(
            "pub async fn probe_field_receiver(&self, conn: &Connection) {{\n    \
             conn.execute(sql, params)?;\n    \
             {call}\n}}\n"
        );
        let scan = scan_tool_executions("src/memory/sqlite.rs", &source);
        assert_eq!(scan.sites.len(), 2, "{label}: {:?}", scan.sites);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        let receivers: Vec<&str> = direct.iter().map(|site| site.receiver_path.as_str()).collect();
        assert_eq!(
            receivers,
            vec![*expected_receiver],
            "{label}: the local `conn: &Connection` call must stay proven and the non-local \
             receiver must be inventoried; scan was {:?}",
            scan.sites
        );
    }
}

/// Independent audit B01-M4: a producer is matched against the whole
/// initialiser, so a method that merely shares a name with one proves nothing.
#[test]
fn a_producer_match_is_anchored_to_the_whole_initialiser() {
    let cases: &[(&str, &str, bool)] = &[
        ("the declared receiver", "conn.transaction()?", true),
        ("a different receiver", "self.transaction()", false),
        ("an undeclared receiver", "factory.transaction()?", false),
        ("a chained conversion", "conn.transaction()?.into_tool()", false),
        ("an error-context tail", "conn.transaction().context(reason)?", true),
        ("a declared constructor", "Connection::open(path)?", true),
        (
            "a constructor of some other type",
            "MessageSendTool::new(policy)",
            false,
        ),
        (
            "a constructor turned into something else",
            "Connection::open(path)?.into_tool()",
            false,
        ),
    ];
    for (label, initialiser, proven) in cases {
        let source = format!(
            "pub fn probe_producer() {{\n    let handle = {initialiser};\n    let _ = handle.execute(args);\n}}\n"
        );
        let scan = scan_tool_executions("src/memory/sqlite.rs", &source);
        assert_eq!(scan.sites.len(), 1, "{label}: {:?}", scan.sites);
        assert_eq!(
            scan.sites[0].non_tool_binding.is_some(),
            *proven,
            "{label}: `let handle = {initialiser};` resolved to {:?}",
            scan.sites[0]
        );
    }
}

/// The direct-invocation inventory counts calls, not functions, so a second
/// direct tool execution inside an already allowlisted function changes the
/// value the allowlist is compared against.
#[test]
fn the_direct_invocation_inventory_counts_calls_not_functions() {
    let one = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = tool.execute(args).await;\n}\n",
    );
    assert_eq!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&one.sites, NON_TOOL_EXECUTE_CALL_SITES)),
        vec!["src/example.rs::handler x1".to_string()]
    );

    let two = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = tool.execute(args).await;\n    let _ = other.execute_with_cancellation(args, None).await;\n}\n",
    );
    assert_eq!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&two.sites, NON_TOOL_EXECUTE_CALL_SITES)),
        vec!["src/example.rs::handler x2".to_string()],
        "a second direct tool execution in the same function must change the inventory line"
    );

    let none = scan_tool_executions("src/example.rs", "pub async fn handler() {\n    let _ = 1;\n}\n");
    assert!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&none.sites, NON_TOOL_EXECUTE_CALL_SITES))
            .is_empty(),
        "a removed call must drop its inventory line so a stale allowlist entry fails"
    );
}

/// Every allowlist entry declares how many direct tool executions its function
/// makes, and the count is what the guard compares.
#[test]
fn direct_tool_invocation_allowlist_counts_match_the_tree() {
    let scan = scan_repository_tool_executions();
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    let mut counted: BTreeMap<&str, usize> = BTreeMap::new();
    for site in &direct {
        *counted.entry(site.entry.as_str()).or_default() += 1;
    }
    for (entry, occurrences, _) in ALLOWED_DIRECT_TOOL_INVOCATIONS {
        let actual = counted.get(entry).copied().unwrap_or_default();
        assert_eq!(
            actual, *occurrences,
            "{entry} is allowlisted for {occurrences} direct tool execution(s) but the tree has \
             {actual}. Every direct call is reviewed one at a time: raise the count only with the \
             reason that justifies the new call, and remove the entry when the last call goes"
        );
    }
}

/// Every reviewed non-`Tool` call site still matches exactly the calls it
/// declares, on the real tree.
#[test]
fn non_tool_execute_call_sites_match_their_declared_occurrences() {
    let scan = scan_repository_tool_executions();
    let errors = non_tool_call_site_errors(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    assert!(
        errors.is_empty(),
        "NON_TOOL_EXECUTE_CALL_SITES is out of date:\n{}",
        errors.join("\n")
    );
}

/// The occurrence rule itself: an extra identical call, a vanished call site and
/// a duplicate entry all have to be reported, or the exception list would be a
/// name exemption wearing a longer key.
#[test]
fn the_occurrence_rule_reports_extra_calls_stale_entries_and_duplicates() {
    let exception = NonToolCallSite {
        entry: "src/example.rs::handler",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/example.rs",
        evidence: "service: Option<&ToolExecutionService>",
        reason: "synthetic fixture for the occurrence rule, long enough to pass the reason check",
    };

    let one = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n}\n",
    );
    assert!(non_tool_call_site_errors(&one.sites, &[exception]).is_empty());

    let two = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n    let _ = service.execute(args).await;\n}\n",
    );
    let errors = non_tool_call_site_errors(&two.sites, &[exception]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("declares 1 `execute` call(s) but the tree now has 2")),
        "a second identical call must not be absorbed by the first: {errors:?}"
    );

    let twin = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n    let _ = service.execute_with_cancellation(args, None).await;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&twin.sites, &[exception])
            .iter()
            .any(|error| error.contains("the tree now has 2")),
        "the cancellation twin counts as its own call"
    );

    let moved = scan_tool_executions(
        "src/example.rs",
        "pub async fn other_handler() {\n    let _ = service.execute(args).await;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&moved.sites, &[exception])
            .iter()
            .any(|error| error.contains("no longer matches any `execute` call site")),
        "an exception whose call site moved must go stale"
    );

    assert!(
        non_tool_call_site_errors(&one.sites, &[exception, exception])
            .iter()
            .any(|error| error.contains("is listed twice")),
        "a duplicated exception must be reported"
    );

    let proven = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler(service: &Connection) {\n    let _ = service.execute(sql, params)?;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&proven.sites, &[exception])
            .iter()
            .any(|error| error.contains("no longer matches any `execute` call site")),
        "an exception the binding rule now covers is redundant and must be reported"
    );
}

/// Every reviewed call site points at a declaration that is still in the source
/// and that names a type the tables know is not a `Tool`.
#[test]
fn every_non_tool_execute_call_site_shows_its_type_evidence() {
    for exception in NON_TOOL_EXECUTE_CALL_SITES {
        let source = fs::read_to_string(repository_root().join(exception.evidence_file))
            .unwrap_or_else(|error| panic!("read {}: {error}", exception.evidence_file));
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        assert!(
            production.contains(exception.evidence),
            "{} / {} cites {:?} in {} but that declaration is gone",
            exception.entry,
            exception.receiver,
            exception.evidence,
            exception.evidence_file
        );
        assert!(
            NON_TOOL_RECEIVER_TYPES
                .iter()
                .any(|(name, _)| contains_identifier(exception.evidence, name)),
            "{} / {} cites {:?}, which names no type in NON_TOOL_RECEIVER_TYPES",
            exception.entry,
            exception.receiver,
            exception.evidence
        );
    }
}

/// Every closure provider still hands out the type the table claims.
#[test]
fn non_tool_closure_providers_match_their_definitions() {
    let mut sources = Vec::new();
    for path in rust_source_files() {
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", relative_path(&path)));
        sources.push((
            relative_path(&path),
            strip_cfg_test_items(&blank_comments_and_literals(&source)),
        ));
    }

    for (provider, parameter, reason) in NON_TOOL_CLOSURE_PROVIDERS {
        assert!(
            NON_TOOL_RECEIVER_TYPES.iter().any(|(name, _)| name == parameter),
            "{provider} claims to hand out {parameter}, which is not a declared non-tool type"
        );
        assert!(
            reason.len() > 20,
            "{provider} needs a reason naming the store it belongs to, not {reason:?}"
        );
        let needle = format!("fn {provider}");
        let mut definitions = 0usize;
        for (relative, source) in &sources {
            for (offset, _) in source.match_indices(&needle) {
                if source[offset + needle.len()..].starts_with(|c: char| is_ident_char(c)) {
                    continue;
                }
                let signature = source[offset..]
                    .split_once('{')
                    .map_or_else(|| source[offset..].to_string(), |(head, _)| head.to_string());
                assert!(
                    signature.contains("FnOnce(") && contains_identifier(&signature, parameter),
                    "{relative} defines `{provider}` but its closure parameter is not a \
                     {parameter}: {}",
                    normalize_line(&signature)
                );
                definitions += 1;
            }
        }
        assert!(
            definitions > 0,
            "{provider} is declared as a non-tool closure provider but no production function \
             defines it; remove the entry instead of leaving a standing exemption"
        );
    }
}

/// The two type tables are review records, so they carry reasons and no entry
/// appears twice.
#[test]
fn the_non_tool_receiver_tables_are_reviewable() {
    let mut types = BTreeSet::new();
    for (name, reason) in NON_TOOL_RECEIVER_TYPES {
        assert!(types.insert(*name), "{name} is listed twice in NON_TOOL_RECEIVER_TYPES");
        assert!(
            reason.len() > 20,
            "{name} needs a reason naming the crate its `execute` comes from, not {reason:?}"
        );
    }
    let mut producers = BTreeSet::new();
    for (pattern, reason) in NON_TOOL_RECEIVER_PRODUCERS {
        assert!(
            producers.insert(*pattern),
            "{pattern} is listed twice in NON_TOOL_RECEIVER_PRODUCERS"
        );
        assert!(
            reason.len() > 20,
            "{pattern} needs a reason naming the type it yields, not {reason:?}"
        );
        assert!(
            pattern.contains('('),
            "{pattern} must be a call expression, not a bare name that any variable could carry"
        );
        assert_eq!(
            strip_inert_producer_tail(&producer_shape(pattern)),
            *pattern,
            "{pattern} is not in canonical producer form and can therefore never match an \
             initialiser; write it as the whole expression with argument lists reduced to `..`"
        );
        assert!(
            constructor_producer_type(pattern).is_none(),
            "{pattern} is an associated function of a declared non-tool type, which \
             constructor_producer_type already recognises; a table entry would only rot"
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

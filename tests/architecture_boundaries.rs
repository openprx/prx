//! Regression guards for architecture boundaries that are not expressed by Rust's type system.
//!
//! These allowlists intentionally describe the current tree. A new entry is a review event:
//! route the behavior through the owning repository/adapter/event fabric, or update the
//! allowlist with an explicit architecture decision.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

// `is_test_only_file`, `cfg_test_ranges`, `strip_cfg_test_items`,
// `blank_comments_and_literals`: one copy, shared with the audit gate in
// `src/runtime/bootstrap.rs`.
include!("support/cfg_test_scan.rs");

const ALLOWED_BRAIN_DB_OPENS_OUTSIDE_SQLITE_REPOSITORY: &[&str] = &[
    // Architecture review Q73B/Q74: exact test-owned read-only ACL and generation-scoped Retrieve witnesses.
    "src/channels/tests/factory_reload_fixture.rs::run::let connection = rusqlite::Connection::open_with_flags(",
    "src/channels/tests/factory_reload_fixture.rs::run::let decisions = rusqlite::Connection::open_with_flags(tmp.path().join(\"self/causal_tree/decisions.db\"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect(\"durable decision evidence\");",
    // Reviewed isolated paid Router/Automix test observers; exact callsites only.
    "src/runtime/turn_decision/routing/tests/paid_embedding.rs::embedding::let database = Connection::open_with_flags(",
    "src/runtime/turn_decision/routing/tests/paid_embedding.rs::run::let db = Connection::open(workspace.path().join(\"memory/brain.db\")).unwrap();",
    "src/runtime/turn_decision/routing/tests/paid_embedding.rs::run::let decisions = Connection::open(workspace.path().join(\"self/causal_tree/decisions.db\")).unwrap();",
    "src/agent/agent/automix_integration_tests.rs::verify_result_work::let database = rusqlite::Connection::open_with_flags(",
    "src/agent/agent/automix_integration_tests.rs::run_dropped_premium_work::let database = rusqlite::Connection::open_with_flags(workspace.path().join(\"memory/brain.db\"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    // Isolated child-process AUT: consistent snapshot and one copied completion tamper.
    "src/runtime/turn_decision/tests/child_process.rs::actual_parent_process_case::let logical_db = rusqlite::Connection::open(workspace.join(\"memory/brain.db\"))?;",
    "src/runtime/turn_decision/tests/child_process.rs::actual_parent_process_case::let copy = rusqlite::Connection::open(&copied_db)?;",
    // Fresh-process fixture: bounded, read-only enumeration before owned repository readback.
    "src/runtime/ingress_recovery/tests.rs::report::let conn = rusqlite::Connection::open_with_flags(",
    // Failure-only, read-only decision diagnostics in the actual Router fixture.
    "src/runtime/turn_decision/routing/tests.rs::seed_diagnostics::let store = Connection::open_with_flags(",
    // Test-only isolated fixtures: actual instance routing, source scope, and durable turn receipts.
    "src/agent/agent.rs::embedded_turn_routes_to_the_registered_instance_and_returns_to_default::let conn = rusqlite::Connection::open_with_flags(",
    "src/agent/embedded_source.rs::embedded_autosave_respects_configuration_and_keeps_original_source::let conn = rusqlite::Connection::open(workspace.path().join(\"memory/brain.db\")).unwrap();",
    "src/agent/embedded_source.rs::embedded_cross_provider_selection_is_refused_before_input_recording::let conn = rusqlite::Connection::open(workspace.path().join(\"memory/brain.db\")).unwrap();",
    "src/agent/tests.rs::turn_returns_text_when_no_tools_called::let conn = rusqlite::Connection::open_with_flags(",
    // Test-only: mutate isolated Router history and inspect actual decision receipts.
    "src/runtime/turn_decision/routing/tests.rs::database::Connection::open(self.workspace.path().join(\"memory/brain.db\")).unwrap()",
    "src/runtime/turn_decision/routing/tests.rs::latest_router_receipt::let store = Connection::open(self.workspace.path().join(\"self/causal_tree/decisions.db\")).unwrap();",
    "src/agent/turn_tests/context_recall.rs::build_agent_context_includes_document_evidence_with_source_ids::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/agent/turn_tests/compaction.rs::configurable_compaction_records_run_and_summary_memory::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
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
    // D-107 amendment: the doctor reads the cron and Xin stores read-only.
    "src/doctor/mod.rs::read_only_unowned_rows::let connection = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    "src/doctor/mod.rs::doctor_names_shell_jobs_without_a_recorded_creator::rusqlite::Connection::open(&db_path)",
    "src/doctor/mod.rs::read_only_sqlite_session_count::let conn = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;",
    // Test-only legacy rows intentionally bypass the new public write guard so
    // the migration and doctor can prove they repair pre-guard data.
    "src/doctor/mod.rs::doctor_reports_legacy_fitness_counts_and_repair_command::let connection = rusqlite::Connection::open(config.workspace_dir.join(\"memory/brain.db\")).unwrap();",
    "src/fitness_cli.rs::migration_exports_then_removes_legacy_rows_and_projections::let connection = rusqlite::Connection::open(db_path).unwrap();",
    // Test-only, read-only (V02-R9): the handler refusal test must prove that a
    // refused `memory_store` left the victim row's `owner_id` unchanged and wrote
    // no identity binding, and no memory API returns either. Every write in that
    // test goes through the production MCP handler.
    "src/gateway/compat/tests/handler.rs::brain_db_probe::let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)",
    "src/gateway/compat/tests/card_jwt.rs::mcp_agent_identity_binding_upserts_sqlite_row::let conn = Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    // Test-only (V02-R5): the anti-over-correction lock needs one row at
    // `visibility = 'public'`, and no memory API writes that visibility —
    // `classify_memory` produces `owner`, `group` or `private` only, and
    // `MemoryStoreMetadata` carries no visibility field. The row is published
    // with a single parameterized UPDATE on the seeded test database; every
    // read in that test still goes through the production MCP seam.
    "src/gateway/compat/tests/memory_tools.rs::mcp_memory_search_still_returns_public_rows::let conn = Connection::open(workspace.path().join(\"memory\").join(\"brain.db\"))",
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
    "src/memory/mod.rs::hygiene_and_snapshot_run_after_the_schema_exists_on_a_fresh_workspace::Connection::open(memory_dir.join(\"brain.db\"))",
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
    // Fresh-process fixture: exact current test executable, owned acknowledgement and reap.
    "src/runtime/ingress_recovery/tests.rs::start::.spawn()",
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
    // Test-only: proving the skill publication generation CAS is process-safe
    // needs real competing processes (the test binary re-executed), and proving
    // the lock is released when its holder dies needs a child that is SIGKILLed.
    "src/skills/lifecycle/tests/publish.rs::a_killed_lock_holder_releases_the_lock_and_publication_continues::.spawn()",
    "src/skills/lifecycle/tests/publish.rs::processes_racing_on_one_generation_have_exactly_one_winner::.spawn()",
    "src/tunnel/cloudflare.rs::start::.spawn()?;",
    "src/tunnel/custom.rs::start::.spawn()?;",
    "src/tunnel/mod.rs::kill_shared_terminates_and_clears_child::.spawn()",
    "src/tunnel/ngrok.rs::start::.spawn()?;",
    "src/tunnel/tailscale.rs::start::.spawn()?;",
];

/// The `.rs` files directly under `src/memory/sqlite/` that make up the split
/// backend (everything but the tests directory). Both lists are compared with
/// the directory, so a new file must be placed in one of them on purpose.
const SQLITE_BACKEND_FILES: &[&str] = &[
    "approval_grants.rs",
    "documents.rs",
    "drafts.rs",
    "events.rs",
    "evolution.rs",
    "fixture_readback.rs",
    "memory_impl.rs",
    "mirror.rs",
    "mod.rs",
    "recall.rs",
    "rows.rs",
    "schema.rs",
];

/// Older child modules that were never part of the exemption for the
/// repository root and stay scanned like any other file.
const SQLITE_SEPARATE_MODULES: &[&str] = &["ingress_recovery.rs", "router_history.rs", "router_learning.rs"];

const ALLOWED_PERSISTED_EVENT_TABLES: &[&str] = &[
    // Dedicated Article 73 compliance ledger: it stores immutable event hashes,
    // is owned by IncidentStore, and is intentionally separate from message/memory events.
    "src/compliance/incident.rs::serious_incident_audit_events",
    "src/cron/postgres.rs::cron_job_events",
    "src/cron/postgres.rs::{qualified_memory_events_table}",
    "src/cron/store.rs::cron_event_outbox",
    "src/cron/store.rs::cron_job_events",
    "src/memory/postgres/schema.rs::{qualified_memory_events_table}",
    "src/memory/postgres/schema.rs::{qualified_message_events_table}",
    "src/memory/postgres/schema.rs::{schema_ident}.approval_grant_events",
    "src/memory/sqlite/approval_grants.rs::approval_grant_events",
    "src/memory/sqlite/approval_grants.rs::memory_events",
    "src/memory/sqlite/schema.rs::evolution_proposal_events",
    "src/memory/sqlite/schema.rs::memory_events",
    "src/memory/sqlite/schema.rs::message_events",
    "src/memory/sqlite/schema.rs::message_events",
    "src/memory/sqlite/tests/schema_and_documents.rs::message_events",
    // The `memory_events` tables in `tests/visibility_events.rs` are pre-v16
    // fixtures (`legacy_memory_events_gets_owner_id_column_on_init` and its
    // siblings) that prove the C03 owner_id migration upgrades a database
    // created before the column existed. Test fixtures, not a second
    // production ledger.
    "src/memory/sqlite/tests/visibility_events.rs::memory_events",
    "src/memory/sqlite/tests/visibility_events.rs::memory_events",
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

thread_local! {
    /// Source files a guard-of-guards probe plants for the current test thread
    /// only, as `(relative path, full text)`: a planted path is walked by
    /// `rust_source_files` and read by `read_source` instead of the disk file.
    /// Empty outside `every_tree_scanner_catches_a_planted_violation`.
    static PLANTED_SOURCES: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn rust_source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(&repository_root().join("src"), &mut files);
    PLANTED_SOURCES.with_borrow(|planted| {
        for (relative, _) in planted {
            let path = repository_root().join(relative);
            if !files.contains(&path) {
                files.push(path);
            }
        }
    });
    files.sort();
    files
}

/// The text of a source file the scanners walk: the planted text for a probe
/// path on this thread, otherwise the file on disk.
fn read_source(path: &Path) -> String {
    let relative = relative_path(path);
    let planted = PLANTED_SOURCES.with_borrow(|planted| {
        planted
            .iter()
            .find(|(planted_path, _)| *planted_path == relative)
            .map(|(_, text)| text.clone())
    });
    planted.unwrap_or_else(|| fs::read_to_string(path).unwrap_or_else(|error| panic!("read {relative}: {error}")))
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
    let mut sqlite_repository_files = 0usize;
    let mut sqlite_directory_files = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if let Some(rest) = relative.strip_prefix("src/memory/sqlite/") {
            if !rest.contains('/') {
                sqlite_directory_files.push(rest.to_string());
            }
            if rest.starts_with("tests/") || SQLITE_BACKEND_FILES.contains(&rest) {
                sqlite_repository_files += 1;
                continue;
            }
        }
        let source = read_source(&path);
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
    assert!(
        sqlite_repository_files > 0,
        "the sqlite repository exemption matched no files under src/memory/sqlite/; the guard would be blind"
    );
    sqlite_directory_files.sort();
    let mut expected_directory_files = SQLITE_BACKEND_FILES
        .iter()
        .chain(SQLITE_SEPARATE_MODULES)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected_directory_files.sort();
    assert_eq!(
        sqlite_directory_files, expected_directory_files,
        "the files directly under src/memory/sqlite/ changed; list each new file in SQLITE_BACKEND_FILES \
         (exempt, and read by the config-generation check) or SQLITE_SEPARATE_MODULES (scanned)"
    );
    assert_inventory(
        "direct brain.db opens outside src/memory/sqlite/",
        actual,
        ALLOWED_BRAIN_DB_OPENS_OUTSIDE_SQLITE_REPOSITORY,
    );
}

#[test]
fn raw_child_process_spawns_are_explicitly_allowlisted() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
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
                // The hardened git launch (GIT-ENV) hands back a command.
                || prefix.contains("git_launch::git_command(")
                || prefix.contains(".build_command()")
                || line.contains("cmd.spawn(")
                || line.contains("command.spawn(")
                // An rmcp child-process transport spawns whatever command it
                // is handed, wherever that command was built.
                || line.contains("TokioChildProcess::builder(");
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
            "run_job_command",
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
        let source = read_source(&path);
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

    // The sqlite backend's production files, listed so this check reads them
    // directly instead of walking the tree (a walk would make it a tree
    // scanner that needs its own planted-violation probe). `include_str!`
    // fails the build if one of them moves, and a column that moves to a file
    // missing from this list fails the check below, so neither can go blind.
    let sqlite_sources = [
        ("mod.rs", include_str!("../src/memory/sqlite/mod.rs")),
        (
            "approval_grants.rs",
            include_str!("../src/memory/sqlite/approval_grants.rs"),
        ),
        ("documents.rs", include_str!("../src/memory/sqlite/documents.rs")),
        ("drafts.rs", include_str!("../src/memory/sqlite/drafts.rs")),
        ("events.rs", include_str!("../src/memory/sqlite/events.rs")),
        ("evolution.rs", include_str!("../src/memory/sqlite/evolution.rs")),
        ("memory_impl.rs", include_str!("../src/memory/sqlite/memory_impl.rs")),
        ("mirror.rs", include_str!("../src/memory/sqlite/mirror.rs")),
        ("recall.rs", include_str!("../src/memory/sqlite/recall.rs")),
        ("rows.rs", include_str!("../src/memory/sqlite/rows.rs")),
        ("schema.rs", include_str!("../src/memory/sqlite/schema.rs")),
    ];
    // Every backend file except `fixture_readback.rs` (test-only observations)
    // must be in the list above.
    let mut listed = sqlite_sources.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    listed.push("fixture_readback.rs");
    listed.sort_unstable();
    let mut backend = SQLITE_BACKEND_FILES.to_vec();
    backend.sort_unstable();
    assert_eq!(
        listed, backend,
        "the sqlite sources read here must be the backend files"
    );
    let sqlite_source = sqlite_sources
        .iter()
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !sqlite_source.trim().is_empty(),
        "the sqlite backend source files under src/memory/sqlite/ must not be empty"
    );
    // The PostgreSQL adapter is the root file plus every file under its
    // directory; the scan must see both and must not come back empty.
    let mut postgres_files = vec!["src/memory/postgres.rs".to_string()];
    for entry in
        fs::read_dir(repository_root().join("src/memory/postgres")).expect("read the PostgreSQL adapter directory")
    {
        let path = entry.expect("read directory entry").path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            postgres_files.push(relative_path(&path));
        }
    }
    assert!(
        postgres_files.len() > 2,
        "the PostgreSQL adapter scan found only {postgres_files:?}"
    );
    let postgres_source = postgres_files
        .iter()
        .map(|file| {
            fs::read_to_string(repository_root().join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for (relative, source) in [
        ("src/memory/sqlite/", sqlite_source),
        ("src/memory/postgres.rs and src/memory/postgres/", postgres_source),
    ] {
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

    for relative in ["src/agent/entry.rs", "src/chat/decision_source.rs"] {
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

/// The production part of a source file for the configuration-write guard:
/// every `#[cfg(test)]` *module* blanked wherever it is (an inline
/// `mod tests { .. }`, a brace-less `mod x;` declaration, one in the middle of
/// the file), and nothing for a test-only file. Other `#[cfg(test)]` items
/// still count, because the pinned write counts below were taken that way.
/// Positions and lines are kept, so nothing after a test module is lost.
fn production_part(source: &str) -> String {
    if is_test_only_file(source) {
        return String::new();
    }
    let masked: Vec<char> = blank_comments_and_literals(source).chars().collect();
    let modules = cfg_test_ranges(source)
        .into_iter()
        .filter(|range| {
            // Past the attributes and the visibility, the construct is `mod`.
            let mut cursor = range.start;
            loop {
                while cursor < masked.len() && masked[cursor].is_whitespace() {
                    cursor += 1;
                }
                if cursor + 1 < masked.len() && masked[cursor] == '#' && masked[cursor + 1] == '[' {
                    cursor = close_of_group(&masked, cursor + 1);
                } else {
                    break;
                }
            }
            let (mut word, mut after) = masked_word_at(&masked, cursor);
            if word == "pub" {
                while after < masked.len() && masked[after].is_whitespace() {
                    after += 1;
                }
                if after < masked.len() && masked[after] == '(' {
                    after = close_of_group(&masked, after);
                }
                (word, _) = masked_word_at(&masked, after);
            }
            word == "mod"
        })
        .collect::<Vec<_>>();
    blank_ranges(source, &modules)
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
fn config_writes_route_through_the_transaction_layer() {
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
            "src/config/schema.rs",
            ".save()",
            1,
            "Config::save (first-run creation)",
        ),
        ("src/config/schema.rs", "commit_mutation_atomically(", 1, "Config::save"),
        (
            "src/config/schema.rs",
            "commit_mutation_guarded(",
            1,
            "the startup secret migration (revision-guarded, D-101 F-5)",
        ),
        (
            "src/config/schema.rs",
            "write_toml_string_atomic_without_lock(",
            2,
            "the atomic write primitive",
        ),
        ("src/config/schema.rs", "plan_structured_save(", 1, "Config::save"),
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
            3,
            "not yet migrated: onboarding (offline)",
        ),
        (
            "src/onboard/wizard.rs",
            "write_split_config(",
            3,
            "not yet migrated: onboarding (offline)",
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

/// Every workspace command hook starts from the cleared launch environment
/// in its dedicated directory, and only behind the operator's opt-in
/// (HOOKS-ENV): the one runner clears the environment, builds it through the
/// shared launch builder and pins the working directory, and `emit` checks
/// the policy before running any action.
#[test]
fn workspace_hooks_launch_through_the_shared_builder_behind_the_opt_in() {
    let source = fs::read_to_string(repository_root().join("src/hooks/mod.rs"))
        .unwrap_or_else(|error| panic!("read src/hooks/mod.rs: {error}"));
    let production = production_part(&source);
    let runner = function_body(&production, "run_action");
    for required in [
        "cmd.env_clear();",
        "mcp_launch::workspace_launch_environment(",
        "cmd.current_dir(&working_dir);",
        "mcp_launch::prepare_launch_directory(",
    ] {
        assert!(runner.contains(required), "run_action must call {required}");
    }
    assert_eq!(
        production.matches("Command::new(").count(),
        1,
        "src/hooks/mod.rs builds a process command outside run_action"
    );
    let emit = function_body(&production, "emit");
    assert!(emit.contains("self.workspace_policy()"), "emit must check the opt-in");
}

/// GIT-ENV F-6 (D-103): the production places that name the `git`
/// program (`"git"`, `"git.exe"`, or a path ending in one), as
/// `file::function::literal`. Only the hardened launch starts git; the rest
/// name it without starting it (a shell-command classifier, the intent
/// catalogue, the doctor's `git --version` probe, a protocol name).
const ALLOWED_GIT_PROGRAM_LITERALS: &[&str] = &[
    "src/doctor/mod.rs::check_environment::git",
    "src/doctor/mod.rs::check_unowned_shell_jobs::git",
    "src/security/git_launch.rs::command_fallback::git",
    "src/security/git_launch.rs::git_command_with_parent::git",
    "src/security/git_launch.rs::program_is_git::git",
    "src/security/git_launch.rs::read_git::git",
    "src/security/policy.rs::command_risk_level::git",
    "src/security/policy.rs::is_args_safe::git",
];

/// GIT-ENV F-6 (D-103): every production site that builds a process
/// command, as `file::function::line`. A new one, whatever the program
/// expression (a constant, a variable, a path, `sh -c`), is reviewed here:
/// git must go through `security::git_launch::git_command`.
const ALLOWED_PROCESS_COMMANDS: &[&str] = &[
    "src/channels/imessage.rs::send::let output = tokio::process::Command::new(\"osascript\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::if let Ok(status_output) = Command::new(\"rc-service\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let _ = Command::new(\"launchctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let active_output = Command::new(\"systemctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let list_output = Command::new(\"launchctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let restart_output = Command::new(\"rc-service\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let restart_output = Command::new(\"systemctl\")",
    "src/channels/service.rs::maybe_restart_managed_daemon_service::let start_output = Command::new(\"launchctl\")",
    "src/channels/signal.rs::run_command::let output = tokio::process::Command::new(cmd)",
    "src/channels/signal_native.rs::build_command::let mut cmd = Command::new(&self.cli_path);",
    "src/channels/wacli.rs::health_check::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::send_prepared_media::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::send_text::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    "src/channels/wacli.rs::set_presence::let mut cmd = tokio::process::Command::new(self.cli_binary());",
    // A `#[cfg(test)]` function outside a test module (the scan keeps it).
    "src/chat/mod.rs::plain_apply_ignores_openprx_approval_override_and_leaves_file_unchanged::let status = std::process::Command::new(exe)",
    "src/chat/mod.rs::run_external_editor_command::std::process::Command::new(\"cmd\")",
    "src/chat/mod.rs::run_external_editor_command::std::process::Command::new(\"sh\")",
    "src/chat/sessions/shell.rs::spawn_shell_with_origin::let mut cmd = tokio::process::Command::new(\"sh\");",
    "src/chat/terminal_proto.rs::copy_to_tmux_buffer::let mut child = Command::new(\"tmux\")",
    "src/doctor/mod.rs::check_command_available::match std::process::Command::new(cmd)",
    "src/doctor/mod.rs::disk_available_mb::let output = std::process::Command::new(\"df\").arg(\"-m\").arg(path).output().ok()?;",
    "src/hooks/mod.rs::run_action::let mut cmd = Command::new(&program);",
    "src/media/mod.rs::run_command_bounded::let mut child = tokio::process::Command::new(command)",
    "src/memory/lucid.rs::run_lucid_command_raw::let mut cmd = Command::new(lucid_cmd);",
    "src/nodes/server.rs::execute_shell_command::let mut cmd = Command::new(&parsed_command.program);",
    "src/runtime/docker.rs::build_command::let mut process = tokio::process::Command::new(\"docker\");",
    "src/runtime/native.rs::build_shell_command::let mut process = tokio::process::Command::new(\"sh\");",
    "src/security/git_launch.rs::git_command_with_parent::let mut command = tokio::process::Command::new(\"git\");",
    "src/security/git_launch.rs::read_git::let mut command = tokio::process::Command::new(\"git\");",
    "src/security/secrets.rs::load_or_create_key::match std::process::Command::new(\"icacls\")",
    "src/service/mod.rs::check_openprx_user::let output = Command::new(\"getent\")",
    "src/service/mod.rs::chown_recursive_to_openprx::let output = Command::new(\"chown\")",
    "src/service/mod.rs::chown_to_openprx::let output = Command::new(\"chown\")",
    "src/service/mod.rs::ensure_openprx_user::let group_output = Command::new(\"getent\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"addgroup\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"adduser\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"getent\")",
    "src/service/mod.rs::ensure_openprx_user::let output = Command::new(\"useradd\")",
    "src/service/mod.rs::ensure_openrc_runtime_path_writable::let output = Command::new(&program)",
    "src/service/mod.rs::install_linux_openrc::run_checked(Command::new(\"rc-update\").args([\"add\", \"prx\", \"default\"])).await?;",
    "src/service/mod.rs::install_linux_systemd::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::install_linux_systemd::run_checked(Command::new(\"systemctl\").args([\"--user\", \"enable\", \"prx.service\"])).await?;",
    "src/service/mod.rs::install_windows::run_checked(Command::new(\"schtasks\").args([",
    "src/service/mod.rs::preferred_user_config_dir::if let Ok(output) = Command::new(\"getent\")",
    "src/service/mod.rs::query_status::let output = run_capture_status(Command::new(\"launchctl\").arg(\"list\")).await?;",
    "src/service/mod.rs::query_status::run_capture_status(Command::new(\"schtasks\").args([\"/Query\", \"/TN\", windows_task_name(), \"/FO\", \"LIST\"]))",
    "src/service/mod.rs::query_status_linux::let output = run_capture_status(Command::new(\"rc-service\").args([\"prx\", \"status\"])).await?;",
    "src/service/mod.rs::query_status_linux::run_capture_status(Command::new(\"systemctl\").args([\"--user\", \"is-active\", \"prx.service\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"restart\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::restart_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"restart\", \"prx.service\"])).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"launchctl\").arg(\"load\").arg(\"-w\").arg(&plist)).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"launchctl\").arg(\"start\").arg(SERVICE_LABEL)).await?;",
    "src/service/mod.rs::start::run_checked(Command::new(\"schtasks\").args([\"/Run\", \"/TN\", windows_task_name()])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"start\"])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/service/mod.rs::start_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"start\", \"prx.service\"])).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"launchctl\").arg(\"stop\").arg(SERVICE_LABEL)).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"launchctl\").arg(\"unload\").arg(\"-w\").arg(&plist)).await?;",
    "src/service/mod.rs::stop::run_checked(Command::new(\"schtasks\").args([\"/End\", \"/TN\", task_name])).await?;",
    "src/service/mod.rs::stop_linux::run_checked(Command::new(\"rc-service\").args([\"prx\", \"stop\"])).await?;",
    "src/service/mod.rs::stop_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"stop\", \"prx.service\"])).await?;",
    "src/service/mod.rs::uninstall::run_checked(Command::new(\"schtasks\").args([\"/Delete\", \"/TN\", task_name, \"/F\"])).await?;",
    "src/service/mod.rs::uninstall_linux::run_checked(Command::new(\"rc-update\").args([\"del\", \"prx\", \"default\"])).await?;",
    "src/service/mod.rs::uninstall_linux::run_checked(Command::new(\"systemctl\").args([\"--user\", \"daemon-reload\"])).await?;",
    "src/skills/mod.rs::install_skill_from_source::let mut junction_cmd = tokio::process::Command::new(\"cmd\");",
    "src/tools/mcp.rs::stdio_command::let mut cmd = Command::new(command);",
    "src/tools/message_send.rs::auto_generate_voice::let mut ffmpeg_cmd = tokio::process::Command::new(\"ffmpeg\");",
    "src/tools/message_send.rs::auto_generate_voice::let mut node_cmd = tokio::process::Command::new(\"node\");",
    "src/tools/message_send.rs::auto_generate_voice::let mut npm_cmd = tokio::process::Command::new(\"npm\");",
    "src/tools/sessions_spawn.rs::run_sub_agent_process::let mut command = tokio::process::Command::new(executable);",
    "src/tunnel/cloudflare.rs::start::let mut child = Command::new(\"cloudflared\")",
    "src/tunnel/custom.rs::start::let mut child = Command::new(parts[0])",
    "src/tunnel/ngrok.rs::start::Command::new(\"ngrok\")",
    "src/tunnel/ngrok.rs::start::let mut child = Command::new(\"ngrok\")",
    "src/tunnel/tailscale.rs::start::let child = Command::new(\"tailscale\")",
    "src/tunnel/tailscale.rs::start::let output = Command::new(\"tailscale\")",
    "src/tunnel/tailscale.rs::stop::Command::new(\"tailscale\")",
];

/// The string literals of one source line, before any `//` comment.
fn line_string_literals(line: &str) -> Vec<String> {
    let characters = line.chars().collect::<Vec<_>>();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '/' && characters.get(index + 1) == Some(&'/') {
            break;
        }
        if character == '\'' {
            // `'"'` and `'\"'` are characters, not string delimiters.
            if characters.get(index + 1) == Some(&'"') && characters.get(index + 2) == Some(&'\'') {
                index += 3;
                continue;
            }
            if characters.get(index + 1) == Some(&'\\')
                && characters.get(index + 2) == Some(&'"')
                && characters.get(index + 3) == Some(&'\'')
            {
                index += 4;
                continue;
            }
        }
        if character == '"' {
            let mut literal = String::new();
            index += 1;
            while index < characters.len() && characters[index] != '"' {
                if characters[index] == '\\' {
                    literal.push(characters[index]);
                    index += 1;
                }
                if let Some(next) = characters.get(index) {
                    literal.push(*next);
                }
                index += 1;
            }
            literals.push(literal);
        }
        index += 1;
    }
    literals
}

/// Whether a string literal names the git program.
fn names_git_program(literal: &str) -> bool {
    let literal = literal.to_ascii_lowercase();
    matches!(literal.as_str(), "git" | "git.exe")
        || literal.ends_with("/git")
        || literal.ends_with("/git.exe")
        || literal.ends_with("\\\\git.exe")
}

/// GIT-ENV F-6: the `git` program is named only in reviewed places, so a
/// constant, a path or a variable holding it cannot start git outside the
/// hardened launch unnoticed.
#[test]
fn git_is_named_only_in_the_reviewed_places() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for literal in line_string_literals(line) {
                if names_git_program(&literal) {
                    actual.push(format!("{relative}::{}::{literal}", enclosing_function(&lines, index)));
                }
            }
        }
    }
    assert_inventory("production literals naming git", actual, ALLOWED_GIT_PROGRAM_LITERALS);
}

/// GIT-ENV F-6: every production process command is inventoried, whatever
/// runs it (`.spawn()`, `.output()`, `.status()`, `run_managed_output`).
#[test]
fn every_production_process_command_is_inventoried() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            // `Command::new` as a call or as a function value (`.map(Command::new)`).
            let builds_command = line.match_indices("Command::new").any(|(offset, _)| {
                offset == 0
                    || line[..offset]
                        .chars()
                        .next_back()
                        .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'))
            });
            if builds_command {
                actual.push(format!(
                    "{relative}::{}::{}",
                    enclosing_function(&lines, index),
                    normalize_line(line)
                ));
            }
        }
    }
    assert_inventory("production process commands", actual, ALLOWED_PROCESS_COMMANDS);
}

/// GIT-ENV F-6 (D-106): no production code renames a process `Command`
/// type (`use tokio::process::Command as Proc;`), which would hide its
/// launch sites from the inventory above.
#[test]
fn process_command_types_are_never_aliased() {
    let mut actual = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let lines = production.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let normalized = normalize_line(line);
            if normalized.contains("Command as ") {
                actual.push(format!(
                    "{relative}::{}::{normalized}",
                    enclosing_function(&lines, index)
                ));
            }
        }
    }
    assert_inventory("aliased process command types", actual, &[]);
}

/// GIT-ENV (D-106, D-107 and its amendment): git in a shell command string
/// is refused on the model-driven surfaces (the `shell` tool, workspace skill
/// scripts, cron jobs and Xin tasks an agent tool created) and hardened
/// through the environment on the operator's (`prx cron`/`prx xin` jobs,
/// system Xin tasks, node allowlists).
#[test]
fn shell_sites_refuse_or_harden_git() {
    let body = |relative: &str, function: &str| {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        function_body(&production_part(&source), function)
    };
    let adapter = body("src/runtime/shell_process.rs", "execute");
    for required in [
        "git_launch::shell_command_runs_git(request.command)",
        "ShellProcessError::GitRefused",
        "git_launch::operator_git_environment()",
    ] {
        assert!(
            adapter.contains(required),
            "ShellProcessAdapter::execute must use {required}"
        );
    }
    for (relative, function) in [
        ("src/tools/shell.rs", "execute_inner"),
        ("src/tools/skills.rs", "execute_resolved"),
    ] {
        assert!(
            body(relative, function).contains("ShellGitPolicy::RefuseGit"),
            "{relative}::{function} is model-driven and must refuse git"
        );
    }
    // D-107 amendment: cron and Xin decide by the recorded creator.
    assert!(
        body("src/cron/scheduler.rs", "run_job_command_with_adapter").contains("git: shell_git_policy(config, job)")
    );
    assert!(body("src/cron/scheduler.rs", "shell_git_policy").contains("job_created_via("));
    assert!(body("src/xin/runner.rs", "run_shell_with_adapter").contains("git: shell_git_policy(config, task)"));
    let xin = body("src/xin/runner.rs", "shell_git_policy");
    assert!(xin.contains("TaskKind::System") && xin.contains("task_created_via("));
    assert!(body("src/nodes/server.rs", "execute_shell_command").contains("git_launch::operator_git_environment()"));
}

/// Every production `git` process is built by the one hardened launch
/// (GIT-ENV, D-098): `src/security/git_launch.rs` is the only production file
/// that names the `git` program, and its builder clears the environment,
/// reads the repository's configuration to neutralise the programs it names
/// (D-100, D-103), sets the fixed program settings on every launch, and
/// points `core.hooksPath` nowhere.
#[test]
fn git_runs_only_through_the_hardened_launch() {
    const LAUNCH: &str = "src/security/git_launch.rs";
    let mut outside = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = production_part(&read_source(&path));
        let count = production.matches("Command::new(\"git\")").count();
        if relative == LAUNCH {
            assert_eq!(count, 2, "{LAUNCH}: the launch and the configuration read");
            let entry = function_body(&production, "git_command");
            assert!(
                entry.contains("std::env::vars_os()") && entry.contains("git_command_with_parent(scope, parent)"),
                "git_command must launch with PRX's own environment"
            );
            let builder = function_body(&production, "git_command_with_parent");
            for required in [
                "command.env_clear()",
                "git_environment(parent)",
                "inspect_repository(dir, &environment).await?",
                "fixed_overrides(&entries, &paths)",
                "repository_overrides(&entries, &paths, &submodules)?",
                "ALWAYS_OVERRIDES",
                "\"core.hooksPath\"",
                "hooks_path()?",
            ] {
                assert!(
                    builder.contains(required),
                    "git_command_with_parent must use {required}"
                );
            }
            let reader = function_body(&production, "read_git");
            assert!(reader.contains(".env_clear()"), "read_git must clear the environment");
            let environment = function_body(&production, "git_environment");
            assert!(environment.contains("mcp_launch::launch_environment("), "{environment}");
        } else if count > 0 {
            outside.push(relative);
        }
    }
    assert!(
        outside.is_empty(),
        "these files start `git` outside the hardened launch (use security::git_launch::git_command): {outside:?}"
    );
}

/// Every stdio MCP launch starts from a cleared environment (D-045): the
/// one command builder clears it and adds only the launch allowlist and the
/// server's own `env`, and both launch paths (discovery, call sessions) use
/// it.
#[test]
fn stdio_mcp_launches_start_from_a_cleared_environment() {
    let source = fs::read_to_string(repository_root().join("src/tools/mcp.rs"))
        .unwrap_or_else(|error| panic!("read src/tools/mcp.rs: {error}"));
    let production = production_part(&source);
    let builder = function_body(&production, "stdio_command");
    assert!(builder.contains("cmd.env_clear();"), "{builder}");
    // D-088 C-3: never PRX's own working directory.
    assert!(builder.contains("cmd.current_dir(working_dir);"), "{builder}");
    assert!(builder.contains("mcp_launch::launch_environment("), "{builder}");
    assert_eq!(
        production.matches("Command::new(").count(),
        1,
        "src/tools/mcp.rs builds a process command outside `stdio_command`"
    );
    for launch in ["discover_server_tools_stdio", "call_stdio"] {
        let body = function_body(&production, launch);
        assert!(body.contains("TokioChildProcess::builder(cmd)"), "{launch}");
        assert!(
            body.contains("Self::stdio_command(server, command, &working_dir)"),
            "{launch}"
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

/// The configuration-write guard's production part drops every test
/// *module*, wherever it is, and keeps what follows it.
#[test]
fn the_production_part_drops_every_test_module_and_keeps_what_follows() {
    let source = "fn a() {}\n#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\n#[allow(clippy::x)]\nmod tests {}\n";
    let production = production_part(source);
    assert!(production.contains("fn helper()"));
    assert!(!production.contains("mod tests"));

    // A brace-less test module first (providers/mod.rs:37 shape) and an inline
    // one in the middle of the file (chat/mod.rs:1577 shape): the production
    // code after either is still part of the file.
    let source = "#[cfg(test)]\npub(crate) mod credential_probe;\npub fn first(cfg: Config) {\n    cfg.save();\n}\n\
                  #[cfg(test)]\nmod tests {\n    fn t() { cfg.save(); }\n}\npub fn second(cfg: Config) {\n    \
                  cfg.apply_config();\n}\n";
    let production = production_part(source);
    assert!(production.contains("cfg.save();\n}"), "{production}");
    assert!(production.contains("cfg.apply_config()"), "{production}");
    assert!(!production.contains("credential_probe"), "{production}");
    assert!(!production.contains("fn t()"), "{production}");
}

/// One case of `a_cfg_test_construct_ends_where_it_ends`: the source, text
/// that must survive stripping, and text that must not.
type StripCase = (&'static str, &'static [&'static str], &'static [&'static str]);

/// Every shape a `#[cfg(test)]` attribute takes in this crate, each followed
/// by production text that must stay visible: the construct ends at its own
/// `;` or `,`, at the close of its block, or just before the closer of the
/// enclosing construct, which always survives.
const STRIP_CASES: &[StripCase] = &[
    // A block-bodied test arm, then an arm opening with a range pattern: `..`
    // begins the next arm, it does not continue the block expression.
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => {}\n        ..=5 => \
         call(MiddlewareStage::Outbound),\n        _ => 0,\n    }\n}\n",
        &["..=5 => call(MiddlewareStage::Outbound),", "_ => 0,"],
        &["0 => {}"],
    ),
    // An inline `const` block is an expression statement, not an item: it has
    // no `;` and ends with its block.
    (
        "fn f() {\n    #[cfg(test)]\n    const { assert!(true) }\n    run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["assert!(true)"],
    ),
    // A method call or `.await` after a test block still continues it.
    (
        "async fn f() {\n    #[cfg(test)]\n    async { probe() }.await;\n    run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["probe()"],
    ),
    // Raw byte and raw C strings ending in a backslash, and an escaped quote
    // char directly followed by another char, inside test code: the literal
    // ends where rustc ends it, so nothing after it is taken for a literal.
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = br\"C:\\\";\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = cr\"C:\\\";\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = ['\\'','('];\n}\npub fn after() {\n    let _ = \
         MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nconst LIMIT: usize = 3;\n#[cfg(test)]\n#[path = \"probe_tests.rs\"]\nmod probe_tests;\n\
         #[cfg(test)]\nuse crate::probe::{A, B};\n#[cfg(test)]\nconst TABLE: [u8; 2] = [\n    1, 2,\n];\npub fn \
         production() {\n    let _ = MiddlewareStage::Outbound;\n}\n#[cfg(test)]\nfn helper() -> Result<(), \
         Error> {\n    Ok(())\n}\npub fn after_helper() {}\n#[cfg(test)]\nmod tests {\n    fn inside() {}\n}\n",
        &[
            "pub fn production()",
            "MiddlewareStage::Outbound",
            "pub fn after_helper()",
        ],
        &[
            "LIMIT",
            "probe_tests",
            "crate::probe",
            "TABLE",
            "1, 2,",
            "fn helper",
            "Ok(())",
            "mod tests",
            "inside",
        ],
    ),
    (
        "pub struct Runtime {\n    #[cfg(test)]\n    probe: u8,\n    pub stage: Stage,\n}\n",
        &["pub stage: Stage,", "pub struct Runtime {"],
        &["probe"],
    ),
    (
        "pub struct Runtime {\n    pub stage: Stage,\n    #[cfg(test)]\n    probe: u8\n}\npub fn later() -> \
         Stage {\n    MiddlewareStage::Outbound\n}\n",
        &["pub stage: Stage,", "pub fn later()", "MiddlewareStage::Outbound"],
        &["probe"],
    ),
    (
        "fn build(\n    #[cfg(test)] hook: Hook,\n    real: u8,\n) -> anyhow::Result<u8> {\n    \
         run(MiddlewareStage::Outbound)\n}\n",
        &["real: u8,", ") -> anyhow::Result<u8> {", "MiddlewareStage::Outbound"],
        &["hook"],
    ),
    (
        "fn f() {\n    call(\n        #[cfg(test)]\n        probe(),\n        run(MiddlewareStage::Outbound),\n    );\n}\n",
        &["run(MiddlewareStage::Outbound),", "    );"],
        &["probe()"],
    ),
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => 1,\n        _ => \
         call(MiddlewareStage::Outbound),\n    }\n}\n",
        &["_ => call(MiddlewareStage::Outbound),"],
        &["0 => 1"],
    ),
    (
        "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => {\n            probe()\n        \
         }\n        _ => call(MiddlewareStage::Outbound),\n    }\n}\n",
        &["_ => call(MiddlewareStage::Outbound),"],
        &["probe()"],
    ),
    (
        "fn f() -> Runtime {\n    Runtime {\n        #[cfg(test)]\n        probe: 1,\n        stage: \
         run(MiddlewareStage::Outbound),\n    }\n}\n",
        &["stage: run(MiddlewareStage::Outbound),"],
        &["probe: 1"],
    ),
    (
        "fn generation() -> Arc<Generation> {\n    #[cfg(test)]\n    Arc::new(Generation { probe: 1 })\n}\nfn \
         later() {\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &[
            "fn generation() -> Arc<Generation> {",
            "fn later()",
            "MiddlewareStage::Outbound",
        ],
        &["Arc::new", "probe"],
    ),
    (
        "fn f() {\n    #[cfg(test)]\n    if armed { probe() } else { other() }\n    \
         run(MiddlewareStage::Outbound);\n}\n",
        &["run(MiddlewareStage::Outbound);"],
        &["armed", "probe()", "other()"],
    ),
    (
        "fn f() {\n    #[cfg(test)]\n    hook();\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &["let _ = MiddlewareStage::Outbound;"],
        &["hook()"],
    ),
    // Brackets inside literals and comments of a test item move nothing.
    (
        "#[cfg(test)]\nfn helper() {\n    let _ = \"(\";\n    let _ = '[';\n    // (\n}\nconst OWNER: &str = \
         \"[context_handoff]\";\nconst EVENT: &str = \"llm.request.context.compiled\";\n",
        &["[context_handoff]", "llm.request.context.compiled"],
        &["fn helper"],
    ),
    (
        "#[cfg(test)]\nmod tests {\n    const OPEN: &str = \"{ ( [\";\n    /* } ) ] */\n}\npub fn after() {\n    \
         let _ = MiddlewareStage::Outbound;\n}\n",
        &["pub fn after()", "MiddlewareStage::Outbound"],
        &["OPEN"],
    ),
    // An attribute spelled inside a literal or comment is not one.
    (
        "const DOC: &str = \"#[cfg(test)]\";\n// #[cfg(test)]\npub fn f() {\n    let _ = MiddlewareStage::Outbound;\n}\n",
        &["pub fn f()", "MiddlewareStage::Outbound"],
        &[],
    ),
];

/// Braces of the production text a stripping kept, after blanking literals:
/// never below zero and zero at the end.
fn braces_balance(text: &str) -> bool {
    let mut depth = 0i64;
    for character in blank_comments_and_literals(text).chars() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// A `#[cfg(test)]` construct ends where it ends (R-LOOPD0 R1-R3): extents are
/// computed on blanked text for raw and blanked callers alike, a brace-less or
/// comma-terminated construct does not swallow what follows, and the closer
/// of the enclosing construct stays.
#[test]
fn a_cfg_test_construct_ends_where_it_ends() {
    for (source, kept, stripped) in STRIP_CASES {
        for (mode, input) in [
            ("raw", (*source).to_string()),
            ("blanked", blank_comments_and_literals(source)),
        ] {
            let production = strip_cfg_test_items(&input);
            for text in *kept {
                // Blanked input has no literal contents left to keep.
                if input.contains(text) {
                    assert!(
                        production.contains(text),
                        "{mode}: `{text}` was stripped from:\n{source}\n---\n{production}"
                    );
                }
            }
            for text in *stripped {
                assert!(
                    !production.contains(text),
                    "{mode}: `{text}` was kept from:\n{source}\n---\n{production}"
                );
            }
            assert!(
                braces_balance(&production),
                "{mode}: stripping unbalanced the braces of:\n{source}\n---\n{production}"
            );
            assert_eq!(
                production.lines().count(),
                source.lines().count(),
                "{mode}: stripping must keep every line"
            );
        }
    }
}

/// A file whose first line of code is `#![cfg(test)]` is test code for every
/// scanner that separates production from tests, and nothing else is.
///
/// This is what lets a test module live in its own file (a `#[path]` child of
/// the item it tests) without a path exemption: the marker travels with the
/// file and the compiler enforces it. The negative half is the mutation: the
/// same production text without the marker, with the marker after the first
/// item, or with a composite predicate is still scanned.
#[test]
fn a_file_level_cfg_test_marker_makes_the_whole_file_test_code() {
    let body = "pub async fn handler() {\n    let _ = MiddlewareStage::Outbound;\n    cfg.save();\n    let _ = \
                tool.execute(args).await;\n}\n";
    for marked in [
        format!("//! Tests of a production module.\n\n#![cfg(test)]\n#![allow(clippy::unwrap_used)]\n\n{body}"),
        format!("/* leading block comment */\n#![cfg(test)] // trailing comment\n{body}"),
    ] {
        assert!(is_test_only_file(&marked), "{marked}");
        assert!(strip_cfg_test_items(&marked).is_empty(), "{marked}");
        assert!(
            strip_cfg_test_items(&blank_comments_and_literals(&marked)).is_empty(),
            "{marked}"
        );
        assert!(production_part(&marked).is_empty(), "{marked}");
        assert!(
            scan_tool_executions("src/agent/turn_tests/probe.rs", &marked)
                .sites
                .is_empty(),
            "{marked}"
        );
    }

    for unmarked in [
        body.to_string(),
        format!("use super::*;\n#![cfg(test)]\n{body}"),
        format!("#![allow(clippy::unwrap_used)]\n#![cfg(test)]\n{body}"),
        format!("#![cfg(any(test, feature = \"probe\"))]\n{body}"),
        format!("#![cfg(not(test))]\n{body}"),
        format!("#![cfg(all(test, feature = \"probe\"))]\n{body}"),
        format!("/* a comment that mentions\n#![cfg(test)]\n*/\n{body}"),
        format!("{body}#[cfg(test)]\nmod tests {{}}\n"),
    ] {
        assert!(!is_test_only_file(&unmarked), "{unmarked}");
        let stripped = strip_cfg_test_items(&blank_comments_and_literals(&unmarked));
        assert!(stripped.contains("MiddlewareStage::Outbound"), "{unmarked}");
        assert!(production_part(&unmarked).contains("cfg.save()"), "{unmarked}");
        assert_eq!(
            scan_tool_executions("src/agent/turn_tests/probe.rs", &unmarked)
                .sites
                .len(),
            1,
            "{unmarked}"
        );
    }
}

/// Tree scanners that deliberately inventory test code as well, with the
/// reason. Every other function that walks the source tree must separate
/// production from tests through a marker-aware helper.
const TEST_INCLUSIVE_TREE_SCANNERS: &[(&str, &str)] = &[
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

fn scanner_fails_with(scanner: fn(), planted: Vec<(String, String)>) -> bool {
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
        const PROBED_TREE_SCANNERS: &[&str] = &[$(stringify!($scanner)),*];
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
        "src/chat/mod.rs", Planted::AppendedTo, Reads::ProductionText,
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
        const MUTATION_PROBED_TREE_SCANNERS: &[(&str, fn(), fn())] = &[
            $((stringify!($scanner), $scanner, $proof)),+
        ];
    };
}

source_mutation_probes! {
    provider_label_sinks_redact_url_userinfo => terminal_provider_label_guard_rejects_bypass_missing_calls_and_literal_decoys,
    embedded_user_input_cannot_bypass_reserved_marker_neutralization => mutation_proof_embedded_input_guard,
    every_non_tool_execute_call_site_shows_its_type_evidence => mutation_proof_execution_binding_guard,
    forwarded_compaction_values_are_produced_from_the_effective_window => mutation_proof_compaction_producer_guard,
    ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates => mutation_proof_credential_comparison_guard,
}

/// Calls that walk or read the source tree. A function whose body makes one
/// walks the tree, and so does every function that calls a walking function
/// (found to a fixpoint), whatever the helper is called.
const TREE_WALKS: &[&str] = &[
    "rust_source_files(",
    "rust_source_and_test_files(",
    "collect_rust_files(",
    "read_dir(",
    "read_source(",
];

/// Every function of `source` (comment- and literal-blanked text), at any
/// indentation: its name, whether `#[test]` or `#[tokio::test]` marks it
/// (also on the same line), and its body.
fn source_functions(source: &str) -> Vec<(String, bool, String)> {
    let bytes = source.as_bytes();
    let mut functions = Vec::new();
    let mut search = 0usize;
    while let Some(found) = source[search..].find("fn ") {
        let at = search + found;
        search = at + 3;
        if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_') {
            continue;
        }
        let name: String = source[at + 3..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let rest = &source[at + 3 + name.len()..];
        let (Some(open), semicolon) = (rest.find('{'), rest.find(';')) else {
            continue;
        };
        if semicolon.is_some_and(|semicolon| semicolon < open) {
            continue;
        }
        let open = at + 3 + name.len() + open;
        let mut depth = 0usize;
        let mut close = source.len();
        for (offset, character) in source[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        // The attributes: the text before `fn` on its line, and the attribute
        // lines directly above it.
        let line_start = source[..at].rfind('\n').map_or(0, |offset| offset + 1);
        let mut head = source[line_start..at].to_string();
        let mut above = source[..line_start].lines().rev();
        while let Some(line) = above.next().filter(|line| line.trim_start().starts_with("#[")) {
            head.push_str(line);
        }
        let is_test = head.contains("#[test]") || head.contains("#[tokio::test");
        functions.push((name, is_test, source[open..close].to_string()));
    }
    functions
}

/// The `#[test]` functions of `source` (comment- and literal-blanked text)
/// that scan the source tree, directly or through any walking helper.
fn tree_scanners(source: &str) -> Vec<String> {
    let functions = source_functions(source);
    let calls = |body: &str, callee: &str| {
        let needle = format!("{callee}(");
        body.match_indices(&needle).any(|(offset, _)| {
            offset == 0 || {
                let before = body.as_bytes()[offset - 1];
                !(before.is_ascii_alphanumeric() || before == b'_')
            }
        })
    };
    let mut walkers: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = walkers.len();
        for (name, _is_test, body) in &functions {
            if walkers.contains(name) {
                continue;
            }
            let walks = TREE_WALKS.iter().any(|walk| calls(body, walk.trim_end_matches('(')))
                || walkers.iter().any(|walker| calls(body, walker));
            if walks {
                walkers.insert(name.clone());
            }
        }
        if walkers.len() == before {
            break;
        }
    }
    let mut scanners: Vec<String> = functions
        .into_iter()
        .filter(|(name, is_test, _)| *is_test && walkers.contains(name))
        .map(|(name, _, _)| name)
        .collect();
    scanners.sort();
    scanners.dedup();
    scanners
}

/// String methods that cut or search source text; applied to the
/// `#[cfg(test)]` spelling they are hand-written test stripping.
const STRIPPING_CALLS: &[&str] = &[
    "split",
    "split_once",
    "rsplit",
    "rsplit_once",
    "split_inclusive",
    "find",
    "rfind",
    "match_indices",
    "matches",
    "starts_with",
    "strip_prefix",
];

/// Hand-written test stripping in `text`: a string method applied to the
/// `#[cfg(test)]` spelling instead of `strip_cfg_test_items`.
fn hand_stripping_patterns(text: &str) -> Vec<String> {
    let marker = ["\"#[cfg", "(test)]"].concat();
    STRIPPING_CALLS
        .iter()
        .map(|call| format!("{call}({marker}"))
        .filter(|pattern| text.contains(pattern.as_str()))
        .collect()
}

/// The memo in front of the blanker and the stripper returns exactly what the
/// computation returns, for every file of the tree, on a miss and on a hit.
#[test]
fn the_strip_memo_returns_the_exact_computation() {
    for path in rust_source_files() {
        let source = read_source(&path);
        let blanked = blank_comments_and_literals_uncached(&source);
        let stripped = if is_test_only_file(&source) {
            String::new()
        } else {
            blank_ranges(&source, &cfg_test_ranges(&source))
        };
        for _ in 0..2 {
            assert_eq!(
                blank_comments_and_literals(&source),
                blanked,
                "{}",
                relative_path(&path)
            );
            assert_eq!(strip_cfg_test_items(&source), stripped, "{}", relative_path(&path));
        }
    }
    let edited = "fn a() {}\n#[cfg(test)]\nfn b() {}\n";
    assert_ne!(
        strip_cfg_test_items(edited),
        strip_cfg_test_items("fn a() {}\nfn b() {}\n")
    );
}

/// Discovery finds a tree scanner in every shape one can be written: inside
/// a module, on one line, as `#[tokio::test]`, and through a walking helper
/// that is not one of the known ones.
#[test]
fn tree_scanner_discovery_sees_every_shape() {
    let source = "fn helper_walk() -> usize {\n    rust_source_files().len()\n}\n\nmod nested {\n    #[test]\n    fn \
                  indented_scanner() {\n        for path in rust_source_files() {}\n    }\n}\n\n#[test] fn \
                  one_line_scanner() { let _ = fs::read_dir(\"src\"); }\n\n#[test]\nfn via_a_new_helper() {\n    \
                  let _ = helper_walk();\n}\n\n#[tokio::test]\nasync fn async_scanner() {\n    let _ = \
                  read_source(path);\n}\n\n#[test]\nfn not_a_scanner() {\n    let _ = 1;\n}\n";
    let mut found = tree_scanners(&blank_comments_and_literals(source));
    found.sort();
    assert_eq!(
        found,
        vec![
            "async_scanner",
            "indented_scanner",
            "one_line_scanner",
            "via_a_new_helper"
        ]
    );
}

/// The ban on hand-written stripping catches every string method that cuts
/// or searches text at the `#[cfg(test)]` spelling.
#[test]
fn hand_stripping_ban_covers_every_split_form() {
    let marker = ["\"#[cfg", "(test)]\""].concat();
    for call in [
        "split",
        "split_once",
        "rsplit",
        "rsplit_once",
        "find",
        "rfind",
        "starts_with",
        "strip_prefix",
    ] {
        let text = format!("let parts = source.{call}({marker});");
        assert!(!hand_stripping_patterns(&text).is_empty(), "`{call}` is not caught");
    }
    assert!(
        hand_stripping_patterns("let production = strip_cfg_test_items(&source);").is_empty(),
        "expected an empty value"
    );
}

#[test]
fn every_tree_scanner_is_probed_or_declared_test_inclusive() {
    let raw = fs::read_to_string(repository_root().join("tests/architecture_boundaries.rs"))
        .expect("read tests/architecture_boundaries.rs");
    let source = blank_comments_and_literals(&raw);
    let scanners = tree_scanners(&source);
    let mut unproven = scanners
        .iter()
        .filter(|name| {
            !PROBED_TREE_SCANNERS.contains(&name.as_str())
                && !MUTATION_PROBED_TREE_SCANNERS
                    .iter()
                    .any(|(listed, _, _)| listed == name)
                && !TEST_INCLUSIVE_TREE_SCANNERS.iter().any(|(listed, _)| listed == name)
        })
        .collect::<Vec<_>>();
    unproven.sort();
    assert!(
        unproven.is_empty(),
        "these tree scanners have no planted-violation probe and are not declared test-inclusive: {unproven:?}"
    );
    for listed in PROBED_TREE_SCANNERS
        .iter()
        .chain(MUTATION_PROBED_TREE_SCANNERS.iter().map(|(name, _, _)| name))
        .chain(TEST_INCLUSIVE_TREE_SCANNERS.iter().map(|(name, _)| name))
    {
        assert!(
            scanners.iter().any(|name| name == listed),
            "{listed} is listed but scans no tree"
        );
    }

    let unique_mutation_scanners: BTreeSet<_> =
        MUTATION_PROBED_TREE_SCANNERS.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(unique_mutation_scanners.len(), MUTATION_PROBED_TREE_SCANNERS.len());
    for (name, scanner, proof) in MUTATION_PROBED_TREE_SCANNERS {
        assert!(
            !PROBED_TREE_SCANNERS.contains(name),
            "duplicate standard probe registration: {name}"
        );
        assert!(
            !TEST_INCLUSIVE_TREE_SCANNERS.iter().any(|(listed, _)| listed == name),
            "mutation-probed scanner cannot use a test-inclusive exemption: {name}"
        );
        assert!(
            !scanner_fails_with(*scanner, Vec::new()),
            "registered scanner fails on the tree: {name}"
        );
        proof();
    }

    let bootstrap = fs::read_to_string(repository_root().join("src/runtime/bootstrap.rs")).expect("read bootstrap");
    for (file, text) in [
        ("tests/architecture_boundaries.rs", &raw),
        ("src/runtime/bootstrap.rs", &bootstrap),
    ] {
        let found = hand_stripping_patterns(text);
        assert!(
            found.is_empty(),
            "{file} strips test code by hand with {found:?}; use strip_cfg_test_items"
        );
    }
}

#[test]
fn prompt_guided_tool_protocol_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            strip_cfg_test_items(&source)
                .contains("## Tool Use Protocol")
                .then_some(relative)
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
    let channels = fs::read_to_string(repository_root().join("src/channels/prompt_context.rs"))
        .expect("read src/channels/prompt_context.rs");
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

/// Every runtime-prompt builder derives its tool descriptions from the
/// compiled `ToolSpec` snapshot. The scan covers all of `src/agent/**` by
/// directory (so a builder moved to a new agent module stays covered without a
/// list edit) plus the builders outside it.
#[test]
fn runtime_prompt_has_no_parallel_tool_description_catalog() {
    const BUILDERS_OUTSIDE_AGENT: &[&str] = &[
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
        "src/chat/mod.rs",
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner.rs",
    ];
    let mut scanned = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/agent/") && !BUILDERS_OUTSIDE_AGENT.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&read_source(&path));
        assert!(
            !production.contains("tool_descs"),
            "{relative} must derive prompt tool descriptions from the compiled ToolSpec snapshot"
        );
        scanned.push(relative);
    }
    for relative in BUILDERS_OUTSIDE_AGENT {
        assert!(
            scanned.iter().any(|path| path == relative),
            "{relative} is gone; update the list"
        );
    }
}

#[test]
fn compiled_request_context_event_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            // Production source only: a test that reads the event back by its
            // type is a consumer, not a second emitter.
            strip_cfg_test_items(&source)
                .contains("llm.request.context.compiled")
                .then_some(relative)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        owners,
        vec!["src/agent/turn_engine.rs"],
        "request-context provenance must be emitted once by the shared provider loop"
    );
}

/// The outbound middleware stage has exactly one production call site.
///
/// `outbound` is contracted to run once per accepted, delivered turn, and the
/// regressions that pin it (`agent::turn_engine::tests::outbound_middleware_lifecycle`)
/// count arrivals at the call site in `src/agent/turn_engine.rs`. A second production
/// site anywhere in the crate would hand a plugin two executions for one
/// delivered reply — two audit records, two rate-budget charges, two external
/// writes — and every one of those regressions would still pass, because none
/// of them can see a stage entered somewhere else.
///
/// The test call sites in `src/plugins/runtime.rs` are the middleware runtime's
/// own coverage and are excluded with the production/test separator the other
/// single-owner guards use.
#[test]
fn outbound_middleware_has_one_production_owner() {
    let mut owners = Vec::new();
    let mut call_sites = 0usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let count = production.matches("MiddlewareStage::Outbound").count();
        if count > 0 {
            owners.push(relative);
            call_sites += count;
        }
    }

    assert_eq!(
        owners,
        vec!["src/agent/turn_engine.rs"],
        "the outbound middleware stage must be invoked from the shared agent tool loop and nowhere else"
    );
    assert_eq!(
        call_sites, 1,
        "the outbound middleware stage must have exactly one production call site; a second one is a \
         second execution per delivered turn that no invocation-count regression can observe"
    );
}

/// Every production entry that builds a `ToolLoopMemory` attaches the
/// request-context event fabric to it.
///
/// The guard reads production text only (comments, literals and test items
/// removed): a `.with_event_fabric(` call inside a test module, a doc comment
/// or a string does not wire a production turn, and a file whose tests still
/// spell the call must not keep the guard green after its production entry
/// moved elsewhere.
#[test]
fn production_tool_loop_memory_is_wired_to_request_context_events() {
    for relative in [
        "src/agent/entry.rs",
        "src/channels/turn.rs",
        "src/chat/mod.rs",
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner.rs",
        "src/tools/delegate.rs",
        "src/tools/sessions_spawn.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        assert!(
            production.contains(".with_event_fabric("),
            "{relative} must attach request-context event persistence to its ToolLoopMemory in production code"
        );
    }

    let dispatcher_source =
        fs::read_to_string(repository_root().join("src/chat/dispatcher.rs")).expect("read src/chat/dispatcher.rs");
    let dispatcher = strip_cfg_test_items(&blank_comments_and_literals(&dispatcher_source));
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

/// Every production turn that runs capability routing pins the text it routes
/// on.
///
/// AG-06 structural inventory: every shared engine call carries the reviewed
/// source-owned decision handoff. Rust makes the context mandatory; this guard
/// prevents a new entrypoint or an altered handoff from escaping review.
/// Runtime origin/parity checks and EP behavior tests prove semantic provenance.
const REVIEWED_DECISION_HANDOFFS: &[(&str, usize, &str)] = &[
    (
        "src/agent/embedded_turn.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service:&service,context:&decision,proof:source,}",
    ),
    (
        "src/agent/entry.rs",
        2,
        "crate::agent::turn_request::TurnDecision{service:decision_service,context:decision_context,proof:decision_proof,}",
    ),
    ("src/channels/turn.rs", 1, "decision_turn.routing()"),
    ("src/chat/dispatcher.rs", 1, "routing"),
    (
        "src/chat/mod.rs",
        1,
        "admitted_binding.decision_routing().ok_or_else(||anyhow::anyhow!())?",
    ),
    (
        "src/gateway/api/sessions.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service:&decision.service,context:&decision.context,proof:decision.proof,}",
    ),
    (
        "src/gateway/mod.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service,context:&context,proof:&gateway_proof,}",
    ),
    ("src/session_worker/runner.rs", 1, "admitted_turn.routing()"),
    ("src/tools/delegate.rs", 1, "child_turn.routing()"),
    ("src/tools/sessions_spawn.rs", 1, "child_turn.routing()"),
];

fn compact_rust_tokens(expression: &str) -> String {
    expression
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
fn every_loop_engine_call_receives_a_turn_decision_context() {
    let mut actual = BTreeMap::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (_, request) in engine_turn_requests(&production) {
            assert!(
                request_field(&request, "routing").is_none(),
                "{relative}: removed routing field returned to the shared request"
            );
            let decision = request_field(&request, "decision")
                .unwrap_or_else(|| panic!("{relative}: shared engine request has no decision"));
            let (_, _, expected) = REVIEWED_DECISION_HANDOFFS
                .iter()
                .find(|(file, _, _)| *file == relative)
                .unwrap_or_else(|| panic!("{relative}: unreviewed shared engine entrypoint"));
            assert_eq!(
                compact_rust_tokens(&decision),
                *expected,
                "{relative}: decision handoff changed; review its service, context and source provenance"
            );
            *actual.entry(relative.clone()).or_insert(0usize) += 1;
        }
        if relative == "src/agent/embedded_turn.rs" {
            let body = compact_rust_tokens(&function_body(&production, "run_work"));
            assert_eq!(
                body.matches("letsource_owner=source;letsource=source_owner.as_ref();")
                    .count(),
                1
            );
            assert_eq!(
                body.matches(".record_guarded(input,&owner,cancellation,defer_model_selection)")
                    .count(),
                1
            );
        }
        if relative == "src/gateway/mod.rs" {
            let body = compact_rust_tokens(&function_body(&production, "run_gateway_chat_with_multimodal"));
            for binding in [
                "let(source_receipt,gateway_user_event,mutadmission_guard)=pending_input.record(message.to_owned(),fabric_ctx.idempotency_key.clone()).await?;",
                "letgateway_proof=source_receipt.bind(&turn_runtime,fabric_ctx)?;",
                "letservice=turn_runtime.decision_slot.publish(&turn_runtime,state).await?;",
                "letcontext=service.decide_agent_initial(&gateway_proof,crate::runtime::turn_decision::AgentTurnKind::SingleShot,&cancellation,).await?;",
            ] {
                assert_eq!(
                    body.matches(binding).count(),
                    1,
                    "Gateway source/service admission provenance changed"
                );
            }
        }
        if relative == "src/chat/dispatcher.rs" {
            assert!(
                compact_rust_tokens(&production).contains("letSome(routing)=binding.decision_routing()else{"),
                "Chat dispatcher must unwrap its admitted binding before the engine handoff"
            );
        }
        if relative == "src/agent/turn_request.rs" {
            assert!(
                compact_rust_tokens(&production).contains("pub(crate)decision:TurnDecision<'a>,"),
                "the shared request must require a non-optional decision context"
            );
        }
    }
    let expected: BTreeMap<_, _> = REVIEWED_DECISION_HANDOFFS
        .iter()
        .map(|(path, count, _)| ((*path).to_owned(), *count))
        .collect();
    assert_eq!(
        actual, expected,
        "shared engine inventory changed; reconcile all EP owners, including future websocket turns"
    );
}

/// The one loop-engine entry, by the name a call site spells.
const ENGINE_ENTRY: &str = "run_turn";

/// Every `run_turn` call in already-stripped production source, as the call's
/// byte offset paired with the text of its `TurnRequest` argument (the second
/// of its four arguments).
fn engine_turn_requests(production: &str) -> Vec<(usize, String)> {
    let bytes = production.as_bytes();
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(found) = production[search..].find(ENGINE_ENTRY) {
        let start = search + found;
        search = start + ENGINE_ENTRY.len();
        if start > 0 && is_identifier_byte(bytes[start - 1]) {
            continue;
        }
        let end = start + ENGINE_ENTRY.len();
        if end < bytes.len() && is_identifier_byte(bytes[end]) {
            continue;
        }
        if production[..start].trim_end().ends_with("fn") {
            continue;
        }
        let mut cursor = end;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            continue;
        }
        let Some(arguments) = top_level_arguments(production, cursor) else {
            continue;
        };
        assert_eq!(
            arguments.len(),
            4,
            "a `run_turn` call with {} arguments does not match `run_turn(history, request, services, delivery)`",
            arguments.len()
        );
        calls.push((start, arguments[1].clone()));
    }
    calls
}

/// The value of field `name` in the first struct literal of `expression`
/// (`Type { a: x, b }`); the shorthand `b` yields `b`. `None` when absent.
fn request_field(expression: &str, name: &str) -> Option<String> {
    let open = expression.find('{')?;
    let bytes = expression.as_bytes();
    let mut depth = 0i32;
    let mut field_start = open + 1;
    let mut fields = Vec::new();
    for index in open..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    fields.push(&expression[field_start..index]);
                    break;
                }
            }
            b',' if depth == 1 => {
                fields.push(&expression[field_start..index]);
                field_start = index + 1;
            }
            _ => {}
        }
    }
    fields.into_iter().find_map(|field| {
        let field = field.trim();
        match field.split_once(':') {
            Some((key, value)) if key.trim() == name && !value.starts_with(':') => Some(value.trim().to_string()),
            None if field == name => Some(field.to_string()),
            _ => None,
        }
    })
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Split the call that opens at `open` into its top-level argument expressions.
fn top_level_arguments(production: &str, open: usize) -> Option<Vec<String>> {
    let bytes = production.as_bytes();
    let mut depth = 0i32;
    let mut arguments: Vec<String> = Vec::new();
    let mut argument_start = open + 1;
    for index in open..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let last = production[argument_start..index].to_string();
                    if !last.trim().is_empty() {
                        arguments.push(last);
                    }
                    return Some(arguments);
                }
            }
            b',' if depth == 1 => {
                arguments.push(production[argument_start..index].to_string());
                argument_start = index + 1;
            }
            _ => {}
        }
    }
    None
}

fn leading_identifier(expression: &str) -> Option<String> {
    let trimmed = expression.trim_start_matches(|character: char| {
        character.is_whitespace() || character == '&' || character == '*' || character == '('
    });
    let identifier: String = trimmed
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    (!identifier.is_empty()).then_some(identifier)
}

/// The initializer of the last `let <name> =` that appears before `before`.
fn nearest_let_initializer(production: &str, name: &str, before: usize) -> Option<String> {
    let limit = before.min(production.len());
    let mut best: Option<usize> = None;
    for pattern in [format!("let {name} ="), format!("let mut {name} =")] {
        let mut search = 0usize;
        while search < limit {
            let Some(found) = production[search..limit].find(&pattern) else {
                break;
            };
            let start = search + found;
            search = start + pattern.len();
            best = Some(best.map_or(start, |current: usize| current.max(start)));
        }
    }
    let equals = production[best?..].find('=')? + best?;
    let bytes = production.as_bytes();
    let mut depth = 0i32;
    for index in equals + 1..bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => return Some(production[equals + 1..index].to_string()),
            _ => {}
        }
    }
    None
}

/// The source files that are test code because every `mod` declaration that
/// loads them is `#[cfg(test)]` (INT-2B a48). A file whose own first line is
/// `#![cfg(test)]` is test code by [`is_test_only_file`]; this covers the
/// test modules that carry no marker, such as `#[cfg(test)] mod x_tests;`
/// and `#[cfg(test)] #[path = "turn_tests/x.rs"] mod x;`. It replaces a
/// file-name rule (`ends_with("tests.rs")`), which also skipped production
/// files such as `contests.rs`. A file no declaration loads is production.
fn test_module_files() -> BTreeSet<String> {
    let files: Vec<(String, String)> = rust_source_files()
        .iter()
        .map(|path| (relative_path(path), read_source(path)))
        .collect();
    test_module_files_of(&files)
}

/// [`test_module_files`] over explicit `(relative path, source)` pairs.
///
/// All offsets are character indices: `blank_comments_and_literals` and
/// `strip_cfg_test_items` keep character positions, not byte positions, so
/// the raw text (for the `#[path]` literal), the masked text and the
/// production text are read at the same character index (INT-2B B4f L-4).
fn test_module_files_of(files: &[(String, String)]) -> BTreeSet<String> {
    const fn is_identifier_char(character: char) -> bool {
        character.is_ascii_alphanumeric() || character == '_'
    }
    let mut production_loaded = BTreeSet::new();
    let mut test_loaded = BTreeSet::new();
    for (relative, raw_text) in files {
        let raw: Vec<char> = raw_text.chars().collect();
        let masked_text = blank_comments_and_literals(raw_text);
        let masked: Vec<char> = masked_text.chars().collect();
        let production: Vec<char> = strip_cfg_test_items(&masked_text).chars().collect();
        let dir = relative.rsplit_once('/').map_or("", |(dir, _)| dir);
        let file = relative.rsplit('/').next().unwrap_or_default();
        let module_dir = if matches!(file, "mod.rs" | "lib.rs" | "main.rs") {
            dir.to_string()
        } else {
            format!("{dir}/{}", file.trim_end_matches(".rs"))
        };
        let keyword = ['m', 'o', 'd', ' '];
        for start in 0..masked.len() {
            if !masked[start..].starts_with(&keyword) || (start > 0 && is_identifier_char(masked[start - 1])) {
                continue;
            }
            let mut cursor = start + keyword.len();
            while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
                cursor += 1;
            }
            let name_start = cursor;
            while masked.get(cursor).copied().is_some_and(is_identifier_char) {
                cursor += 1;
            }
            let name: String = masked[name_start..cursor].iter().collect();
            while masked.get(cursor).is_some_and(|character| character.is_whitespace()) {
                cursor += 1;
            }
            if name.is_empty() || masked.get(cursor) != Some(&';') {
                continue;
            }
            // The attributes right above the declaration, delimited on the
            // masked text; the `#[path]` literal is read from the raw text
            // at the same character positions.
            let attributes_start = masked[..start]
                .iter()
                .rposition(|character| matches!(character, ';' | '}' | '{'))
                .map_or(0, |index| index + 1);
            let attributes: String = raw[attributes_start..start].iter().collect();
            let target = attributes
                .find("#[path = \"")
                .and_then(|at| {
                    let value = &attributes[at + "#[path = \"".len()..];
                    value.find('"').map(|end| format!("{dir}/{}", &value[..end]))
                })
                .map_or_else(
                    || vec![format!("{module_dir}/{name}.rs"), format!("{module_dir}/{name}/mod.rs")],
                    |path| vec![path],
                );
            let in_production = production.get(start..start + keyword.len()) == Some(&keyword[..]);
            for candidate in target {
                if in_production {
                    production_loaded.insert(candidate);
                } else {
                    test_loaded.insert(candidate);
                }
            }
        }
    }
    test_loaded.difference(&production_loaded).cloned().collect()
}

/// Whether the file at `relative` is test code: its own `#![cfg(test)]`
/// marker, or it is loaded only by `#[cfg(test)]` module declarations.
fn is_test_code_file(relative: &str, source: &str, test_modules: &BTreeSet<String>) -> bool {
    is_test_only_file(source) || test_modules.contains(relative)
}

/// Whether a `compaction` argument is `None` in any spelling (INT-2B a48):
/// `None`, `Option::None` under any path, with generic arguments anywhere
/// (`None::<T>`, `Option::<T>::None`), or a plain name the nearest
/// `let <name> =` before the call binds to one of those. Known gaps: a typed
/// binding (`let c: Option<_> = None;`), a binding more than one hop away, a
/// `None` returned from a function, and `Default::default()` (which is
/// `None` for an `Option`) are not followed.
fn compaction_argument_is_none(production: &str, argument: &str, call_start: usize) -> bool {
    fn spelled_none(text: &str) -> bool {
        let compact: String = text.split_whitespace().collect();
        let compact = compact.trim_end_matches(';');
        // Drop every `::<…>` generic argument list (balanced), wherever it is.
        let mut path = String::with_capacity(compact.len());
        let mut depth = 0usize;
        let mut rest = compact;
        while let Some(character) = rest.chars().next() {
            if depth == 0 && rest.starts_with("::<") {
                depth = 1;
                rest = &rest[3..];
                continue;
            }
            if depth > 0 {
                match character {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    _ => {}
                }
            } else {
                path.push(character);
            }
            rest = &rest[character.len_utf8()..];
        }
        path == "None" || path.ends_with("Option::None")
    }
    if spelled_none(argument) {
        return true;
    }
    let trimmed = argument.trim();
    leading_identifier(trimmed)
        .filter(|name| name == trimmed)
        .and_then(|name| nearest_let_initializer(production, &name, call_start))
        .is_some_and(|initializer| spelled_none(&initializer))
}

/// INT-2B a48, RED before (the guards compared the literal text `None` and
/// skipped every file named `*tests.rs`): every spelling of `None` counts,
/// and a file is test code only when it is loaded only under `#[cfg(test)]`.
#[test]
fn compaction_none_spellings_and_test_module_files_are_exact() {
    let production = "fn f() {\n    let budget = None;\n    let kept = Some(&config.agent.compaction);\n}\n";
    let call = production.len();
    for none in [
        "None",
        " None ",
        "Option::None",
        "std::option::Option::None",
        "None::<&AgentCompactionConfig>",
        // B4f L-5
        "Option::<&AgentCompactionConfig>::None",
        "std::option::Option::<Option<u8>>::None",
        "budget",
    ] {
        assert!(compaction_argument_is_none(production, none, call), "{none:?} is None");
    }
    for some in [
        "Some(&config.agent.compaction)",
        "kept",
        "compaction_config",
        "Nonesuch",
    ] {
        assert!(
            !compaction_argument_is_none(production, some, call),
            "{some:?} is not None"
        );
    }

    let modules = test_module_files();
    for test_module in [
        "src/providers/label_redaction_tests.rs",
        "src/providers/tool_dialect_config_tests.rs",
        "src/providers/catalog/resolver_tests.rs",
    ] {
        assert!(
            modules.contains(test_module),
            "{test_module} is loaded only under cfg(test)"
        );
    }
    // L-4: `#[path]` modules declared after non-ASCII comment text resolve
    // to their real files (character offsets, not byte offsets).
    for engine_module in [
        "src/agent/turn_tests/engine_surface.rs",
        "src/agent/turn_tests/engine_budget.rs",
        "src/agent/turn_tests/engine_stay_silent.rs",
    ] {
        assert!(
            modules.contains(engine_module),
            "{engine_module} is loaded only under cfg(test)"
        );
    }
    let planted = vec![(
        "src/x/host.rs".to_string(),
        "// é ü — multibyte text before the declaration\n#[cfg(test)]\n#[path = \"host_cases.rs\"]\nmod cases;\n// ß\nmod plain;\n"
            .to_string(),
    )];
    let planted_modules = test_module_files_of(&planted);
    assert_eq!(
        planted_modules,
        BTreeSet::from(["src/x/host_cases.rs".to_string()]),
        "a #[path] literal after multibyte comments is read from the right place"
    );
    // Loaded by a production `#[path]` declaration, but test code by its own
    // `#![cfg(test)]` marker.
    let marked = "src/agent/turn_tests/context_paging.rs";
    assert!(is_test_code_file(
        marked,
        &read_source(&repository_root().join(marked)),
        &modules
    ));
    for production_file in [
        "src/agent/context_paging.rs",
        "src/providers/mod.rs",
        "src/agent/contests.rs",
    ] {
        assert!(
            !modules.contains(production_file),
            "{production_file} is not a test module"
        );
    }
}

/// Every production turn hands the loop engine a context-budget config.
///
/// `compaction_config = None` switches off the whole bounded-request contract
/// for that turn: no pre-turn flush or paging, no preflight before each
/// provider call (so the compiled-request event reports `budgeted_*: null`),
/// and no reserved mid-turn trim. The Gateway webhook turn shipped that way
/// (OQ-23), which left `POST /webhook` and the WhatsApp, Linq and Nextcloud
/// Talk webhooks unbudgeted while every other surface was budgeted.
///
/// A `compaction: None` in a `TurnRequest`, in any spelling (see
/// [`compaction_argument_is_none`]), is recorded by its enclosing function
/// (test modules excluded by [`is_test_code_file`]). The
/// engine has no pass-through calls of its own. There is no allowed site: the
/// last one, `agent_turn`, was removed with the library-only `process_message`
/// (E2).
///
/// MUTATION GUARD: put `None` back as the webhook's compaction argument and the
/// inventory gains `src/gateway/mod.rs::run_gateway_chat_with_multimodal`.
#[test]
fn production_turns_pass_a_context_budget_to_the_loop_engine() {
    let mut unbudgeted = Vec::new();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        // Test modules (loaded only under `#[cfg(test)]`) drive the engine
        // without a budget on purpose. They are not execution surfaces.
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let lines: Vec<&str> = production.lines().collect();
        for (call_start, request) in engine_turn_requests(&production) {
            let compaction = request_field(&request, "compaction")
                .unwrap_or_else(|| panic!("{relative}: a `run_turn` request without a `compaction` field"));
            if !compaction_argument_is_none(&production, &compaction, call_start) {
                continue;
            }
            let line_index = production[..call_start].matches('\n').count();
            unbudgeted.push(format!("{relative}::{}", enclosing_function(&lines, line_index)));
        }
    }

    assert_inventory("production loop engine calls without a context budget", unbudgeted, &[]);
}

/// Where a loop-engine call's compaction config comes from, as far as the
/// source text shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionSource {
    /// The call reaches `resolve_effective_compaction_config` or a
    /// `CompactionResolver`: the served model's effective window.
    Effective,
    /// The call reaches the raw `[agent.compaction]` table (or a default one)
    /// without passing through a resolver: the base window.
    Base,
    /// Neither is visible here; the value arrives as a parameter or a field
    /// resolved by the caller.
    Forwarded,
}

/// Text that proves the value went through the effective-window resolution.
const EFFECTIVE_COMPACTION_MARKERS: &[&str] = &["resolve_effective_compaction_config(", "compaction_resolver.resolve("];

/// Text that names the base window directly.
const BASE_COMPACTION_MARKERS: &[&str] = &[
    ".agent.compaction",
    "agent_compaction",
    "AgentCompactionConfig::default()",
];

/// Follow a compaction argument through at most four hops (the nearest `let`
/// binding of its leading identifier before the call, or a same-file `fn`,
/// including a `self.` method) until it names a resolver or the base table.
fn classify_compaction_argument(production: &str, expression: &str, call_start: usize) -> CompactionSource {
    let mut text = expression.to_string();
    for _ in 0..4 {
        if EFFECTIVE_COMPACTION_MARKERS.iter().any(|marker| text.contains(marker)) {
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
    "src/session_worker/runner.rs::run_owned_manifest",
    "src/tools/delegate.rs::execute_tool_loop",
    "src/tools/sessions_spawn.rs::run_sub_agent_task",
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
/// through a `CompactionResolver` built from the whole configuration
/// (INT-2B a46).
///
/// `CompactionResolver::from_base` ignores `[[router.models]]` and the model
/// routes, so a sub-agent tool left with it budgets every child on the
/// `[agent.compaction]` table whatever the model. The constructors start from
/// it as a placeholder; every production construction of `DelegateTool` or
/// `SessionsSpawnTool` must replace it with `.with_compaction_resolver(` in
/// the same expression. `EFFECTIVE_COMPACTION_MARKERS` counts any
/// `compaction_resolver.resolve(` as effective, so it cannot see this.
///
/// MUTATION GUARD: drop `.with_compaction_resolver(` from the delegate
/// registration in `src/tools/mod.rs` and the second inventory gains
/// `src/tools/mod.rs::all_tools_with_runtime_ext_and_extensions`.
#[test]
fn sub_agent_tools_resolve_compaction_from_the_whole_configuration() {
    let mut from_base = Vec::new();
    let mut without_resolver = Vec::new();
    let mut with_resolver = Vec::new();
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
                // The rest of the expression: up to its `;` or the end of the
                // enclosing block (a tail expression).
                let bytes = production.as_bytes();
                let mut depth = 0i32;
                let mut end = production.len();
                for (offset, byte) in bytes[at..].iter().enumerate() {
                    match byte {
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' => {
                            depth -= 1;
                            if depth < 0 {
                                end = at + offset;
                                break;
                            }
                        }
                        b';' if depth == 0 => {
                            end = at + offset;
                            break;
                        }
                        _ => {}
                    }
                }
                if production[at..end].contains(".with_compaction_resolver(") {
                    with_resolver.push(site(at));
                } else {
                    without_resolver.push(site(at));
                }
            }
        }
    }

    // The placeholders the constructors start from; each is replaced by
    // every production construction below.
    assert_inventory(
        "production CompactionResolver::from_base calls",
        from_base,
        &[
            "src/tools/delegate.rs::new",
            "src/tools/delegate.rs::with_depth",
            "src/tools/sessions_spawn.rs::new_with_registry",
        ],
    );
    assert_inventory(
        "production sub-agent tool constructions without .with_compaction_resolver(",
        without_resolver,
        &[],
    );
    assert_inventory(
        "production sub-agent tool constructions with .with_compaction_resolver(",
        with_resolver,
        &[
            "src/channels/startup.rs::start_channels_with_config",
            "src/chat/mod.rs::run",
            "src/gateway/mod.rs::register_gateway_turn_tools",
            "src/session_worker/runner.rs::worker_spawn_tool",
            "src/tools/mod.rs::all_tools_with_runtime_ext_and_extensions",
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
        "src/chat/mod.rs",
        "run",
        "Action::StartLLMTurn {",
        Some("compaction_config"),
    ),
    // `WorkerManifest.compaction_config` (process branch).
    (
        "src/tools/sessions_spawn.rs",
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
fn forwarded_compaction_values_are_produced_from_the_effective_window() {
    let mut classified = Vec::new();
    for (relative, function, anchor, field) in FORWARDED_COMPACTION_PRODUCERS {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
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
    for (relative, function, provider, model) in [
        (
            "src/runtime/turn_decision.rs",
            "agent_budget_snapshot",
            "proof.provider_name()",
            "proof.model_name()",
        ),
        (
            "src/runtime/turn_decision/routing.rs",
            "router_history_hop_inner",
            "admitted.provider_name()",
            "admitted.model_name()",
        ),
    ] {
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
            &repository_root().join(relative),
        )));
        let body = compact_rust_tokens(&function_body(&production, function));
        let resolver = format!(
            "let effective=crate::router::resolve_effective_compaction_config(&config.agent.compaction,{provider},{model},&config.router,&config.model_routes,);"
        );
        assert_eq!(
            body.matches(&compact_rust_tokens(&resolver)).count(),
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
fn production_turns_budget_on_the_effective_context_window() {
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
                        "src/session_worker/runner.rs::run_owned_manifest" => {
                            "child_turn.as_ref().map(|turn|turn.compaction()).or(compaction_config_ref)"
                        }
                        "src/tools/delegate.rs::execute_tool_loop"
                        | "src/tools/sessions_spawn.rs::run_sub_agent_task" => "Some(child_turn.compaction())",
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
            "src/chat/mod.rs::run",
            "src/gateway/api/sessions.rs::run_console_runtime_turn",
            "src/gateway/mod.rs::run_gateway_chat_with_multimodal",
        ],
    );
}

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
fn chat_route_decision_names_the_registry_root_count() {
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
fn context_handoff_has_one_production_owner_and_exact_lookup_is_registered() {
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
fn the_loop_engine_has_one_typed_entry() {
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
fn user_text_enters_histories_only_neutralized() {
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
    "src/chat/mod.rs::bounded_legacy_chat_compaction_audit_source",
    "src/multimodal.rs::prepare_messages_for_provider",
    // One-shot provider calls that build no loop history.
    "src/plugins/capabilities/provider.rs::chat_with_system",
    "src/plugins/runtime.rs::chat_provider",
    "src/tools/image.rs::execute",
    // `run_sub_agent_task`: the operator steering turn, which opens with `[Steering instruction from
    // operator]` and so can never open with a reserved marker.
    "src/tools/sessions_spawn.rs::run_sub_agent_task",
    // The session worker's steering turn, the same wrapper.
    "src/session_worker/runner.rs::run_segments_with_steering",
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
fn loop_note_names_are_never_aliased_or_variant_imported() {
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
fn middleware_history_rebuilds_neutralize_user_messages() {
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
fn chat_message_constructors_are_pinned() {
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

/// The files whose log lines name providers or `<provider>/<model>` targets
/// (INT-2B USERINFO-3, B4f).
const LABEL_LOG_FILES: &[&str] = &[
    "src/agent/agent.rs",
    "src/agent/turn_engine.rs",
    "src/chat/mod.rs",
    "src/router/context.rs",
    "src/router/mod.rs",
    "src/tools/delegate.rs",
    "src/tools/sessions_spawn.rs",
];

/// Log fields that name a provider or a `<provider>/<model>` target.
const LABEL_LOG_FIELDS: &[&str] = &[
    "provider",
    "provider_name",
    "default_provider",
    "reachable_providers",
    "planned_model",
    "actual_model",
    "escalate_to",
    "selected_provider",
    "chosen_provider",
];

/// Bindings that hold a provider label or a `<provider>/<model>` target.
const LABEL_BINDINGS: &[&str] = &[
    "provider",
    "provider_name",
    "default_provider",
    "reachable_providers",
    "planned_model",
    "actual_model",
    "premium_model",
    "effective_model",
    "selected_provider",
    "chosen_provider",
];

/// Every argument of every `tracing` event macro in `production` as
/// `(offset, field, value)`: `field` is the name of a `name = value` field
/// (empty for a positional argument or the format string), and a shorthand
/// field (`provider,`) has the identifier as both name and value.
fn log_macro_arguments(production: &str) -> Vec<(usize, String, String)> {
    let bytes = production.as_bytes();
    let mut arguments = Vec::new();
    for level in ["trace!(", "debug!(", "info!(", "warn!(", "error!("] {
        let mut search = 0usize;
        while let Some(found) = production[search..].find(level) {
            let start = search + found;
            search = start + level.len();
            if start > 0 && is_identifier_byte(bytes[start - 1]) {
                continue;
            }
            let open = start + level.len() - 1;
            let mut depth = 0i32;
            let mut segment_start = open + 1;
            for index in open..bytes.len() {
                match bytes[index] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' | b',' if depth == 1 => {
                        let segment = production[segment_start..index].trim();
                        if !segment.is_empty() {
                            let (field, value) = match segment.split_once('=') {
                                Some((name, value))
                                    if !value.starts_with('=')
                                        && !name.trim().is_empty()
                                        && name.trim().bytes().all(|byte| is_identifier_byte(byte) || byte == b'.') =>
                                {
                                    (name.trim().to_string(), value.trim().to_string())
                                }
                                _ => {
                                    let bare = segment.trim_start_matches(['%', '?']);
                                    if bare.bytes().all(is_identifier_byte) {
                                        (bare.to_string(), bare.to_string())
                                    } else {
                                        (String::new(), segment.to_string())
                                    }
                                }
                            };
                            arguments.push((segment_start, field, value));
                        }
                        segment_start = index + 1;
                        if bytes[index] != b',' {
                            break;
                        }
                    }
                    b')' | b']' | b'}' => depth -= 1,
                    _ => {}
                }
            }
        }
    }
    arguments.sort_by_key(|(offset, _, _)| *offset);
    arguments
}

/// Whether a log value shows a label only in its redacted form.
fn log_value_is_redacted(value: &str) -> bool {
    value.contains("redact_endpoint_userinfo(")
        || value.contains(".shown_provider()")
        || value
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|identifier| identifier.starts_with("shown_"))
}

/// The log arguments of `production` that print a provider label raw: a
/// label field, or any argument naming a label binding, whose value is not
/// redacted.
fn raw_label_log_arguments(relative: &str, production: &str) -> Vec<String> {
    let lines: Vec<&str> = production.lines().collect();
    log_macro_arguments(production)
        .into_iter()
        .filter(|(_, field, value)| {
            let names_label = value
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .any(|identifier| LABEL_BINDINGS.contains(&identifier));
            (LABEL_LOG_FIELDS.contains(&field.as_str()) || names_label) && !log_value_is_redacted(value)
        })
        .map(|(offset, field, value)| {
            let line_index = production[..offset].matches('\n').count();
            format!(
                "{relative}::{} `{field} = {}`",
                enclosing_function(&lines, line_index),
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect()
}

/// No log line in [`LABEL_LOG_FILES`] prints a provider label or a
/// `<provider>/<model>` target raw (INT-2B USERINFO-3, B4f M-2).
///
/// Checked site by site: every argument of every `tracing` event macro whose
/// field is a label field, or whose value names a label binding, must show
/// the value through `redact_endpoint_userinfo`, `shown_provider()` or a
/// `shown_*` binding. Reverting any one redaction turns this red, also where
/// one function has several. What it cannot see is a label under a name not
/// in [`LABEL_BINDINGS`], or a log line outside these files.
///
/// MUTATION GUARD: put `planned_model` back as a shorthand field in
/// `RouterEngine::record_outcome_detailed` and the inventory gains it.
#[test]
fn provider_labels_never_reach_a_log_field_raw() {
    let mut raw = Vec::new();
    for relative in LABEL_LOG_FILES {
        let path = repository_root().join(relative);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        raw.extend(raw_label_log_arguments(relative, &production));
    }
    assert_inventory("log arguments that print a provider label raw", raw, &[]);
}

/// The argument scanner behind `provider_labels_never_reach_a_log_field_raw`
/// sees every field form (INT-2B B4f).
#[test]
fn raw_label_log_arguments_sees_every_field_form() {
    let production = "fn f() {\n    tracing::warn!(provider, model, \"x\");\n    tracing::info!(provider = %p, default_provider = default_provider.as_str(), \"y\");\n    warn!(planned_model = ?planned_model);\n    debug!(\"z {}\", provider_name);\n    info!(provider = %redact_endpoint_userinfo(p), x = %shown_reachable.len(), y = %c.shown_provider());\n    error!(escalate_to = premium_model.as_str(), n = 1);\n}\n";
    let found = raw_label_log_arguments("src/x.rs", production);
    assert_eq!(
        found,
        vec![
            "src/x.rs::f `provider = provider`".to_string(),
            "src/x.rs::f `provider = %p`".to_string(),
            "src/x.rs::f `default_provider = default_provider.as_str()`".to_string(),
            "src/x.rs::f `planned_model = ?planned_model`".to_string(),
            "src/x.rs::f `provider_name = provider_name`".to_string(),
            "src/x.rs::f `escalate_to = premium_model.as_str()`".to_string(),
        ]
    );
}

/// Provider labels are redacted at their sinks, and each sink keeps doing it.
///
/// A provider label may be a URL carrying `user:password@`
/// (`custom:http://alice:pw@host/v1`). USERINFO-REDACT and USERINFO-2 redact
/// it once per sink, with the one helper `providers::redact_endpoint_userinfo`,
/// instead of at every print: the failover chain stores its member labels
/// redacted, every observer backend sees redacted events, every hook payload
/// (command, WASM lifecycle and event bus alike), every conversation event's
/// `<provider>/<model>` counterpart, every persisted provider
/// outcome and every `prx doctor` item is redacted where it is built or
/// written. This guard pins that each of those sinks still calls the helper,
/// so deleting one redaction fails here and not only in its behaviour test.
///
/// What it cannot do is catch a *new* print of a raw label outside these
/// sinks. Labels are plain `String` identity keys (configuration, credential
/// scoping, route selection, caches), and a textual scan for format arguments
/// cannot tell a label from any other string or follow a rebinding. A
/// redacting newtype would only help once no raw `String` label can reach a
/// formatter, which means retyping every label from the configuration inward;
/// that is out of scope for this change.
#[test]
fn provider_label_sinks_redact_url_userinfo() {
    fn body_after<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("missing production item `{signature}`"));
        let open = start
            + source[start..]
                .find('{')
                .unwrap_or_else(|| panic!("missing body for `{signature}`"));
        let mut depth = 0usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return &source[start..=open + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated production item `{signature}`")
    }

    let root = repository_root();
    let sinks: &[(&str, &str, &str)] = &[
        (
            "src/providers/reliable.rs",
            "pub fn new(providers: Vec<(String, Box<dyn Provider>)>",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/providers/reliable.rs",
            "pub fn with_unavailable_providers(",
            "redact_endpoint_userinfo(",
        ),
        // INT-2B USERINFO-3 (providers): the factory's `Unknown provider`
        // error names the label it could not build.
        (
            "src/providers/mod.rs",
            "fn create_provider_with_url_and_options(",
            "redact_endpoint_userinfo(name)",
        ),
        (
            "src/security/redaction.rs",
            "pub fn redact_secrets_with(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/observability/mod.rs",
            "pub fn create_observer(",
            "RedactingObserver",
        ),
        (
            "src/observability/mod.rs",
            "fn redact_in_place(",
            "redact_endpoint_userinfo(",
        ),
        // The fan-out point: command hooks, WASM lifecycle hooks and the
        // plugin event bus all receive the payload `emit` redacted.
        ("src/hooks/mod.rs", "pub async fn emit(", "redacted_hook_payload("),
        (
            "src/hooks/mod.rs",
            "fn redacted_hook_payload(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/memory/fabric.rs",
            "fn redact_provider_label(",
            "redact_endpoint_userinfo(",
        ),
        // Only provider labels: identity senders are stored as given.
        ("src/memory/fabric.rs", "fn redact_provider_label(", "byo_dialect_of("),
        (
            "src/llm/route_decision.rs",
            "pub async fn record_route_decision_event(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub async fn record_provider_outcome_events(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub fn route_event_scope(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub fn redact_provider_labels(",
            "redact_endpoint_userinfo(",
        ),
        // The URL redactor (userinfo, query values and fragments, D-095 N-1)
        // includes the userinfo redaction.
        ("src/doctor/mod.rs", "fn with_state(", "redact_url_components("),
        // AG-12 (CDP-08): the decision factory's status shows a backend's
        // provider label redacted.
        (
            "src/decision/factory.rs",
            "fn shown_identity(",
            "redact_endpoint_userinfo(",
        ),
        ("src/decision/factory.rs", "pub fn status(", "shown_identity("),
        // INT-2B USERINFO-3: values built outside a log macro. Every log
        // argument of the label-bearing files is checked site by site by
        // `provider_labels_never_reach_a_log_field_raw`.
        (
            "src/router/mod.rs",
            "fn rank_reachable_models(",
            "decision_log_candidates(&result)",
        ),
        (
            "src/router/mod.rs",
            "fn decision_log_candidates(",
            "redact_endpoint_userinfo(&candidate.provider)",
        ),
        (
            "src/agent/embedded_source.rs",
            "pub(super) async fn record_automix_budget_refusal(",
            "redact_endpoint_userinfo(premium)",
        ),
        (
            "src/tools/sessions_spawn.rs",
            "async fn execute_spawn_with_id(",
            "sanitize_api_error(",
        ),
        (
            "src/router/context.rs",
            "pub fn shown_provider(",
            "redact_endpoint_userinfo(&self.selected_provider)",
        ),
    ];
    for (relative, signature, required) in sinks {
        let source = read_source(&root.join(relative));
        let production = strip_cfg_test_items(&source);
        let body = body_after(&production, signature);
        assert!(
            body.contains(required),
            "{relative}: `{signature}` no longer calls `{required}`; provider labels leaving this sink would \
             carry URL userinfo again"
        );
    }

    // Both write paths of the conversation-event fabric store the redacted
    // sender and recipient.
    terminal_provider_labels_are_redacted();

    let fabric = strip_cfg_test_items(&read_source(&root.join("src/memory/fabric.rs")));
    assert_eq!(
        fabric
            .matches("sender: scope.sender.map(redact_provider_label)")
            .count(),
        2,
        "a MemoryFabric write path stores the raw sender"
    );
    assert_eq!(
        fabric
            .matches("recipient: scope.recipient.map(redact_provider_label)")
            .count(),
        2,
        "a MemoryFabric write path stores the raw recipient"
    );
}

// Inspect executable structure, with comments and literals masked. The
// public wrapper must only delegate, and both distinct outcome copies must
// be sanitized before their first persistence or settlement boundary.
fn terminal_provider_labels_are_redacted() {
    let source = read_source(&repository_root().join("src/agent/terminal.rs"));
    let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
    assert_eq!(
        production.matches("pub async fn finalize_turn<C>(").count(),
        1,
        "review the real public finalizer signature before changing its guard"
    );
    let wrapper_source = production.replace("pub async fn finalize_turn<C>(", "pub async fn finalize_turn(");
    let wrapper = function_body(&wrapper_source, "finalize_turn");
    let wrapper = braced_block(&wrapper, 0).expect("public terminal body");
    let wrapper: String = wrapper.split_whitespace().collect();
    assert_eq!(
        wrapper,
        "{letcommit=commit.try_into().map_err(Into::into)?;finalize_turn_impl(fabric,commit,cost_config,workspace_dir,false).await}",
        "public finalizer must forward directly to the sanitized implementation"
    );
    let implementation = function_body(&production, "finalize_turn_impl");
    let compact: String = implementation.split_whitespace().collect();
    let claim_start = compact
        .find("ifletSome(outcome)=claim_outcome.as_mut()")
        .expect("claim outcome mutable branch");
    assert_eq!(
        braced_block(&compact, claim_start),
        Some("{outcome.redact_provider_labels();}"),
        "claim outcome must redact its own provider labels"
    );
    let claim_clone = compact
        .find("letmutclaim_outcome=commit.provider_outcome.clone();")
        .expect("claim outcome must be cloned from the actual commit");
    let semantic = compact
        .find("letsemantic=redact_persisted_json(")
        .expect("redacted claim digest");
    let claim_write = compact
        .find("letclaim=fabric.record_runtime_event_idempotent(")
        .expect("claim write");
    assert!(claim_clone < claim_start && claim_start < semantic && semantic < claim_write);

    let persisted_start = compact
        .find("ifletSome(outcome)=commit.provider_outcome.as_mut()")
        .expect("persisted outcome mutable branch");
    assert_eq!(
        braced_block(&compact, persisted_start),
        Some(
            "{outcome.redact_provider_labels();*outcome=crate::security::redaction::redact_json_structure(&*outcome)?;}"
        ),
        "persisted outcome must redact its own labels and sensitive fields"
    );
    let settlement = compact.find("letusage_settlement=").expect("usage settlement");
    assert!(claim_write < persisted_start && persisted_start < settlement);
}

#[test]
fn terminal_provider_label_guard_rejects_bypass_missing_calls_and_literal_decoys() {
    assert!(!scanner_fails_with(
        provider_label_sinks_redact_url_userinfo,
        Vec::new()
    ));
    let path = "src/agent/terminal.rs";
    let source = fs::read_to_string(repository_root().join(path)).expect("read untouched terminal mutation control");
    let forwarding = "finalize_turn_impl(fabric, commit, cost_config, workspace_dir, false).await";
    assert_eq!(source.matches(forwarding).count(), 1);
    let bypass = source.replace(forwarding, &format!("return Ok(receipt); {forwarding}"));
    assert!(scanner_fails_with(
        provider_label_sinks_redact_url_userinfo,
        vec![(path.into(), bypass)]
    ));
    assert_eq!(source.matches("outcome.redact_provider_labels();").count(), 2);
    for occurrence in [0, 1] {
        let start = source
            .match_indices("outcome.redact_provider_labels();")
            .nth(occurrence)
            .expect("real branch")
            .0;
        for replacement in [
            "",
            "// outcome.redact_provider_labels();\n",
            "let _ = \"outcome.redact_provider_labels();\";",
        ] {
            let mut changed = source.clone();
            changed.replace_range(start..start + "outcome.redact_provider_labels();".len(), replacement);
            assert!(scanner_fails_with(
                provider_label_sinks_redact_url_userinfo,
                vec![(path.into(), changed)]
            ));
        }
    }
}

/// A session worker must select its tool surface from its own manifest.
///
/// `run_owned_manifest` used to read `manifest.allowed_tools` and
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

    let entry = function_body(&production, "run_owned_manifest");
    assert!(
        entry.contains("select_tools_for_manifest(full_tools, &manifest)"),
        "run_owned_manifest no longer selects the worker's tools from its manifest; a list that never \
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
/// `gateway::tests::tool_surface::both_gateway_launch_paths_publish_the_same_tool_surface`
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
        3,
        "decorator, three sites, each inside an already-gated ToolExecutionService call: \
         `SkillReadTool::execute_named` re-enters its own `self.execute` twice (the exact name, \
         and a catalog alias rewritten into its `name` argument), and \
         `SkillExecuteTool::execute_named` forwards to `execute_named_with_cancellation`",
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
const fn receiver_is_local_binding(characters: &[char], dot: usize) -> bool {
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
        entry: "src/agent/embedded_turn.rs::run_work",
        receiver: "execution",
        occurrences: 1,
        evidence_file: "src/runtime/turn_decision/execution.rs",
        evidence: ") -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {",
        reason: "bind_agent_execution returns the actual shared execution service and its source-bound context; this is the single admitted configured-delegation dispatch",
    },
    NonToolCallSite {
        entry: "src/runtime/turn_decision/execution.rs::execute_agent_resume",
        receiver: "dispatch.service",
        occurrences: 1,
        evidence_file: "src/runtime/turn_decision/execution.rs",
        evidence: "service: Arc<ToolExecutionService>,",
        reason: "sealed ResumeDispatch carries the actual ToolExecutionService; resume invokes exactly the approved command with the issued context",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/ingress_recovery.rs::prepare_ingress_schema",
        receiver: "session.client",
        occurrences: 1,
        evidence_file: "src/memory/postgres/ingress_recovery.rs",
        evidence: "client: Client,",
        reason: "private Session owns a tokio-postgres Client; this one SQL execution installs the bounded session timeout",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/ingress_recovery.rs::new_ingress_lifetime",
        receiver: "session.client",
        occurrences: 3,
        evidence_file: "src/memory/postgres/ingress_recovery.rs",
        evidence: "client: Client,",
        reason: "private Session owns the actual tokio-postgres Client; three SQL executions persist backend identity and acquire or reclaim lifetime fencing",
    },
    NonToolCallSite {
        entry: "src/agent/tool_scheduler.rs::spawn_delegation_tool",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/agent/tool_scheduler.rs",
        evidence: "service: Arc<crate::tools::ToolExecutionService>,",
        reason: "the parameter is the shared approved execution service; the delegation task \
                 dispatches the already-built command through it on its own spawned task",
    },
    NonToolCallSite {
        entry: "src/agent/tool_scheduler.rs::execute_one_unrecorded",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/agent/turn_contracts.rs",
        evidence: "pub tool_execution_service: Option<Arc<crate::tools::ToolExecutionService>>",
        reason: "`let Some(service) = adapter.tool_execution_service.as_ref() else` - the tool \
                 loop's own handle on the approved execution service",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/pool.rs::acquire_reset",
        receiver: "client",
        occurrences: 1,
        evidence_file: "src/memory/postgres/pool.rs",
        evidence: "pub(crate) fn client_mut(&mut self) -> Result<&mut Client>",
        reason: "`checkout.client_mut().and_then(|client| client.execute(...))` resets the \
                 PostgreSQL session context on a pooled tokio-postgres client",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/access.rs::apply_rls_context_raw",
        receiver: "client",
        occurrences: 4,
        evidence_file: "src/memory/postgres/access.rs",
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
        evidence_file: "src/memory/postgres/pool.rs",
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
        let source = read_source(&path);
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
        let scan = scan_tool_executions("src/memory/sqlite/recall.rs", &source);
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
        let scan = scan_tool_executions("src/memory/sqlite/recall.rs", &source);
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
    assert!(
        non_tool_call_site_errors(&one.sites, &[exception]).is_empty(),
        "expected an empty value"
    );

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
    // Pin the two issued execution-service bindings, not merely a nearby type name.
    for (relative, function, binding) in [
        (
            "src/agent/embedded_turn.rs",
            "run_work",
            "let(execution,context)=service.bind_agent_execution(source,&decision,&context,&selected.specs)?;",
        ),
        (
            "src/runtime/turn_decision/execution.rs",
            "execute_agent_resume",
            "letdispatch=self.bind_resume_execution(proof,context,execution_context)?;",
        ),
    ] {
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
            &repository_root().join(relative),
        )));
        assert_eq!(
            compact_rust_tokens(&function_body(&production, function))
                .matches(binding)
                .count(),
            1
        );
    }
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
        let source = read_source(&path);
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

/// Every production function in `src/memory/` that builds a read
/// [`Principal`] from a `MemoryWriteContext`, with the number of principals it
/// constructs. Each construction must be followed by `.with_owner_scope`,
/// which is the single seam that copies the C03 / V02-R5 owner anchor and
/// isolation bit onto the resolved principal.
///
/// A new entry is a review event: either the function applies the anchor, or
/// an owner-isolated MCP caller silently reads every row the principal's scope
/// predicate admits.
///
/// Deliberately **not** listed, because they take no context and therefore
/// have no anchor to drop:
/// - `principal_from_policy` and `anonymous_principal`
///   (`src/memory/principal.rs`) interpret a policy row and a fallback
///   identity. Their doc comments state that owner isolation is a property of
///   the trusted context, and every caller that *has* a context is listed
///   below, so the scan reaches them through their callers.
/// - `sqlite_owner_read_principal` (`src/memory/sqlite/mod.rs`) is the backend's
///   own unrestricted principal for Observe-mode compatibility reads. It is
///   deliberately unanchored and is unreachable for an isolated caller, whose
///   mode is forced to Enforce before the choice is made.
const MEMORY_READ_PRINCIPAL_CONSTRUCTIONS: &[&str] = &[
    "src/memory/postgres/access.rs::principal_from_context::1",
    "src/memory/postgres/decision_reads.rs::decision_principal::1",
    "src/memory/postgres/access.rs::resolve_principal_from_context::2",
    "src/memory/principal.rs::resolve_principal::4",
    "src/memory/sqlite/mod.rs::sqlite_read_principal::1",
];

/// The three ways a read `Principal` is produced in `src/memory/`.
const PRINCIPAL_CONSTRUCTORS: &[&str] = &["Principal {", "principal_from_policy(", "anonymous_principal("];

/// Count occurrences of `needle` that start on an identifier boundary, so
/// `MemoryPrincipal {` and `OwnerPrincipal {` are not counted as `Principal {`.
fn count_constructor_occurrences(haystack: &str, needle: &str) -> usize {
    let characters: Vec<char> = haystack.chars().collect();
    let target: Vec<char> = needle.chars().collect();
    if target.is_empty() || target.len() > characters.len() {
        return 0;
    }
    (0..=characters.len() - target.len())
        .filter(|start| {
            characters[*start..*start + target.len()] == target[..]
                && (*start == 0 || !is_ident_char(characters[*start - 1]))
        })
        .count()
}

/// Every production `.rs` file under `src/memory/`, with comments, string
/// literals and `#[cfg(test)]` items removed. Blanking runs first so the brace
/// matching below cannot be thrown off by a brace inside a SQL `format!`.
fn memory_production_sources() -> Vec<(String, String)> {
    rust_source_files()
        .into_iter()
        .filter(|path| relative_path(path).starts_with("src/memory/"))
        .map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
            (relative, production)
        })
        .collect()
}

/// One `fn` item: its name, its signature and its body.
struct MemoryFunction {
    name: String,
    signature: String,
    body: String,
}

/// Split one production file into its `fn` items by brace matching.
fn memory_functions(production: &str) -> Vec<MemoryFunction> {
    let characters: Vec<char> = production.chars().collect();
    let mut functions = Vec::new();
    let mut index = 0usize;
    while index + 3 <= characters.len() {
        let is_fn_keyword = characters[index] == 'f'
            && characters[index + 1] == 'n'
            && characters.get(index + 2).is_some_and(|c| c.is_whitespace())
            && (index == 0 || !is_ident_char(characters[index - 1]));
        if !is_fn_keyword {
            index += 1;
            continue;
        }
        let name: String = characters[index + 3..]
            .iter()
            .skip_while(|character| character.is_whitespace())
            .take_while(|character| is_ident_char(**character))
            .collect();
        let Some(open) = (index..characters.len()).find(|offset| characters[*offset] == '{') else {
            break;
        };
        let signature: String = characters[index..open].iter().collect();
        let mut depth = 0usize;
        let mut close = open;
        for (offset, character) in characters.iter().enumerate().skip(open) {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth = depth.checked_sub(1).expect("brace depth underflow in src/memory");
                    if depth == 0 {
                        close = offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !name.is_empty() && close > open {
            functions.push(MemoryFunction {
                name,
                signature,
                body: characters[open..=close].iter().collect(),
            });
        }
        index = open.max(index + 1);
    }
    functions
}

#[test]
fn every_memory_read_principal_built_from_a_context_carries_the_owner_scope() {
    let mut actual = Vec::new();
    for (relative, production) in memory_production_sources() {
        for function in memory_functions(&production) {
            if !function.signature.contains("MemoryWriteContext") {
                continue;
            }
            let constructions: usize = PRINCIPAL_CONSTRUCTORS
                .iter()
                .map(|constructor| count_constructor_occurrences(&function.body, constructor))
                .sum();
            if constructions == 0 {
                continue;
            }
            // The needle carries no leading `.`: `count_constructor_occurrences`
            // requires a non-identifier character before it, so a receiver such
            // as `principal.with_owner_scope(context)` is matched whatever it is
            // named. With the dot in the needle, a refactor to a named local
            // turned this guard red for the wrong reason.
            let anchors = count_constructor_occurrences(&function.body, "with_owner_scope(");
            assert_eq!(
                anchors, constructions,
                "{relative}::{} builds {constructions} read principal(s) from a MemoryWriteContext but \
                 applies .with_owner_scope {anchors} time(s). Every one of them must carry the owner \
                 anchor, or an owner-isolated caller reads rows it does not own",
                function.name
            );
            actual.push(format!("{relative}::{}::{constructions}", function.name));
        }
    }
    assert_inventory(
        "read principals built from a MemoryWriteContext in src/memory",
        actual,
        MEMORY_READ_PRINCIPAL_CONSTRUCTIONS,
    );
}

/// The owner-isolation bit may only arrive from the trusted runtime context.
///
/// Both spellings that assert it are forbidden: the struct-literal form
/// `require_owner_match: true`, and the field-assignment form
/// `.require_owner_match = true`, which the literal check alone let through.
/// `Principal::with_owner_scope` copies the bit from a `MemoryWriteContext` and
/// is the one permitted source; it does not contain either spelling.
#[test]
fn the_owner_isolation_bit_is_never_asserted_by_memory_production_code() {
    for (relative, production) in memory_production_sources() {
        for spelling in ["require_owner_match: true", ".require_owner_match = true"] {
            assert!(
                !production.contains(spelling),
                "{relative} asserts the owner-isolation bit with `{spelling}`. It may only arrive from the \
                 trusted runtime context, through Principal::with_owner_scope"
            );
        }
    }
}

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
fn agent_entries_reach_provider_factories_only_through_the_scoped_selection() {
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
fn session_worker_credentials_are_decided_in_one_scoped_place() {
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
        vec!["src/session_worker/runner.rs:build_worker_provider_selection:create_provider_for".to_string()],
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
fn provider_construction_outside_the_provider_module_is_pinned() {
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
        "src/chat/mod.rs:chat_provider_chain:create_routed_provider_for",
        "src/daemon/mod.rs:build_evolution_judge_model:create_provider_for",
        "src/doctor/mod.rs:provider_validation_error:create_provider_for",
        "src/gateway/mod.rs:build_gateway_provider_selection:create_routed_provider_for",
        "src/runtime/child_source.rs:from_selection:create_provider_for",
        "src/runtime/turn_decision/auxiliary.rs:auxiliary_accounting:create_provider_for",
        "src/runtime/turn_decision/auxiliary.rs:smart_group_accounting:create_provider_for",
        "src/session_worker/runner.rs:build_worker_provider_selection:create_provider_for",
        "src/tools/sessions_spawn.rs:build_override_provider:create_provider_for",
        "src/tools/sessions_spawn.rs:execute_spawn_with_id:create_provider_for",
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
fn agent_loop_entries_resolve_the_model_once() {
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
fn no_label_keyed_or_folded_tool_schema_dialect_remains() {
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

/// The production functions outside the config resolver that look up the
/// user's home directory or a per-user platform directory, as
/// `file::function`. Everything else under the home (the default
/// `~/.openprx`, other tools' credential files, `~` in a path) goes through
/// the resolver, which the lib-test binary points at a scratch home.
const ALLOWED_HOME_DIRECTORY_LOOKUPS: &[&str] = &[
    // The resolver: the home directory and the platform config directory.
    "src/config/schema.rs::user_home_dir",
    "src/config/schema.rs::platform_config_dir",
    // Reports whether `HOME` is set at all; nothing is read under it.
    "src/doctor/mod.rs::check_environment",
    "src/runtime/shell_process.rs::<module>",
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
fn the_home_directory_is_resolved_only_by_the_config_resolver() {
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

// ---------------------------------------------------------------------------
// Open Causal Decision Plane: decision protocol guards (CDP-01; SP-CDP-C 3.4)
// ---------------------------------------------------------------------------

/// Occurrences of `Name {` in `production` that build a value of `name`: a
/// struct literal, not the type's own `struct`/`impl` header or a function's
/// `-> Name {` return type.
fn struct_literal_count(production: &str, name: &str) -> usize {
    let needle = format!("{name} {{");
    production
        .match_indices(&needle)
        .filter(|(start, _)| {
            let before = &production[..*start];
            if before
                .chars()
                .next_back()
                .is_some_and(|character| is_ident_char(character) || character == ':')
            {
                return false;
            }
            let previous = before.trim_end();
            !["struct", "impl", "enum", "for", "trait", "->", "dyn", "<", "&"]
                .iter()
                .any(|token| previous.ends_with(token))
        })
        .count()
}

/// AG-05. `MockDecisionBackend` is compiled only for tests: its module is
/// declared behind `cfg(any(test, feature = "test-mock"))`, and no production
/// code outside that module names it.
#[test]
fn mock_decision_backend_is_compiled_only_for_tests() {
    let mut sites = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative == "src/decision/mock.rs" {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for needle in ["MockDecisionBackend", "MockOutcome", "MockStep"] {
            if contains_identifier(&production, needle) {
                sites.push(format!("{relative}: {needle}"));
            }
        }
        if production.contains("decision::mock") {
            sites.push(format!("{relative}: decision::mock"));
        }
    }
    assert!(
        sites.is_empty(),
        "the scripted decision backend is test-only (AG-05); production code names it at {sites:?}"
    );
    let module = read_source(&repository_root().join("src/decision/mod.rs"));
    let lines: Vec<String> = module.lines().map(normalize_line).collect();
    let declarations: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.ends_with("mod mock;"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        declarations.len(),
        1,
        "src/decision/mod.rs declares `mod mock` exactly once"
    );
    assert!(
        declarations[0] > 0 && lines[declarations[0] - 1] == "#[cfg(any(test, feature = \"test-mock\"))]",
        "`mod mock` must sit directly behind `#[cfg(any(test, feature = \"test-mock\"))]`"
    );
}

/// The types that carry text a decision backend returned.
const DECISION_RESPONSE_TYPES: [&str; 6] = [
    "DecisionResponse",
    "DecisionAnswer",
    "ChoiceAnswer",
    "ScoreAnswer",
    "NoulAnswer",
    "ValidatedDecision",
];

/// AG-15 (invariant 16.13). No backend-returned text becomes a chat message:
/// the decision module never names `ChatMessage`, no production file that
/// handles a decision response constructs a chat message, and the wire codec
/// declares no field for free-form backend text, so an `explanation`,
/// `reason` or `reasoning` field is never decoded.
///
/// A textual guard: a message built from a response in a file that names
/// none of the response types (through a helper elsewhere) is not found.
#[test]
fn decision_answers_never_become_chat_messages() {
    const MESSAGE_CONSTRUCTORS: [&str; 4] = [
        "ChatMessage::system(",
        "ChatMessage::user(",
        "ChatMessage::assistant(",
        "ChatMessage {",
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        if relative.starts_with("src/decision/") {
            if contains_identifier(&production, "ChatMessage") {
                violations.push(format!("{relative}: the decision module names ChatMessage"));
            }
            continue;
        }
        let handles_responses = DECISION_RESPONSE_TYPES
            .iter()
            .any(|name| contains_identifier(&production, name));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        if handles_responses {
            for constructor in MESSAGE_CONSTRUCTORS {
                if compact.contains(constructor) {
                    violations.push(format!("{relative}: builds `{constructor}` beside a decision response"));
                }
            }
        }
    }
    let wire = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/wire.rs"),
    )));
    for field in ["explanation", "reason", "reasoning", "rationale"] {
        if contains_identifier(&wire, field) {
            violations.push(format!("src/decision/wire.rs: declares or reads `{field}`"));
        }
    }
    assert!(
        violations.is_empty(),
        "backend-returned text must never reach a chat message (AG-15): {violations:?}"
    );
}

/// AG-22. The path from backend bytes to a validated decision never repairs
/// a value: no normalizing, rescaling or clamping call anywhere in
/// `src/decision/**` (D-057 L-7: every decision file, not only the codec and
/// the validator). The property corpus in
/// `tests/proptest_decision_validate.rs` proves the bits are kept.
///
/// The scan is textual, so a helper outside `src/decision/` escapes it. That
/// is allowed only for code that *classifies request text*, never for code
/// that changes a decision value (D-061 amendment 2). The one such helper,
/// `crate::util::is_combining_mark`, reads a Unicode character class for the
/// builtin's structure scan. A wrapper that normalizes, rescales, clamps or
/// repairs a probability, level or answer anywhere is a violation of this
/// guard, whatever file it lives in, and cannot be accepted by analogy.
#[test]
fn the_decision_validator_never_repairs_a_value() {
    const REPAIRS: [&str; 5] = ["normaliz", "normalis", "rescale", "clamp(", "repair"];
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path))).to_ascii_lowercase();
        for repair in REPAIRS {
            if production.contains(repair) {
                violations.push(format!("{relative}: {repair}"));
            }
        }
    }
    assert!(scanned >= 7, "the decision module has at least its seven CDP-01 files");
    assert!(
        violations.is_empty(),
        "decision responses are validated, never repaired (AG-22): {violations:?}"
    );
}

/// The paths into the rest of the crate that the causal domain may name, as
/// `crate::` prefixes (AG-01). Everything else is out of reach: a decision
/// backend (`decision::compatible_http`, `CompatibleHttpBackend`, CDP-07's
/// `decision::http`), a chat provider, a client builder, a runtime service.
/// The one redactor is `security::redaction` (D-054, D-064).
const CAUSAL_DOMAIN_CRATE_PATHS: &[&str] = &[
    "causal_tree::",
    // The configuration names a backend kind and checks an endpoint's
    // spelling (CDP-03); neither opens a connection.
    "decision::BackendKind",
    "decision::loopback::",
    "decision::model_pin::is_unpinned_model",
    "decision::CandidateKey",
    "decision::DecisionAnswer",
    "decision::DecisionError",
    "decision::DecisionKey",
    "decision::DecisionRequest",
    "decision::KeyDomain",
    // The credential reference grammar (CDP-12): a pure check of the id.
    "config::manage::credentials::CredentialReferences::reference",
    // The store's directory passes the key's directory checks (D-091 F-1).
    "decision::key::check_decision_dir",
    "decision::key::prepare_decision_dir",
    "decision::QuestionId",
    "decision::StructuredValue",
    "decision::ValidatedDecision",
    "decision::builtin::weights::FRESH_ARTIFACT_MS",
    "decision::builtin::weights::STALE_ARTIFACT_MS",
    "observability::Observer",
    "observability::ObserverEvent",
    "observability::ObserverMetric",
    // CDP-09..11 (store, evaluation, admin): the decision store reports
    // through the causal event sink and uses the bounded busy handler; the
    // admin service reads the configuration and the factory the caller
    // builds (its `DecisionPlaneBuilder`: the transports and the catalog
    // stay outside the domain), asks it the neutral fixture, and shows
    // provider labels redacted. None of these opens a connection.
    "config::Config",
    "decision::telemetry::CausalEvent",
    "decision::telemetry::CausalEventFields",
    "decision::telemetry::CausalEventKind",
    "decision::telemetry::CausalEventSink",
    "decision::telemetry::CausalLineage",
    "decision::telemetry::IncompleteReason",
    "decision::factory::AttemptOutcome",
    // The one reading of the configured backend (CDP-14's live-test target).
    "decision::factory::configured_selection",
    "decision::factory::DecisionBackendFactory",
    "decision::factory::FactoryBuildError",
    "decision::factory::FactoryDeps",
    "decision::factory::HealthView",
    "decision::factory::ServedBy",
    "decision::BackendLocality",
    "decision::DecisionQuestion",
    "decision::GlobalQuestion",
    "decision::OptionId",
    "decision::RequestLimits",
    "decision::RequestParts",
    "decision::builtin::weights::MODEL_VERSION",
    "providers::redact_endpoint_userinfo",
    "runtime::sqlite_pool::install_bounded_busy_handler",
    "security::redaction::redact_json_value",
    "security::redaction::redact_secrets",
];

// CDP-18's admission and enumeration ports read immutable catalog and policy
// snapshots. Keep these imports local to those two files: state, projection,
// and backend-free domain types retain the narrower AG-01 allowlist.
const CAUSAL_GATE_CRATE_PATHS: &[&str] = &[
    "capability::CapabilityAvailabilityLevel",
    "security::AutonomyLevel",
    "security::SecurityPolicy",
    "tools::capability::catalog::CapabilityEntry",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
    "tools::capability::child::CapabilityConstraints",
    "tools::capability::child::ChildCapabilityManifest",
    "tools::execution::ToolExecutionDecision",
    "tools::execution::decide_tool_execution",
];

const CAUSAL_GENERATOR_CRATE_PATHS: &[&str] = &[
    "tools::capability::catalog::CapabilityEntry",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
];

// CDP-19 constructs a sealed, provider-neutral request from typed protocol
// values. This allowance belongs only to the question compiler.
const CAUSAL_QUESTION_CRATE_PATHS: &[&str] = &[
    "decision::CandidateQuestion",
    "decision::DecisionError",
    "decision::DecisionKey",
    "decision::DecisionQuestion",
    "decision::DecisionRequest",
    "decision::GlobalQuestion",
    "decision::OmissionReason",
    "decision::OptionId",
    "decision::ProjectionAudit",
    "decision::QuestionId",
    "decision::RequestLimits",
    "decision::RequestParts",
    "decision::RequestRule",
    "decision::StructuredValue",
];

// CDP-20's prediction reader stays protocol-only; it cannot import a
// backend, transport or live catalog.
const CAUSAL_SCORE_CRATE_PATHS: &[&str] = &[
    "decision::CandidateQuestion",
    "decision::OmissionReason",
    "decision::ScoreAnswer",
];

// The bounded rehearsal sees only the pinned catalog snapshot.
const CAUSAL_REHEARSAL_CRATE_PATHS: &[&str] = &["tools::capability::catalog::CatalogView"];

// The engine is the one causal-domain caller of the factory. Its selection
// inputs are a validated result and an immutable catalog view, not a client.
const CAUSAL_ENGINE_CRATE_PATHS: &[&str] = &[
    "capability::CapabilityAvailabilityLevel",
    "decision::factory::PredictionResult",
    "security::AutonomyLevel",
    "security::SecurityPolicy",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
    "tools::capability::child::CapabilityConstraints",
    "tools::capability::child::ChildCapabilityManifest",
    "tools::capability::child::ChildManifestError",
    "tools::execution::ToolExecutionDecision",
    "tools::execution::decide_tool_execution",
    "tools::capability::search::CatalogSearch",
];

// Outcome records retain typed contracts without performing I/O. Only this
// file may name these dependencies; execution and service construction stay closed.
const CAUSAL_OUTCOME_CRATE_PATHS: &[&str] = &[
    "agent::terminal::VerifiedTerminalReplay",
    "tools::capability::execution_trace::ExecutionTraceSnapshot",
    "runtime::turn_source::ingress_event_surface",
    "runtime::turn_source::subject_task_id",
    "agent::terminal::TurnTerminalReceipt",
    "agent::terminal::TurnTerminalStatus",
    "cost::types::CostSettlement",
    "cost::tracker::ceil_micro_usd",
    "decision::factory::FallbackReason",
    "decision::PROTOCOL_VERSION",
    "llm::route_decision::AttemptStatus",
    "llm::route_decision::ExecutionStatus",
    "llm::route_decision::MeteredTokenUsageRecord",
    "llm::route_decision::ProviderExecutionOutcome",
    "memory::MessageEvent",
    "memory::MessageEventSource::Agent",
    "runtime::turn_decision::TurnDecisionContext",
];

// The directive carries a sealed child capability manifest to the runtime;
// only this conversion module may name the manifest and its constraints.
const CAUSAL_DIRECTIVE_CRATE_PATHS: &[&str] = &["tools::capability::child::ChildCapabilityManifest"];

/// AG-24 and AG-26: decisions may use an absolute deadline and may describe
/// an approval scope, but they cannot create a nested duration timeout or an
/// authenticated approval response/grant. Test-only fixtures are excluded.
#[test]
fn decision_domain_has_no_constant_timeout_or_approval_grant_constructor() {
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") && !relative.starts_with("src/causal_tree/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for forbidden in [
            "tokio::time::timeout(",
            "ApprovalResponse",
            "ApprovalGrantV2",
            "issue_one_shot(",
            "sign_grant(",
        ] {
            if production.contains(forbidden) {
                violations.push(format!("{relative}: {forbidden}"));
            }
        }
    }
    assert!(scanned > 0, "decision-domain guard scanned no production source");
    assert!(
        violations.is_empty(),
        "decision-domain boundary violations: {violations:?}"
    );
}

/// Names the causal domain must never use, however it reaches them: network
/// clients and sockets, the proxy-aware client builders and the chat
/// provider factory. (The chat `Provider` trait lives under
/// `crate::providers`, which the path allowlist already refuses; the name
/// itself is also an observed step kind, so it is not banned as a word.)
const CAUSAL_DOMAIN_FORBIDDEN_NAMES: [&str; 25] = [
    "reqwest",
    "hyper",
    "ureq",
    "isahc",
    "curl",
    "TcpStream",
    "TcpListener",
    "TcpSocket",
    "UdpSocket",
    "UnixStream",
    "UnixListener",
    "UnixDatagram",
    "ToSocketAddrs",
    "lookup_host",
    // Processes (a `curl` subprocess is a network client too).
    "process",
    "Command",
    // Websocket, MCP and mail clients.
    "tungstenite",
    "tokio_tungstenite",
    "connect_async",
    "rmcp",
    "lettre",
    "build_runtime_proxy_client",
    "build_runtime_proxy_client_with_timeouts",
    "apply_runtime_proxy_to_builder",
    "create_provider",
];

/// Where a crate-rooted path in a causal file starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathRoot {
    /// `crate::`, or `super::` in `src/causal_tree/mod.rs`, where it is the
    /// crate root.
    Crate,
    /// `super::super::`: the crate root from a top-level causal file, or the
    /// causal domain itself from a module nested in one.
    Relative,
}

/// Every path a `use` tree or a qualified path names from the crate root,
/// with its root, and with brace groups expanded: `crate::decision::{A, b::C}`
/// gives `decision::A` and `decision::b::C`; a glob gives `...::*`. In
/// `mod.rs`, `super::` is the crate root too (D-066 N-5).
fn crate_rooted_paths(production: &str, is_mod_rs: bool) -> Vec<(PathRoot, String)> {
    let mut paths = Vec::new();
    let mut roots = vec![("crate::", PathRoot::Crate), ("super::super::", PathRoot::Relative)];
    if is_mod_rs {
        roots.push(("super::", PathRoot::Crate));
    }
    for (root, kind) in roots {
        let mut search = 0;
        while let Some(found) = production[search..].find(root) {
            let start = search + found;
            search = start + root.len();
            let before = production[..start].chars().next_back();
            if before.is_some_and(|character| character.is_alphanumeric() || character == '_' || character == ':') {
                continue;
            }
            if root == "super::" && production[search..].starts_with("super::") {
                continue;
            }
            let mut found_paths = Vec::new();
            use_tree_paths(production, search, "", &mut found_paths);
            paths.extend(found_paths.into_iter().map(|path| (kind, path)));
        }
    }
    paths
}

/// The paths of the use tree that starts at byte `at`, each prefixed with
/// `prefix`; returns where the tree ends.
fn use_tree_paths(text: &str, at: usize, prefix: &str, paths: &mut Vec<String>) -> usize {
    let bytes = text.as_bytes();
    let skip_space = |mut index: usize| {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        index
    };
    let mut index = skip_space(at);
    if index < bytes.len() && bytes[index] == b'{' {
        index += 1;
        loop {
            index = skip_space(index);
            if index >= bytes.len() {
                return index;
            }
            match bytes[index] {
                b'}' => return index + 1,
                b',' => index += 1,
                _ => {
                    let end = use_tree_paths(text, index, prefix, paths);
                    index = end.max(index + 1);
                }
            }
        }
    }
    if index < bytes.len() && bytes[index] == b'*' {
        paths.push(format!("{prefix}*"));
        return index + 1;
    }
    let start = index;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
        index += 1;
    }
    let segment = &text[start..index];
    if segment.is_empty() {
        paths.push(prefix.to_string());
        return index;
    }
    let path = format!("{prefix}{segment}");
    let after = skip_space(index);
    if text[after..].starts_with("::") {
        return use_tree_paths(text, after + 2, &format!("{path}::"), paths);
    }
    paths.push(path);
    index
}

/// AG-01 (I-04.3; D-064 M-5). The causal domain is pure: building,
/// projecting and fingerprinting a turn state never reaches the network. It
/// reaches a backend only through `DecisionBackend`, from the caller. The
/// guard is an allowlist, so a new route out cannot pass by being unnamed:
///
/// - every path a production file under `src/causal_tree/**` names from the
///   crate root (`crate::`, or `super::super::` from a top-level file,
///   through `use` trees, aliases and qualified paths alike) must start with
///   an entry of [`CAUSAL_DOMAIN_CRATE_PATHS`]; a glob or a bare module
///   (`use crate::decision as d`) is refused;
/// - none of [`CAUSAL_DOMAIN_FORBIDDEN_NAMES`] appears as an identifier;
/// - no `#[path]` attribute or `include!` pulls another file in.
#[test]
fn the_causal_domain_imports_no_backend_and_no_network() {
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    let causal_modules: Vec<String> = rust_source_files()
        .iter()
        .map(|path| relative_path(path))
        .filter_map(|relative| {
            relative
                .strip_prefix("src/causal_tree/")
                .and_then(|file| file.strip_suffix(".rs"))
                .filter(|stem| !stem.contains('/') && *stem != "mod")
                .map(str::to_string)
        })
        .collect();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/causal_tree/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (root, named) in crate_rooted_paths(&production, relative == "src/causal_tree/mod.rs") {
            // From a module nested inside a causal file (a test module under
            // `cfg(all(test, ..))`, which the scan keeps), `super::super::`
            // is the causal domain itself, not the crate root. Only such a
            // relative path is read that way; `crate::<name>` never is
            // (D-066 N-5).
            if root == PathRoot::Relative
                && causal_modules
                    .iter()
                    .any(|module| named == *module || named.starts_with(&format!("{module}::")))
            {
                continue;
            }
            let file_paths = match relative.as_str() {
                "src/causal_tree/gates.rs" => CAUSAL_GATE_CRATE_PATHS,
                "src/causal_tree/candidate_generator.rs" => CAUSAL_GENERATOR_CRATE_PATHS,
                "src/causal_tree/questions.rs" => CAUSAL_QUESTION_CRATE_PATHS,
                "src/causal_tree/decision_score.rs" => CAUSAL_SCORE_CRATE_PATHS,
                "src/causal_tree/decision_rehearsal.rs" => CAUSAL_REHEARSAL_CRATE_PATHS,
                "src/causal_tree/decision_engine.rs" => CAUSAL_ENGINE_CRATE_PATHS,
                "src/causal_tree/directive.rs" => CAUSAL_DIRECTIVE_CRATE_PATHS,
                "src/causal_tree/outcome.rs" => CAUSAL_OUTCOME_CRATE_PATHS,
                "src/causal_tree/admin.rs" => &[
                    "config::files::with_consistent_config_snapshot",
                    "config::files::read_merged_toml_with_gate_once",
                    "config::deprecations::file_holding",
                ],
                "src/causal_tree/store/child_outcome.rs" => &["agent::terminal::VerifiedTerminalReplay"],
                _ => &[],
            };
            let allowed = CAUSAL_DOMAIN_CRATE_PATHS.iter().chain(file_paths.iter()).any(|entry| {
                if entry.ends_with("::") {
                    named.starts_with(entry)
                } else {
                    named == *entry || named.starts_with(&format!("{entry}::"))
                }
            });
            if !allowed {
                violations.push(format!("{relative}: crate::{named}"));
            }
        }
        for name in CAUSAL_DOMAIN_FORBIDDEN_NAMES {
            if contains_identifier(&production, name) {
                violations.push(format!("{relative}: {name}"));
            }
        }
        let compact: String = production.split_whitespace().collect();
        for inclusion in ["#[path", "include!("] {
            if compact.contains(inclusion) {
                violations.push(format!("{relative}: {inclusion}"));
            }
        }
    }
    assert!(
        scanned >= 4,
        "the causal domain has at least mod, branch, state and projection"
    );
    assert!(
        violations.is_empty(),
        "the causal domain imports no backend and no network client (AG-01): {violations:?}"
    );
}

/// The only production files that may build trust-bearing turn state
/// (D-066 N-3). CDP-21 creates the turn decision service; until then no
/// production file builds any.
// Service-owned child modules consume only privately issued source/terminal
// evidence. Keep exact files; no other runtime module may construct state.
const TRUST_BEARING_CALLERS: &[&str] = &[
    "src/runtime/turn_decision.rs",
    "src/runtime/turn_decision/routing.rs",
    "src/runtime/turn_decision/segment_observation.rs",
];

/// The trust-bearing constructors, named by identifiers no other code uses,
/// with the file that defines each once.
const TRUST_BEARING_CONSTRUCTORS: [&str; 3] = ["from_runtime", "from_runtime_records", "recorded_by_runtime"];
const TRUST_BEARING_DEFINITIONS: &str = "src/causal_tree/state.rs";

/// How often `name` occurs in `text` as a whole identifier.
fn identifier_count(text: &str, name: &str) -> usize {
    let mut count = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find(name) {
        let start = search + found;
        search = start + name.len();
        let before = text[..start].chars().next_back();
        let after = text[search..].chars().next();
        if !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char) {
            count += 1;
        }
    }
    count
}

/// AG-28 (CDP-PLAN 1.9, scope amended by D-066 N-3 and its amendment).
/// Trusted and observed state carry authority: a trusted part grants or
/// excludes capabilities, and a failed step marks a candidate
/// `failed_before`. Only the turn decision service may build them. Outside
/// [`TRUST_BEARING_CALLERS`], no production code
///
/// - names `TrustedState::from_runtime`, `ObservedState::from_runtime_records`
///   or `ObservedStep::recorded_by_runtime` at all: called, passed as a
///   function value (`.map(TrustedState::from_runtime)`) or through an alias
///   (the identifiers are unique to these constructors, so the type path is
///   not needed); the defining file names each exactly once, in its `fn`;
/// - writes a `TrustedParts { .. }` literal, borrowed or not;
/// - assigns a turn state's `.trusted` or `.observed`, or borrows either
///   mutably (`std::mem::replace`, `swap` and `take` need `&mut`).
///
/// Comments and literals are blanked first, and the scan runs over the
/// whitespace-normalized token text, so formatting cannot hide a use.
#[test]
fn trust_bearing_turn_state_is_built_only_by_the_turn_decision_service() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if TRUST_BEARING_CALLERS.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        for constructor in TRUST_BEARING_CONSTRUCTORS {
            let allowed = usize::from(relative == TRUST_BEARING_DEFINITIONS);
            let named = identifier_count(&compact, constructor);
            let defined = identifier_count(&compact, &format!("fn {constructor}"));
            if named > allowed || defined > allowed {
                violations.push(format!("{relative}: names {constructor}"));
            }
        }
        let mut search = 0;
        while let Some(found) = compact[search..].find("TrustedParts {") {
            let start = search + found;
            search = start + "TrustedParts {".len();
            // Not a literal: the definition, an impl header, or a return type
            // (owned or borrowed) right before a function body. A borrowed
            // literal (`&TrustedParts { .. }` in an expression) is one.
            let before = compact[..start].trim_end();
            let before = before.strip_suffix("mut").map_or(before, str::trim_end);
            let before = before.strip_suffix('&').map_or(before, str::trim_end);
            let is_type_position = ["struct", "for", "impl", "->"]
                .iter()
                .any(|token| before.ends_with(token));
            if !is_type_position {
                violations.push(format!("{relative}: TrustedParts literal"));
            }
        }
        for field in [".trusted", ".observed"] {
            let mut search = 0;
            while let Some(found) = compact[search..].find(field) {
                let start = search + found;
                search = start + field.len();
                if compact[search..].chars().next().is_some_and(is_ident_char) {
                    continue;
                }
                let assigned = compact[search..].starts_with(" =") && !compact[search..].starts_with(" ==");
                // A mutable borrow of the field: `&mut <path>.field` with a
                // path of identifiers, dots and calls before it.
                let path_start = compact[..start]
                    .rfind(|character: char| !(is_ident_char(character) || matches!(character, '.' | '(' | ')')))
                    .map_or(0, |index| index + 1);
                let borrowed =
                    compact[..path_start].trim_end().ends_with("&mut") || compact[..path_start].ends_with("&mut ");
                if assigned || borrowed {
                    violations.push(format!("{relative}: writes {field}"));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "trusted and observed turn state is built only by the turn decision service (AG-28): {violations:?}"
    );
}

/// The text of the brace-delimited block that opens at the first `{` at or
/// after `from`, braces included.
fn braced_block(text: &str, from: usize) -> Option<&str> {
    let open = from + text[from..].find('{')?;
    let mut depth = 0usize;
    for (offset, character) in text[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open..=open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `impl` header in `production` (whitespace-normalized, up to its
/// `{`) that names `name`, with the block it opens.
fn impl_blocks_naming<'a>(production: &'a str, name: &str) -> Vec<(String, &'a str)> {
    let mut blocks = Vec::new();
    for (start, _) in production.match_indices("impl") {
        let before_ok = start == 0 || !is_ident_char(production[..start].chars().next_back().unwrap_or(' '));
        let after_ok = production[start + 4..]
            .chars()
            .next()
            .is_some_and(|character| character == '<' || character.is_whitespace());
        if !(before_ok && after_ok) {
            continue;
        }
        let Some(open) = production[start..].find('{').map(|offset| start + offset) else {
            continue;
        };
        let header = normalize_line(&production[start..open]);
        if header.contains(';') || !contains_identifier(&header, name) {
            continue;
        }
        if let Some(block) = braced_block(production, open) {
            blocks.push((header, block));
        }
    }
    blocks
}

/// AG-29. Every production request constructor is a reviewed redacted
/// projection or a neutral diagnostic request. A new call site must receive
/// an explicit privacy review before it can reach a decision backend.
fn decision_seal_sites(source: &str) -> (usize, usize, usize) {
    let production = strip_cfg_test_items(&blank_comments_and_literals(source));
    (
        production.matches("DecisionRequest::seal(").count(),
        production
            .matches("DecisionRequest::seal_with_projection_audit(")
            .count(),
        production.matches("DecisionRequest::seal_recovering_parts(").count(),
    )
}

#[test]
fn only_reviewed_production_sites_seal_decision_requests() {
    let mut found = BTreeMap::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let counts = decision_seal_sites(&read_source(&path));
        if counts != (0, 0, 0) {
            found.insert(relative, counts);
        }
    }
    assert_eq!(
        found,
        BTreeMap::from([
            ("src/causal_tree/admin.rs".to_string(), (1, 0, 0)),
            ("src/causal_tree/questions.rs".to_string(), (0, 0, 1)),
            ("src/decision/factory.rs".to_string(), (1, 0, 0)),
        ]),
        "AG-29 decision request constructors changed; review redacted or neutral state provenance"
    );
}

#[test]
fn probe_decision_seal_inventory_rejects_an_extra_caller() {
    let planted = "fn unreviewed() { let _ = DecisionRequest::seal(parts, key, expiry, limits); }";
    assert_eq!(decision_seal_sites(planted), (1, 0, 0));
    assert_ne!(decision_seal_sites(planted), (0, 0, 0));
    let recovered = "fn unreviewed() { DecisionRequest::seal_recovering_parts(parts, audit, key, expiry, limits); }";
    assert_eq!(decision_seal_sites(recovered), (0, 0, 1));
    let hidden = format!("#[cfg(test)] mod tests {{ {recovered} }}");
    assert_eq!(decision_seal_sites(&hidden), (0, 0, 0));
}

/// AG-19 and AG-20 apply to the compiler's production body. Runtime
/// injection tests cover values; this source guard catches a new construction
/// path that starts reading untrusted names or schema metadata directly.
fn decision_compiler_source_errors(source: &str) -> Vec<&'static str> {
    let production = strip_cfg_test_items(source);
    let mut errors = Vec::new();
    if production.matches("redacted_projection(state)?").count() != 1
        || production.matches("state: projection,").count() != 1
        || production.matches("parts.state =").count() != 1
        || !production.contains("parts.state = shrink_oldest_artifact_summary(&parts.state)?.ok_or(CompileError::Budget)?;")
        || production.matches("parts = *recovered;").count() != 1
        || production.matches("seal_recovering_parts(").count() != 1
        || !production.contains("seal_recovering_parts(\n                parts,")
        || !production.contains("Err((DecisionError::InvalidRequest(RequestRule::RequestTooLarge), recovered)) => {\n                    parts = *recovered;")
        || production.contains("projection.clone()")
        || production.contains("parts.clone()")
    {
        errors.push("request state must move from redacted projection and recover only for checked size shrinking");
    }
    let allowed_option_calls = [
        "OptionId::code(candidate.action().as_str())",
        "OptionId::code(action)",
        "OptionId::code(\"other\")",
        "OptionId::code(candidate.target().as_str())",
        "OptionId::code(target.as_str())",
        "OptionId::code(target)",
        "OptionId::code(\"none\")",
    ];
    if production.matches("OptionId::code(").count() != 10
        || allowed_option_calls.iter().any(|call| {
            let expected = match *call {
                "OptionId::code(target)"
                | "OptionId::code(candidate.action().as_str())"
                | "OptionId::code(candidate.target().as_str())" => 2,
                _ => 1,
            };
            production.matches(call).count() != expected
        })
    {
        errors.push("option ids must be derived from typed action or target values");
    }
    let allowed_formats = [
        "format!(\"$.candidates[{index}]\")",
        "format!(\"Does {path} apply to this state?\")",
        "format!(\"Rate the risk of {path}.\")",
        "format!(\"Would {path} succeed?\")",
        "format!(\"Rate the task gain from {path}.\")",
    ];
    if production.matches("format!(").count() != allowed_formats.len()
        || allowed_formats.iter().any(|call| production.matches(call).count() != 1)
    {
        errors.push("question paths and text must use only the typed projected candidate index");
    }
    for forbidden in [
        "ToolSpec",
        "CapabilityCatalog",
        "inputSchema",
        ".schema",
        "QuestionId::from",
        "QuestionId::parse",
        "CandidateId::from",
        "CandidateId::parse",
        "OptionId::code(state",
        "OptionId::code(user",
    ] {
        if production.contains(forbidden) {
            errors.push("compiler reads schema/catalog or constructs an id from untyped input");
        }
    }
    errors
}

#[test]
fn decision_compiler_uses_typed_ids_and_only_redacted_state() {
    let source = read_source(&repository_root().join("src/causal_tree/questions.rs"));
    assert_eq!(decision_compiler_source_errors(&source), Vec::<&str>::new());
    let planted_id = format!("{source}\nfn planted() {{ let _ = QuestionId::parse(user_text); }}");
    assert!(
        !decision_compiler_source_errors(&planted_id).is_empty(),
        "expected a non-empty value"
    );
    let planted_schema = format!("{source}\nfn planted() {{ let _ = entry.schema; }}");
    assert!(
        !decision_compiler_source_errors(&planted_schema).is_empty(),
        "expected a non-empty value"
    );
    let planted_option = format!("{source}\nfn planted() {{ let _ = OptionId::code(state.routing_input.as_str()); }}");
    assert!(
        !decision_compiler_source_errors(&planted_option).is_empty(),
        "expected a non-empty value"
    );
    let planted_path =
        format!("{source}\nfn planted() {{ let _ = format!(\"$.candidates[{{}}]\", state.observed.retry_count()); }}");
    assert!(
        !decision_compiler_source_errors(&planted_path).is_empty(),
        "expected a non-empty value"
    );
    let planted_state = format!("{source}\nfn planted() {{ parts.state = unredacted_state; }}");
    assert!(
        !decision_compiler_source_errors(&planted_state).is_empty(),
        "expected a non-empty value"
    );
    for (old, new) in [
        ("state: projection,", "state: unredacted_state,"),
        (
            "shrink_oldest_artifact_summary(&parts.state)?",
            "shrink_oldest_artifact_summary(&unrelated_state)?",
        ),
        (
            "RequestRule::RequestTooLarge), recovered",
            "RequestRule::ModelInvalid), recovered",
        ),
        (
            "DecisionRequest::seal_recovering_parts(",
            "DecisionRequest::unchecked_seal(",
        ),
    ] {
        assert!(source.contains(old));
        assert!(
            !decision_compiler_source_errors(&source.replacen(old, new, 1)).is_empty(),
            "expected a non-empty value"
        );
    }
}

/// AG-23. Validation always runs against the sealed request: the validator
/// takes the `DecisionRequest` and reads its manifest; a `ValidatedDecision`
/// is built only in the validator; the manifest is built only in
/// `protocol.rs` (by `DecisionRequest::seal`); and no code builds a
/// `DecisionRequest` literal by name (`seal` builds it as `Self`), so every
/// manifest the validator reads matches the request content.
#[test]
fn validation_requires_the_sealed_request_manifest() {
    let owners: [(&str, &[&str]); 3] = [
        ("ValidatedDecision", &["src/decision/validate.rs"]),
        ("DecisionRequest", &[]),
        ("DecisionRequestManifest", &["src/decision/protocol.rs"]),
    ];
    let mut sites = BTreeMap::<&str, Vec<String>>::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (name, _) in owners {
            for _ in 0..struct_literal_count(&production, name) {
                sites.entry(name).or_default().push(relative.clone());
            }
        }
    }
    for (name, owner) in owners {
        assert_eq!(
            sites.get(name).cloned().unwrap_or_default(),
            owner.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "`{name}` struct literals (AG-23)"
        );
    }

    let validator = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/validate.rs"),
    )));
    let body = function_body(&validator, "validate");
    let header: String = body[..body.find('{').expect("validate has a body")]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        header.contains("request: &DecisionRequest,") && header.contains("response: DecisionResponse,"),
        "`validate` takes the sealed request and the response: {header}"
    );
    assert!(
        body.contains("request.manifest"),
        "`validate` reads the manifest of the request it validates against"
    );

    // D-057 M-1: a sealed request cannot be changed. Its fields and the
    // manifest's are private, every `impl` of either type lives in
    // `protocol.rs` and takes no `&mut self`, and no production code holds a
    // `&mut` to one.
    let protocol = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/protocol.rs"),
    )));
    for name in ["validate_parts", "from_validated_parts"] {
        let declaration = format!("fn {name}(");
        let declarations: Vec<_> = protocol
            .lines()
            .filter(|line| line.contains(&declaration))
            .map(str::trim)
            .collect();
        assert_eq!(
            declarations,
            vec![declaration.as_str()],
            "checked validation/construction helpers stay protocol-private"
        );
    }
    assert_eq!(protocol.matches("Self::validate_parts(").count(), 2);
    assert_eq!(protocol.matches("Self::from_validated_parts(").count(), 2);
    let validation = function_body(&protocol, "validate_parts");
    let construction = function_body(&protocol, "from_validated_parts");
    assert!(!validation.contains("Uuid::now_v7"));
    assert!(validation.contains("super::wire::salted_question_ids("));
    assert_eq!(protocol.matches("Uuid::now_v7()").count(), 1);
    assert_eq!(construction.matches("Uuid::now_v7()").count(), 1);
    for name in ["DecisionRequest", "DecisionRequestManifest"] {
        let declaration = format!("pub struct {name} ");
        let start = protocol
            .find(&declaration)
            .unwrap_or_else(|| panic!("protocol.rs declares {name}"));
        let body = braced_block(&protocol, start).expect("struct body");
        let public_fields: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("pub "))
            .collect();
        assert!(
            public_fields.is_empty(),
            "`{name}` must keep its fields private (AG-23): {public_fields:?}"
        );
    }
    let mut impls = BTreeSet::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for name in ["DecisionRequest", "DecisionRequestManifest"] {
            for (header, block) in impl_blocks_naming(&production, name) {
                assert!(
                    !block.contains("&mut self"),
                    "{relative}: `{header}` takes `&mut self` on a sealed request (AG-23)"
                );
                impls.insert(format!("{relative}: {header}"));
            }
            let compact: String = production.split_whitespace().collect();
            assert!(
                !compact.contains(&format!("&mut{name}")),
                "{relative}: holds `&mut {name}`; a sealed request is immutable (AG-23)"
            );
        }
    }
    let expected: BTreeSet<String> = [
        "src/decision/protocol.rs: impl DecisionRequest",
        "src/decision/protocol.rs: impl DecisionRequestManifest",
        "src/decision/protocol.rs: impl Eq for DecisionRequest",
        "src/decision/protocol.rs: impl PartialEq for DecisionRequest",
        "src/decision/protocol.rs: impl fmt::Debug for DecisionRequest",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(impls, expected, "the `impl` blocks of the sealed request types (AG-23)");
}

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
fn causal_policy_is_written_only_by_the_mutation_service() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if CAUSAL_POLICY_WRITERS.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
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
fn decision_code_never_uses_a_chat_provider() {
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

/// How many times `name` is named inside `use ..;` declarations of
/// `compact` (whitespace-normalized production text).
fn identifier_count_in_use_declarations(compact: &str, name: &str) -> usize {
    let mut count = 0;
    let mut search = 0;
    while let Some(found) = compact[search..].find("use ") {
        let start = search + found;
        search = start + "use ".len();
        if compact[..start].chars().next_back().is_some_and(is_ident_char) {
            continue;
        }
        let end = compact[start..]
            .find(';')
            .map_or(compact.len(), |offset| start + offset);
        count += identifier_count(&compact[start..end], name);
        search = end;
    }
    count
}

/// AG-04 (CDP-PLAN 1.4 and 1.5; D-079 L-11): the decision backend factory
/// is the only production code that validates a response, answers with the
/// builtin, builds a backend or asks one to evaluate, so no path around the
/// outbound guard, the breaker and validation exists.
///
/// - `DecisionResponseValidator::validate` and `BuiltinDecisionBackend::answer`
///   are named only in `src/decision/factory.rs`.
/// - `DecisionTransports` is named elsewhere only to import it, implement it
///   (`.. DecisionTransports for ..`), or as the turn service's single borrowed
///   constructor input. Only the factory invokes `build` on it.
/// - A file that names `DecisionBackend` outside the factory calls
///   `.evaluate(` only on `self` (a backend's own health probe).
#[test]
fn only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend() {
    const FACTORY: &str = "src/decision/factory.rs";
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative == FACTORY {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        for name in ["DecisionResponseValidator::validate", "BuiltinDecisionBackend::answer"] {
            if identifier_count(&compact, name) > 0 {
                violations.push(format!("{relative}: {name}"));
            }
        }
        let named = identifier_count(&compact, "DecisionTransports");
        let service_field = if relative == "src/runtime/turn_decision.rs" {
            let count = compact.matches("pub transports: &'a dyn DecisionTransports,").count();
            if count != 1 {
                violations.push(format!("{relative}: expected one borrowed constructor transport field"));
            }
            count
        } else {
            0
        };
        let allowed = identifier_count(&compact, "DecisionTransports for")
            + identifier_count_in_use_declarations(&compact, "DecisionTransports")
            + service_field;
        if named > allowed {
            violations.push(format!("{relative}: DecisionTransports"));
        }
        // Only the factory encodes a request and guards its body (D-082
        // N-2): a backend sends the body it is handed.
        if !matches!(relative.as_str(), "src/decision/outbound.rs" | "src/decision/wire.rs") {
            for name in ["check_outbound", "encode_request"] {
                if identifier_count(&compact, name) > 0 {
                    violations.push(format!("{relative}: {name}"));
                }
            }
        }
        // A concrete backend is built only by its transport.
        if relative != "src/decision/compatible_http.rs" && identifier_count(&compact, "CompatibleHttpBackend") > 0 {
            violations.push(format!("{relative}: CompatibleHttpBackend"));
        }
        // A transport is asked to build only by the factory, however it is
        // reached (`deps.transports.build(..)`), and only the factory fills
        // a backend spec (D-082 N-3).
        if compact.contains("transports.build(") {
            violations.push(format!("{relative}: builds through transports"));
        }
        let mut search = 0;
        while let Some(found) = compact[search..].find("BackendSpec {") {
            let start = search + found;
            search = start + "BackendSpec {".len();
            let before = compact[..start].trim_end();
            if !["struct", "impl", "for", "->"]
                .iter()
                .any(|token| before.ends_with(token))
            {
                violations.push(format!("{relative}: BackendSpec literal"));
            }
        }
        if identifier_count(&compact, "DecisionBackend") > 0 || identifier_count(&compact, "GuardedBody") > 0 {
            let calls = compact.matches(".evaluate(").count();
            let own = compact.matches("self.evaluate(").count();
            if calls > own {
                violations.push(format!("{relative}: evaluates a decision backend"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "only the decision backend factory validates, answers with the builtin, builds or evaluates a backend \
         (AG-04): {violations:?}"
    );
}

/// AG-17 (CDP-PLAN 1.5 and 1.7; SP-CDP-A 5.3 and 5.8): whether a decision
/// endpoint is loopback is decided in one place, `decision::loopback`, from
/// the host as written. Configuration validation, the factory's locality and
/// the HTTP client share it. Elsewhere in `src/decision/**` and in
/// `src/causal_tree/config.rs`:
///
/// - no parsed host is consulted (`host_str`, which normalizes `127.1` and
///   `0x7f000001` to `127.0.0.1`);
/// - no address type is built or matched (`Ipv4Addr`, `Ipv6Addr`), except by
///   the HTTP client, which maps `localhost` to `127.0.0.1` itself;
/// - `is_loopback` is used only by the compatible backend's check of the
///   peer it actually reached.
#[test]
fn loopback_is_decided_only_by_the_shared_predicate() {
    const RULES: [(&str, &[&str]); 4] = [
        ("host_str", &["src/decision/loopback.rs"]),
        ("Ipv4Addr", &["src/decision/loopback.rs", "src/decision/http.rs"]),
        ("Ipv6Addr", &["src/decision/loopback.rs", "src/decision/http.rs"]),
        (
            "is_loopback",
            &["src/decision/loopback.rs", "src/decision/compatible_http.rs"],
        ),
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") && relative != "src/causal_tree/config.rs" {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (name, allowed) in RULES {
            if !allowed.contains(&relative.as_str()) && contains_identifier(&production, name) {
                violations.push(format!("{relative}: {name}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "loopback is decided only by decision::loopback (AG-17): {violations:?}"
    );
}

/// AG-21 (SP-CDP-A 5.8): every decision HTTP client comes from
/// `src/decision/http.rs`, which never follows a redirect, ignores every
/// proxy and resolves `localhost` itself for loopback, and is HTTPS only
/// for remote. No decision file builds another client or turns a
/// certificate check off.
#[test]
fn decision_http_clients_come_only_from_the_pinned_builder() {
    const HTTP: &str = "src/decision/http.rs";
    const PINNED: [&str; 5] = [
        "redirect::Policy::none()",
        ".no_proxy()",
        ".https_only(true)",
        ".https_only(false)",
        ".resolve(",
    ];
    const OTHER_CLIENTS: [&str; 7] = [
        "ClientBuilder",
        "Client::builder",
        "Client::new",
        "Client::default",
        "reqwest::get(",
        "reqwest::blocking",
        "build_runtime_proxy_client",
    ];
    let mut violations = Vec::new();
    let http = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join(HTTP),
    )));
    let compact: String = http.split_whitespace().collect();
    for call in PINNED {
        if !compact.contains(call) {
            violations.push(format!("{HTTP}: no {call}"));
        }
    }
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact: String = production.split_whitespace().collect();
        if compact.contains("danger_") || compact.contains("Policy::limited") || compact.contains("Policy::custom") {
            violations.push(format!("{relative}: disables a check or follows redirects"));
        }
        if relative != HTTP && OTHER_CLIENTS.iter().any(|name| compact.contains(name)) {
            violations.push(format!("{relative}: builds an HTTP client"));
        }
    }
    assert!(
        violations.is_empty(),
        "decision HTTP clients come only from the pinned builder (AG-21): {violations:?}"
    );
}

/// AG-10 (SP-CDP-A 5.10): the decision store's database is named, and so
/// opened, only by `causal_tree/store.rs`. Its `store/` child modules are
/// scanned like any other file; test-only files are empty production text.
/// Comments may mention it.
#[test]
fn the_decision_store_database_is_opened_only_by_the_store() {
    const STORE: &str = "src/causal_tree/store.rs";
    const SCANNED_CHILDREN: [&str; 2] = [
        "src/causal_tree/store/child_outcome.rs",
        "src/causal_tree/store/child_recovery.rs",
    ];
    let mut violations = Vec::new();
    let mut scanned_children = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/") || relative == STORE {
            continue;
        }
        if SCANNED_CHILDREN.contains(&relative.as_str()) {
            scanned_children += 1;
        }
        let production = strip_cfg_test_items(&read_source(&path));
        let names_the_database = production.lines().any(|line| {
            let code = line.split_once("//").map_or(line, |(code, _)| code);
            code.contains("decisions.db")
        });
        if names_the_database {
            violations.push(relative);
        }
    }
    assert_eq!(
        scanned_children,
        SCANNED_CHILDREN.len(),
        "the store child modules must be scanned, not exempted"
    );
    assert!(
        violations.is_empty(),
        "only the decision store opens decisions.db (AG-10): {violations:?}"
    );
}

/// AG-11 (SP-CDP-A 5.11): the decision-plane metrics take their labels from
/// bounded allowlists only, on every metrics backend.
///
/// - Prometheus: every production `prx_causal_*` metric declares labels from
///   `CAUSAL_METRIC_LABELS`, and no production code of the file names the
///   lineage (session, owner, run and decision ids go to logs and trace
///   attributes only) or the configured model, so no helper can carry them
///   into a label.
/// - OpenTelemetry: the causal counter is recorded only with
///   `causal_metric_attributes`, which names neither.
#[test]
fn decision_metric_labels_come_from_bounded_allowlists() {
    const CAUSAL_METRIC_LABELS: [&str; 7] = ["backend", "locality", "served_by", "error_class", "from", "to", "state"];
    const UNBOUNDED: [&str; 7] = [
        "lineage",
        "session_key",
        "owner_id",
        "run_id",
        "task_id",
        "decision_id",
        "configured_model",
    ];
    const PROMETHEUS: &str = "src/observability/prometheus.rs";
    const OTEL: &str = "src/observability/otel.rs";
    let raw = read_source(&repository_root().join(PROMETHEUS));
    // Production text with its literals kept (the names and labels are
    // literals), and the same text blanked for identifier checks.
    let production = strip_cfg_test_items(&raw);
    let mut violations = Vec::new();
    let mut declared = 0_usize;
    let mut search = 0;
    while let Some(found) = production[search..].find("\"prx_causal_") {
        let start = search + found;
        search = start + 1;
        declared += 1;
        let window = &production[start..production.len().min(start + 400)];
        let labels = window
            .find("&[")
            .filter(|open| !window[..*open].contains(".map_err"))
            .and_then(|open| {
                window[open + 2..]
                    .find(']')
                    .map(|close| &window[open + 2..open + 2 + close])
            });
        for label in labels.into_iter().flat_map(|list| list.split(',')) {
            let label = label.trim().trim_matches('"');
            if !label.is_empty() && !CAUSAL_METRIC_LABELS.contains(&label) {
                violations.push(format!("{PROMETHEUS}: label `{label}`"));
            }
        }
    }
    assert!(declared >= 6, "the decision metrics are declared in {PROMETHEUS}");
    let blanked = strip_cfg_test_items(&blank_comments_and_literals(&raw));
    assert!(blanked.contains("fn record_causal("), "record_causal exists");
    for field in UNBOUNDED {
        if contains_identifier(&blanked, field) {
            violations.push(format!("{PROMETHEUS}: names `{field}`"));
        }
    }
    let otel = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join(OTEL),
    )));
    let compact: String = otel.split_whitespace().collect();
    let mut adds = 0_usize;
    let mut search = 0;
    while let Some(found) = compact[search..].find("causal_events.add(") {
        let start = search + found;
        search = start + 1;
        adds += 1;
        if !compact[start..].starts_with("causal_events.add(1,&causal_metric_attributes(") {
            violations.push(format!("{OTEL}: the causal counter is recorded with other attributes"));
        }
    }
    assert!(adds >= 1, "the causal counter is recorded in {OTEL}");
    match otel.find("fn causal_metric_attributes(") {
        Some(start) => {
            let rest = &otel[start..];
            let body = rest.find("\n}\n").map_or(rest, |end| &rest[..end]);
            for field in UNBOUNDED {
                if contains_identifier(body, field) {
                    violations.push(format!("{OTEL}: causal_metric_attributes reads `{field}`"));
                }
            }
        }
        None => violations.push(format!("{OTEL}: no causal_metric_attributes")),
    }
    assert!(
        violations.is_empty(),
        "decision metric labels come from bounded allowlists (AG-11): {violations:?}"
    );
}

#[test]
fn embedded_user_input_cannot_bypass_reserved_marker_neutralization() {
    assert!(!scanner_fails_with(
        user_text_enters_histories_only_neutralized,
        Vec::new()
    ));
    let path = "src/agent/embedded_turn.rs";
    let source = read_source(&repository_root().join(path));
    let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
    let body = compact_rust_tokens(&function_body(&production, "run_work"));
    assert_eq!(body.matches("inbound_user_message(input)").count(), 2);
    assert_eq!(body.matches("inbound_user_message_owned(enriched)").count(), 1);
    assert!(
        body.contains(".record_guarded(input,&owner,cancellation,defer_model_selection)"),
        "source identity must continue to record the original input"
    );
    for (needle, replacement, expected) in [
        (
            "crate::agent::turn_contracts::inbound_user_message(input)",
            "ChatMessage::user(input)",
            2,
        ),
        (
            "crate::agent::turn_contracts::inbound_user_message_owned(enriched)",
            "ChatMessage::user(enriched)",
            1,
        ),
    ] {
        assert_eq!(source.matches(needle).count(), expected);
        for (at, _) in source.match_indices(needle) {
            let mut changed = source.clone();
            changed.replace_range(at..at + needle.len(), replacement);
            assert!(scanner_fails_with(
                user_text_enters_histories_only_neutralized,
                vec![(path.into(), changed)]
            ));
        }
    }
}

#[test]
fn ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates() {
    assert!(!scanner_fails_with(
        session_worker_credentials_are_decided_in_one_scoped_place,
        Vec::new()
    ));
    let path = "src/session_worker/runner/ordinary_source.rs";
    let source = read_source(&repository_root().join(path));
    let comparison = "resolved.credential().api_key() == credentials.sealed_key";
    assert_eq!(source.matches(comparison).count(), 1);
    for replacement in [
        "resolved.credential().api_key() == manifest.api_key.as_deref()",
        "resolved.credential().api_key() == config.api_key.as_deref()",
        "resolved.credential().api_key() == credentials.provider.api_key",
        "resolved.credential().api_key() == credentials.sealed_key && resolved.credential().api_key() == credentials.sealed_key",
    ] {
        assert!(scanner_fails_with(
            session_worker_credentials_are_decided_in_one_scoped_place,
            vec![(path.into(), source.replace(comparison, replacement))]
        ));
    }
    let other = "fn restore() { let value = resolved.credential().api_key(); }";
    assert!(scanner_fails_with(
        session_worker_credentials_are_decided_in_one_scoped_place,
        vec![("src/session_worker/d0_ordinary_neighbor.rs".into(), other.into())]
    ));
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
    assert_reviewed_source_mutation(
        forwarded_compaction_values_are_produced_from_the_effective_window,
        "src/runtime/turn_decision.rs",
        "let main_compaction = reservation.reserved_compaction(&effective.config, extra_token_ratio_limit)?;",
        "let main_compaction = config.agent.compaction.clone();",
    );
}
fn mutation_proof_credential_comparison_guard() {
    assert_reviewed_source_mutation(
        ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates,
        "src/session_worker/runner/ordinary_source.rs",
        "resolved.credential().api_key() == credentials.sealed_key",
        "resolved.credential().api_key() == manifest.api_key.as_deref()",
    );
}

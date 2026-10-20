use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, function_body, is_ident_char, normalize_line,
    read_source, relative_path, repository_root, rust_source_files, strip_cfg_test_items,
};
use std::fs;
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
    "src/chat/tests/legacy_chat_compaction_audit_tests.rs::legacy_chat_compaction_downgrades_fidelity_when_event_provenance_is_unavailable::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
    "src/chat/tests/legacy_chat_compaction_audit_tests.rs::legacy_chat_compaction_persists_run_and_summary_memory::let conn = rusqlite::Connection::open(tmp.path().join(\"memory\").join(\"brain.db\")).unwrap();",
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
const SQLITE_SEPARATE_MODULES: &[&str] = &[
    "event_batch.rs",
    "ingress_recovery.rs",
    "router_history.rs",
    "router_learning.rs",
];

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
        ("mod.rs", include_str!("../../src/memory/sqlite/mod.rs")),
        (
            "approval_grants.rs",
            include_str!("../../src/memory/sqlite/approval_grants.rs"),
        ),
        ("documents.rs", include_str!("../../src/memory/sqlite/documents.rs")),
        ("drafts.rs", include_str!("../../src/memory/sqlite/drafts.rs")),
        ("events.rs", include_str!("../../src/memory/sqlite/events.rs")),
        ("evolution.rs", include_str!("../../src/memory/sqlite/evolution.rs")),
        ("memory_impl.rs", include_str!("../../src/memory/sqlite/memory_impl.rs")),
        ("mirror.rs", include_str!("../../src/memory/sqlite/mirror.rs")),
        ("recall.rs", include_str!("../../src/memory/sqlite/recall.rs")),
        ("rows.rs", include_str!("../../src/memory/sqlite/rows.rs")),
        ("schema.rs", include_str!("../../src/memory/sqlite/schema.rs")),
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
pub(super) fn every_memory_read_principal_built_from_a_context_carries_the_owner_scope() {
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
pub(super) fn the_owner_isolation_bit_is_never_asserted_by_memory_production_code() {
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

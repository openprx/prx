use super::*;

/// The SQLite backend's production source: the root `sqlite/mod.rs` followed
/// by every other source file directly under `src/memory/sqlite/`. Its
/// `tests/` directory is excluded so a test fixture cannot satisfy a
/// production guard.
fn sqlite_backend_source() -> String {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/sqlite");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
        .expect("test: read src/memory/sqlite")
        .map(|entry| entry.expect("test: read directory entry").path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "rs")
                && path
                    .file_name()
                    .is_some_and(|name| name != "mod.rs" && name != "fixture_readback.rs")
        })
        .collect();
    files.sort();
    files.insert(0, directory.join("mod.rs"));
    assert!(files.len() > 1, "test: the sqlite backend source files were not found");
    let source = files
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("test: read sqlite backend source"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!source.is_empty(), "test: the sqlite backend source is empty");
    source
}

#[test]
fn typed_source_and_subject_keep_stable_json_contracts() {
    let known = MessageEventSource::Chat;
    assert_eq!(serde_json::to_string(&known).unwrap(), "\"chat\"");
    assert_eq!(serde_json::from_str::<MessageEventSource>("\"chat\"").unwrap(), known);

    let custom = MessageEventSource::Other("custom-adapter".to_string());
    assert_eq!(serde_json::to_string(&custom).unwrap(), "\"custom-adapter\"");
    assert_eq!(
        serde_json::from_str::<MessageEventSource>("\"custom-adapter\"").unwrap(),
        custom
    );

    let subject = MessageEventSubject::Task("task-1".to_string());
    assert_eq!(
        serde_json::from_str::<MessageEventSubject>(&serde_json::to_string(&subject).unwrap()).unwrap(),
        subject
    );
}

/// The PostgreSQL backend's production text: the root file plus the sibling
/// files that were split out of it. Listed with `include_str!`, so a file
/// that moves or disappears breaks the build instead of blinding the scan.
fn postgres_backend_source() -> String {
    [
        include_str!("../postgres.rs"),
        include_str!("../postgres/pool.rs"),
        include_str!("../postgres/schema.rs"),
        include_str!("../postgres/access.rs"),
        include_str!("../postgres/embedding.rs"),
        include_str!("../postgres/rows.rs"),
        include_str!("../postgres/memory_impl.rs"),
        include_str!("../postgres/events.rs"),
    ]
    .join("\n// ---- next source file ----\n")
}

/// A new file under `postgres/` must join the list above or be named here
/// as outside the old single-file text.
#[test]
fn postgres_backend_source_lists_every_backend_file() {
    let source = postgres_backend_source();
    let scanned: Vec<&str> = source.split("\n// ---- next source file ----\n").collect();
    assert_eq!(
        scanned.len(),
        8,
        "the scanned text must hold the root and seven sibling files"
    );
    let outside = [
        "decision_reads.rs",
        "ingress_recovery.rs",
        "router_history.rs",
        "router_learning.rs",
    ];
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/postgres");
    for entry in std::fs::read_dir(directory).expect("test: read the backend directory") {
        let path = entry.expect("test: read a directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test: utf-8 file name");
        if path.is_dir() {
            assert_eq!(
                name, "tests",
                "postgres/{name}/ is a subdirectory the scan does not read"
            );
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("test: read a backend file");
        assert!(
            scanned.iter().any(|part| *part == text) || outside.contains(&name),
            "postgres/{name} is neither scanned nor listed as outside the scan"
        );
    }
}

#[test]
fn sqlite_and_postgres_append_paths_share_lineage_and_transaction_boundaries() {
    let sqlite = sqlite_backend_source();
    let postgres = postgres_backend_source();
    // The SQLite append lives in `append_event_with_recovery`; the trait
    // method `append_message_event` only delegates to it.
    let sqlite_append = sqlite
        .split_once("async fn append_event_with_recovery")
        .map_or("", |(_, body)| body.split("\n    fn ").next().unwrap_or(body));
    let postgres_append = postgres
        .split_once("async fn append_message_event")
        .map_or("", |(_, body)| body);
    for field in [
        "event_type",
        "source_ref_json",
        "subject_ref_json",
        "goal_id",
        "causation_event_id",
        "correlation_id",
        "attempt_id",
        "lease_epoch",
    ] {
        assert!(sqlite_append.contains(field), "SQLite append must carry {field}");
        assert!(postgres_append.contains(field), "Postgres append must carry {field}");
    }
    assert!(
        sqlite_append.contains("Transaction::new_unchecked")
            && sqlite_append.contains("TransactionBehavior::Immediate"),
        "SQLite append must BEGIN IMMEDIATE so a concurrent opener cannot SQLITE_BUSY the child-start write"
    );
    assert!(
        !sqlite_append.contains("let tx = conn.unchecked_transaction()?"),
        "SQLite append must not BEGIN DEFERRED"
    );
    assert!(postgres_append.contains("let mut tx = client.transaction()?"));
    assert!(sqlite_append.contains("tx.commit()?"));
    assert!(postgres_append.contains("tx.commit()?"));
    assert!(sqlite_append.contains("input.event_type"));
    assert!(postgres_append.contains("input.event_type"));
}

fn compaction_input(
    source_event_ids_json: Option<String>,
    source_event_range_json: Option<String>,
) -> CompactionRunInput {
    CompactionRunInput {
        agent_partition: None,
        run_id: Some("compact-contract".to_string()),
        workspace_id: "workspace".to_string(),
        owner_id: None,
        session_key: Some("chat:session".to_string()),
        agent_id: None,
        persona_id: None,
        trigger: "test".to_string(),
        mode: "safeguard".to_string(),
        source_message_count: 1,
        source_token_estimate: 1,
        summary: "summary".to_string(),
        summary_memory_key: None,
        source_event_ids_json,
        source_event_range_json,
        source_document_refs_json: None,
        fidelity_status: "accepted".to_string(),
        payload_json: None,
    }
}

#[test]
fn compaction_provenance_rejects_hash_objects_masquerading_as_event_ids() {
    let input = compaction_input(
        Some(r#"[{"index":0,"role":"user","content_hash":"fake"}]"#.to_string()),
        Some(
            serde_json::to_string(&CompactionSourceEventRange {
                first_event_id: "fake".to_string(),
                last_event_id: "fake".to_string(),
                first_row_id: 1,
                last_row_id: 1,
                source_event_count: 1,
            })
            .unwrap(),
        ),
    );
    let error = input.validate_source_event_provenance().unwrap_err();
    assert!(error.to_string().contains("string array"));
}

#[test]
fn compaction_provenance_requires_matching_ids_and_covered_range() {
    let range = CompactionSourceEventRange {
        first_event_id: "event-1".to_string(),
        last_event_id: "event-2".to_string(),
        first_row_id: 11,
        last_row_id: 12,
        source_event_count: 2,
    };
    let valid = compaction_input(
        Some(r#"["event-1","event-2"]"#.to_string()),
        Some(serde_json::to_string(&range).unwrap()),
    );
    valid.validate_source_event_provenance().unwrap();

    let missing_range = compaction_input(Some(r#"["event-1"]"#.to_string()), None);
    assert!(missing_range.validate_source_event_provenance().is_err());
}

#[test]
fn sqlite_and_postgres_compaction_paths_persist_ids_and_covered_range() {
    let sqlite = sqlite_backend_source();
    let postgres = postgres_backend_source();
    let sqlite_append = sqlite
        .split_once("async fn append_compaction_run")
        .map_or("", |(_, body)| body);
    let postgres_append = postgres
        .split_once("async fn append_compaction_run")
        .map_or("", |(_, body)| body);
    for field in ["source_event_ids_json", "source_event_range_json"] {
        assert!(
            sqlite_append.contains(field),
            "SQLite compaction append must carry {field}"
        );
        assert!(
            postgres_append.contains(field),
            "Postgres compaction append must carry {field}"
        );
    }
    assert!(sqlite_append.contains("validate_source_event_provenance"));
    assert!(postgres_append.contains("validate_source_event_provenance"));
}

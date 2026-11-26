use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;

#[test]
fn deep_merge_recurses_and_replaces_arrays() {
    let mut base: Value = toml::from_str(
        r#"
[memory]
backend = "sqlite"
paths = ["a", "b"]

[memory.embeddings]
enabled = false
provider = "old"
"#,
    )
    .unwrap();
    let overlay: Value = toml::from_str(
        r#"
[memory]
paths = ["override"]

[memory.embeddings]
enabled = true
"#,
    )
    .unwrap();

    deep_merge_toml(&mut base, overlay);

    let memory = base.get("memory").and_then(Value::as_table).unwrap();
    assert_eq!(memory.get("paths").and_then(Value::as_array).unwrap().len(), 1);
    let embeddings = memory.get("embeddings").and_then(Value::as_table).unwrap();
    assert_eq!(embeddings.get("provider").and_then(Value::as_str), Some("old"));
    assert_eq!(embeddings.get("enabled").and_then(Value::as_bool), Some(true));
}

#[test]
fn build_split_tables_moves_mapped_sections() {
    let root: Value = toml::from_str(
        r#"
default_temperature = 0.7

[memory]
backend = "sqlite"

[storage]
[scheduler]
"#,
    )
    .unwrap();

    let (main, fragments) = build_split_tables(&root).unwrap();
    assert!(main.get("memory").is_none());
    assert!(main.get("storage").is_none());
    assert!(main.get("scheduler").is_none());
    assert!(main.get("default_temperature").is_some());
    assert_eq!(fragments.len(), 2);
    assert_eq!(fragments[0].0, "memory.toml");
    assert_eq!(fragments[1].0, "scheduler.toml");
}

#[test]
fn managed_fragment_names_match_layout() {
    assert_eq!(
        managed_fragment_names(),
        vec![
            "memory.toml",
            "channels.toml",
            "network.toml",
            "security.toml",
            "scheduler.toml",
            "agent.toml",
            "identity.toml",
            "routing.toml",
            "tools.toml",
            "integrations.toml",
            "nodes.toml",
            "cost.toml",
            "observability.toml",
        ]
    );
}

#[test]
fn committed_generation_is_a_hot_reload_event() {
    let config_path = PathBuf::from("/tmp/prx-config/config.toml");
    assert!(is_relevant_config_path(
        &config_path,
        Path::new("/tmp/prx-config/.config-generation")
    ));
}

#[test]
fn existing_config_event_matches_its_canonical_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "default_temperature = 0.7\n").unwrap();
    let canonical_event_path = config_path.canonicalize().unwrap();

    assert!(is_relevant_config_path(&config_path, &canonical_event_path));
}

#[cfg(unix)]
#[test]
fn list_config_fragment_paths_rejects_symlinked_config_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target_dir = tmp.path().join("outside");
    std::fs::create_dir_all(&target_dir).unwrap();
    symlink(&target_dir, tmp.path().join("config.d")).unwrap();

    let error = list_config_fragment_paths(&tmp.path().join("config.toml")).unwrap_err();
    assert!(
        error.to_string().contains("must not be a symlink"),
        "unexpected error: {error}"
    );
}

#[test]
fn consistent_snapshot_retries_when_generation_changes_during_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "default_temperature = 0.7\n").unwrap();
    std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "0\n").unwrap();
    let mut attempts = 0usize;

    let observed = with_consistent_config_snapshot(&config_path, || {
        attempts += 1;
        if attempts == 1 {
            std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "2\n").unwrap();
            bail!("simulated mixed-generation read");
        }
        Ok(attempts)
    })
    .unwrap();

    assert_eq!(observed, 2);
    assert_eq!(attempts, 2);
}

#[tokio::test]
async fn invalid_mutation_plan_leaves_existing_tree_unchanged() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();
    let before = std::fs::read(&config_path).unwrap();

    let error = plan_mutation(&config_path, &workspace, "not = [valid".to_string(), Vec::new()).unwrap_err();

    assert!(error.to_string().contains("Staged effective configuration is invalid"));
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    assert_eq!(read_config_generation(&config_path).unwrap() % 2, 0);
}

#[tokio::test]
async fn failed_multi_file_commit_rolls_back_complete_generation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();

    let before = capture_mutation_snapshot(
        &plan_mutation(
            &config_path,
            &workspace,
            config.to_split_toml_strings().unwrap().0,
            config.to_split_toml_strings().unwrap().1,
        )
        .unwrap(),
    )
    .unwrap();
    let unknown = tmp.path().join("config.d/operator-owned.toml");
    std::fs::write(&unknown, "[operator_owned]\nvalue = 9\n").unwrap();

    config.default_model = Some("transaction-candidate".to_string());
    let (main, fragments) = config.to_split_toml_strings().unwrap();
    let plan = plan_mutation(&config_path, &workspace, main, fragments).unwrap();
    *mutation_failure_hook()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(MutationFailureHook {
        config_path: config_path.clone(),
        writes_remaining: 0,
    });

    let error = commit_mutation_atomically(plan).await.unwrap_err();

    assert!(error.to_string().contains("rolled back"), "unexpected error: {error:#}");
    for (path, previous) in before {
        assert_eq!(
            std::fs::read(&path).ok(),
            previous.bytes,
            "rollback mismatch at {}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&unknown).unwrap(),
        "[operator_owned]\nvalue = 9\n"
    );
    assert_eq!(read_config_generation(&config_path).unwrap() % 2, 0);
    assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
    let loaded = crate::config::Config::load_from_path(&config_path, workspace).unwrap();
    assert_ne!(loaded.default_model.as_deref(), Some("transaction-candidate"));
}

#[tokio::test]
async fn unfinished_generation_rolls_back_from_journal_and_stale_lock_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();
    let stable_generation = read_config_generation(&config_path).unwrap();
    assert_eq!(stable_generation % 2, 0);
    let before = std::fs::read(&config_path).unwrap();

    config.default_model = Some("uncommitted-candidate".to_string());
    let (candidate_main, candidate_fragments) = config.to_split_toml_strings().unwrap();
    let plan = plan_mutation(&config_path, &workspace, candidate_main.clone(), candidate_fragments).unwrap();
    let snapshot = capture_mutation_snapshot(&plan).unwrap();
    write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
        .await
        .unwrap();

    std::fs::write(
        tmp.path().join(CONFIG_GENERATION_FILE),
        format!("{}\n", stable_generation + 1),
    )
    .unwrap();
    std::fs::write(&config_path, candidate_main).unwrap();
    std::fs::write(config_path.with_extension("toml.lock"), "stale lock inode").unwrap();

    assert!(
        recover_unfinished_config_generation(&config_path, &workspace)
            .await
            .unwrap()
    );
    assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation);
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
    let loaded = crate::config::Config::load_from_path(&config_path, workspace).unwrap();
    assert_ne!(loaded.default_model.as_deref(), Some("uncommitted-candidate"));
}

#[tokio::test]
async fn unfinished_generation_without_journal_remains_fail_closed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    std::fs::write(&config_path, "default_temperature = 0.7\n").unwrap();
    std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "1\n").unwrap();

    let error = recover_unfinished_config_generation(&config_path, &workspace)
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("cannot be recovered without its durable transaction journal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(read_config_generation(&config_path).unwrap(), 1);
}

#[tokio::test]
async fn invalid_journal_before_image_never_publishes_stable_generation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();
    let stable_generation = read_config_generation(&config_path).unwrap();
    let mut snapshot = BTreeMap::new();
    snapshot.insert(
        config_path.clone(),
        ConfigFileSnapshot {
            bytes: Some(b"not = [valid".to_vec()),
            permissions: std::fs::metadata(&config_path)
                .ok()
                .map(|metadata| metadata.permissions()),
        },
    );
    write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
        .await
        .unwrap();
    std::fs::write(
        tmp.path().join(CONFIG_GENERATION_FILE),
        format!("{}\n", stable_generation + 1),
    )
    .unwrap();

    let error = recover_unfinished_config_generation(&config_path, &workspace)
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("Recovered config transaction before-image is invalid"),
        "unexpected error: {error:#}"
    );
    assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation + 1);
    assert!(config_transaction_journal_path(&config_path).unwrap().exists());
}

#[tokio::test]
async fn stable_generation_discards_journal_left_before_pending_publish() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();
    let stable_generation = read_config_generation(&config_path).unwrap();
    let (main, fragments) = config.to_split_toml_strings().unwrap();
    let plan = plan_mutation(&config_path, &workspace, main, fragments).unwrap();
    let snapshot = capture_mutation_snapshot(&plan).unwrap();
    write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
        .await
        .unwrap();

    assert!(
        !recover_unfinished_config_generation(&config_path, &workspace)
            .await
            .unwrap()
    );
    assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation);
    assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
}

#[tokio::test]
async fn raw_managed_file_update_is_validated_and_preserves_peer_fragments() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();
    let peer_path = tmp.path().join("config.d/agent.toml");
    let peer_before = std::fs::read(&peer_path).unwrap();
    let target_path = tmp.path().join("config.d/memory.toml");
    let target_before = std::fs::read(&target_path).unwrap();

    let invalid = plan_config_file_mutation(
        &config_path,
        &workspace,
        "memory.toml",
        "[memory]\nbackend = 7\n".to_string(),
    )
    .unwrap_err();
    assert!(
        invalid
            .to_string()
            .contains("Staged effective configuration is invalid")
    );
    assert_eq!(std::fs::read(&target_path).unwrap(), target_before);

    let updated = "[memory]\nbackend = \"sqlite\"\nauto_save = false\n";
    let plan = plan_config_file_mutation(&config_path, &workspace, "memory.toml", updated.to_string()).unwrap();
    commit_mutation_atomically(plan).await.unwrap();

    assert_eq!(std::fs::read_to_string(target_path).unwrap(), updated);
    assert_eq!(std::fs::read(peer_path).unwrap(), peer_before);
}

#[tokio::test]
async fn explicit_unknown_file_update_is_preserved_as_user_owned() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace.clone();
    write_split_config(&config, false).await.unwrap();

    let plan = plan_config_file_mutation(
        &config_path,
        &workspace,
        "operator-owned.toml",
        "[operator_owned]\nvalue = 11\n".to_string(),
    )
    .unwrap();
    commit_mutation_atomically(plan).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.d/operator-owned.toml")).unwrap(),
        "[operator_owned]\nvalue = 11\n"
    );
    assert!(tmp.path().join("config.d/memory.toml").exists());
}

async fn split_fixture() -> (tempfile::TempDir, crate::config::Config) {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = crate::config::Config::default();
    config.config_path = tmp.path().join("config.toml");
    config.workspace_dir = tmp.path().join("workspace");
    write_split_config(&config, false).await.unwrap();
    (tmp, config)
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let config_dir = root.join("config.d");
    out.insert(
        root.join("config.toml"),
        std::fs::read(root.join("config.toml")).unwrap(),
    );
    if let Ok(entries) = std::fs::read_dir(&config_dir) {
        for entry in entries {
            let path = entry.unwrap().path();
            out.insert(path.clone(), std::fs::read(&path).unwrap());
        }
    }
    out
}

#[tokio::test]
async fn guarded_commit_refuses_a_stale_revision_and_writes_nothing() {
    let (tmp, mut config) = split_fixture().await;
    let base = current_config_revision(&config.config_path).unwrap();
    // Another writer (here: a manual edit) changes the tree afterwards.
    let manual = tmp.path().join("config.d/memory.toml");
    let mut edited = std::fs::read_to_string(&manual).unwrap();
    edited.push_str("\n# edited by hand\n");
    std::fs::write(&manual, &edited).unwrap();
    let on_disk = tree_bytes(tmp.path());
    let generation = read_config_generation(&config.config_path).unwrap();

    config.default_model = Some("stale-candidate".into());
    let plan = plan_structured_save(&config).unwrap();
    let hook_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&hook_ran);
    let error = commit_mutation_guarded(plan, Some(&base), move |_| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    })
    .await
    .unwrap_err();

    let conflict = error
        .downcast_ref::<ConfigRevisionConflict>()
        .expect("a typed conflict");
    assert_eq!(conflict.expected, base);
    assert_eq!(conflict.actual, current_config_revision(&config.config_path).unwrap());
    assert!(
        !hook_ran.load(std::sync::atomic::Ordering::SeqCst),
        "nothing may be recorded for a refused commit"
    );
    assert_eq!(tree_bytes(tmp.path()), on_disk, "the manual edit must survive");
    assert_eq!(read_config_generation(&config.config_path).unwrap(), generation);
}

#[tokio::test]
async fn guarded_commit_of_the_current_content_writes_nothing() {
    let (tmp, config) = split_fixture().await;
    let on_disk = tree_bytes(tmp.path());
    let generation = read_config_generation(&config.config_path).unwrap();
    let before = BeforeImage::capture(tmp.path());
    let plan = plan_before_image_restore(&config.config_path, &config.workspace_dir, &before).unwrap();

    let outcome = commit_mutation_guarded(plan, None, |_| bail!("must not run"))
        .await
        .unwrap();

    assert!(matches!(outcome, GuardedCommit::Unchanged { .. }), "{outcome:?}");
    assert_eq!(tree_bytes(tmp.path()), on_disk);
    assert_eq!(read_config_generation(&config.config_path).unwrap(), generation);
}

#[tokio::test]
async fn a_failing_before_publish_hook_aborts_with_nothing_written() {
    let (tmp, mut config) = split_fixture().await;
    let on_disk = tree_bytes(tmp.path());
    config.default_model = Some("unrecorded-candidate".into());
    let plan = plan_structured_save(&config).unwrap();

    let error = commit_mutation_guarded(plan, None, |_| bail!("history is unavailable"))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("history is unavailable"), "{error:#}");
    assert_eq!(tree_bytes(tmp.path()), on_disk);
    assert_eq!(read_config_generation(&config.config_path).unwrap() % 2, 0);
    assert!(!config_transaction_journal_path(&config.config_path).unwrap().exists());
}

#[tokio::test]
async fn restoring_a_guarded_before_image_is_byte_identical() {
    let (tmp, config) = split_fixture().await;
    let on_disk = tree_bytes(tmp.path());

    // A raw write that also creates a user-owned fragment.
    let plan = plan_config_file_mutation(
        &config.config_path,
        &config.workspace_dir,
        "operator-owned.toml",
        "[operator_owned]\nvalue = 1\n".to_string(),
    )
    .unwrap();
    let GuardedCommit::Committed { before, .. } = commit_mutation_guarded(plan, None, |_| Ok(())).await.unwrap() else {
        panic!("the raw write must commit");
    };
    let mut changed = config.clone();
    changed.default_model = Some("second-change".into());
    let plan = plan_structured_save(&changed).unwrap();
    let GuardedCommit::Committed { before: second, .. } =
        commit_mutation_guarded(plan, None, |_| Ok(())).await.unwrap()
    else {
        panic!("the structured write must commit");
    };
    assert!(tmp.path().join("config.d/operator-owned.toml").exists());

    // Restore the image taken before the first write: the second change
    // is undone and the created user-owned file is removed.
    let merged = ConfigBeforeImage {
        files: before
            .files
            .iter()
            .chain(second.files.iter().filter(|file| {
                !before
                    .files
                    .iter()
                    .any(|first| first.relative_path == file.relative_path)
            }))
            .cloned()
            .collect(),
    };
    let restore = plan_before_image_restore(&config.config_path, &config.workspace_dir, &merged).unwrap();
    commit_mutation_guarded(restore, None, |_| Ok(())).await.unwrap();

    assert_eq!(tree_bytes(tmp.path()), on_disk, "rollback must restore the exact bytes");
    assert!(!tmp.path().join("config.d/operator-owned.toml").exists());
}

#[test]
fn a_before_image_debug_rendering_carries_no_content() {
    let image = ConfigBeforeImage {
        files: vec![BeforeImageFile {
            relative_path: "config.toml".into(),
            contents: Some("api_key = \"plain-sentinel\"\n".into()),
        }],
    };
    let rendered = format!("{image:?}");
    assert!(!rendered.contains("plain-sentinel"), "{rendered}");
    assert!(rendered.contains("config.toml"), "{rendered}");
}

/// Test helper: the before-image of the current tree, as a guarded
/// commit would capture it for a structured save.
struct BeforeImage;

impl BeforeImage {
    fn capture(root: &Path) -> ConfigBeforeImage {
        let config_path = root.join("config.toml");
        let plan = ConfigMutationPlan {
            config_path: config_path.clone(),
            workspace_dir: root.join("workspace"),
            main_toml: String::new(),
            managed_fragments: BTreeMap::new(),
            user_owned_writes: BTreeMap::new(),
            user_owned_removals: BTreeSet::new(),
        };
        ConfigBeforeImage::from_snapshot(&config_path, &capture_mutation_snapshot(&plan).unwrap()).unwrap()
    }
}

#[tokio::test]
async fn config_save_does_not_convert_unknown_only_directory_to_managed_split() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    let mut config = crate::config::Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = workspace;
    config.save().await.unwrap();
    std::fs::create_dir_all(tmp.path().join("config.d")).unwrap();
    let unknown = tmp.path().join("config.d/operator-owned.toml");
    std::fs::write(&unknown, "[operator_owned]\nvalue = 12\n").unwrap();

    config.default_model = Some("flat-save".to_string());
    config.save().await.unwrap();

    assert!(std::fs::read_to_string(&config_path).unwrap().contains("flat-save"));
    assert_eq!(
        std::fs::read_to_string(unknown).unwrap(),
        "[operator_owned]\nvalue = 12\n"
    );
    assert!(!has_managed_fragments(&config_path).unwrap());
}

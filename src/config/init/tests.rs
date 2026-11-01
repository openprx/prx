#![cfg(test)]
use super::*;
use std::fs;

#[test]
fn every_memory_template_writes_the_embedding_provider_explicitly() {
    let defaults = crate::config::schema::MemoryConfig::default();
    for spec in [Spec::Minimal, Spec::Server, Spec::Full] {
        let content = memory_template(spec);
        assert!(
            content.contains(&format!("\nembedding_provider = \"{}\"", defaults.embedding_provider)),
            "{} template must state embedding_provider, not only comment it",
            spec.name()
        );
        assert!(
            content.contains(&format!("\nembedding_model = \"{}\"", defaults.embedding_model)),
            "{} template must state embedding_model",
            spec.name()
        );
        assert!(
            content.contains(&format!("\nembedding_dimensions = {}", defaults.embedding_dimensions)),
            "{} template must state embedding_dimensions",
            spec.name()
        );
        assert!(
            content.contains("embedding_provider = \"none\" to turn vector recall off"),
            "{} template must say how to turn vector recall off",
            spec.name()
        );
    }
}

#[tokio::test]
async fn force_regeneration_preserves_an_explicitly_disabled_embedding_provider() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let memory_path = dir.join("config.d/memory.toml");
    fs::write(
            &memory_path,
            "# Embeddings disabled for latency isolation.\n[memory]\nbackend = \"sqlite\"\nauto_save = true\nembedding_provider = \"none\"\n",
        )
        .expect("test: write operator memory fragment");

    Spec::Full.generate(dir, true).await.expect("test: force regenerate");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert_eq!(
        config.memory.embedding_provider, "none",
        "prx init --force must not undo an explicit embedding_provider"
    );
    assert!(config.memory.embedding_provider_explicit);
    assert!(config.embedding_default_upgrade_notice().is_none());
    let regenerated = fs::read_to_string(&memory_path).expect("test: read regenerated fragment");
    assert!(
        regenerated.contains("# Memory configuration (full)"),
        "the template comments must still be refreshed: {regenerated}"
    );
}

#[tokio::test]
async fn force_regeneration_preserves_values_written_in_the_main_config() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    // A pre-0.8.10x tree: the fragment never mentioned the provider, and the
    // operator disabled it from the main config file.
    fs::write(
        dir.join("config.d/memory.toml"),
        "[memory]\nbackend = \"sqlite\"\nauto_save = true\n",
    )
    .expect("test: write legacy memory fragment");
    let main_path = dir.join("config.toml");
    let mut main = fs::read_to_string(&main_path).expect("test: read main config");
    main.push_str("\n[memory]\nembedding_provider = \"none\"\n");
    fs::write(&main_path, main).expect("test: write operator main config");

    Spec::Full.generate(dir, true).await.expect("test: force regenerate");

    let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert_eq!(
        config.memory.embedding_provider, "none",
        "a fragment template must not out-rank a value the operator wrote in config.toml"
    );
}

#[tokio::test]
async fn force_regeneration_preserves_unrelated_explicit_settings() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let main_path = dir.join("config.toml");
    let mut main = fs::read_to_string(&main_path).expect("test: read main config");
    main.push_str("\ndefault_model = \"operator-model\"\n");
    let release_default = crate::providers::selection::default_model_for_unset();
    fs::write(
        &main_path,
        main.replace(&format!("default_model = \"{release_default}\"\n"), ""),
    )
    .expect("test: write operator main config");
    fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = false\nconversation_retention_days = 99\n[memory.events]\nrecord_tool_events = true\n",
        )
        .expect("test: write operator memory fragment");
    fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config]\ncli = false\n[channels_config.telegram]\nbot_token = \"operator-token\"\nallowed_users = [\"alice\"]\n",
        )
        .expect("test: write operator channels fragment");

    Spec::Full.generate(dir, true).await.expect("test: force regenerate");

    let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert_eq!(config.default_model.as_deref(), Some("operator-model"));
    assert!(!config.memory.auto_save, "an explicit false must survive regeneration");
    assert_eq!(config.memory.conversation_retention_days, 99);
    assert!(config.memory.events.record_tool_events);
    assert!(!config.channels_config.cli);
    let telegram = config
        .channels_config
        .telegram
        .as_ref()
        .expect("test: telegram settings the template only comments must survive");
    assert_eq!(telegram.bot_token, "operator-token");
    assert_eq!(telegram.allowed_users, vec!["alice".to_string()]);
}

#[tokio::test]
async fn switching_to_a_preset_without_a_fragment_still_keeps_operator_settings() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config]\ncli = true\n[channels_config.telegram]\nbot_token = \"operator-token\"\nallowed_users = [\"alice\"]\n",
        )
        .expect("test: write operator channels fragment");

    Spec::Minimal
        .generate(dir, true)
        .await
        .expect("test: force regenerate minimal");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: regenerated config must load");
    let telegram = config
        .channels_config
        .telegram
        .as_ref()
        .expect("test: a preset without a channels template must not delete channel settings");
    assert_eq!(telegram.bot_token, "operator-token");
}

/// `prx init` returns before any tracing subscriber is installed, so the
/// preservation report has to be printed, not logged. This pins the lines
/// an operator actually receives: the archive directory and the explicit
/// keys that survived.
#[tokio::test]
async fn force_regeneration_reports_the_archive_and_the_preserved_keys() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    fs::write(
        dir.join("config.d/memory.toml"),
        "[memory]\nembedding_provider = \"none\"\nconversation_retention_days = 99\n",
    )
    .expect("test: write operator memory fragment");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");

    assert!(!report.was_reset_to_templates());
    let notices = report.notices().join("\n");
    let archive_line = report
        .notices()
        .iter()
        .find(|line| line.contains("Previous configuration archived to"))
        .expect("test: the archive directory must be reported");
    let archive_path = archive_line
        .rsplit_once("archived to ")
        .map(|(_, path)| PathBuf::from(path))
        .expect("test: the archive line must name a path");
    assert!(
        archive_path.join("memory.toml").exists(),
        "the reported archive directory must be the real one: {}",
        archive_path.display()
    );
    assert!(
        notices.contains("Preserved") && notices.contains("explicit setting(s)"),
        "the preserved-key count must be reported: {notices}"
    );
    assert!(
        notices.contains("memory.conversation_retention_days"),
        "each preserved key must be named: {notices}"
    );
    assert!(report.warnings().is_empty(), "a clean preserve must not warn");
}

/// The fallback to bare templates throws away every value the operator
/// typed. It stays available — it is how `--force` repairs a configuration
/// this build cannot load — but it must be impossible to miss: an uppercase
/// warning naming the archive, and a non-zero exit.
#[tokio::test]
async fn a_configuration_that_cannot_validate_reports_the_reset_and_exits_non_zero() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    // Start without stored decision-plane settings; this exercises generic repair.
    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");
    // A telegram block without `allowed_users` folds in cleanly but fails
    // the staged validation, which is exactly the shape that used to reset
    // the whole tree with a single dropped `tracing::warn!`.
    fs::write(
        dir.join("config.d/channels.toml"),
        "[channels_config.telegram]\nbot_token = \"operator-token\"\n",
    )
    .expect("test: write invalid operator channels fragment");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: the tree is still regenerated");

    assert!(
        report.was_reset_to_templates(),
        "an unvalidatable preserve must be recorded as a reset"
    );
    let warning = report
        .warnings()
        .iter()
        .find(|line| line.contains("EVERY EXPLICIT SETTING WAS RESET"))
        .expect("test: the reset must be an uppercase warning");
    let archive = fs::read_dir(dir)
        .expect("test: read target dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("config.d.pre-"))
        })
        .expect("test: the previous configuration must still be archived");
    assert!(
        warning.contains(&archive.display().to_string()),
        "the warning must point at the archive directory: {warning}"
    );
    assert!(
        fs::read_to_string(archive.join("channels.toml"))
            .expect("test: read archived fragment")
            .contains("operator-token"),
        "the archive the warning points at must hold the operator's values"
    );

    // The tree on disk is valid and usable ...
    crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: the regenerated config must load");
    // ... but the command still fails, so no script treats this as clean.
    let error = report.into_result().expect_err("test: a reset must exit non-zero");
    assert!(
        format!("{error:#}").contains("EVERY EXPLICIT SETTING WAS RESET"),
        "the process exit must carry the same explanation: {error:#}"
    );
}

#[tokio::test]
async fn force_regeneration_archives_the_previous_configuration() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    fs::write(
        dir.join("config.d/memory.toml"),
        "[memory]\nembedding_provider = \"none\"\n",
    )
    .expect("test: write operator memory fragment");

    Spec::Full.generate(dir, true).await.expect("test: force regenerate");

    let archive = fs::read_dir(dir)
        .expect("test: read target dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("config.d.pre-"))
        })
        .expect("test: force regeneration must archive the previous configuration");
    let archived = fs::read_to_string(archive.join("memory.toml")).expect("test: read archived fragment");
    assert!(archived.contains("embedding_provider = \"none\""));
    assert!(archive.join("config.toml").exists());
}

// ── Quoted keys and quoted table headers ─────────────────────
//
// The folder used to reject any key path containing a quote. The value was
// then neither carried over nor reported: the operator's setting simply
// became this build's default on the next `--force`.

#[tokio::test]
async fn force_regeneration_keeps_a_quoted_table_header_the_template_does_not_ship() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let main_path = dir.join("config.toml");
    let mut main = fs::read_to_string(&main_path).expect("test: read main config");
    // A real model name carries a dot, so this table can only be written
    // with a quoted segment.
    main.push_str("\n[cost.prices.\"gpt-4.1\"]\ninput = 1.5\noutput = 7.25\n");
    fs::write(&main_path, main).expect("test: write operator main config");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");
    assert!(
        !report.was_reset_to_templates(),
        "a quoted header must fold, not trip the whole-tree reset: {:?}",
        report.warnings()
    );
    assert!(
        report.unpreserved_keys().is_empty(),
        "nothing should have been unfoldable: {:?}",
        report.unpreserved_keys()
    );

    let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
        .expect("test: regenerated config must load");
    let price = config
        .cost
        .prices
        .get("gpt-4.1")
        .expect("test: the quoted price table must survive --force");
    assert!((price.input - 1.5).abs() < f64::EPSILON, "input price: {}", price.input);
    assert!(
        (price.output - 7.25).abs() < f64::EPSILON,
        "output price: {}",
        price.output
    );
    // The key must be re-emitted quoted; `cost.prices.gpt-4.1` would be a
    // different path entirely.
    let regenerated = fs::read_to_string(&main_path).expect("test: read regenerated main config");
    assert!(
        regenerated.contains("[cost.prices.\"gpt-4.1\"]"),
        "the header must be re-emitted quoted: {regenerated}"
    );
    assert!(
        report.notices().join("\n").contains("cost.prices.\"gpt-4.1\".input"),
        "the preserved key must be named with its quoting: {:?}",
        report.notices()
    );
}

#[tokio::test]
async fn force_regeneration_keeps_a_quoted_key_inside_a_map_table() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let main_path = dir.join("config.toml");
    let mut main = fs::read_to_string(&main_path).expect("test: read main config");
    main.push_str("\n[reliability.model_fallbacks]\n\"gpt-4.1\" = [\"gpt-4o\", \"claude-sonnet-4-6\"]\n");
    fs::write(&main_path, main).expect("test: write operator main config");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");
    assert!(
        !report.was_reset_to_templates(),
        "a quoted key must fold, not trip the whole-tree reset: {:?}",
        report.warnings()
    );

    let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert_eq!(
        config.reliability.model_fallbacks.get("gpt-4.1").map(Vec::as_slice),
        Some(["gpt-4o".to_string(), "claude-sonnet-4-6".to_string()].as_slice()),
        "a quoted map key must keep both its name and its value"
    );
}

/// The other half of the same bug: a *template* that writes a quoted key or
/// sits under a quoted header. No shipped template does today, so this
/// exercises the rewriter directly — the day one does, the operator's value
/// must win there too.
#[test]
fn a_template_written_quoted_key_takes_the_operator_value() {
    let template = "[agent]\n\"my.key\" = \"from-template\"\n\n[channels_config.\"x.y\"]\ntoken = \"from-template\"\n";
    let previous = toml::from_str::<toml::Value>(
        "[agent]\n\"my.key\" = \"from-operator\"\n\n[channels_config.\"x.y\"]\ntoken = \"from-operator\"\n",
    )
    .expect("test: parse previous configuration");
    let mut outcome = PreservationOutcome::default();

    let merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
        .expect("test: fold the previous configuration");

    assert!(
        outcome.unfoldable.is_empty(),
        "every quoted key must be foldable: {:?}",
        outcome.unfoldable
    );
    assert!(
        merged.contains("\"my.key\" = \"from-operator\""),
        "a quoted assignment must take the operator value: {merged}"
    );
    assert!(
        merged.contains("token = \"from-operator\""),
        "a key under a quoted header must take the operator value: {merged}"
    );
    let reparsed = toml::from_str::<toml::Value>(&merged).expect("test: the merged file must still parse");
    assert_eq!(
        reparsed
            .get("channels_config")
            .and_then(|table| table.get("x.y"))
            .and_then(|table| table.get("token"))
            .and_then(toml::Value::as_str),
        Some("from-operator"),
        "the value must land on the quoted path, not on a split one: {merged}"
    );
    assert!(outcome.kept.contains("agent.\"my.key\""), "kept: {:?}", outcome.kept);
    assert!(
        outcome.kept.contains("channels_config.\"x.y\".token"),
        "kept: {:?}",
        outcome.kept
    );
}

/// Whatever the rewriter still cannot place is named and fails the command.
/// Silence is the defect this whole area was built around.
#[test]
fn a_setting_that_cannot_be_folded_is_named_and_exits_non_zero() {
    // An array of tables is left verbatim by the rewriter, so an operator
    // value there has nowhere to go.
    let template = "[[route]]\nname = \"from-template\"\n";
    let previous =
        toml::from_str::<toml::Value>("[[route]]\nname = \"from-operator\"\n").expect("test: parse previous");
    let mut outcome = PreservationOutcome::default();

    let _merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
        .expect("test: fold the previous configuration");
    assert_eq!(
        outcome.unfoldable.iter().cloned().collect::<Vec<_>>(),
        vec!["route".to_string()],
        "an unplaceable override must be recorded, not dropped in silence"
    );

    let mut report = InitReport::default();
    report.record_preservation(&outcome, None);
    let warning = report
        .warnings()
        .iter()
        .find(|line| line.contains("could not fold"))
        .expect("test: the operator must be warned");
    assert!(warning.contains("route"), "the warning must name the key: {warning}");
    let error = report
        .into_result()
        .expect_err("test: an unfoldable setting must exit non-zero");
    assert!(format!("{error:#}").contains("route"), "{error:#}");
}

/// A key whose shape this build can no longer hold is the same data loss as
/// one the rewriter could not place, so it ends the command the same way
/// and is listed under its own heading rather than folded into the other.
#[test]
fn a_setting_this_build_cannot_represent_is_listed_and_exits_non_zero() {
    // The template now writes `agent.mode` as a scalar, so the previous
    // tree's table at that path has nowhere to go.
    let template = "[agent]\nmode = \"auto\"\n";
    let previous = toml::from_str::<toml::Value>("[agent.mode]\nlegacy = true\n").expect("test: parse previous");
    let mut outcome = PreservationOutcome::default();

    let _merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
        .expect("test: fold the previous configuration");
    assert_eq!(
        outcome.dropped.iter().cloned().collect::<Vec<_>>(),
        vec!["agent.mode.legacy".to_string()],
        "a key this build cannot represent must be recorded"
    );
    assert!(
        outcome.unfoldable.is_empty(),
        "the two loss classes must stay distinct: {:?}",
        outcome.unfoldable
    );

    let mut report = InitReport::default();
    report.record_preservation(&outcome, None);
    assert_eq!(report.dropped_keys(), ["agent.mode.legacy".to_string()]);
    let error = report
        .into_result()
        .expect_err("test: a setting this build cannot represent must exit non-zero");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("can no longer represent") && rendered.contains("agent.mode.legacy"),
        "the error must list the dropped keys under their own heading: {rendered}"
    );
}

/// The mirror of the test above: a fold that loses nothing still exits zero.
#[test]
fn a_fold_that_drops_nothing_exits_zero() {
    let template = "[agent]\nmode = \"auto\"\n";
    let previous = toml::from_str::<toml::Value>("[agent]\nmode = \"manual\"\n").expect("test: parse previous");
    let mut outcome = PreservationOutcome::default();

    let merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
        .expect("test: fold the previous configuration");
    assert!(merged.contains("mode = \"manual\""), "{merged}");
    assert!(outcome.dropped.is_empty(), "dropped: {:?}", outcome.dropped);

    let mut report = InitReport::default();
    report.record_preservation(&outcome, None);
    assert!(report.dropped_keys().is_empty(), "expected an empty value");
    report.into_result().expect("test: a lossless fold must exit zero");
}

/// A step that runs after the mutation commits must not take the report
/// with it: the operator's settings are already off the live tree, so the
/// reset warning, the archive that holds them and the failure itself all
/// have to reach the caller, and the run still ends non-zero.
#[tokio::test]
async fn a_failure_after_the_commit_keeps_the_committed_report() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    // Start without stored decision-plane settings; this exercises generic repair.
    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");

    // `allowed_users` is required, so the preserved settings cannot validate
    // and `--force` falls back to the shipped templates.
    fs::write(
        dir.join("config.d/channels.toml"),
        "[channels_config]\ncli = true\n\n[channels_config.telegram]\nbot_token = \"unit-operator-token\"\n",
    )
    .expect("test: write a fragment this build cannot load");
    // Obstruct a workspace subdirectory so the scaffolding after the commit fails.
    let sessions = dir.join("workspace").join("sessions");
    fs::remove_dir_all(&sessions).expect("test: clear workspace/sessions");
    fs::write(&sessions, "not a directory").expect("test: obstruct workspace/sessions");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: a failure after the commit must still return the report");

    assert!(
        report.was_reset_to_templates(),
        "the fixture must reproduce the reset: {:?}",
        report.warnings()
    );
    let failure = report
        .post_commit_failure()
        .expect("test: the failure that stopped the run must be reported");
    assert!(
        failure.contains("workspace/sessions"),
        "the failure must name what it could not create: {failure}"
    );

    let archives = archive_dirs(dir);
    assert_eq!(archives.len(), 1, "exactly one archive: {archives:?}");
    let archive = archives.first().expect("test: the archive must exist");
    let archive_path = archive.display().to_string();
    assert!(
        report.warnings().iter().any(|line| line.contains(&archive_path)),
        "the archive path must be printed: {:?}",
        report.warnings()
    );
    assert!(
        fs::read_to_string(archive.join("channels.toml"))
            .expect("test: read the archived fragment")
            .contains("unit-operator-token"),
        "the archive must hold the operator's original value"
    );
    assert!(
        !fs::read_to_string(dir.join("config.d/channels.toml"))
            .expect("test: read the regenerated fragment")
            .contains("unit-operator-token"),
        "the reset the report describes must actually have committed"
    );

    let error = report
        .into_result()
        .expect_err("test: a failure after the commit must exit non-zero");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("workspace/sessions") && rendered.contains("reset"),
        "the error must name both the failure and the mutation: {rendered}"
    );
}

// ── Archive directories ──────────────────────────────────────

#[test]
fn archive_directories_never_overwrite_one_another() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let first = create_unique_archive_dir(tmp.path(), "config.d.pre-0.0.0-20260915120000000")
        .expect("test: first archive directory");
    let second = create_unique_archive_dir(tmp.path(), "config.d.pre-0.0.0-20260915120000000")
        .expect("test: second archive directory");

    assert_ne!(
        first, second,
        "a second archive with the same stamp must get its own directory"
    );
    assert!(first.is_dir() && second.is_dir(), "both archives must exist");
}

#[tokio::test]
async fn two_force_runs_in_the_same_second_keep_both_archives() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let memory_path = dir.join("config.d/memory.toml");
    fs::write(
        &memory_path,
        "[memory]\nbackend = \"sqlite\"\nembedding_model = \"first-model\"\n",
    )
    .expect("test: write the first operator fragment");
    Spec::Full.generate(dir, true).await.expect("test: first force");
    fs::write(
        &memory_path,
        "[memory]\nbackend = \"sqlite\"\nembedding_model = \"second-model\"\n",
    )
    .expect("test: write the second operator fragment");
    Spec::Full.generate(dir, true).await.expect("test: second force");

    let mut archived_models = fs::read_dir(dir)
        .expect("test: read target dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("config.d.pre-"))
        })
        .map(|archive| fs::read_to_string(archive.join("memory.toml")).unwrap_or_default())
        .collect::<Vec<_>>();
    archived_models.sort();

    assert_eq!(
        archived_models.len(),
        2,
        "each --force run must keep its own archive, found: {archived_models:?}"
    );
    assert!(
        archived_models.iter().any(|body| body.contains("first-model")),
        "the first run's archive must survive the second run: {archived_models:?}"
    );
    assert!(
        archived_models.iter().any(|body| body.contains("second-model")),
        "the second run's archive must exist: {archived_models:?}"
    );
}

/// The defaults sidecar is the baseline the *next* `--force` classifies
/// against, so an archive that omits it cannot restore a working tree.
#[tokio::test]
async fn the_archive_carries_the_defaults_sidecar() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    assert!(
        dir.join(MANAGED_DEFAULTS_FILE).is_file(),
        "the first run must write the defaults record"
    );

    Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");

    let archives = archive_dirs(dir);
    assert_eq!(archives.len(), 1, "one --force run makes one archive: {archives:?}");
    let archive = archives.first().expect("test: the archive must exist");
    assert!(
        archive.join(MANAGED_DEFAULTS_FILE).is_file(),
        "the archive must carry the defaults record next to the files it describes"
    );
    assert!(
        archive.join("config.toml").is_file() && archive.join("memory.toml").is_file(),
        "the archive must still carry config.toml and the fragments"
    );
}

/// A tree written by a build older than the sidecar has none to archive,
/// which is an ordinary case and not a failure.
#[tokio::test]
async fn archiving_a_tree_without_a_sidecar_succeeds() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    fs::remove_file(dir.join(MANAGED_DEFAULTS_FILE)).expect("test: remove the defaults record");

    Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: an older tree must still regenerate");

    let archives = archive_dirs(dir);
    assert_eq!(archives.len(), 1, "one --force run makes one archive: {archives:?}");
    let archive = archives.first().expect("test: the archive must exist");
    assert!(
        !archive.join(MANAGED_DEFAULTS_FILE).exists(),
        "there was no sidecar to archive"
    );
    assert!(
        archive.join("config.toml").is_file(),
        "the archive must still be written"
    );
}

/// Every `config.d.pre-*` directory directly under `dir`, sorted.
fn archive_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut archives = fs::read_dir(dir)
        .expect("test: read target dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.d.pre-"))
        })
        .collect::<Vec<_>>();
    archives.sort();
    archives
}

// ── Default refresh semantics ────────────────────────────────

/// Read the value a freshly rendered template writes for `path`, so these
/// tests state the *rule* and never a copy of a shipped default.
fn template_default(template: &str, path: &[&str]) -> toml::Value {
    let value = toml::from_str::<toml::Value>(template).expect("test: the template must be valid TOML");
    let owned = path.iter().map(|segment| (*segment).to_string()).collect::<Vec<_>>();
    toml_lookup(&value, &owned)
        .cloned()
        .unwrap_or_else(|| panic!("test: the template must write {path:?}"))
}

#[tokio::test]
async fn force_refresh_separates_preserved_added_and_reset_defaults() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let template = memory_template(Spec::Full);
    let shipped_auto_save = template_default(&template, &["memory", "auto_save"]);
    let shipped_record_tool_events = template_default(&template, &["memory", "events", "record_tool_events"]);

    // The tree on disk holds two values that differ from this build's
    // templates. Only one of them was ever typed by the operator.
    fs::write(
        dir.join("config.d/memory.toml"),
        "[memory]\nbackend = \"sqlite\"\nauto_save = false\n\n[memory.events]\nrecord_tool_events = true\n",
    )
    .expect("test: write operator memory fragment");
    // The sidecar says `auto_save = false` is what the *previous* templates
    // shipped, so it is a stale default; `record_tool_events` is recorded at
    // its shipped value, so the operator's `true` is their own choice.
    fs::write(
        dir.join(MANAGED_DEFAULTS_FILE),
        "[[entry]]\npath = [\"memory\", \"auto_save\"]\nvalue = false\n\n\
             [[entry]]\npath = [\"memory\", \"events\", \"record_tool_events\"]\nvalue = false\n",
    )
    .expect("test: write the previous defaults record");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");
    let notices = report.notices().join("\n");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert_eq!(
        toml::Value::Boolean(config.memory.auto_save),
        shipped_auto_save,
        "a default the operator never changed must follow the template: {notices}"
    );
    assert_ne!(
        toml::Value::Boolean(config.memory.events.record_tool_events),
        shipped_record_tool_events,
        "a value the operator wrote must win over a refreshed default: {notices}"
    );

    let reset_line = report
        .notices()
        .iter()
        .find(|line| line.contains("refreshed defaults"))
        .expect("test: the refreshed defaults must be reported");
    assert!(reset_line.contains("memory.auto_save"), "{reset_line}");
    let preserved_line = report
        .notices()
        .iter()
        .find(|line| line.contains("Preserved"))
        .expect("test: the preserved settings must be reported");
    assert!(
        preserved_line.contains("memory.events.record_tool_events"),
        "{preserved_line}"
    );
    let added_line = report
        .notices()
        .iter()
        .find(|line| line.contains("new default(s) this version ships"))
        .expect("test: keys this version adds must be reported");
    assert!(
        added_line.contains("memory.embedding_provider"),
        "a key absent from the previous tree is an added default: {added_line}"
    );
    // The three classes are disjoint.
    assert!(!reset_line.contains("memory.events.record_tool_events"), "{reset_line}");
    assert!(!preserved_line.contains("memory.auto_save"), "{preserved_line}");
}

#[tokio::test]
async fn without_a_defaults_record_every_current_value_is_preserved() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");
    assert!(
        dir.join(MANAGED_DEFAULTS_FILE).is_file(),
        "prx init must record which values came from its own templates"
    );

    fs::write(
        dir.join("config.d/memory.toml"),
        "[memory]\nbackend = \"sqlite\"\nauto_save = false\n",
    )
    .expect("test: write operator memory fragment");
    // A tree written by a build that had no record at all.
    fs::remove_file(dir.join(MANAGED_DEFAULTS_FILE)).expect("test: remove the defaults record");

    let report = Spec::Full
        .generate_reported(dir, true)
        .await
        .expect("test: force regenerate");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: regenerated config must load");
    assert!(
        !config.memory.auto_save,
        "with no record of the previous defaults, every value on disk counts as the operator's"
    );
    assert!(
        report.notices().iter().all(|line| !line.contains("refreshed defaults")),
        "nothing may be reset without evidence that it was a shipped default: {:?}",
        report.notices()
    );
}

#[test]
fn spec_name_matches_variant() {
    assert_eq!(Spec::Minimal.name(), "minimal");
    assert_eq!(Spec::Server.name(), "server");
    assert_eq!(Spec::Full.name(), "full");
}

#[test]
fn main_config_template_contains_spec_name() {
    let content = main_config_template(Spec::Server);
    assert!(content.contains("--spec server"));
    assert!(!content.contains("[modules]"));
    assert!(content.contains("default_model"));
}

#[tokio::test]
async fn generate_creates_expected_files() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");

    assert!(dir.join("config.toml").exists());
    assert!(dir.join("config.d").is_dir());
    assert!(dir.join("workspace/sessions").is_dir());
    assert!(dir.join("workspace/memory").is_dir());
    assert!(dir.join("workspace/state").is_dir());
    assert!(dir.join("workspace/cron").is_dir());
    assert!(dir.join("workspace/skills").is_dir());

    for name in super::super::files::managed_fragment_names() {
        assert!(dir.join("config.d").join(name).exists(), "missing config.d/{name}");
    }
}

#[tokio::test]
async fn generate_full_creates_all_module_files() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Full.generate(dir, false).await.expect("test: generate full");

    for name in &[
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
    ] {
        assert!(dir.join("config.d").join(name).exists(), "missing config.d/{name}");
    }
}

#[tokio::test]
async fn generated_templates_load_as_valid_config() {
    // BUG-D1-03: the generated server/full templates must be loadable
    // out-of-the-box. Regression guard against illegal enum values / missing
    // required fields (compaction mode, autonomy.forbidden_paths,
    // tunnel.provider). Loads through the real merge + deserialize + validate
    // path so it exercises exactly what `prx info` would.
    for spec in [Spec::Minimal, Spec::Server, Spec::Full] {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        spec.generate(dir, false)
            .await
            .unwrap_or_else(|e| panic!("test: generate {}: {e}", spec.name()));

        let config_path = dir.join("config.toml");
        let workspace_dir = dir.join("workspace");
        crate::config::Config::load_from_path(&config_path, workspace_dir).unwrap_or_else(|e| {
            panic!(
                "test: generated `{}` template must load as a valid Config: {e:#}",
                spec.name()
            )
        });
    }
}

#[tokio::test]
async fn generated_full_template_round_trips_schema_native_values() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: full template must deserialize without ignored paths");
    assert_eq!(config.http_request.max_response_size, 10_485_760);
    assert_eq!(config.memory.embedding_provider, "local");
    assert_eq!(config.memory.embedding_model, "prx-local-hash-v1");
    assert_eq!(config.memory.embedding_dimensions, 384);
}

#[tokio::test]
async fn generated_config_rejects_unknown_paths() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");

    let tools_path = dir.join("config.d/tools.toml");
    fs::write(&tools_path, "[web_search]\nprovider = \"duckduckgo\"\ntyop = true\n")
        .expect("test: write invalid tools fragment");
    let error = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect_err("test: unknown config path must fail")
        .to_string();
    assert!(error.contains("web_search.tyop"), "unexpected error: {error}");
}

#[tokio::test]
async fn legacy_config_keys_are_migrated_explicitly() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();
    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");

    let main_path = dir.join("config.toml");
    let mut main = fs::read_to_string(&main_path).expect("test: read main config");
    main.push_str("\n[modules]\nmemory = false\ntools = true\n");
    fs::write(&main_path, main).expect("test: write legacy module gates");

    fs::write(
        dir.join("config.d/tools.toml"),
        "[http_request]\nmax_response_bytes = 2048\nenabled = false\n",
    )
    .expect("test: write legacy tools fragment");
    fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = true\n[memory.embedding]\nprovider = \"none\"\nmodel = \"legacy\"\ndimension = 512\n",
        )
        .expect("test: write legacy memory fragment");

    let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("test: known legacy keys must migrate");
    assert_eq!(config.http_request.max_response_size, 2048);
    assert_eq!(config.memory.embedding_provider, "none");
    assert_eq!(config.memory.embedding_model, "legacy");
    assert_eq!(config.memory.embedding_dimensions, 512);
}

#[tokio::test]
async fn generated_server_full_templates_do_not_pin_compaction_context() {
    for spec in [Spec::Server, Spec::Full] {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        spec.generate(dir, false)
            .await
            .unwrap_or_else(|e| panic!("test: generate {}: {e}", spec.name()));

        let agent = fs::read_to_string(dir.join("config.d/agent.toml")).expect("test: read agent template");
        assert!(
            !agent.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("max_context_tokens") && !trimmed.starts_with('#')
            }),
            "{} template must not explicitly override model-derived context",
            spec.name()
        );

        let config_path = dir.join("config.toml");
        let workspace_dir = dir.join("workspace");
        let loaded = crate::config::Config::load_from_path(&config_path, workspace_dir)
            .unwrap_or_else(|e| panic!("test: generated `{}` template must load: {e:#}", spec.name()));
        assert!(
            !loaded.agent.compaction.max_context_tokens_explicit,
            "{} template must leave compaction context model-derived",
            spec.name()
        );
    }
}

#[tokio::test]
async fn generate_refuses_overwrite_without_force() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Minimal.generate(dir, false).await.expect("test: first generate");
    let result = Spec::Minimal.generate(dir, false).await;
    assert!(result.is_err());
    assert!(
        result
            .as_ref()
            .err()
            .map_or(false, |e| format!("{e}").contains("--force"))
    );
}

#[tokio::test]
async fn generate_allows_overwrite_with_force() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Minimal.generate(dir, false).await.expect("test: first generate");
    Spec::Server.generate(dir, true).await.expect("test: force overwrite");

    let content = fs::read_to_string(dir.join("config.toml")).expect("test: read config");
    assert!(content.contains("--spec server"));
}

#[tokio::test]
async fn force_regeneration_removes_stale_managed_fragments_but_preserves_unknown() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Full.generate(dir, false).await.expect("test: generate full");
    let unknown = dir.join("config.d/operator-owned.toml");
    fs::write(&unknown, "[operator_owned]\nvalue = 7\n").expect("test: write unknown fragment");

    Spec::Minimal
        .generate(dir, true)
        .await
        .expect("test: force regenerate minimal");

    assert!(dir.join("config.d/memory.toml").exists());
    assert!(dir.join("config.d/agent.toml").exists());
    assert!(unknown.exists(), "unknown user-owned fragment must be preserved");
    for managed in [
        "channels.toml",
        "network.toml",
        "security.toml",
        "scheduler.toml",
        "identity.toml",
        "routing.toml",
        "tools.toml",
        "integrations.toml",
        "nodes.toml",
        "cost.toml",
        "observability.toml",
    ] {
        assert!(dir.join("config.d").join(managed).exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn generated_files_have_restricted_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Minimal
        .generate(dir, false)
        .await
        .expect("test: generate minimal");

    let config_perms = fs::metadata(dir.join("config.toml"))
        .expect("test: config metadata")
        .permissions()
        .mode();
    // Check that the file permission bits (lower 9 bits) are 0o600
    assert_eq!(config_perms & 0o777, 0o600);
}

#[tokio::test]
async fn generate_creates_workspace_md_files() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Full.generate(dir, false).await.expect("test: generate full");

    let ws = dir.join("workspace");
    for name in &[
        "IDENTITY.md",
        "AGENTS.md",
        "HEARTBEAT.md",
        "SOUL.md",
        "USER.md",
        "TOOLS.md",
        "BOOTSTRAP.md",
        "MEMORY.md",
        "THINKING.md",
    ] {
        assert!(
            crate::identity::root::file_directory(&ws, name).join(name).exists(),
            "missing workspace/{name}"
        );
    }

    // Verify content includes agent name
    let soul = fs::read_to_string(ws.join("identities/main/SOUL.md")).expect("test: read SOUL.md");
    assert!(soul.contains("OpenPRX"), "SOUL.md should contain agent name");

    // Verify content includes user from env (or fallback)
    let user_md = fs::read_to_string(ws.join("USER.md")).expect("test: read USER.md");
    assert!(user_md.contains("Name:"), "USER.md should contain Name field");

    let tools_md = fs::read_to_string(ws.join("TOOLS.md")).expect("test: read TOOLS.md");
    assert!(tools_md.contains("runtime `ToolSpec` snapshot"));
    assert!(!tools_md.contains("## Built-in Tools"));
}

#[test]
fn full_security_template_configures_always_on_audit() {
    let content = security_template(Spec::Full);
    assert!(content.contains("[security.audit]"));
    assert!(content.contains("log_path = \"audit.log\""));
    assert!(content.contains("max_size_mb = 100"));
    assert!(content.contains("[compliance.interaction_notice]"));
    assert!(content.contains("[compliance.eu_ai_act.classification]"));
    assert!(content.contains("status = \"unclassified\""));
}

#[test]
fn full_security_template_is_open_by_default() {
    let content = security_template(Spec::Full);
    assert!(content.contains("level = \"full\""));
    assert!(content.contains("workspace_only = false"));
    assert!(content.contains("forbidden_paths = []"));
    assert!(!content.contains("[autonomy.sandbox]"));
}

#[test]
fn server_security_template_defaults_to_autonomous_with_workspace_boundary() {
    let content = security_template(Spec::Server);
    assert!(content.contains("level = \"full\""));
    assert!(content.contains("workspace_only = true"));
    assert!(content.contains("max_cost_per_day_cents = 500"));
    assert!(content.contains("\"/etc\""));
    assert!(!content.contains("level = \"supervised\""));
}

#[test]
fn full_memory_template_uses_schema_native_embedding_keys() {
    let content = memory_template(Spec::Full);
    assert!(!content.contains("[memory.embedding]"));
    assert!(content.contains("# embedding_provider = \"custom:http://127.0.0.1:11434/v1\""));
    assert!(content.contains("# embedding_model = \"nomic-embed-text:latest\""));
    assert!(content.contains("# embedding_dimensions = 768"));
}

#[test]
fn server_scheduler_template_is_commented_scaffold() {
    let content = scheduler_template(Spec::Server);
    assert!(!content.is_empty(), "server scheduler template must not be empty");
    assert!(content.contains("Scheduler configuration (server)"));
    // All actual table headers must stay commented so the disabled module
    // does not get activated by the scaffold.
    assert!(content.contains("# [scheduler]"));
    assert!(content.contains("# [cron]"));
    assert!(content.contains("# [heartbeat]"));
    assert!(!content.contains("\n[scheduler]\n"));
}

#[test]
fn server_channels_template_is_commented_scaffold() {
    let content = channels_template(Spec::Server);
    assert!(!content.is_empty(), "server channels template must not be empty");
    assert!(content.contains("Channels configuration (server)"));
    assert!(content.contains("# [channels_config.telegram]"));
    // No platform should be active by default.
    assert!(!content.contains("\n[channels_config.telegram]\n"));
}

#[tokio::test]
async fn server_generate_writes_scheduler_and_channels_scaffolds() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Server.generate(dir, false).await.expect("test: generate server");

    // Server preset writes configuration scaffolds without module gates.
    assert!(dir.join("config.d/scheduler.toml").exists());
    assert!(dir.join("config.d/channels.toml").exists());

    let scheduler = fs::read_to_string(dir.join("config.d/scheduler.toml")).expect("test: read scheduler");
    assert!(scheduler.contains("# [scheduler]"));
    assert!(scheduler.contains("# claim_lease_secs = 90"));
    let channels = fs::read_to_string(dir.join("config.d/channels.toml")).expect("test: read channels");
    assert!(channels.contains("# [channels_config.telegram]"));

    let config = fs::read_to_string(dir.join("config.toml")).expect("test: read config");
    assert!(!config.contains("[modules]"));
}

#[tokio::test]
async fn scaffold_skips_existing_md_files() {
    let tmp = tempfile::tempdir().expect("test: create tempdir");
    let dir = tmp.path();

    Spec::Minimal.generate(dir, false).await.expect("test: first generate");

    // Write a custom SOUL.md before second generate
    let soul_path = dir.join("workspace/identities/main/SOUL.md");
    fs::write(&soul_path, "# My custom soul\n").expect("test: write custom soul");

    // Force regenerate — config files are overwritten, but .md files should be skipped
    Spec::Full.generate(dir, true).await.expect("test: force overwrite");

    let soul = fs::read_to_string(&soul_path).expect("test: read SOUL.md");
    assert_eq!(soul, "# My custom soul\n", "existing .md files must not be overwritten");
}

/// D-074 with X-3 and O6: the frozen full template from BASE `8a5d160d`
/// contains a bare `enabled = false`. X-3 used to read it as the old
/// serialized default and turn the plane on; the two readings differ in
/// whether the decision plane runs, so the migration refuses the bare
/// disable and names the `schema_version = 2` spelling that keeps it.
#[test]
fn the_base_full_template_disable_is_refused_rather_than_flipped_on() {
    let mut root: toml::Value =
        toml::from_str(include_str!("../fixtures/agent-full-base-8a5d160d.toml")).expect("test: template parses");
    assert_eq!(
        root.get("causal_tree")
            .and_then(|table| table.as_table())
            .map(|table| table.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["enabled".to_string()]),
        "the template writes a bare disable"
    );
    let error = crate::causal_tree::config::migrate_legacy(&mut root, &|_| None)
        .expect_err("a bare unversioned disable is refused");
    assert_eq!(error.key, "enabled");
    assert!(error.remedy.contains("schema_version = 2"), "{error:?}");
}

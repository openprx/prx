use super::*;
use crate::config::generation::{ConfigGeneration, ConfigGenerationParticipant, PreparedConfigGeneration};
use crate::config::secret_paths::fixture;
use std::collections::BTreeMap;

/// A runtime participant that refuses any generation whose default
/// temperature is above 1.5, as a provider rebuild refuses a bad default.
struct RefusesHotTemperature;

struct Accepted;

impl PreparedConfigGeneration for Accepted {
    fn commit(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn rollback(&mut self) {}
}

impl ConfigGenerationParticipant for RefusesHotTemperature {
    fn name(&self) -> &'static str {
        "test_refuses_hot_temperature"
    }

    fn prepares_for_field(&self, field: &str) -> bool {
        field == "default_temperature"
    }

    fn prepare(
        &self,
        generation: Arc<ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        if generation.effective.default_temperature > 1.5 {
            anyhow::bail!("test participant refuses a temperature above 1.5");
        }
        Ok(Box::new(Accepted))
    }
}

/// A participant of the CLI's own validation reload that, while the CLI
/// activates, plays a running PRX settling the deferred record as
/// `activated`, then refuses the CLI's activation.
struct SettlesThenRefuses {
    config_path: PathBuf,
}

struct RefusesCliActivation;

impl ConfigGenerationParticipant for RefusesCliActivation {
    fn name(&self) -> &'static str {
        "test_refuses_cli_activation"
    }

    fn prepares_for_field(&self, field: &str) -> bool {
        field == "default_temperature"
    }

    fn prepare(
        &self,
        _generation: Arc<ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        anyhow::bail!("test CLI activation refused")
    }
}

impl ConfigGenerationParticipant for SettlesThenRefuses {
    fn name(&self) -> &'static str {
        "test_settles_then_refuses"
    }

    fn prepares_for_field(&self, field: &str) -> bool {
        field == "default_temperature"
    }

    fn prepare(
        &self,
        _generation: Arc<ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        let history = ConfigHistory::for_config(&self.config_path)?;
        for mut record in history.list()? {
            if record.status == RecordStatus::ActivationDeferred {
                record.status = RecordStatus::Activated;
                history.update(&mut record)?;
            }
        }
        anyhow::bail!("test participant refuses after the daemon settled the change")
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    shared: SharedConfig,
    _participant: Arc<dyn ConfigGenerationParticipant>,
}

impl Fixture {
    fn config_path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        out.insert(
            self.config_path(),
            std::fs::read(self.config_path()).expect("test: read config.toml"),
        );
        if let Ok(entries) = std::fs::read_dir(self.dir.path().join("config.d")) {
            for entry in entries {
                let path = entry.expect("test: entry").path();
                out.insert(path.clone(), std::fs::read(&path).expect("test: read fragment"));
            }
        }
        out
    }

    fn service(&self) -> ConfigTransactions {
        ConfigTransactions::new(Arc::clone(&self.shared))
    }

    fn desired(&self) -> Config {
        self.shared.desired().config.as_ref().clone()
    }
}

/// The secret fixture saved in the split layout and loaded through the
/// real load path, with the refusing participant registered.
async fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let mut config = fixture::config_in(dir.path());
    // The confirmation tests exercise the rule below Full (at Full none is demanded).
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    crate::config::files::write_split_config(&config, false)
        .await
        .expect("test: write split fixture");
    let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load fixture");
    let shared = crate::config::new_shared(loaded);
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesHotTemperature);
    shared.register_participant(&participant);
    Fixture {
        dir,
        shared,
        _participant: participant,
    }
}

async fn apply_structured(
    fixture: &Fixture,
    change: impl FnOnce(&mut Config),
    expected: Option<&str>,
) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
    let service = fixture.service();
    let mut config = fixture.desired();
    change(&mut config);
    let plan = service.plan(ConfigChange::Structured(Box::new(config))).await?;
    // The confirmation names the revision the change is checked against.
    let planned = plan.planned_revision().map(str::to_string);
    service
        .apply(
            plan,
            ConfigActor::GatewayConfigApi,
            ApplyOptions {
                expected_revision: expected,
                confirm_high_risk: true,
                confirmed_revision: expected.or(planned.as_deref()),
            },
        )
        .await
}

#[tokio::test]
async fn an_activated_change_is_recorded_with_its_redacted_plan() {
    let fixture = fixture().await;
    let generation = fixture.shared.active_generation_id();

    let outcome = apply_structured(&fixture, |config| config.default_temperature = 0.4, None)
        .await
        .expect("test: apply");

    assert_ne!(fixture.shared.active_generation_id(), generation);
    assert!((fixture.shared.load_full().default_temperature - 0.4).abs() < f64::EPSILON);
    let record_id = outcome.record_id.expect("test: recorded");
    let record = fixture
        .service()
        .history()
        .expect("test: history")
        .get(&record_id)
        .expect("test: record");
    assert_eq!(record.status, RecordStatus::Activated);
    assert_eq!(
        record
            .summary
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["default_temperature"]
    );
    assert_eq!(
        record.committed_revision,
        ConfigTransactions::base_revision(&fixture.shared.desired())
    );
}

/// Apply `change` as the `prx` CLI does: a process without the daemon's
/// participants, recording `activation_deferred`.
async fn apply_deferred(fixture: &Fixture, change: impl FnOnce(&mut Config)) -> String {
    let cli = ConfigTransactions::new(crate::config::new_shared(fixture.desired())).deferring_activation();
    let mut config = fixture.desired();
    change(&mut config);
    let plan = cli
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");
    let outcome = cli
        .apply(plan, ConfigActor::Cli, ApplyOptions::default())
        .await
        .expect("test: the CLI writes it");
    let record = outcome.record_id.expect("test: recorded");
    assert_eq!(
        fixture
            .service()
            .history()
            .expect("test: history")
            .get(&record)
            .expect("test: record")
            .status,
        RecordStatus::ActivationDeferred
    );
    record
}

/// What the watcher saw of a successful reload.
fn watcher_active(report: &ConfigApplyReport) -> WatcherActivation {
    WatcherActivation {
        revision: report.desired_source_revision.fingerprint_sha256.clone(),
        outcome: WatcherOutcome::Active {
            restart_required: report.restart_required.clone(),
        },
    }
}

/// What the watcher saw of a refused reload: bound to the revision the
/// reload evaluated.
fn watcher_refused(fixture: &Fixture, error: &anyhow::Error) -> WatcherActivation {
    WatcherActivation {
        revision: fixture
            .shared
            .status()
            .last_failure
            .and_then(|failure| failure.desired_source_revision)
            .map(|revision| revision.fingerprint_sha256)
            .expect("test: the refused reload names its revision"),
        outcome: WatcherOutcome::Refused {
            reason: format!("{error:#}"),
        },
    }
}

fn record_of(fixture: &Fixture, id: &str) -> HistoryRecord {
    fixture
        .service()
        .history()
        .expect("test: history")
        .get(id)
        .expect("test: record")
}

/// RED before PMM-H (D-085 F-4): a change the CLI wrote stayed
/// `activation_deferred` whatever the running PRX did with it. The
/// daemon's watcher now settles it: `activated` when the reload took it.
#[tokio::test]
async fn the_watcher_marks_a_deferred_change_it_activated() {
    let fixture = fixture().await;
    let record = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;

    let report = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect("test: the daemon takes it");
    let settled = fixture
        .service()
        .settle_deferred_activation(watcher_active(&report))
        .await;

    assert_eq!(settled, Settlement::Settled(RecordStatus::Activated));
    let stored = record_of(&fixture, &record);
    assert_eq!(stored.status, RecordStatus::Activated);
    assert_eq!(stored.active_generation, Some(fixture.shared.active_generation_id().0));
    // Settled once: nothing names the revision any more.
    assert_eq!(
        fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await,
        Settlement::NotYet
    );
}

/// RED before PMM-H (D-085 F-4): a CLI change the running PRX refused
/// stayed on disk (desired and active disagreed) and the record said
/// `activation_deferred`. The watcher now restores the before-image,
/// re-activates it, and records `rolled_back` with the reason.
#[tokio::test]
async fn the_watcher_restores_a_deferred_change_it_refused() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let generation = fixture.shared.active_generation_id();
    let record = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
    assert_ne!(fixture.tree(), before, "test precondition: the CLI wrote the change");

    let refused = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect_err("test: the daemon's participant refuses it");
    let settled = fixture
        .service()
        .settle_deferred_activation(watcher_refused(&fixture, &refused))
        .await;

    assert_eq!(settled, Settlement::Settled(RecordStatus::RolledBack));
    assert_eq!(fixture.tree(), before, "the refused change is still on disk");
    assert!((fixture.shared.load_full().default_temperature - 1.9).abs() > f64::EPSILON);
    let stored = record_of(&fixture, &record);
    assert_eq!(stored.status, RecordStatus::RolledBack);
    let reason = stored.reason.unwrap_or_default();
    assert!(reason.contains("refuses a temperature above 1.5"), "{reason}");
    assert!(fixture.shared.active_generation_id().0 >= generation.0);
}

/// RED before PMMH-f (D-101 F-1, R-PMMH race A): a second CLI write that
/// lands between the watcher's refused reload of the first and its
/// settlement was compensated with the first write's refusal (the valid
/// second change rolled back, the refused content written back). The
/// settlement is bound to the refused revision now: nothing is restored
/// over the later write, the refused record becomes `superseded`, and
/// the second record is left for its own outcome.
#[tokio::test]
async fn a_refusal_never_restores_over_a_later_write() {
    let fixture = fixture().await;
    let first = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
    let refused = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect_err("test: the daemon's participant refuses it");
    let seen = watcher_refused(&fixture, &refused);
    // The second write lands before the settlement.
    let second = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
    let after_second = fixture.tree();

    let settled = fixture.service().settle_deferred_activation(seen).await;

    assert_eq!(settled, Settlement::Settled(RecordStatus::Superseded));
    assert_eq!(fixture.tree(), after_second, "the later write was reverted");
    assert_eq!(record_of(&fixture, &first).status, RecordStatus::Superseded);
    assert_eq!(record_of(&fixture, &second).status, RecordStatus::ActivationDeferred);
    // The second write's own outcome settles it.
    let report = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect("test: the daemon takes the second write");
    assert_eq!(
        fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await,
        Settlement::Settled(RecordStatus::Activated)
    );
    assert_eq!(record_of(&fixture, &second).status, RecordStatus::Activated);
}

#[tokio::test]
async fn a_stale_refusal_after_return_to_baseline_does_not_restore_over_a_newer_write() {
    let fixture = fixture().await;
    let baseline = fixture.tree();
    let baseline_revision =
        crate::config::files::current_config_revision(&fixture.config_path()).expect("test: baseline revision");
    let first = apply_deferred(&fixture, |config| config.default_temperature = 1.9).await;
    let refused = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect_err("test: watcher refuses R1 before settlement");
    let seen = watcher_refused(&fixture, &refused);

    // Stage the exact R0 files while the captured R1 refusal is still
    // pending. The CLI's structured rendering is not byte-identical.
    for (path, bytes) in baseline {
        std::fs::write(path, bytes).expect("test: restore exact R0 file");
    }
    assert_eq!(
        crate::config::files::current_config_revision(&fixture.config_path()).expect("test: R0 revision"),
        baseline_revision,
        "test: R0 returned to byte-identical files"
    );
    let second = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
    let after_second = fixture.tree();

    assert_eq!(
        fixture.service().settle_deferred_activation(seen).await,
        Settlement::Settled(RecordStatus::Superseded)
    );
    assert_eq!(fixture.tree(), after_second, "stale R1 outcome restored over R2");
    assert_eq!(record_of(&fixture, &first).status, RecordStatus::Superseded);
    assert_eq!(record_of(&fixture, &second).status, RecordStatus::ActivationDeferred);

    let report = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect("test: watcher loads R2");
    assert_eq!(
        fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await,
        Settlement::Settled(RecordStatus::Activated)
    );
    assert_eq!(record_of(&fixture, &second).status, RecordStatus::Activated);
}

/// RED before PMMH-f (D-101 F-1, R-PMMH race B): the CLI recorded its
/// committed revision only after its own validation reload, so a
/// watcher reload that ran first found no record and nothing retried. The
/// revision is recorded right after the commit now, and an outcome that
/// finds no record yet reports `NotYet`, so the watcher retries and
/// settles it once the record names the revision.
#[tokio::test]
async fn an_outcome_that_finds_no_record_yet_is_settled_on_retry() {
    let fixture = fixture().await;
    let record = apply_deferred(&fixture, |config| config.default_temperature = 0.4).await;
    let recorded = record_of(&fixture, &record);
    let committed = recorded
        .committed_revision
        .clone()
        .expect("test: recorded right after the commit");
    assert_eq!(
        Some(committed.as_str()),
        crate::config::files::current_config_revision(&fixture.config_path())
            .ok()
            .as_deref()
    );
    // The window before the CLI recorded its commit.
    let history = fixture.service().history().expect("test: history");
    let mut hidden = recorded.clone();
    hidden.status = RecordStatus::Pending;
    hidden.committed_revision = None;
    history.update(&mut hidden).expect("test: hide the record");
    let report = fixture
        .shared
        .reload_from_disk(ConfigReloadTrigger::FileWatcher)
        .expect("test: the daemon takes it");
    assert_eq!(
        fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await,
        Settlement::NotYet
    );
    // The CLI records it; the retry settles it.
    let mut shown = recorded;
    history.update(&mut shown).expect("test: record it");
    assert_eq!(
        fixture
            .service()
            .settle_deferred_activation(watcher_active(&report))
            .await,
        Settlement::Settled(RecordStatus::Activated)
    );
}

/// RED before PMMH-f (D-101 F-4): the transaction scrubbed its reasons by
/// replacing every secret value of the candidate as a substring, so a
/// short value garbled unrelated words of a refusal ("a *** above
/// 1.5"). Reasons are redacted by shape now: the refusal reads as
/// written, and a secret-shaped token is still masked.
#[tokio::test]
async fn a_refusal_reason_is_not_garbled_by_a_secret_value() {
    let fixture = fixture().await;
    let error = apply_structured(
        &fixture,
        |config| {
            config.api_key = Some("temperature".into());
            config.default_temperature = 1.9;
        },
        None,
    )
    .await
    .expect_err("test: the participant refuses");
    let ConfigTransactionError::NotApplied { reason, record_id, .. } = error else {
        panic!("expected NotApplied, got {error}");
    };
    assert!(reason.contains("refuses a temperature above 1.5"), "{reason}");
    let stored = record_of(&fixture, &record_id.expect("test: recorded"));
    assert!(
        stored
            .reason
            .unwrap_or_default()
            .contains("refuses a temperature above 1.5"),
        "the history reason was garbled"
    );
    assert!(
        !redacted_reason("provider said: invalid key sk-proj-abcdefghijklmnopqrstuvwxyz0123")
            .contains("abcdefghijklmnop"),
        "a secret-shaped token must stay masked"
    );
}

/// RED before PMMH-g (D-104 C-4): when the CLI's own validation
/// reload failed after a running PRX had already settled its change,
/// the CLI compensated anyway and overwrote the settled record. A
/// settled record is left as it is now, and so are the files.
#[tokio::test]
async fn the_cli_never_overwrites_a_settled_record() {
    let fixture = fixture().await;
    let cli_shared = crate::config::new_shared(fixture.desired());
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(SettlesThenRefuses {
        config_path: fixture.config_path(),
    });
    cli_shared.register_participant(&participant);
    let cli = ConfigTransactions::new(cli_shared).deferring_activation();
    let mut config = fixture.desired();
    config.default_temperature = 0.4;
    let plan = cli
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");

    let error = cli
        .apply(plan, ConfigActor::Cli, ApplyOptions::default())
        .await
        .expect_err("test: the CLI's validation is refused");
    let ConfigTransactionError::NotApplied { record_id, reason, .. } = error else {
        panic!("expected NotApplied, got {error}");
    };
    let record = record_of(&fixture, &record_id.expect("test: recorded"));

    assert_eq!(
        record.status,
        RecordStatus::Activated,
        "the settled record was overwritten"
    );
    assert!(reason.contains("already settled"), "{reason}");
    let on_disk = Config::load_from_path(&fixture.config_path(), fixture.desired().workspace_dir).expect("test: load");
    assert!(
        (on_disk.default_temperature - 0.4).abs() < f64::EPSILON,
        "the settled change was undone"
    );
}

/// The other process starts settlement after the CLI read its deferred
/// record. It must wait for the CLI's compensation and then see the
/// final status, rather than marking a restored change as activated.
#[tokio::test]
async fn a_settlement_after_the_cli_read_cannot_race_compensation() {
    use fs2::FileExt as _;

    let fixture = fixture().await;
    let cli_shared = crate::config::new_shared(fixture.desired());
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesCliActivation);
    cli_shared.register_participant(&participant);
    let mut cli = ConfigTransactions::new(cli_shared).deferring_activation();
    let history = cli.history().expect("test: history");
    let lock_path = fixture.dir.path().join(".config-history.settlement.lock");
    let contender = Arc::new(parking_lot::Mutex::new(None));
    let contender_for_hook = Arc::clone(&contender);
    cli.after_deferred_read = Some(Arc::new(move || {
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("test: second lock handle");
        assert!(
            second.try_lock_exclusive().is_err(),
            "the CLI holds the cross-process lock after its status read"
        );
        let record = history
            .list()
            .expect("test: history")
            .into_iter()
            .find(|record| record.status == RecordStatus::ActivationDeferred)
            .expect("test: deferred record");
        let record_id = record.id;
        let history = history.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            ready.send(()).expect("test: contender ready");
            let _lock = history.lock_settlement().expect("test: contender lock");
            let mut stored = history.get(&record_id).expect("test: settled record");
            if stored.status == RecordStatus::ActivationDeferred {
                stored.status = RecordStatus::Activated;
                history.update(&mut stored).expect("test: settle");
            }
            stored.status
        });
        started.recv().expect("test: contender started");
        *contender_for_hook.lock() = Some(handle);
    }));
    let mut config = fixture.desired();
    config.default_temperature = 0.4;
    let plan = cli
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");
    let error = cli
        .apply(plan, ConfigActor::Cli, ApplyOptions::default())
        .await
        .expect_err("test: CLI activation refused");
    let ConfigTransactionError::NotApplied { record_id, .. } = error else {
        panic!("expected NotApplied, got {error}");
    };
    let observed = contender
        .lock()
        .take()
        .expect("test: contender")
        .join()
        .expect("test: contender joined");
    assert_eq!(observed, RecordStatus::RolledBack);
    assert_eq!(
        record_of(&fixture, &record_id.expect("test: record id")).status,
        RecordStatus::RolledBack
    );
    let on_disk =
        Config::load_from_path(&fixture.config_path(), fixture.desired().workspace_dir).expect("test: restored config");
    assert_ne!(on_disk.default_temperature, 0.4);
}

/// RED before PMMH-g (D-104 C-5): a credential record that never reached
/// `credential_replaced` (`pending` after a crash, `commit_failed`) was
/// still offered for rollback. Restorability follows what the record
/// changed, not only its status.
#[tokio::test]
async fn an_unfinished_credential_record_is_not_rolled_back() {
    let fixture = fixture().await;
    let history = fixture.service().history().expect("test: history");
    let before = crate::config::files::current_before_image(&fixture.config_path()).expect("test: image");
    for status in [RecordStatus::Pending, RecordStatus::CommitFailed] {
        let mut record = history
            .create_pending(
                NewRecord {
                    actor: ConfigActor::Cli,
                    base_revision: None,
                    restores: None,
                    summary: ConfigChangeSummary::credential_reference_replaced("cred-x"),
                },
                &before,
            )
            .expect("test: record");
        record.status = status;
        history.update(&mut record).expect("test: update");
        assert!(!record.is_restorable());
        let error = fixture
            .service()
            .plan_rollback(&record.id)
            .await
            .expect_err("test: not restorable");
        assert!(
            error.to_string().contains("prx providers credential set"),
            "{status:?}: {error}"
        );
    }
}

/// D-104 C-1 ruling: a configuration with two embedding routes sharing
/// a hint keeps loading and accepts unrelated changes, but a change to
/// the embedding routes that creates or keeps a duplicate is refused,
/// with nothing written.
#[tokio::test]
async fn a_change_to_embedding_routes_may_not_keep_a_duplicate_hint() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let route = |model: &str| crate::config::EmbeddingRouteConfig {
        hint: "semantic".into(),
        provider: "openai".into(),
        model: model.into(),
        dimensions: None,
        api_key: None,
        credential_ref: None,
    };
    let mut config = fixture::config_in(dir.path());
    config.embedding_routes = vec![route("text-embedding-3-small"), route("text-embedding-3-large")];
    crate::config::files::write_split_config(&config, false)
        .await
        .expect("test: write a stored duplicate");
    let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone())
        .expect("test: an existing duplicate still loads");
    let service = ConfigTransactions::new(crate::config::new_shared(loaded.clone()));

    let mut unrelated = loaded.clone();
    unrelated.default_temperature = 0.3;
    service
        .plan(ConfigChange::Structured(Box::new(unrelated)))
        .await
        .expect("test: an unrelated change is planned");

    let mut kept = loaded.clone();
    if let Some(route) = kept.embedding_routes.last_mut() {
        route.model = "text-embedding-ada-002".into();
    }
    let error = service
        .plan(ConfigChange::Structured(Box::new(kept)))
        .await
        .expect_err("test: a change that keeps the duplicate is refused");
    assert!(error.to_string().contains("both use hint 'semantic'"), "{error}");

    let mut fixed = loaded;
    fixed.embedding_routes.truncate(1);
    service
        .plan(ConfigChange::Structured(Box::new(fixed)))
        .await
        .expect("test: removing the duplicate is planned");
}

/// D-101 F-2: a replaced credential's record is not restorable; a
/// rollback of it is refused with the way to set the credential again.
#[tokio::test]
async fn a_replaced_credential_record_is_not_rolled_back() {
    let fixture = fixture().await;
    let history = fixture.service().history().expect("test: history");
    let before = crate::config::files::current_before_image(&fixture.config_path()).expect("test: image");
    let mut record = history
        .create_pending(
            NewRecord {
                actor: ConfigActor::Cli,
                base_revision: None,
                restores: None,
                summary: ConfigChangeSummary::credential_reference_replaced("cred-x"),
            },
            &before,
        )
        .expect("test: record");
    record.status = RecordStatus::CredentialReplaced;
    history.update(&mut record).expect("test: update");
    let tree = fixture.tree();

    let error = fixture
        .service()
        .plan_rollback(&record.id)
        .await
        .expect_err("test: not restorable");
    assert!(error.to_string().contains("prx providers credential set"), "{error}");
    assert_eq!(fixture.tree(), tree);
}

/// RED before D-3 (the raw PUT and POST paths saved, then activated,
/// and left a refused candidate on disk): a refused activation restores
/// the exact before-image, keeps the active generation, re-aligns the
/// desired configuration, and records `rolled_back` without a secret.
#[tokio::test]
async fn a_refused_activation_restores_the_before_image() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let generation = fixture.shared.active_generation_id();
    let revision = ConfigTransactions::base_revision(&fixture.shared.desired());

    let error = apply_structured(&fixture, |config| config.default_temperature = 1.9, None)
        .await
        .expect_err("test: the participant refuses");

    let ConfigTransactionError::NotApplied {
        record_id,
        reason,
        outcome,
    } = &error
    else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(*outcome, NotAppliedOutcome::Restored, "{error}");
    assert!(reason.contains("refuses a temperature"), "{reason}");
    assert!(
        error.to_string().contains("previous configuration was restored"),
        "{error}"
    );
    assert_eq!(fixture.tree(), before, "disk must hold the before-image again");
    assert_eq!(fixture.shared.active_generation_id(), generation);
    assert!(fixture.shared.desired().config.default_temperature < 1.5);
    assert_eq!(ConfigTransactions::base_revision(&fixture.shared.desired()), revision);

    let history = fixture.service().history().expect("test: history");
    let record = history
        .get(record_id.as_deref().expect("test: recorded"))
        .expect("test: record");
    assert_eq!(record.status, RecordStatus::RolledBack);
    let json = serde_json::to_string(&record).expect("test: json");
    assert!(!json.contains(fixture::SENTINEL_PREFIX), "{json}");
}

#[tokio::test]
async fn a_stale_base_revision_is_a_conflict_with_nothing_written() {
    let fixture = fixture().await;
    let base = ConfigTransactions::base_revision(&fixture.shared.desired()).expect("test: disk revision");
    // Another writer lands first.
    let manual = fixture.dir.path().join("config.d/memory.toml");
    let mut edited = std::fs::read_to_string(&manual).expect("test: read");
    edited.push_str("\n# another writer\n");
    std::fs::write(&manual, edited).expect("test: write");
    let on_disk = fixture.tree();
    let history_dir = fixture.dir.path().join(crate::config::manage::history::HISTORY_DIR);

    let error = apply_structured(&fixture, |config| config.default_temperature = 0.2, Some(&base))
        .await
        .expect_err("test: conflict");

    assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
    assert_eq!(fixture.tree(), on_disk);
    assert!(!history_dir.exists(), "a refused commit records nothing");
}

#[tokio::test]
async fn an_injected_commit_failure_leaves_the_before_image_and_records_it() {
    let fixture = fixture().await;
    let before = fixture.tree();
    *crate::config::files::mutation_failure_hook()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(crate::config::files::MutationFailureHook {
        config_path: fixture.config_path(),
        writes_remaining: 1,
    });

    let error = apply_structured(&fixture, |config| config.default_temperature = 0.3, None)
        .await
        .expect_err("test: injected failure");

    assert!(matches!(error, ConfigTransactionError::Failed(_)), "{error}");
    assert_eq!(fixture.tree(), before, "the journal restores a partial write");
    let records = fixture
        .service()
        .history()
        .expect("test: history")
        .list()
        .expect("test: list");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records.first().map(|record| record.status),
        Some(RecordStatus::CommitFailed)
    );
}

#[tokio::test]
async fn concurrent_transactions_based_on_one_revision_never_lose_an_update() {
    let fixture = fixture().await;
    let base = ConfigTransactions::base_revision(&fixture.shared.desired()).expect("test: revision");

    let first = apply_structured(&fixture, |config| config.default_temperature = 0.6, Some(&base));
    let second = apply_structured(
        &fixture,
        |config| {
            config.default_model = Some("second-writer".into());
            let provider = crate::providers::selection::default_provider(config).to_owned();
            declare_chat(config, &provider, "second-writer");
        },
        Some(&base),
    );
    let (first, second) = tokio::join!(first, second);

    let conflicts = [&first, &second]
        .iter()
        .filter(|result| matches!(result, Err(ConfigTransactionError::Conflict(_))))
        .count();
    assert_eq!(conflicts, 1, "exactly one writer wins: {first:?} / {second:?}");
}

#[tokio::test]
async fn raw_replacement_protects_secrets_outside_the_effective_config() {
    for filename in ["tools.toml", "zz-local.toml"] {
        let fixture = fixture().await;
        let target = fixture.dir.path().join("config.d").join(filename);
        let original = std::fs::read_to_string(&target).unwrap_or_default();
        let stored = format!("{original}\n[future_section]\nclient_secret = 'prx-hidden-fixture'\n");
        std::fs::write(&target, &stored).unwrap();
        let before = fixture.tree();
        let error = fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: filename.into(),
                contents: original.clone(),
                allow_secret_clear: false,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("future_section.client_secret"), "{error}");
        assert!(error.to_string().contains("PUT /api/config/secrets"), "{error}");
        assert!(!error.to_string().contains("prx-hidden-fixture"), "{error}");
        assert_eq!(fixture.tree(), before);
        fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: filename.into(),
                contents: original,
                allow_secret_clear: true,
            })
            .await
            .unwrap();
        assert_eq!(fixture.tree(), before, "planning never writes");
    }
}

#[tokio::test]
async fn raw_replacement_requires_permission_when_the_stored_file_cannot_be_inspected() {
    for (filename, broken) in [
        ("tools.toml", b"api_key = 'prx-stored-fixture'\n[broken".as_slice()),
        ("zz-local.toml", b"api_key = 'prx-stored-fixture'\n[broken".as_slice()),
        ("zz-local.toml", &[0xff]),
    ] {
        let fixture = fixture().await;
        let target = fixture.dir.path().join("config.d").join(filename);
        let original = std::fs::read_to_string(&target).unwrap_or_default();
        std::fs::write(&target, broken).unwrap();
        let before = fixture.tree();
        let error = fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: filename.into(),
                contents: original.clone(),
                allow_secret_clear: false,
            })
            .await
            .unwrap_err()
            .to_string();
        for expected in ["Cannot inspect stored secrets", filename, "--allow-credential-clear"] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(
            error.contains(&format!("PUT /api/config/files/{filename}?allow_secret_clear=true")),
            "{error}"
        );
        assert!(!error.contains("prx-stored-fixture"), "{error}");
        assert_eq!(fixture.tree(), before);
        let plan = fixture
            .service()
            .plan(ConfigChange::RawFile {
                filename: filename.into(),
                contents: original,
                allow_secret_clear: true,
            })
            .await
            .unwrap();
        if filename == "tools.toml" {
            assert!(plan.summary().risk_reasons.contains(&RiskReason::BaselineUnavailable));
        }
        assert_eq!(fixture.tree(), before, "planning a repair never writes");
    }
}

#[tokio::test]
async fn raw_posted_parse_error_precedes_secret_omission_and_uninspectable_storage() {
    for stored_is_broken in [false, true] {
        let fixture = fixture().await;
        if stored_is_broken {
            std::fs::write(fixture.dir.path().join("config.d/tools.toml"), "[broken").unwrap();
        }
        let before = fixture.tree();
        for allow_secret_clear in [false, true] {
            let error = fixture
                .service()
                .plan(ConfigChange::RawFile {
                    filename: "tools.toml".into(),
                    contents: "api_key = 'prx-posted-fixture\n".into(),
                    allow_secret_clear,
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Invalid TOML"), "{error}");
            assert!(error.contains("line 1"), "{error}");
            assert!(!error.contains("would drop"), "{error}");
            assert!(!error.contains("Cannot inspect"), "{error}");
            assert!(!error.contains("prx-posted-fixture"), "{error}");
            assert_eq!(fixture.tree(), before);
        }
    }
}

#[tokio::test]
async fn an_invalid_raw_file_is_refused_before_anything_is_written() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let service = fixture.service();

    let error = service
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: "memory.toml".into(),
            contents: "[memory]\nbackend = 7\n".into(),
        })
        .await
        .expect_err("test: invalid");

    assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
    assert_eq!(fixture.tree(), before);
}

/// A raw write of a well-formed but undecryptable ciphertext is refused
/// at plan time: the candidate is loaded with the live key first.
#[tokio::test]
async fn an_undecryptable_credential_is_refused_at_plan_time() {
    let fixture = fixture().await;
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let bogus = format!("api_key = \"enc2:{}\"\n{}", "00".repeat(40), strip_root_api_key(&main));

    let error = fixture
        .service()
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: "config.toml".into(),
            contents: bogus,
        })
        .await
        .expect_err("test: undecryptable");

    assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
}

fn strip_root_api_key(main: &str) -> String {
    main.lines()
        .filter(|line| !line.starts_with("api_key"))
        .map(|line| format!("{line}\n"))
        .collect()
}

// ── Rollback (D-4) ────────────────────────────────────────────────────

#[tokio::test]
async fn an_ordinary_plan_cannot_select_a_retired_model_by_naming_the_rollback_actor() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let service = fixture.service();
    let mut candidate = fixture.desired();
    candidate.default_provider = Some("openai".into());
    candidate.default_model = Some("gpt-4o".into());
    let plan = service
        .plan(ConfigChange::Structured(Box::new(candidate)))
        .await
        .expect("test: ordinary structured plan");
    assert!(plan.restores.is_none());
    let error = service
        .apply(plan, ConfigActor::Rollback, ApplyOptions::default())
        .await
        .expect_err("test: actor does not grant a lifecycle exception");
    assert!(error.to_string().contains("retired"), "{error}");
    assert_eq!(fixture.tree(), before);
}

/// Apply A, apply B, roll back B's record: the stored files are byte
/// for byte those after A (ciphertext included, so no mask and no
/// re-encryption), and the rollback has its own record.
#[tokio::test]
async fn rollback_restores_the_recorded_stored_bytes() {
    let fixture = fixture().await;
    apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
        .await
        .expect("test: apply A");
    let after_a = fixture.tree();
    let b = apply_structured(
        &fixture,
        |config| {
            config.default_temperature = 0.9;
            config.api_key = Some("sentinel-rotated-root".into());
        },
        None,
    )
    .await
    .expect("test: apply B");
    assert_eq!(
        fixture.shared.desired().config.api_key.as_deref(),
        Some("sentinel-rotated-root")
    );

    let b_id = b.record_id.expect("test: B recorded");
    let outcome = fixture
        .service()
        .rollback(RollbackRequest {
            record_id: &b_id,
            expected_revision: &current_revision(&fixture),
            confirm: true,
            restore_credentials: true,
            disable_encryption: false,
        })
        .await
        .expect("test: rollback");

    assert_eq!(fixture.tree(), after_a, "rollback must restore the exact stored bytes");
    assert_eq!(
        fixture.shared.desired().config.api_key.as_deref(),
        Some("sentinel-root")
    );
    assert!((fixture.shared.load_full().default_temperature - 0.5).abs() < f64::EPSILON);
    let record = fixture
        .service()
        .history()
        .expect("test: history")
        .get(&outcome.record_id.expect("test: rollback recorded"))
        .expect("test: record");
    assert_eq!(record.actor, ConfigActor::Rollback);
    assert_eq!(record.restores.as_deref(), Some(b_id.as_str()));
    assert_eq!(record.status, RecordStatus::Activated);
}

/// A before-image whose credentials the current key cannot decrypt (the
/// key was replaced) is refused with nothing written.
#[tokio::test]
async fn rollback_to_an_undecryptable_record_is_refused() {
    let fixture = fixture().await;
    let a = apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
        .await
        .expect("test: apply A");
    let key_path = fixture.dir.path().join(".secret_key");
    std::fs::write(&key_path, "11".repeat(32)).expect("test: replace key");
    let on_disk = fixture.tree();

    let error = fixture
        .service()
        .rollback(confirmed(
            &a.record_id.expect("test: recorded"),
            &current_revision(&fixture),
        ))
        .await
        .expect_err("test: refused");

    assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{error}");
    assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
    assert_eq!(fixture.tree(), on_disk);
}

#[tokio::test]
async fn rollback_of_an_unknown_record_is_refused() {
    let fixture = fixture().await;
    for id in ["../../etc", "not-a-record", "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"] {
        let error = fixture
            .service()
            .rollback(confirmed(id, &current_revision(&fixture)))
            .await
            .expect_err("test: refused");
        assert!(matches!(error, ConfigTransactionError::Invalid(_)), "{id}: {error}");
    }
}

fn current_revision(fixture: &Fixture) -> String {
    crate::config::files::current_config_revision(&fixture.config_path()).expect("test: revision")
}

fn confirmed<'a>(record_id: &'a str, expected_revision: &'a str) -> RollbackRequest<'a> {
    RollbackRequest {
        record_id,
        expected_revision,
        confirm: true,
        restore_credentials: false,
        disable_encryption: false,
    }
}

// ── PMMD-f (R-PMMD) ──────────────────────────────────────────────────

/// RED before PMMD-f (R-PMMD probe 1, F-1): a raw write planned without
/// an expected revision reverted another writer's edit to a different
/// fragment that landed between plan and commit. After: the plan's own
/// revision guards the commit; 409 and the edit survives.
#[tokio::test]
async fn a_raw_write_never_reverts_an_edit_made_after_its_plan() {
    let fixture = fixture().await;
    let service = fixture.service();
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let plan = service
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: "config.toml".into(),
            contents: main.replacen("default_temperature = 0.7", "default_temperature = 0.6", 1),
        })
        .await
        .expect("test: plan");
    assert!(plan.planned_revision().is_some());
    let memory = fixture.dir.path().join("config.d/memory.toml");
    let mut edited = std::fs::read_to_string(&memory).expect("test: read");
    edited.push_str("\n# FOREIGN-WRITER-EDIT\n");
    std::fs::write(&memory, &edited).expect("test: foreign edit");

    let result = service
        .apply(plan, ConfigActor::GatewayConfigFile, ApplyOptions::default())
        .await;

    assert!(matches!(result, Err(ConfigTransactionError::Conflict(_))), "{result:?}");
    assert_eq!(std::fs::read_to_string(&memory).expect("test: read"), edited);
}

/// RED before PMMD-f (R-PMMD probe 2, F-2): one rollback call turned
/// encryption off and wrote the root key in plaintext. After: it needs
/// `confirm`, and then `disable_encryption`; only with both is it done.
#[tokio::test]
async fn a_rollback_that_turns_encryption_off_needs_every_confirmation() {
    let fixture = fixture().await;
    apply_structured(&fixture, |config| config.secrets.encrypt = false, None)
        .await
        .expect("test: apply A");
    let b = apply_structured(&fixture, |config| config.secrets.encrypt = true, None)
        .await
        .expect("test: apply B");
    let b_id = b.record_id.expect("test: recorded");
    let main = || std::fs::read_to_string(fixture.config_path()).expect("test: read");
    assert!(!main().contains("\"sentinel-root\""), "test precondition: encrypted");
    let on_disk = fixture.tree();

    let mut request = confirmed(&b_id, "");
    let revision = current_revision(&fixture);
    request.expected_revision = &revision;
    request.confirm = false;
    for (confirm, disable, required) in [
        (false, false, "disable_encryption"),
        (true, false, "disable_encryption"),
        (false, true, "confirm"),
    ] {
        request.confirm = confirm;
        request.disable_encryption = disable;
        let error = fixture.service().rollback(request).await.expect_err("test: refused");
        assert!(
            matches!(&error, ConfigTransactionError::ConfirmationRequired { required: flag, .. } if *flag == required),
            "{error}"
        );
        assert_eq!(fixture.tree(), on_disk, "nothing may be written");
        assert!(fixture.shared.desired().config.secrets.encrypt);
    }

    request.confirm = true;
    request.disable_encryption = true;
    fixture
        .service()
        .rollback(request)
        .await
        .expect("test: confirmed rollback");
    assert!(!fixture.shared.desired().config.secrets.encrypt);
}

/// A rollback that would bring back a credential the current
/// configuration no longer holds needs `restore_credentials`.
#[tokio::test]
async fn a_rollback_that_restores_a_rotated_credential_needs_restore_credentials() {
    let fixture = fixture().await;
    let rotated = apply_structured(
        &fixture,
        |config| config.api_key = Some("sentinel-rotated-root".into()),
        None,
    )
    .await
    .expect("test: rotate");
    let on_disk = fixture.tree();
    let revision = current_revision(&fixture);

    let error = fixture
        .service()
        .rollback(confirmed(&rotated.record_id.expect("test: recorded"), &revision))
        .await
        .expect_err("test: refused");

    let ConfigTransactionError::ConfirmationRequired { required, reasons, .. } = &error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(*required, "restore_credentials");
    assert!(
        reasons.contains(&RiskReason::CredentialReintroduced { path: "api_key".into() }),
        "{reasons:?}"
    );
    assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
    assert_eq!(fixture.tree(), on_disk);
}

#[tokio::test]
async fn a_rollback_needs_the_current_revision() {
    let fixture = fixture().await;
    let a = apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
        .await
        .expect("test: apply A");
    let error = fixture
        .service()
        .rollback(confirmed(&a.record_id.expect("test: recorded"), "00"))
        .await
        .expect_err("test: stale");
    assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
}

/// A high-risk plan is refused without confirmation, with nothing
/// written and the reasons named by path only.
#[tokio::test]
async fn a_high_risk_plan_needs_confirmation() {
    let fixture = fixture().await;
    let service = fixture.service();
    let on_disk = fixture.tree();
    let mut config = fixture.desired();
    config.api_key = Some("sentinel-new-root".into());
    let plan = service
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");
    assert_eq!(plan.summary().risk, ConfigChangeRisk::High);

    let error = service
        .apply(plan, ConfigActor::GatewayConfigApi, ApplyOptions::default())
        .await
        .expect_err("test: unconfirmed");

    assert!(
        matches!(
            &error,
            ConfigTransactionError::ConfirmationRequired {
                required: "confirm_high_risk",
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("api_key"), "{error}");
    assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
    assert_eq!(fixture.tree(), on_disk);
}

/// The shared plan step refuses an introduced mask for every surface.
#[tokio::test]
async fn an_introduced_mask_is_refused_at_plan_time() {
    let fixture = fixture().await;
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let masked = format!("api_key = \"***\"\n{}", strip_root_api_key(&main));
    let error = fixture
        .service()
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: "config.toml".into(),
            contents: masked,
        })
        .await
        .expect_err("test: masked");
    assert!(error.to_string().contains("redaction mask"), "{error}");
}

/// RED before PMMD-f (F-4): the restore was written but re-activating
/// it failed, and the message claimed the rejected configuration was
/// still on disk.
#[tokio::test]
async fn a_written_restore_is_not_reported_as_still_on_disk() {
    let fixture = fixture().await;
    let participant: Arc<dyn ConfigGenerationParticipant> = Arc::new(RefusesAndReplacesKey {
        key_path: fixture.dir.path().join(".secret_key"),
    });
    fixture.shared.register_participant(&participant);
    let before = fixture.tree();

    let error = apply_structured(&fixture, |config| config.default_temperature = 1.3, None)
        .await
        .expect_err("test: refused");

    assert_eq!(fixture.tree(), before, "the restore was written");
    assert!(
        matches!(
            &error,
            ConfigTransactionError::NotApplied {
                outcome: NotAppliedOutcome::RestoredNotReactivated,
                ..
            }
        ),
        "{error}"
    );
    assert!(!error.to_string().contains("still on disk"), "{error}");
    assert!(error.to_string().contains("restored on disk"), "{error}");
}

/// Refuses a warm temperature and replaces the secret key, so the
/// restored (encrypted) tree cannot be re-activated.
struct RefusesAndReplacesKey {
    key_path: PathBuf,
}

impl ConfigGenerationParticipant for RefusesAndReplacesKey {
    fn name(&self) -> &'static str {
        "test_refuses_and_replaces_key"
    }

    fn prepares_for_field(&self, field: &str) -> bool {
        field == "default_temperature"
    }

    fn prepare(
        &self,
        generation: Arc<ConfigGeneration>,
        _changed_fields: &[String],
    ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
        if generation.effective.default_temperature > 1.2 {
            std::fs::write(&self.key_path, "22".repeat(32))?;
            anyhow::bail!("test participant refuses and replaces the key");
        }
        Ok(Box::new(Accepted))
    }
}

/// When the files already hold the refused configuration (an unreloaded
/// manual edit), nothing is written and the message says so.
#[tokio::test]
async fn activation_of_unchanged_files_is_reported_as_nothing_written() {
    let fixture = fixture().await;
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let hot = main.replacen("default_temperature = 0.7", "default_temperature = 1.9", 1);
    assert_ne!(hot, main, "test precondition: the edit applies");
    std::fs::write(fixture.config_path(), &hot).expect("test: manual edit");
    let service = fixture.service();
    let plan = service
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: "config.toml".into(),
            contents: hot,
        })
        .await
        .expect("test: plan");

    let error = service
        .apply(plan, ConfigActor::GatewayConfigFile, ApplyOptions::default())
        .await
        .expect_err("test: refused");

    assert!(
        matches!(
            &error,
            ConfigTransactionError::NotApplied {
                outcome: NotAppliedOutcome::NothingWritten,
                ..
            }
        ),
        "{error}"
    );
    assert!(!error.to_string().contains("could not be restored"), "{error}");
}

// ── PMMD-g (R-PMMDf) ─────────────────────────────────────────────────

async fn apply_raw(
    fixture: &Fixture,
    filename: &str,
    contents: String,
    confirm_high_risk: bool,
) -> Result<ConfigTransactionOutcome, ConfigTransactionError> {
    let service = fixture.service();
    let plan = service
        .plan(ConfigChange::RawFile {
            allow_secret_clear: false,
            filename: filename.into(),
            contents,
        })
        .await?;
    let planned = plan.planned_revision().map(str::to_string);
    service
        .apply(
            plan,
            ConfigActor::GatewayConfigFile,
            ApplyOptions {
                expected_revision: None,
                confirm_high_risk,
                confirmed_revision: planned.as_deref(),
            },
        )
        .await
}

/// RED before PMMD-g (N-1): plaintext written to a user-owned fragment
/// the loader skips was accepted (risk low) and stored with encryption on.
#[tokio::test]
async fn plaintext_in_an_unloaded_fragment_is_refused() {
    let fixture = fixture().await;
    let target = fixture.dir.path().join("config.d/zz-local.toml");
    for value in ["sentinel-probe-plain", "***"] {
        let result = apply_raw(&fixture, "zz-local.toml", format!("api_key = \"{value}\"\n"), true).await;
        assert!(
            matches!(result, Err(ConfigTransactionError::Invalid(_))),
            "{value}: {result:?}"
        );
        assert!(!target.exists(), "{value}: nothing may be written");
    }
}

/// RED before PMMD-g (N-1): a `config.toml` section a fragment overrides
/// took a plaintext key (risk low) and stored it with encryption on.
#[tokio::test]
async fn plaintext_in_a_shadowed_section_is_refused() {
    let fixture = fixture().await;
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let shadowed = format!(
        "{main}\n[[model_routes]]\nhint = \"shadow\"\nprovider = \"openrouter\"\nmodel = \"m\"\napi_key = \"sentinel-probe-shadow\"\n"
    );

    let result = apply_raw(&fixture, "config.toml", shadowed, true).await;

    assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
    assert!(
        !std::fs::read_to_string(fixture.config_path())
            .expect("test: read")
            .contains("sentinel-probe-shadow")
    );
}

const USERINFO_PW: &str = "userinfo-sentinel-pw";

fn userinfo_provider() -> String {
    format!("custom:http://alice:{USERINFO_PW}@127.0.0.1:1/v1")
}

/// Declares chat and native tools for `model` on `provider`, so the
/// capability check admits the selection the test writes.
fn declare_chat(config: &mut crate::config::Config, provider: &str, model: &str) {
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = \"{provider}\"\nmodel = \"{model}\"\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .expect("test: a valid [[providers.models]] entry"),
    );
}

/// RED before PMMD-g (N-2): a raw write that puts C2BU's `[redacted]@`
/// placeholder where real URL userinfo is stored was accepted with the
/// flag, and the real userinfo was gone.
#[tokio::test]
async fn a_redacted_userinfo_placeholder_never_replaces_real_userinfo() {
    let fixture = fixture().await;
    apply_structured(
        &fixture,
        |config| {
            config.default_provider = Some(userinfo_provider());
            declare_chat(config, &userinfo_provider(), "anthropic/claude-sonnet-5");
        },
        None,
    )
    .await
    .expect("test: store userinfo");
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    assert!(main.contains(USERINFO_PW), "test precondition: userinfo on disk");
    let placeholder = main.replace(&format!("alice:{USERINFO_PW}@"), "[redacted]@");

    let result = apply_raw(&fixture, "config.toml", placeholder, true).await;

    assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
    assert!(
        std::fs::read_to_string(fixture.config_path())
            .expect("test: read")
            .contains(USERINFO_PW)
    );
}

/// RED before PMM-H (D-099 7c): a secret the redactor found in a URL
/// path was shown as `[REDACTED]`, which the write boundary did not know
/// as a mask, so sending a shown provider back stored the literal mask.
/// Both path masks are refused now, with nothing written.
#[tokio::test]
async fn a_url_path_mask_is_never_stored() {
    let fixture = fixture().await;
    let before = fixture.tree();
    for provider in [
        "custom:https://llm.example/v1/[REDACTED]/chat",
        "custom:https://llm.example/v1/[redacted]/chat",
    ] {
        let result = apply_structured(
            &fixture,
            |config| config.default_provider = Some(provider.to_string()),
            None,
        )
        .await;
        assert!(
            matches!(result, Err(ConfigTransactionError::Invalid(_))),
            "{provider}: {result:?}"
        );
        assert_eq!(fixture.tree(), before, "{provider} was written");
    }
}

fn price_key() -> String {
    format!("{}/model-a", userinfo_provider())
}

/// RED before PMMD-g (N-2, N-3): a credentialed `cost.prices` key needed
/// no confirmation, and its userinfo appeared in the plan paths.
#[tokio::test]
async fn a_credentialed_price_key_is_high_risk_and_redacted_in_the_plan() {
    let fixture = fixture().await;
    let mut config = fixture.desired();
    config.cost.prices.insert(
        price_key(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    let plan = fixture
        .service()
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");

    let rendered = serde_json::to_string(plan.summary()).expect("test: json");
    assert!(!rendered.contains(USERINFO_PW), "the plan leaked userinfo: {rendered}");
    assert_eq!(plan.summary().risk, ConfigChangeRisk::High, "{rendered}");
    let error = fixture
        .service()
        .apply(plan, ConfigActor::GatewayConfigApi, ApplyOptions::default())
        .await
        .expect_err("test: needs confirmation");
    assert!(
        matches!(error, ConfigTransactionError::ConfirmationRequired { .. }),
        "{error}"
    );
    assert!(!error.to_string().contains(USERINFO_PW), "{error}");
}

/// RED before PMMD-g (N-7): a `custom:` endpoint in these provider
/// fields classified low.
#[test]
fn custom_endpoints_in_every_provider_field_are_high_risk() {
    let before = Config::default();
    for change in [
        (|config: &mut Config| {
            config.reliability.fallback_providers = vec!["custom:https://attacker.example/v1".into()]
        }) as fn(&mut Config),
        |config: &mut Config| config.memory.embedding_provider = "custom:https://attacker.example/v1".into(),
        |config: &mut Config| config.smart_group.classifier_provider = "custom:https://attacker.example/v1".into(),
    ] {
        let mut after = before.clone();
        change(&mut after);
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        assert_eq!(summary.risk, ConfigChangeRisk::High, "{summary:?}");
        assert!(
            summary
                .risk_reasons
                .iter()
                .any(|reason| matches!(reason, RiskReason::EndpointChanged { .. })),
            "{summary:?}"
        );
    }
}

// ── PMMD-h (R-PMMDg) ─────────────────────────────────────────────────

/// RED before PMMD-h (G-1, R-PMMDg probe): on a case-insensitive
/// filesystem a raw write of `Security.toml` was planned as a user-owned
/// fragment (candidate unchanged, risk low) but overwrote the managed
/// `security.toml`, turning encryption off with no confirmation.
#[tokio::test]
async fn a_case_variant_of_a_managed_file_name_is_refused() {
    let fixture = fixture().await;
    let security = fixture.dir.path().join("config.d/security.toml");
    let stored = std::fs::read_to_string(&security).expect("test: read security.toml");
    assert!(stored.contains("encrypt = true"), "test precondition: {stored}");
    let off = stored.replacen("encrypt = true", "encrypt = false", 1);

    for name in ["Security.toml", "SECURITY.TOML", "Config.toml"] {
        let result = apply_raw(&fixture, name, off.clone(), false).await;
        assert!(
            matches!(result, Err(ConfigTransactionError::Invalid(_))),
            "{name}: {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&security).expect("test: read"),
            stored,
            "{name}: the managed file must not change"
        );
    }
    assert!(fixture.shared.desired().config.secrets.encrypt);
}

// ── PMMD-i (R-PMMDh) ─────────────────────────────────────────────────

/// RED before PMMD-i (H-1, R-PMMDh probe): APFS folds Unicode case, so
/// `ſecurity.toml` (U+017F) and `networK.toml` (U+212A Kelvin) open the
/// managed file; the write was planned as a user-owned fragment (risk
/// low, no confirmation) and turned encryption off. Only plain lowercase
/// ASCII names are accepted now.
#[tokio::test]
async fn a_unicode_fold_variant_of_a_managed_file_name_is_refused() {
    let fixture = fixture().await;
    let security = fixture.dir.path().join("config.d/security.toml");
    let network = fixture.dir.path().join("config.d/network.toml");
    let stored_security = std::fs::read_to_string(&security).expect("test: read security.toml");
    let stored_network = std::fs::read_to_string(&network).expect("test: read network.toml");
    let off = stored_security.replacen("encrypt = true", "encrypt = false", 1);
    assert_ne!(off, stored_security, "test precondition");

    for (name, contents) in [
        ("\u{17f}ecurity.toml", off.clone()),
        ("networ\u{212a}.toml", stored_network.clone()),
        ("co\u{fb06}.toml", "[cost]\n".to_string()),
        ("caf\u{e9}.toml", "[cafe]\n".to_string()),
    ] {
        let result = apply_raw(&fixture, name, contents, false).await;
        assert!(
            matches!(result, Err(ConfigTransactionError::Invalid(_))),
            "{name}: {result:?}"
        );
    }
    assert_eq!(std::fs::read_to_string(&security).expect("test: read"), stored_security);
    assert_eq!(std::fs::read_to_string(&network).expect("test: read"), stored_network);
    assert!(fixture.shared.desired().config.secrets.encrypt);
}

/// D-049 B, RED before SECRETS-h: `confirm_high_risk` alone applied a
/// high-risk plan. After: the confirmation must name the revision the
/// plan is checked against; without one it is refused as `if_match`
/// with that revision, with another one it is a conflict, and with the
/// right one it applies. Nothing is written on a refusal.
#[tokio::test]
async fn a_high_risk_confirmation_is_bound_to_the_planned_revision() {
    let fixture = fixture().await;
    let on_disk = fixture.tree();
    let high_risk = || {
        let mut config = fixture.desired();
        config.api_key = Some("sentinel-rotated-root".into());
        ConfigChange::Structured(Box::new(config))
    };
    let revision = current_revision(&fixture);

    let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
    let error = fixture
        .service()
        .apply(
            plan,
            ConfigActor::GatewayConfigApi,
            ApplyOptions {
                expected_revision: Some(&revision),
                confirm_high_risk: true,
                confirmed_revision: None,
            },
        )
        .await
        .expect_err("test: an unbound confirmation is refused");
    assert!(
        matches!(
            &error,
            ConfigTransactionError::ConfirmationRequired { required: "if_match", revision: Some(reported), .. }
                if *reported == revision
        ),
        "{error}"
    );
    assert!(!error.to_string().contains("sentinel-rotated-root"), "{error}");
    assert_eq!(fixture.tree(), on_disk, "nothing may be written");

    let stale = "0".repeat(64);
    let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
    let error = fixture
        .service()
        .apply(
            plan,
            ConfigActor::GatewayConfigApi,
            ApplyOptions {
                expected_revision: Some(&revision),
                confirm_high_risk: true,
                confirmed_revision: Some(&stale),
            },
        )
        .await
        .expect_err("test: a confirmation of another revision is refused");
    assert!(matches!(error, ConfigTransactionError::Conflict(_)), "{error}");
    assert_eq!(fixture.tree(), on_disk, "nothing may be written");

    let plan = fixture.service().plan(high_risk()).await.expect("test: plan");
    fixture
        .service()
        .apply(
            plan,
            ConfigActor::GatewayConfigApi,
            ApplyOptions {
                expected_revision: Some(&revision),
                confirm_high_risk: true,
                confirmed_revision: Some(&revision),
            },
        )
        .await
        .expect("test: a bound confirmation applies");
}

/// D-050 item 3, RED before SECRETS-i: a stored configuration that does
/// not load was no baseline, and the plan compared the change with
/// nothing (every rule that looks at the prior value failed open). After:
/// the plan is high risk with `baseline_unavailable`.
#[tokio::test]
async fn a_plan_without_a_loadable_baseline_is_high_risk() {
    let fixture = fixture().await;
    // The stored secrets no longer decrypt: the files are valid TOML, but
    // the configuration does not load.
    let key = fixture.config_path().with_file_name(".secret_key");
    std::fs::remove_file(&key).expect("test: remove the secret key");
    let mut config = fixture.desired();
    config.default_temperature = 0.25;
    let plan = fixture
        .service()
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");
    assert_eq!(plan.summary().risk, ConfigChangeRisk::High, "{:?}", plan.summary());
    assert!(
        plan.summary().risk_reasons.contains(&RiskReason::BaselineUnavailable),
        "{:?}",
        plan.summary().risk_reasons
    );
}

// ── PMM-D-5: model validation for every affected selection ───────────

/// A loopback-shaped endpoint no test contacts: selections are resolved,
/// never sent.
const D5_ENDPOINT: &str = "custom:http://127.0.0.1:9/v1";

fn d5_declare(config: &mut Config, model: &str, operations: &str) {
    let parsed: crate::config::schema::ProviderModelConfig = toml::from_str(&format!(
        "provider = \"{D5_ENDPOINT}\"\nmodel = \"{model}\"\noperations = {operations}\n"
    ))
    .expect("test: a valid [[providers.models]] entry");
    config.providers.models.push(parsed);
}

fn d5_route(model: &str) -> crate::config::ModelRouteConfig {
    crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "d5".into(),
        provider: D5_ENDPOINT.into(),
        model: model.into(),
        api_key: None,
        credential_ref: None,
    }
}

/// RED before PMM-D-5: a new model route to a model the operator's own
/// declaration says cannot chat was committed (only the default was ever
/// checked, and only on POST). After: refused, nothing written.
#[tokio::test]
async fn a_route_to_a_model_that_cannot_chat_is_refused_before_anything_is_written() {
    let fixture = fixture().await;
    let before = fixture.tree();

    let result = apply_structured(
        &fixture,
        |config| {
            d5_declare(config, "embed-only", "[\"embedding\"]");
            config.model_routes.push(d5_route("embed-only"));
        },
        None,
    )
    .await;

    let Err(ConfigTransactionError::Invalid(error)) = &result else {
        panic!("a route to an embedding-only model must be refused: {result:?}");
    };
    assert!(format!("{error:#}").contains("hint:d5"), "{error:#}");
    assert_eq!(fixture.tree(), before, "nothing may be written");
}

/// RED before PMM-D-5: the same through the raw file API.
#[tokio::test]
async fn a_raw_write_of_a_route_that_cannot_chat_is_refused() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let main = std::fs::read_to_string(fixture.config_path()).expect("test: read");
    let routing = std::fs::read_to_string(fixture.dir.path().join("config.d/routing.toml")).unwrap_or_default();
    let contents =
        format!("{routing}\n[[model_routes]]\nhint = \"d5\"\nprovider = \"{D5_ENDPOINT}\"\nmodel = \"embed-only\"\n");
    let declared = format!(
        "{main}\n[[providers.models]]\nprovider = \"{D5_ENDPOINT}\"\nmodel = \"embed-only\"\noperations = [\"embedding\"]\n"
    );
    apply_raw(&fixture, "config.toml", declared, true)
        .await
        .expect("test: the declaration alone is accepted");
    let declared_tree = fixture.tree();

    let result = apply_raw(&fixture, "routing.toml", contents, true).await;

    assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
    assert_eq!(fixture.tree(), declared_tree, "nothing may be written");
    assert_ne!(fixture.tree(), before);
}

/// RED before PMM-D-5: an agent binding is a selection too.
#[tokio::test]
async fn an_agent_bound_to_a_model_that_cannot_chat_is_refused() {
    let fixture = fixture().await;
    let result = apply_structured(
        &fixture,
        |config| {
            d5_declare(config, "embed-only", "[\"embedding\"]");
            let mut agent = config
                .agents
                .values()
                .next()
                .cloned()
                .expect("test: the fixture has an agent");
            agent.provider = D5_ENDPOINT.into();
            agent.model = "embed-only".into();
            agent.api_key = None;
            config.agents.insert("d5".into(), agent);
        },
        None,
    )
    .await;
    let Err(ConfigTransactionError::Invalid(error)) = &result else {
        panic!("an agent bound to an embedding-only model must be refused: {result:?}");
    };
    assert!(format!("{error:#}").contains("agent `d5`"), "{error:#}");
}

/// RED before PMM-D-5: changing a declaration so that an UNCHANGED
/// selection can no longer run is refused too (the selection is affected
/// by the change even though its fields did not move).
#[tokio::test]
async fn a_declaration_that_rules_out_an_existing_route_is_refused() {
    let fixture = fixture().await;
    apply_structured(
        &fixture,
        |config| {
            d5_declare(config, "model-a", "[\"chat\"]");
            config.model_routes.push(d5_route("model-a"));
        },
        None,
    )
    .await
    .expect("test: a route to a declared chat model is accepted");
    let before = fixture.tree();

    let result = apply_structured(
        &fixture,
        |config| {
            config.providers.models.clear();
            d5_declare(config, "model-a", "[\"embedding\"]");
        },
        None,
    )
    .await;

    assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
    assert_eq!(fixture.tree(), before);
}

/// RED before PMM-D-5: a durable selection made by a model (the agent
/// `config.patch` actor) is strict at the transaction layer itself: an
/// unverified model is refused (D-034), whatever surface built the plan.
#[tokio::test]
async fn a_model_made_selection_of_an_unverified_model_is_refused() {
    let fixture = fixture().await;
    let before = fixture.tree();
    let service = fixture.service();
    let mut config = fixture.desired();
    config.model_routes.push(crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "d5".into(),
        provider: "openrouter".into(),
        model: "vendor/never-seen-model".into(),
        api_key: None,
        credential_ref: None,
    });
    let plan = service
        .plan(ConfigChange::Structured(Box::new(config)))
        .await
        .expect("test: plan");
    let planned = plan.planned_revision().map(str::to_string);

    let result = service
        .apply(
            plan,
            ConfigActor::AgentConfigPatch,
            ApplyOptions {
                expected_revision: None,
                confirm_high_risk: true,
                confirmed_revision: planned.as_deref(),
            },
        )
        .await;

    assert!(matches!(result, Err(ConfigTransactionError::Invalid(_))), "{result:?}");
    assert_eq!(fixture.tree(), before);
}

/// The operator's surfaces accept a selection the evidence neither
/// proves nor refutes and report it (D-034).
#[tokio::test]
async fn an_operator_selection_of_an_unverified_model_is_accepted_with_a_warning() {
    let fixture = fixture().await;
    let outcome = apply_structured(
        &fixture,
        |config| {
            config.model_routes.push(crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "d5".into(),
                provider: "openrouter".into(),
                model: "vendor/never-seen-model".into(),
                api_key: None,
                credential_ref: None,
            });
        },
        None,
    )
    .await
    .expect("test: accepted");
    assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
    assert!(
        outcome
            .warnings
            .first()
            .is_some_and(|warning| warning.contains("hint:d5")),
        "{:?}",
        outcome.warnings
    );
}

// ── PMM-D-8b: URL userinfo re-introduced by a rollback ───────────────

/// RED before PMM-D-8: a rollback that brings back URL userinfo
/// (`user:password@` in a provider URL) the current configuration no
/// longer holds needed no `restore_credentials`, unlike a rotated-out key.
#[tokio::test]
async fn a_rollback_that_restores_rotated_url_userinfo_needs_restore_credentials() {
    let fixture = fixture().await;
    let old = "custom:http://alice:pmmd8-old-password@127.0.0.1:9/v1";
    let new = "custom:http://alice:pmmd8-new-password@127.0.0.1:9/v1";
    apply_structured(
        &fixture,
        |config| {
            config.default_provider = Some(old.into());
            declare_chat(config, old, "anthropic/claude-sonnet-5");
        },
        None,
    )
    .await
    .expect("test: store the old userinfo");
    let rotated = apply_structured(
        &fixture,
        |config| {
            // Drop the old declaration: it carries the old password, and a
            // rollback must still find that password absent from the current config.
            config.providers.models.clear();
            config.default_provider = Some(new.into());
            declare_chat(config, new, "anthropic/claude-sonnet-5");
        },
        None,
    )
    .await
    .expect("test: rotate it");
    let on_disk = fixture.tree();
    let revision = current_revision(&fixture);

    let result = fixture
        .service()
        .rollback(confirmed(&rotated.record_id.expect("test: recorded"), &revision))
        .await;

    assert!(
        matches!(
            &result,
            Err(ConfigTransactionError::ConfirmationRequired {
                required: "restore_credentials",
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fixture.tree(), on_disk, "nothing may be written");
}

/// PMM-D-8: purging the history waits for a transaction in flight, so a
/// record being written is never purged half-way (R-PMMDf §3).
#[tokio::test]
async fn purging_the_history_waits_for_a_transaction_in_flight() {
    let fixture = fixture().await;
    apply_structured(&fixture, |config| config.default_temperature = 0.5, None)
        .await
        .expect("test: a recorded change");
    let service = fixture.service();
    let held = transaction_lock(&fixture.config_path());
    let guard = held.lock().await;

    let purge = tokio::spawn(async move { service.purge_history().await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!purge.is_finished(), "the purge ran while a transaction held the lock");
    drop(guard);

    let removed = purge.await.expect("test: join").expect("test: purge");
    assert_eq!(removed, 1);
    assert!(
        fixture
            .service()
            .history()
            .expect("test: history")
            .list()
            .expect("test: list")
            .is_empty(),
        "expected an empty value"
    );
}

// ── PMMDtail-g (D-080) ───────────────────────────────────────────────

/// RED before PMMDtail-g (D-080 N-3): a rollback asked for
/// `restore_credentials` and `confirm` before it checked the model
/// selections it would restore, so an operator could confirm a restore
/// that was then refused. The selections are checked first.
#[tokio::test]
async fn a_rollback_checks_its_selections_before_asking_for_confirmation() {
    let fixture = fixture().await;
    apply_structured(
        &fixture,
        |config| {
            config.model_routes.push(crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "gone".into(),
                provider: "openrouter".into(),
                model: "vendor/image-maker".into(),
                api_key: None,
                credential_ref: None,
            });
        },
        None,
    )
    .await
    .expect("test: an unverified route is accepted");
    let removed = apply_structured(
        &fixture,
        |config| {
            config.model_routes.retain(|route| route.hint != "gone");
            config.api_key = Some("sentinel-rotated-root".into());
        },
        None,
    )
    .await
    .expect("test: the route is removed and the key rotated");
    // The model cache now says the model only makes images.
    let payload = serde_json::json!({"data": [{
        "id": "vendor/image-maker",
        "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
    }]});
    crate::providers::catalog::discovery::record_discovered(
        &fixture.desired().workspace_dir,
        "openrouter",
        &crate::providers::catalog::discovery::parse_openai_compatible_models(&payload),
        9,
    )
    .expect("test: record the catalog");
    let on_disk = fixture.tree();
    let revision = current_revision(&fixture);

    let error = fixture
        .service()
        .rollback(RollbackRequest {
            record_id: &removed.record_id.expect("test: recorded"),
            expected_revision: &revision,
            confirm: false,
            restore_credentials: false,
            disable_encryption: false,
        })
        .await
        .expect_err("test: refused");

    assert!(
        matches!(error, ConfigTransactionError::Invalid(_)),
        "the refusal must come before any confirmation is asked for: {error}"
    );
    assert!(error.to_string().contains("hint:gone"), "{error}");
    assert!(!error.to_string().contains(fixture::SENTINEL_PREFIX), "{error}");
    assert_eq!(fixture.tree(), on_disk);
}

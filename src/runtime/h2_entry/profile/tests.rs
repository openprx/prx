//! Test-only bootstrap bypasses the OS terminal check, never admission or budgets.
use super::*;
use crate::skills::lifecycle::experiment::*;

pub(in crate::runtime::h2_entry) struct Fixture {
    pub directory: tempfile::TempDir,
    pub config: Config,
    pub profile: Arc<ControlledH2Profile>,
}
impl Fixture {
    pub fn new(kind: EntryKind) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = PathBuf::new();
        config.causal_tree.enabled = true;
        config.memory.events.record_user_messages = true;
        config.skills_lifecycle.enabled = true;
        config.skills_lifecycle.owner_scope = Some("owner:prx:h2:test".into());
        config.memory.backend = "sqlite".into();
        config.runtime.kind = "native".into();
        config.tunnel.provider = "none".into();
        config.plugins.enabled = false;
        config.sessions_spawn.allow_recursive_delegation = false;
        ControlledH2Profile::validate_config(&config).unwrap();
        let owner = config.skills_lifecycle.owner().unwrap().unwrap();
        let store = SkillLifecycleStore::open(&workspace, owner.clone()).unwrap();
        let principal = if kind == EntryKind::Terminal {
            crate::runtime::envelope::RuntimeEnvelope::chat(workspace.to_string_lossy(), "h2-os-startup")
                .with_recipient("terminal:user")
                .resolved_owner_id()
        } else {
            "api:prx-existing-principal".into()
        };
        let profile = Arc::new(ControlledH2Profile {
            boot: uuid::Uuid::new_v4().to_string(),
            kind,
            workspace,
            owner,
            principal: Some(principal),
            configuration: ControlledH2Profile::config_digest(&config).unwrap(),
            source_revision: None,
            schema: ControlledH2Profile::schema_digest(store.root()).unwrap(),
            digest: "a".repeat(64),
            expires: Utc::now() + chrono::Duration::minutes(5),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(300),
            revoked: AtomicBool::new(false),
            limits: EntryLimits {
                max_roots: 2,
                resources: resources(16),
                elapsed_ms: 60_000,
                max_concurrent: 2,
            },
            reader: parking_lot::Mutex::new(ReaderState::default()),
            roots: parking_lot::Mutex::new(Default::default()),
        });
        Self {
            directory,
            config,
            profile,
        }
    }
    pub fn claimed_roots(&self) -> usize {
        self.profile.roots.lock().len()
    }
    pub fn origin(&self) -> Arc<super::super::VerifiedEntryOrigin> {
        Arc::new(super::super::VerifiedEntryOrigin {
            profile: Arc::clone(&self.profile),
            record: super::super::EntryOriginRecord {
                version: 1,
                authority_expires_at: self.profile.expires,
                kind: if self.profile.kind == EntryKind::Gateway {
                    "gateway"
                } else {
                    "terminal"
                }
                .into(),
                boot: self.profile.boot.clone(),
                profile: self.profile.digest.clone(),
                actor: self.profile.principal.clone().unwrap(),
                target_owner: self.profile.owner.as_str().into(),
                workspace: self.profile.workspace.to_string_lossy().into_owned(),
                session: "prx_session".into(),
                run: "prx_run".into(),
                event: "prx_actual_event".into(),
                input: "b".repeat(64),
                routing: "b".repeat(64),
                sequence: Some(0),
            },
        })
    }
}
fn resources(calls: u64) -> ExperimentResources {
    ExperimentResources {
        calls,
        tokens: calls * 1024,
        micro_usd: calls * 100,
        tool_calls: calls * 2,
        output_bytes: calls * 1024,
        scratch_bytes: calls * 8192,
    }
}
pub(in crate::runtime::h2_entry) fn plan(fixture: &Fixture, id: &str) -> FrozenExperiment {
    let now = Utc::now();
    ExperimentSpec {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        id: ExperimentId::parse(id).unwrap(),
        owner_scope: fixture.profile.owner.clone(),
        controlled_admission: None,
        pins: ExperimentPins {
            skill_id: crate::skills::lifecycle::SkillId::parse("prx-skill").unwrap(),
            baseline_revision: crate::skills::lifecycle::RevisionId::parse(&format!("r1-{}", "1".repeat(64))).unwrap(),
            baseline_record_digest: "2".repeat(64),
            baseline_generation: 1,
            binary_sha256: "3".repeat(64),
            config_sha256: "4".repeat(64),
            policy_sha256: "5".repeat(64),
            catalog_sha256: "6".repeat(64),
            environment_sha256: "7".repeat(64),
            edit_scope_sha256: "8".repeat(64),
            model: crate::skills::lifecycle::ModelBinding {
                provider_label: "prx-provider".into(),
                model: "prx-model".into(),
                temperature_milli: 0,
            },
            suite_digest: "a".repeat(64),
            development_split_digest: "b".repeat(64),
            verification_split: crate::skills::lifecycle::EvalSplit::Validation,
            verification_split_digest: "c".repeat(64),
            holdout_split_digest: None,
            lesson_snapshot: None,
        },
        resources: resources(8),
        max_concurrent: 1,
        elapsed_ms: 60_000,
        created_at: now,
        expires_at: now + chrono::Duration::seconds(60),
        stages: vec![ExperimentStage {
            id: "proposal".into(),
            kind: ExperimentStageKind::ModelProposal,
            dependencies: vec![],
            input_sha256: "c".repeat(64),
            allocation: resources(8),
        }],
    }
    .freeze()
    .unwrap()
}

#[test]
fn aggregate_claim_is_idempotent_and_racing_roots_cannot_overallocate() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let same = plan(&fixture, "same");
    fixture.profile.claim_experiment(&same).unwrap();
    fixture.profile.claim_experiment(&same).unwrap();
    assert_eq!(fixture.profile.roots.lock().len(), 1);
    let a = plan(&fixture, "a");
    let b = plan(&fixture, "b");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = [a, b]
        .into_iter()
        .map(|plan| {
            let profile = fixture.profile.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                profile.claim_experiment(&plan).is_ok()
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|accepted| *accepted)
            .count(),
        1
    );
    assert_eq!(fixture.profile.roots.lock().len(), 2);
}
#[test]
fn changed_config_revokes_before_any_second_dispatch() {
    let fixture = Fixture::new(EntryKind::Terminal);
    fixture.profile.verify(&fixture.config).unwrap();
    let mut changed = fixture.config.clone();
    changed.default_temperature += 0.1;
    assert!(fixture.profile.verify(&changed).is_err());
    assert!(fixture.profile.verify(&fixture.config).is_err());
    assert!(
        fixture
            .profile
            .claim_experiment(&plan(&fixture, "after-revoke"))
            .is_err()
    );
}
#[test]
fn strict_profile_refuses_executable_and_remote_factories_before_writes() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let edits: [fn(&mut Config); 5] = [
        |c: &mut Config| c.plugins.enabled = true,
        |c: &mut Config| c.sessions_spawn.allow_recursive_delegation = true,
        |c: &mut Config| c.runtime.kind = "docker".into(),
        |c: &mut Config| c.storage.provider.config.db_url = Some("postgres://invalid".into()),
        |c: &mut Config| c.default_provider = Some("exec:untrusted".into()),
    ];
    for edit in edits {
        let mut config = fixture.config.clone();
        edit(&mut config);
        assert!(ControlledH2Profile::validate_trial_config(&config).is_err());
    }
    for (method, path) in [
        ("PUT", "/api/config"),
        ("POST", "/api/tools/execute"),
        ("POST", "/api/plugins"),
        ("GET", "/api/%69dentity"),
    ] {
        assert!(!ControlledH2Profile::permits_route(method, path));
    }
    assert!(ControlledH2Profile::permits_route("GET", "/api/identity"));
    assert!(!fixture.profile.permits_tool("shell"));
    assert!(!fixture.profile.permits_tool("sessions_spawn"));
}
#[tokio::test]
async fn reader_lifetime_revokes_and_cancel_is_not_a_cleanup_claim() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let cancel = tokio_util::sync::CancellationToken::new();
    let lifetime = super::ControlledReaderLifetime::new(Some(fixture.profile.clone()), cancel.clone());
    assert!(!cancel.is_cancelled());
    drop(lifetime);
    assert!(cancel.is_cancelled());
    assert!(fixture.profile.ensure_live().is_err());
    fixture.profile.close_reader().await.unwrap();
}
#[test]
fn wall_clock_expiry_refuses_new_root_claim() {
    let mut fixture = Fixture::new(EntryKind::Terminal);
    Arc::get_mut(&mut fixture.profile).unwrap().expires = Utc::now() - chrono::Duration::seconds(1);
    assert!(fixture.profile.ensure_live().is_err());
    assert!(fixture.profile.claim_experiment(&plan(&fixture, "expired")).is_err());
    assert!(fixture.profile.roots.lock().is_empty());
}

fn blocked_reader(fixture: &Fixture) -> std::sync::mpsc::SyncSender<()> {
    let (release, wait) = std::sync::mpsc::sync_channel(0);
    fixture
        .profile
        .own_reader(std::thread::spawn(move || {
            let _ = wait.recv();
        }))
        .unwrap();
    release
}
#[tokio::test]
async fn reader_timeout_and_cancelled_repeat_keep_original_observation_until_real_drain() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let release = blocked_reader(&fixture);
    assert!(fixture.profile.close_reader().await.is_err());
    assert!(
        fixture
            .profile
            .reader
            .lock()
            .handle
            .as_ref()
            .is_some_and(|reader| !reader.is_finished())
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), fixture.profile.close_reader())
            .await
            .is_err()
    );
    assert!(fixture.profile.reader.lock().handle.is_some());
    release.send(()).unwrap();
    fixture.profile.close_reader().await.unwrap();
    assert!(fixture.profile.reader.lock().handle.is_none());
}
#[tokio::test]
async fn concurrent_closes_and_aborted_close_cannot_misreport_running_reader_as_clean() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let release = blocked_reader(&fixture);
    let profile = fixture.profile.clone();
    let mut first = tokio::spawn(async move { profile.close_reader().await });
    let profile = fixture.profile.clone();
    let second = tokio::spawn(async move { profile.close_reader().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut first)
            .await
            .is_err()
    );
    assert!(!second.is_finished());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(
        fixture
            .profile
            .reader
            .lock()
            .handle
            .as_ref()
            .is_some_and(|reader| !reader.is_finished())
    );
    release.send(()).unwrap();
    second.await.unwrap().unwrap();
    assert!(fixture.profile.reader.lock().handle.is_none());
    fixture.profile.close_reader().await.unwrap();
}
#[tokio::test]
async fn failed_reader_join_is_sticky_and_never_overwritten_by_empty_slot_success() {
    let fixture = Fixture::new(EntryKind::Terminal);
    fixture
        .profile
        .own_reader(std::thread::spawn(|| panic!("reader fixture failure")))
        .unwrap();
    assert!(fixture.profile.close_reader().await.is_err());
    assert!(fixture.profile.reader.lock().handle.is_none());
    assert!(fixture.profile.reader.lock().failure.is_some());
    assert!(fixture.profile.close_reader().await.is_err());
}

pub(in crate::runtime::h2_entry) fn claimed_roots(profile: &ControlledH2Profile) -> usize {
    profile.roots.lock().len()
}

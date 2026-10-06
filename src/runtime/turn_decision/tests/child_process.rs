#![cfg(test)]
//! AUT component slice: genuine parent grant, real worker and verified close.
//! The parent decision backend is controlled; this is not release C21-07.

use super::*;
use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::providers::{ChatRequest, ChatResponse, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::sessions_spawn::{
    ProcessFinalization, ProcessTerminationRequestResult, SessionsSpawnTool, SubAgentRun, SubAgentStatus,
};
use crate::tools::{ToolExecutionCommand, ToolExecutionContext};
use futures_util::FutureExt as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactFile {
    path: PathBuf,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerArtifact {
    schema: String,
    profile: String,
    binary: ArtifactFile,
    source_manifest: ArtifactFile,
    build_evidence: ArtifactFile,
}

fn checked_file(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(path.is_absolute(), "artifact path must be absolute");
    let mut prefix = PathBuf::new();
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, Component::RootDir | Component::Normal(_)),
            "artifact path must not contain traversal"
        );
        prefix.push(component.as_os_str());
        let kind = std::fs::symlink_metadata(&prefix)?.file_type();
        anyhow::ensure!(
            if prefix == path { kind.is_file() } else { kind.is_dir() },
            "artifact path contains a symlink or special node"
        );
    }
    Ok(())
}

fn checked_hash(artifact: &ArtifactFile) -> anyhow::Result<()> {
    use std::io::Read as _;
    checked_file(&artifact.path)?;
    anyhow::ensure!(
        artifact.sha256.len() == 64
            && artifact
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "artifact digest must be lowercase SHA-256"
    );
    let mut input = std::fs::File::open(&artifact.path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(buffer.get(..count).expect("read count must fit the supplied buffer"));
    }
    anyhow::ensure!(
        hex::encode(hash.finalize()) == artifact.sha256,
        "artifact digest mismatch"
    );
    Ok(())
}

fn worker_artifact() -> anyhow::Result<WorkerArtifact> {
    // Mandatory external bindings are supplied by the reviewed guarded command.
    // A self-authored JSON claim is never sufficient to select an executable.
    let parameter = ArtifactFile {
        path: PathBuf::from(std::env::var("PRX_C28_ARTIFACT_MANIFEST")?),
        sha256: std::env::var("PRX_C28_ARTIFACT_MANIFEST_SHA256")?,
    };
    checked_hash(&parameter)?;
    anyhow::ensure!(
        std::fs::metadata(&parameter.path)?.len() <= 16 * 1024,
        "artifact parameter manifest too large"
    );
    let artifact: WorkerArtifact = serde_json::from_slice(&std::fs::read(&parameter.path)?)?;
    anyhow::ensure!(
        artifact.schema == "prx-c28-prebuilt-v1",
        "unexpected worker artifact contract"
    );
    anyhow::ensure!(
        artifact.profile == std::env::var("PRX_C28_EXPECTED_PROFILE")?
            && matches!(artifact.profile.as_str(), "dev" | "release")
            && artifact.source_manifest.sha256 == std::env::var("PRX_C28_EXPECTED_SOURCE_SHA256")?,
        "worker source/profile differs from the independently frozen expected build"
    );
    checked_hash(&artifact.binary)?;
    checked_hash(&artifact.source_manifest)?;
    checked_hash(&artifact.build_evidence)?;
    Ok(artifact)
}

struct NoTransport;
#[async_trait::async_trait]
impl Provider for NoTransport {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("this parent component fixture must not dispatch a provider")
    }
    async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        anyhow::bail!("this parent component fixture must not dispatch a provider")
    }
}

struct NoAnnouncements;
#[async_trait::async_trait]
impl Channel for NoAnnouncements {
    fn name(&self) -> &str {
        "prx_component"
    }
    async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
        anyhow::bail!("batch completion must return through join, not announce")
    }
    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        anyhow::bail!("component fixture has no inbound channel listener")
    }
}

struct ParentDelegateBackend {
    identity: BackendIdentity,
}
#[async_trait::async_trait]
impl DecisionBackend for ParentDelegateBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
    // This cfg(test) backend computes controlled answers locally and never
    // performs physical I/O. Explicitly accept the new injected accounting
    // interface without inventing a reservation, receipt, or usage settlement.
    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        self.evaluate(request, body, deadline).await
    }

    async fn evaluate(
        &self,
        request: &DecisionRequest,
        _: &GuardedBody,
        _: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        // Only model answers change. The factory still binds the real compiled
        // request, validates every answer, and runs the ordinary hard gates.
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".into();
        response.resolved_model = self.identity.configured_model.to_string();
        for (id, answer) in &mut response.answers {
            let work = id.as_str().starts_with("candidate.delegate-session");
            match answer {
                DecisionAnswer::Choice(choice) => {
                    let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                        "delegate"
                    } else {
                        "session"
                    };
                    if let Some(chosen) = choice.probabilities.keys().find(|key| key.as_str() == code).cloned() {
                        for (key, probability) in &mut choice.probabilities {
                            *probability = Probability::new(if key == &chosen { 1.0 } else { 0.0 })?;
                        }
                        choice.chosen = chosen;
                        choice.confidence = Probability::new(1.0)?;
                    }
                }
                DecisionAnswer::Noul(noul) => {
                    noul.probability_true = Probability::new(
                        if work || id == &QuestionId::global(GlobalQuestion::EvidenceSufficient) {
                            1.0
                        } else {
                            0.0
                        },
                    )?;
                }
                DecisionAnswer::Score(score) => {
                    let gain = id.as_str().ends_with("gain") && (work || !id.as_str().starts_with("candidate."));
                    let chosen = if gain {
                        score.distribution.len().saturating_sub(1)
                    } else {
                        0
                    };
                    for (index, probability) in score.distribution.iter_mut().enumerate() {
                        *probability = Probability::new(if index == chosen { 1.0 } else { 0.0 })?;
                    }
                    score.expected = f64::from(u32::try_from(chosen).expect("bounded score"));
                    score.confidence = Probability::new(1.0)?;
                }
            }
        }
        Ok(response)
    }
}

/// Only the existing sole monitor may signal/reap a child. Registry rows are
/// published before that monitor can spawn, so even an interrupted launch is
/// visible here. Pending/unknown is failure, never cleanup acknowledgement.
async fn drain_fixture_processes(runs: &Arc<tokio::sync::RwLock<Vec<SubAgentRun>>>) -> anyhow::Result<()> {
    let cleanup = async {
        let owned: Vec<_> = runs
            .read()
            .await
            .iter()
            .map(|run| (run.id.clone(), run.process_control.clone()))
            .collect();
        let mut unconfirmed = Vec::new();
        for (run_id, control) in owned {
            let Some(control) = control else {
                unconfirmed.push(format!("{run_id}: missing process owner"));
                continue;
            };
            let outcome = if let Some(finalization) = control.finalization() {
                ProcessTerminationRequestResult::Finalized(finalization)
            } else {
                control.request_termination("C28 AUT fixture cleanup").await
            };
            if !matches!(
                outcome,
                ProcessTerminationRequestResult::Finalized(
                    ProcessFinalization::Natural | ProcessFinalization::Terminated
                )
            ) {
                unconfirmed.push(format!("{run_id}: {outcome:?}"));
            }
        }
        anyhow::ensure!(
            unconfirmed.is_empty(),
            "process cleanup unconfirmed: {}",
            unconfirmed.join(", ")
        );
        Ok::<_, anyhow::Error>(())
    };
    // Independent test cleanup allowance, never fed into request policy or
    // decision deadlines. The process owner's existing request wait is 5s.
    tokio::time::timeout(Duration::from_secs(15), cleanup)
        .await
        .map_err(|_| anyhow::anyhow!("process cleanup watchdog expired; drain unconfirmed"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root-reviewed hash-bound production worker artifact and outer HOME/network guard; AUT only"]
async fn actual_parent_grant_process_builtin_close_has_signed_completion_and_join() -> anyhow::Result<()> {
    boxed_parent_process_case(false).await
}

mod route_binding;

// Keep the large existing fixture future behind a synchronous test-only box
// boundary; adding a selector must not duplicate its stack construction frame.
fn boxed_parent_process_case(
    non_root_route: bool,
) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<()>>>> {
    Box::pin(actual_parent_process_case(non_root_route))
}

async fn actual_parent_process_case(non_root_route: bool) -> anyhow::Result<()> {
    let artifact = worker_artifact()?;
    let route_capture = non_root_route.then(|| {
        Arc::new(parking_lot::Mutex::new(
            crate::tools::sessions_spawn::TestWorkerRouteCapture::default(),
        ))
    });
    let scratch = tempfile::tempdir()?;
    // This ignored fresh-process AUT bypasses CLI ingress in its parent. Publish
    // the same owned config directory that the actual child CLI receives before
    // either side resolves its lazy session-worker signing key. Keep it through
    // the common cleanup tail, and restore even after a panic.
    struct ParentConfigScope {
        previous: Option<std::path::PathBuf>,
        _lock: parking_lot::MutexGuard<'static, ()>,
    }
    impl Drop for ParentConfigScope {
        fn drop(&mut self) {
            // Restore while the existing global test lock is still held.
            crate::config::schema::swap_process_config_dir_for_tests(self.previous.take());
        }
    }
    let config_lock = crate::config::schema::process_config_dir_test_lock();
    let _parent_config_scope = ParentConfigScope {
        previous: crate::config::schema::swap_process_config_dir_for_tests(Some(scratch.path().join("config"))),
        _lock: config_lock,
    };
    let owned_runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    // Catch every assertion panic and Result error after launch. The common
    // tail always waits for the actual process owner before returning.
    let scenario = std::panic::AssertUnwindSafe(async {
        let config_dir = scratch.path().join("config");
        // Match the production --config-dir loader; workspace_dir is not serialized.
        let workspace = config_dir.join("workspace");
        let private_home = scratch.path().join("worker-home");
        let workers = scratch.path().join("workers");
        for path in [&workspace, &config_dir, &private_home, &workers] {
            std::fs::create_dir_all(path)?;
        }
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = config_dir.join("config.toml");
        config.default_provider = Some("ollama".into());
        config.default_model = Some("prx-local-model".into());
        if non_root_route {
            route_binding::configure(&mut config);
        }
        config.memory.backend = "sqlite".into();
        config.causal_tree.enabled = true;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        config.sessions_spawn.default_mode = "process".into();
        config.sessions_spawn.process_memory_strategy = "isolated_private".into();
        config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
        config.sessions_spawn.cleanup_on_complete = false;
        config.providers.models.push(toml::from_str(
            "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
        )?);
        let resolved = crate::providers::selection::check(
            &config,
            "ollama",
            "prx-local-model",
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        assert_eq!(
            resolved.verification(),
            &crate::providers::catalog::Verification::Verified
        );
        std::fs::write(&config.config_path, toml::to_string(&config)?)?;
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &workspace));
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
        let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let spawn = SessionsSpawnTool::new_with_registry(
            Arc::new(NoAnnouncements),
            Arc::new(NoTransport),
            "ollama",
            if non_root_route {
                route_binding::MODEL_HINT
            } else {
                "prx-local-model"
            },
            0.0,
            Arc::clone(&security),
            workspace.clone(),
            config.multimodal.clone(),
            config.agent.compaction.clone(),
            Default::default(),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            config.sessions_spawn.clone(),
            Arc::clone(&owned_runs),
        )
        .with_hint_config(&config)
        .with_model_catalog(crate::providers::selection::catalog_for(&config))
        .with_shared_memory(Arc::clone(&memory))
        .with_source_fabric(fabric)
        .with_event_recording(config.memory.event_recording_config())
        .with_test_worker_executable(artifact.binary.path, artifact.binary.sha256, private_home)?;
        let spawn = if let Some(capture) = route_capture.as_ref() {
            spawn.with_test_route_observer(Arc::clone(capture))?
        } else {
            spawn
        };
        let runs = spawn.active_runs_arc();
        let tools_handle = spawn.tools_handle();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(spawn)]);
        assert!(tools_handle.set(&registry));
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy {
                    direct_names: BTreeSet::from(["sessions_spawn".into()]),
                    ..SurfacePolicy::default()
                },
                provider_catalog: &catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )?;
        let owning = Arc::get_mut(&mut service).expect("unshared parent service");
        owning.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(ParentDelegateBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
            })],
            Arc::clone(&owning.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let proof = if non_root_route {
            crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "sessions_spawn: Describe the project.",
                "prx_parent_process_component",
                ("ollama", route_binding::MODEL_HINT),
            )
            .await
        } else {
            crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                &generation,
                Arc::clone(&memory),
                "sessions_spawn: Describe the project.",
                "prx_parent_process_component",
            )
            .await
        };
        let cancellation = CancellationToken::new();
        let context = service
            .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
            .await?;
        assert!(matches!(context.directive(), TurnExecutionDirective::Delegate { .. }));
        assert!(context.record_admitted.load(Ordering::Acquire));
        let selected = service.agent_execution_specs(&proof, &context)?;
        assert_eq!(selected.specs.len(), 1);
        assert_eq!(
            selected.specs.first().expect("one selected spawn spec").name,
            "sessions_spawn"
        );
        let envelope = RuntimeEnvelope::agent(proof.workspace_id(), proof.run_id())
            .with_config_generation(&generation)
            .with_source_message_event_id(proof.source_event_id().expect("real parent source"));
        let original_execution =
            ToolExecutionContext::new(envelope.clone(), "private").with_chat_id(proof.session_key());
        let (execution, bound) =
            service.bind_agent_execution(&proof, &context, &original_execution, &selected.specs)?;
        let grant = bound.child_grant.as_ref().expect("actual parent-issued grant");
        assert_eq!(grant.parent().source_event_id(), proof.source_event_id());
        assert_eq!(grant.parent().owner_id(), proof.owner_id());
        assert_eq!(grant.parent().session_key(), proof.session_key());
        assert_eq!(grant.parent().workspace_id(), proof.workspace_id());
        assert_eq!(
            grant.parent_record_binding().0,
            service.store.workspace_id().expect("store UUID").as_ref()
        );
        assert_ne!(grant.parent_record_binding().0, proof.workspace_id());
        let (child_scope, parent_scope) = grant.scope().gate_inputs();
        assert!(
            child_scope.allowed.is_empty(),
            "closed model-only child requests no inherited tools"
        );
        assert!(!parent_scope.allowed.is_empty());
        assert_eq!(child_scope.parent_decision_id, context.decision_id());
        // Match the real CLI turn adapter's ambient lineage. This carries
        // the issued proof's actual parent IDs; it grants no capabilities.
        let turn_spawn_context = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            proof.run_id().to_owned(),
            proof.session_key().to_owned(),
        );
        let launch = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
            .scope(
                turn_spawn_context.clone(),
                execution.execute(
                    ToolExecutionCommand::new(
                        "sessions_spawn",
                        serde_json::json!({
                            "action":"spawn_batch", "mode":"process", "tasks":["Describe the project."]
                        }),
                    ),
                    bound.clone(),
                    Some(cancellation.clone()),
                ),
            )
            .await;
        assert!(launch.succeeded(), "actual launch failed: {:?}", launch.error);
        let payload: serde_json::Value = serde_json::from_str(&launch.result.expect("launch result").output)?;
        assert_eq!(payload.get("started").expect("started field"), 1);
        assert_eq!(
            payload
                .get("recovery_journal_confirmed")
                .expect("recovery_journal_confirmed field"),
            true
        );
        let batch = payload
            .get("batch_id")
            .expect("batch_id field")
            .as_str()
            .expect("real batch ID");
        let run_id = payload
            .get("spawned")
            .and_then(serde_json::Value::as_array)
            .and_then(|rows| rows.first())
            .and_then(|row| row.get("run_id"))
            .expect("first spawned worker run ID")
            .as_str()
            .expect("real worker run ID");
        let joined = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
            .scope(
                turn_spawn_context,
                execution.execute(
                    ToolExecutionCommand::new(
                        "sessions_spawn",
                        serde_json::json!({
                            "action":"join", "batch_id":batch
                        }),
                    ),
                    bound,
                    Some(cancellation),
                ),
            )
            .await;
        assert!(joined.succeeded(), "actual join failed: {:?}", joined.error);
        let summary: serde_json::Value = serde_json::from_str(&joined.result.expect("join result").output)?;
        assert_eq!(summary.get("total").expect("total field"), 1);
        assert_eq!(
            summary
                .get("completed")
                .expect("completed field")
                .as_array()
                .expect("completed array")
                .len(),
            1,
            "{summary}"
        );
        assert_eq!(
            summary
                .get("failed")
                .expect("failed field")
                .as_array()
                .expect("failed array")
                .len(),
            0
        );
        assert_eq!(
            summary
                .get("no_result")
                .expect("no_result field")
                .as_array()
                .expect("unconfirmed array")
                .len(),
            0
        );
        let run = runs
            .read()
            .await
            .iter()
            .find(|run| run.id == run_id)
            .expect("owned registry member")
            .clone();
        assert!(matches!(run.status, SubAgentStatus::Completed(_)));
        assert_eq!(run.owner_id.as_deref(), Some(proof.owner_id()));
        assert_eq!(run.parent_run_id.as_deref(), Some(proof.run_id()));
        assert_ne!(run.session_scope_key, proof.session_key());
        assert!(
            run.token_usage_records.is_empty(),
            "built-in close must not report provider usage"
        );
        let completion = memory
            .find_owned_message_event_by_idempotency_key(
                proof.workspace_id(),
                proof.owner_id(),
                &format!("worker:{run_id}:completion"),
            )
            .await?
            .expect("actual verified completion journal");
        assert_eq!(completion.event_type, "worker.completion.confirmed");
        assert_eq!(completion.owner_id.as_deref(), Some(proof.owner_id()));
        assert_eq!(completion.workspace_id, proof.workspace_id());
        let result: crate::session_worker::protocol::WorkerResult =
            serde_json::from_str(completion.raw_payload_json.as_deref().expect("completion payload"))?;
        assert!(result.success);
        let evidence = result
            .completion_evidence
            .as_ref()
            .expect("decision-enabled completion");
        assert_eq!(evidence.run_id, run_id);
        if let Some(capture) = route_capture.as_ref() {
            route_binding::verify_capture(capture, &proof, run_id)?;
        }
        assert_eq!(
            run.source_message_event_id.as_deref(),
            proof.source_event_id(),
            "registry retains the parent input linkage"
        );
        let child_input = memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), &evidence.input_event_id)
            .await?
            .expect("issued worker input");
        assert_eq!(child_input.run_id.as_deref(), Some(run_id));
        assert_eq!(child_input.parent_run_id.as_deref(), Some(proof.run_id()));
        assert_eq!(child_input.parent_session_key.as_deref(), Some(proof.session_key()));
        assert_eq!(child_input.owner_id.as_deref(), Some(proof.owner_id()));
        assert_eq!(child_input.session_key.as_deref(), Some(run.session_scope_key.as_str()));
        assert_ne!(Some(child_input.event_id.as_str()), proof.source_event_id());
        assert_eq!(
            run.process_control.as_ref().expect("real process owner").finalization(),
            Some(ProcessFinalization::Natural)
        );
        assert_eq!(evidence.recorded_segments, 1);
        assert_eq!(evidence.segments.len(), 1);
        let segment = memory
            .find_owned_message_event(
                proof.workspace_id(),
                proof.owner_id(),
                &evidence.segments.first().expect("one settled child segment").event_id,
            )
            .await?
            .expect("real settled child segment");
        let terminal: serde_json::Value =
            serde_json::from_str(segment.raw_payload_json.as_deref().expect("terminal payload"))?;
        assert!(terminal.get("provider_outcome").is_none_or(serde_json::Value::is_null));
        assert!(terminal.get("usage_settlement").is_none_or(serde_json::Value::is_null));
        assert!(
            terminal.get("decision_id").is_some_and(serde_json::Value::is_string),
            "real child still runs its own builtin decision"
        );
        let store_uuid = service.store.workspace_id().expect("original logical store UUID");
        let replay = crate::agent::terminal::verify_terminal_replay(
            memory.as_ref(),
            &service.store.reader(),
            &service.state_key,
            &store_uuid,
            proof.workspace_id(),
            proof.owner_id(),
            &evidence
                .segments
                .first()
                .expect("one settled child segment")
                .terminal_id,
            &segment,
        )
        .await?;
        assert_eq!(replay.parent_decision_id(), Some(context.decision_id()));
        assert_eq!(replay.source().event_id, child_input.event_id);
        assert_eq!(replay.decision().backend_kind, "builtin");
        assert!(matches!(
            replay.decision().directive_kind.as_str(),
            "clarify" | "refuse"
        ));
        let parent_record = service
            .store
            .reader()
            .replay_record(
                &DecisionScope::Owner {
                    workspace_id: Arc::clone(&store_uuid),
                    owner_id: Arc::from(proof.owner_id()),
                },
                &context.decision_id(),
            )?
            .expect("actual parent decision remains durable");
        assert_eq!(replay.parent_digest(), Some(parent_record.digest.as_str()));
        assert!(
            workers.join(run_id).join("brain.db").is_file(),
            "private execution memory exists separately from logical source"
        );
        assert!(workspace.join("memory/brain.db").is_file());
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);

        // A consistent SQLite snapshot preserves actual issued evidence. No test
        // creates a signature, source receipt, grant, or positive worker result.
        let copied_workspace = scratch.path().join("recovery-copy");
        std::fs::create_dir_all(copied_workspace.join("memory"))?;
        let copied_db = copied_workspace.join("memory/brain.db");
        let logical_db = rusqlite::Connection::open(workspace.join("memory/brain.db"))?;
        logical_db.execute("VACUUM INTO ?1", [copied_db.to_string_lossy().as_ref()])?;
        let copied_memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&copied_workspace)?);
        let copy = rusqlite::Connection::open(&copied_db)?;
        let launch_event = copied_memory
            .find_owned_message_event_by_idempotency_key(
                proof.workspace_id(),
                proof.owner_id(),
                &format!("worker:{run_id}:launch"),
            )
            .await?
            .expect("copied genuine launch");
        let before_count: i64 = copy.query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))?;
        for tampered in [false, true] {
            if tampered {
                let mut altered = result.clone();
                altered
                    .completion_evidence
                    .as_mut()
                    .expect("real completion inventory")
                    .seal[0] ^= 1;
                assert_eq!(
                    copy.execute(
                        "UPDATE message_events SET raw_payload_json = ?1 WHERE event_id = ?2",
                        rusqlite::params![serde_json::to_string(&altered)?, completion.event_id],
                    )?,
                    1
                );
            }
            let recovery = SessionsSpawnTool::new(
                Arc::new(NoAnnouncements),
                Arc::new(NoTransport),
                "ollama",
                "prx-local-model",
                0.0,
                Arc::new(SecurityPolicy::from_config(&config.autonomy, &workspace)),
                workspace.clone(),
                config.multimodal.clone(),
                config.agent.compaction.clone(),
                Default::default(),
                None,
                crate::providers::ProviderRuntimeOptions::default(),
                config.sessions_spawn.clone(),
            )
            .with_source_fabric(MemoryFabric::new(Arc::clone(&copied_memory), proof.workspace_id()));
            // Exact one-row page: errors cannot be hidden by a different launch.
            let page = recovery.recover_process_page_for_test(launch_event.id - 1, 1).await?;
            assert_eq!(page.len(), 1);
            let item = page.into_iter().next().expect("one recovery verdict");
            assert_eq!(item.launch_event_id, launch_event.id);
            let restored = recovery.active_runs_arc();
            if tampered {
                let error = item.result.expect_err("modified real completion MAC must be rejected");
                assert!(
                    format!("{error:#}").contains("worker completion authentication failed"),
                    "{error:#}"
                );
                assert!(
                    restored.read().await.is_empty(),
                    "rejected completion must not create a recovered success"
                );
            } else {
                assert_eq!(
                    item.result?,
                    Some(true),
                    "unmodified copy must pass the same real consumer"
                );
                let rows = restored.read().await;
                assert_eq!(rows.len(), 1);
                let row = rows.first().expect("one recovered child");
                assert_eq!(row.id, run_id);
                assert!(matches!(row.status, SubAgentStatus::Completed(_)));
                assert_eq!(row.owner_id.as_deref(), Some(proof.owner_id()));
            }
            let after_count: i64 = copy.query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))?;
            assert_eq!(after_count, before_count, "recovery must not append or resend work");
            assert_eq!(runs.read().await.len(), 1, "no new live process registry member");
        }
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind();
    // This is a fixture watchdog, not a larger business decision allowance.
    let outcome = tokio::time::timeout(Duration::from_mins(1), scenario).await;
    let cleanup = drain_fixture_processes(&owned_runs).await;
    if let Err(error) = cleanup {
        let retained = scratch.keep();
        anyhow::bail!(
            "C28 AUT cleanup failed; retained scratch {} for explicit reconciliation; {error:#}",
            retained.display()
        );
    }
    match outcome {
        Ok(Ok(Ok(()))) => {
            let retained = scratch.keep();
            let subscriber = tracing_subscriber::fmt()
                .with_test_writer()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::INFO)
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(
                    "C28 AUT succeeded after confirmed process cleanup; retained scratch {}",
                    retained.display()
                );
            });
            Ok(())
        }
        Ok(Ok(Err(error))) => {
            let retained = scratch.keep();
            Err(error.context(format!(
                "C28 AUT failed after confirmed process cleanup; retained scratch {}",
                retained.display()
            )))
        }
        Ok(Err(panic)) => {
            let detail = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            let retained = scratch.keep();
            anyhow::bail!(
                "C28 AUT assertion panicked ({detail}); process cleanup confirmed; retained scratch {}",
                retained.display()
            );
        }
        Err(_) => {
            let retained = scratch.keep();
            anyhow::bail!(
                "C28 AUT 60s watchdog expired; process cleanup confirmed; retained scratch {}",
                retained.display()
            );
        }
    }
}

// Admission-only regression: the real parent source, model catalog, compiler,
// validator, gates and durable grant issuer run; no worker is launched.
async fn parent_effect_policy_case(level: crate::security::AutonomyLevel) -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let owned_runs = Arc::new(tokio::sync::RwLock::new(Vec::<SubAgentRun>::new()));
    let workspace = scratch.path().join("logical");
    let config_dir = scratch.path().join("config");
    let private_home = scratch.path().join("worker-home");
    let workers = scratch.path().join("workers");
    for path in [&workspace, &config_dir, &private_home, &workers] {
        std::fs::create_dir_all(path)?;
    }
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = config_dir.join("config.toml");
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    config.memory.backend = "sqlite".into();
    config.causal_tree.enabled = true;
    config.autonomy.level = level;
    config.sessions_spawn.default_mode = "process".into();
    config.sessions_spawn.process_memory_strategy = "isolated_private".into();
    config.sessions_spawn.worker_workspace_root = Some(workers.to_string_lossy().into_owned());
    config.sessions_spawn.cleanup_on_complete = false;
    config.providers.models.push(toml::from_str(
        "provider='ollama'\nmodel='prx-local-model'\noperations=['chat']\nnative_tools=true\n",
    )?);
    let resolved = crate::providers::selection::check(
        &config,
        "ollama",
        "prx-local-model",
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    assert_eq!(
        resolved.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    std::fs::write(&config.config_path, toml::to_string(&config)?)?;
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &workspace));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
    let fabric = MemoryFabric::new(Arc::clone(&memory), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let spawn = SessionsSpawnTool::new_with_registry(
        Arc::new(NoAnnouncements),
        Arc::new(NoTransport),
        "ollama",
        "prx-local-model",
        0.0,
        Arc::clone(&security),
        workspace.clone(),
        config.multimodal.clone(),
        config.agent.compaction.clone(),
        Default::default(),
        None,
        crate::providers::ProviderRuntimeOptions::default(),
        config.sessions_spawn.clone(),
        Arc::clone(&owned_runs),
    )
    .with_hint_config(&config)
    .with_model_catalog(crate::providers::selection::catalog_for(&config))
    .with_shared_memory(Arc::clone(&memory))
    .with_source_fabric(fabric)
    .with_event_recording(config.memory.event_recording_config());
    let tools_handle = spawn.tools_handle();
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(spawn)]);
    assert!(tools_handle.set(&registry));
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let mut service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                direct_names: BTreeSet::from(["sessions_spawn".into()]),
                ..SurfacePolicy::default()
            },
            provider_catalog: &catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )?;
    let owning = Arc::get_mut(&mut service).expect("unshared parent service");
    owning.factory = DecisionBackendFactory::from_slots(
        vec![Box::new(ParentDelegateBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-local-model"),
            },
        })],
        Arc::clone(&owning.state_key),
        OutboundSecrets::default(),
        SlotLimits::default(),
    );
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        "sessions_spawn: Describe the project.",
        "prx_parent_process_component",
    )
    .await;
    let cancellation = CancellationToken::new();
    let context = service
        .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
        .await?;
    assert_eq!(
        context.engine.state.trusted.parts().owner_id.as_deref(),
        Some(proof.owner_id())
    );
    assert_eq!(
        context.engine.state.trusted.parts().source_event_id.as_deref(),
        proof.source_event_id()
    );
    assert_eq!(context.catalog_generation(), *service.catalog.generation());
    assert!(context.record_admitted.load(Ordering::Acquire));
    if level == crate::security::AutonomyLevel::Full {
        assert!(matches!(context.directive(), TurnExecutionDirective::Delegate { .. }));
        let selected = service.agent_execution_specs(&proof, &context)?;
        assert_eq!(selected.specs.len(), 1);
        assert_eq!(
            selected.specs.first().expect("one selected spawn spec").name,
            "sessions_spawn"
        );
        let grant = service.issue_child_grant(&proof, &context)?;
        assert_eq!(grant.parent().source_event_id(), proof.source_event_id());
        assert_eq!(grant.parent().owner_id(), proof.owner_id());
        let (manifest, parent) = grant.scope().gate_inputs();
        manifest.verify(parent)?;
        assert_eq!(manifest.authority.level, level);
        assert!(
            manifest.allowed.is_empty(),
            "model-only child inherits no executable tools"
        );
        assert!(manifest.allowed.len() < parent.allowed.len());

        // Recheck the actual selected parent capability against the real
        // narrower child seal. Full runtime mode cannot widen that scope.
        use crate::causal_tree::gates::{GateInputs, GateOutcome, GatePass, HardGates};
        let crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } =
            &context.engine.candidates
        else {
            anyhow::bail!("real parent has no legal candidate set");
        };
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.action() == crate::causal_tree::branch::CausalActionKind::Delegate)
            .expect("actual delegate candidate");
        let calls = BTreeMap::new();
        let seen = BTreeSet::new();
        let mut gates = GateInputs {
            policy: &service.security,
            owner_id: proof.owner_id(),
            sender: proof.policy_sender(),
            policy_channel: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            model_ref: &context.engine.state.trusted.parts().resolved_model,
            model_can_chat: true,
            model_can_call_tools: service.model_can_call_tools(&proof),
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &seen,
            retry_limit: Some(0),
        };
        let view = service.catalog_view();
        for pass in [GatePass::PrePrediction, GatePass::PostRehearsal, GatePass::PreCommit] {
            assert_eq!(
                HardGates::check_one(pass, &context.engine.state, &view, candidate, &gates),
                GateOutcome::Passed
            );
            gates.child_scope = Some((manifest, parent));
            assert_eq!(
                HardGates::check_one(pass, &context.engine.state, &view, candidate, &gates),
                GateOutcome::Blocked {
                    gate: 1,
                    reason_code: "scope"
                }
            );
            gates.child_scope = None;
            // Negative gate projection only: no resume receipt or approval is
            // minted. An exact-resume restriction cannot borrow Full policy
            // to admit this otherwise legal, unapproved executable candidate.
            let mut resumed = context.engine.state.clone();
            let mut trusted = resumed.trusted.parts().clone();
            trusted
                .constraints
                .push(crate::causal_tree::state::ConstraintCode::new("resume_exact_call"));
            resumed.trusted = TrustedState::from_runtime(trusted);
            assert_eq!(
                HardGates::check_one(pass, &resumed, &view, candidate, &gates),
                GateOutcome::Blocked {
                    gate: 9,
                    reason_code: "resume_scope"
                }
            );
        }
    } else {
        assert!(matches!(context.disposition()?, TurnDisposition::Close { .. }));
        assert!(service.agent_execution_specs(&proof, &context).is_err());
        assert!(service.issue_child_grant(&proof, &context).is_err());
        assert_eq!(
            match service.verified_delegation_scope(&context) {
                Ok(_) => panic!("closed turn has no executable scope"),
                Err(error) => error.to_string(),
            },
            "selected decision is not delegation"
        );
        if level == crate::security::AutonomyLevel::ReadOnly {
            assert!(context.engine.outgoing_child.is_none());
        } else {
            // Preparation derives this sealed subset before candidate gates
            // and selection. It is not an IssuedChildGrant or a dispatch port.
            let prepared = context
                .engine
                .outgoing_child
                .as_ref()
                .expect("prepared supervised subset");
            assert_eq!(prepared.authority.level, crate::security::AutonomyLevel::Supervised);
            assert_eq!(prepared.authority.owner, proof.owner_id());
            assert_eq!(prepared.authority.workspace, proof.workspace_id());
            assert_eq!(prepared.parent_decision_id, context.decision_id());
            assert_eq!(prepared.catalog_generation, context.catalog_generation());
            assert!(
                prepared.allowed.is_empty(),
                "prepared model-only child cannot acquire tools"
            );
        }
    }
    assert!(owned_runs.read().await.is_empty(), "admission never launches a worker");
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    service.shutdown(Instant::now() + Duration::from_secs(2)).await;
    Ok(())
}

#[tokio::test]
async fn full_policy_issues_real_parent_delegate_grant() -> anyhow::Result<()> {
    parent_effect_policy_case(crate::security::AutonomyLevel::Full).await
}

#[tokio::test]
async fn read_only_policy_cannot_issue_parent_delegate_grant() -> anyhow::Result<()> {
    parent_effect_policy_case(crate::security::AutonomyLevel::ReadOnly).await
}

#[tokio::test]
async fn supervised_without_exact_approval_cannot_issue_parent_delegate_grant() -> anyhow::Result<()> {
    parent_effect_policy_case(crate::security::AutonomyLevel::Supervised).await
}

mod g8_segment;

#![cfg(test)]

use super::{recovery_config, recovery_input, recovery_recorder};
use crate::memory::Memory;
use crate::memory::SqliteMemory;
use crate::memory::ingress_recovery::IngressAttestationError;
use crate::memory::recovery_authority::{RecoveryChain, RecoveryPublication};
use crate::runtime::ingress_recovery::tombstone_fixture;
use crate::runtime::ingress_recovery::{LegacyUnconfirmed, RecoveryScope, SettledTerminalFacts, backend_memory};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::path::Path;
use std::sync::Arc;

fn home_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tombstone_fixture::home_dir();
    let workspace = dir.path().canonicalize().expect("test: workspace");
    (dir, workspace)
}

fn cell_config(workspace: &Path, recording: bool) -> crate::config::Config {
    let mut config = recovery_config(workspace);
    config.memory.events.record_user_messages = recording;
    config
}

async fn prepare_recovery(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn Memory>,
    tracker: &Arc<CleanupTracker>,
) -> Arc<RecoveryScope> {
    RecoveryScope::prepare(
        generation,
        memory,
        crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop,
        tracker,
    )
    .await
    .expect("test: recovery prepare")
}

fn settled_facts(terminal_id: &str, claim: &str, final_id: &str, key_id: &str) -> SettledTerminalFacts {
    SettledTerminalFacts {
        v: 1,
        terminal_id: terminal_id.to_owned(),
        claim_event_id: claim.to_owned(),
        assistant_event_id: None,
        usage_event_id: None,
        final_event_id: final_id.to_owned(),
        semantic_mac_v3: "00".repeat(32),
        status: "completed".into(),
        surface: crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop
            .as_str()
            .to_owned(),
        parent_authority_id: None,
        pricing: "unknown".into(),
        usage_digest: "00".repeat(32),
        key_id: key_id.to_owned(),
        skill_observation: super::super::authority::SkillObservation {
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            awaiting_approval: false,
            verified: true,
        },
    }
}

struct SettledSetup {
    dir: tempfile::TempDir,
    workspace: std::path::PathBuf,
    memory: Arc<dyn Memory>,
    recovery: Arc<RecoveryScope>,
    tracker: Arc<CleanupTracker>,
    chain: RecoveryChain,
    input_id: String,
    final_id: String,
    key_id: String,
}

async fn settled_setup_with(facts: SettledTerminalFacts, facts_bytes: Option<Vec<u8>>) -> SettledSetup {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = backend_memory("sqlite", &workspace).expect("test: sqlite");
    let config = Arc::new(cell_config(&workspace, true));
    let (recorder, ticket) = recovery_recorder(Arc::clone(&memory), &config).await;
    let input = recorder
        .append(recovery_input(&workspace, "prx_run"))
        .await
        .expect("test: ingress");
    let mut final_input = recovery_input(&workspace, "prx_run");
    final_input.idempotency_key = Some("turn:prx_run:final".into());
    final_input.event_id = Some(uuid::Uuid::new_v4().to_string());
    let final_event = memory.append_message_event(final_input).await.expect("test: final");
    let repository = memory.recovery_repository().expect("test: repository");
    let chain = RecoveryChain {
        workspace_id: workspace.to_string_lossy().into_owned(),
        owner_id: "prx_user".into(),
        source: "agent".into(),
        run_id: "prx_run".into(),
    };
    let pending = repository
        .read_recovery_receipt(&chain)
        .expect("test: pending read")
        .expect("test: pending receipt");
    let mut facts = facts;
    if facts.key_id.is_empty() {
        facts.key_id = pending.key_id.clone();
    }
    if facts.claim_event_id.is_empty() {
        facts.claim_event_id = input.event_id.clone();
    }
    if facts.final_event_id.is_empty() {
        facts.final_event_id = final_event.event_id.clone();
    }
    let bytes = if let Some(raw) = facts_bytes {
        let mut value: serde_json::Value = serde_json::from_slice(&raw).expect("test: facts patch");
        let object = value.as_object_mut().expect("test: facts object");
        object.insert("claim_event_id".into(), facts.claim_event_id.clone().into());
        object.insert("final_event_id".into(), facts.final_event_id.clone().into());
        object.insert("key_id".into(), facts.key_id.clone().into());
        object.insert("terminal_id".into(), facts.terminal_id.clone().into());
        serde_json::to_vec(&value).expect("test: patched facts")
    } else {
        serde_json::to_vec(&facts).expect("test: facts")
    };
    repository
        .settle_recovery(&RecoveryPublication {
            chain: chain.clone(),
            input: Some(
                repository
                    .recovery_event_evidence(&chain, &input.event_id)
                    .expect("test: input evidence"),
            ),
            terminal: Some(
                repository
                    .recovery_event_evidence(&chain, &final_event.event_id)
                    .expect("test: terminal evidence"),
            ),
            facts: bytes,
        })
        .expect("test: settle");
    ticket.finish(true);
    drop(recorder);
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = prepare_recovery(&generation, Arc::clone(&memory), &tracker).await;
    SettledSetup {
        dir,
        workspace,
        memory,
        recovery,
        tracker,
        chain,
        input_id: input.event_id,
        final_id: final_event.event_id,
        key_id: pending.key_id,
    }
}

fn default_facts() -> SettledTerminalFacts {
    settled_facts("prx_run", "", "", "")
}

fn is_mismatch(error: &anyhow::Error, field: &'static str) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<IngressAttestationError>()
            .is_some_and(|attestation| *attestation == IngressAttestationError::Mismatch(field))
            || cause.to_string().contains(&format!("`{field}`"))
    })
}

fn is_unknown_key(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<IngressAttestationError>() && cause.to_string().contains("unknown key"))
}

fn sqlite(workspace: &Path) -> SqliteMemory {
    SqliteMemory::new(workspace).expect("test: sqlite accessor")
}

pub(super) async fn recovery_settled_receipt_each_bound_field_is_refused() {
    let workspace_cell = settled_setup_with(default_facts(), None).await;
    let (other_dir, other_ws) = home_workspace();
    let mut other_config = cell_config(&other_ws, true);
    other_config.workspace_dir = other_ws.clone();
    let generation = crate::config::ConfigGenerationManager::new(other_config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let foreign = prepare_recovery(&generation, Arc::clone(&workspace_cell.memory), &tracker).await;
    let error = foreign
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("workspace");
    assert!(
        error.to_string().contains("committed terminal marker is missing") || is_mismatch(&error, "workspace"),
        "workspace: {error}"
    );
    drop(other_dir);

    let owner_cell = settled_setup_with(default_facts(), None).await;
    let error = owner_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_other")
        .await
        .expect_err("owner");
    assert!(
        error
            .to_string()
            .contains("terminal replay scope does not match its owner")
            || is_mismatch(&error, "owner"),
        "owner: {error}"
    );

    let source_cell = settled_setup_with(default_facts(), None).await;
    sqlite(&source_cell.workspace)
        .replace_event_source_for_test("prx_run", "channel")
        .expect("test: source");
    let error = source_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("source");
    assert!(
        error.chain().any(|cause| cause.is::<LegacyUnconfirmed>())
            || is_mismatch(&error, "source")
            || error.to_string().contains("Query returned no rows"),
        "source: {error}"
    );

    let run_cell = settled_setup_with(default_facts(), None).await;
    let error = run_cell
        .recovery
        .rejoin_terminal("prx_other", "prx_user")
        .await
        .expect_err("run");
    assert!(
        error.to_string().contains("committed terminal marker is missing") || is_mismatch(&error, "run"),
        "run: {error}"
    );

    let mut input_facts = default_facts();
    input_facts.claim_event_id = uuid::Uuid::new_v4().to_string();
    let input_cell = settled_setup_with(input_facts, None).await;
    let error = input_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("input event");
    assert!(
        is_mismatch(&error, "claim_event_id")
            || error.to_string().contains("QueryReturnedNoRows")
            || error.to_string().contains("no rows"),
        "input event: {error:#}"
    );

    let mut terminal_facts = default_facts();
    terminal_facts.final_event_id = uuid::Uuid::new_v4().to_string();
    let terminal_cell = settled_setup_with(terminal_facts, None).await;
    let error = terminal_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("terminal event");
    assert!(
        is_mismatch(&error, "final_event_id")
            || error.to_string().contains("QueryReturnedNoRows")
            || error.to_string().contains("no rows"),
        "terminal event: {error:#}"
    );

    let bytes_cell = settled_setup_with(default_facts(), None).await;
    sqlite(&bytes_cell.workspace)
        .replace_event_content_for_test(&bytes_cell.final_id, "mutated")
        .expect("test: terminal bytes");
    let error = bytes_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("terminal bytes");
    assert!(
        error.to_string().contains("recovery evidence changed") || error.to_string().contains("invalid recovery"),
        "terminal bytes: {error:#}"
    );

    let mut key_facts = default_facts();
    key_facts.key_id = uuid::Uuid::nil().to_string();
    let key_cell = settled_setup_with(key_facts, None).await;
    let error = key_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("key id");
    assert!(is_mismatch(&error, "key_id"), "key id: {error:#}");

    let relabel = settled_setup_with(default_facts(), None).await;
    let relabel_err = sqlite(&relabel.workspace)
        .replace_recovery_attestation_state_for_test("pending")
        .expect_err("state relabel");
    assert!(
        relabel_err.to_string().contains("immutable recovery receipt"),
        "state relabel: {relabel_err}"
    );

    let mut unknown = serde_json::to_value(default_facts()).expect("test: facts value");
    unknown
        .as_object_mut()
        .expect("test: facts object")
        .insert("extra".into(), serde_json::json!(1));
    let unknown_cell = settled_setup_with(
        default_facts(),
        Some(serde_json::to_vec(&unknown).expect("test: extra")),
    )
    .await;
    let error = unknown_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("facts unknown key");
    assert!(is_unknown_key(&error), "facts unknown key: {error:#}");

    let mut surface_facts = default_facts();
    surface_facts.surface = "channel".into();
    let surface_cell = settled_setup_with(surface_facts, None).await;
    let error = surface_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("surface");
    assert!(is_mismatch(&error, "surface"), "surface: {error:#}");

    let mut parent_facts = default_facts();
    parent_facts.parent_authority_id = Some(uuid::Uuid::nil().to_string());
    let parent_cell = settled_setup_with(parent_facts, None).await;
    let error = parent_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("parent_authority_id");
    assert!(
        is_mismatch(&error, "parent_authority_id"),
        "parent_authority_id: {error:#}"
    );

    let mut skill = serde_json::to_value(default_facts()).expect("test: skill value");
    skill
        .get_mut("skill_observation")
        .and_then(serde_json::Value::as_object_mut)
        .expect("test: skill object")
        .insert("extra".into(), serde_json::json!(false));
    let skill_cell = settled_setup_with(
        default_facts(),
        Some(serde_json::to_vec(&skill).expect("test: skill extra")),
    )
    .await;
    let error = skill_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("skill_observation");
    assert!(is_unknown_key(&error), "skill_observation: {error:#}");

    let _keep = (
        &workspace_cell.dir,
        &workspace_cell.tracker,
        workspace_cell.input_id,
        workspace_cell.final_id,
        workspace_cell.key_id,
        workspace_cell.chain,
    );
}

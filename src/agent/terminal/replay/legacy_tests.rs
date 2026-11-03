#![cfg(test)]
//! Committed v1 fixture captured before removal; only isolated read-side state.
use super::*;
use crate::memory::SqliteMemory;
use crate::memory::traits::RouterLearningEvidence;
use base64::Engine;

#[derive(Deserialize)]
struct Fixture {
    source_revision: String,
    workspace_id: String,
    memory_database: String,
    decision_database: String,
    synthetic_key: Vec<u8>,
    costs: String,
    reservations: String,
    owner_id: String,
    terminal_id: String,
    reply: String,
    served_model: String,
}

struct Restored {
    workspace: tempfile::TempDir,
    fixture: Fixture,
    memory: SqliteMemory,
    store: Arc<crate::causal_tree::store::CausalDecisionStore>,
    key: Arc<DecisionKey>,
}

impl Restored {
    fn load() -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let fixture: Fixture = serde_json::from_str(include_str!("../../../../tests/fixtures/served-reply-v1.json"))?;
        assert_eq!(fixture.source_revision, "25c76043fe8a8969befeab2c7cc3e4b6bdc32701");
        let workspace = tempfile::tempdir()?;
        for dir in ["memory", "self/causal_tree", "state"] {
            std::fs::create_dir_all(workspace.path().join(dir))?;
        }
        std::fs::set_permissions(
            workspace.path().join("self/causal_tree"),
            std::fs::Permissions::from_mode(0o700),
        )?;
        for (path, encoded) in [
            ("memory/brain.db", &fixture.memory_database),
            ("self/causal_tree/decisions.db", &fixture.decision_database),
        ] {
            std::fs::write(
                workspace.path().join(path),
                base64::engine::general_purpose::STANDARD.decode(encoded)?,
            )?;
        }
        let key_path = workspace.path().join("self/causal_tree/.decision_key");
        std::fs::write(&key_path, &fixture.synthetic_key)?;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(workspace.path().join("state/costs.jsonl"), &fixture.costs)?;
        std::fs::write(
            workspace.path().join("state/costs.reservations.jsonl"),
            &fixture.reservations,
        )?;
        let key = DecisionKey::load_existing(workspace.path())?;
        let memory = SqliteMemory::new(workspace.path())?;
        let store = crate::causal_tree::store::CausalDecisionStore::open_unshared_read_only(workspace.path());
        Ok(Self {
            workspace,
            fixture,
            memory,
            store,
            key,
        })
    }
    async fn terminal(&self) -> anyhow::Result<MessageEvent> {
        self.memory
            .find_message_event_by_idempotency_key(
                &self.fixture.workspace_id,
                &format!("turn:{}:final", self.fixture.terminal_id),
            )
            .await?
            .context("pre-removal committed terminal")
    }
    async fn verify(&self, terminal: &MessageEvent) -> anyhow::Result<VerifiedTerminalReplay> {
        verify_terminal_replay(
            &self.memory,
            &self.store.reader(),
            &self.key,
            &self.store.workspace_id().context("fixture store UUID")?,
            &self.fixture.workspace_id,
            &self.fixture.owner_id,
            &self.fixture.terminal_id,
            terminal,
        )
        .await
    }
}

#[tokio::test]
async fn retired_served_reply_v1_replays_same_reply_model_and_usage_without_new_execution() -> anyhow::Result<()> {
    let restored = Restored::load()?;
    let terminal = restored.terminal().await?;
    let verified = Box::pin(restored.verify(&terminal)).await?;
    assert_eq!(
        verified.assistant().context("committed assistant")?.content,
        restored.fixture.reply
    );
    let payload: Value = serde_json::from_str(terminal.raw_payload_json.as_deref().context("payload")?)?;
    assert_eq!(
        payload.pointer("/history/assistant_content").and_then(Value::as_str),
        Some(restored.fixture.reply.as_str())
    );
    let source = verified.source();
    let evidence = RouterLearningEvidence::from_terminal(
        &terminal,
        source.event_id.clone(),
        &source.content,
        source.raw_payload_json.as_deref(),
        terminal.config_generation_id.context("generation")?,
        terminal.config_source_revision.as_deref().context("revision")?,
        "0".repeat(64),
    )?;
    assert_eq!(evidence.served().1, restored.fixture.served_model);
    assert_eq!(
        payload.pointer("/provider_outcome/final_model").and_then(Value::as_str),
        Some("prx-cheap")
    );
    // No provider/engine is constructed. This API can only authenticate stored
    // facts; readback neither re-executes the turn nor settles costs again.
    Box::pin(restored.verify(&terminal)).await?;
    for (path, expected) in [
        ("state/costs.jsonl", &restored.fixture.costs),
        ("state/costs.reservations.jsonl", &restored.fixture.reservations),
    ] {
        assert_eq!(
            std::fs::read_to_string(restored.workspace.path().join(path))?,
            *expected
        );
    }
    assert_eq!(restored.fixture.costs.lines().count(), 2);
    Ok(())
}

#[tokio::test]
async fn retired_served_reply_v1_rejects_changed_authenticated_bindings() -> anyhow::Result<()> {
    let restored = Restored::load()?;
    let terminal = restored.terminal().await?;
    let verified = Box::pin(restored.verify(&terminal)).await?;
    let payload: Value = serde_json::from_str(terminal.raw_payload_json.as_deref().context("payload")?)?;
    for pointer in [
        "/terminal_replay_v1/served_auxiliary/event/mac",
        "/terminal_replay_v1/served_auxiliary/persisted_reply_sha256",
        "/served_auxiliary/provider_outcome/final_model",
        "/terminal_replay_v1/owner_id",
    ] {
        let mut changed = payload.clone();
        *changed.pointer_mut(pointer).context("binding under test")? = Value::String("0".repeat(64));
        // Keep the outer terminal signature valid: the reader must check the
        // inner event authentication, exact digest, owner and model projection.
        reseal_payload_for_test(&restored.key, &mut changed, verified.assistant())?;
        let mut tampered = terminal.clone();
        tampered.raw_payload_json = Some(serde_json::to_string(&changed)?);
        assert!(
            Box::pin(restored.verify(&tampered)).await.is_err(),
            "reader accepted changed {pointer}"
        );
    }
    Ok(())
}

//! Test-mock-only receiving process for C5b crash proofs. Real store paths.
use super::{SqliteMailbox, pin::scope_from_row};
use crate::memory::sqlite::SqliteMemory;
use crate::memory::{AgentPartition, PartitionKey};
use crate::messaging::mailbox::{MailboxAuthority, SystemMailboxClock};
use crate::messaging::{IdempotencyKey, MessageBody, MessageKind, PrxMessage, SessionAddr};
use rusqlite::{Transaction, TransactionBehavior};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

pub fn accept_adopt(workspace: &Path) -> anyhow::Result<()> {
    let memory = SqliteMemory::new(workspace)?;
    let workspace_id = workspace.to_string_lossy().into_owned();
    let owner = "prx_harness_owner";
    let source = SessionAddr::new("prx_harness", owner, "source", None);
    let target = SessionAddr::new("prx_harness", owner, "target", None);
    let scope = {
        let writer = memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let partition = AgentPartition {
            workspace_id: workspace_id.clone(),
            owner_id: owner.into(),
            key: PartitionKey::Main,
        };
        let id =
            crate::memory::sqlite::agent_migration::resolve_write_scope(&tx, &partition, Some("prx_harness_target"))?;
        let (inc, generation): (String, i64) = tx.query_row(
            "SELECT incarnation,generation FROM agent_scopes WHERE scope_id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let scope = scope_from_row(workspace_id.clone(), owner.into(), id, inc, generation)?;
        tx.commit()?;
        scope
    };
    let store = SqliteMailbox::new(
        memory,
        "prx_harness".into(),
        [31; 32],
        Arc::new(SystemMailboxClock),
        crate::config::schema::RemoteTreeLimits::default(),
    )?;
    let authority = MailboxAuthority::local(workspace_id, source.clone(), target.clone())?;
    let request_path = workspace.join("mailbox-harness-request.json");
    let message: PrxMessage = if request_path.exists() {
        serde_json::from_slice(&std::fs::read(&request_path)?)?
    } else {
        let pin = store.recipient_pin(&authority, &scope, "prx_harness_target")?;
        let id = Uuid::from_u128(1001);
        let message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse("prx-harness-message-0001")?,
            stream_id: Some(Uuid::from_u128(500)),
            seq: 1,
            ts: 1000,
            from: source,
            to: target.clone(),
            to_pin: pin,
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime: "text/plain".into(),
                bytes: b"crash proof input".to_vec(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        std::fs::write(&request_path, serde_json::to_vec(&message)?)?;
        message
    };
    let _execution = store.execution_guard(&scope, &target, "prx_harness_run".into())?;
    let receipt = store.accept(&authority, message)?;
    if receipt.state != crate::messaging::DeliveryState::Consumed {
        store.recover_delivery(&scope, &target)?;
        let delivery = store
            .deliver(&scope, &target)?
            .ok_or_else(|| anyhow::anyhow!("harness delivery missing"))?;
        store.adopt(&delivery)?;
    }
    Ok(())
}

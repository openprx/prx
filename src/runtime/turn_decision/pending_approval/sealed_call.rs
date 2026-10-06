//! Recoverable original calls, encrypted under a domain-separated workspace
//! decision key. The outer pending MAC also authenticates the ciphertext.
use super::{PendingRecord, TurnDecisionService, VerifiedPendingDecisionApproval};
use anyhow::Context;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EncryptedCall {
    version: u8,
    nonce: [u8; 12],
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCall {
    tool: String,
    arguments: serde_json::Value,
}

fn associated_data(record: &PendingRecord) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        "prx.approval.call.v1",
        &record.subject,
        &record.binding,
        &record.source_event_id,
        &record.run_id,
    ))?)
}

impl TurnDecisionService {
    fn pending_call_cipher(&self) -> anyhow::Result<ChaCha20Poly1305> {
        anyhow::ensure!(self.durable_key, "approval call encryption requires a durable key");
        let key = Zeroizing::new(
            self.state_key
                .hmac(crate::decision::KeyDomain::ApprovalState, &[b"call-encryption-key-v1"]),
        );
        Ok(ChaCha20Poly1305::new(Key::from_slice(key.as_ref())))
    }

    pub(super) fn seal_pending_call(
        &self,
        record: &PendingRecord,
        command: &crate::tools::ToolExecutionCommand,
    ) -> anyhow::Result<EncryptedCall> {
        let plain = Zeroizing::new(serde_json::to_vec(&StoredCall {
            tool: command.name.clone(),
            arguments: command.arguments.clone(),
        })?);
        let aad = associated_data(record)?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .pending_call_cipher()?
            .encrypt(
                &nonce,
                Payload {
                    msg: plain.as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("approval call encryption failed"))?;
        Ok(EncryptedCall {
            version: 1,
            nonce: nonce.into(),
            ciphertext: hex::encode(ciphertext),
        })
    }

    /// Input must already have passed pending/source/decision authentication.
    /// Decryption grants no authority; operation, expiry and final consumption
    /// still belong to the ordinary resume path.
    pub(super) fn open_pending_call(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<crate::tools::ToolExecutionCommand> {
        use crate::tools::capability::execution_binding::{CallDigestSource, decision_call_digest};
        let record = &pending.record;
        let sealed = record
            .sealed_call
            .as_ref()
            .context("original approval call is unavailable")?;
        anyhow::ensure!(sealed.version == 1, "unsupported approval call encoding");
        let aad = associated_data(record)?;
        let ciphertext = hex::decode(&sealed.ciphertext).context("invalid approval call encoding")?;
        let plain = Zeroizing::new(
            self.pending_call_cipher()?
                .decrypt(
                    Nonce::from_slice(&sealed.nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| anyhow::anyhow!("approval call authentication failed"))?,
        );
        let call: StoredCall =
            serde_json::from_slice(plain.as_slice()).map_err(|_| anyhow::anyhow!("invalid stored approval call"))?;
        let entry = self
            .catalog
            .entry_for_public_name(&call.tool)
            .context("approval capability unavailable")?;
        let [(id, fingerprint, generation)] = record.capabilities.as_slice() else {
            anyhow::bail!("stored approval call requires one capability");
        };
        let [operation] = record.op_ids.as_slice() else {
            anyhow::bail!("stored approval call requires one operation");
        };
        anyhow::ensure!(
            id == entry.id.as_str() && *fingerprint == entry.fingerprint.0 && *generation == entry.generation.0,
            "stored approval capability changed"
        );
        let capability = crate::causal_tree::branch::CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let source = &pending.source;
        let digest = decision_call_digest(
            &record.binding.decision_id,
            &capability,
            CallDigestSource {
                workspace: &record.subject.workspace_id,
                owner: &record.subject.owner_id,
                session: record
                    .subject
                    .session_key
                    .as_deref()
                    .context("approval session missing")?,
                run: source.run_id.as_deref(),
                event: Some(&source.event_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            &call.arguments,
            operation,
            record.approval_risk()?,
        )?;
        anyhow::ensure!(
            digest == record.binding.call_digest,
            "stored approval call digest differs"
        );
        Ok(crate::tools::ToolExecutionCommand::new(call.tool, call.arguments))
    }
}

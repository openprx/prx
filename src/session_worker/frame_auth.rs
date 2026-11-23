//! Run-bound, ordered authentication for mailbox frames on inherited pipes.
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub struct FrameAuth {
    key: Arc<ring::hmac::Key>,
    run: String,
    send: Arc<AtomicU64>,
    receive: Arc<AtomicU64>,
    outgoing: &'static str,
    incoming: &'static str,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedFrame {
    kind: String,
    run: String,
    sequence: u64,
    frame: serde_json::Value,
    mac: String,
}
impl FrameAuth {
    pub(crate) fn parent(run: &str) -> Result<Self> {
        Ok(Self::new(
            crate::session_worker::transport_key::TransportSecret::for_launch()?.hmac_key(),
            run,
            false,
        ))
    }
    pub(crate) fn worker(key: ring::hmac::Key, run: &str) -> Self {
        Self::new(key, run, true)
    }
    fn new(key: ring::hmac::Key, run: &str, worker: bool) -> Self {
        Self {
            key: Arc::new(key),
            run: run.into(),
            send: Arc::new(AtomicU64::new(0)),
            receive: Arc::new(AtomicU64::new(0)),
            outgoing: if worker { "worker" } else { "parent" },
            incoming: if worker { "parent" } else { "worker" },
        }
    }
    fn payload(&self, direction: &str, sequence: u64, value: &serde_json::Value) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "prx-mailbox-frame-v1",
            direction,
            &self.run,
            sequence,
            value,
        ))?)
    }
    pub(crate) fn sign(&self, value: &impl Serialize) -> Result<serde_json::Value> {
        let frame = serde_json::to_value(value)?;
        let previous = self
            .send
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("worker frame sequence exhausted"))?;
        let sequence = previous + 1;
        let mac = ring::hmac::sign(&self.key, &self.payload(self.outgoing, sequence, &frame)?);
        Ok(serde_json::to_value(SignedFrame {
            kind: "authenticated_mailbox".into(),
            run: self.run.clone(),
            sequence,
            frame,
            mac: base64::engine::general_purpose::STANDARD.encode(mac.as_ref()),
        })?)
    }
    pub(crate) fn verify(&self, value: serde_json::Value) -> Result<serde_json::Value> {
        let signed: SignedFrame =
            serde_json::from_value(value).context("worker mailbox frame authentication required")?;
        ensure!(
            signed.kind == "authenticated_mailbox" && signed.run == self.run,
            "worker frame run mismatch"
        );
        let previous = self.receive.load(Ordering::SeqCst);
        ensure!(
            previous.checked_add(1) == Some(signed.sequence),
            "worker frame sequence mismatch"
        );
        let mac = base64::engine::general_purpose::STANDARD.decode(&signed.mac)?;
        ring::hmac::verify(
            &self.key,
            &self.payload(self.incoming, signed.sequence, &signed.frame)?,
            &mac,
        )
        .map_err(|_| anyhow::anyhow!("worker frame MAC mismatch"))?;
        self.receive
            .compare_exchange(previous, signed.sequence, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow::anyhow!("concurrent worker frame reader"))?;
        Ok(signed.frame)
    }
}

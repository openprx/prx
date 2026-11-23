//! Connections indexed by certificate, with a shared limit per SPIFFE peer.
use crate::config::{A2aConfig, A2aPeer};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
pub(super) struct Connections {
    state: parking_lot::Mutex<State>,
}
#[derive(Default)]
struct State {
    counts: HashMap<String, usize>,
    certificates: HashMap<String, HashMap<uuid::Uuid, Connection>>,
}
struct Connection {
    peer: A2aPeer,
    local_fingerprint: String,
    cancellation: CancellationToken,
}
pub(super) struct Permit {
    registry: Arc<Connections>,
    peer: String,
    fingerprint: String,
    id: uuid::Uuid,
    cancellation: CancellationToken,
}
impl Permit {
    pub(super) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.registry.state.lock();
        if let Some(count) = state.counts.get_mut(&self.peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.counts.remove(&self.peer);
            }
        }
        if let Some(connections) = state.certificates.get_mut(&self.fingerprint) {
            connections.remove(&self.id);
            if connections.is_empty() {
                state.certificates.remove(&self.fingerprint);
            }
        }
    }
}
impl Connections {
    pub(super) fn register(
        self: &Arc<Self>,
        peer: A2aPeer,
        fingerprint: String,
        local_fingerprint: String,
    ) -> Option<Permit> {
        let mut state = self.state.lock();
        let count = state.counts.entry(peer.spiffe_id.clone()).or_default();
        if *count >= 8 {
            return None;
        }
        *count += 1;
        let id = uuid::Uuid::new_v4();
        let cancellation = CancellationToken::new();
        let name = peer.spiffe_id.clone();
        state.certificates.entry(fingerprint.clone()).or_default().insert(
            id,
            Connection {
                peer,
                local_fingerprint,
                cancellation: cancellation.clone(),
            },
        );
        Some(Permit {
            registry: self.clone(),
            peer: name,
            fingerprint,
            id,
            cancellation,
        })
    }
    pub(super) fn recheck(&self, config: Option<&A2aConfig>, local_fingerprint: Option<&str>) {
        let state = self.state.lock();
        for (fingerprint, connections) in &state.certificates {
            for entry in connections.values() {
                let peer = config.and_then(|config| {
                    config
                        .peers
                        .values()
                        .find(|peer| peer.fingerprints.contains(fingerprint) && peer.spiffe_id == entry.peer.spiffe_id)
                });
                if peer != Some(&entry.peer) || local_fingerprint != Some(entry.local_fingerprint.as_str()) {
                    entry.cancellation.cancel();
                }
            }
        }
    }
}

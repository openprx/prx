//! One revocable HTTP client per pinned peer and local key generation.
use super::tls::{self, Trust};
use crate::config::A2aPeer;
use anyhow::Result;
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub(crate) struct Clients {
    trust: Trust,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    states: Arc<Mutex<HashMap<String, &'static str>>>,
}
struct Entry {
    peer: A2aPeer,
    local_fingerprint: String,
    client: reqwest::Client,
    cancellation: CancellationToken,
}
impl Clients {
    pub(crate) fn new(trust: Trust) -> Self {
        Self {
            trust,
            entries: Default::default(),
            states: Default::default(),
        }
    }
    pub(crate) fn get(&self, peer: &A2aPeer) -> Result<(reqwest::Client, CancellationToken)> {
        let local = self.trust.identity()?;
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get(&peer.spiffe_id) {
            if same(&entry.peer, peer) && entry.local_fingerprint == local.fingerprint {
                return Ok((entry.client.clone(), entry.cancellation.clone()));
            }
            entry.cancellation.cancel();
        }
        let client = tls::client(self.trust.clone(), peer)?;
        let cancellation = CancellationToken::new();
        entries.insert(
            peer.spiffe_id.clone(),
            Entry {
                peer: peer.clone(),
                local_fingerprint: local.fingerprint.clone(),
                client: client.clone(),
                cancellation: cancellation.clone(),
            },
        );
        Ok((client, cancellation))
    }
    pub(crate) fn monitor(&self) -> tokio_util::sync::DropGuard {
        let stop = CancellationToken::new();
        let guard = stop.clone().drop_guard();
        let cache = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    ()=stop.cancelled()=>break,
                    _=interval.tick()=> {
                        let _ = cache.trust.refresh_async().await;
                        let config=cache.trust.current();let local=cache.trust.identity();
                        cache.states.lock().retain(|spiffe,_|config.as_ref().is_ok_and(|config|config.peers.values().any(|peer|&peer.spiffe_id==spiffe)));
                        cache.entries.lock().retain(|spiffe,entry| {
                            let current=config.as_ref().ok().and_then(|config|config.peers.values().find(|peer|&peer.spiffe_id==spiffe));
                            let valid=current.is_some_and(|peer|same(&entry.peer,peer)) && local.as_ref().is_ok_and(|identity|identity.fingerprint==entry.local_fingerprint);
                            if !valid {entry.cancellation.cancel();}valid
                        });
                    }
                }
            }
            for entry in cache.entries.lock().values() {
                entry.cancellation.cancel();
            }
            cache.entries.lock().clear();
        });
        guard
    }
    pub(crate) fn state(&self, peer: &A2aPeer, state: &'static str) {
        if self.states.lock().insert(peer.spiffe_id.clone(), state) != Some(state) {
            let fingerprint = peer
                .fingerprints
                .first()
                .and_then(|value| value.get(..16))
                .unwrap_or_default();
            tracing::info!(spiffe_id=%peer.spiffe_id,fingerprint,state,"A2A peer delivery state changed");
        }
    }
}
fn same(left: &A2aPeer, right: &A2aPeer) -> bool {
    left.spiffe_id == right.spiffe_id
        && left.fingerprints == right.fingerprints
        && left.address == right.address
        && left.owner == right.owner
        && left.sessions == right.sessions
        && left.agents == right.agents
}

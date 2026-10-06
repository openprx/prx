//! The one decision cache (CDP-PLAN 1.5 M8; SP-CDP-A 5.6; SP-CDP-C V7.06).
//!
//! A bounded LRU of validated decisions on a `parking_lot` mutex, with no
//! new dependency. An entry lives [`CACHE_TTL`] and is keyed by the
//! request fingerprint (protocol, state fingerprint, candidate-set hash,
//! policy, catalog, questions and omissions) together with the backend
//! identity, the configured model and the configuration generation, so any
//! change to one of them misses. A request with a pending approval is never
//! cached (the manifest says so), and only validated decisions are stored.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use super::protocol::{BackendIdentity, DecisionRequest};
use super::validate::ValidatedDecision;

/// How many decisions the cache holds.
pub const CACHE_CAPACITY: usize = 256;

/// How long a cached decision is served.
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// The key of one cached decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    /// The key of `request` answered by `identity` in configuration
    /// generation `generation`.
    #[must_use]
    pub fn new(request: &DecisionRequest, identity: &BackendIdentity, generation: u64) -> Self {
        let mut hasher = Sha256::new();
        let mut part = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        };
        part(b"prx.decision.cache.v1");
        part(request.manifest().request_fingerprint());
        part(identity.kind.as_str().as_bytes());
        part(
            identity
                .provider
                .as_ref()
                .map_or("", |provider| provider.as_str())
                .as_bytes(),
        );
        part(identity.configured_model.as_bytes());
        part(&generation.to_be_bytes());
        Self(hasher.finalize().into())
    }
}

#[derive(Debug)]
struct Entry {
    decision: ValidatedDecision,
    stored_at: Instant,
    used: u64,
}

#[derive(Debug, Default)]
struct Entries {
    map: HashMap<CacheKey, Entry>,
    tick: u64,
}

/// The decision cache of one factory.
#[derive(Debug, Default)]
pub struct DecisionCache {
    entries: Mutex<Entries>,
}

impl DecisionCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The decision cached under `key`, if it is younger than
    /// [`CACHE_TTL`] at `now`. An expired entry is removed.
    #[must_use]
    pub fn get(&self, key: &CacheKey, now: Instant) -> Option<ValidatedDecision> {
        let mut entries = self.entries.lock();
        entries.tick += 1;
        let tick = entries.tick;
        let fresh = entries
            .map
            .get(key)
            .map(|entry| now.saturating_duration_since(entry.stored_at) < CACHE_TTL)?;
        if !fresh {
            entries.map.remove(key);
            return None;
        }
        let entry = entries.map.get_mut(key)?;
        entry.used = tick;
        Some(entry.decision.clone())
    }

    /// Cache `decision` under `key` at `now`, evicting the least recently
    /// used entry when full.
    pub fn put(&self, key: CacheKey, decision: ValidatedDecision, now: Instant) {
        let mut entries = self.entries.lock();
        entries.tick += 1;
        let tick = entries.tick;
        if !entries.map.contains_key(&key) && entries.map.len() >= CACHE_CAPACITY {
            let oldest = entries
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(oldest, _)| *oldest);
            if let Some(oldest) = oldest {
                entries.map.remove(&oldest);
            }
        }
        entries.map.insert(
            key,
            Entry {
                decision,
                stored_at: now,
                used: tick,
            },
        );
    }

    /// How many decisions are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

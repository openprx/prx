//! Delivery-level deduplication for platform-pushed channel webhooks.
//!
//! Every platform that pushes messages over HTTP also redelivers them: Meta
//! retries a WhatsApp webhook "with decreasing frequency until the request
//! succeeds, for up to 7 days", Linq retries "up to 10 times over ~25 minutes",
//! and Slack-style senders retry three times. A redelivery carries the *same*
//! payload, so without a ledger the agent runs the same turn again and commits
//! its side effects a second time.
//!
//! Answering fast makes redelivery rare; it does not make it impossible. A
//! network drop after the handler wrote its response, a proxy that times out in
//! front of an otherwise healthy gateway, or a restart mid-request all produce a
//! retry of work that already started. This ledger is what makes that retry a
//! no-op.
//!
//! The unit is the *delivery*, not the message: a redelivery is byte-identical
//! to the original, while two genuine messages differ in at least their platform
//! message id and timestamp. Keying on the payload digest therefore needs no
//! per-platform id extraction and cannot be defeated by a channel parser that
//! discards the platform id (`whatsapp.rs` and `linq.rs` both mint a fresh UUID
//! per parsed message, so `ChannelMessage::id` is useless for this).
//!
//! A claim resolves the same way a gateway idempotency claim does: work that
//! never reached a side effect releases the key, so a genuine retry may run;
//! work that did reach one keeps the key, because nothing here knows how far it
//! got and "not known to be safe" must not become "safe to repeat".

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Ceiling on retained delivery keys. Entries are a digest plus a timestamp, so
/// this is cheap; it exists to bound an idle process, not to bound traffic.
pub(super) const MAX_DELIVERY_KEYS: usize = 10_000;

/// Digest identifying one inbound delivery, scoped by channel so two channels
/// cannot collide on an identical body.
pub(super) fn delivery_key(channel: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(channel.as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hex::encode(hasher.finalize())
}

#[derive(Debug)]
enum DeliveryEntry {
    /// A claim is live. `generation` fences a stale claim's resolution against
    /// a newer one that reused the key after expiry.
    InFlight { generation: u64 },
    /// The delivery reached observable work; redeliveries are dropped until the
    /// entry expires.
    Handled { at: Instant },
}

#[derive(Debug, Default)]
struct LedgerState {
    entries: HashMap<String, DeliveryEntry>,
    next_generation: u64,
}

/// Process-wide ledger of inbound deliveries.
#[derive(Debug, Default)]
struct DeliveryLedger {
    state: Mutex<LedgerState>,
}

static LEDGER: LazyLock<DeliveryLedger> = LazyLock::new(DeliveryLedger::default);

/// Outcome of asking the ledger whether a delivery is new.
#[derive(Debug)]
pub(super) enum DeliveryOutcome {
    /// First sighting: the caller owns the claim and must do the work.
    Fresh(DeliveryClaim),
    /// The same payload is already in flight or was already handled.
    Duplicate,
}

/// Ownership token for one accepted delivery.
#[derive(Debug)]
pub(super) struct DeliveryClaim {
    key: String,
    generation: u64,
    /// Whether this attempt reached work that can produce observable side
    /// effects — memory writes, tool calls, outbound messages.
    dispatched: bool,
}

impl DeliveryClaim {
    /// Record that the delivery is about to start doing observable work.
    ///
    /// Call this immediately before the first side effect: everything up to that
    /// point is exactly what makes a platform retry safe to re-run.
    pub(super) const fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }
}

impl Drop for DeliveryClaim {
    /// Resolution is deliberately in `Drop` rather than an explicit call, so a
    /// cancelled job, an early return, or a panic all resolve the key the same
    /// way a normal completion does — never by leaving it in flight forever.
    fn drop(&mut self) {
        let mut state = LEDGER.state.lock();
        let still_ours = matches!(
            state.entries.get(&self.key),
            Some(DeliveryEntry::InFlight { generation }) if *generation == self.generation
        );
        if !still_ours {
            return;
        }
        if self.dispatched {
            state
                .entries
                .insert(self.key.clone(), DeliveryEntry::Handled { at: Instant::now() });
        } else {
            state.entries.remove(&self.key);
        }
    }
}

/// Claim a delivery, or report it as a redelivery of one already seen.
///
/// `ttl` bounds how long a handled delivery is remembered; a redelivery that
/// arrives after it has expired is indistinguishable from a new message and will
/// run again.
pub(super) fn claim(key: String, ttl: Duration) -> DeliveryOutcome {
    let now = Instant::now();
    let mut state = LEDGER.state.lock();

    state.entries.retain(|_, entry| match entry {
        DeliveryEntry::InFlight { .. } => true,
        DeliveryEntry::Handled { at } => now.saturating_duration_since(*at) < ttl,
    });

    if state.entries.contains_key(&key) {
        return DeliveryOutcome::Duplicate;
    }

    while state.entries.len() >= MAX_DELIVERY_KEYS {
        let Some(oldest) = state
            .entries
            .iter()
            .filter_map(|(key, entry)| match entry {
                DeliveryEntry::Handled { at } => Some((key.clone(), *at)),
                DeliveryEntry::InFlight { .. } => None,
            })
            .min_by_key(|(_, at)| *at)
            .map(|(key, _)| key)
        else {
            // Everything retained is still in flight. Refusing here would drop
            // live traffic to protect a bound that live work already justifies,
            // so admit the delivery and let the entry count follow real
            // concurrency.
            break;
        };
        state.entries.remove(&oldest);
    }

    let generation = state.next_generation.wrapping_add(1);
    state.next_generation = generation;
    state
        .entries
        .insert(key.clone(), DeliveryEntry::InFlight { generation });
    DeliveryOutcome::Fresh(DeliveryClaim {
        key,
        generation,
        dispatched: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_key(tag: &str) -> String {
        delivery_key(tag, uuid::Uuid::new_v4().to_string().as_bytes())
    }

    fn expect_fresh(outcome: DeliveryOutcome) -> DeliveryClaim {
        match outcome {
            DeliveryOutcome::Fresh(claim) => claim,
            DeliveryOutcome::Duplicate => panic!("expected a fresh delivery claim"),
        }
    }

    #[test]
    fn delivery_key_is_scoped_by_channel() {
        let whatsapp = delivery_key("whatsapp", b"{\"a\":1}");
        let linq = delivery_key("linq", b"{\"a\":1}");
        assert_ne!(whatsapp, linq);
        assert_eq!(whatsapp.len(), 64);
    }

    #[test]
    fn redelivery_while_in_flight_is_a_duplicate() {
        let key = unique_key("in-flight");
        let held = expect_fresh(claim(key.clone(), Duration::from_mins(1)));
        assert!(matches!(claim(key, Duration::from_mins(1)), DeliveryOutcome::Duplicate));
        drop(held);
    }

    #[test]
    fn dispatched_delivery_stays_remembered_after_completion() {
        let key = unique_key("dispatched");
        let mut held = expect_fresh(claim(key.clone(), Duration::from_mins(1)));
        held.mark_dispatched();
        drop(held);

        assert!(matches!(claim(key, Duration::from_mins(1)), DeliveryOutcome::Duplicate));
    }

    #[test]
    fn undispatched_delivery_releases_the_key_for_a_genuine_retry() {
        let key = unique_key("undispatched");
        let held = expect_fresh(claim(key.clone(), Duration::from_mins(1)));
        drop(held);

        // Nothing observable happened, so the platform's retry must be allowed
        // to run the work rather than being swallowed as a duplicate.
        let retry = expect_fresh(claim(key, Duration::from_mins(1)));
        drop(retry);
    }

    #[test]
    fn handled_entry_expires_with_the_ttl() {
        let key = unique_key("expiring");
        let mut first = expect_fresh(claim(key.clone(), Duration::from_mins(1)));
        first.mark_dispatched();
        drop(first);

        // A zero TTL retires the handled entry on the very next claim.
        let retry = expect_fresh(claim(key, Duration::ZERO));
        drop(retry);
    }

    #[test]
    fn distinct_payloads_never_collide() {
        let first = delivery_key("whatsapp", b"{\"id\":\"wamid.A\"}");
        let second = delivery_key("whatsapp", b"{\"id\":\"wamid.B\"}");
        let first_claim = expect_fresh(claim(first, Duration::from_mins(1)));
        let second_claim = expect_fresh(claim(second, Duration::from_mins(1)));
        drop(first_claim);
        drop(second_claim);
    }
}

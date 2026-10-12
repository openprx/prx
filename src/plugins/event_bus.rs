//! In-process Event Bus for WASM plugin inter-plugin communication.
//!
//! Provides bounded fire-and-forget publish/subscribe messaging. Each registered
//! receiver must be owned by a subscriber pump; publishers never wait on consumers.
//!
//! # Safety
//!
//! - Payload size is capped at `MAX_PAYLOAD_BYTES` (64 KB).
//! - Recursive publish depth is capped at `MAX_RECURSION_DEPTH` (8) to prevent
//!   cycles.
//! - Each subscription ID is dispatched to at most once per publish call (ID-level dedup).

#[cfg(feature = "wasm-plugins")]
use std::collections::HashMap;
#[cfg(feature = "wasm-plugins")]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "wasm-plugins")]
use tokio::sync::{RwLock, mpsc};

/// Maximum payload size: 64 KB.
#[cfg(feature = "wasm-plugins")]
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Maximum recursive publish depth before returning an error.
#[cfg(feature = "wasm-plugins")]
pub const MAX_RECURSION_DEPTH: u32 = 8;

/// Per-subscriber queue bound. Slow consumers drop new events instead of growing memory.
#[cfg(feature = "wasm-plugins")]
pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;
#[cfg(feature = "wasm-plugins")]
pub const MAX_SUBSCRIPTIONS: usize = 4096;
#[cfg(feature = "wasm-plugins")]
pub const MAX_TOPIC_BYTES: usize = 256;

/// A registered subscription (exact-topic match).
#[cfg(feature = "wasm-plugins")]
#[derive(Clone, Debug)]
pub struct Subscription {
    pub id: u64,
    pub plugin_name: String,
    pub sender: mpsc::Sender<EventMessage>,
}

/// A registered wildcard subscription (prefix match: `topic.*`).
#[cfg(feature = "wasm-plugins")]
#[derive(Clone, Debug)]
pub struct WildcardSubscription {
    pub id: u64,
    pub plugin_name: String,
    /// Original pattern, e.g. `"weather.*"`.
    pub pattern: String,
    /// Derived prefix, e.g. `"weather."`.
    pub prefix: String,
    pub sender: mpsc::Sender<EventMessage>,
}

/// A subscription whose deliveries are pumped into a plugin's event sink.
/// Kept so a repeated subscription from the same sink resolves to the
/// existing one; the sink is held weakly so the registry never keeps a
/// plugin's delivery channel open.
#[cfg(feature = "wasm-plugins")]
#[derive(Debug)]
struct PumpSubscription {
    id: u64,
    plugin_name: String,
    pattern: String,
    sink: mpsc::WeakSender<EventMessage>,
}

#[cfg(feature = "wasm-plugins")]
impl PumpSubscription {
    fn matches(&self, plugin_name: &str, pattern: &str, sink: &mpsc::Sender<EventMessage>) -> bool {
        self.plugin_name == plugin_name
            && self.pattern == pattern
            && self.sink.upgrade().is_some_and(|live| live.same_channel(sink))
    }
}

/// An event message delivered to a subscriber.
#[cfg(feature = "wasm-plugins")]
#[derive(Clone, Debug)]
pub struct EventMessage {
    pub topic: String,
    pub payload: String,
    /// Current recursion depth (incremented on each nested publish).
    pub depth: u32,
}

/// The in-process event bus.
#[cfg(feature = "wasm-plugins")]
pub struct EventBus {
    /// Exact-topic subscriptions: topic → list of subs.
    subscriptions: RwLock<HashMap<String, Vec<Subscription>>>,
    /// Wildcard subscriptions (pattern ends with `.*`).
    wildcard_subscriptions: RwLock<Vec<WildcardSubscription>>,
    /// Monotonically increasing subscription ID counter.
    next_id: AtomicU64,
    active_subscriptions: AtomicUsize,
    /// Pump subscriptions by plugin, pattern and sink; see `subscribe_pump`.
    pumps: tokio::sync::Mutex<Vec<PumpSubscription>>,
}

#[cfg(feature = "wasm-plugins")]
impl EventBus {
    /// Create a new, empty `EventBus`.
    pub fn new() -> Self {
        Self {
            subscriptions: RwLock::new(HashMap::new()),
            wildcard_subscriptions: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
            active_subscriptions: AtomicUsize::new(0),
            pumps: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Publish an event to `topic` with `payload`.
    ///
    /// Returns an error if:
    /// - `payload` exceeds `MAX_PAYLOAD_BYTES`
    /// - `depth` exceeds `MAX_RECURSION_DEPTH`
    ///
    /// Otherwise, delivers the event fire-and-forget to all matching
    /// subscribers (exact match + prefix wildcard). Each subscription ID
    /// is dispatched to at most once, regardless of how many patterns match it.
    pub async fn publish(&self, topic: &str, payload: &str) -> Result<(), String> {
        self.publish_with_depth(topic, payload, 0).await
    }

    /// Internal publish with recursion depth tracking.
    pub async fn publish_with_depth(&self, topic: &str, payload: &str, depth: u32) -> Result<(), String> {
        if topic.is_empty() || topic.len() > MAX_TOPIC_BYTES {
            return Err(format!("event bus: topic must contain 1..={MAX_TOPIC_BYTES} bytes"));
        }
        if depth > MAX_RECURSION_DEPTH {
            return Err(format!(
                "event bus: recursion depth limit ({MAX_RECURSION_DEPTH}) exceeded for topic '{topic}'"
            ));
        }

        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "event bus: payload size {} exceeds maximum {} bytes",
                payload.len(),
                MAX_PAYLOAD_BYTES
            ));
        }

        tracing::debug!(
            topic = %topic,
            payload_bytes = payload.len(),
            depth,
            "event bus: publish"
        );

        let msg = EventMessage {
            topic: topic.to_string(),
            payload: payload.to_string(),
            depth: depth + 1,
        };

        // Collect all matching senders (deduplicated by subscription ID).
        let mut to_notify: Vec<(u64, mpsc::Sender<EventMessage>)> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // Exact matches.
        {
            let subs = self.subscriptions.read().await;
            if let Some(list) = subs.get(topic) {
                for sub in list {
                    if seen_ids.insert(sub.id) {
                        to_notify.push((sub.id, sub.sender.clone()));
                    }
                }
            }
        }

        // Wildcard matches.
        {
            let wildcards = self.wildcard_subscriptions.read().await;
            for wc in wildcards.iter() {
                if topic.starts_with(&wc.prefix) && seen_ids.insert(wc.id) {
                    to_notify.push((wc.id, wc.sender.clone()));
                }
            }
        }

        tracing::debug!(
            topic = %topic,
            subscribers = to_notify.len(),
            "event bus: dispatching"
        );

        // Bounded fire-and-forget dispatch. Closed receivers are removed eagerly;
        // full queues drop this event so one slow plugin cannot grow host memory.
        let mut closed = Vec::new();
        for (id, sender) in to_notify {
            match sender.try_send(msg.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(subscription_id = id, topic = %topic, "event bus: subscriber queue full; event dropped");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => closed.push(id),
            }
        }
        for id in closed {
            let _ = self.remove_subscription(id, None).await;
        }

        Ok(())
    }

    /// Register a subscription for `topic_pattern`.
    ///
    /// - Exact pattern (no `.*` suffix) → exact-match subscription.
    /// - Wildcard pattern ending in `.*` → prefix-match subscription.
    ///
    /// Returns `(subscription_id, receiver)`.
    pub async fn subscribe(
        &self,
        plugin_name: &str,
        topic_pattern: &str,
    ) -> Result<(u64, mpsc::Receiver<EventMessage>), String> {
        if topic_pattern.is_empty() || topic_pattern.len() > MAX_TOPIC_BYTES {
            return Err(format!(
                "event bus: topic pattern must contain 1..={MAX_TOPIC_BYTES} bytes"
            ));
        }
        self.active_subscriptions
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_SUBSCRIPTIONS).then_some(count + 1)
            })
            .map_err(|_| format!("event bus: subscription limit ({MAX_SUBSCRIPTIONS}) reached"))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);

        tracing::debug!(
            plugin = %plugin_name,
            pattern = %topic_pattern,
            subscription_id = id,
            "event bus: subscribe"
        );

        if topic_pattern == "*" || topic_pattern.ends_with(".*") {
            let prefix = if topic_pattern == "*" {
                String::new()
            } else {
                topic_pattern[..topic_pattern.len() - 1].to_string()
            }; // strip `*`
            let wc = WildcardSubscription {
                id,
                plugin_name: plugin_name.to_string(),
                pattern: topic_pattern.to_string(),
                prefix,
                sender: tx,
            };
            self.wildcard_subscriptions.write().await.push(wc);
        } else {
            let sub = Subscription {
                id,
                plugin_name: plugin_name.to_string(),
                sender: tx,
            };
            self.subscriptions
                .write()
                .await
                .entry(topic_pattern.to_string())
                .or_default()
                .push(sub);
        }

        Ok((id, rx))
    }

    /// Register a subscription whose receiver is continuously consumed by a
    /// bounded subscriber pump. The subscription is removed when the sink closes.
    ///
    /// Subscribing is idempotent per plugin, pattern and sink: a repeated
    /// subscription resolves to the existing one. A WASM hook's stores all
    /// deliver into one sink and survive a trap rebuild, but the guest's own
    /// memory of having subscribed does not, so a guest that subscribes
    /// lazily subscribes again after every rebuild; without this every
    /// matching event would be delivered once more per rebuild.
    pub async fn subscribe_pump(
        self: &std::sync::Arc<Self>,
        plugin_name: &str,
        topic_pattern: &str,
        sink: mpsc::Sender<EventMessage>,
    ) -> Result<u64, String> {
        let mut pumps = self.pumps.lock().await;
        pumps.retain(|pump| pump.sink.upgrade().is_some());
        if let Some(existing) = pumps
            .iter()
            .find(|pump| pump.matches(plugin_name, topic_pattern, &sink))
        {
            tracing::debug!(
                plugin = %plugin_name,
                pattern = %topic_pattern,
                subscription_id = existing.id,
                "event bus: repeated pump subscription resolved to the existing one"
            );
            return Ok(existing.id);
        }
        let (id, mut receiver) = self.subscribe(plugin_name, topic_pattern).await?;
        pumps.push(PumpSubscription {
            id,
            plugin_name: plugin_name.to_string(),
            pattern: topic_pattern.to_string(),
            sink: sink.downgrade(),
        });
        drop(pumps);
        let bus = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = bus.remove_subscription(id, None).await;
        });
        Ok(id)
    }

    /// Remove one of `plugin_name`'s own subscriptions by ID.
    ///
    /// Subscription ids are small sequential integers, so any plugin with the
    /// `events` permission can name another plugin's; only the plugin that
    /// subscribed may remove it. An id that exists but belongs to another
    /// plugin is refused with the same error as one that does not exist, so
    /// the answer does not reveal other plugins' subscriptions.
    pub async fn unsubscribe(&self, plugin_name: &str, subscription_id: u64) -> Result<(), String> {
        tracing::debug!(plugin = %plugin_name, subscription_id, "event bus: unsubscribe");
        self.remove_subscription(subscription_id, Some(plugin_name)).await
    }

    /// Remove a subscription by ID: any owner's when `owner` is `None` (the
    /// bus's own cleanup of closed receivers and ended pumps), otherwise only
    /// that owner's.
    async fn remove_subscription(&self, subscription_id: u64, owner: Option<&str>) -> Result<(), String> {
        let owned_by = |plugin_name: &str| owner.is_none_or(|owner| owner == plugin_name);

        // Check exact subscriptions.
        let mut found = false;
        {
            let mut subs = self.subscriptions.write().await;
            for list in subs.values_mut() {
                let before = list.len();
                list.retain(|s| s.id != subscription_id || !owned_by(&s.plugin_name));
                if list.len() < before {
                    found = true;
                }
            }
        }

        // Check wildcard subscriptions.
        if !found {
            let mut wildcards = self.wildcard_subscriptions.write().await;
            let before = wildcards.len();
            wildcards.retain(|w| w.id != subscription_id || !owned_by(&w.plugin_name));
            found = wildcards.len() < before;
        }

        if !found {
            return Err(format!("event bus: subscription ID {subscription_id} not found"));
        }
        self.active_subscriptions.fetch_sub(1, Ordering::AcqRel);
        self.pumps.lock().await.retain(|pump| pump.id != subscription_id);
        Ok(())
    }

    /// Returns the number of active subscriptions (exact + wildcard).
    pub fn subscription_count(&self) -> usize {
        self.active_subscriptions.load(Ordering::Acquire)
    }
}

#[cfg(feature = "wasm-plugins")]
impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "wasm-plugins"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::time::{Duration, timeout};

    /// Helper: receive one message from `rx` with a short timeout.
    async fn recv_one(rx: &mut mpsc::Receiver<EventMessage>) -> Option<EventMessage> {
        timeout(Duration::from_millis(100), rx.recv()).await.ok().flatten()
    }

    // 1. Publish to empty bus — no subscribers, should succeed without panic.
    #[tokio::test]
    async fn publish_to_empty_bus() {
        let bus = EventBus::new();
        let result = bus.publish("test.topic", r#"{"hello":"world"}"#).await;
        assert!(result.is_ok(), "publish to empty bus should succeed");
    }

    // 2. Subscribe and receive — subscriber gets matching event.
    #[tokio::test]
    async fn subscribe_and_receive() {
        let bus = Arc::new(EventBus::new());
        let (sub_id, mut rx) = bus.subscribe("plugin-a", "weather.update").await.unwrap();
        assert!(sub_id > 0);

        bus.publish("weather.update", r#"{"temp":22}"#).await.unwrap();

        let msg = recv_one(&mut rx).await.expect("should receive event");
        assert_eq!(msg.topic, "weather.update");
        assert_eq!(msg.payload, r#"{"temp":22}"#);
    }

    // 3. Wildcard subscribe — `weather.*` matches `weather.update` and `weather.forecast`.
    #[tokio::test]
    async fn wildcard_subscribe() {
        let bus = Arc::new(EventBus::new());
        let (_id, mut rx) = bus.subscribe("plugin-b", "weather.*").await.unwrap();

        bus.publish("weather.update", "payload1").await.unwrap();
        bus.publish("weather.forecast", "payload2").await.unwrap();
        // Non-matching topic — should NOT be received.
        bus.publish("news.latest", "payload3").await.unwrap();

        let msg1 = recv_one(&mut rx).await.expect("should receive weather.update");
        assert_eq!(msg1.topic, "weather.update");

        let msg2 = recv_one(&mut rx).await.expect("should receive weather.forecast");
        assert_eq!(msg2.topic, "weather.forecast");

        // No third message.
        let msg3 = recv_one(&mut rx).await;
        assert!(msg3.is_none(), "non-matching topic should not be received");
    }

    #[tokio::test]
    async fn global_wildcard_subscribe_matches_every_topic() {
        let bus = Arc::new(EventBus::new());
        let (_id, mut rx) = bus.subscribe("audit", "*").await.unwrap();
        bus.publish("weather.update", "one").await.unwrap();
        bus.publish("runtime.ready", "two").await.unwrap();
        assert_eq!(recv_one(&mut rx).await.unwrap().payload, "one");
        assert_eq!(recv_one(&mut rx).await.unwrap().payload, "two");
    }

    // 4. Unsubscribe — after unsubscribe, no more events.
    #[tokio::test]
    async fn unsubscribe() {
        let bus = Arc::new(EventBus::new());
        let (sub_id, mut rx) = bus.subscribe("plugin-c", "data.ready").await.unwrap();

        // First publish — should arrive.
        bus.publish("data.ready", "first").await.unwrap();
        let msg = recv_one(&mut rx).await.expect("should receive first event");
        assert_eq!(msg.payload, "first");

        // Unsubscribe.
        bus.unsubscribe("plugin-c", sub_id).await.unwrap();

        // Second publish — should NOT arrive.
        bus.publish("data.ready", "second").await.unwrap();
        let nothing = recv_one(&mut rx).await;
        assert!(nothing.is_none(), "should not receive after unsubscribe");
    }

    // 5. Payload size limit — over 64 KB returns error.
    #[tokio::test]
    async fn payload_size_limit() {
        let bus = EventBus::new();
        let large = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        let result = bus.publish("topic", &large).await;
        assert!(result.is_err(), "oversized payload should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("payload size"),
            "error message should mention payload size: {err}"
        );
    }

    // 6. Multiple subscribers on same topic — all receive the event.
    #[tokio::test]
    async fn multiple_subscribers_same_topic() {
        let bus = Arc::new(EventBus::new());
        let (_id1, mut rx1) = bus.subscribe("plugin-d", "shared.topic").await.unwrap();
        let (_id2, mut rx2) = bus.subscribe("plugin-e", "shared.topic").await.unwrap();

        bus.publish("shared.topic", "broadcast").await.unwrap();

        let m1 = recv_one(&mut rx1).await.expect("plugin-d should receive");
        let m2 = recv_one(&mut rx2).await.expect("plugin-e should receive");
        assert_eq!(m1.payload, "broadcast");
        assert_eq!(m2.payload, "broadcast");
    }

    // 7. No cross-topic delivery — publishing to A does not deliver to B subscriber.
    #[tokio::test]
    async fn publish_no_cross_topic() {
        let bus = Arc::new(EventBus::new());
        let (_id, mut rx) = bus.subscribe("plugin-f", "topic.a").await.unwrap();

        bus.publish("topic.b", "wrong").await.unwrap();

        let nothing = recv_one(&mut rx).await;
        assert!(nothing.is_none(), "cross-topic event should not be received");
    }

    // 8. Recursion depth limit — depth > MAX_RECURSION_DEPTH returns error.
    #[tokio::test]
    async fn recursion_depth_limit() {
        let bus = EventBus::new();
        let result = bus
            .publish_with_depth("loop.topic", "payload", MAX_RECURSION_DEPTH + 1)
            .await;
        assert!(result.is_err(), "exceeded depth should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("recursion depth limit"),
            "error should mention recursion limit: {err}"
        );
    }

    // 9. Stress test — N tasks publish concurrently, no panics, all deliveries complete.
    #[tokio::test]
    async fn stress_concurrent_publish() {
        const NUM_PUBLISHERS: usize = 50;
        const EVENTS_PER_PUBLISHER: usize = 20;

        let bus = Arc::new(EventBus::new());

        // Subscribe to all events so we can count deliveries.
        let (_sub_id, mut rx) = bus.subscribe("stress-consumer", "stress.event").await.unwrap();

        // Spawn NUM_PUBLISHERS tasks, each publishing EVENTS_PER_PUBLISHER events.
        let mut handles = Vec::with_capacity(NUM_PUBLISHERS);
        for i in 0..NUM_PUBLISHERS {
            let bus_clone = Arc::clone(&bus);
            let handle = tokio::spawn(async move {
                for j in 0..EVENTS_PER_PUBLISHER {
                    let payload = format!(r#"{{"publisher":{i},"seq":{j}}}"#);
                    bus_clone
                        .publish("stress.event", &payload)
                        .await
                        .expect("publish should not fail under load");
                }
            });
            handles.push(handle);
        }

        // Wait for all publishers to finish.
        for h in handles {
            h.await.expect("publisher task should not panic");
        }

        // Drain all received events with a short timeout between reads.
        let expected = NUM_PUBLISHERS * EVENTS_PER_PUBLISHER;
        let mut received = 0usize;
        while let Ok(Some(_)) = timeout(Duration::from_millis(200), rx.recv()).await {
            received += 1;
            if received >= expected {
                break;
            }
        }

        assert_eq!(
            received, expected,
            "stress test: expected {expected} events but received {received}"
        );
    }

    // 10. subscription_count reflects add/remove correctly.
    #[tokio::test]
    async fn subscription_count_tracking() {
        let bus = Arc::new(EventBus::new());
        assert_eq!(bus.subscription_count(), 0);

        let (id1, _rx1) = bus.subscribe("p1", "topic.a").await.unwrap();
        assert_eq!(bus.subscription_count(), 1);

        let (id2, _rx2) = bus.subscribe("p2", "topic.*").await.unwrap();
        assert_eq!(bus.subscription_count(), 2);

        bus.unsubscribe("p1", id1).await.unwrap();
        assert_eq!(bus.subscription_count(), 1);

        bus.unsubscribe("p2", id2).await.unwrap();
        assert_eq!(bus.subscription_count(), 0);
    }

    /// R-C3Xf N5: subscription ids are small sequential integers, so any
    /// plugin with the `events` permission can name another plugin's. Only
    /// the plugin that subscribed may remove a subscription, and the refusal
    /// reads like an unknown id.
    #[tokio::test]
    async fn a_plugin_cannot_unsubscribe_another_plugins_subscription() {
        let bus = Arc::new(EventBus::new());
        let (exact_id, mut exact_rx) = bus.subscribe("victim", "data.ready").await.unwrap();
        let (wildcard_id, mut wildcard_rx) = bus.subscribe("victim", "data.*").await.unwrap();

        assert!(
            bus.unsubscribe("attacker", exact_id).await.is_err(),
            "another plugin's exact subscription"
        );
        assert!(
            bus.unsubscribe("attacker", wildcard_id).await.is_err(),
            "another plugin's wildcard subscription"
        );
        assert_eq!(bus.subscription_count(), 2);

        bus.publish("data.ready", "still delivered").await.unwrap();
        for rx in [&mut exact_rx, &mut wildcard_rx] {
            let message = recv_one(rx).await.expect("the victim still receives");
            assert_eq!(message.payload, "still delivered");
        }

        bus.unsubscribe("victim", exact_id).await.unwrap();
        assert_eq!(bus.subscription_count(), 1, "the owner can still unsubscribe");
    }

    // 11. Unsubscribe with unknown ID returns error.
    #[tokio::test]
    async fn unsubscribe_unknown_id_returns_error() {
        let bus = EventBus::new();
        let result = bus.unsubscribe("any-plugin", 99999).await;
        assert!(result.is_err(), "unknown subscription ID should return error");
    }

    #[tokio::test]
    async fn subscriber_pump_forwards_and_cleans_up_when_sink_closes() {
        let bus = Arc::new(EventBus::new());
        let (sink, mut inbox) = mpsc::channel(1);
        let id = bus.subscribe_pump("hook-plugin", "runtime.*", sink).await.unwrap();

        bus.publish("runtime.ready", "payload").await.unwrap();
        let message = timeout(Duration::from_millis(100), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.topic, "runtime.ready");
        assert_eq!(message.payload, "payload");

        drop(inbox);
        bus.publish("runtime.ready", "cleanup").await.unwrap();
        timeout(Duration::from_millis(100), async {
            while bus.subscription_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(bus.unsubscribe("hook-plugin", id).await.is_err());
    }

    /// R-C3X F5: a hook guest that subscribes lazily, behind a flag in its own
    /// memory, subscribes again after every rebuild, and all of its adapter's
    /// stores share one delivery sink. Each duplicate used to get its own pump,
    /// so every matching event was delivered once more per rebuild.
    #[tokio::test]
    async fn a_repeated_pump_subscription_from_one_sink_is_registered_once() {
        let bus = Arc::new(EventBus::new());
        let (sink, _deliveries) = mpsc::channel(8);
        let first = bus
            .subscribe_pump("audit-hook", "prx.lifecycle.*", sink.clone())
            .await
            .expect("test: first subscription");
        let again = bus
            .subscribe_pump("audit-hook", "prx.lifecycle.*", sink.clone())
            .await
            .expect("test: repeated subscription");
        assert_eq!(
            first, again,
            "the repeated subscription must resolve to the existing one"
        );
        assert_eq!(bus.subscription_count(), 1);

        let (other_sink, _other_deliveries) = mpsc::channel(8);
        bus.subscribe_pump("audit-hook", "prx.lifecycle.*", other_sink)
            .await
            .expect("test: a different adapter's sink");
        bus.subscribe_pump("audit-hook", "prx.plugin.*", sink)
            .await
            .expect("test: a different pattern");
        assert_eq!(
            bus.subscription_count(),
            3,
            "a different sink or pattern is a different subscription"
        );
    }
}

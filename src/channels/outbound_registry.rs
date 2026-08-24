//! Process-wide view of the channels this process can send on.
//!
//! The routing registry that `message_send` and `sessions_spawn` share is built
//! per component: the channels supervisor builds one from every configured
//! channel, and the gateway builds one from the channels it constructs for its
//! own webhook paths. Both live in the daemon process, but neither is reachable
//! from the other — and the gateway API has no channel objects at all.
//!
//! That is the gap this module closes. Each component publishes its registry
//! here for as long as it is running, and `POST /api/channels/{name}/send`
//! resolves against the merged view. Publication is a slot, not a global
//! overwrite: two components can publish at once, and a component that stops
//! takes only its own entries away when the returned guard drops.
//!
//! This is a lookup table, not an ownership transfer: the channel objects stay
//! owned by whoever built them, and are handed out as `Arc` clones.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::traits::Channel;

/// One component's channel registry, keyed by [`Channel::name`].
pub type ChannelRegistry = Arc<HashMap<String, Arc<dyn Channel>>>;

/// Live publications, in publication order. Later publications win a name
/// collision, so a component that starts after another (channels after the
/// gateway) owns the shared name — it is the one holding the live connection.
static PUBLICATIONS: LazyLock<Mutex<Vec<(u64, ChannelRegistry)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

static NEXT_PUBLICATION_ID: AtomicU64 = AtomicU64::new(0);

/// A live publication. Dropping it withdraws exactly the entries it added.
///
/// Held by the component that owns the channels for as long as those channels
/// exist, so a restarted or reconfigured component never leaves dead channel
/// objects addressable.
#[derive(Debug)]
pub struct OutboundChannelsPublication {
    id: u64,
}

impl Drop for OutboundChannelsPublication {
    fn drop(&mut self) {
        let mut publications = PUBLICATIONS.lock();
        publications.retain(|(id, _)| *id != self.id);
    }
}

/// Publish `channels` for as long as the returned guard is alive.
#[must_use = "the publication is withdrawn as soon as the guard is dropped"]
pub fn publish(channels: ChannelRegistry) -> OutboundChannelsPublication {
    let id = NEXT_PUBLICATION_ID.fetch_add(1, Ordering::Relaxed);
    PUBLICATIONS.lock().push((id, channels));
    OutboundChannelsPublication { id }
}

/// The merged registry across every live publication.
#[must_use]
pub fn snapshot() -> HashMap<String, Arc<dyn Channel>> {
    let publications = PUBLICATIONS.lock();
    let mut merged: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    for (_, registry) in publications.iter() {
        for (name, channel) in registry.iter() {
            merged.insert(name.clone(), Arc::clone(channel));
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{ChannelMessage, SendMessage};
    use async_trait::async_trait;

    struct NamedChannel(&'static str);

    #[async_trait]
    impl Channel for NamedChannel {
        fn name(&self) -> &str {
            self.0
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn registry(names: &[&'static str]) -> ChannelRegistry {
        Arc::new(
            names
                .iter()
                .map(|name| ((*name).to_string(), Arc::new(NamedChannel(name)) as Arc<dyn Channel>))
                .collect(),
        )
    }

    #[test]
    fn publications_merge_and_are_withdrawn_on_drop() {
        let first = publish(registry(&["outbound-registry-a"]));
        let second = publish(registry(&["outbound-registry-b"]));

        let merged = snapshot();
        assert!(merged.contains_key("outbound-registry-a"));
        assert!(merged.contains_key("outbound-registry-b"));

        drop(second);
        let merged = snapshot();
        assert!(merged.contains_key("outbound-registry-a"));
        assert!(
            !merged.contains_key("outbound-registry-b"),
            "a withdrawn publication must not stay addressable"
        );

        drop(first);
        assert!(!snapshot().contains_key("outbound-registry-a"));
    }

    #[test]
    fn a_later_publication_wins_a_name_collision() {
        let first = publish(registry(&["outbound-registry-collision"]));
        let second = publish(registry(&["outbound-registry-collision"]));
        let winner = snapshot()
            .get("outbound-registry-collision")
            .map(|channel| Arc::as_ptr(channel).cast::<()>());
        let expected = second
            .live_entry_for_test("outbound-registry-collision")
            .map(|channel| Arc::as_ptr(&channel).cast::<()>());
        assert_eq!(winner, expected, "the later publication must own the name");
        drop(first);
        drop(second);
    }
}

#[cfg(test)]
impl OutboundChannelsPublication {
    /// The channel this publication contributes under `name`, for tests that
    /// need to tell two publications of the same name apart.
    fn live_entry_for_test(&self, name: &str) -> Option<Arc<dyn Channel>> {
        let publications = PUBLICATIONS.lock();
        publications
            .iter()
            .find(|(id, _)| *id == self.id)
            .and_then(|(_, registry)| registry.get(name).map(Arc::clone))
    }
}

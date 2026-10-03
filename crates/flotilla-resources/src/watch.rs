use std::{
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{
    stream::{self, BoxStream},
    Stream, StreamExt,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
    error::ResourceError,
    resource::{Resource, ResourceObject},
};

pub(crate) const WATCH_RING_CAPACITY: usize = 256;

// One bounded ring per resource stream, shared by all subscribers. A slow
// subscriber cannot retain a private, ever-growing queue of full objects.
#[derive(Debug)]
pub(crate) struct WatchChannel<E> {
    sender: broadcast::Sender<Arc<E>>,
}

impl<E> Default for WatchChannel<E> {
    fn default() -> Self {
        Self { sender: broadcast::channel(WATCH_RING_CAPACITY).0 }
    }
}

impl<E: Send + Sync + 'static> WatchChannel<E> {
    pub(crate) fn send(&self, event: E) {
        let _ = self.sender.send(Arc::new(event));
    }

    pub(crate) fn subscribe(&self, kind: &'static str, namespace: &str) -> BoxStream<'static, Result<Arc<E>, ResourceError>> {
        stream::unfold((Some(self.sender.subscribe()), namespace.to_owned()), move |(receiver, namespace)| async move {
            let mut receiver = receiver?;
            match receiver.recv().await {
                Ok(event) => Some((Ok(event), (Some(receiver), namespace))),
                Err(broadcast::error::RecvError::Closed) => None,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(kind, %namespace, skipped, capacity = WATCH_RING_CAPACITY, "resource watch lagged; relist required");
                    Some((
                        Err(ResourceError::WatchExpired {
                            requested_version: "live watch lagged; relist required".to_string(),
                            compacted_through: None,
                        }),
                        (None, namespace),
                    ))
                }
            }
        })
        .boxed()
    }
}

pub struct WatchStream<T: Resource> {
    generation: Option<String>,
    inner: BoxStream<'static, Result<WatchEvent<T>, ResourceError>>,
}

impl<T: Resource> WatchStream<T> {
    pub fn new(generation: Option<String>, inner: BoxStream<'static, Result<WatchEvent<T>, ResourceError>>) -> Self {
        Self { generation, inner }
    }

    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }
}

impl<T: Resource> fmt::Debug for WatchStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchStream").field("generation", &self.generation).finish_non_exhaustive()
    }
}

impl<T: Resource> Stream for WatchStream<T> {
    type Item = Result<WatchEvent<T>, ResourceError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchStart {
    /// Deliver future events only. No replay of current state.
    Now,
    /// Resume from a specific version, delivering all events since that point.
    FromVersion(String),
    /// Resume from a specific version within an ephemeral store generation.
    FromVersionInGeneration { generation: String, resource_version: String },
}

impl WatchStart {
    /// Resume a watch from where a list left off, carrying the list's store
    /// generation when it has one. Generational stores reject a plain
    /// `FromVersion` resume, so every list-then-watch caller must go through
    /// this instead of constructing `FromVersion` directly.
    pub fn resuming_from<T: Resource>(listed: &ResourceList<T>) -> Self {
        match &listed.generation {
            Some(generation) => {
                Self::FromVersionInGeneration { generation: generation.clone(), resource_version: listed.resource_version.clone() }
            }
            None => Self::FromVersion(listed.resource_version.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub enum WatchEvent<T: Resource> {
    Added(ResourceObject<T>),
    Modified(ResourceObject<T>),
    Deleted(ResourceObject<T>),
    DeletedByName(ResourceTombstone),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTombstone {
    pub name: String,
    pub namespace: String,
    pub resource_version: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TombstoneWrite {
    pub tombstone: ResourceTombstone,
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub struct ResourceList<T: Resource> {
    pub items: Vec<ResourceObject<T>>,
    pub resource_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::WatchStart;

    #[test]
    fn watch_start_roundtrips_through_serde() {
        let encoded = serde_json::to_string(&WatchStart::FromVersion("7".to_string())).expect("serialize watch start");
        let decoded: WatchStart = serde_json::from_str(&encoded).expect("deserialize watch start");
        assert_eq!(decoded, WatchStart::FromVersion("7".to_string()));
    }
    // The live event heap is bounded independently of subscriber count and
    // update count. Weak references count retained payloads, not RSS affected
    // by allocator caching. Every run crosses both ring boundary sides.
    #[hegel::test]
    fn shared_watch_ring_bounds_retained_payloads_and_preserves_fast_subscribers(tc: hegel::TestCase) {
        use futures::StreamExt;
        use hegel::generators as gs;

        use super::{WatchChannel, WATCH_RING_CAPACITY};
        // Fleet-sized fan-out, and bursts spanning capacity + 1 through 8x capacity.
        let subscribers = tc.draw(gs::integers::<usize>().min_value(1).max_value(64));
        let updates = tc.draw(gs::integers::<usize>().min_value(WATCH_RING_CAPACITY + 1).max_value(WATCH_RING_CAPACITY * 8));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let channel = WatchChannel::default();
            let slow: Vec<_> = (0..subscribers).map(|_| channel.subscribe("Convoy", "flotilla")).collect();
            let mut fast = channel.subscribe("Convoy", "flotilla");
            let mut payloads = Vec::new();
            for index in 0..updates {
                let payload = Arc::new(vec![(index % 256) as u8; 16 * 1024]);
                payloads.push(Arc::downgrade(&payload));
                channel.send(payload);
                let received = fast.next().await.expect("fast subscriber event").expect("never lagged");
                assert_eq!(received[0], (index % 256) as u8);
                drop(received);
                assert_eq!(
                    payloads.iter().filter(|payload| payload.strong_count() > 0).count(),
                    (index + 1).min(WATCH_RING_CAPACITY),
                    "only one ring of payloads may survive, regardless of subscriber count"
                );
            }
            for mut subscriber in slow {
                assert!(matches!(subscriber.next().await, Some(Err(crate::ResourceError::WatchExpired { .. }))));
                assert!(subscriber.next().await.is_none());
            }
            drop(fast);
            assert!(payloads.iter().all(|payload| payload.strong_count() == 0), "disconnect frees retained events");
        });
    }

    // No demand retains no event payloads, even if writers keep publishing.
    #[test]
    fn watch_channel_without_subscribers_drops_payloads() {
        let channel = super::WatchChannel::default();
        let payload = Arc::new(vec![0; 16384]);
        let weak = Arc::downgrade(&payload);
        channel.send(payload);
        assert_eq!(weak.strong_count(), 0);
    }
}

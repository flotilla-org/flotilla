use std::sync::Arc;

use flotilla_resources::*;
use futures::{
    stream::{self, BoxStream},
    StreamExt,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use tokio::sync::broadcast;

// Shared live payloads borrow their JSON tree; owned replay payloads can reuse
// their strings. Both return owned objects without an intermediate tree clone.
pub fn decode_watch_object<'de, T: Resource>(
    value: impl Deserializer<'de, Error = serde_json::Error>,
) -> Result<ResourceObject<T>, ResourceError> {
    let object: K8sResourceObject<T> =
        Deserialize::deserialize(value).map_err(|error| ResourceError::decode(format!("decode stored object: {error}")))?;
    ResourceObject::from_k8s_object(object)
}

pub fn decode_watch_tombstone(value: &Value) -> Result<ResourceTombstone, ResourceError> {
    let metadata = value.get("metadata").ok_or_else(|| ResourceError::decode("decode tombstone: missing metadata"))?;
    let name =
        metadata.get("name").and_then(Value::as_str).ok_or_else(|| ResourceError::decode("decode tombstone: missing name"))?.to_string();
    let resource_version = metadata
        .get("resourceVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| ResourceError::decode("decode tombstone: missing resourceVersion"))?
        .to_string();
    let namespace = metadata.get("namespace").and_then(Value::as_str).unwrap_or_default().to_string();
    let annotations = metadata
        .get("annotations")
        .map(serde::Deserialize::deserialize)
        .transpose()
        .map_err(|error| ResourceError::decode(format!("decode tombstone annotations: {error}")))?
        .unwrap_or_default();
    Ok(ResourceTombstone { name, namespace, resource_version, annotations })
}

pub const WATCH_RING_CAPACITY: usize = 256;

// One bounded ring per resource stream, shared by all subscribers. A slow
// subscriber cannot retain a private, ever-growing queue of full objects.
#[derive(Debug)]
pub struct WatchChannel<E> {
    sender: broadcast::Sender<Arc<E>>,
}

impl<E> Default for WatchChannel<E> {
    fn default() -> Self {
        Self { sender: broadcast::channel(WATCH_RING_CAPACITY).0 }
    }
}

impl<E: Send + Sync + 'static> WatchChannel<E> {
    pub fn send(&self, event: E) {
        let _ = self.sender.send(Arc::new(event));
    }

    pub fn subscribe(&self, kind: &'static str, namespace: &str) -> BoxStream<'static, Result<Arc<E>, ResourceError>> {
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

#[derive(Debug, Clone)]
pub struct TombstoneWrite {
    pub tombstone: ResourceTombstone,
    pub created: bool,
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
    // Borrowed decoding must agree with the previous owned-value decoder for
    // valid objects and reject malformed records. Generate empty through large
    // payloads/maps and wrong type, metadata, spec, kind and API-version cases.
    #[hegel::test]
    fn borrowed_watch_decode_matches_owned(tc: hegel::TestCase) {
        use hegel::generators as gs;

        use crate::{api_version, Convoy, ConvoySpec, K8sResourceObject, Resource, ResourceObject};
        let labels = tc.draw(gs::integers::<usize>().min_value(0).max_value(64));
        let size = tc.draw(gs::integers::<usize>().min_value(0).max_value(16384));
        let corruption = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let mut value = serde_json::json!({
            "apiVersion": api_version(Convoy::API_PATHS), "kind": Convoy::API_PATHS.kind,
            "metadata": {"name": "payload", "namespace": "test", "resourceVersion": "1", "creationTimestamp": "2026-01-01T00:00:00Z"},
            "spec": ConvoySpec::builder().workflow_ref("x".repeat(size)).build(),
        });
        let map: std::collections::BTreeMap<_, _> = (0..labels).map(|i| (format!("key-{i}"), "value".to_owned())).collect();
        value["metadata"]["labels"] = serde_json::to_value(map).expect("labels");
        match corruption {
            0 => {}
            1 => value = serde_json::Value::Null,
            2 => value["metadata"]["name"] = serde_json::json!(42),
            3 => value["spec"] = serde_json::Value::Null,
            4 => value["kind"] = serde_json::json!("Wrong"),
            5 => value["apiVersion"] = serde_json::json!("Wrong"),
            _ => unreachable!(),
        }
        let owned = serde_json::from_value::<K8sResourceObject<Convoy>>(value.clone())
            .map_err(|error| crate::ResourceError::decode(error.to_string()))
            .and_then(ResourceObject::from_k8s_object);
        let borrowed = super::decode_watch_object::<Convoy>(&value);
        assert_eq!(borrowed.is_ok(), owned.is_ok());
        if let (Ok(borrowed), Ok(owned)) = (borrowed, owned) {
            drop(value);
            assert_eq!(
                serde_json::to_value(borrowed.to_k8s_object()).expect("borrowed"),
                serde_json::to_value(owned.to_k8s_object()).expect("owned")
            );
        }
    }

    // Name-only deletes own their metadata and reject malformed annotations.
    #[test]
    fn borrowed_watch_tombstone_keeps_annotations_and_rejects_invalid_metadata() {
        let mut value =
            serde_json::json!({"metadata": {"name": "gone", "namespace": "test", "resourceVersion": "7", "annotations": {"key": "value"}}});
        let tombstone = super::decode_watch_tombstone(&value).expect("tombstone");
        value["metadata"]["annotations"]["key"] = serde_json::json!(123);
        assert!(super::decode_watch_tombstone(&value).is_err());
        value["metadata"].as_object_mut().expect("metadata").remove("name");
        assert!(super::decode_watch_tombstone(&value).is_err());
        drop(value);
        assert_eq!(tombstone.name, "gone");
        assert_eq!(tombstone.resource_version, "7");
        assert_eq!(tombstone.annotations["key"], "value");
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

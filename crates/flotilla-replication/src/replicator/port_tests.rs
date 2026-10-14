//! Engine behaviour through the host ports, against in-memory stores.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use flotilla_protocol::{CommandValue, NodeId, ResourceCursor, ResourceReadEnvelope, ResourceReadRecord, ResourceRecordProvenance};
use flotilla_resources::{Convoy, ConvoySpec, InputMeta, PartitionDigest, ResourceObject, ResourceProvenance};
use flotilla_store::ResourceBackend;

use super::test_ports::{host, stores, HealthReport, InMemoryPeers, NoRoutedPeer, RecordingHealth};
use super::*;
use crate::ports::{PeerSources, RoutedDigestRequest, RoutedPeer, RoutedWatch, RoutedWatchItem, RoutedWatchRequest};

fn spec(workflow: &str) -> ConvoySpec {
    ConvoySpec::builder().workflow_ref(workflow.into()).build()
}

async fn author(backend: &ResourceBackend, name: &str) -> ResourceObject<Convoy> {
    backend
        .using::<Convoy>(REPLICATION_NAMESPACE)
        .create(&InputMeta::builder().name(name.into()).build(), &spec("workflow"))
        .await
        .expect("author")
}

async fn replica_origins(backend: &ResourceBackend) -> Vec<(String, NodeId)> {
    let mut rows = backend
        .including_replicas::<Convoy>(REPLICATION_NAMESPACE)
        .list()
        .await
        .expect("list replicas")
        .items
        .into_iter()
        .filter_map(|item| match item.provenance {
            ResourceProvenance::Replica { origin_root, .. } => Some((item.object.metadata.name, origin_root)),
            ResourceProvenance::Local => None,
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

async fn await_replicas(backend: &ResourceBackend, expected: &[(&str, &NodeId)]) {
    let expected = expected.iter().map(|(name, origin)| (name.to_string(), (*origin).clone())).collect::<Vec<_>>();
    tokio::time::timeout(Duration::from_secs(5), async {
        while replica_origins(backend).await != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("replicas did not converge to {expected:?}"));
}

// The direct peer is any Store: snapshot, live log and deletes reach the
// holder as replicas of the peer, and the established watch reports health.
#[tokio::test]
async fn direct_peer_store_replicates_snapshot_and_live_log() {
    let authority_id = NodeId::new("authority");
    let authority = stores(&authority_id);
    author(&authority.durable, "snapshotted").await;
    let health = Arc::new(RecordingHealth::default());
    let holder = host("holder", Arc::clone(&health));

    let task = tokio::spawn({
        let holder = holder.clone();
        let remote = authority.durable.clone();
        let peer = authority_id.clone();
        async move { replicate_kind_direct::<Convoy>(remote, &holder, &peer, ReplicationStore::Durable).await }
    });
    await_replicas(&holder.stores().durable, &[("snapshotted", &authority_id)]).await;
    author(&authority.durable, "live").await;
    await_replicas(&holder.stores().durable, &[("live", &authority_id), ("snapshotted", &authority_id)]).await;
    authority.durable.using::<Convoy>(REPLICATION_NAMESPACE).delete("snapshotted").await.expect("delete");
    await_replicas(&holder.stores().durable, &[("live", &authority_id)]).await;

    assert!(health.reports().contains(&HealthReport::Healthy(authority_id.clone(), "Convoy".into())));
    assert!(replica_origins(&holder.stores().observed).await.is_empty(), "durable replication must not write the observed store");
    // Load metrics (#3206): one snapshot, then the live create and delete as events.
    let metrics = holder.metrics().snapshot();
    assert_eq!((metrics.snapshots, metrics.lag_resets), (1, 0));
    assert!(metrics.events_applied >= 2, "{metrics:?}");
    task.abort();
}

// Supervisors open the direct peer through the connector at the generation's
// socket, clear stale health first, and stop when the generation ends.
#[tokio::test]
async fn supervisors_replicate_a_connected_direct_peer_until_disconnect() {
    let authority_id = NodeId::new("authority");
    let authority = stores(&authority_id);
    author(&authority.durable, "first").await;
    let peers = Arc::new(InMemoryPeers::default());
    let socket = PathBuf::from("/peers/authority.sock");
    peers.serve(socket.clone(), authority.clone());
    let health = Arc::new(RecordingHealth::default());
    let holder = host("holder", Arc::clone(&health));
    let mut supervisors = PeerReplicatorSupervisors::new(
        holder.clone(),
        PeerSources { direct: Arc::clone(&peers) as Arc<dyn DirectPeerConnector>, routed: Arc::new(NoRoutedPeer) },
        ReplicationOptions::new(Some(&["Convoy"])),
    );

    supervisors.peer_connected(authority_id.clone(), 1, Some(socket.clone())).await;
    await_replicas(&holder.stores().durable, &[("first", &authority_id)]).await;
    assert_eq!(health.reports().first(), Some(&HealthReport::Begin(authority_id.clone())));
    assert!(peers
        .connections
        .lock()
        .expect("connections")
        .iter()
        .all(|(path, store)| path == &socket && *store == ReplicationStore::Durable));

    supervisors.peer_disconnected(&authority_id, 1);
    tokio::task::yield_now().await;
    author(&authority.durable, "after-disconnect").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(replica_origins(&holder.stores().durable).await, vec![("first".to_string(), authority_id)]);
}

/// A routed peer that replays scripted watch items and records requests.
#[derive(Default)]
struct ScriptedRoutedPeer {
    items: Mutex<VecDeque<RoutedWatchItem>>,
    requests: Mutex<Vec<RoutedWatchRequest>>,
}

struct ScriptedWatch(Arc<ScriptedRoutedPeer>);

#[async_trait::async_trait]
impl RoutedWatch for ScriptedWatch {
    async fn next(&mut self) -> RoutedWatchItem {
        let next = self.0.items.lock().expect("scripted items").pop_front();
        match next {
            Some(item) => item,
            None => std::future::pending().await,
        }
    }
    async fn cancel(&self) {}
}

#[async_trait::async_trait]
impl RoutedPeer for Arc<ScriptedRoutedPeer> {
    async fn watch(&self, _peer: &NodeId, request: RoutedWatchRequest) -> Result<Box<dyn RoutedWatch>, String> {
        self.requests.lock().expect("requests").push(request);
        Ok(Box::new(ScriptedWatch(Arc::clone(self))))
    }
    async fn digest(&self, _peer: &NodeId, _request: RoutedDigestRequest) -> Result<PartitionDigest, String> {
        Err("no digest in this script".into())
    }
}

fn envelope(resource_version: &str, records: Vec<ResourceReadRecord>) -> RoutedWatchItem {
    RoutedWatchItem::Response(Box::new(ResourceReadEnvelope {
        api_version: "flotilla.work/v1".into(),
        resource_kind: "Convoy".into(),
        plural: "convoys".into(),
        namespace: REPLICATION_NAMESPACE.into(),
        cursor: ResourceCursor::from_position(resource_version, None),
        records,
    }))
}

fn record(record_type: ResourceRecordType, object: &ResourceObject<Convoy>) -> ResourceReadRecord {
    ResourceReadRecord {
        record_type,
        provenance: ResourceRecordProvenance::Local { node_id: NodeId::new("peer") },
        object: Some(serde_json::to_value(object.to_k8s_object()).expect("encode object")),
    }
}

fn bookmark() -> ResourceReadRecord {
    ResourceReadRecord {
        record_type: ResourceRecordType::Bookmark,
        provenance: ResourceRecordProvenance::Local { node_id: NodeId::new("peer") },
        object: None,
    }
}

// A routed snapshot is applied at its bookmark, and a finished command ends
// the attempt cleanly after the watch reported health.
#[tokio::test]
async fn routed_watch_snapshot_replaces_replicas_at_the_bookmark() {
    let peer_id = NodeId::new("peer");
    let peer_store = stores(&peer_id);
    let object = author(&peer_store.durable, "routed").await;
    let routed = Arc::new(ScriptedRoutedPeer::default());
    routed.items.lock().expect("items").extend([
        envelope(&object.metadata.resource_version, vec![record(ResourceRecordType::Current, &object), bookmark()]),
        RoutedWatchItem::Finished(Box::new(CommandValue::Cancelled)),
    ]);
    let health = Arc::new(RecordingHealth::default());
    let holder = host("holder", Arc::clone(&health));

    replicate_kind_over_routed_watch::<Convoy>(&routed, &holder, &peer_id, ReplicationStore::Durable, None).await.expect("routed watch");

    assert_eq!(replica_origins(&holder.stores().durable).await, vec![("routed".to_string(), peer_id.clone())]);
    assert_eq!(
        routed.requests.lock().expect("requests").as_slice(),
        [RoutedWatchRequest { namespace: REPLICATION_NAMESPACE.into(), kind: "convoys".into(), replica_sources: false, cursor: None }]
    );
    assert_eq!(health.reports(), vec![HealthReport::Healthy(peer_id, "Convoy".into())]);
    let metrics = holder.metrics().snapshot();
    assert_eq!((metrics.snapshots, metrics.events_applied), (1, 0), "a snapshot's records are not live events");
}

// Relay applies only third-origin records: records originating at this host
// or at the relaying peer are skipped.
#[tokio::test]
async fn routed_relay_skips_local_and_peer_origins() {
    let peer_id = NodeId::new("peer");
    let holder = host("holder", Arc::new(RecordingHealth::default()));
    let third = NodeId::new("third");
    let scratch = stores(&peer_id);
    let mut records = Vec::new();
    for (name, origin) in [("from-holder", holder.node_id()), ("from-peer", &peer_id), ("from-third", &third)] {
        let mut object = author(&scratch.durable, name).await;
        object.metadata.annotations.insert("flotilla.work/origin-root".into(), origin.to_string());
        object.metadata.annotations.insert("flotilla.work/last-synced-at".into(), "2026-10-01T00:00:00Z".into());
        records.push(record(ResourceRecordType::Added, &object));
    }
    let routed = Arc::new(ScriptedRoutedPeer::default());
    routed.items.lock().expect("items").extend([envelope("3", records), RoutedWatchItem::Finished(Box::new(CommandValue::Ok))]);

    replicate_relay_over_routed_watch::<Convoy>(&routed, &holder, &peer_id, ReplicationStore::Durable).await.expect("relay");

    assert_eq!(replica_origins(&holder.stores().durable).await, vec![("from-third".to_string(), third)]);
    assert!(routed.requests.lock().expect("requests")[0].replica_sources);
}

// Transport loss surfaces as the attempt's error, so supervision retries it.
#[tokio::test]
async fn routed_watch_lag_is_a_sequence_gap() {
    let routed = Arc::new(ScriptedRoutedPeer::default());
    routed.items.lock().expect("items").push_back(RoutedWatchItem::Lagged(4));
    let holder = host("holder", Arc::new(RecordingHealth::default()));
    let error = run_routed_watch::<Convoy>(&routed, &holder, &NodeId::new("peer"), ReplicationStore::Durable, None, None)
        .await
        .expect_err("lag cannot continue the log");
    assert!(error.contains("sequence gap"), "{error}");
    assert_eq!(holder.metrics().snapshot().lag_resets, 1);
}

// The direct watch's speculative version must never survive an apply error.
// Keep its source open so swallowing that error would stall instead of return.
#[tokio::test(start_paused = true)]
async fn direct_watch_terminates_on_apply_error() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let object = author(&backend, "first").await;
    let peer = NodeId::new("peer");
    let writer = backend.replica_writer::<Convoy>(peer.clone(), REPLICATION_NAMESPACE);
    writer
        .replace(&flotilla_resources::ResourceList { items: vec![], resource_version: "0".into(), generation: None }, Utc::now())
        .await
        .unwrap();
    // The event is contiguous with the watch's cut, but the durable prefix has
    // been invalidated. Sequence validation succeeds and application refuses.
    writer.invalidate_cursor().await.unwrap();
    let events = futures::stream::iter([Ok(WatchEvent::Added(object))]).chain(futures::stream::pending()).boxed();
    let watch = flotilla_resources::WatchStream::new(None, events);
    let remote = backend.using::<Convoy>(REPLICATION_NAMESPACE);
    let result =
        tokio::time::timeout(Duration::from_secs(2), apply_direct_watch(watch, &writer, "0".into(), &remote, &peer, &Default::default()))
            .await
            .expect("apply error must terminate the open watch");
    assert!(matches!(result, Err(OriginWatchFailure::SnapshotRequired(message)) if message.contains("replica prefix missing")));
    assert!(writer.cursor().await.unwrap().is_none());
}

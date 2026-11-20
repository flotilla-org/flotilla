//! Typed reads decode each stored body once (#3154).
//!
//! A probe resource counts every typed deserialization of its spec and where
//! the deserializer read from: the stored text itself, or a `serde_json::Value`
//! tree built from it first. Each test owns its probe type, so the counters are
//! private to that test even though the integration binary runs in parallel.
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{ApiPaths, InputMeta, ObjectMeta, ReplicationClass, Resource, ResourceObject, StatusPatch, WatchStart};
use flotilla_store::{replica::StoredReplicaEventKind, ResourceBackend, ResourceKind, SerializedResource, StoreExt, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;
use hegel::generators as gs;
use serde::{Deserialize, Deserializer, Serialize};

const PROBES: usize = 6;
static FROM_TEXT: [AtomicUsize; PROBES] = [const { AtomicUsize::new(0) }; PROBES];
static FROM_TREE: [AtomicUsize; PROBES] = [const { AtomicUsize::new(0) }; PROBES];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decodes {
    from_text: usize,
    from_tree: usize,
}

fn take_decodes<const ID: usize>() -> Decodes {
    Decodes { from_text: FROM_TEXT[ID].swap(0, Ordering::SeqCst), from_tree: FROM_TREE[ID].swap(0, Ordering::SeqCst) }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ProbeSpec<const ID: usize> {
    value: String,
}

impl<'de, const ID: usize> Deserialize<'de> for ProbeSpec<ID> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // serde_json's tree deserializers (`Value`, `&Value` and their map and
        // sequence accessors) live in `serde_json::value`; its text
        // deserializer reads the stored body directly.
        if std::any::type_name::<D>().contains("serde_json::value") {
            FROM_TREE[ID].fetch_add(1, Ordering::SeqCst);
        } else {
            FROM_TEXT[ID].fetch_add(1, Ordering::SeqCst);
        }
        #[derive(Deserialize)]
        struct Fields {
            value: String,
        }
        Ok(Self { value: Fields::deserialize(deserializer)?.value })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeStatus {
    phase: String,
}

enum ProbePatch {}
impl StatusPatch<ProbeStatus> for ProbePatch {
    fn apply(&self, _: &mut ProbeStatus) {
        match *self {}
    }
}

struct Probe<const ID: usize>;
impl<const ID: usize> Resource for Probe<ID> {
    type Spec = ProbeSpec<ID>;
    type Status = ProbeStatus;
    type StatusPatch = ProbePatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "decodeprobes", kind: "DecodeProbe" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::ConvergentFacts;
}

const NAMESPACE: &str = "decode";

fn labelled(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(BTreeMap::from([("probe".to_string(), "yes".to_string())])).build()
}

fn spec<const ID: usize>(value: &str) -> ProbeSpec<ID> {
    ProbeSpec { value: value.to_string() }
}

fn replica_object<const ID: usize>(name: &str) -> ResourceObject<Probe<ID>> {
    ResourceObject {
        metadata: ObjectMeta {
            name: name.to_string(),
            namespace: NAMESPACE.to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::from([("probe".to_string(), "yes".to_string())]),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec: spec(name),
        status: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    Memory,
}

impl Store {
    fn open(self) -> ResourceBackend {
        let backend = match self {
            Self::Memory => ResourceBackend::new(InMemoryBackend::default()),
        };
        backend.with_local_root(NodeId::new("decode-root"))
    }

    /// The memory store holds trees: one borrowed typed decode of the tree.
    fn once_per_row(self, rows: usize) -> Decodes {
        match self {
            Self::Memory => Decodes { from_text: 0, from_tree: rows },
        }
    }
}

// Behaviour (#3154): every typed read path (get, list, label list, declared
// query, replica list and get, and watch replay) deserializes each stored row
// into the caller's type exactly once; SQLite does so straight from the stored
// text, without a throwaway validation pass or a `Value` round trip. Raw reads
// validate each row with one typed pass and keep the stored tree.
//
// Generator: local objects 0..=4, extra updates 0..=3 (each one more replayed
// event), replica objects 0..=3 from one origin, and both stores, so empty,
// single and several rows, replay longer than the object count and a store
// with no replicas are all drawn. Invariants are checked after every read.
#[hegel::test]
fn typed_reads_decode_each_row_once(tc: hegel::TestCase) {
    const ID: usize = 0;
    let store = Store::Memory;
    let locals = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let updates = if locals == 0 { 0 } else { tc.draw(gs::integers::<usize>().min_value(0).max_value(3)) };
    let replicas = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));

    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = store.open();
        let resolver = backend.using::<Probe<ID>>(NAMESPACE);
        let names: Vec<String> = (0..locals).map(|index| format!("local-{index}")).collect();
        for name in &names {
            resolver.create(&labelled(name), &spec(name)).await.expect("create");
        }
        for update in 0..updates {
            let current = resolver.get(&names[0]).await.expect("read for update");
            resolver.update(&labelled(&names[0]), &current.metadata.resource_version, &spec(&format!("v{update}"))).await.expect("update");
        }
        let origin = NodeId::new("decode-origin");
        let replica_names: Vec<String> = (0..replicas).map(|index| format!("replica-{index}")).collect();
        for name in &replica_names {
            backend
                .store()
                .apply_replica_typed(&origin, NAMESPACE, StoredReplicaEventKind::Added, &replica_object::<ID>(name), Utc::now())
                .await
                .expect("apply replica");
        }
        take_decodes::<ID>();

        let listed = resolver.list().await.expect("list");
        assert_eq!(listed.items.len(), locals);
        assert_eq!(take_decodes::<ID>(), store.once_per_row(locals), "list");

        for name in &names {
            resolver.get(name).await.expect("get");
            assert_eq!(take_decodes::<ID>(), store.once_per_row(1), "get {name}");
        }

        let required = BTreeMap::from([("probe".to_string(), "yes".to_string())]);
        assert_eq!(
            backend.store().list_typed_matching_labels::<Probe<ID>>(NAMESPACE, &required).await.expect("label list").items.len(),
            locals
        );
        assert_eq!(take_decodes::<ID>(), store.once_per_row(locals), "label list");

        let queried = backend.query_declared::<Probe<ID>>(NAMESPACE, &StoreQuery::labels(&required, true, true)).await.expect("query");
        assert_eq!(queried.items.len(), locals + replicas);
        assert_eq!(take_decodes::<ID>(), store.once_per_row(locals + replicas), "declared query");

        assert_eq!(backend.store().list_replicas_typed::<Probe<ID>>(NAMESPACE).await.expect("replica list").len(), replicas);
        assert_eq!(take_decodes::<ID>(), store.once_per_row(replicas), "replica list");
        for name in &replica_names {
            assert_eq!(backend.store().get_replicas_typed::<Probe<ID>>(NAMESPACE, name).await.expect("replica get").len(), 1);
            assert_eq!(take_decodes::<ID>(), store.once_per_row(1), "replica get {name}");
        }

        let events = locals + updates;
        let mut replay = resolver.watch(WatchStart::FromVersion("0".to_string())).await.expect("replay");
        for _ in 0..events {
            tokio::time::timeout(Duration::from_secs(5), replay.next()).await.expect("replayed event").expect("open").expect("decoded");
        }
        assert_eq!(take_decodes::<ID>(), store.once_per_row(events), "replay");

        // Raw readers keep the stored tree; validating it is one typed pass.
        let raw = backend.store().list_raw(ResourceKind::of::<Probe<ID>>(), NAMESPACE).await.expect("raw list");
        assert_eq!(raw.items.len(), locals);
        assert_eq!(take_decodes::<ID>(), Decodes { from_text: 0, from_tree: locals }, "raw list");
    });
}

// Behaviour (#3154, ADR 0047): raw readers receive the stored spec tree as
// written, so a field this generation does not know survives a raw read for
// pass-through; typed readers decode the same row into the registered type.
#[tokio::test]
async fn raw_reads_keep_unknown_fields_and_typed_reads_ignore_them() {
    const ID: usize = 3;
    for store in [Store::Memory] {
        let backend = store.open();
        let kind = ResourceKind::of::<Probe<ID>>();
        let stored = replica_object::<ID>("future");
        let object = ResourceObject::<SerializedResource> {
            metadata: stored.metadata,
            spec: serde_json::json!({"value": "future", "addedNextGeneration": {"nested": [1, 2]}}),
            status: None,
        };
        backend
            .store()
            .apply_replica_raw(kind, &NodeId::new("decode-origin"), NAMESPACE, StoredReplicaEventKind::Added, &object, Utc::now())
            .await
            .expect("apply replica with an unknown field");

        let raw = backend.store().list_replicas_raw(kind, NAMESPACE).await.expect("raw replicas");
        assert_eq!(raw.len(), 1, "{store:?}");
        assert_eq!(raw[0].object.spec, object.spec, "{store:?}: raw read keeps the stored tree");

        let typed = backend.store().list_replicas_typed::<Probe<ID>>(NAMESPACE).await.expect("typed replicas");
        assert_eq!(typed.len(), 1, "{store:?}");
        assert_eq!(typed[0].object.spec, spec::<ID>("future"), "{store:?}");
    }
}

// Behaviour (#3154): a stored `status: null` decodes to no status on the typed
// path, exactly as an absent status does, in both stores.
#[tokio::test]
async fn typed_reads_decode_null_status_as_none() {
    const ID: usize = 4;
    for store in [Store::Memory] {
        let backend = store.open();
        let kind = ResourceKind::of::<Probe<ID>>();
        let stored = replica_object::<ID>("null-status");
        let object = ResourceObject::<SerializedResource> {
            metadata: stored.metadata,
            spec: serde_json::json!({"value": "null-status"}),
            status: Some(serde_json::Value::Null),
        };
        backend
            .store()
            .apply_replica_raw(kind, &NodeId::new("decode-origin"), NAMESPACE, StoredReplicaEventKind::Added, &object, Utc::now())
            .await
            .expect("apply replica with a null status");

        let typed = backend.store().list_replicas_typed::<Probe<ID>>(NAMESPACE).await.expect("typed replicas");
        assert_eq!(typed.len(), 1, "{store:?}");
        assert_eq!(typed[0].object.status, None, "{store:?}");
    }
}

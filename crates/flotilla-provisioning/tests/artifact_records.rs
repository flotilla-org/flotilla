//! Artifact's reads and writes through the store's typed paths: one contract, run against the old
//! store and against the engine's records installed behind `[primitives]`, so Artifact's writers
//! (put, the brief, a build log, the reaper) and readers keep their behaviour on either.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::{TimeZone, Utc};
use flotilla_provisioning::engine::artifacts::EngineArtifacts;
use flotilla_resources::{Artifact, ArtifactSpec, Clock, InputMeta, SystemClock};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn spec(convoy: &str, digest: &str) -> ArtifactSpec {
    ArtifactSpec::builder()
        .convoy(convoy.into())
        .producer("coder".into())
        .kind("decision-ledger".into())
        .subject(convoy.into())
        .summary(BTreeMap::from([("approved".to_owned(), serde_json::json!(true)), ("round".to_owned(), serde_json::json!(2))]))
        .digest(digest.into())
        .size(43)
        .media_type("text/markdown".into())
        .recorded_at(Utc.timestamp_opt(1_800_000_000, 0).single().expect("time"))
        .expires_at(Utc.timestamp_opt(1_900_000_000, 0).single().expect("time"))
        .build()
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend) {
    let artifacts = backend.using::<Artifact>(NS);
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "Artifact").await.expect("watch");

    // A put creates the slot's record; every read finds it with its spec.
    let created = artifacts.create(&InputMeta::builder().name("ledger".into()).build(), &spec("demo", "one")).await.expect("put");
    artifacts.create(&InputMeta::builder().name("other".into()).build(), &spec("elsewhere", "two")).await.expect("put another");
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");
    let read = backend.including_replicas::<Artifact>(NS).get("ledger").await.expect("read").object;
    assert_eq!(read.spec, spec("demo", "one"));

    // A convoy's artifacts are found by its name.
    let query = StoreQuery { fields: BTreeMap::from([("spec.convoy".into(), "demo".into())]), ..Default::default() };
    let found = artifacts.query_declared(&query).await.expect("query");
    assert_eq!(found.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), ["ledger"]);

    // A later put publishes the latest body at the same address.
    let replaced = artifacts.update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &spec("demo", "three")).await;
    let replaced = replaced.expect("replace");
    assert_eq!(artifacts.get("ledger").await.expect("get").spec.digest, "three");

    // A put at a version that moved on, or one that changes the address, is refused.
    assert!(artifacts
        .update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &spec("demo", "four"))
        .await
        .is_err());
    let mut moved = spec("demo", "four");
    moved.subject = "another".into();
    assert!(artifacts.update(&InputMeta::from(&replaced.metadata), &replaced.metadata.resource_version, &moved).await.is_err());

    // The reaper deletes a record; it is gone from every read.
    artifacts.delete("ledger").await.expect("delete");
    assert!(artifacts.get("ledger").await.is_err());
    assert!(backend.including_replicas::<Artifact>(NS).get("ledger").await.is_err());
    assert_eq!(artifacts.list().await.expect("list").items.len(), 1);
}

#[tokio::test]
async fn the_old_store_keeps_the_artifact_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn the_engine_keeps_the_artifact_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::artifact::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineArtifacts::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    contract(&backend).await;
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Artifact>(), NS).await.expect("old rows").items.is_empty());
}

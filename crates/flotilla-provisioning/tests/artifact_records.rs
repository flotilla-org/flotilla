//! Artifact's reads and writes through the store's typed paths, served by the engine's records,
//! the daemon's only path for the kind. Artifact's writers (put, the brief, a build log, the
//! reaper) and readers hold to one contract; it also runs against the plain in-memory store that
//! tests stand in with.

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

    // Each body's digest, as a put names it.
    let [one, two, three, four] = ["1", "2", "3", "4"].map(|digit| digit.repeat(64));

    // A put creates the slot's record; every read finds it with its spec.
    let created = artifacts.create(&InputMeta::builder().name("ledger".into()).build(), &spec("demo", &one)).await.expect("put");
    artifacts.create(&InputMeta::builder().name("other".into()).build(), &spec("elsewhere", &two)).await.expect("put another");
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");
    let read = backend.including_replicas::<Artifact>(NS).get("ledger").await.expect("read").object;
    assert_eq!(read.spec, spec("demo", &one));

    // A convoy's artifacts are found by its name.
    let query = StoreQuery { fields: BTreeMap::from([("spec.convoy".into(), "demo".into())]), ..Default::default() };
    let found = artifacts.query_declared(&query).await.expect("query");
    assert_eq!(found.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), ["ledger"]);

    // A later put publishes the latest body at the same address.
    let replaced = artifacts.update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &spec("demo", &three)).await;
    let replaced = replaced.expect("replace");
    assert_eq!(artifacts.get("ledger").await.expect("get").spec.digest, three);

    // A put at a version that moved on, or one that changes the address, is refused.
    assert!(artifacts.update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &spec("demo", &four)).await.is_err());
    let mut moved = spec("demo", &four);
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

fn home(directory: &tempfile::TempDir, home: &str) -> (Arc<Service>, Arc<EngineArtifacts>) {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::artifact::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join(format!("{home}.sqlite")), home, kinds).expect("store"));
    let records = EngineArtifacts::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    (service, Arc::new(records))
}

#[derive(Debug)]
struct ManualClock(std::sync::Mutex<chrono::DateTime<Utc>>);

impl ManualClock {
    fn set(&self, seconds: i64) {
        *self.0.lock().expect("clock") = Utc.timestamp_opt(seconds, 0).single().expect("time");
    }
}

impl Clock for ManualClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

/// `home`'s engine artifacts behind a backend, written at the time `clock` says.
fn backend_at(directory: &tempfile::TempDir, home: &str, clock: &Arc<ManualClock>) -> (Arc<Service>, ResourceBackend) {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::artifact::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join(format!("{home}.sqlite")), home, kinds).expect("store"));
    let records = EngineArtifacts::open(Arc::clone(&service), Arc::clone(clock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    (service, backend)
}

// Behaviour (#3108 creation time): an engine record reads the time it was created as its
// creation timestamp, from the write's answer and from every read; a later write keeps it, and
// another home reads the home's time, not when the record arrived there.
#[tokio::test]
async fn an_engine_record_reads_its_creation_time_at_every_home() {
    let directory = tempfile::tempdir().expect("temp dir");
    let created_at = Utc.timestamp_opt(1_000, 0).single().expect("time");
    let clock = Arc::new(ManualClock(std::sync::Mutex::new(created_at)));
    let (beta, beta_backend) = backend_at(&directory, "beta", &clock);
    let (zeta, zeta_backend) = backend_at(&directory, "zeta", &clock);
    let artifacts = beta_backend.using::<Artifact>(NS);
    let [one, two] = ["1", "2"].map(|digit| digit.repeat(64));

    let created = artifacts.create(&InputMeta::builder().name("ledger".into()).build(), &spec("demo", &one)).await.expect("put");
    assert_eq!(created.metadata.creation_timestamp, created_at);
    clock.set(2_000);
    artifacts.update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &spec("demo", &two)).await.expect("replace");
    assert_eq!(artifacts.get("ledger").await.expect("get").metadata.creation_timestamp, created_at);
    let listed = artifacts.list().await.expect("list").items;
    assert_eq!(listed.iter().map(|item| item.metadata.creation_timestamp).collect::<Vec<_>>(), [created_at]);

    clock.set(5_000);
    pump(&beta, "beta", &zeta, None);
    let replica = zeta_backend.including_replicas::<Artifact>(NS).get("ledger").await.expect("read at zeta").object;
    assert_eq!(replica.metadata.creation_timestamp, created_at);
}

/// One round of `from`'s stream applied at `to`, as a follower does; a reset round when `cursor` is none.
fn pump(from: &Service, from_home: &str, to: &Service, cursor: Option<u64>) -> u64 {
    let mut frames = Vec::new();
    let next = from
        .serve(cursor, flotilla_schema::Timestamp::from_datetime(&Utc::now()), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
    to.with_store(|store| {
        flotilla_schema_sqlite::stream::Follower::new(store).receive(from_home, |sink| frames.iter().try_for_each(|frame| sink(frame)))
    })
    .expect("follow");
    next
}

// Behaviour (one home writes, every home reads): an artifact another home wrote reads at this home
// once its log arrives; a name both homes hold reads as this home's own, every home's copies are
// listed as sources, and only this home's own copy is deleted here.
#[tokio::test]
async fn another_homes_artifacts_read_here_and_this_homes_own_win() {
    use flotilla_resources::engine_records::{KindRecords, Record};
    let directory = tempfile::tempdir().expect("temp dir");
    let (zeta, zeta_records) = home(&directory, "zeta");
    let (beta, beta_records) = home(&directory, "beta");
    let put = |records: &Arc<EngineArtifacts>, name: &'static str, digest: &'static str| {
        let records = Arc::clone(records);
        async move { records.apply(name, None, &BTreeMap::new(), &BTreeMap::new(), &spec("demo", digest)).await.expect("put") }
    };
    put(&beta_records, "shared", "beta").await;
    put(&beta_records, "remote", "beta").await;
    put(&zeta_records, "shared", "zeta").await;
    let cursor = pump(&beta, "beta", &zeta, None);

    let digest = |object: Option<Record<Artifact>>| object.map(|object| (object.home, object.spec.digest));
    assert_eq!(digest(zeta_records.get("shared").await.expect("get")), Some(("zeta".into(), "zeta".into())));
    assert_eq!(digest(zeta_records.get("remote").await.expect("get")), Some(("beta".into(), "beta".into())));
    assert_eq!(zeta_records.list().await.expect("list").len(), 2);
    assert_eq!(zeta_records.sources().await.expect("sources").len(), 3);

    zeta_records.delete("shared").await.expect("delete own");
    zeta_records.delete("remote").await.expect("another home's copy is not this home's to delete");
    assert_eq!(digest(zeta_records.get("shared").await.expect("get")), Some(("beta".into(), "beta".into())));
    beta_records.delete("remote").await.expect("delete at its home");
    pump(&beta, "beta", &zeta, Some(cursor));
    assert_eq!(digest(zeta_records.get("remote").await.expect("get")), None);
}

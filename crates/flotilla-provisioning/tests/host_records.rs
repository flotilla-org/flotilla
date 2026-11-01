//! Host's reads and writes through the store's typed paths behind `[primitives]`: the engine's
//! records are the only copy, an observation is written only when a value changes, and a Host
//! reads back with the judgements its readers expect.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::provider::{EnvironmentClass, ProviderAdvertisement, ProviderKind, ProviderReadiness};
use flotilla_provisioning::{
    engine::hosts::{self, EngineHosts},
    stored::host::{HostObservation, HostRecord},
};
use flotilla_resources::{
    Clock, ConditionValue, FulfilmentFacts, Host, HostCondition, HostConnection, HostSpec, HostStatus, InputMeta, SystemClock,
};
use flotilla_schema::Kind;
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use serde_json::json;

const NS: &str = "flotilla";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + seconds, 0).single().expect("time")
}

fn condition(condition_type: &str, value: ConditionValue, message: &str, observed_at: DateTime<Utc>) -> HostCondition {
    HostCondition::builder()
        .condition_type(condition_type)
        .value(value)
        .reason("Probed")
        .message(message)
        .observed_at(observed_at)
        .blocks_forge_observation(false)
        .build()
}

/// What this host's observers report at `time`: the same observation each time, but for the time.
fn observed(time: DateTime<Utc>, free_space: &str) -> HostStatus {
    let provider = ProviderAdvertisement {
        instance: "docker".into(),
        kind: Some(ProviderKind::new(EnvironmentClass::Container, "docker")),
        readiness: ProviderReadiness::Ready,
        ..ProviderAdvertisement::default()
    };
    HostStatus {
        heartbeat_at: Some(time),
        ready: true,
        daemon_generation: Some("g7".into()),
        daemon_rss_bytes: Some(time.timestamp() as u64),
        disk_free_bytes: Some(5),
        admission_free_space_floor_bytes: Some(10),
        description: Some(
            serde_json::from_value(json!({
                "environment_id": "kiwi", "node": {"node_id": "n", "display_name": "kiwi"}, "system": {"arch": "aarch64"}
            }))
            .expect("summary"),
        ),
        conditions: vec![
            condition("Capacity/FreeSpace", ConditionValue::True, free_space, time),
            HostCondition { blocks_readiness: false, ..condition("ResourceStore/ReadRetries", ConditionValue::False, "3 retries", time) },
        ],
        capabilities: BTreeMap::from([
            ("environment_providers".to_owned(), json!([provider])),
            ("agent_adapters".to_owned(), json!(["claude-code"])),
            ("held_credentials".to_owned(), json!(["claude-max"])),
            ("image_digests".to_owned(), json!({"docker": ["sha256:a"]})),
            ("io_pressure".to_owned(), json!({"some": time.timestamp()})),
        ]),
        fulfilment_facts: BTreeMap::from([(
            "docker-on-kiwi".to_owned(),
            FulfilmentFacts { gui_session_logged_in: true, observed_at: time, ..FulfilmentFacts::default() },
        )]),
        ..HostStatus::default()
    }
}

fn engine(directory: &std::path::Path) -> (Arc<Service>, ResourceBackend) {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::host::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineHosts::open(Arc::clone(&service), "kiwi".into(), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    (service, backend)
}

fn last_writer(service: &Service, code: flotilla_schema::KindCode, name: &str) -> Option<String> {
    service
        .with_store(|store| {
            let mut read = store.read()?;
            let id = read.resolve(code, name, "alpha")?.expect("held");
            Ok(read.last_change(id)?.map(|change| change.writer))
        })
        .expect("read")
}

// Behaviour (#3824 layer 4d): an observation is written only when a value changes, never for the
// time alone. The observers' heartbeat, a figure that changes every pass and a diagnostic condition
// write nothing; a condition whose value stands keeps the reason and message it took that value
// with; a changed value is one write.
#[tokio::test]
async fn an_observation_is_written_only_when_a_value_changes() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (service, backend) = engine(directory.path());
    let hosts = backend.using::<Host>(NS);
    let host = hosts.create(&InputMeta { name: "kiwi".into(), ..Default::default() }, &HostSpec::default()).await.expect("create");

    let first = hosts.update_status("kiwi", &host.metadata.resource_version, &observed(at(0), "12 GB free")).await.expect("first");
    assert_eq!(last_writer(&service, <HostObservation as Kind>::CODE, "kiwi").as_deref(), Some("host-observer"));
    let mut version = first.metadata.resource_version.clone();
    for pass in 1..=4 {
        let again = hosts.update_status("kiwi", &version, &observed(at(30 * pass), "11 GB free")).await.expect("again");
        assert_eq!(again.metadata.resource_version, version, "pass {pass} changed nothing recorded, so wrote nothing");
        version = again.metadata.resource_version;
    }
    let read = hosts.get("kiwi").await.expect("read").status.expect("status");
    let free = read.conditions.iter().find(|condition| condition.condition_type == "Capacity/FreeSpace").expect("kept");
    assert_eq!(free.message, "12 GB free", "a condition whose value stands keeps its message");
    assert!(read.conditions.iter().all(|condition| condition.condition_type != "ResourceStore/ReadRetries"), "diagnostics are metrics");

    let mut flipped = observed(at(150), "0 GB free");
    flipped.conditions[0].value = ConditionValue::False;
    let written = hosts.update_status("kiwi", &version, &flipped).await.expect("flipped");
    assert_ne!(written.metadata.resource_version, version, "a changed value is written");
    let status = written.status.expect("status");
    let free = status.conditions.iter().find(|condition| condition.condition_type == "Capacity/FreeSpace").expect("kept");
    assert_eq!((free.value, free.message.as_str()), (ConditionValue::False, "0 GB free"));
    assert!(!status.ready, "a blocking condition holds readiness");

    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Host>(), NS).await.expect("old rows").items.is_empty());
}

// Behaviour (#3824 layer 4d): a Host reads back as the old object. Its heartbeat is when its home
// was last heard, now for this home, and it is ready while that is fresh and no condition blocks;
// its capabilities are rebuilt from the observation, an SSH host's markers from its connection;
// its summary carries the architecture, and its disk figures stand in for the floor flag.
#[tokio::test]
async fn a_host_reads_back_with_its_judgements() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_, backend) = engine(directory.path());
    let hosts = backend.using::<Host>(NS);
    let host = hosts.create(&InputMeta { name: "kiwi".into(), ..Default::default() }, &HostSpec::default()).await.expect("create");
    let before = Utc::now();
    let written = hosts.update_status("kiwi", &host.metadata.resource_version, &observed(at(0), "12 GB free")).await.expect("status");
    let status = written.status.expect("status");
    assert!(status.heartbeat_at.is_some_and(|heard| heard >= before), "this home is heard now");
    assert!(status.ready);
    assert!(status.fulfilment_facts["docker-on-kiwi"].observed_at >= before);
    assert_eq!(status.daemon_generation.as_deref(), Some("g7"));
    assert_eq!(status.description.and_then(|summary| summary.system.arch).as_deref(), Some("arm64"));
    assert_eq!((status.disk_free_bytes, status.admission_free_space_floor_bytes), (Some(0), Some(1)), "below the floor");
    assert_eq!(status.capabilities["agent_adapters"], json!(["claude-code"]));
    assert_eq!(status.capabilities["held_credentials"], json!(["claude-max"]));
    assert_eq!(status.capabilities["image_digests"], json!({"docker": ["sha256:a"]}));
    let providers: Vec<ProviderAdvertisement> =
        serde_json::from_value(status.capabilities["environment_providers"].clone()).expect("providers");
    assert_eq!(providers[0].kind.as_ref().map(|kind| kind.class), Some(EnvironmentClass::Container));
    assert!(!status.capabilities.contains_key("io_pressure"), "metrics are not recorded");

    let ssh = HostSpec {
        connection: HostConnection::SshHost { owning_daemon: "kiwi".into(), destination: "crew@b1".into() },
        ..HostSpec::default()
    };
    let b1 = hosts.create(&InputMeta { name: "b1".into(), ..Default::default() }, &ssh).await.expect("an SSH host kiwi owns");
    let b1 = hosts.update_status("b1", &b1.metadata.resource_version, &HostStatus::default()).await.expect("status");
    let capabilities = b1.status.expect("status").capabilities;
    assert_eq!((capabilities["ssh_host"].clone(), capabilities["owning_daemon"].clone()), (json!(true), json!("kiwi")));
}

// Behaviour (#3824 layer 4d): another home's Host is ready while that home was heard within the
// heartbeat TTL and nothing blocks; never heard, or heard too long ago, it is not.
#[test]
fn another_homes_host_is_ready_while_its_home_is_heard() {
    let observation = HostObservation::default();
    let judged = |heard: Option<DateTime<Utc>>| hosts::status("b1", "beta", &HostSpec::default(), &observation, heard, at(100)).ready;
    assert!(judged(Some(at(70))));
    assert!(!judged(Some(at(30))), "heard 70 s ago");
    assert!(!judged(None), "never heard");
}

// Behaviour (#3824 layer 4d): a Host is written only at its home. A daemon's Host is refused at
// another daemon, an SSH host at any daemon but its owner, and no Host's status is written where
// its Host is not held.
#[tokio::test]
async fn a_host_is_written_only_at_its_home() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (service, backend) = engine(directory.path());
    let hosts = backend.using::<Host>(NS);
    let meta = |name: &str| InputMeta { name: name.into(), ..Default::default() };
    let error = hosts.create(&meta("feta"), &HostSpec::default()).await.expect_err("feta's own daemon is its home");
    assert!(error.to_string().contains("homed at its own daemon"), "{error}");
    let owned = HostSpec {
        connection: HostConnection::SshHost { owning_daemon: "feta".into(), destination: "crew@b2".into() },
        ..HostSpec::default()
    };
    let error = hosts.create(&meta("b2"), &owned).await.expect_err("feta owns b2");
    assert!(error.to_string().contains("owning daemon `feta`"), "{error}");
    assert!(hosts.update_status("feta", "0", &HostStatus::default()).await.is_err());

    let kiwi = hosts.create(&meta("kiwi"), &HostSpec { max_vessels: Some(2), ..HostSpec::default() }).await.expect("kiwi's own");
    assert_eq!(kiwi.spec.max_vessels, Some(2));
    assert_eq!(last_writer(&service, <HostRecord as Kind>::CODE, "kiwi").as_deref(), Some("host-spec"));
    hosts.delete("kiwi").await.expect("delete");
    assert!(hosts.get("kiwi").await.is_err());
}

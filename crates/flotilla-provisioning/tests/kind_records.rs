//! FulfilmentKind's reads and writes through the store's typed paths: one contract, run against
//! the old store and against the engine's records installed behind `[primitives]`.

use std::{collections::BTreeMap, sync::Arc};

use flotilla_provisioning::{engine::kinds::EngineKinds, stored::placement::FulfilmentKindDefinition};
use flotilla_resources::{
    definition_provenance::SEEDING_MANAGER, Clock, DockerImageSource, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec,
    FulfilmentRealisation, InputMeta, SystemClock, CONVOY_LABEL, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
};
use flotilla_schema::{ChangeHeader, Kind};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NS: &str = "flotilla";

fn kind(pool: &str, realisation: FulfilmentRealisation) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref("kiwi".into())
        .pool(pool.into())
        .grants([FulfilmentGrant::platform("linux".into()), FulfilmentGrant::network("scoped".into())].into())
        .realisation(realisation)
        .build()
}

fn labelled(name: &str, labels: &[(&str, &str)]) -> InputMeta {
    InputMeta::builder()
        .name(name.to_owned())
        .labels(labels.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<BTreeMap<_, _>>())
        .build()
}

fn snapshot_labels(convoy: &str) -> [(&'static str, String); 2] {
    [(PREPARED_SNAPSHOT_LABEL, PLACEMENT_SNAPSHOT_KIND.to_owned()), (CONVOY_LABEL, convoy.to_owned())]
}

async fn contract(backend: &ResourceBackend) {
    let kinds = backend.clone().using::<FulfilmentKind>(NS);
    let docker = kind("cleat", FulfilmentRealisation::DockerPerVessel { image: Some(DockerImageSource::Literal("ubuntu:24.04".into())) });
    let seeded = kinds.create(&labelled("docker-on-kiwi", &[(MANAGED_BY_LABEL, SEEDING_MANAGER)]), &docker).await.expect("seed");
    assert_eq!(seeded.spec, docker);

    let frozen = kind(
        "cleat",
        FulfilmentRealisation::DockerPerVessel {
            image: Some(DockerImageSource::Built { identity: "identity".into(), image_build_ref: "image-identity-amd64".into() }),
        },
    );
    let [(snapshot, kind_value), (convoy, convoy_value)] = snapshot_labels("convoy-0001");
    kinds
        .create(&labelled("placement-snapshot-012345abcdef", &[(snapshot, &kind_value), (convoy, &convoy_value)]), &frozen)
        .await
        .expect("snapshot");
    kinds.create(&labelled("host-direct-kiwi", &[]), &kind("passthrough", FulfilmentRealisation::HostDirect)).await.expect("operator's");

    let size = flotilla_resources::CloudSandboxSize { cpus: 4, memory_bytes: 8 << 30, disk_bytes: 10 << 30 };
    let cloud = kind("cleat", FulfilmentRealisation::CloudSandboxPerVessel { image: "image@sha256:digest".into(), size: Some(size) });
    kinds.create(&labelled("cloud-sized", &[]), &cloud).await.expect("cloud");
    assert_eq!(kinds.get("cloud-sized").await.expect("get cloud").spec, cloud, "the policy's size survives storage");

    let read = kinds.get("placement-snapshot-012345abcdef").await.expect("get snapshot");
    assert_eq!(read.spec, frozen);
    assert_eq!(read.metadata.labels.get(PREPARED_SNAPSHOT_LABEL).map(String::as_str), Some(PLACEMENT_SNAPSHOT_KIND));
    assert_eq!(read.metadata.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-0001"));
    let read = kinds.get("docker-on-kiwi").await.expect("get seeded");
    assert_eq!(read.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(SEEDING_MANAGER));

    let mut names: Vec<_> = kinds.list().await.expect("list").items.into_iter().map(|kind| kind.metadata.name).collect();
    names.sort();
    assert_eq!(names, ["cloud-sized", "docker-on-kiwi", "host-direct-kiwi", "placement-snapshot-012345abcdef"]);
    assert_eq!(backend.including_replicas::<FulfilmentKind>(NS).list().await.expect("replicas").items.len(), 4);

    let mut observed = read.spec.clone();
    observed.grants.insert(FulfilmentGrant::gui_session());
    let updated = kinds.update(&InputMeta::from(&read.metadata), &read.metadata.resource_version, &observed).await.expect("update");
    assert_eq!(kinds.get("docker-on-kiwi").await.expect("updated").spec, observed);
    assert_eq!(updated.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(SEEDING_MANAGER));
    assert!(kinds.update(&InputMeta::from(&read.metadata), &read.metadata.resource_version, &observed).await.is_err(), "stale version");

    kinds.delete("placement-snapshot-012345abcdef").await.expect("delete");
    assert!(kinds.get("placement-snapshot-012345abcdef").await.is_err());
    assert_eq!(kinds.list().await.expect("list").items.len(), 3);
}

#[tokio::test]
async fn the_old_store_keeps_the_kind_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

fn engine(directory: &tempfile::TempDir) -> (Arc<Service>, ResourceBackend) {
    let mut kinds = flotilla_engine_store::registry().expect("engine kinds");
    flotilla_provisioning::stored::placement::register(&mut kinds).expect("register kinds");
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineKinds::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    (service, backend)
}

#[tokio::test]
async fn the_engine_keeps_the_kind_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_, backend) = engine(&directory);
    contract(&backend).await;
    assert!(
        backend.store().list_raw(flotilla_store::ResourceKind::of::<FulfilmentKind>(), NS).await.expect("old rows").items.is_empty(),
        "nothing reached the old store"
    );
}

// A kind holds only its fields: who wrote it is its change. A seeded kind is `seeding`'s; a
// placement snapshot is `admission`'s with the Convoy it was frozen for as its item; an
// operator's kind is the operator's.
#[tokio::test]
async fn on_the_engine_each_writer_is_its_change() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (service, backend) = engine(&directory);
    let kinds = backend.clone().using::<FulfilmentKind>(NS);
    let host_direct = kind("passthrough", FulfilmentRealisation::HostDirect);
    kinds.create(&labelled("host-direct-kiwi", &[(MANAGED_BY_LABEL, SEEDING_MANAGER)]), &host_direct).await.expect("seed");
    let [(snapshot, kind_value), (convoy, convoy_value)] = snapshot_labels("convoy-0001");
    kinds
        .create(&labelled("placement-snapshot-012345abcdef", &[(snapshot, &kind_value), (convoy, &convoy_value)]), &host_direct)
        .await
        .expect("snapshot");
    kinds.create(&labelled("operator-kind", &[]), &host_direct).await.expect("operator's");
    let last_change = |name: &str| -> ChangeHeader {
        service
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(<FulfilmentKindDefinition as Kind>::CODE, name, "alpha")?;
                read.last_change(id.expect("stored"))
            })
            .expect("read")
            .expect("a change")
    };
    assert_eq!(last_change("host-direct-kiwi").writer, "seeding");
    assert!(last_change("host-direct-kiwi").items.is_empty());
    let frozen = last_change("placement-snapshot-012345abcdef");
    assert_eq!(frozen.writer, "admission");
    let items: Vec<_> = frozen.items.iter().map(|item| (item.record.name.as_str(), item.record.home.as_str())).collect();
    assert_eq!(items, [("convoy-0001", "alpha")]);
    assert_eq!(
        frozen.items[0].record.kind,
        <flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy> as Kind>::CODE
    );
    assert_eq!(last_change("operator-kind").writer, "operator");
}

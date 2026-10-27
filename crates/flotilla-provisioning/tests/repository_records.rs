//! Repository's reads and writes through the store's typed paths (#3824 layer 3a): one contract,
//! run against the old store and against the engine's records installed behind `[primitives]`,
//! so Repository's writers (ensure, project ops, the reconciler's status) and readers keep their
//! behaviour on either.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_provisioning::engine::repositories::EngineRepositories;
use flotilla_resources::{
    definition_provenance::{VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION},
    Clock, DefaultBranchObservation, DefaultBranchProvenance, InputMeta, Repository, RepositorySpec, RepositoryStatus, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{ensure_repository, watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn status(branch: &str) -> RepositoryStatus {
    RepositoryStatus {
        default_branch: Some(branch.into()),
        default_branch_observations: vec![DefaultBranchObservation { branch: branch.into(), provenance: DefaultBranchProvenance::Forge }],
        ..Default::default()
    }
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend) {
    let repositories = backend.using::<Repository>(NS);
    let spec = RepositorySpec::remote("https://github.com/example/app").expect("spec");
    let key = spec.key();
    let name = key.to_string();
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "Repository").await.expect("watch");

    // An observed repository is ensured once; ensuring it again finds it.
    ensure_repository(&repositories, &key, &spec).await.expect("ensure");
    let created = ensure_repository(&repositories, &key, &spec).await.expect("ensure again");
    assert_eq!(created.spec, spec);
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");

    // The reconciler's observed default branch is its status.
    let observed = repositories.update_status(&name, &created.metadata.resource_version, &status("main")).await.expect("status");
    assert_eq!(observed.status.as_ref().and_then(|status| status.default_branch.as_deref()), Some("main"));

    // Project ops declare verification commands with their project and provenance; the status stays.
    let provenance = r#"[{"sourceRepository":"ops","sourceCommit":"abc","entryPath":".flotilla/ops/checks.yaml"}]"#;
    let mut meta = InputMeta::from(&observed.metadata);
    meta.annotations.insert(VERIFICATION_PROJECT_ANNOTATION.into(), "app".into());
    meta.annotations.insert(VERIFICATION_PROVENANCE_ANNOTATION.into(), provenance.into());
    let commands = BTreeMap::from([("test".to_owned(), "cargo test".to_owned())]);
    let declared = repositories
        .update(&meta, &observed.metadata.resource_version, &spec.clone().with_verification_commands(commands.clone()))
        .await
        .expect("declare commands");
    let read = backend.including_replicas::<Repository>(NS).get(&name).await.expect("read").object;
    assert_eq!(read.spec.verification_commands(), &commands);
    assert_eq!(read.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).map(String::as_str), Some("app"));
    let read_provenance: serde_json::Value =
        serde_json::from_str(&read.metadata.annotations[VERIFICATION_PROVENANCE_ANNOTATION]).expect("json");
    assert_eq!(read_provenance, serde_json::from_str::<serde_json::Value>(provenance).expect("json"));
    assert_eq!(read.status.and_then(|status| status.default_branch).as_deref(), Some("main"), "a spec write keeps the observations");

    // Project ops find the repositories a project materialised by its annotation.
    let query = StoreQuery {
        fields: BTreeMap::from([("metadata.annotations.\"flotilla.work/verification-command-project\"".into(), "app".into())]),
        ..Default::default()
    };
    let found = repositories.query_declared(&query).await.expect("query");
    assert_eq!(found.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), [name.as_str()]);

    // A write at a version that moved on is refused.
    assert!(repositories.update(&meta, &observed.metadata.resource_version, &spec).await.is_err(), "stale version");
    assert_eq!(repositories.get(&name).await.expect("get").metadata.resource_version, declared.metadata.resource_version);

    // A deleted repository is gone from every read.
    repositories.delete(&name).await.expect("delete");
    assert!(repositories.get(&name).await.is_err());
    assert!(backend.including_replicas::<Repository>(NS).get(&name).await.is_err());
}

#[tokio::test]
async fn the_old_store_keeps_the_repository_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn the_engine_keeps_the_repository_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineRepositories::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    contract(&backend).await;
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Repository>(), NS).await.expect("old rows").items.is_empty());
}

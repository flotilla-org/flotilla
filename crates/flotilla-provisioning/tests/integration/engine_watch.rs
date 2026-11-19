//! An engine watch wakes for commits that write the kinds it follows, and for no other commit.

use std::{sync::Arc, time::Duration};

use flotilla_engine_store::follow::Followed;
use flotilla_provisioning::{
    engine::{kinds::EngineKinds, repositories::EngineRepositories, watch::EngineWatch},
    stored::placement::FulfilmentKindDefinition,
};
use flotilla_resources::{
    Clock, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta, Repository, RepositorySpec, SystemClock,
};
use flotilla_schema::Kind;
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{ensure_repository, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use futures::FutureExt;

const NS: &str = "flotilla";

#[tokio::test]
async fn a_watch_wakes_only_for_writes_of_its_kinds() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_engine_store::registry().expect("engine kinds");
    flotilla_provisioning::stored::placement::register(&mut kinds).expect("register kinds");
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let clock = Arc::new(SystemClock) as Arc<dyn Clock>;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(EngineKinds::open(Arc::clone(&service), Arc::clone(&clock)).expect("kinds")));
    backend.install_engine_records(Arc::new(EngineRepositories::open(Arc::clone(&service), clock).expect("repositories")));
    let mut watch = EngineWatch::start(Arc::clone(&service), [<FulfilmentKindDefinition as Kind>::CODE]).expect("watch");

    // A repository is no kind the watch follows: its commit wakes nothing.
    let spec = RepositorySpec::remote("https://github.com/example/app").expect("spec");
    ensure_repository(&backend.using::<Repository>(NS), &spec.key(), &spec).await.expect("ensure");
    assert!(watch.woken().now_or_never().is_none(), "a commit of another kind woke the watch");

    // A fulfilment kind is: its commit wakes the watch, and the pass names it.
    let host_direct = FulfilmentKindSpec::builder()
        .host_ref("kiwi".into())
        .pool("passthrough".into())
        .realisation(FulfilmentRealisation::HostDirect)
        .build();
    backend
        .using::<FulfilmentKind>(NS)
        .create(&InputMeta::builder().name("host-direct-kiwi".to_owned()).build(), &host_direct)
        .await
        .expect("create");
    tokio::time::timeout(Duration::from_secs(10), watch.woken()).await.expect("the watch woke");
    let followed = watch.pass(|_, followed| Ok(followed)).await.expect("pass");
    let names: Vec<_> = followed
        .into_iter()
        .map(|followed| match followed {
            Followed::Changed(identity) => identity.name,
            Followed::Rescan(home) => panic!("unexpected rescan of {home}"),
        })
        .collect();
    assert_eq!(names, ["host-direct-kiwi"]);
}

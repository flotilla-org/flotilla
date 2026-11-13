//! An engine-served kind's watch cannot resume from a listing: a resume is refused as expired, and
//! a watch opened before a list delivers a change committed between the two.

use std::{sync::Arc, time::Duration};

use flotilla_provisioning::engine::kinds::EngineKinds;
use flotilla_resources::{
    Clock, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta, ResourceError, SystemClock, WatchEvent, WatchStart,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn engine_backend(directory: &std::path::Path) -> ResourceBackend {
    let mut kinds = flotilla_engine_store::registry().expect("engine kinds");
    flotilla_provisioning::stored::placement::register(&mut kinds).expect("register kinds");
    let service = Arc::new(Service::open(&directory.join("decided.sqlite"), "alpha", kinds).expect("store"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(EngineKinds::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("kinds")));
    backend
}

fn host_direct(host: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder().host_ref(host.into()).pool("passthrough".into()).realisation(FulfilmentRealisation::HostDirect).build()
}

#[tokio::test]
async fn a_resume_from_an_engine_kinds_listing_is_refused_as_expired() {
    let directory = tempfile::tempdir().expect("temp dir");
    let kinds = engine_backend(directory.path()).using::<FulfilmentKind>(NS);
    kinds.create(&InputMeta::builder().name("host-direct-kiwi".to_owned()).build(), &host_direct("kiwi")).await.expect("create");

    let listed = kinds.list().await.expect("list");
    match kinds.watch(WatchStart::resuming_from(&listed)).await {
        Err(ResourceError::WatchExpired { .. }) => {}
        Err(error) => panic!("a resume is refused as expired, not {error}"),
        Ok(_) => panic!("a resume from an engine kind's listing must not start silently from now"),
    }
}

#[tokio::test]
async fn a_watch_opened_before_a_list_delivers_a_change_committed_between_them() {
    let directory = tempfile::tempdir().expect("temp dir");
    let kinds = engine_backend(directory.path()).using::<FulfilmentKind>(NS);
    kinds.create(&InputMeta::builder().name("host-direct-kiwi".to_owned()).build(), &host_direct("kiwi")).await.expect("create");

    let mut watch = kinds.watch(WatchStart::Now).await.expect("watch");
    kinds.create(&InputMeta::builder().name("host-direct-feta".to_owned()).build(), &host_direct("feta")).await.expect("create between");
    let listed = kinds.list().await.expect("list");
    let names: Vec<_> = listed.items.iter().map(|kind| kind.metadata.name.as_str()).collect();
    assert!(names.contains(&"host-direct-feta"), "the list holds the change: {names:?}");

    // The watch holds it too, so a caller that missed it in a list taken before the commit still
    // sees it.
    let delivered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match watch.next().await.expect("watch open").expect("watch event") {
                WatchEvent::Added(kind) | WatchEvent::Modified(kind) if kind.metadata.name == "host-direct-feta" => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(delivered.is_ok(), "the change committed after the watch opened arrives as an event");
}

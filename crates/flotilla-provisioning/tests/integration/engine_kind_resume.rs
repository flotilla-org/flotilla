//! An engine-served kind's watch resumes from a listing's position: it delivers each change made
//! since, and is refused as expired when the log no longer holds what followed the position. A
//! watch opened before a list delivers a change committed between the two.

use std::{sync::Arc, time::Duration};

use flotilla_provisioning::engine::kinds::EngineKinds;
use flotilla_provisioning_testkit::other_home::OtherHome;
use flotilla_resources::{
    Clock, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta, ResourceError, SystemClock, WatchEvent, WatchStart,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn engine_backend(directory: &std::path::Path) -> (ResourceBackend, Arc<Service>) {
    let mut kinds = flotilla_engine_store::registry().expect("engine kinds");
    flotilla_provisioning::stored::placement::register(&mut kinds).expect("register kinds");
    let service = Arc::new(Service::open(&directory.join("decided.sqlite"), "alpha", kinds).expect("store"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .install_engine_records(Arc::new(EngineKinds::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("kinds")));
    (backend, service)
}

fn host_direct(host: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder().host_ref(host.into()).pool("passthrough".into()).realisation(FulfilmentRealisation::HostDirect).build()
}

#[tokio::test]
async fn a_resume_from_an_engine_kinds_listing_delivers_a_change_made_since() {
    let directory = tempfile::tempdir().expect("temp dir");
    let kinds = engine_backend(directory.path()).0.using::<FulfilmentKind>(NS);
    kinds.create(&InputMeta::builder().name("host-direct-kiwi".to_owned()).build(), &host_direct("kiwi")).await.expect("create");

    let listed = kinds.list().await.expect("list");
    kinds.create(&InputMeta::builder().name("host-direct-feta".to_owned()).build(), &host_direct("feta")).await.expect("create since");
    let mut watch = kinds.watch(WatchStart::resuming_from(&listed)).await.expect("a resume from the listing starts");

    let first = tokio::time::timeout(Duration::from_secs(10), watch.next()).await.expect("an event").expect("watch open").expect("event");
    match first {
        WatchEvent::Added(kind) | WatchEvent::Modified(kind) => {
            assert_eq!(kind.metadata.name, "host-direct-feta", "the resume names first the change made since the listing")
        }
        other => panic!("the change made since the listing arrives as the kind, not {other:?}"),
    }
}

#[tokio::test]
async fn a_resume_across_a_snapshot_catch_up_is_refused_as_expired() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (backend, service) = engine_backend(directory.path());
    let kinds = backend.using::<FulfilmentKind>(NS);
    let beta = OtherHome::open("beta");
    beta.kind("host-direct-beta", &host_direct("beta")).await;
    beta.replicate_to(&service);

    let listed = kinds.list().await.expect("list");
    // Beta writes again, and reaches alpha as a snapshot, which logs nothing to read since.
    beta.kind("host-direct-beta-two", &host_direct("beta")).await;
    beta.replicate_to(&service);

    match kinds.watch(WatchStart::resuming_from(&listed)).await {
        Err(ResourceError::WatchExpired { .. }) => {}
        Err(error) => panic!("a resume across a snapshot is refused as expired, not {error}"),
        Ok(_) => panic!("a resume across a snapshot must not start past beta's change"),
    }
}

#[tokio::test]
async fn a_watch_opened_before_a_list_delivers_a_change_committed_between_them() {
    let directory = tempfile::tempdir().expect("temp dir");
    let kinds = engine_backend(directory.path()).0.using::<FulfilmentKind>(NS);
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

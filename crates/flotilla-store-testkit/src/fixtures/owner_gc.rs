use std::time::Duration;

use flotilla_resources::{Host, HostSpec, InputMeta, LifecycleAuthority, OwnerReference, ResourceError, WatchEvent, WatchStart};
use flotilla_store::{OwnerGarbageCollector, ResourceBackend};
use futures::StreamExt;

pub const NS: &str = "gc-test";

pub fn meta(name: &str, owner: Option<&str>) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .owner_references(
            owner
                .into_iter()
                .map(|name| OwnerReference {
                    api_version: "flotilla.work/v1".to_string(),
                    kind: "Host".to_string(),
                    name: name.to_string(),
                    controller: true,
                })
                .collect(),
        )
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Managed)
}

pub async fn create(backend: &ResourceBackend, meta: InputMeta) {
    backend.using::<Host>(NS).create(&meta, &HostSpec::default()).await.expect("create host");
}

pub async fn deleted(watch: &mut flotilla_resources::WatchStream<Host>, name: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == name) {
                break;
            }
        }
    }).await.expect("reactive deletion without an hourly sweep");
}

pub async fn start(backend: &ResourceBackend) -> tokio::task::JoinHandle<Result<(), ResourceError>> {
    let mut watch = backend.using::<Host>(NS).watch(WatchStart::Now).await.expect("watch startup");
    create(backend, meta("startup-orphan", Some("missing"))).await;
    let collector = OwnerGarbageCollector::new(backend.clone(), NS);
    let mut task = tokio::spawn(async move { collector.run(Duration::from_secs(3600)).await });
    // Startup recovery deletes this marker only after all watches are established.
    tokio::select! {
        _ = deleted(&mut watch, "startup-orphan") => {},
        result = &mut task => panic!("collector stopped: {result:?}"),
    }
    task
}

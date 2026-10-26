use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::lifecycle_reads::NoLifecycleReads;
use flotilla_resources::{
    image_identity, ImageBuild, ImageBuildPhase, ImageBuildReservation, ImageBuildSpec, ImageBuildStatus, ImageDelivery, ImageLayerSource,
    ImageLayerSpec, ImageLayerTier, InputMeta,
};
use flotilla_store::{read_image_build, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;

fn spec(host_ref: &str) -> ImageBuildSpec {
    let layers = vec![ImageLayerSpec::builder()
        .name("base".into())
        .version("1".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from(format!("ubuntu@sha256:{}", "1".repeat(64)))
        .fragment("FROM ${BASE}\n".into())
        .probe("true".into())
        .build()];
    ImageBuildSpec::builder()
        .identity(image_identity(&layers))
        .architecture("amd64".into())
        .layers(layers)
        .host_ref(host_ref.into())
        .delivery(ImageDelivery::Registry { repository: "registry.test/crew".into() })
        .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
        .build()
}

async fn authored(root: &str, host_ref: &str, phase: Option<ImageBuildPhase>) -> ResourceBackend {
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new(root));
    let spec = spec(host_ref);
    let name = spec.resource_name();
    let builds = backend.using::<ImageBuild>("flotilla");
    let created = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("build");
    if let Some(phase) = phase {
        let status = ImageBuildStatus { phase, ..Default::default() };
        builds.update_status(&name, &created.metadata.resource_version, &status).await.expect("status");
    }
    backend
}

// Under replication lag, roots may hold one build name with different specs
// (another builder or delivery view). Reads never fail on the disagreement:
// they prefer Built, then Building, then Queued, then Failed, breaking ties
// by builder host and then origin.
#[tokio::test]
async fn disagreeing_replicas_resolve_deterministically() {
    let local = authored("root-a", "udder", None).await;
    let name = spec("udder").resource_name();
    let replicate = |source: ResourceBackend, root: &'static str| {
        let local = local.clone();
        async move {
            local
                .replica_writer::<ImageBuild>(NodeId::new(root), "flotilla")
                .replace(&source.using::<ImageBuild>("flotilla").list().await.expect("list"), Utc::now())
                .await
                .expect("replicate");
        }
    };
    let read = || async {
        read_image_build(&local, "flotilla", &name, Arc::new(NoLifecycleReads)).await.expect("read despite disagreement").build.object
    };
    replicate(authored("root-b", "feta", None).await, "root-b").await;
    assert_eq!(read().await.spec.current().expect("current").host_ref, "feta", "equal phases tie-break by builder host");
    replicate(authored("root-c", "kiwi", Some(ImageBuildPhase::Failed)).await, "root-c").await;
    assert_eq!(read().await.spec.current().expect("current").host_ref, "feta", "Queued beats Failed");
    replicate(authored("root-d", "zeta", Some(ImageBuildPhase::Building)).await, "root-d").await;
    assert_eq!(read().await.spec.current().expect("current").host_ref, "zeta", "Building beats Queued");
}

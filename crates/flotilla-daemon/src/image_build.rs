//! Builder-side ImageBuild demand projection (ADR 0062). Admission may author
//! the per-identity ImageBuild on any root; the declared builder projects that
//! demand into its own store so its build service writes execution evidence
//! at its own authority, and replication returns the evidence to consumers.
//! The build service that executes projected builds is wired by the image
//! build-service reconciler (PR 2); this module only mirrors demand.
use flotilla_resources::InputMeta;
use flotilla_store::ResourceBackend;

/// Mirror build demands onto their builder host, following Vessel placement's
/// projection convention. Retired ADR 0053 records are never projected.
pub(crate) async fn project_builds(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<(), flotilla_resources::ResourceError> {
    use flotilla_resources::{ImageBuild, ResourceProvenance, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION};
    let builds = backend.using::<ImageBuild>(namespace);
    for source in backend.including_replicas::<ImageBuild>(namespace).list().await?.items {
        let ResourceProvenance::Replica { origin_root, .. } = source.provenance else {
            continue;
        };
        let obj = source.object;
        let Some(spec) = obj.spec.current() else { continue };
        if spec.host_ref != host_ref
            || obj.metadata.deletion_timestamp.is_some()
            || obj.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION)
        {
            continue;
        }
        match builds.get(&obj.metadata.name).await {
            Ok(current) => {
                if current.spec != obj.spec {
                    tracing::error!(role = "infra", image_build = %obj.metadata.name, "image build name collision; skipping demand");
                }
            }
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let mut annotations = obj.metadata.annotations;
                annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.into(), host_ref.into());
                annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.into(), origin_root.to_string());
                builds.create(&InputMeta::builder().name(obj.metadata.name).annotations(annotations).build(), &obj.spec).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_protocol::NodeId;
    use flotilla_resources::{
        image_build_name, image_identity, ImageBuild, ImageBuildPhase, ImageBuildReservation, ImageBuildSpec, ImageBuildStatusPatch,
        ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier,
    };
    use flotilla_store::{apply_status_patch, read_image_build};

    use super::*;

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
            .delivery(ImageDelivery::Local)
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .build()
    }

    // A declared builder projects a remote demand only once, refuses a name
    // that already holds a different spec, writes evidence at its own
    // authority, and replication returns that evidence to admission.
    #[tokio::test]
    async fn declared_builder_projects_demand_and_returns_execution_evidence() {
        let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("builder-root"));
        let spec = spec("builder");
        let name = image_build_name(&spec.identity, &spec.architecture);
        home.using::<ImageBuild>("test")
            .create(&InputMeta::builder().name(name.clone()).build(), &spec.clone().into())
            .await
            .expect("demand");
        builder
            .replica_writer::<ImageBuild>(NodeId::new("home"), "test")
            .replace(&home.using::<ImageBuild>("test").list().await.expect("demand snapshot"), Utc::now())
            .await
            .expect("replicate demand");
        for _ in 0..2 {
            project_builds(&builder, "test", "builder").await.expect("project");
        }
        let builds = builder.using::<ImageBuild>("test");
        assert_eq!(builds.list().await.expect("list").items.len(), 1);
        project_builds(&builder, "test", "elsewhere").await.expect("other hosts project nothing");
        assert_eq!(builds.list().await.expect("list").items.len(), 1);
        apply_status_patch(&builds, &name, &ImageBuildStatusPatch::Start { at: Utc::now(), attempt: 1 }).await.expect("start");
        apply_status_patch(
            &builds,
            &name,
            &ImageBuildStatusPatch::Built {
                at: Utc::now(),
                digest: format!("sha256:{}", "3".repeat(64)),
                probes: Vec::new(),
                log_ref: "log".into(),
            },
        )
        .await
        .expect("built");
        home.replica_writer::<ImageBuild>(NodeId::new("builder-root"), "test")
            .replace(&builds.list().await.expect("execution snapshot"), Utc::now())
            .await
            .expect("replicate evidence");
        assert_eq!(
            read_image_build(&home, "test", &name).await.expect("read execution").status.expect("evidence").phase,
            ImageBuildPhase::Built
        );
        assert!(home.using::<ImageBuild>("test").get(&name).await.expect("original demand").status.is_none());
    }

    // A local build of the same name with a different builder is never
    // overwritten by projection.
    #[tokio::test]
    async fn name_collisions_are_not_projected() {
        let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("builder-root"));
        let demand = spec("builder");
        let name = image_build_name(&demand.identity, &demand.architecture);
        home.using::<ImageBuild>("test").create(&InputMeta::builder().name(name.clone()).build(), &demand.into()).await.expect("demand");
        builder
            .using::<ImageBuild>("test")
            .create(&InputMeta::builder().name(name.clone()).build(), &spec("other").into())
            .await
            .expect("existing");
        builder
            .replica_writer::<ImageBuild>(NodeId::new("home"), "test")
            .replace(&home.using::<ImageBuild>("test").list().await.expect("demand snapshot"), Utc::now())
            .await
            .expect("replicate demand");
        project_builds(&builder, "test", "builder").await.expect("project");
        let local = builder.using::<ImageBuild>("test").get(&name).await.expect("local");
        assert_eq!(local.spec.current().expect("current").host_ref, "other");
    }
}

use flotilla_protocol::LifecycleReading;
use flotilla_resources::lifecycle_reads::{LifecycleReads, ProvisioningKind};
use flotilla_resources::*;

/// A selected build and the lifecycle reading used to select it, at its origin.
pub struct ImageBuildReading {
    pub build: ReadResourceObject<ImageBuild>,
    pub lifecycle: Option<LifecycleReading>,
}

/// Select a build using the engine's judgement at each origin, falling back only for unheld builds.
pub async fn read_image_build(
    backend: &crate::ResourceBackend,
    namespace: &str,
    name: &str,
    reads: std::sync::Arc<dyn LifecycleReads>,
) -> Result<ImageBuildReading, ResourceError> {
    let sources = backend
        .including_replicas::<ImageBuild>(namespace)
        .get_all(name)
        .await?
        .items
        .into_iter()
        .filter(|source| source.object.metadata.name == name && !source.object.spec.is_retired())
        .collect::<Vec<_>>();
    if sources.windows(2).any(|pair| pair[0].object.spec != pair[1].object.spec) {
        tracing::warn!(image_build = %name, copies = sources.len(), "image build copies disagree; using the most advanced");
    }
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut sources = sources
            .into_iter()
            .map(|build| {
                let origin = match &build.provenance {
                    ResourceProvenance::Local => None,
                    ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.to_string()),
                };
                let lifecycle = reads.try_reading(ProvisioningKind::ImageBuild, &name, origin.as_deref()).map_err(ResourceError::other)?;
                Ok(ImageBuildReading { build, lifecycle })
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        sources.sort_by_key(|reading| {
            let source = &reading.build;
            let phase = match &reading.lifecycle {
                Some(LifecycleReading::Ready { .. }) => 3,
                Some(LifecycleReading::InProgress) => 2,
                Some(LifecycleReading::Retrying { .. }) => 1,
                // Ended states supply neither a usable image nor a pending attempt. They tie with legacy
                // Failed; host and origin make that unusable choice deterministic.
                Some(_) => 0,
                None => match source.object.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase) {
                    ImageBuildPhase::Built => 3,
                    ImageBuildPhase::Building => 2,
                    ImageBuildPhase::Queued => 1,
                    ImageBuildPhase::Failed => 0,
                },
            };
            let host = source.object.spec.current().map(|spec| spec.host_ref.clone()).unwrap_or_default();
            let origin = match &source.provenance {
                ResourceProvenance::Local => String::new(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
            };
            (std::cmp::Reverse(phase), host, origin)
        });
        sources.into_iter().next().ok_or_else(|| ResourceError::not_found(&name))
    })
    .await
    .map_err(|error| ResourceError::other(format!("image build lifecycle read: {error}")))?
}

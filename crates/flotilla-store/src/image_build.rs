use flotilla_resources::*;

/// Prefer execution evidence written by the declared build host over the
/// admitting root's queued demand. Never borrow another recipe's old digest.
pub async fn read_image_build(
    backend: &crate::ResourceBackend,
    namespace: &str,
    name: &str,
) -> Result<crate::ResourceObject<ImageBuild>, ResourceError> {
    let mut sources = backend
        .including_replicas::<ImageBuild>(namespace)
        .list()
        .await?
        .items
        .into_iter()
        .filter(|source| source.object.metadata.name == name)
        .collect::<Vec<_>>();
    if let Some(first) = sources.first() {
        if sources.iter().any(|source| {
            source.object.spec.inputs != first.object.spec.inputs || source.object.spec.recipe_key != first.object.spec.recipe_key
        }) {
            return Err(ResourceError::invalid(format!("image build {name} has conflicting immutable sources")));
        }
    }
    sources.sort_by_key(|source| {
        let actor = source
            .object
            .metadata
            .annotations
            .get(crate::ACTUATOR_HOST_REF_ANNOTATION)
            .is_some_and(|host| host == &source.object.spec.host_ref);
        let phase = match source.object.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase) {
            ImageBuildPhase::Built => 3,
            ImageBuildPhase::Failed => 2,
            ImageBuildPhase::Building => 1,
            ImageBuildPhase::Queued => 0,
        };
        (std::cmp::Reverse(actor), std::cmp::Reverse(phase), source.object.metadata.creation_timestamp)
    });
    sources.into_iter().next().map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
}

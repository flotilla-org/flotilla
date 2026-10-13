use flotilla_resources::*;

/// Read one per-identity ImageBuild across the local store and replicas.
/// Retired ADR 0053 records are ignored. Copies of one name may disagree
/// under replication lag (another builder or delivery view): the read never
/// fails on that, preferring Built, then Building, then Queued, then Failed,
/// with the builder host and then the origin as deterministic tiebreaks.
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
        .filter(|source| source.object.metadata.name == name && !source.object.spec.is_retired())
        .collect::<Vec<_>>();
    if sources.windows(2).any(|pair| pair[0].object.spec != pair[1].object.spec) {
        tracing::warn!(image_build = %name, copies = sources.len(), "image build copies disagree; using the most advanced");
    }
    sources.sort_by_key(|source| {
        let phase = match source.object.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase) {
            ImageBuildPhase::Built => 3,
            ImageBuildPhase::Building => 2,
            ImageBuildPhase::Queued => 1,
            ImageBuildPhase::Failed => 0,
        };
        let host = source.object.spec.current().map(|spec| spec.host_ref.clone()).unwrap_or_default();
        let origin = match &source.provenance {
            ResourceProvenance::Local => String::new(),
            ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
        };
        (std::cmp::Reverse(phase), host, origin)
    });
    sources.into_iter().next().map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
}

//! Checked vessel views for readers that decide from a vessel's lifecycle phase.
use std::sync::Arc;

use flotilla_resources::{lifecycle_reads::LifecycleReads, ReadResourceObject, ResourceError, ResourceProvenance, Vessel};

pub async fn read_vessel_sources(
    reads: Arc<dyn LifecycleReads>,
    sources: Vec<ReadResourceObject<Vessel>>,
) -> Result<Vec<ReadResourceObject<Vessel>>, ResourceError> {
    if sources.is_empty() || reads.is_disabled() {
        return Ok(sources);
    }
    tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .map(|mut source| {
                let origin = match &source.provenance {
                    ResourceProvenance::Local => None,
                    ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
                };
                source.object = flotilla_resources::vessel::read(reads.as_ref(), &source.object, origin).map_err(ResourceError::other)?;
                Ok(source)
            })
            .collect()
    })
    .await
    .map_err(|error| ResourceError::other(error.to_string()))?
}

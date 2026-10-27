//! Checked terminal views used by controllers and crew readers.
use std::sync::Arc;

use flotilla_resources::{
    lifecycle_reads::LifecycleReads, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance, TerminalSession,
};

pub async fn read_terminal_session(
    reads: Arc<dyn LifecycleReads>,
    obj: &ResourceObject<TerminalSession>,
    origin: Option<&str>,
) -> Result<(ResourceObject<TerminalSession>, Option<String>), ResourceError> {
    let (obj, origin) = (obj.clone(), origin.map(str::to_owned));
    tokio::task::spawn_blocking(move || flotilla_resources::terminal_launches::read(reads.as_ref(), &obj, origin.as_deref()))
        .await
        .map_err(|error| ResourceError::other(error.to_string()))?
        .map_err(ResourceError::other)
}

pub async fn read_terminal_sources(
    reads: Arc<dyn LifecycleReads>,
    sources: Vec<ReadResourceObject<TerminalSession>>,
) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
    tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .map(|mut source| {
                let origin = match &source.provenance {
                    ResourceProvenance::Local => None,
                    ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
                };
                source.object =
                    flotilla_resources::terminal_launches::read(reads.as_ref(), &source.object, origin).map_err(ResourceError::other)?.0;
                Ok(source)
            })
            .collect()
    })
    .await
    .map_err(|error| ResourceError::other(error.to_string()))?
}

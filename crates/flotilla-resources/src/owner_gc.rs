//! Home-root owner-reference collection. Replicated owner deletions are inputs;
//! only locally authored children are mutated. Normal delete semantics let each
//! child's controller drain its finalizers before its own deletion propagates.
use std::time::Duration;

use futures::{stream::SelectAll, StreamExt};

use crate::{
    registry::collect_owned_resources, watch_resource_kind, watch_resource_kind_including_replicas, OwnerReference, ReplicationClass,
    ResourceBackend, ResourceError, REGISTERED_RESOURCE_KINDS,
};

pub struct OwnerGarbageCollector {
    backend: ResourceBackend,
    namespace: String,
}

impl OwnerGarbageCollector {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>) -> Self {
        Self { backend, namespace: namespace.into() }
    }

    /// Relist recovery for restart, expired watches, and failed deletion attempts.
    pub async fn sweep(&self) -> Result<(), ResourceError> {
        collect_owned_resources(&self.backend, &self.namespace, None).await
    }

    pub async fn run(&self, backstop_interval: Duration) -> Result<(), ResourceError> {
        let mut watches = SelectAll::new();
        // Establish every watch before recovery, so deletes during the sweep
        // remain queued. The daemon supervisor restarts and relists on errors.
        for kind in REGISTERED_RESOURCE_KINDS {
            let watch = if kind.replication_class == ReplicationClass::None {
                watch_resource_kind(&self.backend, &self.namespace, kind.kind).await?
            } else {
                watch_resource_kind_including_replicas(&self.backend, &self.namespace, kind.kind).await?
            };
            // Turn a closed stream into an error rather than silently losing a kind.
            watches.push(
                watch
                    .stream
                    .chain(futures::stream::once(async { Err(ResourceError::other("owner garbage collector watch closed")) }))
                    .boxed(),
            );
        }
        let mut backstop = tokio::time::interval(backstop_interval);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = backstop.tick() => self.sweep().await?,
                event = watches.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("owner garbage collector watches closed"))??;
                    if event["type"] != "DELETED" {
                        continue;
                    }
                    let object = &event["object"];
                    let field = |value: &serde_json::Value| {
                        value.as_str().map(str::to_string).ok_or_else(|| ResourceError::decode("invalid owner deletion event"))
                    };
                    let owner = OwnerReference {
                        api_version: field(&object["apiVersion"])?,
                        kind: field(&object["kind"])?,
                        name: field(&object["metadata"]["name"])?,
                        controller: true,
                    };
                    collect_owned_resources(&self.backend, &self.namespace, Some(&owner)).await?;
                }
            }
        }
    }
}

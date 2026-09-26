//! Home-root owner-reference collection. Replicated owner deletions are inputs;
//! only locally authored children are mutated. Normal delete semantics let each
//! child's controller drain its finalizers before its own deletion propagates.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures::{stream::SelectAll, StreamExt};

use crate::{
    registry::{collect_indexed_child, collect_owned_resources},
    replica::ORIGIN_ROOT_ANNOTATION,
    watch_resource_kind, watch_resource_kind_including_replicas, OwnerReference, ReplicationClass, ResourceBackend, ResourceError,
    REGISTERED_RESOURCE_KINDS,
};

type ChildKey = (String, String);

#[derive(Default)]
struct ControlledChildren {
    by_owner: HashMap<OwnerReference, HashSet<ChildKey>>,
    by_child: HashMap<ChildKey, Vec<OwnerReference>>,
}

impl ControlledChildren {
    fn remove(&mut self, child: &ChildKey) {
        if let Some(owners) = self.by_child.remove(child) {
            for owner in owners {
                if let Some(children) = self.by_owner.get_mut(&owner) {
                    children.remove(child);
                    if children.is_empty() {
                        self.by_owner.remove(&owner);
                    }
                }
            }
        }
    }

    fn record(&mut self, object: &serde_json::Value) -> Result<Vec<OwnerReference>, ResourceError> {
        let field = |value: &serde_json::Value| {
            value.as_str().map(str::to_string).ok_or_else(|| ResourceError::decode("invalid owner garbage collector watch object"))
        };
        let child = (field(&object["kind"])?, field(&object["metadata"]["name"])?);
        // Replica records are never locally mutable. Their owner-deletion
        // events still drive collection of local children.
        if object["metadata"]["annotations"].get(ORIGIN_ROOT_ANNOTATION).is_some() {
            return Ok(Vec::new());
        }
        self.remove(&child);
        let owners: Vec<OwnerReference> = serde_json::from_value(object["metadata"]["ownerReferences"].clone())
            .or_else(|error| object["metadata"]["ownerReferences"].is_null().then(Vec::new).ok_or(error))
            .map_err(|error| ResourceError::decode(format!("invalid owner references in watch object: {error}")))?;
        let owners: Vec<_> = owners.into_iter().filter(|owner| owner.controller).collect();
        for owner in &owners {
            self.by_owner.entry(owner.clone()).or_default().insert(child.clone());
        }
        if !owners.is_empty() {
            self.by_child.insert(child, owners.clone());
        }
        Ok(owners)
    }
}

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
        let mut children = ControlledChildren::default();
        // Establish every watch before recovery, so deletes during the sweep
        // remain queued. The daemon supervisor restarts and relists on errors.
        for kind in REGISTERED_RESOURCE_KINDS {
            let watch = if kind.replication_class == ReplicationClass::None {
                watch_resource_kind(&self.backend, &self.namespace, kind.kind).await?
            } else {
                watch_resource_kind_including_replicas(&self.backend, &self.namespace, kind.kind).await?
            };
            for event in &watch.initial {
                children.record(&event["object"])?;
            }
            // Turn a closed stream into an error rather than silently losing a kind.
            watches.push(
                watch
                    .stream
                    .chain(futures::stream::once(async { Err(ResourceError::other("owner garbage collector watch closed")) }))
                    .boxed(),
            );
        }
        // The interval's immediate first tick performs startup recovery after
        // all watches are established. Delaying that tick would skip recovery.
        let mut backstop = tokio::time::interval(backstop_interval);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = backstop.tick() => self.sweep().await?,
                event = watches.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("owner garbage collector watches closed"))??;
                    let object = &event["object"];
                    let field = |value: &serde_json::Value| {
                        value.as_str().map(str::to_string).ok_or_else(|| ResourceError::decode("invalid owner deletion event"))
                    };
                    match event["type"].as_str() {
                        Some("ADDED" | "MODIFIED") => {
                            let kind = field(&object["kind"])?;
                            let name = field(&object["metadata"]["name"])?;
                            for owner in children.record(object)? {
                                collect_indexed_child(&self.backend, &self.namespace, &kind, &name, &owner).await?;
                            }
                        }
                        Some("DELETED") => {
                            let kind = field(&object["kind"])?;
                            let name = field(&object["metadata"]["name"])?;
                            if object["metadata"]["annotations"].get(ORIGIN_ROOT_ANNOTATION).is_none() {
                                children.remove(&(kind.clone(), name.clone()));
                            }
                            let owner = OwnerReference { api_version: field(&object["apiVersion"])?, kind, name, controller: true };
                            if let Some(affected) = children.by_owner.get(&owner) {
                                for (kind, name) in affected.clone() {
                                    collect_indexed_child(&self.backend, &self.namespace, &kind, &name, &owner).await?;
                                }
                            }
                        }
                        _ => return Err(ResourceError::decode("invalid owner garbage collector watch event")),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use futures::StreamExt;

    use super::*;
    use crate::{registry::GC_FULL_KIND_LISTS, Host, HostSpec, InMemoryBackend, InputMeta, WatchEvent, WatchStart};

    #[tokio::test]
    async fn deleting_a_leaf_does_not_list_resource_kinds() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>("gc-leaf-list-test");
        let meta = |name: &str| InputMeta::builder().name(name.to_string()).build();
        hosts.create(&meta("leaf"), &HostSpec::default()).await.expect("create leaf");
        // An orphan marker confirms startup recovery has completed before we
        // capture the list count.
        let mut orphan = meta("startup-orphan");
        orphan.owner_references.push(OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Host".to_string(),
            name: "missing".to_string(),
            controller: true,
        });
        hosts.create(&orphan, &HostSpec::default()).await.expect("create startup orphan");
        let mut watch = hosts.watch(WatchStart::Now).await.expect("watch marker");
        let collector = OwnerGarbageCollector::new(backend, "gc-leaf-list-test");
        let task = tokio::spawn(async move { collector.run(Duration::from_secs(3600)).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == "startup-orphan") {
                    break;
                }
            }
        }).await.expect("startup sweep");
        let before = GC_FULL_KIND_LISTS.load(Ordering::Relaxed);
        hosts.delete("leaf").await.expect("delete leaf");
        orphan.name = "after-leaf-orphan".to_string();
        hosts.create(&orphan, &HostSpec::default()).await.expect("create marker again");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == "after-leaf-orphan") {
                    break;
                }
            }
        }).await.expect("collector consumed leaf deletion");
        assert_eq!(GC_FULL_KIND_LISTS.load(Ordering::Relaxed), before);
        task.abort();
        let _ = task.await;
    }
}

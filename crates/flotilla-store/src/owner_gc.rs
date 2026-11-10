// Home-root owner-reference collection. Replicated owner deletions are inputs;
// only locally authored children are mutated. Normal delete semantics let each
// child's controller drain its finalizers before its own deletion propagates.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures::{stream::SelectAll, StreamExt};

use crate::{
    old_store_resource_kinds,
    registry::{collect_indexed_child, collect_owned_resources},
    replica::ORIGIN_ROOT_ANNOTATION,
    watch_resource_kind, watch_resource_kind_including_replicas, OwnerReference, ReplicationClass, ResourceBackend, ResourceError,
};

type ChildKey = (String, String);

/// The first retry delay of a child whose collection failed, doubling to [`RETRY_CAP`].
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(60);
/// Failed collections of one child before it is left until its owner or it changes again.
const RETRY_LIMIT: u32 = 10;

/// Children whose collection failed, each with its failures so far and when it is next tried.
#[derive(Default)]
struct Retries {
    due: HashMap<(ChildKey, OwnerReference), (u32, tokio::time::Instant)>,
}

impl Retries {
    /// Counts a failed collection of `child` for `owner`, and schedules it again unless it has
    /// failed [`RETRY_LIMIT`] times.
    fn failed(&mut self, child: ChildKey, owner: OwnerReference, error: &ResourceError) {
        let key = (child, owner);
        let failures = self.due.get(&key).map_or(0, |(failures, _)| *failures) + 1;
        if failures >= RETRY_LIMIT {
            self.due.remove(&key);
            tracing::error!(kind = %key.0 .0, name = %key.0 .1, owner = %key.1.name, %error, failures, "owner garbage collection of a child failed; left until it changes");
            return;
        }
        tracing::warn!(kind = %key.0 .0, name = %key.0 .1, owner = %key.1.name, %error, failures, "owner garbage collection of a child failed; retrying");
        let delay = RETRY_BASE.saturating_mul(1 << failures.min(6)).min(RETRY_CAP);
        self.due.insert(key, (failures, tokio::time::Instant::now() + delay));
    }

    /// A child seen changing starts its count again.
    fn changed(&mut self, child: &ChildKey) {
        self.due.retain(|(held, _), _| held != child);
    }

    fn next(&self) -> Option<tokio::time::Instant> {
        self.due.values().map(|(_, at)| *at).min()
    }

    /// The children due to be tried again; each stays held until it succeeds or runs out.
    fn ready(&self) -> Vec<(ChildKey, OwnerReference)> {
        let now = tokio::time::Instant::now();
        self.due.iter().filter(|(_, (_, at))| *at <= now).map(|(key, _)| key.clone()).collect()
    }

    fn succeeded(&mut self, child: ChildKey, owner: OwnerReference) {
        self.due.remove(&(child, owner));
    }
}

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
        let owners: Vec<OwnerReference> = match &object["metadata"]["ownerReferences"] {
            serde_json::Value::Null => Vec::new(),
            value => serde_json::from_value(value.clone())
                .map_err(|error| ResourceError::decode(format!("invalid owner references in watch object: {error}")))?,
        };
        // The index only needs references that the collector can act on.
        let owners: Vec<_> = owners
            .into_iter()
            .filter(|owner| {
                owner.controller
                    && owner.api_version == "flotilla.work/v1"
                    && old_store_resource_kinds().any(|kind| kind.kind == owner.kind)
            })
            .collect();
        let previous = self.by_child.get(&child).cloned().unwrap_or_default();
        if previous == owners {
            return Ok(Vec::new());
        }
        self.remove(&child);
        for owner in &owners {
            self.by_owner.entry(owner.clone()).or_default().insert(child.clone());
        }
        if !owners.is_empty() {
            self.by_child.insert(child, owners.clone());
        }
        Ok(owners.into_iter().filter(|owner| !previous.contains(owner)).collect())
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

    /// Relist recovery, run at start once every watch is established.
    pub async fn sweep(&self) -> Result<(), ResourceError> {
        collect_owned_resources(&self.backend, &self.namespace, None).await
    }

    /// Collects children as their owners are deleted. A failed collection is retried with
    /// backoff up to [`RETRY_LIMIT`] times; a closed or failed watch ends the run, for the
    /// supervisor to restart it with a fresh startup sweep.
    pub async fn run(&self) -> Result<(), ResourceError> {
        let mut watches = SelectAll::new();
        let mut children = ControlledChildren::default();
        // Establish every watch before recovery, so deletes during the sweep
        // remain queued. The daemon supervisor restarts and relists on errors.
        // Engine-held kinds have no records here to own or be owned.
        for kind in old_store_resource_kinds() {
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
        // Startup recovery, after every watch is established.
        self.sweep().await?;
        let mut retries = Retries::default();
        loop {
            let next = retries.next();
            tokio::select! {
                () = async { match next { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                    for ((kind, name), owner) in retries.ready() {
                        match collect_indexed_child(&self.backend, &self.namespace, &kind, &name, &owner).await {
                            Ok(()) => retries.succeeded((kind, name), owner),
                            Err(error) => retries.failed((kind, name), owner, &error),
                        }
                    }
                }
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
                            retries.changed(&(kind.clone(), name.clone()));
                            for owner in children.record(object)? {
                                if let Err(error) = collect_indexed_child(&self.backend, &self.namespace, &kind, &name, &owner).await {
                                    retries.failed((kind.clone(), name.clone()), owner, &error);
                                }
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
                                    retries.changed(&(kind.clone(), name.clone()));
                                    if let Err(error) = collect_indexed_child(&self.backend, &self.namespace, &kind, &name, &owner).await {
                                        retries.failed((kind, name), owner.clone(), &error);
                                    }
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
    use serde_json::json;

    use super::*;
    use crate::{registry::GC_FULL_KIND_LISTS, Host, HostSpec, InputMeta, WatchEvent, WatchStart};

    #[test]
    fn unchanged_owners_do_not_trigger_repeat_lookups() {
        let mut index = ControlledChildren::default();
        let object = |owner: &str, api_version: &str| {
            json!({
                "kind": "Host",
                "metadata": {
                    "name": "child",
                    "ownerReferences": [{"apiVersion": api_version, "kind": "Host", "name": owner, "controller": true}]
                }
            })
        };
        assert_eq!(index.record(&object("first", "flotilla.work/v1")).expect("index child").len(), 1);
        assert!(index.record(&object("first", "flotilla.work/v1")).expect("unchanged child").is_empty());
        assert_eq!(index.record(&object("second", "flotilla.work/v1")).expect("reparent child").len(), 1);
        assert!(index.record(&object("foreign", "foreign/v1")).expect("ignore foreign owner").is_empty());
        assert!(index.by_owner.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_leaf_does_not_list_resource_kinds() {
        let backend = ResourceBackend::new(crate::test_store::memory_backend());
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
        let task = tokio::spawn(async move { collector.run().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == "startup-orphan") {
                    break;
                }
            }
        }).await.expect("startup sweep");
        // The startup sweep deletes that orphan part-way through its lists. A marker made now is
        // collected from its watch event, which the collector reads only once the sweep is done.
        orphan.name = "after-sweep-orphan".to_string();
        hosts.create(&orphan, &HostSpec::default()).await.expect("create sweep marker");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == "after-sweep-orphan") {
                    break;
                }
            }
        }).await.expect("startup sweep finished");
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

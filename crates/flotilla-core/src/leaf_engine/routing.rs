//! Object routing is shared by a namespace; rows retain only their dependencies.
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Weak,
};

use flotilla_protocol::LeafAddress;
use flotilla_resources::{Artifact, ChangeRequest, Convoy, Issue, ReadWatchEvent, Resource, ResourceBackend, ResourceError, Usage, Vessel};
use futures::{
    stream::{BoxStream, SelectAll},
    StreamExt,
};
use tokio::{sync::watch, task::JoinHandle};

use super::subscriptions::LeafWatchRecovery;
use super::{LeafSubscriptionRow, LeafSubscriptionTable, LeafSubscriptionTableInner, LeafWatcher};
use crate::{change_request_observer::ChangeRequestRef, issue_observer::IssueRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ObjectKind {
    Convoy,
    Vessel,
    ChangeRequest,
    Issue,
    Usage,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ObjectAddress {
    pub(super) kind: ObjectKind,
    pub(super) name: String,
}

#[derive(Clone, Default)]
pub(super) struct RowDependencies {
    namespace: String,
    pub(super) objects: HashSet<ObjectAddress>,
    pub(super) change_requests: HashSet<ChangeRequestRef>,
    pub(super) issues: HashSet<IssueRef>,
}

impl RowDependencies {
    pub(super) fn derive(row: &LeafSubscriptionRow) -> Self {
        let mut dependencies = Self { namespace: row.namespace.clone(), ..Default::default() };
        for leaf in &row.leaves {
            let (kind, name) = match &leaf.address {
                LeafAddress::Convoy { name } => (ObjectKind::Convoy, name.clone()),
                LeafAddress::Work { convoy, .. } => (ObjectKind::Convoy, convoy.clone()),
                LeafAddress::Vessel { name } => (ObjectKind::Vessel, name.clone()),
                LeafAddress::ChangeRequest { service, scope, number } => {
                    dependencies.change_requests.insert(ChangeRequestRef {
                        namespace: row.namespace.clone(),
                        service: service.clone(),
                        scope: scope.clone(),
                        number: *number,
                    });
                    (ObjectKind::ChangeRequest, flotilla_resources::change_request_record_name(service, scope, *number))
                }
                LeafAddress::Issue { service, scope, number } => {
                    dependencies.issues.insert(IssueRef {
                        namespace: row.namespace.clone(),
                        service: service.clone(),
                        scope: scope.clone(),
                        number: *number,
                    });
                    (ObjectKind::Issue, flotilla_resources::issue_record_name(service, scope, *number))
                }
                LeafAddress::Usage { provider, account } => (ObjectKind::Usage, flotilla_resources::usage_record_name(provider, account)),
                LeafAddress::Artifact { convoy, producer, kind, subject } => {
                    (ObjectKind::Artifact, flotilla_resources::artifact_record_name(convoy, producer, kind, subject))
                }
            };
            dependencies.objects.insert(ObjectAddress { kind, name });
        }
        if let LeafWatcher::TurnDelivery { convoy, .. } = &row.watcher {
            dependencies.objects.insert(ObjectAddress { kind: ObjectKind::Convoy, name: convoy.clone() });
        }
        dependencies
    }
}

struct RouterTask(JoinHandle<()>);
impl Drop for RouterTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct NamespaceRouting {
    index: HashMap<ObjectAddress, HashMap<uuid::Uuid, watch::Sender<()>>>,
    rows: HashMap<uuid::Uuid, watch::Sender<()>>,
    _task: RouterTask,
}

#[derive(Default)]
pub(super) struct SubscriptionRouting {
    pub(super) dependencies: HashMap<uuid::Uuid, RowDependencies>,
    namespaces: HashMap<String, NamespaceRouting>,
    #[cfg(test)]
    pub(super) events_processed: usize,
    #[cfg(test)]
    pub(super) resyncs: usize,
    #[cfg(test)]
    pub(super) removal_visits: usize,
}

impl SubscriptionRouting {
    fn notify(&mut self, namespace: &str, address: Option<&ObjectAddress>) {
        let Some(namespace) = self.namespaces.get(namespace) else { return };
        if let Some(address) = address {
            if let Some(rows) = namespace.index.get(address) {
                for sender in rows.values() {
                    sender.send_replace(());
                }
            }
        } else {
            // Each row is registered once, regardless of its address count.
            for sender in namespace.rows.values() {
                sender.send_replace(());
            }
        }
    }
}

type ObjectEvents = SelectAll<BoxStream<'static, Result<ObjectAddress, ResourceError>>>;

async fn object_watch<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    kind: ObjectKind,
) -> Result<BoxStream<'static, Result<ObjectAddress, ResourceError>>, ResourceError> {
    Ok(backend
        .including_replicas::<T>(namespace)
        .watch()
        .await?
        .map(move |event| {
            event.map(|event| {
                let name = match event {
                    ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => {
                        item.object.metadata.name
                    }
                    ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                };
                ObjectAddress { kind, name }
            })
        })
        .chain(futures::stream::once(async { Err(ResourceError::other("leaf object watch closed")) }))
        .boxed())
}

async fn open_watches(backend: &ResourceBackend, namespace: &str) -> Result<ObjectEvents, ResourceError> {
    let mut streams = SelectAll::new();
    // Watches precede addressed reads, buffering changes across initial load/resync.
    streams.push(object_watch::<Convoy>(backend, namespace, ObjectKind::Convoy).await?);
    streams.push(object_watch::<Vessel>(backend, namespace, ObjectKind::Vessel).await?);
    streams.push(object_watch::<ChangeRequest>(backend, namespace, ObjectKind::ChangeRequest).await?);
    streams.push(object_watch::<Usage>(backend, namespace, ObjectKind::Usage).await?);
    streams.push(object_watch::<Issue>(backend, namespace, ObjectKind::Issue).await?);
    streams.push(object_watch::<Artifact>(backend, namespace, ObjectKind::Artifact).await?);
    Ok(streams)
}

async fn route_events(inner: Weak<LeafSubscriptionTableInner>, backend: ResourceBackend, namespace: String, mut streams: ObjectEvents) {
    let mut recovery = LeafWatchRecovery::default();
    let mut started = tokio::time::Instant::now();
    loop {
        match streams.next().await {
            Some(Ok(address)) => {
                let Some(inner) = inner.upgrade() else { return };
                let mut routing = inner.routing.lock().await;
                routing.notify(&namespace, Some(&address));
                #[cfg(test)]
                {
                    routing.events_processed += 1;
                }
            }
            error => {
                tracing::warn!(?error, %namespace, "leaf object watch lost; resynchronizing");
                loop {
                    let delay = recovery.expired(started.elapsed());
                    tokio::time::sleep(delay).await;
                    // Include watch-open latency in the healthy interval, as
                    // slow store I/O already bounds repeated recovery work.
                    started = tokio::time::Instant::now();
                    match open_watches(&backend, &namespace).await {
                        Ok(watches) => {
                            streams = watches;
                            let Some(inner) = inner.upgrade() else { return };
                            let mut routing = inner.routing.lock().await;
                            routing.notify(&namespace, None);
                            #[cfg(test)]
                            {
                                routing.resyncs += 1;
                            }
                            break;
                        }
                        Err(error) => tracing::warn!(%error, %namespace, "reopen leaf object watches failed"),
                    }
                }
            }
        }
    }
}

impl LeafSubscriptionTable {
    pub(super) async fn register_routing(
        &self,
        row: &LeafSubscriptionRow,
    ) -> Result<(RowDependencies, watch::Receiver<()>), ResourceError> {
        self.register_routing_with(row, async {
            let mut recovery = LeafWatchRecovery::default();
            loop {
                // As in route_events, the interval starts before opening watches.
                let started = tokio::time::Instant::now();
                match open_watches(&self.inner.backend, &row.namespace).await {
                    Ok(streams) => return Ok(streams),
                    Err(ResourceError::WatchExpired { .. }) => {
                        tokio::time::sleep(recovery.expired(started.elapsed())).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await
    }

    // The watch opener is an internal store-I/O seam. Registration rechecks
    // namespace ownership after opening; concurrent candidates are discarded.
    pub(super) async fn register_routing_with(
        &self,
        row: &LeafSubscriptionRow,
        opening: impl Future<Output = Result<ObjectEvents, ResourceError>>,
    ) -> Result<(RowDependencies, watch::Receiver<()>), ResourceError> {
        // Production admission already derived dependencies; tests may arm directly.
        let dependencies =
            self.inner.routing.lock().await.dependencies.get(&row.id).cloned().unwrap_or_else(|| RowDependencies::derive(row));
        tokio::pin!(opening);
        let mut streams = None;
        loop {
            let mut routing = self.inner.routing.lock().await;
            let namespace = match routing.namespaces.entry(row.namespace.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let Some(streams) = streams.take() else {
                        drop(routing);
                        // No table lock is held across I/O or recovery backoff.
                        streams = Some(opening.as_mut().await?);
                        continue;
                    };
                    let task = tokio::spawn(route_events(
                        std::sync::Arc::downgrade(&self.inner),
                        self.inner.backend.clone(),
                        row.namespace.clone(),
                        streams,
                    ));
                    entry.insert(NamespaceRouting { index: HashMap::new(), rows: HashMap::new(), _task: RouterTask(task) })
                }
            };
            let (sender, receiver) = watch::channel(());
            namespace.rows.insert(row.id, sender.clone());
            for address in &dependencies.objects {
                namespace.index.entry(address.clone()).or_default().insert(row.id, sender.clone());
            }
            routing.dependencies.entry(row.id).or_insert_with(|| dependencies.clone());
            return Ok((dependencies, receiver));
        }
    }

    pub(super) async fn remove_routing(&self, id: uuid::Uuid) {
        let mut routing = self.inner.routing.lock().await;
        let Some(dependencies) = routing.dependencies.remove(&id) else { return };
        #[cfg(test)]
        {
            routing.removal_visits += dependencies.objects.len();
        }
        if let Some(namespace) = routing.namespaces.get_mut(&dependencies.namespace) {
            for address in &dependencies.objects {
                if let std::collections::hash_map::Entry::Occupied(mut entry) = namespace.index.entry(address.clone()) {
                    entry.get_mut().remove(&id);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }
            namespace.rows.remove(&id);
            if namespace.rows.is_empty() {
                routing.namespaces.remove(&dependencies.namespace);
            }
        }
    }
}

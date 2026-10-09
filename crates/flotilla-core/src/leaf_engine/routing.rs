//! Object routing is shared by a namespace; rows retain only their dependencies.
use std::{
    collections::{HashMap, HashSet},
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ObjectAddress {
    pub(super) kind: &'static str,
    pub(super) name: String,
}

#[derive(Clone, Default)]
pub(super) struct RowDependencies {
    pub(super) objects: HashSet<ObjectAddress>,
    pub(super) change_requests: HashSet<ChangeRequestRef>,
    pub(super) issues: HashSet<IssueRef>,
}

impl RowDependencies {
    pub(super) fn derive(row: &LeafSubscriptionRow) -> Self {
        let mut dependencies = Self::default();
        for leaf in &row.leaves {
            let (kind, name) = match &leaf.address {
                LeafAddress::Convoy { name } => ("Convoy", name.clone()),
                LeafAddress::Work { convoy, .. } => ("Convoy", convoy.clone()),
                LeafAddress::Vessel { name } => ("Vessel", name.clone()),
                LeafAddress::ChangeRequest { service, scope, number } => {
                    dependencies.change_requests.insert(ChangeRequestRef::from_address(&row.namespace, &leaf.address).expect("CR address"));
                    ("ChangeRequest", flotilla_resources::change_request_record_name(service, scope, *number))
                }
                LeafAddress::Issue { service, scope, number } => {
                    dependencies.issues.insert(IssueRef::from_address(&row.namespace, &leaf.address).expect("issue address"));
                    ("Issue", flotilla_resources::issue_record_name(service, scope, *number))
                }
                LeafAddress::Usage { provider, account } => ("Usage", flotilla_resources::usage_record_name(provider, account)),
                LeafAddress::Artifact { convoy, producer, kind, subject } => {
                    ("Artifact", flotilla_resources::artifact_record_name(convoy, producer, kind, subject))
                }
            };
            dependencies.objects.insert(ObjectAddress { kind, name });
        }
        if let LeafWatcher::TurnDelivery { convoy, .. } = &row.watcher {
            dependencies.objects.insert(ObjectAddress { kind: "Convoy", name: convoy.clone() });
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

#[derive(Default)]
pub(super) struct SubscriptionRouting {
    pub(super) dependencies: HashMap<uuid::Uuid, RowDependencies>,
    index: HashMap<(String, ObjectAddress), HashMap<uuid::Uuid, watch::Sender<()>>>,
    routers: HashMap<String, RouterTask>,
    #[cfg(test)]
    pub(super) events_processed: usize,
    #[cfg(test)]
    pub(super) resyncs: usize,
}

impl SubscriptionRouting {
    fn notify(&mut self, namespace: &str, address: Option<&ObjectAddress>) {
        if let Some(address) = address {
            if let Some(rows) = self.index.get(&(namespace.to_owned(), address.clone())) {
                for sender in rows.values() {
                    sender.send_replace(());
                }
            }
        } else {
            // Resync touches each row once, even if it addresses several objects.
            let mut notified = HashSet::new();
            for ((ns, _), rows) in &self.index {
                if ns == namespace {
                    for (id, sender) in rows {
                        if notified.insert(*id) {
                            sender.send_replace(());
                        }
                    }
                }
            }
        }
    }
}

type ObjectEvents = SelectAll<BoxStream<'static, Result<ObjectAddress, ResourceError>>>;

async fn object_watch<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<BoxStream<'static, Result<ObjectAddress, ResourceError>>, ResourceError> {
    Ok(backend
        .including_replicas::<T>(namespace)
        .watch()
        .await?
        .map(|event| {
            event.map(|event| {
                let name = match event {
                    ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => {
                        item.object.metadata.name
                    }
                    ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                };
                ObjectAddress { kind: T::API_PATHS.kind, name }
            })
        })
        .chain(futures::stream::once(async { Err(ResourceError::other("leaf object watch closed")) }))
        .boxed())
}

async fn open_watches(backend: &ResourceBackend, namespace: &str) -> Result<ObjectEvents, ResourceError> {
    let mut streams = SelectAll::new();
    // Watches precede addressed reads, buffering changes across initial load/resync.
    streams.push(object_watch::<Convoy>(backend, namespace).await?);
    streams.push(object_watch::<Vessel>(backend, namespace).await?);
    streams.push(object_watch::<ChangeRequest>(backend, namespace).await?);
    streams.push(object_watch::<Usage>(backend, namespace).await?);
    streams.push(object_watch::<Issue>(backend, namespace).await?);
    streams.push(object_watch::<Artifact>(backend, namespace).await?);
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
        let mut routing = self.inner.routing.lock().await;
        if !routing.routers.contains_key(&row.namespace) {
            let mut recovery = LeafWatchRecovery::default();
            let streams = loop {
                let started = tokio::time::Instant::now();
                match open_watches(&self.inner.backend, &row.namespace).await {
                    Ok(streams) => break streams,
                    Err(ResourceError::WatchExpired { .. }) => {
                        tokio::time::sleep(recovery.expired(started.elapsed())).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let task = tokio::spawn(route_events(
                std::sync::Arc::downgrade(&self.inner),
                self.inner.backend.clone(),
                row.namespace.clone(),
                streams,
            ));
            routing.routers.insert(row.namespace.clone(), RouterTask(task));
        }
        // Tests can install rows directly; production rows have already run demand admission.
        let dependencies = routing.dependencies.entry(row.id).or_insert_with(|| RowDependencies::derive(row)).clone();
        let (sender, receiver) = watch::channel(());
        for address in &dependencies.objects {
            routing.index.entry((row.namespace.clone(), address.clone())).or_default().insert(row.id, sender.clone());
        }
        Ok((dependencies, receiver))
    }

    pub(super) async fn remove_routing(&self, id: uuid::Uuid) {
        let mut routing = self.inner.routing.lock().await;
        routing.dependencies.remove(&id);
        routing.index.retain(|_, rows| {
            rows.remove(&id);
            !rows.is_empty()
        });
        let active: HashSet<_> = routing.index.keys().map(|(namespace, _)| namespace.clone()).collect();
        routing.routers.retain(|namespace, _| active.contains(namespace));
    }
}

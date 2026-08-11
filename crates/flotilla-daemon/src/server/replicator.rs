use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
#[cfg(feature = "test-support")]
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::NodeId;
#[cfg(feature = "test-support")]
use flotilla_protocol::{
    Command, CommandAction, CommandValue, DaemonEvent, ResourceCursor, ResourceReadEnvelope, ResourceReadRecord, ResourceRecordType,
};
use flotilla_resources::{
    HttpBackend, ReadWatchEvent, ReplicationClass, Resource, ResourceBackend, ResourceProvenance, WatchEvent, WatchStart,
};
#[cfg(feature = "test-support")]
use flotilla_resources::{K8sWatchEvent, ResourceList, ResourceObject};
use futures::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::remote_commands::RemoteCommandRouter;

const REPLICATION_NAMESPACE: &str = "flotilla";
const REPLICATION_RETRY: RetryBackoff =
    RetryBackoff { initial: Duration::from_millis(100), maximum: Duration::from_secs(30), reset_after: Duration::from_secs(60) };

#[derive(Clone, Copy)]
struct RetryBackoff {
    initial: Duration,
    maximum: Duration,
    reset_after: Duration,
}

#[derive(Default)]
pub(super) struct PeerReplicatorSupervisors {
    generations: HashMap<NodeId, ActiveGeneration>,
}

struct ActiveGeneration {
    generation: u64,
    cancellation: CancellationToken,
    socket_path_source: SocketPathSource,
}

/// Generation-scoped source refreshed by same-generation reconnect notices.
///
/// Replication attempts resolve this value after each backoff so they can move
/// from a dead forwarded socket, or from no socket, to the live transport.
#[derive(Clone)]
struct SocketPathSource {
    path: watch::Sender<Option<PathBuf>>,
}

impl SocketPathSource {
    fn new(path: Option<PathBuf>) -> Self {
        let (path, _) = watch::channel(path);
        Self { path }
    }

    async fn resolve(&self) -> Result<PathBuf, String> {
        let mut path = self.path.subscribe();
        loop {
            if let Some(path) = path.borrow_and_update().clone() {
                return Ok(path);
            }
            path.changed().await.map_err(|_| "peer resource socket path source closed".to_string())?;
        }
    }

    #[cfg(test)]
    fn current(&self) -> Option<PathBuf> {
        self.path.borrow().clone()
    }

    fn update(&self, path: PathBuf) {
        self.path.send_replace(Some(path));
    }
}

impl PeerReplicatorSupervisors {
    pub(super) async fn peer_connected(
        &mut self,
        _router: RemoteCommandRouter,
        daemon: Arc<InProcessDaemon>,
        peer: NodeId,
        generation: u64,
        resource_socket_path: Option<PathBuf>,
    ) {
        let Some((cancellation, socket_path_source)) = self.begin_generation(&peer, generation, resource_socket_path.clone()) else {
            return;
        };
        daemon.begin_peer_resource_replication(&peer).await;
        let transport = match resource_socket_path {
            Some(_) => ReplicationTransport::Http(socket_path_source),
            #[cfg(feature = "test-support")]
            None => ReplicationTransport::Routed(_router),
            #[cfg(not(feature = "test-support"))]
            None => {
                debug!(%peer, generation, "peer has no forwarded resource socket; replication waits for an outbound SSH connection");
                ReplicationTransport::Http(socket_path_source)
            }
        };
        flotilla_resources::for_each_registered_resource!(spawn_kind, &daemon, &peer, generation, &transport, &cancellation)
    }

    /// Cancel and drop a peer's resource replicators, but only if `generation`
    /// still matches the generation currently tracked for that peer.
    ///
    /// Callers must only invoke this from a connection-owning task that has
    /// no retry loop of its own (a terminal teardown), never from a
    /// transient, still-retrying disconnect — otherwise replication could
    /// not heal from a reconnect. The generation check guards against a
    /// stale/displaced connection's belated teardown cancelling a newer,
    /// already-reconnected generation's replicators.
    pub(super) fn peer_disconnected(&mut self, peer: &NodeId, generation: u64) {
        let is_current = self.generations.get(peer).is_some_and(|active| active.generation == generation);
        if !is_current {
            debug!(%peer, generation, "ignoring teardown for stale or already-superseded peer generation");
            return;
        }
        if let Some(active) = self.generations.remove(peer) {
            active.cancellation.cancel();
            debug!(%peer, generation, "peer permanently disconnected; cancelled resource replicators");
        }
    }

    fn begin_generation(
        &mut self,
        peer: &NodeId,
        generation: u64,
        resource_socket_path: Option<PathBuf>,
    ) -> Option<(CancellationToken, SocketPathSource)> {
        if let Some(active) = self.generations.get(peer) {
            if generation <= active.generation {
                if generation == active.generation {
                    if let Some(path) = resource_socket_path {
                        active.socket_path_source.update(path);
                    }
                }
                debug!(
                    %peer,
                    generation,
                    active_generation = active.generation,
                    "ignoring stale or duplicate peer replicator generation"
                );
                return None;
            }
            active.cancellation.cancel();
        }

        let cancellation = CancellationToken::new();
        let socket_path_source = SocketPathSource::new(resource_socket_path);
        self.generations.insert(peer.clone(), ActiveGeneration {
            generation,
            cancellation: cancellation.clone(),
            socket_path_source: socket_path_source.clone(),
        });
        Some((cancellation, socket_path_source))
    }
}

#[derive(Clone)]
enum ReplicationTransport {
    Http(SocketPathSource),
    #[cfg(feature = "test-support")]
    Routed(RemoteCommandRouter),
}

fn spawn_kind<T: Resource>(
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    generation: u64,
    transport: &ReplicationTransport,
    cancellation: &CancellationToken,
) {
    if T::REPLICATION_CLASS == ReplicationClass::None {
        return;
    }
    match transport.clone() {
        ReplicationTransport::Http(socket_path_source) => {
            let relay_daemon = Arc::clone(daemon);
            let relay_peer = peer.clone();
            let relay_cancellation = cancellation.clone();
            tokio::spawn(async move {
                let run_daemon = Arc::clone(&relay_daemon);
                let run_peer = relay_peer.clone();
                supervise_kind(
                    relay_peer,
                    generation,
                    T::API_PATHS.kind,
                    relay_cancellation,
                    REPLICATION_RETRY,
                    move || {
                        let socket_path_source = socket_path_source.clone();
                        async move { socket_path_source.resolve().await }
                    },
                    move |path| {
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        async move {
                            let http = HttpBackend::from_unix_socket(path).map_err(|error| error.to_string())?;
                            replicate_relay_over_http::<T>(http, &daemon, &peer).await
                        }
                    },
                )
                .await;
            });
        }
        #[cfg(feature = "test-support")]
        ReplicationTransport::Routed(router) => {
            let relay_daemon = Arc::clone(daemon);
            let relay_peer = peer.clone();
            let relay_cancellation = cancellation.clone();
            tokio::spawn(async move {
                let run_daemon = Arc::clone(&relay_daemon);
                let run_peer = relay_peer.clone();
                supervise_kind(
                    relay_peer,
                    generation,
                    T::API_PATHS.kind,
                    relay_cancellation,
                    REPLICATION_RETRY,
                    || async { Ok(()) },
                    move |()| {
                        let router = router.clone();
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        async move { replicate_relay_over_routed_watch::<T>(&router, &daemon, &peer).await }
                    },
                )
                .await;
            });
        }
    }
    let daemon = Arc::clone(daemon);
    let peer = peer.clone();
    let transport = transport.clone();
    let cancellation = cancellation.clone();
    tokio::spawn(async move {
        match transport {
            ReplicationTransport::Http(socket_path_source) => {
                let run_daemon = Arc::clone(&daemon);
                let run_peer = peer.clone();
                supervise_kind(
                    peer,
                    generation,
                    T::API_PATHS.kind,
                    cancellation,
                    REPLICATION_RETRY,
                    move || {
                        let socket_path_source = socket_path_source.clone();
                        async move { socket_path_source.resolve().await }
                    },
                    move |path| {
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        async move {
                            let http = HttpBackend::from_unix_socket(path).map_err(|error| error.to_string())?;
                            let result = replicate_kind_over_http::<T>(http, &daemon, &peer).await;
                            if let Err(error) = &result {
                                daemon.report_resource_replication_failure(&peer, T::API_PATHS.kind, error).await;
                            }
                            result
                        }
                    },
                )
                .await;
            }
            #[cfg(feature = "test-support")]
            ReplicationTransport::Routed(router) => {
                let run_daemon = Arc::clone(&daemon);
                let run_peer = peer.clone();
                supervise_kind(
                    peer,
                    generation,
                    T::API_PATHS.kind,
                    cancellation,
                    REPLICATION_RETRY,
                    || async { Ok(()) },
                    move |()| {
                        let router = router.clone();
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        async move {
                            let result = replicate_kind_over_routed_watch::<T>(&router, &daemon, &peer).await;
                            if let Err(error) = &result {
                                daemon.report_resource_replication_failure(&peer, T::API_PATHS.kind, error).await;
                            }
                            result
                        }
                    },
                )
                .await;
            }
        }
    });
}

async fn replicate_relay_over_http<T: Resource>(http: HttpBackend, daemon: &Arc<InProcessDaemon>, peer: &NodeId) -> Result<(), String> {
    let mut watch = http.watch_replica_sources_typed::<T>(REPLICATION_NAMESPACE).await.map_err(|error| error.to_string())?;
    while let Some(event) = watch.next().await {
        let event = event.map_err(|error| error.to_string())?;
        if let ReadWatchEvent::DeletedByName { mut tombstone, provenance } = event {
            let ResourceProvenance::Replica { origin_root, last_synced_at } = provenance else {
                continue;
            };
            if &origin_root == daemon.node_id() || &origin_root == peer {
                continue;
            }
            tombstone.annotations.remove("flotilla.work/origin-root");
            tombstone.annotations.remove("flotilla.work/last-synced-at");
            daemon
                .resource_backend()
                .replica_writer::<T>(origin_root, REPLICATION_NAMESPACE)
                .apply(WatchEvent::DeletedByName(tombstone), last_synced_at)
                .await
                .map_err(|error| error.to_string())?;
            continue;
        }
        let (kind, mut source) = match event {
            ReadWatchEvent::Added(source) => (StoredRelayEventKind::Added, source),
            ReadWatchEvent::Modified(source) => (StoredRelayEventKind::Modified, source),
            ReadWatchEvent::Deleted(source) => (StoredRelayEventKind::Deleted, source),
            ReadWatchEvent::DeletedByName { .. } => unreachable!("handled above"),
        };
        let ResourceProvenance::Replica { origin_root, last_synced_at } = source.provenance else {
            continue;
        };
        if &origin_root == daemon.node_id() || &origin_root == peer {
            continue;
        }
        source.object.metadata.annotations.remove("flotilla.work/origin-root");
        source.object.metadata.annotations.remove("flotilla.work/last-synced-at");
        let event = match kind {
            StoredRelayEventKind::Added => WatchEvent::Added(source.object),
            StoredRelayEventKind::Modified => WatchEvent::Modified(source.object),
            StoredRelayEventKind::Deleted => WatchEvent::Deleted(source.object),
        };
        daemon
            .resource_backend()
            .replica_writer::<T>(origin_root, REPLICATION_NAMESPACE)
            .apply(event, last_synced_at)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StoredRelayEventKind {
    Added,
    Modified,
    Deleted,
}

async fn supervise_kind<I, S, SourceFut, F, Fut>(
    peer: NodeId,
    generation: u64,
    kind: &'static str,
    cancellation: CancellationToken,
    retry: RetryBackoff,
    mut source: S,
    mut run: F,
) where
    S: FnMut() -> SourceFut,
    SourceFut: Future<Output = Result<I, String>>,
    F: FnMut(I) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut backoff = retry.initial;
    loop {
        let started_at = tokio::time::Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = async {
                let input = source().await?;
                run(input).await
            } => result,
        };
        if started_at.elapsed() >= retry.reset_after {
            backoff = retry.initial;
        }
        match result {
            Ok(()) => debug!(%peer, generation, kind, "resource replicator ended; restarting after backoff"),
            Err(error) => warn!(%peer, generation, kind, %error, "resource replicator failed; restarting after backoff"),
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = backoff.saturating_mul(2).min(retry.maximum);
    }
}

pub(super) async fn replicate_kind_over_http<T: Resource>(
    http: HttpBackend,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
) -> Result<(), String> {
    let remote = ResourceBackend::Http(http).using::<T>(REPLICATION_NAMESPACE);
    let writer = daemon.resource_backend().replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    let mut listed = remote.list().await.map_err(|error| error.to_string())?;
    // Validate the persisted cache before trusting its cursor. The SQLite
    // backend quarantines an undecodable replica partition and removes this
    // cursor, turning a cache schema mismatch into the full relist below.
    daemon.resource_backend().including_replicas::<T>(REPLICATION_NAMESPACE).list().await.map_err(|error| error.to_string())?;
    let cursor = writer.cursor().await.map_err(|error| error.to_string())?;
    if let Some(cursor) = cursor.clone().filter(|cursor| cursor.generation == listed.generation) {
        let start = match cursor.generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version: cursor.resource_version },
            None => WatchStart::FromVersion(cursor.resource_version),
        };
        match remote.watch(start).await {
            Ok(watch) => {
                daemon.report_resource_replication_healthy(peer, T::API_PATHS.kind).await;
                match apply_http_watch(watch, &writer).await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        debug!(%peer, kind = T::API_PATHS.kind, %error, "replica cursor watch failed; relisting origin");
                        listed = remote.list().await.map_err(|error| error.to_string())?;
                    }
                }
            }
            Err(error) => {
                debug!(%peer, kind = T::API_PATHS.kind, %error, "replica cursor rejected; relisting origin");
            }
        }
    } else if cursor.is_some() {
        debug!(
            %peer,
            kind = T::API_PATHS.kind,
            stored_generation = ?cursor.as_ref().and_then(|cursor| cursor.generation.as_deref()),
            origin_generation = ?listed.generation,
            "replica origin generation changed; replacing local view"
        );
    }

    let start = WatchStart::resuming_from(&listed);
    writer.replace(&listed, Utc::now()).await.map_err(|error| error.to_string())?;
    let watch = remote.watch(start).await.map_err(|error| error.to_string())?;
    daemon.report_resource_replication_healthy(peer, T::API_PATHS.kind).await;
    apply_http_watch(watch, &writer).await
}

async fn apply_http_watch<T: Resource>(
    mut watch: flotilla_resources::WatchStream<T>,
    writer: &flotilla_resources::ReplicaWriter<T>,
) -> Result<(), String> {
    while let Some(event) = watch.next().await {
        writer.apply(event.map_err(|error| error.to_string())?, Utc::now()).await.map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(feature = "test-support")]
async fn replicate_kind_over_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
) -> Result<(), String> {
    let writer = daemon.resource_backend().replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    daemon.resource_backend().including_replicas::<T>(REPLICATION_NAMESPACE).list().await.map_err(|error| error.to_string())?;
    let cursor = writer.cursor().await.map_err(|error| error.to_string())?;
    match run_routed_watch::<T>(router, daemon, peer, cursor.clone()).await {
        Ok(()) => Ok(()),
        Err(error)
            if cursor.is_some() && (error.contains("expired") || error.contains("generation") || error.contains("resourceVersion")) =>
        {
            debug!(%peer, kind = T::API_PATHS.kind, %error, "replica cursor rejected; relisting origin");
            run_routed_watch::<T>(router, daemon, peer, None).await
        }
        Err(error) => Err(error),
    }
}

#[cfg(feature = "test-support")]
async fn run_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    cursor: Option<flotilla_resources::ReplicaCursor>,
) -> Result<(), String> {
    let resuming = cursor.is_some();
    let protocol_cursor = cursor.map(|cursor| ResourceCursor::from_position(cursor.resource_version, cursor.generation));
    let mut events = daemon.subscribe();
    let command_id = router
        .dispatch_execute_for_principal(
            Command {
                node_id: Some(peer.clone()),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceWatch {
                    namespace: REPLICATION_NAMESPACE.to_string(),
                    kind: T::API_PATHS.plural.to_string(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: protocol_cursor,
                },
            },
            None,
        )
        .await?;
    daemon.report_resource_replication_healthy(peer, T::API_PATHS.kind).await;
    let writer = daemon.resource_backend().replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    let mut initial = Vec::<ResourceObject<T>>::new();
    let mut initializing = !resuming;

    loop {
        match events.recv().await {
            Ok(DaemonEvent::CommandStepUpdate {
                command_id: event_command_id,
                status: flotilla_protocol::StepStatus::Produced { value },
                ..
            }) if event_command_id == command_id => {
                let CommandValue::ResourceWatchEvent(response) = *value else {
                    continue;
                };
                if response.resource_kind != T::API_PATHS.kind {
                    continue;
                }
                apply_response(&writer, &mut initial, &mut initializing, *response).await?;
            }
            Ok(DaemonEvent::CommandFinished { command_id: event_command_id, result, .. }) if event_command_id == command_id => {
                return match result {
                    CommandValue::Cancelled | CommandValue::Ok => Ok(()),
                    CommandValue::Error { message } => Err(message),
                    other => Err(format!("resource watch ended unexpectedly: {other:?}")),
                };
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(%peer, kind = T::API_PATHS.kind, skipped, "resource replicator lagged; reconnect will resume from stored cursor");
                return Err("resource replicator event subscriber lagged".to_string());
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("daemon event stream closed".to_string()),
        }
    }
}

#[cfg(feature = "test-support")]
async fn replicate_relay_over_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
) -> Result<(), String> {
    let mut events = daemon.subscribe();
    let command_id = router
        .dispatch_execute_for_principal(
            Command {
                node_id: Some(peer.clone()),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceWatch {
                    namespace: REPLICATION_NAMESPACE.to_string(),
                    kind: T::API_PATHS.plural.to_string(),
                    name: None,
                    include_replicas: false,
                    replica_sources: true,
                    cursor: None,
                },
            },
            None,
        )
        .await?;
    loop {
        match events.recv().await {
            Ok(DaemonEvent::CommandStepUpdate {
                command_id: event_command_id,
                status: flotilla_protocol::StepStatus::Produced { value },
                ..
            }) if event_command_id == command_id => {
                let CommandValue::ResourceWatchEvent(response) = *value else {
                    continue;
                };
                if response.resource_kind == T::API_PATHS.kind {
                    apply_relay_response::<T>(daemon, peer, *response).await?;
                }
            }
            Ok(DaemonEvent::CommandFinished { command_id: event_command_id, result, .. }) if event_command_id == command_id => {
                return match result {
                    CommandValue::Cancelled | CommandValue::Ok => Ok(()),
                    CommandValue::Error { message } => Err(message),
                    other => Err(format!("resource relay watch ended unexpectedly: {other:?}")),
                };
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(%peer, kind = T::API_PATHS.kind, skipped, "resource relay lagged; reconnect will relist");
                return Err("resource relay event subscriber lagged".to_string());
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("daemon event stream closed".to_string()),
        }
    }
}

#[cfg(feature = "test-support")]
async fn apply_relay_response<T: Resource>(
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    response: ResourceReadEnvelope,
) -> Result<(), String> {
    for record in response.records {
        let Some(event) = record_watch_event::<T>(record)? else {
            continue;
        };
        if let WatchEvent::DeletedByName(mut tombstone) = event {
            let Some(origin) = tombstone.annotations.remove("flotilla.work/origin-root") else {
                continue;
            };
            let origin = NodeId::new(origin);
            if &origin == daemon.node_id() || &origin == peer {
                continue;
            }
            let synced_at = tombstone
                .annotations
                .remove("flotilla.work/last-synced-at")
                .ok_or_else(|| "relayed tombstone is missing last-synced-at".to_string())
                .and_then(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .map(|value| value.with_timezone(&Utc))
                        .map_err(|error| format!("decode relayed tombstone sync timestamp: {error}"))
                })?;
            daemon
                .resource_backend()
                .replica_writer::<T>(origin, REPLICATION_NAMESPACE)
                .apply(WatchEvent::DeletedByName(tombstone), synced_at)
                .await
                .map_err(|error| error.to_string())?;
            continue;
        }
        let object = match &event {
            WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => object,
            WatchEvent::DeletedByName(_) => unreachable!("handled above"),
        };
        let Some(origin) = object.metadata.annotations.get("flotilla.work/origin-root") else {
            continue;
        };
        let origin = NodeId::new(origin.clone());
        if &origin == daemon.node_id() || &origin == peer {
            continue;
        }
        let synced_at = object
            .metadata
            .annotations
            .get("flotilla.work/last-synced-at")
            .ok_or_else(|| "relayed resource is missing last-synced-at".to_string())
            .and_then(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|value| value.with_timezone(&Utc))
                    .map_err(|error| format!("decode relayed sync timestamp: {error}"))
            })?;
        let strip = |mut object: ResourceObject<T>| {
            object.metadata.annotations.remove("flotilla.work/origin-root");
            object.metadata.annotations.remove("flotilla.work/last-synced-at");
            object
        };
        let event = match event {
            WatchEvent::Added(object) => WatchEvent::Added(strip(object)),
            WatchEvent::Modified(object) => WatchEvent::Modified(strip(object)),
            WatchEvent::Deleted(object) => WatchEvent::Deleted(strip(object)),
            WatchEvent::DeletedByName(_) => unreachable!("handled above"),
        };
        daemon
            .resource_backend()
            .replica_writer::<T>(origin, REPLICATION_NAMESPACE)
            .apply(event, synced_at)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(feature = "test-support")]
async fn apply_response<T: Resource>(
    writer: &flotilla_resources::ReplicaWriter<T>,
    initial: &mut Vec<ResourceObject<T>>,
    initializing: &mut bool,
    response: ResourceReadEnvelope,
) -> Result<(), String> {
    let (resource_version, generation) = response.cursor.position()?;
    for record in response.records {
        if record.record_type == ResourceRecordType::Bookmark {
            if *initializing {
                writer
                    .replace(
                        &ResourceList {
                            items: std::mem::take(initial),
                            resource_version: resource_version.clone(),
                            generation: generation.clone(),
                        },
                        Utc::now(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                *initializing = false;
            }
            continue;
        }
        let Some(event) = record_watch_event::<T>(record)? else {
            continue;
        };
        if *initializing && matches!(event, WatchEvent::Added(_)) {
            let WatchEvent::Added(object) = event else { unreachable!("matched added event") };
            initial.push(object);
        } else {
            writer.apply(event, Utc::now()).await.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(feature = "test-support")]
fn record_watch_event<T: Resource>(record: ResourceReadRecord) -> Result<Option<WatchEvent<T>>, String> {
    let event_type = match record.record_type {
        ResourceRecordType::Current | ResourceRecordType::Added => "ADDED",
        ResourceRecordType::Modified => "MODIFIED",
        ResourceRecordType::Deleted => "DELETED",
        ResourceRecordType::Bookmark => return Ok(None),
    };
    let object = record.object.ok_or_else(|| format!("{event_type} resource record is missing object"))?;
    let encoded = serde_json::json!({ "type": event_type, "object": object.clone() });
    match serde_json::from_value::<K8sWatchEvent<T>>(encoded) {
        Ok(event) => event.into_watch_event().map(Some).map_err(|error| error.to_string()),
        Err(_) if event_type == "DELETED" => {
            let metadata = &object["metadata"];
            let name = metadata["name"]
                .as_str()
                .ok_or_else(|| format!("decode replicated {} tombstone: missing name", T::API_PATHS.kind))?
                .to_string();
            let namespace = metadata["namespace"].as_str().unwrap_or_default().to_string();
            let resource_version = metadata["resourceVersion"]
                .as_str()
                .ok_or_else(|| format!("decode replicated {} tombstone: missing resourceVersion", T::API_PATHS.kind))?
                .to_string();
            let annotations = metadata["annotations"]
                .as_object()
                .map(|annotations| {
                    annotations.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect()
                })
                .unwrap_or_default();
            Ok(Some(WatchEvent::DeletedByName(flotilla_resources::ResourceTombstone { name, namespace, resource_version, annotations })))
        }
        Err(error) => Err(format!("decode replicated {} event: {error}", T::API_PATHS.kind)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use flotilla_resources::Convoy;
    use serde_json::json;

    use super::*;

    #[test]
    fn routed_replication_decodes_name_tombstones() {
        let event = record_watch_event::<Convoy>(ResourceReadRecord {
            record_type: ResourceRecordType::Deleted,
            provenance: flotilla_protocol::ResourceRecordProvenance::Local { node_id: NodeId::new("authority") },
            object: Some(json!({
                "apiVersion": "flotilla.work/v1",
                "kind": "Convoy",
                "metadata": {
                    "name": "lost-at-authority",
                    "namespace": "flotilla",
                    "resourceVersion": "9",
                    "annotations": {
                        "flotilla.work/origin-root": "authority",
                        "flotilla.work/last-synced-at": "2026-08-11T20:00:00Z",
                    },
                },
            })),
        })
        .expect("decode routed tombstone")
        .expect("deleted record produces an event");

        assert!(matches!(
            event,
            WatchEvent::DeletedByName(tombstone)
                if tombstone.name == "lost-at-authority"
                    && tombstone.namespace == "flotilla"
                    && tombstone.annotations["flotilla.work/origin-root"] == "authority"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn socket_path_source_changes_are_resolved_between_retries() {
        let source = SocketPathSource::new(Some(PathBuf::from("/tmp/first.sock")));
        let attempted_paths = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(supervise_kind(
            NodeId::new("peer"),
            1,
            Convoy::API_PATHS.kind,
            cancellation.clone(),
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(4), reset_after: Duration::from_secs(60) },
            {
                let source = source.clone();
                move || {
                    let source = source.clone();
                    async move { source.resolve().await }
                }
            },
            {
                let attempted_paths = Arc::clone(&attempted_paths);
                move |path| {
                    attempted_paths.lock().expect("attempted paths lock").push(path);
                    async { Err("transient watch failure".to_string()) }
                }
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*attempted_paths.lock().expect("attempted paths lock"), vec![PathBuf::from("/tmp/first.sock")]);

        source.update(PathBuf::from("/tmp/second.sock"));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(*attempted_paths.lock().expect("attempted paths lock"), vec![
            PathBuf::from("/tmp/first.sock"),
            PathBuf::from("/tmp/second.sock")
        ]);

        cancellation.cancel();
        task.await.expect("replicator supervisor task");
    }

    #[tokio::test(start_paused = true)]
    async fn missing_socket_path_waits_until_the_source_resolves() {
        let source = SocketPathSource::new(None);
        let resolutions = Arc::new(AtomicUsize::new(0));
        let attempted_paths = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(supervise_kind(
            NodeId::new("peer"),
            1,
            Convoy::API_PATHS.kind,
            cancellation.clone(),
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(4), reset_after: Duration::from_secs(60) },
            {
                let source = source.clone();
                let resolutions = Arc::clone(&resolutions);
                move || {
                    let source = source.clone();
                    resolutions.fetch_add(1, Ordering::SeqCst);
                    async move { source.resolve().await }
                }
            },
            {
                let attempted_paths = Arc::clone(&attempted_paths);
                move |path| {
                    attempted_paths.lock().expect("attempted paths lock").push(path);
                    async { Err("transient watch failure".to_string()) }
                }
            },
        ));

        tokio::task::yield_now().await;
        assert!(attempted_paths.lock().expect("attempted paths lock").is_empty());
        assert_eq!(resolutions.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(resolutions.load(Ordering::SeqCst), 1, "an unavailable source should wait instead of polling and warning");

        source.update(PathBuf::from("/tmp/ready.sock"));
        tokio::task::yield_now().await;
        assert_eq!(*attempted_paths.lock().expect("attempted paths lock"), vec![PathBuf::from("/tmp/ready.sock")]);

        cancellation.cancel();
        task.await.expect("replicator supervisor task");
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_event_failure_retries_the_kind() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(supervise_kind(
            NodeId::new("peer"),
            1,
            Convoy::API_PATHS.kind,
            cancellation.clone(),
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(4), reset_after: Duration::from_secs(60) },
            || async { Ok(()) },
            {
                let attempts = Arc::clone(&attempts);
                move |()| {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        serde_json::from_value::<K8sWatchEvent<Convoy>>(json!({"type": "BROKEN"}))
                            .map(|_| ())
                            .map_err(|error| format!("decode replicated Convoy event: {error}"))
                    }
                }
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        cancellation.cancel();
        task.await.expect("replicator supervisor task");
    }

    #[tokio::test(start_paused = true)]
    async fn newer_generation_cancels_a_replicator_during_backoff() {
        let peer = NodeId::new("peer");
        let mut supervisors = PeerReplicatorSupervisors::default();
        let (old_cancellation, _) = supervisors.begin_generation(&peer, 7, None).expect("start old generation");
        let attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(supervise_kind(
            peer.clone(),
            7,
            Convoy::API_PATHS.kind,
            old_cancellation,
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(4), reset_after: Duration::from_secs(60) },
            || async { Ok(()) },
            {
                let attempts = Arc::clone(&attempts);
                move |()| {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        Err("transient watch failure".to_string())
                    }
                }
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        supervisors.begin_generation(&peer, 8, None).expect("start new generation");
        task.await.expect("cancelled old supervisor");
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "cancelled generation must not retry after its backoff");
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_resets_after_a_stable_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(supervise_kind(
            NodeId::new("peer"),
            1,
            Convoy::API_PATHS.kind,
            cancellation.clone(),
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(8), reset_after: Duration::from_secs(5) },
            || async { Ok(()) },
            {
                let attempts = Arc::clone(&attempts);
                move |()| {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 1 {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                        Err("watch ended".to_string())
                    }
                }
            },
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "stable attempts reset the next delay to the initial backoff");

        cancellation.cancel();
        task.await.expect("replicator supervisor task");
    }

    #[test]
    fn duplicate_and_stale_notices_do_not_cause_duplicate_application() {
        let peer = NodeId::new("peer");
        let mut supervisors = PeerReplicatorSupervisors::default();
        let mut applications = 0;

        let (_, source) = supervisors.begin_generation(&peer, 4, None).expect("start generation");
        applications += 1;
        assert!(supervisors.begin_generation(&peer, 4, Some(PathBuf::from("/tmp/current.sock"))).is_none());
        assert!(supervisors.begin_generation(&peer, 3, Some(PathBuf::from("/tmp/stale.sock"))).is_none());
        assert_eq!(
            source.current(),
            Some(PathBuf::from("/tmp/current.sock")),
            "same-generation reconnects refresh the live source, while stale notices cannot replace it"
        );
        assert_eq!(applications, 1, "one generation may apply only once despite duplicate or stale notices");

        if supervisors.begin_generation(&peer, 5, None).is_some() {
            applications += 1;
        }
        assert_eq!(applications, 2, "a newer generation starts exactly one new application stream");
    }

    #[test]
    fn permanent_disconnect_cancels_and_removes_the_current_generation() {
        let peer = NodeId::new("peer");
        let mut supervisors = PeerReplicatorSupervisors::default();
        let (cancellation, _) = supervisors.begin_generation(&peer, 3, None).expect("start generation");
        assert!(!cancellation.is_cancelled());

        supervisors.peer_disconnected(&peer, 3);

        assert!(cancellation.is_cancelled(), "terminal teardown of the current generation must cancel its replicators");
        assert!(
            supervisors.begin_generation(&peer, 3, None).is_some(),
            "removing the entry lets a later reconnect at the same generation number start fresh, \
             instead of being rejected as stale"
        );
    }

    #[test]
    fn stale_disconnect_notice_does_not_cancel_a_newer_generation() {
        let peer = NodeId::new("peer");
        let mut supervisors = PeerReplicatorSupervisors::default();
        let (_old_cancellation, _) = supervisors.begin_generation(&peer, 1, None).expect("start old generation");
        let (new_cancellation, _) = supervisors.begin_generation(&peer, 2, None).expect("start newer generation");

        // A belated teardown notice for the superseded generation (e.g. the
        // old connection's task finally winding down after being displaced)
        // must not touch the newer, currently-active generation.
        supervisors.peer_disconnected(&peer, 1);

        assert!(!new_cancellation.is_cancelled(), "a stale-generation disconnect must not cancel the current generation's replicators");
        assert!(
            supervisors.begin_generation(&peer, 2, None).is_none(),
            "the current generation's map entry must still be present after a stale disconnect notice"
        );
    }

    #[test]
    fn unknown_peer_disconnect_is_a_no_op() {
        let peer = NodeId::new("peer");
        let mut supervisors = PeerReplicatorSupervisors::default();

        supervisors.peer_disconnected(&peer, 1);

        assert!(
            supervisors.begin_generation(&peer, 1, None).is_some(),
            "disconnecting an untracked peer must not leave stray state behind"
        );
    }
}

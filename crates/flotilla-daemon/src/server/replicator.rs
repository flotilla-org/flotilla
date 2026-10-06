use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
#[cfg(any(test, feature = "test-support"))]
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::NodeId;
#[cfg(any(test, feature = "test-support"))]
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, ResourceReadEnvelope, ResourceReadRecord, ResourceRecordType};
use flotilla_resources::{
    DigestQuery, HttpBackend, PartitionDigest, ReadWatchEvent, ReplicationClass, Resource, ResourceBackend, ResourceProvenance, WatchEvent,
    WatchStart,
};
#[cfg(any(test, feature = "test-support"))]
use flotilla_resources::{K8sWatchEvent, ResourceList, ResourceObject};
use futures::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::remote_commands::RemoteCommandRouter;

const DIGEST_INTERVAL: Duration = Duration::from_secs(60);

const REPLICATION_NAMESPACE: &str = "flotilla";
const REPLICATION_RETRY: RetryBackoff =
    RetryBackoff { initial: Duration::from_millis(100), maximum: Duration::from_secs(30), reset_after: Duration::from_secs(60) };

/// Observation replicas share the observed read view, separately from desired
/// state. Its local generation is ephemeral; received replicas can be durable.
#[derive(Clone, Copy)]
pub(super) enum ReplicationStore {
    Durable,
    Observed,
}

impl ReplicationStore {
    fn backend(self, daemon: &InProcessDaemon) -> ResourceBackend {
        match self {
            Self::Durable => daemon.resource_backend(),
            Self::Observed => daemon.observed_resource_backend(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn kind<T: Resource>(self) -> String {
        match self {
            Self::Durable => T::API_PATHS.plural.to_string(),
            Self::Observed => format!("observed/{}", T::API_PATHS.plural),
        }
    }

    fn health_kind<T: Resource>(self) -> String {
        match self {
            Self::Durable => T::API_PATHS.kind.to_string(),
            Self::Observed => format!("observed/{}", T::API_PATHS.kind),
        }
    }

    fn http(self, path: PathBuf) -> Result<HttpBackend, String> {
        let http = HttpBackend::from_unix_socket(path).map_err(|error| error.to_string())?;
        Ok(match self {
            Self::Durable => http,
            Self::Observed => http.with_path_prefix("observed"),
        })
    }
}

#[derive(Clone, Copy)]
struct RetryBackoff {
    initial: Duration,
    maximum: Duration,
    reset_after: Duration,
}

/// Test harnesses can select authored kinds and drive digest rounds explicitly.
/// Production keeps full-fleet replication and periodic digest scheduling.
#[derive(Clone, Default)]
pub(super) struct ReplicationTestOptions {
    #[cfg(any(test, feature = "test-support"))]
    kinds: Option<&'static [&'static str]>,
    #[cfg(any(test, feature = "test-support"))]
    pub(super) digest_driver: Option<super::test_support::DigestDriver>,
}

impl ReplicationTestOptions {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn new(kinds: Option<&'static [&'static str]>) -> Self {
        Self { kinds, digest_driver: None }
    }

    fn includes<T: Resource>(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if self.kinds.is_some_and(|kinds| !kinds.contains(&T::API_PATHS.kind)) {
            return false;
        }
        true
    }
}

#[derive(Default)]
pub(super) struct PeerReplicatorSupervisors {
    generations: HashMap<NodeId, ActiveGeneration>,
    test_options: ReplicationTestOptions,
}

impl Drop for PeerReplicatorSupervisors {
    fn drop(&mut self) {
        for active in self.generations.values() {
            active.cancellation.cancel();
        }
    }
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
    pub(super) fn new(test_options: ReplicationTestOptions) -> Self {
        Self { generations: HashMap::new(), test_options }
    }

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
            #[cfg(any(test, feature = "test-support"))]
            None => ReplicationTransport::Routed(_router),
            #[cfg(not(any(test, feature = "test-support")))]
            None => {
                debug!(%peer, generation, "peer has no forwarded resource socket; replication waits for an outbound SSH connection");
                ReplicationTransport::Http(socket_path_source)
            }
        };
        flotilla_resources::for_each_registered_resource!(
            spawn_kind,
            &daemon,
            &peer,
            generation,
            &transport,
            &cancellation,
            ReplicationStore::Durable,
            self.test_options.clone()
        );
        spawn_kind::<flotilla_resources::Checkout>(
            &daemon,
            &peer,
            generation,
            &transport,
            &cancellation,
            ReplicationStore::Observed,
            self.test_options.clone(),
        );
        spawn_kind::<flotilla_resources::TerminalSession>(
            &daemon,
            &peer,
            generation,
            &transport,
            &cancellation,
            ReplicationStore::Observed,
            self.test_options.clone(),
        )
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
    #[cfg(any(test, feature = "test-support"))]
    Routed(RemoteCommandRouter),
}

fn spawn_kind<T: Resource>(
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    generation: u64,
    transport: &ReplicationTransport,
    cancellation: &CancellationToken,
    store: ReplicationStore,
    test_options: ReplicationTestOptions,
) {
    if !test_options.includes::<T>() {
        return;
    }
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
                    &store.health_kind::<T>(),
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
                            let http = store.http(path)?;
                            replicate_relay_over_http::<T>(http, &daemon, &peer, store).await
                        }
                    },
                )
                .await;
            });
        }
        #[cfg(any(test, feature = "test-support"))]
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
                    &store.health_kind::<T>(),
                    relay_cancellation,
                    REPLICATION_RETRY,
                    || async { Ok(()) },
                    move |()| {
                        let router = router.clone();
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        async move { replicate_relay_over_routed_watch::<T>(&router, &daemon, &peer, store).await }
                    },
                )
                .await;
            });
        }
    }
    let daemon = Arc::clone(daemon);
    let peer = peer.clone();
    let transport = transport.clone();
    #[cfg(any(test, feature = "test-support"))]
    let digest_control = test_options.digest_driver.map(|driver| driver.control(daemon.node_id(), &peer, &store.kind::<T>()));
    let cancellation = cancellation.clone();
    tokio::spawn(async move {
        match transport {
            ReplicationTransport::Http(socket_path_source) => {
                let run_daemon = Arc::clone(&daemon);
                let run_peer = peer.clone();
                supervise_kind(
                    peer,
                    generation,
                    &store.health_kind::<T>(),
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
                            let http = store.http(path)?;
                            let result = replicate_kind_over_http::<T>(http, &daemon, &peer, store).await;
                            if let Err(error) = &result {
                                daemon.report_resource_replication_failure(&peer, &store.health_kind::<T>(), error).await;
                            }
                            result
                        }
                    },
                )
                .await;
            }
            #[cfg(any(test, feature = "test-support"))]
            ReplicationTransport::Routed(router) => {
                let run_daemon = Arc::clone(&daemon);
                let run_peer = peer.clone();
                supervise_kind(
                    peer,
                    generation,
                    &store.health_kind::<T>(),
                    cancellation,
                    REPLICATION_RETRY,
                    || async { Ok(()) },
                    move |()| {
                        let router = router.clone();
                        let daemon = Arc::clone(&run_daemon);
                        let peer = run_peer.clone();
                        let digest_control = digest_control.clone();
                        async move {
                            let result = replicate_kind_over_routed_watch::<T>(&router, &daemon, &peer, store, digest_control).await;
                            if let Err(error) = &result {
                                daemon.report_resource_replication_failure(&peer, &store.health_kind::<T>(), error).await;
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

async fn replicate_relay_over_http<T: Resource>(
    http: HttpBackend,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    store: ReplicationStore,
) -> Result<(), String> {
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
            store
                .backend(daemon)
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
        store
            .backend(daemon)
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
    kind: &str,
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

enum OriginWatchFailure {
    SnapshotRequired(String),
    Retry(String),
}

impl OriginWatchFailure {
    fn from_resource(error: flotilla_resources::ResourceError) -> Self {
        match &error {
            flotilla_resources::ResourceError::WatchExpired { .. } | flotilla_resources::ResourceError::Invalid { .. } => {
                Self::SnapshotRequired(error.to_string())
            }
            // ResourceError::decode shares Other with transport failures. Keep
            // the HTTP decoder's explicit prefix separate from network errors.
            flotilla_resources::ResourceError::Other { message }
                if message.starts_with("decode watch event:") || message.starts_with("decode tombstone watch event:") =>
            {
                Self::SnapshotRequired(error.to_string())
            }
            _ => Self::Retry(error.to_string()),
        }
    }
}

pub(super) async fn replicate_kind_over_http<T: Resource>(
    http: HttpBackend,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    store: ReplicationStore,
) -> Result<(), String> {
    let remote = ResourceBackend::Http(http).using::<T>(REPLICATION_NAMESPACE);
    let writer = store.backend(daemon).replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    let mut cursor = writer.cursor().await.map_err(|error| error.to_string())?;
    let mut repaired = false;
    loop {
        if cursor.is_none() {
            let listed = remote.list().await.map_err(|error| error.to_string())?;
            store.backend(daemon).including_replicas::<T>(REPLICATION_NAMESPACE).list().await.map_err(|error| error.to_string())?;
            writer.replace(&listed, Utc::now()).await.map_err(|error| error.to_string())?;
            cursor = writer.cursor().await.map_err(|error| error.to_string())?;
        }
        let prefix = cursor.as_ref().ok_or_else(|| "snapshot did not establish replica prefix".to_string())?;
        let start = match &prefix.generation {
            Some(generation) => {
                WatchStart::FromVersionInGeneration { generation: generation.clone(), resource_version: prefix.resource_version.clone() }
            }
            None => WatchStart::FromVersion(prefix.resource_version.clone()),
        };
        let result = match remote.watch(start).await {
            Ok(watch) => {
                daemon.report_resource_replication_healthy(peer, &store.health_kind::<T>()).await;
                apply_http_watch(watch, &writer, prefix.resource_version.clone(), &remote, peer).await
            }
            Err(error) => Err(OriginWatchFailure::from_resource(error)),
        };
        match result {
            Err(OriginWatchFailure::SnapshotRequired(error)) => {
                writer.invalidate_cursor().await.map_err(|error| error.to_string())?;
                if repaired {
                    return Err(error);
                }
                debug!(%peer, kind = T::API_PATHS.kind, %error, "origin log cannot continue; resnapshotting");
                cursor = None;
                repaired = true;
            }
            Err(OriginWatchFailure::Retry(error)) => return Err(error),
            Ok(()) => return Ok(()),
        }
    }
}

async fn apply_http_watch<T: Resource>(
    mut watch: flotilla_resources::WatchStream<T>,
    writer: &flotilla_resources::ReplicaWriter<T>,
    snapshot_version: String,
    remote: &flotilla_resources::TypedResolver<T>,
    peer: &NodeId,
) -> Result<(), OriginWatchFailure> {
    // Relay updates share the writer but cannot move this direct stream's
    // expected sequence. Its position starts at the authoritative snapshot.
    let mut version = Some(snapshot_version);
    let mut digest_failures = DigestFailures::default();
    let mut digest_tick = tokio::time::interval_at(tokio::time::Instant::now() + DIGEST_INTERVAL, DIGEST_INTERVAL);
    digest_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let event = tokio::select! {
            event = watch.next() => match event { Some(event) => event, None => return Ok(()) },
            _ = digest_tick.tick() => {
                let result=reconcile_digest::<T, _, _>(writer, peer, |query| async move { remote.digest(&query).await.map_err(|error| error.to_string()) }).await;
                digest_failures.report::<T>(peer,&result);
                match result {
                    // Reopen from the proven cut, discarding buffered older events.
                    Ok(true)=>return Ok(()),
                    Ok(false)=>{},
                    Err(_)=>{},
                }
                continue;
            }
        };
        let event = event.map_err(OriginWatchFailure::from_resource)?;
        check_sequence(&mut version, &event).map_err(OriginWatchFailure::SnapshotRequired)?;
        writer.apply_direct(event, Utc::now()).await.map_err(OriginWatchFailure::from_resource)?;
    }
}

/// Warn on a sustained failure run, then every third failure; reset on success.
#[derive(Default)]
struct DigestFailures {
    consecutive: u64,
}
impl DigestFailures {
    fn record(&mut self, failed: bool) -> bool {
        if !failed {
            self.consecutive = 0;
            return false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive >= 3 && self.consecutive.is_multiple_of(3)
    }
    fn report<T: Resource>(&mut self, peer: &NodeId, result: &Result<bool, String>) {
        let warn = self.record(result.is_err());
        if let Err(error) = result {
            if warn {
                warn!(%peer,kind=T::API_PATHS.kind,consecutive=self.consecutive,%error,"digest safety net repeatedly failed; preserving replicas");
            } else {
                debug!(%peer,kind=T::API_PATHS.kind,%error,"digest repair deferred; preserving replicas");
            }
        }
    }
}

/// Run beside the direct log consumer, so live events cannot be applied during
/// an authoritative snapshot replacement. Relay sources are never authorities.
async fn reconcile_digest<T: Resource, F, Fut>(
    writer: &flotilla_resources::ReplicaWriter<T>,
    peer: &NodeId,
    mut fetch: F,
) -> Result<bool, String>
where
    F: FnMut(DigestQuery) -> Fut,
    Fut: Future<Output = Result<PartitionDigest, String>>,
{
    let remote = fetch(DigestQuery::Root).await?;
    if remote.origin != *peer || remote.kind != T::API_PATHS.kind || remote.namespace != REPLICATION_NAMESPACE {
        return Err("digest authority identity mismatch".into());
    }
    let previous = writer.cursor().await.map_err(|error| error.to_string())?.ok_or("digest needs an established replica prefix")?;
    let generation = previous.generation.clone();
    // A generation change is handled by the log's generation check on reconnect;
    // do not compare an old-generation replica as if it were current.
    if generation != remote.generation {
        return Err("digest generation changed; log reconnect required".into());
    }
    let local = writer.digest(generation).await.map_err(|error| error.to_string())?;
    if local.root == remote.root {
        return Ok(false);
    }
    let children = fetch(DigestQuery::Children { expected_root: remote.root.clone() }).await?;
    if children.root != remote.root
        || children.origin != remote.origin
        || children.generation != remote.generation
        || children.kind != remote.kind
        || children.namespace != remote.namespace
    {
        return Err("digest tree changed during drill-down".into());
    }
    children.validate_tree::<T>().map_err(|error| error.to_string())?;
    let remote_hashes =
        children.children.as_ref().filter(|hashes| hashes.len() == flotilla_resources::DIGEST_FANOUT).ok_or("incomplete digest tree")?;
    let local_hashes = local.children.as_ref().ok_or("incomplete replica digest tree")?;
    // Fetch and validate every differing bucket before replacing any. Partial
    // availability or concurrent authoritative writes cannot imply deletion.
    let mut snapshots = Vec::new();
    for (bucket, (authority, replica)) in remote_hashes.iter().zip(local_hashes).enumerate() {
        if authority == replica {
            continue;
        }
        let bucket = u8::try_from(bucket).expect("validated 256-way digest tree");
        let snapshot = fetch(DigestQuery::Snapshot { expected_root: remote.root.clone(), bucket }).await?;
        let listed = snapshot.snapshot::<T>(&children, bucket).map_err(|error| error.to_string())?;
        snapshots.push((bucket, listed));
    }
    // Confirm the cut is still current before acting on absence. A write during
    // the last bucket read defers the whole repair to the next interval.
    let confirmed = fetch(DigestQuery::Children { expected_root: remote.root.clone() }).await?;
    if confirmed.root != remote.root || confirmed.generation != remote.generation {
        return Err("digest snapshot cut changed".into());
    }
    let synced_at = Utc::now();
    for (bucket, listed) in snapshots {
        writer.replace_bucket(bucket, &listed, synced_at).await.map_err(|error| error.to_string())?;
    }
    writer.confirm_digest_position(&remote, &previous).await.map_err(|error| error.to_string())?;
    Ok(true)
}

#[cfg(any(test, feature = "test-support"))]
async fn fetch_routed_digest<T: Resource>(
    router: &RemoteCommandRouter,
    peer: &NodeId,
    store: ReplicationStore,
    query: DigestQuery,
) -> Result<PartitionDigest, String> {
    let result = router
        .dispatch_query(
            Command::builder()
                .node_id(peer.clone())
                .action(CommandAction::QueryResourceDigest { namespace: REPLICATION_NAMESPACE.into(), kind: store.kind::<T>(), query })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await?;
    match result {
        CommandValue::ResourceDigest(value) => Ok((*value).into()),
        CommandValue::Error { message } => Err(message),
        other => Err(format!("unexpected digest response: {other:?}")),
    }
}

// The origin's per-kind sequence includes deletes. Never advance past a hole,
// including a historical event skipped during schema-decode quarantine.
// A dropped final event is repaired by the periodic digest safety net.
// These watches are unfiltered. Both authoritative stores assign numeric, dense
// versions independently per (group, version, kind, namespace), not Kubernetes'
// opaque versions. Relay writes must not alter this direct stream's position.
fn check_sequence<T: Resource>(version: &mut Option<String>, event: &WatchEvent<T>) -> Result<(), String> {
    let next = match event {
        WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => &object.metadata.resource_version,
        WatchEvent::DeletedByName(tombstone) => &tombstone.resource_version,
    };
    if let Some(previous) = version {
        let previous = previous.parse::<u64>().map_err(|error| error.to_string())?;
        let next_number = next.parse::<u64>().map_err(|error| error.to_string())?;
        if previous.checked_add(1) != Some(next_number) {
            return Err(format!("resourceVersion sequence gap: after {previous}, received {next_number}; resnapshot required"));
        }
    }
    *version = Some(next.clone());
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
async fn replicate_kind_over_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    store: ReplicationStore,
    digest_control: Option<Arc<super::test_support::DigestControl>>,
) -> Result<(), String> {
    store.backend(daemon).including_replicas::<T>(REPLICATION_NAMESPACE).list().await.map_err(|error| error.to_string())?;
    let writer = store.backend(daemon).replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    let mut cursor = writer.cursor().await.map_err(|error| error.to_string())?;
    let mut repaired = false;
    loop {
        match run_routed_watch::<T>(router, daemon, peer, store, cursor, digest_control.clone()).await {
            Err(error)
                if flotilla_resources::ResourceError::is_invalid_message(&error)
                    || error.contains("sequence gap")
                    || error.contains("watch resourceVersion")
                    || error.starts_with("replica event decode gap:") =>
            {
                writer.invalidate_cursor().await.map_err(|error| error.to_string())?;
                if repaired {
                    return Err(error);
                }
                debug!(%peer, kind = T::API_PATHS.kind, %error, "origin log cannot continue; resnapshotting");
                cursor = None;
                repaired = true;
            }
            result => return result,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
async fn run_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    store: ReplicationStore,
    prefix: Option<flotilla_resources::ReplicaCursor>,
    digest_control: Option<Arc<super::test_support::DigestControl>>,
) -> Result<(), String> {
    if let Some(control) = &digest_control {
        control.ready.send_replace(false);
    }
    let mut events = daemon.subscribe();
    let command_id = router
        .dispatch_execute_for_principal(
            Command {
                node_id: Some(peer.clone()),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceWatch {
                    namespace: REPLICATION_NAMESPACE.to_string(),
                    kind: store.kind::<T>(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: prefix.as_ref().map(|prefix| {
                        flotilla_protocol::ResourceCursor::from_position(prefix.resource_version.clone(), prefix.generation.clone())
                    }),
                },
            },
            None,
        )
        .await?;
    daemon.report_resource_replication_healthy(peer, &store.health_kind::<T>()).await;
    let writer = store.backend(daemon).replica_writer::<T>(peer.clone(), REPLICATION_NAMESPACE);
    let mut initial = Vec::<ResourceObject<T>>::new();
    let mut initializing = prefix.is_none();

    // One primary watch owns this key's receiver for its lifetime. A replacement
    // watch waits for that owner to exit; queued rounds survive unrelated errors.
    let mut requests = match &digest_control {
        Some(control) => Some(control.requests.lock().await),
        None => None,
    };
    let mut version = prefix.map(|prefix| prefix.resource_version);
    let mut digest_failures = DigestFailures::default();
    let mut digest_tick = tokio::time::interval_at(tokio::time::Instant::now() + DIGEST_INTERVAL, DIGEST_INTERVAL);
    digest_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let completion;
        let received = tokio::select! {
            event = events.recv() => { completion = None; Some(event) },
            request = async {
                match &mut requests {
                    Some(requests) => requests.recv().await,
                    None => std::future::pending().await,
                }
            }, if !initializing => { completion = request; None },
            _ = digest_tick.tick(), if !initializing && digest_control.is_none() => { completion = None; None },
        };
        let Some(received) = received else {
            let result = reconcile_digest::<T, _, _>(&writer, peer, |query| fetch_routed_digest::<T>(router, peer, store, query)).await;
            digest_failures.report::<T>(peer, &result);
            if matches!(result, Ok(true)) {
                // Clear readiness before acknowledging repair so callers await
                // the replacement watch's bookmark rather than the old stream.
                if let Some(control) = &digest_control {
                    control.ready.send_replace(false);
                }
                let _ = router.dispatch_cancel(command_id).await;
            }
            if let Some(completion) = completion {
                let _ = completion.send(result.clone());
            }
            if matches!(result, Ok(true)) {
                return Ok(());
            }
            continue;
        };
        match received {
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
                let snapshot_version = response.cursor.position()?.0;
                if !initializing {
                    for record in &response.records {
                        let event = match record_watch_event::<T>(record.clone()) {
                            Ok(event) => event,
                            Err(error) => {
                                let _ = router.dispatch_cancel(command_id).await;
                                return Err(format!("replica event decode gap: {error}"));
                            }
                        };
                        if let Some(event) = event {
                            if let Err(error) = check_sequence(&mut version, &event) {
                                let _ = router.dispatch_cancel(command_id).await;
                                return Err(error);
                            }
                        }
                    }
                }
                let bookmark = response.records.iter().any(|record| record.record_type == ResourceRecordType::Bookmark);
                let was_initializing = initializing;
                apply_response(&writer, &mut initial, &mut initializing, *response).await?;
                if bookmark {
                    if let Some(control) = &digest_control {
                        control.ready.send_replace(true);
                    }
                }
                if was_initializing && !initializing {
                    // The snapshot's final bookmark is the position preceding
                    // the first live event, so that first event is checked too.
                    version = Some(snapshot_version);
                }
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
                warn!(%peer, kind = T::API_PATHS.kind, skipped, "resource replicator lagged; reconnect will resnapshot");
                let _ = router.dispatch_cancel(command_id).await;
                return Err("resourceVersion sequence gap: event subscriber lagged".to_string());
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("daemon event stream closed".to_string()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
async fn replicate_relay_over_routed_watch<T: Resource>(
    router: &RemoteCommandRouter,
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    store: ReplicationStore,
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
                    kind: store.kind::<T>(),
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
                    apply_relay_response::<T>(daemon, peer, *response, store).await?;
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

#[cfg(any(test, feature = "test-support"))]
async fn apply_relay_response<T: Resource>(
    daemon: &Arc<InProcessDaemon>,
    peer: &NodeId,
    response: ResourceReadEnvelope,
    store: ReplicationStore,
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
            store
                .backend(daemon)
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
        store
            .backend(daemon)
            .replica_writer::<T>(origin, REPLICATION_NAMESPACE)
            .apply(event, synced_at)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
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
            writer.apply_direct(event, Utc::now()).await.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
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
    #[test]
    fn digest_failures_warn_after_three_and_reset_after_success() {
        let mut failures = super::DigestFailures::default();
        for run in 0..3 {
            for count in 1u64..=9 {
                assert_eq!(failures.record(true), count.is_multiple_of(3), "run {run}, failure {count}");
            }
            assert!(!failures.record(false));
            assert_eq!(failures.consecutive, 0);
        }
    }

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
    fn dropping_supervisors_cancels_all_peer_generations() {
        // Runtime teardown ends every replication generation, including watches
        // and retry backoffs, rather than leaving them to work on dead sessions.
        let mut supervisors = PeerReplicatorSupervisors::default();
        let (first, _) = supervisors.begin_generation(&NodeId::new("first"), 1, None).expect("first generation");
        let (second, _) = supervisors.begin_generation(&NodeId::new("second"), 2, None).expect("second generation");
        drop(supervisors);
        assert!(first.is_cancelled(), "first peer generation survived teardown");
        assert!(second.is_cancelled(), "second peer generation survived teardown");
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

#[cfg(test)]
mod relay_tests;

#[cfg(test)]
mod digest_tests {
    use flotilla_resources::{Convoy, ConvoySpec, InMemoryBackend, InputMeta};

    use super::*;

    // A write during drill-down invalidates its cut before any bucket is
    // replaced, even when an earlier bucket snapshot was already fetched.
    #[tokio::test]
    async fn concurrent_authority_write_defers_all_bucket_repairs() {
        let peer = NodeId::new("authority");
        let authority =
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(peer.clone()).using::<Convoy>(REPLICATION_NAMESPACE);
        let holder = ResourceBackend::InMemory(InMemoryBackend::default());
        let writer = holder.replica_writer::<Convoy>(peer.clone(), REPLICATION_NAMESPACE);
        let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
        for name in ["gone", "retained"] {
            authority.create(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("create");
        }
        writer.replace(&authority.list().await.expect("seed"), Utc::now()).await.expect("snapshot");
        authority.delete("gone").await.expect("delete");
        let current = authority.get("retained").await.expect("retained");
        authority
            .update(
                &InputMeta::builder().name("retained".into()).build(),
                &current.metadata.resource_version,
                &ConvoySpec::builder().workflow_ref("new".into()).build(),
            )
            .await
            .expect("modify");
        let mut snapshots = 0;
        let result = reconcile_digest::<Convoy, _, _>(&writer, &peer, |query| {
            let should_write = if matches!(query, DigestQuery::Snapshot { .. }) {
                snapshots += 1;
                snapshots == 2
            } else {
                false
            };
            let authority = authority.clone();
            let spec = spec.clone();
            async move {
                if should_write {
                    authority.create(&InputMeta::builder().name("concurrent".into()).build(), &spec).await.expect("concurrent write");
                }
                authority.digest(&query).await.map_err(|error| error.to_string())
            }
        })
        .await;
        assert!(result.is_err(), "a concurrent write must refuse the old snapshot cut");
        let before = holder.including_replicas::<Convoy>(REPLICATION_NAMESPACE).list().await.expect("replicas");
        assert_eq!(before.items.len(), 2, "no earlier bucket was applied after the later read failed");
        assert!(before.items.iter().any(|item| item.object.metadata.name == "gone"));
        assert_eq!(
            before.items.iter().find(|item| item.object.metadata.name == "retained").expect("retained").object.spec.workflow_ref,
            "workflow"
        );
        reconcile_digest::<Convoy, _, _>(&writer, &peer, |query| {
            let authority = authority.clone();
            async move { authority.digest(&query).await.map_err(|error| error.to_string()) }
        })
        .await
        .expect("stable retry");
        assert_eq!(
            writer.digest(None).await.expect("holder root").root,
            authority.digest(&DigestQuery::Root).await.expect("authority root").root
        );
    }
}

use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, NodeId, ResourceWatchCursor, ResourceWatchResponse};
use flotilla_resources::{
    HttpBackend, K8sWatchEvent, ReplicationClass, Resource, ResourceBackend, ResourceError, ResourceList, ResourceObject, WatchStart,
};
use futures::StreamExt;
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
}

impl PeerReplicatorSupervisors {
    pub(super) fn peer_connected(
        &mut self,
        router: RemoteCommandRouter,
        daemon: Arc<InProcessDaemon>,
        peer: NodeId,
        generation: u64,
        resource_socket_path: Option<PathBuf>,
    ) {
        let Some(cancellation) = self.begin_generation(&peer, generation) else {
            return;
        };
        let transport = match resource_socket_path {
            Some(path) => ReplicationTransport::Http(path),
            #[cfg(feature = "test-support")]
            None => ReplicationTransport::Routed(router),
            #[cfg(not(feature = "test-support"))]
            None => {
                debug!(%peer, generation, "peer has no forwarded resource socket; replication waits for an outbound SSH connection");
                return;
            }
        };
        flotilla_resources::for_each_registered_resource!(spawn_kind, &daemon, &peer, generation, &transport, &cancellation)
    }

    fn begin_generation(&mut self, peer: &NodeId, generation: u64) -> Option<CancellationToken> {
        if let Some(active) = self.generations.get(peer) {
            if generation <= active.generation {
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
        self.generations.insert(peer.clone(), ActiveGeneration { generation, cancellation: cancellation.clone() });
        Some(cancellation)
    }
}

#[derive(Clone)]
enum ReplicationTransport {
    Http(PathBuf),
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
    if T::REPLICATION_CLASS != ReplicationClass::HomeBoundRuntime {
        return;
    }
    let daemon = Arc::clone(daemon);
    let peer = peer.clone();
    let transport = transport.clone();
    let cancellation = cancellation.clone();
    tokio::spawn(async move {
        supervise_kind(peer.clone(), generation, T::API_PATHS.kind, cancellation, REPLICATION_RETRY, || {
            let transport = transport.clone();
            let daemon = Arc::clone(&daemon);
            let peer = peer.clone();
            async move {
                match transport {
                    ReplicationTransport::Http(path) => {
                        let http = HttpBackend::from_unix_socket(path).map_err(|error| error.to_string())?;
                        replicate_kind_over_http::<T>(http, &daemon, &peer).await
                    }
                    #[cfg(feature = "test-support")]
                    ReplicationTransport::Routed(router) => replicate_kind_over_routed_watch::<T>(&router, &daemon, &peer).await,
                }
            }
        })
        .await;
    });
}

async fn supervise_kind<F, Fut>(
    peer: NodeId,
    generation: u64,
    kind: &'static str,
    cancellation: CancellationToken,
    retry: RetryBackoff,
    mut run: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut backoff = retry.initial;
    loop {
        let started_at = tokio::time::Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = run() => result,
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
    let cursor = writer.cursor().await.map_err(|error| error.to_string())?;
    if let Some(cursor) = cursor {
        let start = match cursor.generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version: cursor.resource_version },
            None => WatchStart::FromVersion(cursor.resource_version),
        };
        match remote.watch(start).await {
            Ok(watch) => return apply_http_watch(watch, &writer).await,
            Err(error @ (ResourceError::WatchExpired { .. } | ResourceError::Invalid { .. })) => {
                debug!(%peer, kind = T::API_PATHS.kind, %error, "replica cursor rejected; relisting origin");
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    let listed = remote.list().await.map_err(|error| error.to_string())?;
    let start = WatchStart::resuming_from(&listed);
    writer.replace(&listed, Utc::now()).await.map_err(|error| error.to_string())?;
    let watch = remote.watch(start).await.map_err(|error| error.to_string())?;
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
    let protocol_cursor =
        cursor.map(|cursor| ResourceWatchCursor { resource_version: cursor.resource_version, generation: cursor.generation });
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
                    include_replicas: false,
                    cursor: protocol_cursor,
                },
            },
            None,
        )
        .await?;
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
                if response.kind != T::API_PATHS.kind {
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
async fn apply_response<T: Resource>(
    writer: &flotilla_resources::ReplicaWriter<T>,
    initial: &mut Vec<ResourceObject<T>>,
    initializing: &mut bool,
    response: ResourceWatchResponse,
) -> Result<(), String> {
    if response.event["type"] == "BOOKMARK" {
        if *initializing {
            writer
                .replace(
                    &ResourceList {
                        items: std::mem::take(initial),
                        resource_version: response.resource_version,
                        generation: response.generation,
                    },
                    Utc::now(),
                )
                .await
                .map_err(|error| error.to_string())?;
            *initializing = false;
        }
        return Ok(());
    }

    let event: K8sWatchEvent<T> =
        serde_json::from_value(response.event).map_err(|error| format!("decode replicated {} event: {error}", T::API_PATHS.kind))?;
    let event = event.into_watch_event().map_err(|error| error.to_string())?;
    if *initializing && matches!(event, flotilla_resources::WatchEvent::Added(_)) {
        let object = match event {
            flotilla_resources::WatchEvent::Added(object) => object,
            _ => unreachable!("matched added event"),
        };
        initial.push(object);
        return Ok(());
    }
    writer.apply(event, Utc::now()).await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use flotilla_resources::Convoy;
    use serde_json::json;

    use super::*;

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
            {
                let attempts = Arc::clone(&attempts);
                move || {
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
        let old_cancellation = supervisors.begin_generation(&peer, 7).expect("start old generation");
        let attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(supervise_kind(
            peer.clone(),
            7,
            Convoy::API_PATHS.kind,
            old_cancellation,
            RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(4), reset_after: Duration::from_secs(60) },
            {
                let attempts = Arc::clone(&attempts);
                move || {
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
        supervisors.begin_generation(&peer, 8).expect("start new generation");
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
            {
                let attempts = Arc::clone(&attempts);
                move || {
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

        for generation in [4, 4, 3] {
            if supervisors.begin_generation(&peer, generation).is_some() {
                applications += 1;
            }
        }
        assert_eq!(applications, 1, "one generation may apply only once despite duplicate or stale notices");

        if supervisors.begin_generation(&peer, 5).is_some() {
            applications += 1;
        }
        assert_eq!(applications, 2, "a newer generation starts exactly one new application stream");
    }
}

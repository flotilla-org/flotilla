use std::{path::PathBuf, sync::Arc};

use chrono::Utc;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, NodeId, ResourceWatchCursor, ResourceWatchResponse};
use flotilla_resources::{
    HttpBackend, K8sWatchEvent, ReplicationClass, Resource, ResourceBackend, ResourceError, ResourceList, ResourceObject, WatchStart,
};
use futures::StreamExt;
use tracing::{debug, warn};

use super::remote_commands::RemoteCommandRouter;

const REPLICATION_NAMESPACE: &str = "flotilla";

pub(super) fn spawn_peer_replicators(
    router: RemoteCommandRouter,
    daemon: Arc<InProcessDaemon>,
    peer: NodeId,
    resource_socket_path: Option<PathBuf>,
) {
    let http = resource_socket_path.map(HttpBackend::from_unix_socket).transpose().map_err(|error| error.to_string());
    match http {
        Ok(http) => flotilla_resources::for_each_registered_resource!(spawn_kind, &router, &daemon, &peer, &http),
        Err(error) => warn!(%peer, %error, "could not start peer resource replicators"),
    }
}

fn spawn_kind<T: Resource>(router: &RemoteCommandRouter, daemon: &Arc<InProcessDaemon>, peer: &NodeId, http: &Option<HttpBackend>) {
    if T::REPLICATION_CLASS != ReplicationClass::HomeBoundRuntime {
        return;
    }
    let router = router.clone();
    let daemon = Arc::clone(daemon);
    let peer = peer.clone();
    let http = http.clone();
    tokio::spawn(async move {
        let result = match http {
            Some(http) => replicate_kind_over_http::<T>(http, &daemon, &peer).await,
            None => replicate_kind_over_routed_watch::<T>(&router, &daemon, &peer).await,
        };
        if let Err(error) = result {
            debug!(%peer, kind = T::API_PATHS.kind, %error, "resource replicator stopped");
        }
    });
}

async fn replicate_kind_over_http<T: Resource>(http: HttpBackend, daemon: &Arc<InProcessDaemon>, peer: &NodeId) -> Result<(), String> {
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

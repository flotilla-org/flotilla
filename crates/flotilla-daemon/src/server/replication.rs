//! Daemon implementations of the `flotilla-replication` host ports.
//!
//! The engine lives in `flotilla-replication`. The daemon supplies its local
//! stores, node identity and health from `InProcessDaemon`, opens direct peers
//! as resource HTTP stores at forwarded sockets, and carries routed watches
//! over the remote command router and the daemon event stream.

use std::{path::Path, sync::Arc};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, NodeId};
use flotilla_replication::{
    DirectPeerConnector, LocalStores, PeerSources, ReplicationHealth, ReplicationHost, ReplicationStore, RoutedDigestRequest, RoutedPeer,
    RoutedWatch, RoutedWatchItem, RoutedWatchRequest,
};
use flotilla_resources::PartitionDigest;
use flotilla_store::ResourceBackend;
use flotilla_store_http::HttpBackend;
use tokio::sync::broadcast;
use tracing::debug;

use super::remote_commands::RemoteCommandRouter;

/// The daemon's local replication side: its stores, identity and health.
pub(super) fn replication_host(daemon: &Arc<InProcessDaemon>) -> ReplicationHost {
    ReplicationHost::new(
        daemon.node_id().clone(),
        LocalStores { durable: daemon.resource_backend(), observed: daemon.observed_resource_backend() },
        Arc::new(DaemonReplicationHealth(Arc::clone(daemon))),
    )
}

/// Direct peers over forwarded resource sockets; routed peers over the router.
pub(super) fn peer_sources(daemon: Arc<InProcessDaemon>, router: RemoteCommandRouter) -> PeerSources {
    PeerSources { direct: Arc::new(ResourceSocketConnector), routed: Arc::new(RoutedResourcePeer { daemon, router }) }
}

struct DaemonReplicationHealth(Arc<InProcessDaemon>);

#[async_trait::async_trait]
impl ReplicationHealth for DaemonReplicationHealth {
    async fn begin(&self, peer: &NodeId) {
        self.0.begin_peer_resource_replication(peer).await;
    }

    async fn healthy(&self, peer: &NodeId, kind: &str) {
        self.0.report_resource_replication_healthy(peer, kind).await;
    }

    async fn failure(&self, peer: &NodeId, kind: &str, message: &str) {
        self.0.report_resource_replication_failure(peer, kind, message).await;
    }
}

/// Opens a peer's resource HTTP API at a forwarded Unix socket. The observed
/// read view is served under the `observed` path prefix.
struct ResourceSocketConnector;

impl DirectPeerConnector for ResourceSocketConnector {
    fn connect(&self, socket: &Path, store: ReplicationStore) -> Result<ResourceBackend, String> {
        let http = HttpBackend::from_unix_socket(socket).map_err(|error| error.to_string())?;
        Ok(ResourceBackend::new(match store {
            ReplicationStore::Durable => http,
            ReplicationStore::Observed => http.with_path_prefix("observed"),
        }))
    }
}

/// Routed resource watches: remote `ResourceWatch` commands whose produced
/// values arrive on the daemon event stream.
struct RoutedResourcePeer {
    daemon: Arc<InProcessDaemon>,
    router: RemoteCommandRouter,
}

#[async_trait::async_trait]
impl RoutedPeer for RoutedResourcePeer {
    async fn watch(&self, peer: &NodeId, request: RoutedWatchRequest) -> Result<Box<dyn RoutedWatch>, String> {
        // Subscribe before dispatch so the watch's first responses are seen.
        let events = self.daemon.subscribe();
        let command_id = self
            .router
            .dispatch_execute_for_principal(
                Command {
                    node_id: Some(peer.clone()),
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ResourceWatch {
                        namespace: request.namespace,
                        kind: request.kind,
                        name: None,
                        include_replicas: false,
                        replica_sources: request.replica_sources,
                        cursor: request
                            .cursor
                            .map(|prefix| flotilla_protocol::ResourceCursor::from_position(prefix.resource_version, prefix.generation)),
                    },
                },
                None,
            )
            .await?;
        Ok(Box::new(RoutedResourceWatch { events, command_id, router: self.router.clone() }))
    }

    async fn digest(&self, peer: &NodeId, request: RoutedDigestRequest) -> Result<PartitionDigest, String> {
        let result = self
            .router
            .dispatch_query(
                Command::builder()
                    .node_id(peer.clone())
                    .action(CommandAction::QueryResourceDigest { namespace: request.namespace, kind: request.kind, query: request.query })
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
}

struct RoutedResourceWatch {
    events: broadcast::Receiver<DaemonEvent>,
    command_id: u64,
    router: RemoteCommandRouter,
}

#[async_trait::async_trait]
impl RoutedWatch for RoutedResourceWatch {
    // Cancel-safe: the only await is the broadcast receive, and unrelated
    // events are discarded whether or not the future is dropped.
    async fn next(&mut self) -> RoutedWatchItem {
        loop {
            match self.events.recv().await {
                Ok(DaemonEvent::CommandStepUpdate {
                    command_id: event_command_id,
                    status: flotilla_protocol::StepStatus::Produced { value },
                    ..
                }) if event_command_id == self.command_id => {
                    let CommandValue::ResourceWatchEvent(response) = *value else {
                        continue;
                    };
                    return RoutedWatchItem::Response(response);
                }
                Ok(DaemonEvent::CommandFinished { command_id: event_command_id, result, .. }) if event_command_id == self.command_id => {
                    return RoutedWatchItem::Finished(Box::new(result));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => return RoutedWatchItem::Lagged(skipped),
                Err(broadcast::error::RecvError::Closed) => return RoutedWatchItem::Closed,
            }
        }
    }

    async fn cancel(&self) {
        // A failed cancel can leave the remote watch running until its peer drops.
        if let Err(error) = self.router.dispatch_cancel(self.command_id).await {
            debug!(command_id = self.command_id, %error, "routed replication watch cancel failed");
        }
    }
}

#[cfg(test)]
mod relay_tests;

use std::sync::Arc;

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, NodeId, ResourceCursor, ResourceReadEnvelope, StepStatus};
use tokio::sync::broadcast;

#[derive(Debug, Clone, bon::Builder)]
pub struct ResourceListRequest {
    pub kind: String,
    #[builder(default = "flotilla".to_string())]
    pub namespace: String,
    pub node_id: Option<NodeId>,
    #[builder(default)]
    pub include_replicas: bool,
}

#[derive(Debug, Clone, bon::Builder)]
pub struct ResourceGetRequest {
    pub kind: String,
    pub name: String,
    #[builder(default = "flotilla".to_string())]
    pub namespace: String,
    pub node_id: Option<NodeId>,
}

#[derive(Debug, Clone, bon::Builder)]
pub struct ResourceWatchRequest {
    pub kind: String,
    #[builder(default = "flotilla".to_string())]
    pub namespace: String,
    pub name: Option<String>,
    pub node_id: Option<NodeId>,
    #[builder(default)]
    pub include_replicas: bool,
    pub cursor: Option<ResourceCursor>,
}

#[derive(Clone)]
pub struct ResourceClient {
    daemon: Arc<dyn DaemonHandle>,
}

impl ResourceClient {
    pub fn new(daemon: Arc<dyn DaemonHandle>) -> Self {
        Self { daemon }
    }

    pub async fn list(&self, request: ResourceListRequest) -> Result<ResourceReadEnvelope, String> {
        let result = self
            .daemon
            .execute_query(
                Command {
                    node_id: request.node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryResourceList {
                        namespace: request.namespace,
                        kind: request.kind,
                        include_replicas: request.include_replicas,
                    },
                },
                uuid::Uuid::new_v4(),
            )
            .await?;
        resource_read_result(result)
    }

    pub async fn get(&self, request: ResourceGetRequest) -> Result<ResourceReadEnvelope, String> {
        let result = self
            .daemon
            .execute_query(
                Command {
                    node_id: request.node_id,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryResourceGet { namespace: request.namespace, kind: request.kind, name: request.name },
                },
                uuid::Uuid::new_v4(),
            )
            .await?;
        resource_read_result(result)
    }

    pub async fn watch(&self, request: ResourceWatchRequest) -> Result<ResourceWatch, String> {
        let events = self.daemon.subscribe();
        let command_id = self
            .daemon
            .execute(Command {
                node_id: request.node_id,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceWatch {
                    namespace: request.namespace,
                    kind: request.kind,
                    name: request.name,
                    include_replicas: request.include_replicas,
                    replica_sources: false,
                    cursor: request.cursor,
                },
            })
            .await?;
        Ok(ResourceWatch { daemon: Arc::clone(&self.daemon), events, command_id, finished: false })
    }
}

fn resource_read_result(result: CommandValue) -> Result<ResourceReadEnvelope, String> {
    match result {
        CommandValue::ResourceRead(envelope) => Ok(*envelope),
        CommandValue::Error { message } => Err(message),
        other => Err(format!("unexpected resource read response: {other:?}")),
    }
}

pub struct ResourceWatch {
    daemon: Arc<dyn DaemonHandle>,
    events: broadcast::Receiver<DaemonEvent>,
    command_id: u64,
    finished: bool,
}

impl ResourceWatch {
    pub async fn next(&mut self) -> Result<Option<ResourceReadEnvelope>, String> {
        if self.finished {
            return Ok(None);
        }
        loop {
            match self.events.recv().await {
                Ok(DaemonEvent::CommandStepUpdate { command_id, status: StepStatus::Produced { value }, .. })
                    if command_id == self.command_id =>
                {
                    if let CommandValue::ResourceWatchEvent(envelope) = *value {
                        return Ok(Some(*envelope));
                    }
                }
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == self.command_id => {
                    self.finished = true;
                    return match result {
                        CommandValue::Ok | CommandValue::Cancelled => Ok(None),
                        CommandValue::Error { message } => Err(message),
                        other => Err(format!("unexpected resource watch result: {other:?}")),
                    };
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(format!("resource watch fell behind by {skipped} events; resume from the last emitted cursor"));
                }
                Err(broadcast::error::RecvError::Closed) => return Err("daemon disconnected during resource watch".to_string()),
            }
        }
    }

    pub async fn cancel(mut self) -> Result<(), String> {
        if !self.finished {
            self.daemon.cancel(self.command_id).await?;
            self.finished = true;
        }
        Ok(())
    }
}

impl Drop for ResourceWatch {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let daemon = Arc::clone(&self.daemon);
        let command_id = self.command_id;
        runtime.spawn(async move {
            let _ = daemon.cancel(command_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{Message, RepoIdentity, Request, ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType, Response};
    use flotilla_transport::message::message_session_pair;

    use super::*;
    use crate::SocketDaemon;

    fn envelope(record_type: ResourceRecordType, version: &str) -> ResourceReadEnvelope {
        ResourceReadEnvelope {
            api_version: "flotilla.work/v1".to_string(),
            resource_kind: "Convoy".to_string(),
            plural: "convoys".to_string(),
            namespace: "flotilla".to_string(),
            cursor: ResourceCursor::from_position(version, None),
            records: vec![ResourceReadRecord {
                record_type,
                provenance: ResourceRecordProvenance::Local { node_id: NodeId::new("feta") },
                object: Some(serde_json::json!({"metadata": {"name": "demo", "resourceVersion": version}})),
            }],
        }
    }

    #[tokio::test]
    async fn client_lists_and_watches_through_the_shared_resource_surface() {
        let (client_session, server_session) = message_session_pair();
        let daemon = SocketDaemon::from_session(client_session).expect("create socket daemon");
        let client = ResourceClient::new(daemon);
        let server = tokio::spawn(async move {
            let Some(Message::Request { id, request: Request::Execute { command } }) =
                server_session.read().await.expect("read list request")
            else {
                panic!("expected list request");
            };
            assert!(matches!(command.action, CommandAction::QueryResourceList { ref kind, .. } if kind == "convoys"));
            server_session
                .write(Message::ok_response(id, Response::QueryResult {
                    command_id: 1,
                    value: CommandValue::ResourceRead(Box::new(envelope(ResourceRecordType::Current, "1"))),
                }))
                .await
                .expect("write list response");

            let Some(Message::Request { id, request: Request::Execute { command } }) =
                server_session.read().await.expect("read watch request")
            else {
                panic!("expected watch request");
            };
            assert!(matches!(
                command.action,
                CommandAction::ResourceWatch { ref name, ref cursor, .. }
                    if name.as_deref() == Some("demo") && cursor.as_ref().is_some_and(|cursor| cursor.position().is_ok())
            ));
            server_session.write(Message::ok_response(id, Response::Execute { command_id: 2 })).await.expect("write watch start");
            server_session
                .write(Message::Event {
                    event: Box::new(DaemonEvent::CommandStepUpdate {
                        command_id: 2,
                        node_id: NodeId::new("feta"),
                        repo_identity: RepoIdentity { authority: "local".to_string(), path: "resource".to_string() },
                        repo: None,
                        step_index: 0,
                        step_count: 1,
                        description: "modified Convoy".to_string(),
                        status: StepStatus::Produced {
                            value: Box::new(CommandValue::ResourceWatchEvent(Box::new(envelope(ResourceRecordType::Modified, "2")))),
                        },
                    }),
                })
                .await
                .expect("write watch event");
            server_session
                .write(Message::Event {
                    event: Box::new(DaemonEvent::CommandFinished {
                        command_id: 2,
                        node_id: NodeId::new("feta"),
                        repo_identity: RepoIdentity { authority: "local".to_string(), path: "resource".to_string() },
                        repo: None,
                        result: CommandValue::Ok,
                    }),
                })
                .await
                .expect("write watch finish");
        });

        let listed = client.list(ResourceListRequest::builder().kind("convoys".to_string()).build()).await.expect("list resources");
        assert_eq!(listed.records[0].record_type, ResourceRecordType::Current);

        let mut watch = client
            .watch(ResourceWatchRequest::builder().kind("convoys".to_string()).name("demo".to_string()).cursor(listed.cursor).build())
            .await
            .expect("watch resources");
        assert_eq!(
            watch.next().await.expect("next watch record").expect("watch envelope").records[0].record_type,
            ResourceRecordType::Modified
        );
        assert!(watch.next().await.expect("watch end").is_none());
        server.await.expect("server task");
    }
}

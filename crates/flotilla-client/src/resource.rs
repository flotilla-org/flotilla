use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    Command, CommandAction, CommandValue, DaemonEvent, HostListEntry, NodeId, PeerConnectionState, ResourceCursor, ResourceReadEnvelope,
    StepStatus,
};
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

const SINGLE_HOME_SWEEP_KINDS: &[&str] = &["hosts", "placementpolicies"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupSweepDeletion {
    pub kind: String,
    pub name: String,
    pub deleted_root: String,
    pub home_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupSweepReport {
    pub inspected_roots: usize,
    pub duplicate_records: usize,
    pub deletions: Vec<DedupSweepDeletion>,
}

#[derive(Debug, Clone)]
struct SweepRoot {
    host_id: String,
    node_id: NodeId,
}

#[derive(Debug, Clone)]
struct AuthoredRecord {
    root: SweepRoot,
    namespace: String,
    kind: String,
    name: String,
    natural_home: Option<String>,
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

    /// Remove standing multi-authored Host and PlacementPolicy records from all
    /// non-home roots. This is deliberately an inventory-then-delete operation:
    /// no mutation occurs unless every duplicate agrees on one natural home and
    /// that home actually holds an authored copy.
    pub async fn dedup_single_home_records(&self, namespace: &str) -> Result<DedupSweepReport, String> {
        let roots = self.sweep_roots().await?;
        let mut records = BTreeMap::<(String, String), Vec<AuthoredRecord>>::new();
        for root in &roots {
            for kind in SINGLE_HOME_SWEEP_KINDS {
                let listed = self
                    .list(
                        ResourceListRequest::builder()
                            .kind((*kind).to_string())
                            .namespace(namespace.to_string())
                            .node_id(root.node_id.clone())
                            .include_replicas(false)
                            .build(),
                    )
                    .await
                    .map_err(|error| format!("inventory {kind} on root {}: {error}", root.host_id))?;
                for record in listed.records {
                    let Some(object) = record.object else {
                        continue;
                    };
                    let name = object
                        .pointer("/metadata/name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| format!("inventory {kind} on root {} returned an object without metadata.name", root.host_id))?
                        .to_string();
                    let natural_home = natural_home(kind, &object);
                    records.entry(((*kind).to_string(), name.clone())).or_default().push(AuthoredRecord {
                        root: root.clone(),
                        namespace: namespace.to_string(),
                        kind: (*kind).to_string(),
                        name,
                        natural_home,
                    });
                }
            }
        }

        let mut duplicate_records = 0;
        let mut planned_deletions = Vec::new();
        for ((kind, name), sources) in records {
            if sources.len() < 2 {
                continue;
            }
            duplicate_records += 1;
            let homes = sources.iter().filter_map(|source| source.natural_home.as_deref()).collect::<BTreeSet<_>>();
            if homes.len() != 1 {
                return Err(format!(
                    "refusing to sweep {kind}/{name}: authored copies do not agree on exactly one natural home ({})",
                    homes.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            let home = homes.into_iter().next().expect("one natural home").to_string();
            if !sources.iter().any(|source| source.root.host_id == home) {
                return Err(format!("refusing to sweep {kind}/{name}: natural home {home} has no authored copy"));
            }
            for source in sources.into_iter().filter(|source| source.root.host_id != home) {
                planned_deletions.push((source, home.clone()));
            }
        }

        let mut deletions = Vec::new();
        for (source, home) in planned_deletions {
            let changed = self.delete_authored_record(&source).await?;
            if changed {
                deletions.push(DedupSweepDeletion {
                    kind: source.kind,
                    name: source.name,
                    deleted_root: source.root.host_id,
                    home_root: home,
                });
            }
        }
        deletions.sort_by(|a, b| (&a.kind, &a.name, &a.deleted_root).cmp(&(&b.kind, &b.name, &b.deleted_root)));
        Ok(DedupSweepReport { inspected_roots: roots.len(), duplicate_records, deletions })
    }

    async fn sweep_roots(&self) -> Result<Vec<SweepRoot>, String> {
        let result = self
            .daemon
            .execute_query(
                Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
                uuid::Uuid::new_v4(),
            )
            .await?;
        let CommandValue::HostList(response) = result else {
            return Err(format!("unexpected host inventory response: {result:?}"));
        };
        sweep_roots_from_hosts(&response.hosts)
    }

    async fn delete_authored_record(&self, source: &AuthoredRecord) -> Result<bool, String> {
        let mut events = self.daemon.subscribe();
        let command_id = self
            .daemon
            .execute(Command {
                node_id: Some(source.root.node_id.clone()),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceDelete {
                    namespace: source.namespace.clone(),
                    kind: source.kind.clone(),
                    name: source.name.clone(),
                    replica_origin: None,
                },
            })
            .await?;
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished, result, .. }) if finished == command_id => {
                    return match result {
                        CommandValue::ResourceDeleted(_) => Ok(true),
                        CommandValue::ResourceAlreadyDeleted(_) => Ok(false),
                        CommandValue::Error { message } => {
                            Err(format!("delete {}/{} from root {}: {message}", source.kind, source.name, source.root.host_id))
                        }
                        other => {
                            Err(format!("delete {}/{} from root {} returned {other:?}", source.kind, source.name, source.root.host_id))
                        }
                    };
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(format!("daemon closed while deleting {}/{}", source.kind, source.name));
                }
            }
        }
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

fn sweep_roots_from_hosts(hosts: &[HostListEntry]) -> Result<Vec<SweepRoot>, String> {
    let mut roots = Vec::new();
    for host in hosts {
        let Some(environment_id) = &host.environment_id else {
            continue;
        };
        let Some(host_id) = environment_id.host_id() else {
            continue;
        };
        if !host.is_local && host.connection_status != PeerConnectionState::Connected {
            return Err(format!("cannot sweep while host {} ({host_id}) is unreachable", host.host_name));
        }
        let node = host.node.as_ref().ok_or_else(|| format!("cannot sweep host {} ({host_id}) without a node identity", host.host_name))?;
        roots.push(SweepRoot { host_id: host_id.to_string(), node_id: node.node_id.clone() });
    }
    roots.sort_by(|a, b| a.host_id.cmp(&b.host_id));
    roots.dedup_by(|a, b| a.host_id == b.host_id);
    if roots.is_empty() {
        return Err("cannot sweep: fleet host inventory is empty".to_string());
    }
    Ok(roots)
}

fn natural_home(kind: &str, object: &serde_json::Value) -> Option<String> {
    match kind {
        "hosts" => object.pointer("/metadata/name").and_then(serde_json::Value::as_str).map(ToOwned::to_owned),
        "placementpolicies" => object
            .pointer("/spec/host_direct/host_ref")
            .or_else(|| object.pointer("/spec/docker_per_vessel/host_ref"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
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
    use flotilla_protocol::{
        qualified_path::HostId, EnvironmentId, HostListResponse, HostName, Message, NodeInfo, RepoIdentity, Request, ResourceJsonResponse,
        ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType, Response,
    };
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

    fn list_envelope(kind: &str, node_id: &NodeId, objects: Vec<serde_json::Value>) -> ResourceReadEnvelope {
        ResourceReadEnvelope {
            api_version: "flotilla.work/v1".to_string(),
            resource_kind: kind.to_string(),
            plural: kind.to_string(),
            namespace: "flotilla".to_string(),
            cursor: ResourceCursor::from_position("1", None),
            records: objects
                .into_iter()
                .map(|object| ResourceReadRecord {
                    record_type: ResourceRecordType::Current,
                    provenance: ResourceRecordProvenance::Local { node_id: node_id.clone() },
                    object: Some(object),
                })
                .collect(),
        }
    }

    fn three_root_hosts() -> Vec<HostListEntry> {
        ["a", "b", "c"]
            .into_iter()
            .map(|name| HostListEntry {
                environment_id: Some(EnvironmentId::host(HostId::new(format!("host-{name}")))),
                host_name: HostName::new(name),
                node: Some(NodeInfo::new(NodeId::new(format!("root-{name}")), name)),
                is_local: name == "a",
                configured: name != "a",
                connection_status: PeerConnectionState::Connected,
                reconnect: None,
                has_summary: true,
                repo_count: 0,
            })
            .collect()
    }

    fn duplicated_three_root_fixture() -> BTreeMap<(String, String), Vec<serde_json::Value>> {
        let mut fixture = BTreeMap::new();
        for root in ["a", "b", "c"] {
            fixture.insert(
                (format!("root-{root}"), "hosts".to_string()),
                ["a", "b", "c"]
                    .into_iter()
                    .map(|home| serde_json::json!({"metadata": {"name": format!("host-{home}")}, "spec": {"display_name": home}}))
                    .collect(),
            );
            let mut policies = ["a", "b", "c"]
                .into_iter()
                .map(|home| {
                    serde_json::json!({
                        "metadata": {"name": format!("host-direct-host-{home}")},
                        "spec": {"pool": "passthrough", "host_direct": {"host_ref": format!("host-{home}"), "checkout": "worktree"}}
                    })
                })
                .collect::<Vec<_>>();
            policies.push(serde_json::json!({
                "metadata": {"name": "placement-snapshot-012345abcdef"},
                "spec": {"pool": "cleat", "docker_per_vessel": {"host_ref": "host-b", "image": "crew:latest"}}
            }));
            fixture.insert((format!("root-{root}"), "placementpolicies".to_string()), policies);
        }
        fixture
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

    #[tokio::test]
    async fn dedup_sweep_converges_three_roots_and_is_idempotent() {
        let (client_session, server_session) = message_session_pair();
        let daemon = SocketDaemon::from_session(client_session).expect("create socket daemon");
        let client = ResourceClient::new(daemon);
        let server = tokio::spawn(async move {
            let mut fixture = duplicated_three_root_fixture();
            let mut next_command_id = 1;
            for pass in 0..2 {
                let Some(Message::Request { id, request: Request::Execute { command } }) =
                    server_session.read().await.expect("read host inventory")
                else {
                    panic!("expected host inventory request");
                };
                assert!(matches!(command.action, CommandAction::QueryHostList {}));
                server_session
                    .write(Message::ok_response(id, Response::QueryResult {
                        command_id: next_command_id,
                        value: CommandValue::HostList(Box::new(HostListResponse { hosts: three_root_hosts() })),
                    }))
                    .await
                    .expect("write host inventory");
                next_command_id += 1;

                for root in ["a", "b", "c"] {
                    for kind in SINGLE_HOME_SWEEP_KINDS {
                        let Some(Message::Request { id, request: Request::Execute { command } }) =
                            server_session.read().await.expect("read resource inventory")
                        else {
                            panic!("expected resource inventory request");
                        };
                        assert_eq!(command.node_id, Some(NodeId::new(format!("root-{root}"))));
                        assert!(matches!(
                            command.action,
                            CommandAction::QueryResourceList { kind: ref requested, include_replicas: false, .. } if requested == kind
                        ));
                        let node_id = command.node_id.expect("inventory target");
                        let objects = fixture.get(&(node_id.to_string(), (*kind).to_string())).cloned().expect("fixture root and kind");
                        server_session
                            .write(Message::ok_response(id, Response::QueryResult {
                                command_id: next_command_id,
                                value: CommandValue::ResourceRead(Box::new(list_envelope(kind, &node_id, objects))),
                            }))
                            .await
                            .expect("write resource inventory");
                        next_command_id += 1;
                    }
                }

                if pass == 0 {
                    for _ in 0..14 {
                        let Some(Message::Request { id, request: Request::Execute { command } }) =
                            server_session.read().await.expect("read raw delete")
                        else {
                            panic!("expected raw delete request");
                        };
                        let node_id = command.node_id.clone().expect("delete target");
                        let CommandAction::ResourceDelete { kind, name, replica_origin: None, .. } = command.action else {
                            panic!("expected authoritative resource delete");
                        };
                        let objects = fixture.get_mut(&(node_id.to_string(), kind.clone())).expect("delete fixture root and kind");
                        let before = objects.len();
                        objects
                            .retain(|object| object.pointer("/metadata/name").and_then(serde_json::Value::as_str) != Some(name.as_str()));
                        assert_eq!(objects.len() + 1, before, "sweep should delete an existing authored copy");
                        let command_id = next_command_id;
                        next_command_id += 1;
                        server_session
                            .write(Message::ok_response(id, Response::Execute { command_id }))
                            .await
                            .expect("acknowledge raw delete");
                        server_session
                            .write(Message::Event {
                                event: Box::new(DaemonEvent::CommandFinished {
                                    command_id,
                                    node_id,
                                    repo_identity: RepoIdentity { authority: "local".to_string(), path: "resource".to_string() },
                                    repo: None,
                                    result: CommandValue::ResourceDeleted(Box::new(ResourceJsonResponse {
                                        kind: kind.clone(),
                                        plural: kind,
                                        namespace: "flotilla".to_string(),
                                        value: serde_json::json!({"metadata": {"name": name}}),
                                        replica_origin: None,
                                    })),
                                }),
                            })
                            .await
                            .expect("finish raw delete");
                    }
                }
            }
            fixture
        });

        let report = client.dedup_single_home_records("flotilla").await.expect("sweep duplicates");
        assert_eq!(report.inspected_roots, 3);
        assert_eq!(report.duplicate_records, 7);
        assert_eq!(report.deletions.len(), 14);

        let second = client.dedup_single_home_records("flotilla").await.expect("repeat sweep");
        assert_eq!(second.duplicate_records, 0);
        assert!(second.deletions.is_empty());

        let fixture = server.await.expect("server task");
        let mut federated_counts = BTreeMap::<(String, String), usize>::new();
        for ((_, kind), objects) in fixture {
            for object in objects {
                let name = object.pointer("/metadata/name").and_then(serde_json::Value::as_str).expect("fixture name");
                *federated_counts.entry((kind.clone(), name.to_string())).or_default() += 1;
            }
        }
        assert_eq!(federated_counts.len(), 7);
        assert!(federated_counts.values().all(|count| *count == 1));
    }
}

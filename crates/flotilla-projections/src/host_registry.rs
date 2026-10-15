use std::collections::{HashMap, HashSet};

use flotilla_protocol::{
    BlobSyncStatus, DaemonEvent, EnvironmentId, EnvironmentInfo, HostIdentity, HostListEntry, HostListResponse, HostName,
    HostProvidersResponse, HostSnapshot, HostStatusResponse, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, SystemInfo,
    ToolInventory, TopologyResponse, TopologyRoute,
};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCounts {
    pub repo_count: usize,
}

#[derive(Debug, Clone)]
struct HostState {
    node_id: NodeId,
    environment_id: EnvironmentId,
    summary: Option<HostSummary>,
    seq: u64,
    removed: bool,
}

/// Resource-derived query details which are not part of HostSnapshot's shape.
#[derive(Debug, Clone, Default, bon::Builder)]
pub struct HostQueryDetails {
    pub visible_environments: Option<Vec<EnvironmentInfo>>,
    pub blob_sync: Option<BlobSyncStatus>,
}

/// Plane-A peer state supplied by the host; projections only join it to read models.
pub trait PeerOverlay: Send + Sync {
    fn node_connectivity(&self) -> &RwLock<HashMap<NodeId, PeerConnectionState>>;
    fn configured_peers(&self) -> &RwLock<HashMap<NodeId, String>>;
    fn topology_routes(&self) -> &RwLock<Vec<TopologyRoute>>;
    fn peer_identities(&self) -> &RwLock<HashMap<EnvironmentId, HostIdentity>>;
    fn is_configured_peer_environment_id(&self, environment_id: &EnvironmentId) -> bool;
    fn summary_is_overlay_placeholder(&self, summary: &HostSummary) -> bool;
    fn connection_status_for_node(
        &self,
        local_node: &NodeInfo,
        node_connectivity: &HashMap<NodeId, PeerConnectionState>,
        node_id: &NodeId,
    ) -> PeerConnectionState;
}

pub struct HostRegistry {
    peers: std::sync::Arc<dyn PeerOverlay>,
    pub description_projection: tokio::sync::Mutex<()>,
    resource_environments: RwLock<HashSet<EnvironmentId>>,
    query_details: RwLock<HashMap<EnvironmentId, HostQueryDetails>>,
    local_node: NodeInfo,
    hosts: RwLock<HashMap<EnvironmentId, HostState>>,
    node_environments: RwLock<HashMap<NodeId, EnvironmentId>>,
    local_host_summary: RwLock<HostSummary>,
}

impl HostRegistry {
    pub fn new(local_node: NodeInfo, local_host_summary: HostSummary, peers: std::sync::Arc<dyn PeerOverlay>) -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(
            local_host_summary.environment_id.clone(),
            HostState {
                node_id: local_node.node_id.clone(),
                environment_id: local_host_summary.environment_id.clone(),
                summary: Some(local_host_summary.clone()),
                seq: 1,
                removed: false,
            },
        );
        let mut node_environments = HashMap::new();
        node_environments.insert(local_node.node_id.clone(), local_host_summary.environment_id.clone());
        Self {
            peers,
            description_projection: tokio::sync::Mutex::new(()),
            resource_environments: RwLock::new(HashSet::new()),
            query_details: RwLock::new(HashMap::new()),
            local_node,
            hosts: RwLock::new(hosts),
            node_environments: RwLock::new(node_environments),
            local_host_summary: RwLock::new(local_host_summary),
        }
    }

    #[allow(dead_code)]
    pub async fn local_host_summary(&self) -> HostSummary {
        self.local_host_summary.read().await.clone()
    }

    pub async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        let node_connectivity = self.peers.node_connectivity().read().await;
        self.peers.connection_status_for_node(&self.local_node, &node_connectivity, node_id)
    }

    pub async fn connected_peer_summaries(&self) -> Vec<HostSummary> {
        let node_connectivity = self.peers.node_connectivity().read().await;
        let hosts = self.hosts.read().await;
        hosts
            .values()
            .filter(|state| !state.removed && state.node_id != self.local_node.node_id)
            .filter(|state| {
                self.peers.connection_status_for_node(&self.local_node, &node_connectivity, &state.node_id)
                    == PeerConnectionState::Connected
            })
            .filter_map(|state| state.summary.clone())
            .collect()
    }

    pub async fn list_hosts(&self, counts: &HashMap<EnvironmentId, HostCounts>) -> HostListResponse {
        let configured = self.peers.configured_peers().read().await.clone();
        let node_connectivity = self.peers.node_connectivity().read().await.clone();
        let hosts = self.hosts.read().await;
        let mut host_entries: Vec<_> = hosts
            .iter()
            .filter(|(_, state)| !state.removed)
            .map(|(environment_id, state)| {
                build_host_list_entry_from_state(
                    self.peers.as_ref(),
                    &self.local_node,
                    &configured,
                    &node_connectivity,
                    counts,
                    environment_id,
                    state,
                )
            })
            .collect();
        host_entries.sort_by(|a, b| {
            b.is_local
                .cmp(&a.is_local)
                .then_with(|| a.node.as_ref().map(|node| &node.node_id).cmp(&b.node.as_ref().map(|node| &node.node_id)))
                .then_with(|| a.environment_id.cmp(&b.environment_id))
        });

        HostListResponse { hosts: host_entries }
    }

    pub async fn environment_id_for_node(&self, node_id: &NodeId) -> Option<EnvironmentId> {
        self.node_environments.read().await.get(node_id).cloned()
    }

    pub async fn node_id_for_host_name(&self, host: &HostName) -> Result<Option<NodeId>, String> {
        let hosts = self.hosts.read().await;
        let mut matches = hosts
            .values()
            .filter(|state| !state.removed)
            .filter_map(|state| {
                state
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.host_name.as_ref())
                    .filter(|name| *name == host)
                    .map(|_| state.node_id.clone())
            })
            .collect::<Vec<_>>();
        drop(hosts);
        matches.extend(
            self.peers
                .topology_routes()
                .read()
                .await
                .iter()
                .filter(|route| route.connected || !route.fallbacks.is_empty())
                .filter(|route| HostName::new(route.target.display_name.clone()) == *host)
                .map(|route| route.target.node_id.clone()),
        );
        matches.sort();
        matches.dedup();
        match matches.as_slice() {
            [] => Ok(None),
            [node_id] => Ok(Some(node_id.clone())),
            _ => Err(format!("host name '{host}' matches multiple routed nodes")),
        }
    }

    pub async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
    ) -> Result<HostStatusResponse, String> {
        let configured = self.peers.configured_peers().read().await.clone();
        let node_connectivity = self.peers.node_connectivity().read().await.clone();
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        if state.removed {
            return Err(format!("host not found: {environment_id}"));
        }
        let summary = state.summary.clone();

        let mut response = build_host_status(
            self.peers.as_ref(),
            environment_id,
            state,
            summary,
            HostStatusContext { local_node: &self.local_node, configured: &configured, node_connectivity: &node_connectivity, counts },
        );
        if let Some(details) = self.query_details.read().await.get(environment_id) {
            response.blob_sync = details.blob_sync.clone();
            if let Some(environments) = &details.visible_environments {
                response.visible_environments = environments.clone();
            }
        }
        Ok(response)
    }

    pub async fn get_host_providers(
        &self,
        environment_id: &EnvironmentId,
        _counts: &HashMap<EnvironmentId, HostCounts>,
    ) -> Result<HostProvidersResponse, String> {
        let configured = self.peers.configured_peers().read().await.clone();
        let node_connectivity = self.peers.node_connectivity().read().await.clone();
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        if state.removed {
            return Err(format!("host not found: {environment_id}"));
        }
        let summary = state.summary.clone().ok_or_else(|| format!("no summary available for host: {environment_id}"))?;

        let mut response =
            build_host_providers(self.peers.as_ref(), environment_id, state, &self.local_node, &configured, &node_connectivity, summary);
        if let Some(environments) =
            self.query_details.read().await.get(environment_id).and_then(|details| details.visible_environments.as_ref())
        {
            response.visible_environments = environments.clone();
        }
        Ok(response)
    }

    pub async fn get_topology(&self) -> TopologyResponse {
        let routes = self.peers.topology_routes().read().await.clone();
        let configured = self.peers.configured_peers().read().await.clone();
        build_topology(&self.local_node, &routes, &configured)
    }

    pub async fn next_hop_host_for_target_host(&self, target_host: &HostName) -> Result<Option<HostName>, String> {
        let local_host_name = self.local_host_summary.read().await.host_name.clone().unwrap_or_else(HostName::local);
        let hosts = self.hosts.read().await;
        let mut host_names_by_node: HashMap<NodeId, HostName> = hosts
            .values()
            .filter(|state| !state.removed)
            .filter_map(|state| {
                state.summary.as_ref().and_then(|summary| summary.host_name.clone()).map(|host| (state.node_id.clone(), host))
            })
            .collect();
        host_names_by_node.insert(self.local_node.node_id.clone(), local_host_name);

        let routes = self.peers.topology_routes().read().await;
        for route in routes.iter() {
            host_names_by_node.entry(route.target.node_id.clone()).or_insert_with(|| HostName::new(route.target.display_name.clone()));
            host_names_by_node.entry(route.next_hop.node_id.clone()).or_insert_with(|| HostName::new(route.next_hop.display_name.clone()));
            for fallback in &route.fallbacks {
                host_names_by_node.entry(fallback.node_id.clone()).or_insert_with(|| HostName::new(fallback.display_name.clone()));
            }
        }

        let mut target_nodes: Vec<_> =
            host_names_by_node.iter().filter_map(|(node, host)| (host == target_host).then_some(node.clone())).collect();
        target_nodes.sort();
        target_nodes.dedup();

        let target_node = match target_nodes.as_slice() {
            [] => return Ok(None),
            [node] => node,
            _ => return Err(format!("host name '{target_host}' matches multiple routed nodes")),
        };

        let Some(route) = routes.iter().find(|route| route.target.node_id == *target_node) else {
            return Ok(None);
        };
        let next_hop_node = if route.connected {
            &route.next_hop.node_id
        } else if let Some(fallback) = route.fallbacks.first() {
            // PeerManager emits fallbacks most-recently learned first, matching
            // its route promotion policy.
            &fallback.node_id
        } else {
            return Err(format!("unreachable next hop for host '{target_host}'"));
        };

        host_names_by_node
            .get(next_hop_node)
            .cloned()
            .ok_or_else(|| format!("unreachable next hop for host '{target_host}': no host summary for node {next_hop_node}"))
            .map(Some)
    }

    pub async fn host_name_for_node(&self, node_id: &NodeId) -> Option<HostName> {
        let environment_id = self.node_environments.read().await.get(node_id).cloned()?;
        self.hosts
            .read()
            .await
            .get(&environment_id)
            .filter(|state| !state.removed)
            .and_then(|state| state.summary.as_ref()?.host_name.clone())
    }

    pub async fn live_routed_host_name(&self, target_node: &NodeId) -> Option<HostName> {
        let routes = self.peers.topology_routes().read().await;
        let route = routes.iter().find(|route| route.target.node_id == *target_node)?;
        if !route.connected && route.fallbacks.is_empty() {
            return None;
        }
        drop(routes);

        self.host_name_for_node(target_node).await
    }

    pub async fn replay_host_events(&self, last_seen: &HashMap<StreamKey, u64>) -> Vec<DaemonEvent> {
        let configured = self.peers.configured_peers().read().await.clone();
        let node_connectivity = self.peers.node_connectivity().read().await.clone();
        let mut events = Vec::new();
        for state in self.hosts.read().await.values() {
            let environment_id = state.environment_id.clone();
            let stream_key = StreamKey::Host { environment_id: environment_id.clone() };
            let up_to_date = last_seen.get(&stream_key).is_some_and(|seq| *seq == state.seq);
            if up_to_date {
                continue;
            }
            if state.removed {
                if last_seen.contains_key(&stream_key) {
                    events.push(DaemonEvent::HostRemoved { environment_id, seq: state.seq });
                }
            } else {
                events.push(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(
                    self.peers.as_ref(),
                    &self.local_node,
                    &configured,
                    &node_connectivity,
                    &environment_id,
                    state,
                ))));
            }
        }
        events
    }

    pub async fn sync_host_membership(&self, counts: &HashMap<EnvironmentId, HostCounts>, emit: &impl Fn(DaemonEvent)) {
        let configured = self.peers.configured_peers().read().await.clone();
        let node_connectivity = self.peers.node_connectivity().read().await.clone();
        let mut hosts = self.hosts.write().await;

        let environment_ids: Vec<_> = hosts.keys().cloned().collect();
        for environment_id in environment_ids {
            let Some(state) = hosts.get(&environment_id) else {
                continue;
            };
            if should_present_host_state(
                PeerHost { peers: self.peers.as_ref(), local_node: &self.local_node },
                &configured,
                &node_connectivity,
                counts,
                &environment_id,
                &state.node_id,
                state,
            ) {
                continue;
            }
            let environment_id = state.environment_id.clone();
            if let Some(seq) = mark_host_removed(&mut hosts, &environment_id) {
                emit(DaemonEvent::HostRemoved { environment_id, seq });
            }
        }
    }

    pub async fn publish_peer_connection_status(
        &self,
        node: &NodeInfo,
        status: PeerConnectionState,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let snapshots = {
            let configured = self.peers.configured_peers().read().await.clone();
            let mut node_connectivity = self.peers.node_connectivity().write().await;
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            update_host_status(
                PeerHost { peers: self.peers.as_ref(), local_node: &self.local_node },
                &configured,
                &mut node_connectivity,
                &mut node_environments,
                &mut hosts,
                node,
                status.clone(),
            )
        };
        if !snapshots.is_empty() {
            emit(DaemonEvent::PeerStatusChanged { node_id: node.node_id.clone(), status });
            for snapshot in snapshots {
                emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
            }
        }
        self.sync_host_membership(counts, emit).await;
    }

    pub async fn has_resource_description(&self, environment_id: &EnvironmentId) -> bool {
        self.query_details.read().await.get(environment_id).is_some_and(|details| details.visible_environments.is_some())
    }

    async fn remove_host_presentation(&self, environment_id: &EnvironmentId, emit: &impl Fn(DaemonEvent)) {
        let mut environments = self.node_environments.write().await;
        let mut hosts = self.hosts.write().await;
        let Some(node_id) = hosts.get(environment_id).map(|state| state.node_id.clone()) else {
            return;
        };
        if let Some(seq) = mark_host_removed(&mut hosts, environment_id) {
            reassign_node_environment_if_needed(&hosts, &mut environments, &node_id, environment_id);
            emit(DaemonEvent::HostRemoved { environment_id: environment_id.clone(), seq });
        }
    }

    pub async fn sync_peer_identities(&self, identities: HashMap<EnvironmentId, HostIdentity>, emit: &impl Fn(DaemonEvent)) {
        let removed = {
            let mut known = self.peers.peer_identities().write().await;
            let removed = known.keys().filter(|id| !identities.contains_key(*id)).cloned().collect::<Vec<_>>();
            *known = identities.clone();
            removed
        };
        let descriptions = self.resource_environments.read().await;
        for environment_id in removed.iter().filter(|id| !descriptions.contains(*id)) {
            self.remove_host_presentation(environment_id, emit).await;
        }
        drop(descriptions);
        for identity in identities.into_values() {
            self.publish_peer_identity(identity, emit).await;
        }
    }

    pub async fn publish_peer_identity(&self, identity: HostIdentity, emit: &impl Fn(DaemonEvent)) {
        self.peers.peer_identities().write().await.insert(identity.environment_id.clone(), identity.clone());
        let snapshot = {
            let configured = self.peers.configured_peers().read().await.clone();
            let connectivity = self.peers.node_connectivity().read().await.clone();
            let mut environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            let mut summary =
                hosts.get(&identity.environment_id).and_then(|state| state.summary.clone()).unwrap_or_else(|| identity.clone().into());
            summary.node = identity.node;
            summary.host_name = identity.host_name;
            let node = summary.node.clone();
            update_host_summary(
                PeerHost { peers: self.peers.as_ref(), local_node: &self.local_node },
                &configured,
                &connectivity,
                &mut environments,
                &mut hosts,
                &node,
                summary,
            )
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
    }

    pub async fn sync_resource_summaries(
        &self,
        summaries: HashMap<EnvironmentId, HostSummary>,
        details: HashMap<EnvironmentId, HostQueryDetails>,
        emit: &impl Fn(DaemonEvent),
    ) {
        *self.query_details.write().await = details;
        let mut projected = self.resource_environments.write().await;
        let removed = projected.difference(&summaries.keys().cloned().collect()).cloned().collect::<Vec<_>>();
        *projected = summaries.keys().cloned().collect();
        drop(projected);
        for environment_id in removed {
            let identity = self.peers.peer_identities().read().await.get(&environment_id).cloned();
            if let Some(identity) = identity {
                // Losing a resource description clears observations, not a live
                // identity. Keep the peer visible with an identity-only summary.
                self.publish_resource_summary(identity.into(), emit).await;
            } else {
                self.remove_host_presentation(&environment_id, emit).await;
            }
        }
        for summary in summaries.into_values() {
            if summary.node.node_id == self.local_node.node_id {
                *self.local_host_summary.write().await = summary.clone();
            }
            self.publish_resource_summary(summary, emit).await;
        }
    }

    pub async fn publish_peer_summary(&self, summary: HostSummary, emit: &impl Fn(DaemonEvent)) {
        if self.peers.summary_is_overlay_placeholder(&summary)
            && self
                .hosts
                .read()
                .await
                .get(&summary.environment_id)
                .and_then(|state| state.summary.as_ref())
                .is_some_and(|existing| !self.peers.summary_is_overlay_placeholder(existing))
        {
            return;
        }
        self.publish_resource_summary(summary, emit).await;
    }

    async fn publish_resource_summary(&self, mut summary: HostSummary, emit: &impl Fn(DaemonEvent)) {
        let identities = self.peers.peer_identities().read().await;
        // A currently advertised identity wins in both publication paths. Resource
        // identity is the last-known fallback after the live identity disappears.
        if let Some(identity) = identities.get(&summary.environment_id) {
            summary.node = identity.node.clone();
            summary.host_name = identity.host_name.clone();
        }
        let snapshot = {
            let configured = self.peers.configured_peers().read().await.clone();
            let node_connectivity = self.peers.node_connectivity().read().await.clone();
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            let node = summary.node.clone();
            update_host_summary(
                PeerHost { peers: self.peers.as_ref(), local_node: &self.local_node },
                &configured,
                &node_connectivity,
                &mut node_environments,
                &mut hosts,
                &node,
                summary,
            )
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
    }

    pub async fn set_configured_peers(
        &self,
        peers: Vec<NodeInfo>,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let peers_map: HashMap<NodeId, String> = peers.iter().map(|node| (node.node_id.clone(), node.display_name.clone())).collect();
        {
            let mut configured = self.peers.configured_peers().write().await;
            *configured = peers_map;
        }
        self.sync_host_membership(counts, emit).await;
    }

    pub async fn set_peer_host_summaries(
        &self,
        summaries: HashMap<EnvironmentId, HostSummary>,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        {
            let configured = self.peers.configured_peers().read().await.clone();
            let node_connectivity = self.peers.node_connectivity().read().await.clone();
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            let environment_ids: Vec<_> = hosts.keys().cloned().collect();
            for environment_id in environment_ids {
                let Some(state) = hosts.get(&environment_id) else {
                    continue;
                };
                if state.node_id == self.local_node.node_id {
                    continue;
                }
                if !summaries.contains_key(&environment_id) {
                    let node_id = state.node_id.clone();
                    if let Some(seq) = mark_host_removed(&mut hosts, &environment_id) {
                        reassign_node_environment_if_needed(&hosts, &mut node_environments, &node_id, &environment_id);
                        emit(DaemonEvent::HostRemoved { environment_id, seq });
                    }
                }
            }
            for (_environment_id, summary) in summaries {
                let node = summary.node.clone();
                if let Some(snapshot) = update_host_summary(
                    PeerHost { peers: self.peers.as_ref(), local_node: &self.local_node },
                    &configured,
                    &node_connectivity,
                    &mut node_environments,
                    &mut hosts,
                    &node,
                    summary,
                ) {
                    emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                }
            }
        }

        self.sync_host_membership(counts, emit).await;
    }

    pub async fn set_topology_routes(&self, mut routes: Vec<TopologyRoute>) {
        routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
        let mut stored = self.peers.topology_routes().write().await;
        *stored = routes;
    }

    pub fn apply_event(&self, event: &DaemonEvent) {
        match event {
            DaemonEvent::PeerStatusChanged { node_id, status } => {
                if let Ok(mut node_connectivity) = self.peers.node_connectivity().try_write() {
                    node_connectivity.insert(node_id.clone(), status.clone());
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Ok(mut node_environments) = self.node_environments.try_write() {
                        if let Ok(mut node_connectivity) = self.peers.node_connectivity().try_write() {
                            if hosts.get(&snap.environment_id).is_some_and(|state| state.seq > snap.seq) {
                                return;
                            }

                            let state = ensure_host_state(
                                self.peers.as_ref(),
                                &mut hosts,
                                &mut node_environments,
                                &snap.node,
                                snap.environment_id.clone(),
                            );
                            if state.seq <= snap.seq {
                                node_connectivity.entry(snap.node.node_id.clone()).or_insert(snap.connection_status.clone());
                                state.environment_id = snap.environment_id.clone();
                                state.summary = Some(snap.summary.clone());
                                state.seq = snap.seq;
                                state.removed = false;
                            }
                        }
                    }
                }
            }
            DaemonEvent::HostRemoved { environment_id, seq } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Some(state) = hosts.get_mut(environment_id) {
                        if state.seq <= *seq {
                            let node_id = state.node_id.clone();
                            state.summary = None;
                            state.seq = *seq;
                            state.removed = true;
                            if let Ok(mut node_environments) = self.node_environments.try_write() {
                                reassign_node_environment_if_needed(&hosts, &mut node_environments, &node_id, environment_id);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn default_host_summary(node: &NodeInfo, environment_id: &EnvironmentId) -> HostSummary {
    HostSummary {
        environment_id: environment_id.clone(),
        host_name: Some(HostName::new(node.display_name.clone())),
        node: node.clone(),
        system: SystemInfo::default(),
        inventory: ToolInventory::default(),
        providers: vec![],
        environments: vec![],
    }
}

fn ensure_host_state<'a>(
    peers: &dyn PeerOverlay,
    hosts: &'a mut HashMap<EnvironmentId, HostState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    node: &NodeInfo,
    environment_id: EnvironmentId,
) -> &'a mut HostState {
    let node_id = node.node_id.clone();
    let should_update_mapping = node_environments.get(&node_id).cloned().is_none_or(|current_environment_id| {
        current_environment_id == environment_id
            || peers.is_configured_peer_environment_id(&current_environment_id)
            || hosts.get(&current_environment_id).is_none_or(|state| state.removed)
    });
    if should_update_mapping {
        node_environments.insert(node_id.clone(), environment_id.clone());
    }

    let state = hosts.entry(environment_id.clone()).or_insert_with(|| HostState {
        node_id: node_id.clone(),
        environment_id: environment_id.clone(),
        summary: None,
        seq: 1,
        removed: false,
    });
    state.node_id = node_id;
    state.environment_id = environment_id;
    state
}

fn node_info_for(
    node_id: &NodeId,
    configured: &HashMap<NodeId, String>,
    summaries: Option<&HashMap<NodeId, HostSummary>>,
    local_node: Option<&NodeInfo>,
) -> NodeInfo {
    if let Some(local_node) = local_node.filter(|local| local.node_id == *node_id) {
        return local_node.clone();
    }
    if let Some(summary) = summaries.and_then(|summaries| summaries.get(node_id)) {
        return summary.node.clone();
    }
    if let Some(display_name) = configured.get(node_id) {
        return NodeInfo::new(node_id.clone(), display_name.clone());
    }
    NodeInfo::new(node_id.clone(), node_id.as_str())
}

fn build_host_snapshot(
    peers: &dyn PeerOverlay,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    _environment_id: &EnvironmentId,
    state: &HostState,
) -> HostSnapshot {
    debug_assert!(!state.removed, "removed hosts should not be materialized as snapshots");
    let node = state
        .summary
        .as_ref()
        .map(|summary| summary.node.clone())
        .unwrap_or_else(|| node_info_for(&state.node_id, configured, None, Some(local_node)));
    HostSnapshot {
        seq: state.seq,
        environment_id: state.environment_id.clone(),
        node: node.clone(),
        is_local: state.node_id == local_node.node_id,
        connection_status: peers.connection_status_for_node(local_node, node_connectivity, &state.node_id),
        summary: state.summary.clone().unwrap_or_else(|| default_host_summary(&node, &state.environment_id)),
    }
}

#[derive(Clone, Copy)]
struct PeerHost<'a> {
    peers: &'a dyn PeerOverlay,
    local_node: &'a NodeInfo,
}

fn update_host_status(
    PeerHost { peers, local_node }: PeerHost<'_>,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &mut HashMap<NodeId, PeerConnectionState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    status: PeerConnectionState,
) -> Vec<HostSnapshot> {
    let previous_status = peers.connection_status_for_node(local_node, node_connectivity, &node.node_id);
    let environment_ids: Vec<_> =
        hosts.values().filter(|state| !state.removed && state.node_id == node.node_id).map(|state| state.environment_id.clone()).collect();
    if environment_ids.is_empty() {
        node_connectivity.insert(node.node_id.clone(), status);
        return Vec::new();
    }
    node_connectivity.insert(node.node_id.clone(), status.clone());
    let mut snapshots = Vec::new();
    for environment_id in environment_ids {
        let state = ensure_host_state(peers, hosts, node_environments, node, environment_id);
        if !state.removed && previous_status == status {
            continue;
        }
        if state.summary.is_none() {
            let default_summary = default_host_summary(node, &state.environment_id);
            state.summary = Some(default_summary);
        }
        state.removed = false;
        state.seq += 1;
        snapshots.push(build_host_snapshot(peers, local_node, configured, node_connectivity, &state.environment_id, state));
    }
    snapshots
}

fn update_host_summary(
    PeerHost { peers, local_node }: PeerHost<'_>,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    summary: HostSummary,
) -> Option<HostSnapshot> {
    let current_environment_id = node_environments.get(&node.node_id).cloned();
    if let Some(current_environment_id) = current_environment_id.as_ref() {
        if *current_environment_id != summary.environment_id && peers.is_configured_peer_environment_id(current_environment_id) {
            if let Some(state) = hosts.get_mut(current_environment_id) {
                state.summary = None;
                state.removed = true;
                state.seq += 1;
            }
        }
    }
    if hosts.get(&summary.environment_id).is_some_and(|state| !state.removed && state.summary.as_ref() == Some(&summary)) {
        return None;
    }
    let state = ensure_host_state(peers, hosts, node_environments, node, summary.environment_id.clone());
    state.environment_id = summary.environment_id.clone();
    state.summary = Some(summary);
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(peers, local_node, configured, node_connectivity, &state.environment_id, state))
}

fn should_present_host_state(
    PeerHost { peers, local_node }: PeerHost<'_>,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    counts: &HashMap<EnvironmentId, HostCounts>,
    environment_id: &EnvironmentId,
    node_id: &NodeId,
    state: &HostState,
) -> bool {
    node_id == &local_node.node_id
        || configured.contains_key(node_id)
        || peers.connection_status_for_node(local_node, node_connectivity, node_id) != PeerConnectionState::Disconnected
        || state.summary.is_some()
        || counts.contains_key(environment_id)
}

fn mark_host_removed(hosts: &mut HashMap<EnvironmentId, HostState>, environment_id: &EnvironmentId) -> Option<u64> {
    let state = hosts.get_mut(environment_id)?;
    if state.removed {
        return None;
    }
    state.summary = None;
    state.removed = true;
    state.seq += 1;
    Some(state.seq)
}

fn reassign_node_environment_if_needed(
    hosts: &HashMap<EnvironmentId, HostState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    node_id: &NodeId,
    removed_environment_id: &EnvironmentId,
) {
    if node_environments.get(node_id).is_some_and(|current_environment_id| current_environment_id != removed_environment_id) {
        return;
    }

    let replacement = hosts
        .values()
        .filter(|state| !state.removed && state.node_id == *node_id)
        .max_by(|left, right| left.seq.cmp(&right.seq).then_with(|| left.environment_id.cmp(&right.environment_id)))
        .map(|state| state.environment_id.clone());

    if let Some(environment_id) = replacement {
        node_environments.insert(node_id.clone(), environment_id);
    } else {
        node_environments.remove(node_id);
    }
}

struct HostStatusContext<'a> {
    local_node: &'a NodeInfo,
    configured: &'a HashMap<NodeId, String>,
    node_connectivity: &'a HashMap<NodeId, PeerConnectionState>,
    counts: &'a HashMap<EnvironmentId, HostCounts>,
}

fn build_host_status(
    peers: &dyn PeerOverlay,
    environment_id: &EnvironmentId,
    state: &HostState,
    summary: Option<HostSummary>,
    ctx: HostStatusContext<'_>,
) -> HostStatusResponse {
    let is_local = state.node_id == ctx.local_node.node_id;
    let counts = ctx.counts.get(environment_id).copied().unwrap_or_default();
    let node = summary
        .as_ref()
        .map(|summary| summary.node.clone())
        .unwrap_or_else(|| node_info_for(&state.node_id, ctx.configured, None, Some(ctx.local_node)));
    let host_name =
        summary.as_ref().and_then(|summary| summary.host_name.clone()).unwrap_or_else(|| HostName::new(node.display_name.clone()));

    HostStatusResponse {
        environment_id: environment_id.clone(),
        host_name,
        node,
        is_local,
        configured: !is_local && ctx.configured.contains_key(&state.node_id),
        connection_status: peers.connection_status_for_node(ctx.local_node, ctx.node_connectivity, &state.node_id),
        visible_environments: summary.as_ref().map(|summary| summary.environments.clone()).unwrap_or_default(),
        summary,
        repo_count: counts.repo_count,
        blob_sync: None,
    }
}

fn build_host_list_entry_from_state(
    peers: &dyn PeerOverlay,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    counts: &HashMap<EnvironmentId, HostCounts>,
    environment_id: &EnvironmentId,
    state: &HostState,
) -> HostListEntry {
    let is_local = state.node_id == local_node.node_id;
    let counts = counts.get(environment_id).copied().unwrap_or_default();
    let node = state
        .summary
        .as_ref()
        .map(|summary| summary.node.clone())
        .unwrap_or_else(|| node_info_for(&state.node_id, configured, None, Some(local_node)));
    let host_name =
        state.summary.as_ref().and_then(|summary| summary.host_name.clone()).unwrap_or_else(|| HostName::new(node.display_name.clone()));

    HostListEntry {
        environment_id: Some(state.environment_id.clone()),
        host_name,
        node: Some(node),
        is_local,
        configured: !is_local && configured.contains_key(&state.node_id),
        connection_status: peers.connection_status_for_node(local_node, node_connectivity, &state.node_id),
        reconnect: None,
        has_summary: state.summary.is_some(),
        repo_count: counts.repo_count,
    }
}

fn build_host_providers(
    peers: &dyn PeerOverlay,
    environment_id: &EnvironmentId,
    state: &HostState,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    summary: HostSummary,
) -> HostProvidersResponse {
    HostProvidersResponse {
        environment_id: environment_id.clone(),
        host_name: summary.host_name.clone().unwrap_or_else(|| HostName::new(summary.node.display_name.clone())),
        node: summary.node.clone(),
        is_local: state.node_id == local_node.node_id,
        configured: state.node_id != local_node.node_id && configured.contains_key(&state.node_id),
        connection_status: peers.connection_status_for_node(local_node, node_connectivity, &state.node_id),
        visible_environments: summary.environments.clone(),
        summary,
    }
}

fn build_topology(local_node: &NodeInfo, routes: &[TopologyRoute], configured_peers: &HashMap<NodeId, String>) -> TopologyResponse {
    let mut all_routes = routes.to_vec();

    for (node_id, display_name) in configured_peers {
        if node_id == &local_node.node_id {
            continue;
        }
        if !routes.iter().any(|r| r.target.node_id == *node_id) {
            let node = NodeInfo::new(node_id.clone(), display_name.clone());
            all_routes.push(TopologyRoute {
                target: node.clone(),
                next_hop: node,
                direct: true,
                connected: false,
                fallbacks: vec![],
                last_attempt: None,
                last_error: None,
            });
        }
    }

    all_routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
    TopologyResponse { local_node: local_node.clone(), routes: all_routes }
}

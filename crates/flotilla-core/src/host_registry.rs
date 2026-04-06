use std::collections::{hash_map::Entry, HashMap, HashSet};

use flotilla_protocol::{
    DaemonEvent, HostListEntry, HostListResponse, HostProvidersResponse, HostSnapshot, HostStatusResponse, HostSummary, NodeId, NodeInfo,
    PeerConnectionState, StreamKey, SystemInfo, ToolInventory, TopologyResponse, TopologyRoute,
};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCounts {
    pub(crate) repo_count: usize,
    pub(crate) work_item_count: usize,
}

#[derive(Debug, Clone)]
struct HostState {
    connection_status: PeerConnectionState,
    summary: Option<HostSummary>,
    seq: u64,
    removed: bool,
}

pub(crate) struct HostRegistry {
    local_node: NodeInfo,
    hosts: RwLock<HashMap<NodeId, HostState>>,
    configured_peers: RwLock<HashMap<NodeId, String>>,
    topology_routes: RwLock<Vec<TopologyRoute>>,
    local_host_summary: RwLock<HostSummary>,
}

impl HostRegistry {
    pub(crate) fn new(local_node: NodeInfo, local_host_summary: HostSummary) -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(local_node.node_id.clone(), HostState {
            connection_status: PeerConnectionState::Connected,
            summary: Some(local_host_summary.clone()),
            seq: 1,
            removed: false,
        });
        Self {
            local_node,
            hosts: RwLock::new(hosts),
            configured_peers: RwLock::new(HashMap::new()),
            topology_routes: RwLock::new(Vec::new()),
            local_host_summary: RwLock::new(local_host_summary),
        }
    }

    pub(crate) async fn local_host_summary(&self) -> HostSummary {
        self.local_host_summary.read().await.clone()
    }

    pub(crate) async fn set_local_host_summary(&self, summary: HostSummary) {
        let changed = {
            let current = self.local_host_summary.read().await;
            *current != summary
        };
        if !changed {
            return;
        }

        {
            let mut current = self.local_host_summary.write().await;
            *current = summary.clone();
        }

        let mut hosts = self.hosts.write().await;
        if let Some(state) = hosts.get_mut(&self.local_node.node_id) {
            state.summary = Some(summary);
            state.seq += 1;
            state.removed = false;
        }
    }

    pub(crate) async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        self.hosts
            .read()
            .await
            .get(node_id)
            .filter(|state| !state.removed)
            .map(|state| state.connection_status.clone())
            .unwrap_or(PeerConnectionState::Disconnected)
    }

    pub(crate) async fn list_hosts(&self, local_counts: HostCounts, remote_counts: &HashMap<NodeId, HostCounts>) -> HostListResponse {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;

        let hosts = known_nodes(&self.local_node.node_id, &configured, &statuses, &summaries, remote_counts)
            .into_iter()
            .map(|node_id| {
                build_host_list_entry(&node_id, &self.local_node, &configured, &statuses, &summaries, local_counts, remote_counts)
            })
            .collect();

        HostListResponse { hosts }
    }

    pub(crate) async fn get_host_status(
        &self,
        node_id: &str,
        local_counts: HostCounts,
        remote_counts: &HashMap<NodeId, HostCounts>,
    ) -> Result<HostStatusResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let known = known_nodes(&self.local_node.node_id, &configured, &statuses, &summaries, remote_counts);
        let resolved =
            known.into_iter().find(|candidate| candidate.as_str() == node_id).ok_or_else(|| format!("host not found: {node_id}"))?;
        let summary =
            if resolved == self.local_node.node_id { Some(self.local_host_summary().await) } else { summaries.get(&resolved).cloned() };

        Ok(build_host_status(&resolved, &self.local_node, &configured, &statuses, &summaries, summary, local_counts, remote_counts))
    }

    pub(crate) async fn get_host_providers(
        &self,
        node_id: &str,
        remote_counts: &HashMap<NodeId, HostCounts>,
    ) -> Result<HostProvidersResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let known = known_nodes(&self.local_node.node_id, &configured, &statuses, &summaries, remote_counts);
        let resolved =
            known.into_iter().find(|candidate| candidate.as_str() == node_id).ok_or_else(|| format!("host not found: {node_id}"))?;
        let summary = if resolved == self.local_node.node_id {
            self.local_host_summary().await
        } else {
            summaries.get(&resolved).cloned().ok_or_else(|| format!("no summary available for host: {node_id}"))?
        };

        Ok(build_host_providers(&resolved, &self.local_node, &configured, &statuses, &summaries, summary))
    }

    pub(crate) async fn get_topology(&self) -> TopologyResponse {
        let routes = self.topology_routes.read().await.clone();
        let configured = self.configured_peers.read().await.clone();
        build_topology(&self.local_node, &routes, &configured)
    }

    pub(crate) async fn replay_host_events(&self, last_seen: &HashMap<StreamKey, u64>) -> Vec<DaemonEvent> {
        let configured = self.configured_peers.read().await.clone();
        let mut events = Vec::new();
        for (node_id, state) in self.hosts.read().await.iter() {
            let stream_key = StreamKey::Host { node_id: node_id.clone() };
            let up_to_date = last_seen.get(&stream_key).is_some_and(|seq| *seq == state.seq);
            if up_to_date {
                continue;
            }
            if state.removed {
                if last_seen.contains_key(&stream_key) {
                    events.push(DaemonEvent::HostRemoved { node_id: node_id.clone(), seq: state.seq });
                }
            } else {
                events.push(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.local_node, &configured, node_id, state))));
            }
        }
        events
    }

    async fn active_host_maps(&self) -> (HashMap<NodeId, PeerConnectionState>, HashMap<NodeId, HostSummary>) {
        let hosts = self.hosts.read().await;
        let active: HashMap<_, _> = hosts.iter().filter(|(_, state)| !state.removed).map(|(h, s)| (h.clone(), s.clone())).collect();
        let statuses = active.iter().map(|(node_id, state)| (node_id.clone(), state.connection_status.clone())).collect();
        let summaries =
            active.iter().filter_map(|(node_id, state)| state.summary.clone().map(|summary| (node_id.clone(), summary))).collect();
        (statuses, summaries)
    }

    pub(crate) async fn sync_host_membership(&self, remote_counts: &HashMap<NodeId, HostCounts>, emit: &impl Fn(DaemonEvent)) {
        let configured = self.configured_peers.read().await.clone();
        let mut hosts = self.hosts.write().await;

        for node_id in configured.keys().chain(remote_counts.keys()) {
            if node_id != &self.local_node.node_id {
                match hosts.entry(node_id.clone()) {
                    Entry::Vacant(entry) => {
                        let state = entry.insert(HostState {
                            connection_status: PeerConnectionState::Disconnected,
                            summary: None,
                            seq: 1,
                            removed: false,
                        });
                        emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.local_node, &configured, node_id, state))));
                    }
                    Entry::Occupied(mut entry) => {
                        let state = entry.get_mut();
                        if state.removed {
                            state.removed = false;
                            state.seq += 1;
                            emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.local_node, &configured, node_id, state))));
                        }
                    }
                }
            }
        }

        let node_ids: Vec<_> = hosts.keys().cloned().collect();
        for node_id in node_ids {
            let Some(state) = hosts.get(&node_id) else {
                continue;
            };
            if should_present_host_state(&self.local_node.node_id, &configured, remote_counts, &node_id, state) {
                continue;
            }
            if let Some(seq) = mark_host_removed(&mut hosts, &node_id) {
                emit(DaemonEvent::HostRemoved { node_id, seq });
            }
        }
    }

    pub(crate) async fn publish_peer_connection_status(
        &self,
        node: &NodeInfo,
        status: PeerConnectionState,
        remote_counts: &HashMap<NodeId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let snapshot = {
            let configured = self.configured_peers.read().await.clone();
            let mut hosts = self.hosts.write().await;
            update_host_status(&self.local_node, &configured, &mut hosts, node, status.clone())
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::PeerStatusChanged { node_id: node.node_id.clone(), status });
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
        self.sync_host_membership(remote_counts, emit).await;
    }

    pub(crate) async fn publish_peer_summary(&self, summary: HostSummary, emit: &impl Fn(DaemonEvent)) {
        let snapshot = {
            let configured = self.configured_peers.read().await.clone();
            let mut hosts = self.hosts.write().await;
            let node = summary.node.clone();
            update_host_summary(&self.local_node, &configured, &mut hosts, &node, summary)
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
    }

    pub(crate) async fn set_configured_peers(
        &self,
        peers: Vec<NodeInfo>,
        remote_counts: &HashMap<NodeId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let mut configured = self.configured_peers.write().await;
        *configured = peers.into_iter().map(|node| (node.node_id, node.display_name)).collect();
        drop(configured);
        self.sync_host_membership(remote_counts, emit).await;
    }

    pub(crate) async fn set_peer_host_summaries(
        &self,
        summaries: HashMap<NodeId, HostSummary>,
        remote_counts: &HashMap<NodeId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        {
            let configured = self.configured_peers.read().await.clone();
            let mut hosts = self.hosts.write().await;
            let node_ids: Vec<_> = hosts.keys().cloned().collect();
            for node_id in node_ids {
                if node_id == self.local_node.node_id {
                    continue;
                }
                if !summaries.contains_key(&node_id) {
                    if let Some(snapshot) = clear_host_summary(&self.local_node, &configured, &mut hosts, &node_id) {
                        emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                    }
                }
            }
            for (node_id, mut summary) in summaries {
                summary.node.node_id = node_id.clone();
                let node = summary.node.clone();
                if let Some(snapshot) = update_host_summary(&self.local_node, &configured, &mut hosts, &node, summary) {
                    emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                }
            }
        }

        self.sync_host_membership(remote_counts, emit).await;
    }

    pub(crate) async fn set_topology_routes(&self, mut routes: Vec<TopologyRoute>) {
        routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
        let mut stored = self.topology_routes.write().await;
        *stored = routes;
    }

    pub(crate) fn apply_event(&self, event: &DaemonEvent) {
        match event {
            DaemonEvent::PeerStatusChanged { node_id, status } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    let configured = self.configured_peers.try_read().map(|guard| guard.clone()).unwrap_or_default();
                    let node = node_info_for(node_id, &configured, None, None);
                    let _ = update_host_status(&self.local_node, &configured, &mut hosts, &node, status.clone());
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    match hosts.get_mut(&snap.node.node_id) {
                        Some(state) if state.seq == snap.seq => {
                            state.connection_status = snap.connection_status.clone();
                            state.summary = Some(snap.summary.clone());
                            state.removed = false;
                        }
                        Some(state) if state.seq < snap.seq => {
                            *state = HostState {
                                connection_status: snap.connection_status.clone(),
                                summary: Some(snap.summary.clone()),
                                seq: snap.seq,
                                removed: false,
                            };
                        }
                        None => {
                            hosts.insert(snap.node.node_id.clone(), HostState {
                                connection_status: snap.connection_status.clone(),
                                summary: Some(snap.summary.clone()),
                                seq: snap.seq,
                                removed: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            DaemonEvent::HostRemoved { node_id, seq } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Some(state) = hosts.get_mut(node_id) {
                        if state.seq <= *seq {
                            state.connection_status = PeerConnectionState::Disconnected;
                            state.summary = None;
                            state.seq = *seq;
                            state.removed = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn default_host_summary(node: &NodeInfo) -> HostSummary {
    HostSummary {
        node: node.clone(),
        system: SystemInfo::default(),
        inventory: ToolInventory::default(),
        providers: vec![],
        environments: vec![],
    }
}

fn ensure_remote_host_state<'a>(hosts: &'a mut HashMap<NodeId, HostState>, node_id: &NodeId) -> &'a mut HostState {
    hosts.entry(node_id.clone()).or_insert_with(|| HostState {
        connection_status: PeerConnectionState::Disconnected,
        summary: None,
        seq: 1,
        removed: false,
    })
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

fn build_host_snapshot(local_node: &NodeInfo, configured: &HashMap<NodeId, String>, node_id: &NodeId, state: &HostState) -> HostSnapshot {
    debug_assert!(!state.removed, "removed hosts should not be materialized as snapshots");
    let node = state
        .summary
        .as_ref()
        .map(|summary| summary.node.clone())
        .unwrap_or_else(|| node_info_for(node_id, configured, None, Some(local_node)));
    HostSnapshot {
        seq: state.seq,
        node: node.clone(),
        is_local: *node_id == local_node.node_id,
        connection_status: state.connection_status.clone(),
        summary: state.summary.clone().unwrap_or_else(|| default_host_summary(&node)),
    }
}

fn update_host_status(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    hosts: &mut HashMap<NodeId, HostState>,
    node: &NodeInfo,
    status: PeerConnectionState,
) -> Option<HostSnapshot> {
    let state = ensure_remote_host_state(hosts, &node.node_id);
    if !state.removed && state.connection_status == status {
        return None;
    }
    if state.summary.is_none() {
        state.summary = Some(default_host_summary(node));
    }
    state.connection_status = status;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, &node.node_id, state))
}

fn update_host_summary(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    hosts: &mut HashMap<NodeId, HostState>,
    node: &NodeInfo,
    summary: HostSummary,
) -> Option<HostSnapshot> {
    let state = ensure_remote_host_state(hosts, &node.node_id);
    if !state.removed && state.summary.as_ref() == Some(&summary) {
        return None;
    }
    state.summary = Some(summary);
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, &node.node_id, state))
}

fn clear_host_summary(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    hosts: &mut HashMap<NodeId, HostState>,
    node_id: &NodeId,
) -> Option<HostSnapshot> {
    if node_id == &local_node.node_id {
        return None;
    }
    let state = hosts.get_mut(node_id)?;
    state.summary.as_ref()?;
    state.summary = None;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, node_id, state))
}

fn should_present_host_state(
    local_node_id: &NodeId,
    configured: &HashMap<NodeId, String>,
    remote_counts: &HashMap<NodeId, HostCounts>,
    node_id: &NodeId,
    state: &HostState,
) -> bool {
    node_id == local_node_id
        || configured.contains_key(node_id)
        || state.connection_status != PeerConnectionState::Disconnected
        || state.summary.is_some()
        || remote_counts.contains_key(node_id)
}

fn mark_host_removed(hosts: &mut HashMap<NodeId, HostState>, node_id: &NodeId) -> Option<u64> {
    let state = hosts.get_mut(node_id)?;
    if state.removed {
        return None;
    }
    state.connection_status = PeerConnectionState::Disconnected;
    state.summary = None;
    state.removed = true;
    state.seq += 1;
    Some(state.seq)
}

fn known_nodes(
    local_node_id: &NodeId,
    configured: &HashMap<NodeId, String>,
    statuses: &HashMap<NodeId, PeerConnectionState>,
    summaries: &HashMap<NodeId, HostSummary>,
    remote_counts: &HashMap<NodeId, HostCounts>,
) -> Vec<NodeId> {
    let mut nodes = HashSet::from([local_node_id.clone()]);
    nodes.extend(configured.keys().cloned());
    nodes.extend(statuses.keys().cloned());
    nodes.extend(summaries.keys().cloned());
    nodes.extend(remote_counts.keys().cloned());

    let mut nodes: Vec<_> = nodes.into_iter().collect();
    nodes.sort_by(|a, b| {
        let a_local = a == local_node_id;
        let b_local = b == local_node_id;
        b_local.cmp(&a_local).then_with(|| a.cmp(b))
    });
    nodes
}

fn connection_status(node_id: &NodeId, local_node_id: &NodeId, statuses: &HashMap<NodeId, PeerConnectionState>) -> PeerConnectionState {
    if node_id == local_node_id {
        PeerConnectionState::Connected
    } else {
        statuses.get(node_id).cloned().unwrap_or(PeerConnectionState::Disconnected)
    }
}

fn build_host_list_entry(
    node_id: &NodeId,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    statuses: &HashMap<NodeId, PeerConnectionState>,
    summaries: &HashMap<NodeId, HostSummary>,
    local_counts: HostCounts,
    remote_counts: &HashMap<NodeId, HostCounts>,
) -> HostListEntry {
    let is_local = node_id == &local_node.node_id;
    let counts = if is_local { local_counts } else { remote_counts.get(node_id).copied().unwrap_or_default() };

    HostListEntry {
        node: node_info_for(node_id, configured, Some(summaries), Some(local_node)),
        is_local,
        configured: !is_local && configured.contains_key(node_id),
        connection_status: connection_status(node_id, &local_node.node_id, statuses),
        has_summary: is_local || summaries.contains_key(node_id),
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_status(
    node_id: &NodeId,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    statuses: &HashMap<NodeId, PeerConnectionState>,
    summaries: &HashMap<NodeId, HostSummary>,
    summary: Option<HostSummary>,
    local_counts: HostCounts,
    remote_counts: &HashMap<NodeId, HostCounts>,
) -> HostStatusResponse {
    let is_local = node_id == &local_node.node_id;
    let counts = if is_local { local_counts } else { remote_counts.get(node_id).copied().unwrap_or_default() };

    HostStatusResponse {
        node: node_info_for(node_id, configured, Some(summaries), Some(local_node)),
        is_local,
        configured: !is_local && configured.contains_key(node_id),
        connection_status: connection_status(node_id, &local_node.node_id, statuses),
        summary,
        visible_environments: vec![],
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_providers(
    node_id: &NodeId,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    statuses: &HashMap<NodeId, PeerConnectionState>,
    summaries: &HashMap<NodeId, HostSummary>,
    summary: HostSummary,
) -> HostProvidersResponse {
    HostProvidersResponse {
        node: node_info_for(node_id, configured, Some(summaries), Some(local_node)),
        is_local: node_id == &local_node.node_id,
        configured: node_id != &local_node.node_id && configured.contains_key(node_id),
        connection_status: connection_status(node_id, &local_node.node_id, statuses),
        summary,
        visible_environments: vec![],
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
            all_routes.push(TopologyRoute { target: node.clone(), next_hop: node, direct: true, connected: false, fallbacks: vec![] });
        }
    }

    all_routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
    TopologyResponse { local_node: local_node.clone(), routes: all_routes }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use flotilla_protocol::{DaemonEvent, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, SystemInfo, ToolInventory};

    use super::{HostCounts, HostRegistry};

    fn local_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("local-node"), "local-host")
    }

    fn peer_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("peer-node"), "peer-host")
    }

    fn minimal_summary(node: &NodeInfo) -> HostSummary {
        HostSummary {
            node: node.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        }
    }

    #[tokio::test]
    async fn publish_peer_connection_status_emits_node_keyed_events() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let events = RefCell::new(Vec::new());
        let emit = |event| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &emit).await;

        assert!(events.borrow().iter().any(|event| {
            matches!(event, DaemonEvent::PeerStatusChanged { node_id, status } if *node_id == peer_node().node_id && *status == PeerConnectionState::Connected)
        }));
        assert!(events
            .borrow()
            .iter()
            .any(|event| { matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id) }));
    }

    #[tokio::test]
    async fn replay_uses_node_id_stream_keys() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let replay = registry.replay_host_events(&HashMap::new()).await;
        assert!(replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));

        let replay = registry.replay_host_events(&HashMap::from([(StreamKey::Host { node_id: peer_node().node_id.clone() }, 2)])).await;
        assert!(!replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));
    }

    #[tokio::test]
    async fn configured_peers_keep_display_names_separate_from_identity() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        registry
            .set_configured_peers(
                vec![NodeInfo::new(NodeId::new("peer-node"), "Build Box")],
                &HashMap::from([(peer_node().node_id.clone(), HostCounts::default())]),
                &|_| {},
            )
            .await;

        let hosts = registry.list_hosts(HostCounts::default(), &HashMap::new()).await;
        let peer = hosts.hosts.into_iter().find(|entry| entry.node.node_id == peer_node().node_id).expect("peer entry");
        assert_eq!(peer.node.display_name, "Build Box");
        assert_eq!(peer.node.node_id, peer_node().node_id);
    }
}

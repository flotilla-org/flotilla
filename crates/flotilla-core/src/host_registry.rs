use std::collections::HashMap;

use flotilla_protocol::{
    DaemonEvent, EnvironmentId, HostListEntry, HostListResponse, HostName, HostProvidersResponse, HostSnapshot, HostStatusResponse,
    HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, SystemInfo, ToolInventory, TopologyResponse, TopologyRoute,
};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCounts {
    pub(crate) repo_count: usize,
    pub(crate) work_item_count: usize,
}

#[derive(Debug, Clone)]
struct HostState {
    node_id: NodeId,
    environment_id: EnvironmentId,
    summary: Option<HostSummary>,
    seq: u64,
    removed: bool,
}

pub(crate) struct HostRegistry {
    local_node: NodeInfo,
    hosts: RwLock<HashMap<EnvironmentId, HostState>>,
    node_connectivity: RwLock<HashMap<NodeId, PeerConnectionState>>,
    node_environments: RwLock<HashMap<NodeId, EnvironmentId>>,
    configured_peers: RwLock<HashMap<NodeId, String>>,
    topology_routes: RwLock<Vec<TopologyRoute>>,
    local_host_summary: RwLock<HostSummary>,
}

impl HostRegistry {
    pub(crate) fn new(local_node: NodeInfo, local_host_summary: HostSummary) -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(local_host_summary.environment_id.clone(), HostState {
            node_id: local_node.node_id.clone(),
            environment_id: local_host_summary.environment_id.clone(),
            summary: Some(local_host_summary.clone()),
            seq: 1,
            removed: false,
        });
        let mut node_connectivity = HashMap::new();
        node_connectivity.insert(local_node.node_id.clone(), PeerConnectionState::Connected);
        let mut node_environments = HashMap::new();
        node_environments.insert(local_node.node_id.clone(), local_host_summary.environment_id.clone());
        Self {
            local_node,
            hosts: RwLock::new(hosts),
            node_connectivity: RwLock::new(node_connectivity),
            node_environments: RwLock::new(node_environments),
            configured_peers: RwLock::new(HashMap::new()),
            topology_routes: RwLock::new(Vec::new()),
            local_host_summary: RwLock::new(local_host_summary),
        }
    }

    #[allow(dead_code)]
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

        let mut node_environments = self.node_environments.write().await;
        let mut hosts = self.hosts.write().await;
        let state = ensure_host_state(&mut hosts, &mut node_environments, &self.local_node, summary.environment_id.clone());
        node_environments.insert(self.local_node.node_id.clone(), summary.environment_id.clone());
        if state.summary.as_ref() != Some(&summary) {
            state.summary = Some(summary);
            state.seq += 1;
            state.removed = false;
        }
    }

    pub(crate) async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        let node_connectivity = self.node_connectivity.read().await;
        connection_status_for_node(&self.local_node, &node_connectivity, node_id)
    }

    pub(crate) async fn list_hosts(&self, counts: &HashMap<EnvironmentId, HostCounts>) -> HostListResponse {
        let configured = self.configured_peers.read().await.clone();
        let node_connectivity = self.node_connectivity.read().await.clone();
        let hosts = self.hosts.read().await;
        let mut host_entries: Vec<_> = hosts
            .iter()
            .filter(|(_, state)| !state.removed)
            .map(|(environment_id, state)| {
                build_host_list_entry_from_state(&self.local_node, &configured, &node_connectivity, counts, environment_id, state)
            })
            .collect();
        host_entries.sort_by(|a, b| {
            b.is_local
                .cmp(&a.is_local)
                .then_with(|| a.node.node_id.cmp(&b.node.node_id))
                .then_with(|| a.environment_id.cmp(&b.environment_id))
        });

        HostListResponse { hosts: host_entries }
    }

    #[cfg(test)]
    pub(crate) async fn environment_id_for_node(&self, node_id: &NodeId) -> Option<EnvironmentId> {
        self.node_environments.read().await.get(node_id).cloned()
    }

    pub(crate) async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
    ) -> Result<HostStatusResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let node_connectivity = self.node_connectivity.read().await.clone();
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        let summary = state.summary.clone();

        Ok(build_host_status(environment_id, state, summary, HostStatusContext {
            local_node: &self.local_node,
            configured: &configured,
            node_connectivity: &node_connectivity,
            counts,
        }))
    }

    pub(crate) async fn get_host_providers(
        &self,
        environment_id: &EnvironmentId,
        _counts: &HashMap<EnvironmentId, HostCounts>,
    ) -> Result<HostProvidersResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let node_connectivity = self.node_connectivity.read().await.clone();
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        let summary = state.summary.clone().ok_or_else(|| format!("no summary available for host: {environment_id}"))?;

        Ok(build_host_providers(environment_id, state, &self.local_node, &configured, &node_connectivity, summary))
    }

    pub(crate) async fn get_topology(&self) -> TopologyResponse {
        let routes = self.topology_routes.read().await.clone();
        let configured = self.configured_peers.read().await.clone();
        build_topology(&self.local_node, &routes, &configured)
    }

    pub(crate) async fn replay_host_events(&self, last_seen: &HashMap<StreamKey, u64>) -> Vec<DaemonEvent> {
        let configured = self.configured_peers.read().await.clone();
        let node_connectivity = self.node_connectivity.read().await.clone();
        let mut events = Vec::new();
        for (_environment_id, state) in self.hosts.read().await.iter() {
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

    pub(crate) async fn sync_host_membership(&self, counts: &HashMap<EnvironmentId, HostCounts>, emit: &impl Fn(DaemonEvent)) {
        let configured = self.configured_peers.read().await.clone();
        let node_connectivity = self.node_connectivity.read().await.clone();
        let mut hosts = self.hosts.write().await;

        let environment_ids: Vec<_> = hosts.keys().cloned().collect();
        for environment_id in environment_ids {
            let Some(state) = hosts.get(&environment_id) else {
                continue;
            };
            if should_present_host_state(&self.local_node, &configured, &node_connectivity, counts, &environment_id, &state.node_id, state)
            {
                continue;
            }
            let environment_id = state.environment_id.clone();
            if let Some(seq) = mark_host_removed(&mut hosts, &environment_id) {
                emit(DaemonEvent::HostRemoved { environment_id, seq });
            }
        }
    }

    pub(crate) async fn publish_peer_connection_status(
        &self,
        node: &NodeInfo,
        status: PeerConnectionState,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let snapshots = {
            let configured = self.configured_peers.read().await.clone();
            let mut node_connectivity = self.node_connectivity.write().await;
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            update_host_status(
                &self.local_node,
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

    pub(crate) async fn publish_peer_summary(&self, summary: HostSummary, emit: &impl Fn(DaemonEvent)) {
        let snapshot = {
            let configured = self.configured_peers.read().await.clone();
            let node_connectivity = self.node_connectivity.read().await.clone();
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            let node = summary.node.clone();
            update_host_summary(&self.local_node, &configured, &node_connectivity, &mut node_environments, &mut hosts, &node, summary)
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
    }

    pub(crate) async fn set_configured_peers(
        &self,
        peers: Vec<NodeInfo>,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let peers_map: HashMap<NodeId, String> = peers.iter().map(|node| (node.node_id.clone(), node.display_name.clone())).collect();
        {
            let mut configured = self.configured_peers.write().await;
            *configured = peers_map;
        }
        self.sync_host_membership(counts, emit).await;
    }

    pub(crate) async fn set_peer_host_summaries(
        &self,
        summaries: HashMap<EnvironmentId, HostSummary>,
        counts: &HashMap<EnvironmentId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        {
            let configured = self.configured_peers.read().await.clone();
            let node_connectivity = self.node_connectivity.read().await.clone();
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
                    if let Some(snapshot) =
                        clear_host_summary(&self.local_node, &configured, &node_connectivity, &mut hosts, &environment_id)
                    {
                        emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                    }
                }
            }
            for (_environment_id, summary) in summaries {
                let node = summary.node.clone();
                if let Some(snapshot) = update_host_summary(
                    &self.local_node,
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

    pub(crate) async fn set_topology_routes(&self, mut routes: Vec<TopologyRoute>) {
        routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
        let mut stored = self.topology_routes.write().await;
        *stored = routes;
    }

    pub(crate) fn apply_event(&self, event: &DaemonEvent) {
        match event {
            DaemonEvent::PeerStatusChanged { node_id, status } => {
                if let Ok(mut node_connectivity) = self.node_connectivity.try_write() {
                    node_connectivity.insert(node_id.clone(), status.clone());
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Ok(mut node_environments) = self.node_environments.try_write() {
                        if let Ok(mut node_connectivity) = self.node_connectivity.try_write() {
                            if hosts.get(&snap.environment_id).is_some_and(|state| state.seq > snap.seq) {
                                return;
                            }

                            let state = ensure_host_state(&mut hosts, &mut node_environments, &snap.node, snap.environment_id.clone());
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

fn is_configured_peer_environment_id(environment_id: &EnvironmentId) -> bool {
    environment_id.provisioned_id().is_some_and(|id| id.starts_with("configured-peer:"))
}

fn ensure_host_state<'a>(
    hosts: &'a mut HashMap<EnvironmentId, HostState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    node: &NodeInfo,
    environment_id: EnvironmentId,
) -> &'a mut HostState {
    let node_id = node.node_id.clone();
    let should_update_mapping = node_environments.get(&node_id).cloned().is_none_or(|current_environment_id| {
        current_environment_id == environment_id
            || is_configured_peer_environment_id(&current_environment_id)
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
        connection_status: connection_status_for_node(local_node, node_connectivity, &state.node_id),
        summary: state.summary.clone().unwrap_or_else(|| default_host_summary(&node, &state.environment_id)),
    }
}

fn update_host_status(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &mut HashMap<NodeId, PeerConnectionState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    status: PeerConnectionState,
) -> Vec<HostSnapshot> {
    let previous_status = connection_status_for_node(local_node, node_connectivity, &node.node_id);
    let environment_ids: Vec<_> =
        hosts.values().filter(|state| !state.removed && state.node_id == node.node_id).map(|state| state.environment_id.clone()).collect();
    if environment_ids.is_empty() {
        node_connectivity.insert(node.node_id.clone(), status);
        return Vec::new();
    }
    node_connectivity.insert(node.node_id.clone(), status.clone());
    let mut snapshots = Vec::new();
    for environment_id in environment_ids {
        let state = ensure_host_state(hosts, node_environments, node, environment_id);
        if !state.removed && previous_status == status {
            continue;
        }
        if state.summary.is_none() {
            let default_summary = default_host_summary(node, &state.environment_id);
            state.summary = Some(default_summary);
        }
        state.removed = false;
        state.seq += 1;
        snapshots.push(build_host_snapshot(local_node, configured, node_connectivity, &state.environment_id, state));
    }
    snapshots
}

fn update_host_summary(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    summary: HostSummary,
) -> Option<HostSnapshot> {
    let current_environment_id = node_environments.get(&node.node_id).cloned();
    if let Some(current_environment_id) = current_environment_id.as_ref() {
        if *current_environment_id != summary.environment_id && is_configured_peer_environment_id(current_environment_id) {
            if let Some(state) = hosts.get_mut(current_environment_id) {
                state.summary = None;
                state.removed = true;
                state.seq += 1;
            }
        }
    }
    if current_environment_id.as_ref().is_some_and(|current| *current == summary.environment_id) {
        if let Some(state) = current_environment_id.as_ref().and_then(|current| hosts.get(current)) {
            if summary_is_overlay_placeholder(&summary)
                && state.summary.as_ref().is_some_and(|existing| !summary_is_overlay_placeholder(existing))
            {
                return None;
            }
            if !state.removed && state.summary.as_ref() == Some(&summary) {
                return None;
            }
        }
    }

    let state = ensure_host_state(hosts, node_environments, node, summary.environment_id.clone());
    state.environment_id = summary.environment_id.clone();
    state.summary = Some(summary);
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, node_connectivity, &state.environment_id, state))
}

fn summary_is_overlay_placeholder(summary: &HostSummary) -> bool {
    summary.system == SystemInfo::default()
        && summary.inventory == ToolInventory::default()
        && summary.providers.is_empty()
        && summary.environments.is_empty()
}

fn clear_host_summary(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    environment_id: &EnvironmentId,
) -> Option<HostSnapshot> {
    let state = hosts.get_mut(environment_id)?;
    if state.node_id == local_node.node_id {
        return None;
    }
    state.summary.as_ref()?;
    state.summary = None;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, node_connectivity, environment_id, state))
}

fn should_present_host_state(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    counts: &HashMap<EnvironmentId, HostCounts>,
    environment_id: &EnvironmentId,
    node_id: &NodeId,
    state: &HostState,
) -> bool {
    node_id == &local_node.node_id
        || configured.contains_key(node_id)
        || connection_status_for_node(local_node, node_connectivity, node_id) != PeerConnectionState::Disconnected
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
        connection_status: connection_status_for_node(ctx.local_node, ctx.node_connectivity, &state.node_id),
        summary,
        visible_environments: vec![],
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_list_entry_from_state(
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
        environment_id: state.environment_id.clone(),
        host_name,
        node,
        is_local,
        configured: !is_local && configured.contains_key(&state.node_id),
        connection_status: connection_status_for_node(local_node, node_connectivity, &state.node_id),
        has_summary: state.summary.is_some(),
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_providers(
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
        connection_status: connection_status_for_node(local_node, node_connectivity, &state.node_id),
        summary,
        visible_environments: vec![],
    }
}

fn connection_status_for_node(
    local_node: &NodeInfo,
    node_connectivity: &HashMap<NodeId, PeerConnectionState>,
    node_id: &NodeId,
) -> PeerConnectionState {
    if *node_id == local_node.node_id {
        PeerConnectionState::Connected
    } else {
        node_connectivity.get(node_id).cloned().unwrap_or(PeerConnectionState::Disconnected)
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

    use flotilla_protocol::{
        qualified_path::HostId, DaemonEvent, EnvironmentId, HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState,
        StreamKey, SystemInfo, ToolInventory,
    };

    use super::{HostCounts, HostRegistry};

    fn local_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("local-node"), "local-host")
    }

    fn peer_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("peer-node"), "peer-host")
    }

    fn minimal_summary(node: &NodeInfo) -> HostSummary {
        HostSummary {
            environment_id: EnvironmentId::host(HostId::new(format!("{}-host", node.node_id))),
            host_name: Some(HostName::new(node.display_name.clone())),
            node: node.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        }
    }

    #[tokio::test]
    async fn publish_peer_connection_status_without_environment_mapping_does_not_synthesize_host_snapshot() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let events = RefCell::new(Vec::new());
        let emit = |event| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &emit).await;

        assert!(events.borrow().is_empty(), "peer status without a canonical environment id should not emit host state");
    }

    #[tokio::test]
    async fn replay_uses_environment_id_stream_keys() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        registry.publish_peer_summary(minimal_summary(&peer_node()), &|_| {}).await;

        let replay = registry.replay_host_events(&HashMap::new()).await;
        assert!(replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));

        let replay = registry
            .replay_host_events(&HashMap::from([(
                StreamKey::Host { environment_id: EnvironmentId::host(HostId::new("peer-node-host")) },
                2,
            )]))
            .await;
        assert!(!replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));
    }

    #[tokio::test]
    async fn configured_peers_without_environment_mapping_do_not_create_host_entries() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        registry.set_configured_peers(vec![NodeInfo::new(NodeId::new("peer-node"), "Build Box")], &HashMap::new(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(hosts.hosts.iter().all(|entry| entry.node.node_id != peer_node().node_id));
    }

    #[tokio::test]
    async fn overlay_placeholder_does_not_clobber_existing_real_summary() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = NodeInfo::new(NodeId::new("peer-node-42"), "Build Box");
        let real_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-42-host")),
            host_name: Some(HostName::new("build-box")),
            node: peer.clone(),
            system: SystemInfo { os: Some("linux".into()), arch: Some("x86_64".into()), ..SystemInfo::default() },
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
            }],
            environments: vec![],
        };

        registry.publish_peer_summary(real_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(minimal_summary(&peer), &|_| {}).await;

        let status = registry.get_host_status(&real_summary.environment_id, &HashMap::new()).await.expect("host status");
        let summary = status.summary.expect("summary should remain available");
        assert_eq!(summary.node.display_name, "Build Box");
        assert_eq!(summary.providers, real_summary.providers);
        assert_eq!(summary.system, real_summary.system);
    }

    #[tokio::test]
    async fn overlay_placeholder_for_second_environment_on_same_node_is_not_dropped() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = NodeInfo::new(NodeId::new("peer-node-43"), "Build Box");
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-43-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-43-host-b"));

        let real_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("build-box-a")),
            node: peer.clone(),
            system: SystemInfo { os: Some("linux".into()), arch: Some("x86_64".into()), ..SystemInfo::default() },
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
            }],
            environments: vec![],
        };
        let placeholder_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("build-box-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(real_summary, &|_| {}).await;
        registry.publish_peer_summary(placeholder_summary.clone(), &|_| {}).await;

        let status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("status for second environment");

        assert_eq!(status.environment_id, second_environment_id);
        assert_eq!(status.summary.expect("placeholder summary should be visible").environment_id, placeholder_summary.environment_id);

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(
            hosts.hosts.iter().any(|entry| entry.environment_id == first_environment_id),
            "the first live environment should remain visible"
        );
        assert!(
            hosts.hosts.iter().any(|entry| entry.environment_id == second_environment_id),
            "the second live environment should be visible even with a placeholder summary"
        );
    }

    #[tokio::test]
    async fn host_entries_are_not_synthesized_from_node_identity_without_environment_mapping() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(hosts.hosts.iter().all(|entry| entry.node.node_id != peer_node().node_id));
    }

    #[tokio::test]
    async fn out_of_order_status_remove_and_reconnect_recover_cached_peer_state() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));
        let first_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        let events = RefCell::new(Vec::new());
        let emit = |event: DaemonEvent| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;
        assert!(events.borrow().is_empty(), "status before mapping should be cached, not emitted");

        registry.publish_peer_summary(first_summary.clone(), &emit).await;
        let first_snapshot_seq = {
            let captured = events.borrow();
            let first_snapshot = captured
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer.node_id => Some(snapshot),
                    _ => None,
                })
                .expect("expected first host snapshot");
            assert_eq!(first_snapshot.environment_id, first_environment_id);
            assert_eq!(first_snapshot.connection_status, PeerConnectionState::Connected);
            first_snapshot.seq
        };
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(first_environment_id.clone()));

        events.borrow_mut().clear();
        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: first_environment_id.clone(), seq: first_snapshot_seq + 1 });
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, None, "removing the host should clear the node mapping");

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;
        assert!(events.borrow().is_empty(), "reconnect before the replacement summary should be cached");

        registry.publish_peer_summary(second_summary.clone(), &emit).await;
        {
            let captured = events.borrow();
            let second_snapshot = captured
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer.node_id => Some(snapshot),
                    _ => None,
                })
                .expect("expected second host snapshot");
            assert_eq!(second_snapshot.environment_id, second_environment_id);
            assert_eq!(second_snapshot.connection_status, PeerConnectionState::Connected);
        }
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(second_environment_id));
    }

    #[tokio::test]
    async fn host_list_retains_multiple_environments_for_the_same_node() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();

        let first_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-a")),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-b")),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(first_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(second_summary.clone(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        let peer_entries: Vec<_> = hosts.hosts.iter().filter(|entry| entry.node.node_id == peer.node_id).collect();

        assert_eq!(peer_entries.len(), 2, "a single node should be able to expose multiple host environments");
        assert!(peer_entries.iter().any(|entry| entry.environment_id == first_summary.environment_id));
        assert!(peer_entries.iter().any(|entry| entry.environment_id == second_summary.environment_id));

        let status_a =
            registry.get_host_status(&first_summary.environment_id, &HashMap::new()).await.expect("status for first environment");
        let status_b =
            registry.get_host_status(&second_summary.environment_id, &HashMap::new()).await.expect("status for second environment");

        assert_eq!(status_a.environment_id, first_summary.environment_id);
        assert_eq!(status_b.environment_id, second_summary.environment_id);
    }

    #[tokio::test]
    async fn peer_connection_status_updates_all_live_environments_for_the_same_node() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: first_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: second_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-b")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let first_status = registry.get_host_status(&first_environment_id, &HashMap::new()).await.expect("first host status");
        let second_status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("second host status");
        assert_eq!(first_status.connection_status, PeerConnectionState::Connected);
        assert_eq!(second_status.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn visible_host_status_changes_emit_live_events_for_existing_hosts() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        let events = RefCell::new(Vec::new());
        let emit = |event: DaemonEvent| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;

        let captured = events.borrow();
        assert!(
            captured.iter().any(|event| matches!(event, DaemonEvent::PeerStatusChanged { node_id, status } if node_id == &peer.node_id && *status == PeerConnectionState::Connected)),
            "a visible host status transition should emit a peer status event"
        );
        let snapshot = captured
            .iter()
            .find_map(|event| match event {
                DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id => Some(snapshot),
                _ => None,
            })
            .expect("a visible host should receive a refreshed snapshot");
        assert_eq!(snapshot.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn host_list_uses_environment_scoped_counts_for_shared_node() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: first_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: second_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-b")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        let counts = HashMap::from([
            (first_environment_id.clone(), HostCounts { repo_count: 1, work_item_count: 2 }),
            (second_environment_id.clone(), HostCounts { repo_count: 3, work_item_count: 5 }),
        ]);
        let hosts = registry.list_hosts(&counts).await;

        let first_entry = hosts.hosts.iter().find(|entry| entry.environment_id == first_environment_id).expect("first entry");
        let second_entry = hosts.hosts.iter().find(|entry| entry.environment_id == second_environment_id).expect("second entry");
        assert_eq!((first_entry.repo_count, first_entry.work_item_count), (1, 2));
        assert_eq!((second_entry.repo_count, second_entry.work_item_count), (3, 5));
    }

    #[tokio::test]
    async fn host_snapshot_replay_does_not_rollback_newer_node_connectivity() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry.apply_event(&DaemonEvent::PeerStatusChanged { node_id: peer.node_id.clone(), status: PeerConnectionState::Connected });

        registry.apply_event(&DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
            seq: 2,
            environment_id: environment_id.clone(),
            node: peer.clone(),
            is_local: false,
            connection_status: PeerConnectionState::Disconnected,
            summary: HostSummary {
                environment_id: environment_id.clone(),
                host_name: Some(HostName::new("peer-host-a")),
                node: peer.clone(),
                system: SystemInfo::default(),
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        })));

        assert_eq!(registry.peer_connection_status(&peer.node_id).await, PeerConnectionState::Connected);
        let status = registry.get_host_status(&environment_id, &HashMap::new()).await.expect("host status");
        assert_eq!(status.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn stale_host_snapshot_from_another_environment_does_not_repoint_canonical_mapping_even_with_higher_seq() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let current_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let stale_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        let current_summary = HostSummary {
            environment_id: current_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
            }],
            environments: vec![],
        };
        registry.publish_peer_summary(current_summary, &|_| {}).await;
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(current_environment_id.clone()));

        let stale_snapshot = HostSnapshot {
            seq: 9,
            environment_id: stale_environment_id.clone(),
            node: peer.clone(),
            is_local: false,
            connection_status: PeerConnectionState::Disconnected,
            summary: HostSummary {
                environment_id: stale_environment_id.clone(),
                host_name: Some(HostName::new("peer-host-b")),
                node: peer.clone(),
                system: SystemInfo::default(),
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        };

        registry.apply_event(&DaemonEvent::HostSnapshot(Box::new(stale_snapshot)));

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(current_environment_id),
            "cross-environment snapshots must not move the canonical node-to-environment mapping based on seq alone"
        );
    }

    #[tokio::test]
    async fn removing_one_environment_keeps_another_live_environment_reachable() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        let first_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(first_summary, &|_| {}).await;
        registry.publish_peer_summary(second_summary, &|_| {}).await;

        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: first_environment_id.clone(), seq: 2 });

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(second_environment_id.clone()),
            "removing one environment should leave another live environment addressable"
        );

        let status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("status for remaining environment");
        assert_eq!(status.environment_id, second_environment_id);
    }

    #[tokio::test]
    async fn removing_the_canonical_environment_reassigns_to_the_most_recent_remaining_environment() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();
        let canonical_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let older_remaining_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));
        let newer_remaining_environment_id = EnvironmentId::host(HostId::new("peer-node-host-c"));

        let canonical_summary = HostSummary {
            environment_id: canonical_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let older_remaining_summary = HostSummary {
            environment_id: older_remaining_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let newer_remaining_summary = HostSummary {
            environment_id: newer_remaining_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-c")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
            }],
            environments: vec![],
        };

        registry.publish_peer_summary(canonical_summary, &|_| {}).await;
        registry.publish_peer_summary(older_remaining_summary, &|_| {}).await;
        registry.publish_peer_summary(newer_remaining_summary.clone(), &|_| {}).await;

        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: canonical_environment_id.clone(), seq: 2 });

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(newer_remaining_environment_id.clone()),
            "the canonical mapping should follow the most recently updated remaining environment"
        );

        let status =
            registry.get_host_status(&newer_remaining_environment_id, &HashMap::new()).await.expect("status for remaining environment");
        assert_eq!(status.environment_id, newer_remaining_environment_id);
        assert!(status.summary.is_some());
    }
}

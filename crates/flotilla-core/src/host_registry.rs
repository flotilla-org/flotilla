use std::collections::{hash_map::Entry, HashMap, HashSet};

use flotilla_protocol::{
    qualified_path::HostId, DaemonEvent, EnvironmentId, HostListEntry, HostListResponse, HostProvidersResponse, HostSnapshot,
    HostStatusResponse, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, SystemInfo, ToolInventory, TopologyResponse,
    TopologyRoute,
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
    connection_status: PeerConnectionState,
    summary: Option<HostSummary>,
    seq: u64,
    removed: bool,
}

pub(crate) struct HostRegistry {
    local_node: NodeInfo,
    hosts: RwLock<HashMap<EnvironmentId, HostState>>,
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
            connection_status: PeerConnectionState::Connected,
            summary: Some(local_host_summary.clone()),
            seq: 1,
            removed: false,
        });
        let mut node_environments = HashMap::new();
        node_environments.insert(local_node.node_id.clone(), local_host_summary.environment_id.clone());
        Self {
            local_node,
            hosts: RwLock::new(hosts),
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
        if state.summary.as_ref() != Some(&summary) {
            state.summary = Some(summary);
            state.seq += 1;
            state.removed = false;
        }
    }

    pub(crate) async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        let node_environments = self.node_environments.read().await;
        let hosts = self.hosts.read().await;
        let environment_id = node_environments.get(node_id).cloned().unwrap_or_else(|| fallback_host_environment_id(node_id));
        hosts
            .get(&environment_id)
            .filter(|state| !state.removed)
            .map(|state| state.connection_status.clone())
            .unwrap_or(PeerConnectionState::Disconnected)
    }

    pub(crate) async fn list_hosts(&self, local_counts: HostCounts, remote_counts: &HashMap<NodeId, HostCounts>) -> HostListResponse {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let hosts = self.hosts.read().await;
        let mut host_entries: Vec<_> = hosts
            .iter()
            .filter(|(_, state)| !state.removed)
            .map(|(_, state)| build_host_list_entry_from_state(&self.local_node, &configured, local_counts, remote_counts, state))
            .collect();

        let active_node_ids: HashSet<_> = host_entries.iter().map(|entry| entry.node.node_id.clone()).collect();
        host_entries.extend(
            known_nodes(&self.local_node.node_id, &configured, &statuses, &summaries, remote_counts)
                .into_iter()
                .filter(|node_id| !active_node_ids.contains(node_id))
                .map(|node_id| {
                    build_host_list_entry(
                        &node_id,
                        &self.local_node,
                        &configured,
                        &statuses,
                        &summaries,
                        &HashMap::new(),
                        local_counts,
                        remote_counts,
                    )
                }),
        );
        host_entries.sort_by(|a, b| {
            b.is_local
                .cmp(&a.is_local)
                .then_with(|| a.node.node_id.cmp(&b.node.node_id))
                .then_with(|| a.environment_id.cmp(&b.environment_id))
        });

        HostListResponse { hosts: host_entries }
    }

    pub(crate) async fn environment_id_for_node(&self, node_id: &NodeId) -> Option<EnvironmentId> {
        self.node_environments.read().await.get(node_id).cloned()
    }

    pub(crate) async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        local_counts: HostCounts,
        remote_counts: &HashMap<NodeId, HostCounts>,
    ) -> Result<HostStatusResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        let summary = state.summary.clone();
        let resolved_node = state.node_id.clone();

        Ok(build_host_status(environment_id, &resolved_node, summary, HostStatusContext {
            local_node: &self.local_node,
            configured: &configured,
            statuses: &statuses,
            summaries: &summaries,
            local_counts,
            remote_counts,
        }))
    }

    pub(crate) async fn get_host_providers(
        &self,
        environment_id: &EnvironmentId,
        _remote_counts: &HashMap<NodeId, HostCounts>,
    ) -> Result<HostProvidersResponse, String> {
        let configured = self.configured_peers.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let hosts = self.hosts.read().await;
        let state = hosts.get(environment_id).ok_or_else(|| format!("host not found: {environment_id}"))?;
        let summary = state.summary.clone().ok_or_else(|| format!("no summary available for host: {environment_id}"))?;

        Ok(build_host_providers(environment_id, &state.node_id, &self.local_node, &configured, &statuses, &summaries, summary))
    }

    pub(crate) async fn get_topology(&self) -> TopologyResponse {
        let routes = self.topology_routes.read().await.clone();
        let configured = self.configured_peers.read().await.clone();
        build_topology(&self.local_node, &routes, &configured)
    }

    pub(crate) async fn replay_host_events(&self, last_seen: &HashMap<StreamKey, u64>) -> Vec<DaemonEvent> {
        let configured = self.configured_peers.read().await.clone();
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
                    &environment_id,
                    state,
                ))));
            }
        }
        events
    }

    async fn active_host_maps(&self) -> (HashMap<NodeId, PeerConnectionState>, HashMap<NodeId, HostSummary>) {
        let hosts = self.hosts.read().await;
        let active: HashMap<_, _> = hosts.iter().filter(|(_, state)| !state.removed).map(|(h, s)| (h.clone(), s.clone())).collect();
        let statuses = active.iter().map(|(_, state)| (state.node_id.clone(), state.connection_status.clone())).collect();
        let summaries =
            active.iter().filter_map(|(_, state)| state.summary.clone().map(|summary| (state.node_id.clone(), summary))).collect();
        (statuses, summaries)
    }

    pub(crate) async fn sync_host_membership(&self, remote_counts: &HashMap<NodeId, HostCounts>, emit: &impl Fn(DaemonEvent)) {
        let configured = self.configured_peers.read().await.clone();
        let mut node_environments = self.node_environments.write().await;
        let mut hosts = self.hosts.write().await;

        for node_id in configured.keys().chain(remote_counts.keys()) {
            if node_id != &self.local_node.node_id {
                let environment_id = current_host_environment_id_for_node(node_id, &node_environments);
                match hosts.entry(environment_id.clone()) {
                    Entry::Vacant(entry) => {
                        let state = entry.insert(HostState {
                            node_id: node_id.clone(),
                            environment_id: environment_id.clone(),
                            connection_status: PeerConnectionState::Disconnected,
                            summary: None,
                            seq: 1,
                            removed: false,
                        });
                        node_environments.insert(node_id.clone(), environment_id.clone());
                        emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(
                            &self.local_node,
                            &configured,
                            &environment_id,
                            state,
                        ))));
                    }
                    Entry::Occupied(mut entry) => {
                        let state = entry.get_mut();
                        state.node_id = node_id.clone();
                        if state.removed {
                            state.removed = false;
                            state.seq += 1;
                            node_environments.insert(node_id.clone(), environment_id.clone());
                            emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(
                                &self.local_node,
                                &configured,
                                &environment_id,
                                state,
                            ))));
                        }
                    }
                }
            }
        }

        let environment_ids: Vec<_> = hosts.keys().cloned().collect();
        for environment_id in environment_ids {
            let Some(state) = hosts.get(&environment_id) else {
                continue;
            };
            if should_present_host_state(&self.local_node.node_id, &configured, remote_counts, &state.node_id, state) {
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
        remote_counts: &HashMap<NodeId, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let snapshot = {
            let configured = self.configured_peers.read().await.clone();
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            update_host_status(&self.local_node, &configured, &mut node_environments, &mut hosts, node, status.clone())
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
            let mut node_environments = self.node_environments.write().await;
            let mut hosts = self.hosts.write().await;
            let node = summary.node.clone();
            update_host_summary(&self.local_node, &configured, &mut node_environments, &mut hosts, &node, summary)
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
                if !summaries.contains_key(&state.node_id) {
                    if let Some(snapshot) = clear_host_summary(&self.local_node, &configured, &mut hosts, &environment_id) {
                        emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                    }
                }
            }
            for (node_id, mut summary) in summaries {
                summary.node.node_id = node_id.clone();
                let node = summary.node.clone();
                if let Some(snapshot) =
                    update_host_summary(&self.local_node, &configured, &mut node_environments, &mut hosts, &node, summary)
                {
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
                    let mut node_environments = self.node_environments.try_write().ok();
                    let node = node_info_for(node_id, &configured, None, None);
                    if let Some(node_environments) = node_environments.as_mut() {
                        let _ = update_host_status(&self.local_node, &configured, node_environments, &mut hosts, &node, status.clone());
                    }
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Ok(mut node_environments) = self.node_environments.try_write() {
                        let state = ensure_host_state(&mut hosts, &mut node_environments, &snap.node, snap.environment_id.clone());
                        if state.seq <= snap.seq {
                            state.environment_id = snap.environment_id.clone();
                            state.connection_status = snap.connection_status.clone();
                            state.summary = Some(snap.summary.clone());
                            state.seq = snap.seq;
                            state.removed = false;
                        }
                    }
                }
            }
            DaemonEvent::HostRemoved { environment_id, seq } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Some(state) = hosts.get_mut(environment_id) {
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

fn default_host_summary(node: &NodeInfo, environment_id: &EnvironmentId) -> HostSummary {
    HostSummary {
        environment_id: environment_id.clone(),
        node: node.clone(),
        system: SystemInfo::default(),
        inventory: ToolInventory::default(),
        providers: vec![],
        environments: vec![],
    }
}

fn fallback_host_environment_id(node_id: &NodeId) -> EnvironmentId {
    EnvironmentId::host(HostId::new(node_id.as_str()))
}

fn current_host_environment_id_for_node(node_id: &NodeId, node_environments: &HashMap<NodeId, EnvironmentId>) -> EnvironmentId {
    node_environments.get(node_id).cloned().unwrap_or_else(|| fallback_host_environment_id(node_id))
}

fn ensure_host_state<'a>(
    hosts: &'a mut HashMap<EnvironmentId, HostState>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    node: &NodeInfo,
    environment_id: EnvironmentId,
) -> &'a mut HostState {
    let node_id = node.node_id.clone();
    if let Some(previous_environment_id) = node_environments.insert(node_id.clone(), environment_id.clone()) {
        if previous_environment_id != environment_id && previous_environment_id == fallback_host_environment_id(&node_id) {
            hosts.remove(&previous_environment_id);
        }
    }

    let state = hosts.entry(environment_id.clone()).or_insert_with(|| HostState {
        node_id: node_id.clone(),
        environment_id: environment_id.clone(),
        connection_status: PeerConnectionState::Disconnected,
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
        connection_status: state.connection_status.clone(),
        summary: state.summary.clone().unwrap_or_else(|| default_host_summary(&node, &state.environment_id)),
    }
}

fn update_host_status(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    status: PeerConnectionState,
) -> Option<HostSnapshot> {
    let environment_id = current_host_environment_id_for_node(&node.node_id, node_environments);
    let state = ensure_host_state(hosts, node_environments, node, environment_id);
    if !state.removed && state.connection_status == status {
        return None;
    }
    if state.summary.is_none() {
        let default_summary = default_host_summary(node, &state.environment_id);
        state.summary = Some(default_summary);
    }
    state.connection_status = status;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, &state.environment_id, state))
}

fn update_host_summary(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    node_environments: &mut HashMap<NodeId, EnvironmentId>,
    hosts: &mut HashMap<EnvironmentId, HostState>,
    node: &NodeInfo,
    summary: HostSummary,
) -> Option<HostSnapshot> {
    let state = ensure_host_state(hosts, node_environments, node, summary.environment_id.clone());
    if summary_is_overlay_placeholder(&summary) && state.summary.as_ref().is_some_and(|existing| !summary_is_overlay_placeholder(existing))
    {
        return None;
    }
    if !state.removed && state.summary.as_ref() == Some(&summary) {
        return None;
    }
    state.environment_id = summary.environment_id.clone();
    state.summary = Some(summary);
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_node, configured, &state.environment_id, state))
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
    Some(build_host_snapshot(local_node, configured, environment_id, state))
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

fn mark_host_removed(hosts: &mut HashMap<EnvironmentId, HostState>, environment_id: &EnvironmentId) -> Option<u64> {
    let state = hosts.get_mut(environment_id)?;
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
    host_states: &HashMap<NodeId, HostState>,
    local_counts: HostCounts,
    remote_counts: &HashMap<NodeId, HostCounts>,
) -> HostListEntry {
    let is_local = node_id == &local_node.node_id;
    let counts = if is_local { local_counts } else { remote_counts.get(node_id).copied().unwrap_or_default() };
    let environment_id = host_states
        .get(node_id)
        .map(|state| state.environment_id.clone())
        .or_else(|| summaries.get(node_id).map(|summary| summary.environment_id.clone()))
        .unwrap_or_else(|| fallback_host_environment_id(node_id));

    HostListEntry {
        environment_id,
        node: node_info_for(node_id, configured, Some(summaries), Some(local_node)),
        is_local,
        configured: !is_local && configured.contains_key(node_id),
        connection_status: connection_status(node_id, &local_node.node_id, statuses),
        has_summary: is_local || summaries.contains_key(node_id),
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_list_entry_from_state(
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    local_counts: HostCounts,
    remote_counts: &HashMap<NodeId, HostCounts>,
    state: &HostState,
) -> HostListEntry {
    let is_local = state.node_id == local_node.node_id;
    let counts = if is_local { local_counts } else { remote_counts.get(&state.node_id).copied().unwrap_or_default() };
    let node = state
        .summary
        .as_ref()
        .map(|summary| summary.node.clone())
        .unwrap_or_else(|| node_info_for(&state.node_id, configured, None, Some(local_node)));

    HostListEntry {
        environment_id: state.environment_id.clone(),
        node,
        is_local,
        configured: !is_local && configured.contains_key(&state.node_id),
        connection_status: state.connection_status.clone(),
        has_summary: state.summary.is_some(),
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

struct HostStatusContext<'a> {
    local_node: &'a NodeInfo,
    configured: &'a HashMap<NodeId, String>,
    statuses: &'a HashMap<NodeId, PeerConnectionState>,
    summaries: &'a HashMap<NodeId, HostSummary>,
    local_counts: HostCounts,
    remote_counts: &'a HashMap<NodeId, HostCounts>,
}

fn build_host_status(
    environment_id: &EnvironmentId,
    node_id: &NodeId,
    summary: Option<HostSummary>,
    ctx: HostStatusContext<'_>,
) -> HostStatusResponse {
    let is_local = node_id == &ctx.local_node.node_id;
    let counts = if is_local { ctx.local_counts } else { ctx.remote_counts.get(node_id).copied().unwrap_or_default() };

    HostStatusResponse {
        environment_id: environment_id.clone(),
        node: node_info_for(node_id, ctx.configured, Some(ctx.summaries), Some(ctx.local_node)),
        is_local,
        configured: !is_local && ctx.configured.contains_key(node_id),
        connection_status: connection_status(node_id, &ctx.local_node.node_id, ctx.statuses),
        summary,
        visible_environments: vec![],
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_providers(
    environment_id: &EnvironmentId,
    node_id: &NodeId,
    local_node: &NodeInfo,
    configured: &HashMap<NodeId, String>,
    statuses: &HashMap<NodeId, PeerConnectionState>,
    summaries: &HashMap<NodeId, HostSummary>,
    summary: HostSummary,
) -> HostProvidersResponse {
    HostProvidersResponse {
        environment_id: environment_id.clone(),
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

    use flotilla_protocol::{
        qualified_path::HostId, DaemonEvent, EnvironmentId, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, SystemInfo,
        ToolInventory,
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

    #[tokio::test]
    async fn overlay_placeholder_does_not_clobber_existing_real_summary() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = NodeInfo::new(NodeId::new("peer-node-42"), "Build Box");
        let real_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-42-host")),
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

        let status =
            registry.get_host_status(&real_summary.environment_id, HostCounts::default(), &HashMap::new()).await.expect("host status");
        let summary = status.summary.expect("summary should remain available");
        assert_eq!(summary.node.display_name, "Build Box");
        assert_eq!(summary.providers, real_summary.providers);
        assert_eq!(summary.system, real_summary.system);
    }

    #[tokio::test]
    async fn host_entries_without_summary_use_host_environment_identity() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let hosts = registry.list_hosts(HostCounts::default(), &HashMap::new()).await;
        let peer = hosts.hosts.into_iter().find(|entry| entry.node.node_id == peer_node().node_id).expect("peer entry");

        assert_eq!(peer.environment_id, EnvironmentId::host(HostId::new("peer-node")));
        assert!(peer.environment_id.is_host());
    }

    #[tokio::test]
    async fn host_list_retains_multiple_environments_for_the_same_node() {
        let registry = HostRegistry::new(local_node(), minimal_summary(&local_node()));
        let peer = peer_node();

        let first_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(first_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(second_summary.clone(), &|_| {}).await;

        let hosts = registry.list_hosts(HostCounts::default(), &HashMap::new()).await;
        let peer_entries: Vec<_> = hosts.hosts.iter().filter(|entry| entry.node.node_id == peer.node_id).collect();

        assert_eq!(peer_entries.len(), 2, "a single node should be able to expose multiple host environments");
        assert!(peer_entries.iter().any(|entry| entry.environment_id == first_summary.environment_id));
        assert!(peer_entries.iter().any(|entry| entry.environment_id == second_summary.environment_id));

        let status_a = registry
            .get_host_status(&first_summary.environment_id, HostCounts::default(), &HashMap::new())
            .await
            .expect("status for first environment");
        let status_b = registry
            .get_host_status(&second_summary.environment_id, HostCounts::default(), &HashMap::new())
            .await
            .expect("status for second environment");

        assert_eq!(status_a.environment_id, first_summary.environment_id);
        assert_eq!(status_b.environment_id, second_summary.environment_id);
    }
}

use std::collections::{hash_map::Entry, HashMap, HashSet};

use flotilla_protocol::{
    DaemonEvent, HostListEntry, HostListResponse, HostProvidersResponse, HostSnapshot, HostStatusResponse, HostSummary,
    PeerConnectionState, StreamKey, SystemInfo, ToolInventory, TopologyResponse, TopologyRoute,
};
use tokio::sync::RwLock;

use crate::HostName;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// HostRegistry
// ---------------------------------------------------------------------------

pub(crate) struct HostRegistry {
    host_name: HostName,
    hosts: RwLock<HashMap<HostName, HostState>>,
    configured_peer_names: RwLock<HashSet<HostName>>,
    topology_routes: RwLock<Vec<TopologyRoute>>,
    /// Static snapshot of the local host's summary, computed once at startup.
    /// Not updated at runtime — provider health changes are reflected in
    /// per-repo snapshots, not in the host-level summary.
    local_host_summary: HostSummary,
}

impl HostRegistry {
    pub(crate) fn new(host_name: HostName, local_host_summary: HostSummary) -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(host_name.clone(), HostState {
            connection_status: PeerConnectionState::Connected,
            summary: Some(local_host_summary.clone()),
            seq: 1,
            removed: false,
        });
        Self {
            host_name,
            hosts: RwLock::new(hosts),
            configured_peer_names: RwLock::new(HashSet::new()),
            topology_routes: RwLock::new(Vec::new()),
            local_host_summary,
        }
    }

    pub(crate) fn local_host_summary(&self) -> &HostSummary {
        &self.local_host_summary
    }

    // -----------------------------------------------------------------------
    // Query methods
    // -----------------------------------------------------------------------

    /// Returns the current connection status for a peer host.
    pub(crate) async fn peer_connection_status(&self, host: &HostName) -> PeerConnectionState {
        self.hosts
            .read()
            .await
            .get(host)
            .filter(|state| !state.removed)
            .map(|state| state.connection_status.clone())
            .unwrap_or(PeerConnectionState::Disconnected)
    }

    pub(crate) async fn list_hosts(&self, local_counts: HostCounts, remote_counts: &HashMap<HostName, HostCounts>) -> HostListResponse {
        let configured = self.configured_peer_names.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;

        let hosts = known_hosts(&self.host_name, &configured, &statuses, &summaries, remote_counts)
            .into_iter()
            .map(|host| build_host_list_entry(&host, &self.host_name, &configured, &statuses, &summaries, local_counts, remote_counts))
            .collect();

        HostListResponse { hosts }
    }

    pub(crate) async fn get_host_status(
        &self,
        host: &str,
        local_counts: HostCounts,
        remote_counts: &HashMap<HostName, HostCounts>,
    ) -> Result<HostStatusResponse, String> {
        let configured = self.configured_peer_names.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let known = known_hosts(&self.host_name, &configured, &statuses, &summaries, remote_counts);
        let resolved = known.into_iter().find(|candidate| candidate.as_str() == host).ok_or_else(|| format!("host not found: {host}"))?;
        let summary = if resolved == self.host_name { Some(self.local_host_summary.clone()) } else { summaries.get(&resolved).cloned() };

        Ok(build_host_status(&resolved, &self.host_name, &configured, &statuses, summary, local_counts, remote_counts))
    }

    pub(crate) async fn get_host_providers(
        &self,
        host: &str,
        remote_counts: &HashMap<HostName, HostCounts>,
    ) -> Result<HostProvidersResponse, String> {
        let configured = self.configured_peer_names.read().await.clone();
        let (statuses, summaries) = self.active_host_maps().await;
        let known = known_hosts(&self.host_name, &configured, &statuses, &summaries, remote_counts);
        let resolved = known.into_iter().find(|candidate| candidate.as_str() == host).ok_or_else(|| format!("host not found: {host}"))?;
        let summary = if resolved == self.host_name {
            self.local_host_summary.clone()
        } else {
            summaries.get(&resolved).cloned().ok_or_else(|| format!("no summary available for host: {host}"))?
        };

        Ok(build_host_providers(&resolved, &self.host_name, &configured, &statuses, summary))
    }

    pub(crate) async fn get_topology(&self) -> TopologyResponse {
        let routes = self.topology_routes.read().await.clone();
        let configured = self.configured_peer_names.read().await.clone();
        build_topology(&self.host_name, &routes, &configured)
    }

    pub(crate) async fn replay_host_events(&self, last_seen: &HashMap<StreamKey, u64>) -> Vec<DaemonEvent> {
        let mut events = Vec::new();
        for (host_name, state) in self.hosts.read().await.iter() {
            let stream_key = StreamKey::Host { host_name: host_name.clone() };
            let up_to_date = last_seen.get(&stream_key).is_some_and(|seq| *seq == state.seq);
            if up_to_date {
                continue;
            }
            if state.removed {
                if last_seen.contains_key(&stream_key) {
                    events.push(DaemonEvent::HostRemoved { host: host_name.clone(), seq: state.seq });
                }
            } else {
                events.push(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.host_name, host_name, state))));
            }
        }
        events
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Extracts connection-status and summary maps from `hosts`, filtering out removed entries.
    async fn active_host_maps(&self) -> (HashMap<HostName, PeerConnectionState>, HashMap<HostName, HostSummary>) {
        let hosts = self.hosts.read().await;
        let active: HashMap<_, _> = hosts.iter().filter(|(_, state)| !state.removed).map(|(h, s)| (h.clone(), s.clone())).collect();
        let statuses = active.iter().map(|(host, state)| (host.clone(), state.connection_status.clone())).collect();
        let summaries = active.iter().filter_map(|(host, state)| state.summary.clone().map(|summary| (host.clone(), summary))).collect();
        (statuses, summaries)
    }

    // -----------------------------------------------------------------------
    // Mutation methods
    // -----------------------------------------------------------------------

    /// Reconcile host membership: ensure configured peers and hosts with
    /// remote repo counts are present; remove hosts that no longer qualify.
    ///
    /// Takes `remote_counts` as a parameter (the caller owns the repo data)
    /// and emits events via the `emit` closure.
    pub(crate) async fn sync_host_membership(&self, remote_counts: &HashMap<HostName, HostCounts>, emit: &impl Fn(DaemonEvent)) {
        let configured = self.configured_peer_names.read().await.clone();
        let mut hosts = self.hosts.write().await;

        for host_name in configured.iter().chain(remote_counts.keys()) {
            if host_name != &self.host_name {
                match hosts.entry(host_name.clone()) {
                    Entry::Vacant(entry) => {
                        let state = entry.insert(HostState {
                            connection_status: PeerConnectionState::Disconnected,
                            summary: None,
                            seq: 1,
                            removed: false,
                        });
                        emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.host_name, host_name, state))));
                    }
                    Entry::Occupied(mut entry) => {
                        let state = entry.get_mut();
                        if state.removed {
                            state.removed = false;
                            state.seq += 1;
                            emit(DaemonEvent::HostSnapshot(Box::new(build_host_snapshot(&self.host_name, host_name, state))));
                        }
                    }
                }
            }
        }

        let host_names: Vec<_> = hosts.keys().cloned().collect();
        for host_name in host_names {
            let Some(state) = hosts.get(&host_name) else {
                continue;
            };
            if should_present_host_state(&self.host_name, &configured, remote_counts, &host_name, state) {
                continue;
            }
            if let Some(seq) = mark_host_removed(&mut hosts, &host_name) {
                emit(DaemonEvent::HostRemoved { host: host_name, seq });
            }
        }
    }

    /// Publish a peer connection status change, emitting `PeerStatusChanged`
    /// and `HostSnapshot` events, then reconciling membership.
    pub(crate) async fn publish_peer_connection_status(
        &self,
        host: &HostName,
        status: PeerConnectionState,
        remote_counts: &HashMap<HostName, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let snapshot = {
            let mut hosts = self.hosts.write().await;
            update_host_status(&self.host_name, &mut hosts, host, status.clone())
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::PeerStatusChanged { host: host.clone(), status });
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
        self.sync_host_membership(remote_counts, emit).await;
    }

    /// Publish a peer host summary update. Normalizes the `host_name` field
    /// and emits a `HostSnapshot` if the summary changed. Does NOT call
    /// `sync_host_membership` (matches current behavior).
    pub(crate) async fn publish_peer_summary(&self, host: &HostName, summary: HostSummary, emit: &impl Fn(DaemonEvent)) {
        let mut summary = summary;
        summary.host_name = host.clone();
        let snapshot = {
            let mut hosts = self.hosts.write().await;
            update_host_summary(&self.host_name, &mut hosts, host, summary)
        };
        if let Some(snapshot) = snapshot {
            emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
        }
    }

    /// Update the set of configured peer names, then reconcile membership.
    pub(crate) async fn set_configured_peer_names(
        &self,
        peers: Vec<HostName>,
        remote_counts: &HashMap<HostName, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let mut configured = self.configured_peer_names.write().await;
        *configured = peers.iter().cloned().collect();
        drop(configured);

        self.sync_host_membership(remote_counts, emit).await;
    }

    /// Replace the peer host summaries map. Clears summaries for peers no
    /// longer present, updates/adds summaries for peers in the new map, then
    /// reconciles membership.
    pub(crate) async fn set_peer_host_summaries(
        &self,
        summaries: HashMap<HostName, HostSummary>,
        remote_counts: &HashMap<HostName, HostCounts>,
        emit: &impl Fn(DaemonEvent),
    ) {
        let mut normalized = HashMap::new();
        for (host_name, mut summary) in summaries {
            summary.host_name = host_name.clone();
            normalized.insert(host_name, summary);
        }

        {
            let mut hosts = self.hosts.write().await;
            let host_names: Vec<_> = hosts.keys().cloned().collect();
            for host_name in host_names {
                if !normalized.contains_key(&host_name) {
                    if let Some(snapshot) = clear_host_summary(&self.host_name, &mut hosts, &host_name) {
                        emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                    }
                }
            }
            for (host_name, summary) in normalized {
                if let Some(snapshot) = update_host_summary(&self.host_name, &mut hosts, &host_name, summary) {
                    emit(DaemonEvent::HostSnapshot(Box::new(snapshot)));
                }
            }
        }

        self.sync_host_membership(remote_counts, emit).await;
    }

    /// Replace the topology routes. Sorts defensively for stable output.
    pub(crate) async fn set_topology_routes(&self, mut routes: Vec<TopologyRoute>) {
        routes.sort_by(|a, b| a.target.cmp(&b.target));
        let mut stored = self.topology_routes.write().await;
        *stored = routes;
    }

    /// Mirror host-related events into internal state. Best-effort: if the
    /// lock is contended the update is silently skipped. Must be synchronous
    /// (`fn`, not `async fn`) because it is called from synchronous contexts.
    pub(crate) fn apply_event(&self, event: &DaemonEvent) {
        match event {
            DaemonEvent::PeerStatusChanged { host, status } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    let _ = update_host_status(&self.host_name, &mut hosts, host, status.clone());
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    let mut summary = snap.summary.clone();
                    summary.host_name = snap.host_name.clone();
                    match hosts.get_mut(&snap.host_name) {
                        Some(state) if state.seq == snap.seq => {
                            // Same seq: idempotent re-apply of connection status only.
                            // Summary is unchanged — same seq means same snapshot content.
                            state.connection_status = snap.connection_status.clone();
                            state.removed = false;
                        }
                        Some(state) if state.seq < snap.seq => {
                            *state = HostState {
                                connection_status: snap.connection_status.clone(),
                                summary: Some(summary),
                                seq: snap.seq,
                                removed: false,
                            };
                        }
                        None => {
                            hosts.insert(snap.host_name.clone(), HostState {
                                connection_status: snap.connection_status.clone(),
                                summary: Some(summary),
                                seq: snap.seq,
                                removed: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            DaemonEvent::HostRemoved { host, seq } => {
                if let Ok(mut hosts) = self.hosts.try_write() {
                    if let Some(state) = hosts.get_mut(host) {
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

// ---------------------------------------------------------------------------
// Free functions (from in_process.rs)
// ---------------------------------------------------------------------------

fn default_host_summary(host_name: &HostName) -> HostSummary {
    HostSummary { host_name: host_name.clone(), system: SystemInfo::default(), inventory: ToolInventory::default(), providers: vec![] }
}

fn ensure_remote_host_state<'a>(hosts: &'a mut HashMap<HostName, HostState>, host_name: &HostName) -> &'a mut HostState {
    hosts.entry(host_name.clone()).or_insert_with(|| HostState {
        connection_status: PeerConnectionState::Disconnected,
        summary: None,
        seq: 1,
        removed: false,
    })
}

fn build_host_snapshot(local_host: &HostName, host_name: &HostName, state: &HostState) -> HostSnapshot {
    debug_assert!(!state.removed, "removed hosts should not be materialized as snapshots");
    HostSnapshot {
        seq: state.seq,
        host_name: host_name.clone(),
        is_local: *host_name == *local_host,
        connection_status: state.connection_status.clone(),
        summary: state.summary.clone().unwrap_or_else(|| default_host_summary(host_name)),
    }
}

fn update_host_status(
    local_host: &HostName,
    hosts: &mut HashMap<HostName, HostState>,
    host_name: &HostName,
    status: PeerConnectionState,
) -> Option<HostSnapshot> {
    let state = ensure_remote_host_state(hosts, host_name);
    if !state.removed && state.connection_status == status {
        return None;
    }
    state.connection_status = status;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_host, host_name, state))
}

fn update_host_summary(
    local_host: &HostName,
    hosts: &mut HashMap<HostName, HostState>,
    host_name: &HostName,
    summary: HostSummary,
) -> Option<HostSnapshot> {
    let state = ensure_remote_host_state(hosts, host_name);
    if !state.removed && state.summary.as_ref() == Some(&summary) {
        return None;
    }
    state.summary = Some(summary);
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_host, host_name, state))
}

fn clear_host_summary(local_host: &HostName, hosts: &mut HashMap<HostName, HostState>, host_name: &HostName) -> Option<HostSnapshot> {
    if host_name == local_host {
        return None;
    }
    let state = hosts.get_mut(host_name)?;
    state.summary.as_ref()?;
    state.summary = None;
    state.removed = false;
    state.seq += 1;
    Some(build_host_snapshot(local_host, host_name, state))
}

fn should_present_host_state(
    local_host: &HostName,
    configured: &HashSet<HostName>,
    remote_counts: &HashMap<HostName, HostCounts>,
    host_name: &HostName,
    state: &HostState,
) -> bool {
    host_name == local_host
        || configured.contains(host_name)
        || state.connection_status != PeerConnectionState::Disconnected
        || state.summary.is_some()
        || remote_counts.contains_key(host_name)
}

fn mark_host_removed(hosts: &mut HashMap<HostName, HostState>, host_name: &HostName) -> Option<u64> {
    let state = hosts.get_mut(host_name)?;
    if state.removed {
        return None;
    }
    state.connection_status = PeerConnectionState::Disconnected;
    state.summary = None;
    state.removed = true;
    state.seq += 1;
    Some(state.seq)
}

// ---------------------------------------------------------------------------
// Query functions (from host_queries.rs)
// ---------------------------------------------------------------------------

fn known_hosts(
    local_host: &HostName,
    configured: &HashSet<HostName>,
    statuses: &HashMap<HostName, PeerConnectionState>,
    summaries: &HashMap<HostName, HostSummary>,
    remote_counts: &HashMap<HostName, HostCounts>,
) -> Vec<HostName> {
    let mut hosts = HashSet::from([local_host.clone()]);
    hosts.extend(configured.iter().cloned());
    hosts.extend(statuses.keys().cloned());
    hosts.extend(summaries.keys().cloned());
    hosts.extend(remote_counts.keys().cloned());

    let mut hosts: Vec<_> = hosts.into_iter().collect();
    hosts.sort_by(|a, b| {
        let a_local = a == local_host;
        let b_local = b == local_host;
        b_local.cmp(&a_local).then_with(|| a.cmp(b))
    });
    hosts
}

fn connection_status(host: &HostName, local_host: &HostName, statuses: &HashMap<HostName, PeerConnectionState>) -> PeerConnectionState {
    if host == local_host {
        PeerConnectionState::Connected
    } else {
        statuses.get(host).cloned().unwrap_or(PeerConnectionState::Disconnected)
    }
}

fn build_host_list_entry(
    host: &HostName,
    local_host: &HostName,
    configured: &HashSet<HostName>,
    statuses: &HashMap<HostName, PeerConnectionState>,
    summaries: &HashMap<HostName, HostSummary>,
    local_counts: HostCounts,
    remote_counts: &HashMap<HostName, HostCounts>,
) -> HostListEntry {
    let is_local = host == local_host;
    let counts = if is_local { local_counts } else { remote_counts.get(host).copied().unwrap_or_default() };

    HostListEntry {
        host: host.clone(),
        is_local,
        configured: !is_local && configured.contains(host),
        connection_status: connection_status(host, local_host, statuses),
        has_summary: is_local || summaries.contains_key(host),
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_status(
    host: &HostName,
    local_host: &HostName,
    configured: &HashSet<HostName>,
    statuses: &HashMap<HostName, PeerConnectionState>,
    summary: Option<HostSummary>,
    local_counts: HostCounts,
    remote_counts: &HashMap<HostName, HostCounts>,
) -> HostStatusResponse {
    let is_local = host == local_host;
    let counts = if is_local { local_counts } else { remote_counts.get(host).copied().unwrap_or_default() };

    HostStatusResponse {
        host: host.clone(),
        is_local,
        configured: !is_local && configured.contains(host),
        connection_status: connection_status(host, local_host, statuses),
        summary,
        repo_count: counts.repo_count,
        work_item_count: counts.work_item_count,
    }
}

fn build_host_providers(
    host: &HostName,
    local_host: &HostName,
    configured: &HashSet<HostName>,
    statuses: &HashMap<HostName, PeerConnectionState>,
    summary: HostSummary,
) -> HostProvidersResponse {
    HostProvidersResponse {
        host: host.clone(),
        is_local: host == local_host,
        configured: host != local_host && configured.contains(host),
        connection_status: connection_status(host, local_host, statuses),
        summary,
    }
}

fn build_topology(local_host: &HostName, routes: &[TopologyRoute], configured_peers: &HashSet<HostName>) -> TopologyResponse {
    let mut all_routes = routes.to_vec();

    // Include configured peers that have no routes (never connected).
    // `direct: true` is a placeholder — no relay is known yet.
    // Clients should treat `direct` as meaningless when `connected` is false.
    for peer in configured_peers {
        if peer == local_host {
            continue;
        }
        if !routes.iter().any(|r| r.target == *peer) {
            all_routes.push(TopologyRoute {
                target: peer.clone(),
                next_hop: peer.clone(),
                direct: true,
                connected: false,
                fallbacks: vec![],
            });
        }
    }

    all_routes.sort_by(|a, b| a.target.cmp(&b.target));
    TopologyResponse { local_host: local_host.clone(), routes: all_routes }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use flotilla_protocol::{DaemonEvent, HostSnapshot, HostSummary, PeerConnectionState, StreamKey, SystemInfo, ToolInventory};

    use super::HostRegistry;
    use crate::HostName;

    fn local_name() -> HostName {
        HostName::new("local-host")
    }

    fn peer_name() -> HostName {
        HostName::new("peer-host")
    }

    fn minimal_summary(name: &HostName) -> HostSummary {
        HostSummary { host_name: name.clone(), system: SystemInfo::default(), inventory: ToolInventory::default(), providers: vec![] }
    }

    fn make_registry() -> HostRegistry {
        HostRegistry::new(local_name(), minimal_summary(&local_name()))
    }

    // -----------------------------------------------------------------------
    // 1. Constructor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn new_initializes_local_host_as_connected() {
        let registry = make_registry();
        let status = registry.peer_connection_status(&local_name()).await;
        assert_eq!(status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn new_returns_disconnected_for_unknown_host() {
        let registry = make_registry();
        let status = registry.peer_connection_status(&peer_name()).await;
        assert_eq!(status, PeerConnectionState::Disconnected);
    }

    // -----------------------------------------------------------------------
    // 2. publish_peer_connection_status
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn publish_peer_connection_status_emits_events_and_returns_snapshot() {
        let registry = make_registry();
        let remote_counts = HashMap::new();

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &emit).await;

        let captured = events.borrow();
        let has_peer_status = captured.iter().any(|e| {
            matches!(e, DaemonEvent::PeerStatusChanged { host, status }
            if *host == peer_name() && *status == PeerConnectionState::Connected)
        });
        assert!(has_peer_status, "should emit PeerStatusChanged");

        let snapshot = captured.iter().find_map(|e| match e {
            DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_name() => Some(snap),
            _ => None,
        });
        let snapshot = snapshot.expect("should emit HostSnapshot");
        assert_eq!(snapshot.host_name, peer_name());
        assert_eq!(snapshot.connection_status, PeerConnectionState::Connected);
        assert!(!snapshot.is_local);
    }

    #[tokio::test]
    async fn publish_peer_connection_status_noop_on_same_status() {
        let registry = make_registry();
        let remote_counts = HashMap::new();

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        // First publish: establishes the status.
        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &emit).await;
        events.borrow_mut().clear();

        // Second publish with the same status: should be a no-op.
        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &emit).await;
        // Only sync_host_membership events may appear — no PeerStatusChanged or new HostSnapshot.
        let has_status_change = events.borrow().iter().any(|e| matches!(e, DaemonEvent::PeerStatusChanged { .. }));
        assert!(!has_status_change, "duplicate status should not emit PeerStatusChanged");
    }

    // -----------------------------------------------------------------------
    // 3. publish_peer_summary
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn publish_peer_summary_emits_host_snapshot() {
        let registry = make_registry();
        let summary = minimal_summary(&peer_name());

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        registry.publish_peer_summary(&peer_name(), summary.clone(), &emit).await;

        let captured = events.borrow();
        let snapshot = captured.iter().find_map(|e| match e {
            DaemonEvent::HostSnapshot(snap) if snap.host_name == peer_name() => Some(snap),
            _ => None,
        });
        let snapshot = snapshot.expect("should emit HostSnapshot");
        assert_eq!(snapshot.host_name, peer_name());
        assert_eq!(snapshot.summary, summary);
    }

    #[tokio::test]
    async fn publish_peer_summary_noop_on_identical_summary() {
        let registry = make_registry();
        let summary = minimal_summary(&peer_name());

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        registry.publish_peer_summary(&peer_name(), summary.clone(), &emit).await;
        events.borrow_mut().clear();

        registry.publish_peer_summary(&peer_name(), summary, &emit).await;
        assert!(events.borrow().is_empty(), "no events should be emitted for identical summary");
    }

    // -----------------------------------------------------------------------
    // 4. set_configured_peer_names
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_configured_peer_names_emits_host_snapshots_for_new_peers() {
        let registry = make_registry();
        let remote_counts = HashMap::new();
        let peer_a = HostName::new("peer-a");
        let peer_b = HostName::new("peer-b");

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        registry.set_configured_peer_names(vec![peer_a.clone(), peer_b.clone()], &remote_counts, &emit).await;

        let captured = events.borrow();
        let snapshot_hosts: Vec<_> = captured
            .iter()
            .filter_map(|e| match e {
                DaemonEvent::HostSnapshot(snap) => Some(snap.host_name.clone()),
                _ => None,
            })
            .collect();
        assert!(snapshot_hosts.contains(&peer_a), "should emit snapshot for peer-a");
        assert!(snapshot_hosts.contains(&peer_b), "should emit snapshot for peer-b");
    }

    #[tokio::test]
    async fn set_configured_peer_names_to_empty_emits_host_removed() {
        let registry = make_registry();
        let remote_counts = HashMap::new();
        let peer_a = HostName::new("peer-a");

        let events = RefCell::new(Vec::new());
        let emit = |e: DaemonEvent| events.borrow_mut().push(e);

        // First, add a configured peer.
        registry.set_configured_peer_names(vec![peer_a.clone()], &remote_counts, &emit).await;
        events.borrow_mut().clear();

        // Now remove all configured peers.
        registry.set_configured_peer_names(vec![], &remote_counts, &emit).await;

        let captured = events.borrow();
        let has_removed = captured.iter().any(|e| matches!(e, DaemonEvent::HostRemoved { host, .. } if *host == peer_a));
        assert!(has_removed, "should emit HostRemoved for peer-a when unconfigured");
    }

    // -----------------------------------------------------------------------
    // 5. apply_event
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_event_peer_status_changed() {
        let registry = make_registry();

        let event = DaemonEvent::PeerStatusChanged { host: peer_name(), status: PeerConnectionState::Connected };
        registry.apply_event(&event);

        let status = registry.peer_connection_status(&peer_name()).await;
        assert_eq!(status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn apply_event_host_snapshot() {
        let registry = make_registry();

        let snapshot = HostSnapshot {
            seq: 5,
            host_name: peer_name(),
            is_local: false,
            connection_status: PeerConnectionState::Connected,
            summary: minimal_summary(&peer_name()),
        };
        registry.apply_event(&DaemonEvent::HostSnapshot(Box::new(snapshot)));

        let status = registry.peer_connection_status(&peer_name()).await;
        assert_eq!(status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn apply_event_host_removed() {
        let registry = make_registry();

        // First, establish the peer as connected.
        let connect_event = DaemonEvent::PeerStatusChanged { host: peer_name(), status: PeerConnectionState::Connected };
        registry.apply_event(&connect_event);
        assert_eq!(registry.peer_connection_status(&peer_name()).await, PeerConnectionState::Connected);

        // Now remove the host.
        let remove_event = DaemonEvent::HostRemoved { host: peer_name(), seq: 100 };
        registry.apply_event(&remove_event);

        let status = registry.peer_connection_status(&peer_name()).await;
        assert_eq!(status, PeerConnectionState::Disconnected, "removed host should appear disconnected");
    }

    // -----------------------------------------------------------------------
    // 6. replay_host_events
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn replay_host_events_empty_last_seen_returns_all() {
        let registry = make_registry();
        let remote_counts = HashMap::new();

        // Add a peer so there's more than just the local host.
        let noop_emit = |_: DaemonEvent| {};
        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &noop_emit).await;

        let events = registry.replay_host_events(&HashMap::new()).await;

        let hosts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DaemonEvent::HostSnapshot(snap) => Some(snap.host_name.clone()),
                _ => None,
            })
            .collect();
        assert!(hosts.contains(&local_name()), "should include local host");
        assert!(hosts.contains(&peer_name()), "should include peer host");
    }

    #[tokio::test]
    async fn replay_host_events_current_seqs_returns_nothing() {
        let registry = make_registry();
        let remote_counts = HashMap::new();
        let noop_emit = |_: DaemonEvent| {};

        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &noop_emit).await;

        // First replay to discover current seqs.
        let initial = registry.replay_host_events(&HashMap::new()).await;
        let mut last_seen = HashMap::new();
        for event in &initial {
            if let DaemonEvent::HostSnapshot(snap) = event {
                let key = StreamKey::Host { host_name: snap.host_name.clone() };
                last_seen.insert(key, snap.seq);
            }
        }

        // Replay with current seqs — should return nothing.
        let events = registry.replay_host_events(&last_seen).await;
        assert!(events.is_empty(), "up-to-date replay should return no events");
    }

    #[tokio::test]
    async fn replay_host_events_stale_seq_returns_updated_snapshot() {
        let registry = make_registry();
        let remote_counts = HashMap::new();
        let noop_emit = |_: DaemonEvent| {};

        registry.publish_peer_connection_status(&peer_name(), PeerConnectionState::Connected, &remote_counts, &noop_emit).await;

        // Capture current state.
        let initial = registry.replay_host_events(&HashMap::new()).await;
        let mut last_seen = HashMap::new();
        for event in &initial {
            if let DaemonEvent::HostSnapshot(snap) = event {
                let key = StreamKey::Host { host_name: snap.host_name.clone() };
                last_seen.insert(key, snap.seq);
            }
        }

        // Update the peer — this bumps its seq.
        let new_summary = HostSummary {
            host_name: peer_name(),
            system: SystemInfo { os: Some("linux".into()), ..Default::default() },
            inventory: ToolInventory::default(),
            providers: vec![],
        };
        registry.publish_peer_summary(&peer_name(), new_summary, &noop_emit).await;

        // Replay with stale seq — should return the updated snapshot for the peer.
        let events = registry.replay_host_events(&last_seen).await;
        let updated_hosts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DaemonEvent::HostSnapshot(snap) => Some(snap.host_name.clone()),
                _ => None,
            })
            .collect();
        assert!(updated_hosts.contains(&peer_name()), "stale peer should be replayed");
        assert!(!updated_hosts.contains(&local_name()), "local host with current seq should not be replayed");
    }
}

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{
    HostListEntry, HostProvidersResponse, HostStatusResponse, HostSummary, PeerConnectionState, TopologyResponse, TopologyRoute,
};

use crate::HostName;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCounts {
    pub(crate) repo_count: usize,
    pub(crate) work_item_count: usize,
}

pub(crate) fn known_hosts(
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

pub(crate) fn connection_status(
    host: &HostName,
    local_host: &HostName,
    statuses: &HashMap<HostName, PeerConnectionState>,
) -> PeerConnectionState {
    if host == local_host {
        PeerConnectionState::Connected
    } else {
        statuses.get(host).cloned().unwrap_or(PeerConnectionState::Disconnected)
    }
}

pub(crate) fn build_host_list_entry(
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

pub(crate) fn build_host_status(
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

pub(crate) fn build_host_providers(
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

pub(crate) fn build_topology(local_host: &HostName, routes: &[TopologyRoute]) -> TopologyResponse {
    TopologyResponse { local_host: local_host.clone(), routes: routes.to_vec() }
}

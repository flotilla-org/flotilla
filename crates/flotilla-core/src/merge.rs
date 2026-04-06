use flotilla_protocol::{HostName, NodeId, NodeInfo, ProviderData};

/// Merge local ProviderData with peer data from remote nodes.
///
/// Display labels remain in `host_name`-style fields for UI continuity, but the
/// peer overlay itself is keyed by `NodeId` and all namespacing uses node ids.
pub fn merge_provider_data(
    local: &ProviderData,
    local_display_name: &HostName,
    _local_node_id: &NodeId,
    peers: &[(NodeInfo, &ProviderData)],
) -> ProviderData {
    let mut merged = local.clone();

    for (peer_node, peer_data) in peers {
        for (host_path, checkout) in &peer_data.checkouts {
            if host_path.host_name() == Some(local_display_name) {
                continue;
            }
            if host_path.host_id().is_some() {
                merged.checkouts.entry(host_path.clone()).or_insert_with(|| {
                    let mut checkout = checkout.clone();
                    checkout.host_name.get_or_insert_with(|| HostName::new(peer_node.display_name.clone()));
                    checkout
                });
                continue;
            }
            if host_path.host_name().is_some_and(|host| host.as_str() != peer_node.display_name) {
                continue;
            }
            let mut checkout = checkout.clone();
            checkout.host_name.get_or_insert_with(|| HostName::new(peer_node.display_name.clone()));
            merged.checkouts.insert(host_path.clone(), checkout);
        }

        for (id, terminal) in &peer_data.managed_terminals {
            let namespaced = flotilla_protocol::AttachableId::new(format!("{}:{}", peer_node.node_id, id));
            merged.managed_terminals.insert(namespaced, terminal.clone());
        }

        for (name, branch) in &peer_data.branches {
            merged.branches.entry(name.clone()).or_insert_with(|| branch.clone());
        }

        for (name, workspace) in &peer_data.workspaces {
            let namespaced = format!("{}:{}", peer_node.node_id, name);
            merged.workspaces.insert(namespaced, workspace.clone());
        }

        for (id, set) in &peer_data.attachable_sets {
            let mut set = set.clone();
            set.host_affinity.get_or_insert_with(|| HostName::new(peer_node.display_name.clone()));
            merged.attachable_sets.entry(id.clone()).or_insert(set);
        }

        for (key, cr) in &peer_data.change_requests {
            merged.change_requests.entry(key.clone()).or_insert_with(|| cr.clone());
        }
        for (key, issue) in &peer_data.issues {
            merged.issues.entry(key.clone()).or_insert_with(|| issue.clone());
        }
        for (key, session) in &peer_data.sessions {
            merged.sessions.entry(key.clone()).or_insert_with(|| session.clone());
        }
        for (key, agent) in &peer_data.agents {
            merged.agents.entry(key.clone()).or_insert_with(|| agent.clone());
        }
    }

    merged
}

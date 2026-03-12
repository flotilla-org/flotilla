use flotilla_protocol::{HostName, ProviderData};

/// Merge local ProviderData with peer data from remote hosts.
///
/// Host-scoped data (checkouts, managed terminals) is combined from all hosts.
/// Checkouts already carry HostPath keys, so no additional namespacing is needed.
/// Terminal names are prefixed with the peer host name to avoid collisions.
///
/// Service-level data (change_requests, issues, sessions) comes only
/// from the leader — followers don't poll external APIs, so there are no
/// duplicates to reconcile. If a peer does send service-level data (e.g. the
/// leader relaying its own data), we include it.
pub fn merge_provider_data(
    local: &ProviderData,
    _local_host: &HostName, // reserved: Phase 2 will use for conflict resolution
    peers: &[(HostName, &ProviderData)],
) -> ProviderData {
    let mut merged = local.clone();

    for (peer_host, peer_data) in peers {
        // Merge checkouts — HostPath keys already carry the peer's host
        for (host_path, checkout) in &peer_data.checkouts {
            merged.checkouts.insert(host_path.clone(), checkout.clone());
        }

        // Merge managed terminals with host-namespaced keys
        for (name, terminal) in &peer_data.managed_terminals {
            let namespaced = format!("{}:{}", peer_host, name);
            merged
                .managed_terminals
                .insert(namespaced, terminal.clone());
        }

        // Merge branches from peers. Followers don't run the remote-branch
        // provider, so peer branch maps are expected to be empty.
        // "or_insert" keeps local data if both sides have the same key.
        for (name, branch) in &peer_data.branches {
            merged
                .branches
                .entry(name.clone())
                .or_insert_with(|| branch.clone());
        }

        // Merge workspaces from peers
        for (name, workspace) in &peer_data.workspaces {
            let namespaced = format!("{}:{}", peer_host, name);
            merged.workspaces.insert(namespaced, workspace.clone());
        }

        // Service-level data (PRs, issues, sessions) comes only from leader.
        // Followers don't poll external APIs so their maps are empty — no
        // conflict. We merge unconditionally so followers see leader's data.
        for (key, cr) in &peer_data.change_requests {
            merged
                .change_requests
                .entry(key.clone())
                .or_insert_with(|| cr.clone());
        }
        for (key, issue) in &peer_data.issues {
            merged
                .issues
                .entry(key.clone())
                .or_insert_with(|| issue.clone());
        }
        for (key, session) in &peer_data.sessions {
            merged
                .sessions
                .entry(key.clone())
                .or_insert_with(|| session.clone());
        }
    }

    merged
}

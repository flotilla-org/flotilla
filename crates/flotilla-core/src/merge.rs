use flotilla_protocol::{HostName, ProviderData};

/// Merge local ProviderData with peer data from remote hosts.
///
/// Host-scoped data is merged with ownership-aware rules:
/// - checkouts are accepted only from the host that owns the `HostPath`
/// - local-host checkouts are never overwritten by peer data
/// - managed terminals and workspaces are namespaced by peer host to avoid collisions
///
/// Service-level data (change_requests, issues, sessions) comes only
/// from the leader — followers don't poll external APIs, so there are no
/// duplicates to reconcile. If a peer does send service-level data (e.g. the
/// leader relaying its own data), we include it.
pub fn merge_provider_data(local: &ProviderData, local_host: &HostName, peers: &[(HostName, &ProviderData)]) -> ProviderData {
    let mut merged = local.clone();

    for (peer_host, peer_data) in peers {
        // Merge checkouts by host ownership.
        // - local host paths are authoritative locally, so peer data must not
        //   overwrite them
        // - peer-owned host paths are only accepted from that owning peer
        for (host_path, checkout) in &peer_data.checkouts {
            if &host_path.host == local_host {
                continue;
            }
            if &host_path.host != peer_host {
                continue;
            }
            merged.checkouts.insert(host_path.clone(), checkout.clone());
        }

        // Merge managed terminals with host-namespaced keys
        for (name, terminal) in &peer_data.managed_terminals {
            let namespaced = format!("{}:{}", peer_host, name);
            merged.managed_terminals.insert(namespaced, terminal.clone());
        }

        // Merge branches from peers. Followers don't run the remote-branch
        // provider, so peer branch maps are expected to be empty.
        // "or_insert" keeps local data if both sides have the same key.
        for (name, branch) in &peer_data.branches {
            merged.branches.entry(name.clone()).or_insert_with(|| branch.clone());
        }

        // Merge workspaces from peers
        for (name, workspace) in &peer_data.workspaces {
            let namespaced = format!("{}:{}", peer_host, name);
            merged.workspaces.insert(namespaced, workspace.clone());
        }

        // Service-level data (PRs, issues, sessions) comes only from leader.
        // Followers don't poll external APIs so their maps are normally empty.
        // Local entries stay authoritative; peer data only fills gaps.
        for (key, cr) in &peer_data.change_requests {
            merged.change_requests.entry(key.clone()).or_insert_with(|| cr.clone());
        }
        for (key, issue) in &peer_data.issues {
            merged.issues.entry(key.clone()).or_insert_with(|| issue.clone());
        }
        for (key, session) in &peer_data.sessions {
            merged.sessions.entry(key.clone()).or_insert_with(|| session.clone());
        }
    }

    merged
}

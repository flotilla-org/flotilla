// Re-export from flotilla-core where the implementation lives.
pub use flotilla_core::merge::merge_provider_data;

#[cfg(test)]
mod tests {
    use flotilla_protocol::{ChangeRequest, ChangeRequestStatus, Checkout, HostName, HostPath, ProviderData};
    use indexmap::IndexMap;

    use super::*;

    fn make_checkout(branch: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    #[test]
    fn merge_combines_checkouts_from_multiple_hosts() {
        let local = ProviderData {
            checkouts: IndexMap::from([(HostPath::new(HostName::new("laptop"), "/home/dev/repo"), make_checkout("main"))]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(HostPath::new(HostName::new("desktop"), "/home/dev/repo"), make_checkout("feature"))]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 2);
        assert!(merged.checkouts.contains_key(&HostPath::new(HostName::new("laptop"), "/home/dev/repo")));
        assert!(merged.checkouts.contains_key(&HostPath::new(HostName::new("desktop"), "/home/dev/repo")));
    }

    #[test]
    fn merge_does_not_duplicate_local_checkouts() {
        let local_host = HostName::new("laptop");
        let local = ProviderData {
            checkouts: IndexMap::from([(HostPath::new(local_host.clone(), "/home/dev/repo"), make_checkout("main"))]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &local_host, &[]);
        assert_eq!(merged.checkouts.len(), 1);
    }

    #[test]
    fn merge_preserves_local_service_data() {
        let mut local = ProviderData::default();
        local.change_requests.insert("PR-1".into(), ChangeRequest {
            title: "Fix bug".into(),
            branch: "fix-bug".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        let remote = ProviderData::default();
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.change_requests.len(), 1);
        assert!(merged.change_requests.contains_key("PR-1"));
    }

    #[test]
    fn merge_with_empty_peers_returns_local_unchanged() {
        let mut local = ProviderData::default();
        local.checkouts.insert(HostPath::new(HostName::new("laptop"), "/repo"), make_checkout("main"));
        local.change_requests.insert("PR-1".into(), ChangeRequest {
            title: "T".into(),
            branch: "b".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[]);
        assert_eq!(merged, local);
    }

    #[test]
    fn merge_local_checkout_wins_for_same_local_host_path() {
        let local_host = HostName::new("laptop");
        let host_path = HostPath::new(local_host.clone(), "/repo");
        let local = ProviderData { checkouts: IndexMap::from([(host_path.clone(), make_checkout("main"))]), ..Default::default() };
        let remote =
            ProviderData { checkouts: IndexMap::from([(host_path.clone(), make_checkout("stale-peer-view"))]), ..Default::default() };

        let merged = merge_provider_data(&local, &local_host, &[(HostName::new("desktop"), &remote)]);

        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&host_path].branch, "main");
    }

    #[test]
    fn merge_peer_checkout_overwrites_same_host_path() {
        // For a peer-owned HostPath, an updated snapshot from that owning peer
        // should overwrite any stale locally cached copy of the same path.
        let host_path = HostPath::new(HostName::new("desktop"), "/repo");
        let local = ProviderData { checkouts: IndexMap::from([(host_path.clone(), make_checkout("old-branch"))]), ..Default::default() };
        let remote = ProviderData { checkouts: IndexMap::from([(host_path.clone(), make_checkout("new-branch"))]), ..Default::default() };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&host_path].branch, "new-branch");
    }

    #[test]
    fn merge_drops_checkout_claimed_for_third_party_host() {
        let local = ProviderData::default();
        let spoofed_path = HostPath::new(HostName::new("server"), "/repo");
        let remote =
            ProviderData { checkouts: IndexMap::from([(spoofed_path.clone(), make_checkout("spoofed-branch"))]), ..Default::default() };

        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);

        assert!(!merged.checkouts.contains_key(&spoofed_path), "peer-owned merge should reject checkout data for a third-party host path");
    }
}

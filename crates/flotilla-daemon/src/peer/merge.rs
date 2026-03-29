// Re-export from flotilla-core where the implementation lives.
pub use flotilla_core::merge::merge_provider_data;

#[cfg(test)]
mod tests {
    use flotilla_protocol::{test_support::TestCheckout, ChangeRequest, ChangeRequestStatus, HostName, ProviderData, QualifiedPath};
    use indexmap::IndexMap;

    use super::*;

    #[test]
    fn merge_combines_checkouts_from_multiple_hosts() {
        let local = ProviderData {
            checkouts: IndexMap::from([(
                QualifiedPath::from_host_path(&HostName::new("laptop"), "/home/dev/repo"),
                TestCheckout::new("main").build(),
            )]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(
                QualifiedPath::from_host_path(&HostName::new("desktop"), "/home/dev/repo"),
                TestCheckout::new("feature").build(),
            )]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 2);
        assert!(merged.checkouts.contains_key(&QualifiedPath::from_host_path(&HostName::new("laptop"), "/home/dev/repo")));
        assert!(merged.checkouts.contains_key(&QualifiedPath::from_host_path(&HostName::new("desktop"), "/home/dev/repo")));
    }

    #[test]
    fn merge_does_not_duplicate_local_checkouts() {
        let local_host = HostName::new("laptop");
        let local = ProviderData {
            checkouts: IndexMap::from([(QualifiedPath::from_host_path(&local_host, "/home/dev/repo"), TestCheckout::new("main").build())]),
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
        local.checkouts.insert(QualifiedPath::from_host_path(&HostName::new("laptop"), "/repo"), TestCheckout::new("main").build());
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
        let host_path = QualifiedPath::from_host_path(&local_host, "/repo");
        let local =
            ProviderData { checkouts: IndexMap::from([(host_path.clone(), TestCheckout::new("main").build())]), ..Default::default() };
        let remote = ProviderData {
            checkouts: IndexMap::from([(host_path.clone(), TestCheckout::new("stale-peer-view").build())]),
            ..Default::default()
        };

        let merged = merge_provider_data(&local, &local_host, &[(HostName::new("desktop"), &remote)]);

        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&host_path].branch, "main");
    }

    #[test]
    fn merge_peer_checkout_overwrites_same_host_path() {
        // For a peer-owned QualifiedPath, an updated snapshot from that owning peer
        // should overwrite any stale locally cached copy of the same path.
        let host_path = QualifiedPath::from_host_path(&HostName::new("desktop"), "/repo");
        let local = ProviderData {
            checkouts: IndexMap::from([(host_path.clone(), TestCheckout::new("old-branch").build())]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(host_path.clone(), TestCheckout::new("new-branch").build())]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&host_path].branch, "new-branch");
    }

    #[test]
    fn merge_drops_checkout_claimed_for_third_party_host() {
        let local = ProviderData::default();
        let spoofed_path = QualifiedPath::from_host_path(&HostName::new("server"), "/repo");
        let remote = ProviderData {
            checkouts: IndexMap::from([(spoofed_path.clone(), TestCheckout::new("spoofed-branch").build())]),
            ..Default::default()
        };

        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);

        assert!(!merged.checkouts.contains_key(&spoofed_path), "peer-owned merge should reject checkout data for a third-party host path");
    }
}

// Re-export from flotilla-core where the implementation lives.
pub use flotilla_core::merge::merge_provider_data;

#[cfg(test)]
mod tests {
    use flotilla_protocol::{
        qualified_path::{HostId, QualifiedPath},
        test_support::TestCheckout,
        ChangeRequest, ChangeRequestStatus, HostName, HostPath, ProviderData,
    };
    use indexmap::IndexMap;

    use super::*;

    #[test]
    fn merge_combines_checkouts_from_multiple_hosts() {
        let local = ProviderData {
            checkouts: IndexMap::from([(
                HostPath::new(HostName::new("laptop"), "/home/dev/repo").into(),
                TestCheckout::new("main").build(),
            )]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(
                HostPath::new(HostName::new("desktop"), "/home/dev/repo").into(),
                TestCheckout::new("feature").build(),
            )]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 2);
        let laptop_checkout: QualifiedPath = HostPath::new(HostName::new("laptop"), "/home/dev/repo").into();
        let desktop_checkout: QualifiedPath = HostPath::new(HostName::new("desktop"), "/home/dev/repo").into();
        assert!(merged.checkouts.contains_key(&laptop_checkout));
        assert!(merged.checkouts.contains_key(&desktop_checkout));
    }

    #[test]
    fn merge_does_not_duplicate_local_checkouts() {
        let local_host = HostName::new("laptop");
        let local = ProviderData {
            checkouts: IndexMap::from([(HostPath::new(local_host.clone(), "/home/dev/repo").into(), TestCheckout::new("main").build())]),
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
        local.checkouts.insert(HostPath::new(HostName::new("laptop"), "/repo").into(), TestCheckout::new("main").build());
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
        let local = ProviderData {
            checkouts: IndexMap::from([(host_path.clone().into(), TestCheckout::new("main").build())]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(host_path.clone().into(), TestCheckout::new("stale-peer-view").build())]),
            ..Default::default()
        };

        let merged = merge_provider_data(&local, &local_host, &[(HostName::new("desktop"), &remote)]);

        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&flotilla_protocol::qualified_path::QualifiedPath::from(host_path.clone())].branch, "main");
    }

    #[test]
    fn merge_peer_checkout_overwrites_same_host_path() {
        // For a peer-owned HostPath, an updated snapshot from that owning peer
        // should overwrite any stale locally cached copy of the same path.
        let host_path = HostPath::new(HostName::new("desktop"), "/repo");
        let local = ProviderData {
            checkouts: IndexMap::from([(host_path.clone().into(), TestCheckout::new("old-branch").build())]),
            ..Default::default()
        };
        let remote = ProviderData {
            checkouts: IndexMap::from([(host_path.clone().into(), TestCheckout::new("new-branch").build())]),
            ..Default::default()
        };
        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);
        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&flotilla_protocol::qualified_path::QualifiedPath::from(host_path.clone())].branch, "new-branch");
    }

    #[test]
    fn merge_accepts_peer_checkout_owned_by_host_id() {
        let local = ProviderData::default();
        let remote_checkout = QualifiedPath::host(HostId::new("follower-host-id"), "/repo");
        let remote = ProviderData {
            checkouts: IndexMap::from([(remote_checkout.clone(), TestCheckout::new("new-branch").build())]),
            ..Default::default()
        };

        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);

        assert_eq!(merged.checkouts.len(), 1);
        assert_eq!(merged.checkouts[&remote_checkout].branch, "new-branch");
        assert_eq!(merged.checkouts[&remote_checkout].host_name, Some(HostName::new("desktop")));
    }

    #[test]
    fn merge_drops_checkout_claimed_for_third_party_host() {
        let local = ProviderData::default();
        let spoofed_path = HostPath::new(HostName::new("server"), "/repo");
        let remote = ProviderData {
            checkouts: IndexMap::from([(spoofed_path.clone().into(), TestCheckout::new("spoofed-branch").build())]),
            ..Default::default()
        };

        let merged = merge_provider_data(&local, &HostName::new("laptop"), &[(HostName::new("desktop"), &remote)]);

        assert!(
            !merged.checkouts.contains_key(&flotilla_protocol::qualified_path::QualifiedPath::from(spoofed_path)),
            "peer-owned merge should reject checkout data for a third-party host path"
        );
    }
}

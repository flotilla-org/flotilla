use flotilla_protocol::{AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout};

use super::*;

fn checkout_with_issue(issue_id: &str) -> Checkout {
    Checkout {
        branch: "main".into(),
        is_main: true,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("gh".into(), issue_id.into())],
        environment_id: None,
    }
}

fn cr_with_issue(issue_id: &str) -> ChangeRequest {
    ChangeRequest {
        title: "Fix bug".into(),
        branch: "feature/fix".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("gh".into(), issue_id.into())],
        provider_name: String::new(),
        provider_display_name: String::new(),
    }
}

#[test]
fn collect_linked_issue_ids_deduplicates_across_sources() {
    let mut providers = ProviderData::default();
    providers.checkouts.insert(
        flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from("/tmp/repo")),
        checkout_with_issue("123"),
    );
    providers.change_requests.insert("1".into(), cr_with_issue("123"));
    providers.change_requests.insert("2".into(), cr_with_issue("456"));

    let mut ids = collect_linked_issue_ids(&providers);
    ids.sort();
    assert_eq!(ids, vec!["123".to_string(), "456".to_string()]);
}

#[test]
fn inject_issues_prefers_search_results_then_cache_then_empty() {
    let base = ProviderData::default();

    let mut cache = IssueCache::new();
    cache.add_pinned(vec![("1".into(), Issue {
        title: "cached".into(),
        labels: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    })]);

    let search_results = Some(vec![("2".into(), Issue {
        title: "search".into(),
        labels: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    })]);

    let from_search = inject_issues(&base, &cache, &search_results);
    assert_eq!(from_search.issues.len(), 1);
    assert!(from_search.issues.contains_key("2"));

    let from_cache = inject_issues(&base, &cache, &None);
    assert!(from_cache.issues.contains_key("1"));

    let empty_cache = IssueCache::new();
    let empty = inject_issues(&base, &empty_cache, &None);
    assert!(empty.issues.is_empty());
}

#[test]
fn choose_event_uses_delta_for_non_initial_changes() {
    let repo = PathBuf::from("/tmp/repo");
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(&repo),
        repo: Some(repo.clone()),
        host_name: HostName::local(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    };

    let initial = DeltaEntry { seq: 1, prev_seq: 0, changes: vec![], work_items: vec![] };
    assert!(matches!(choose_event(snapshot.clone(), initial), DaemonEvent::RepoSnapshot(_)));

    let non_empty = DeltaEntry {
        seq: 2,
        prev_seq: 1,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/x".into(), op: flotilla_protocol::EntryOp::Removed }],
        work_items: vec![],
    };
    assert!(matches!(choose_event(snapshot, non_empty), DaemonEvent::RepoDelta(_)));
}

#[test]
fn choose_event_falls_back_to_full_when_delta_is_larger() {
    let snapshot = RepoSnapshot {
        seq: 3,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        host_name: HostName::local(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    };

    let delta = DeltaEntry {
        seq: 3,
        prev_seq: 2,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/".repeat(128), op: flotilla_protocol::EntryOp::Removed }],
        work_items: vec![],
    };

    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

#[test]
fn build_repo_snapshot_sets_issue_metadata() {
    let mut cache = IssueCache::new();
    cache.total_count = Some(5);
    cache.has_more = true;
    cache.add_pinned(vec![("9".into(), Issue {
        title: "cached issue".into(),
        labels: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    })]);

    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            cache: &cache,
            search_results: &None,
            host_name: &HostName::local(),
        },
        7,
        None,
    );
    assert_eq!(snap.seq, 7);
    assert_eq!(snap.issue_total, Some(5));
    assert!(snap.issue_has_more);
    assert!(snap.providers.issues.contains_key("9"));
}

// --- now_iso8601 ---

#[test]
fn now_iso8601_returns_parseable_timestamp() {
    let ts = now_iso8601();
    assert!(ts.ends_with('Z'), "should be UTC: {ts}");
    assert!(ts.len() >= 20, "should be a full ISO 8601 timestamp: {ts}");
    // Verify it parses as a valid RFC 3339 timestamp
    chrono::DateTime::parse_from_rfc3339(&ts).expect("should parse as RFC 3339");
}

// --- choose_event edge case: empty changes with prev_seq > 0 ---

#[test]
fn choose_event_sends_full_when_delta_has_empty_changes() {
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        host_name: HostName::local(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    };

    // prev_seq > 0 but changes is empty — should still send full
    let delta = DeltaEntry { seq: 2, prev_seq: 1, changes: vec![], work_items: vec![] };
    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

// --- build_repo_snapshot_with_peers ---

#[test]
fn build_repo_snapshot_with_peers_merges_peer_data() {
    let cache = IssueCache::new();
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    // Create peer provider data with a checkout owned by host_b
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(host_b.clone(), PathBuf::from("/remote/repo")), Checkout {
        branch: "remote-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    let peers = vec![(host_b, peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            cache: &cache,
            search_results: &None,
            host_name: &host_a,
        },
        1,
        Some(&peers),
    );

    // The snapshot should contain the merged peer checkout
    assert!(!snap.providers.checkouts.is_empty(), "peer checkout should be merged");
    assert_eq!(snap.providers.checkouts.len(), 1);
}

/// Regression test: when `base` already contains merged peer data (as happens
/// after poll_snapshots stores `re_snapshot` in `last_snapshot`), calling
/// `build_repo_snapshot_with_peers` again must not re-attribute peer checkouts
/// to the local host via `normalize_local_provider_hosts`.
#[test]
fn build_repo_snapshot_with_peers_does_not_duplicate_from_merged_base() {
    let cache = IssueCache::new();
    let local_host = HostName::new("feta");
    let peer_host = HostName::new("kiwi");

    // Simulate local checkout
    let mut local_providers = ProviderData::default();
    local_providers.checkouts.insert(flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/home/dev/repo")), Checkout {
        branch: "main".into(),
        is_main: true,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });

    // Create peer data
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(peer_host.clone(), PathBuf::from("/srv/kiwi/repo")), Checkout {
        branch: "peer-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    });
    let peers = vec![(peer_host.clone(), peer_data.clone())];
    let default_snap = RefreshSnapshot::default();

    // First call — simulates the initial build (local-only base).
    // This produces a merged result containing both local + peer checkouts.
    let first_snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
            path: Path::new("/home/dev/repo"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            cache: &cache,
            search_results: &None,
            host_name: &local_host,
        },
        1,
        Some(&peers),
    );
    assert_eq!(first_snap.providers.checkouts.len(), 2, "first build should have local + peer checkout");

    // Simulate poll_snapshots storing the merged result as last_snapshot
    // while last_local_providers retains only local data.
    // The bug was: passing merged providers as the base to a second call
    // would re-stamp peer checkouts as local via normalize_local_provider_hosts.
    // With the fix, callers always pass local_providers, never merged data.

    // Second call — uses local-only providers (the fix), not merged data.
    let second_snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
            path: Path::new("/home/dev/repo"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            cache: &cache,
            search_results: &None,
            host_name: &local_host,
        },
        2,
        Some(&peers),
    );

    // The peer checkout must appear exactly once under kiwi
    let kiwi_count = second_snap.providers.checkouts.keys().filter(|hp| hp.host == peer_host).count();
    assert_eq!(kiwi_count, 1, "peer checkout should appear once under kiwi, got {kiwi_count}");

    // No ghost checkout — kiwi's path must not appear under the local host
    let ghost = flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/srv/kiwi/repo"));
    assert!(
        !second_snap.providers.checkouts.contains_key(&ghost),
        "peer checkout at /srv/kiwi/repo must not be re-stamped as local host checkout"
    );

    // Total checkout count should remain 2 (1 local + 1 peer)
    assert_eq!(
        second_snap.providers.checkouts.len(),
        2,
        "should have exactly 2 checkouts (1 local + 1 peer), got {}",
        second_snap.providers.checkouts.len()
    );
}

#[test]
fn build_repo_snapshot_with_peers_preserves_remote_attachable_set_for_local_workspace_binding() {
    let cache = IssueCache::new();
    let local_host = HostName::new("kiwi");
    let remote_host = HostName::new("feta");
    let remote_checkout = HostPath::new(remote_host.clone(), PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"));
    let set_id = flotilla_protocol::AttachableSetId::new("set-remote");

    let mut local_providers = ProviderData::default();
    local_providers.workspaces.insert("workspace:9".into(), flotilla_protocol::Workspace {
        name: "attachable-correlation@feta".into(),
        correlation_keys: vec![],
        attachable_set_id: Some(set_id.clone()),
    });
    local_providers.attachable_sets.insert(set_id.clone(), flotilla_protocol::AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(remote_host.clone()),
        checkout: Some(remote_checkout.clone()),
        template_identity: None,
        environment_id: None,
        members: vec![],
    });

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(remote_checkout.clone(), Checkout {
        branch: "attachable-correlation".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![
            CorrelationKey::Branch("attachable-correlation".into()),
            CorrelationKey::CheckoutPath(remote_checkout.clone()),
        ],
        association_keys: vec![],
        environment_id: None,
    });

    let peers = vec![(remote_host.clone(), peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snapshot = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/Users/robert/dev/flotilla")),
            path: Path::new("/Users/robert/dev/flotilla"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            cache: &cache,
            search_results: &None,
            host_name: &local_host,
        },
        1,
        Some(&peers),
    );

    let set = snapshot.providers.attachable_sets.get(&set_id).expect("attachable set should remain projected");
    assert_eq!(set.host_affinity.as_ref(), Some(&remote_host), "remote attachable set host affinity should stay on feta");
    assert_eq!(set.checkout.as_ref(), Some(&remote_checkout), "remote attachable set checkout should stay on feta");

    let set_item =
        snapshot.work_items.iter().find(|item| item.attachable_set_id.as_ref() == Some(&set_id)).expect("work item for attachable set");
    assert_eq!(set_item.host, remote_host, "correlated work item should be anchored to feta");
    assert_eq!(
        set_item.checkout.as_ref().map(|checkout| &checkout.key),
        Some(&remote_checkout),
        "correlated work item should point at the remote checkout"
    );
    assert_eq!(set_item.workspace_refs, vec!["workspace:9".to_string()]);

    let ghost_checkout = HostPath::new(local_host, PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"));
    assert!(
        !snapshot.providers.checkouts.contains_key(&ghost_checkout),
        "remote checkout path must not be duplicated under the local host"
    );
}

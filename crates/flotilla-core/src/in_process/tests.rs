use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, Command, CommandAction, CommandValue, DaemonEvent, EnvironmentId,
    EnvironmentStatus, HostPath, ImageId, Issue, RepoSelector,
};
use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta, TaskPhase, TaskState};

use super::*;
use crate::{
    agents::shared_in_memory_agent_state_store,
    attachable::shared_in_memory_attachable_store,
    config::ConfigStore,
    environment_manager::EnvironmentManager,
    model::RepoModel,
    providers::{
        discovery::{
            test_support::{fake_discovery, DiscoveryMockRunner},
            EnvironmentAssertion, EnvironmentBag,
        },
        environment::{EnvironmentHandle, ProvisionedEnvironment, ProvisionedMount},
        CommandRunner,
    },
};

fn node(name: &str) -> NodeInfo {
    NodeInfo::new(NodeId::new(format!("{name}-node")), name)
}

fn local_node_id() -> NodeId {
    NodeId::new("local-node")
}

fn test_environment_manager() -> &'static EnvironmentManager {
    static MANAGER: OnceLock<EnvironmentManager> = OnceLock::new();
    MANAGER.get_or_init(|| {
        EnvironmentManager::from_local_state(
            EnvironmentId::new("test-local-env"),
            HostId::new("test-local-host"),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
        )
    })
}

fn empty_input_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

#[test]
fn choose_event_uses_delta_for_non_initial_changes() {
    let repo = PathBuf::from("/tmp/repo");
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(&repo),
        repo: Some(repo.clone()),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    let initial = DeltaEntry { seq: 1, prev_seq: 0, changes: vec![] };
    assert!(matches!(choose_event(snapshot.clone(), initial), DaemonEvent::RepoSnapshot(_)));

    let non_empty = DeltaEntry {
        seq: 2,
        prev_seq: 1,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/x".into(), op: flotilla_protocol::EntryOp::Removed }],
    };
    assert!(matches!(choose_event(snapshot, non_empty), DaemonEvent::RepoDelta(_)));
}

#[test]
fn choose_event_falls_back_to_full_when_delta_is_larger() {
    let snapshot = RepoSnapshot {
        seq: 3,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    let delta = DeltaEntry {
        seq: 3,
        prev_seq: 2,
        changes: vec![flotilla_protocol::Change::Branch { key: "feature/".repeat(128), op: flotilla_protocol::EntryOp::Removed }],
    };

    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

#[test]
fn build_repo_snapshot_basic() {
    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &HostName::local(),
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        7,
        None,
    );
    assert_eq!(snap.seq, 7);
}

// --- choose_event edge case: empty changes with prev_seq > 0 ---

#[test]
fn choose_event_sends_full_when_delta_has_empty_changes() {
    let snapshot = RepoSnapshot {
        seq: 2,
        repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
        repo: Some(PathBuf::from("/tmp/repo")),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    };

    // prev_seq > 0 but changes is empty — should still send full
    let delta = DeltaEntry { seq: 2, prev_seq: 1, changes: vec![] };
    assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
}

// --- build_repo_snapshot_with_peers ---

#[test]
fn build_repo_snapshot_with_peers_merges_peer_data() {
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    // Create peer provider data with a checkout owned by host_b
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(host_b.clone(), PathBuf::from("/remote/repo")).into(), Checkout {
        branch: "remote-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let peers = vec![(node(host_b.as_str()), peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snap = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &default_snap.providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &host_a,
            environment_manager: test_environment_manager(),
            environment_id: None,
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
    let local_host = HostName::new("feta");
    let peer_host = HostName::new("kiwi");

    // Simulate local checkout
    let mut local_providers = ProviderData::default();
    local_providers.checkouts.insert(
        flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/home/dev/repo")).into(),
        Checkout {
            branch: "main".into(),
            is_main: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            host_name: None,
            environment_id: None,
        },
    );

    // Create peer data
    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(flotilla_protocol::HostPath::new(peer_host.clone(), PathBuf::from("/srv/kiwi/repo")).into(), Checkout {
        branch: "peer-feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });
    let peers = vec![(node(peer_host.as_str()), peer_data.clone())];
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
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
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
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        2,
        Some(&peers),
    );

    // The peer checkout must appear exactly once under kiwi
    let kiwi_count = second_snap.providers.checkouts.keys().filter(|hp| hp.host_name() == Some(&peer_host)).count();
    assert_eq!(kiwi_count, 1, "peer checkout should appear once under kiwi, got {kiwi_count}");

    // No ghost checkout — kiwi's path must not appear under the local host
    let ghost = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&local_host, PathBuf::from("/srv/kiwi/repo"));
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
        checkout: Some(remote_checkout.clone().into()),
        template_identity: None,
        environment_id: None,
        members: vec![],
    });

    let mut peer_data = ProviderData::default();
    peer_data.checkouts.insert(remote_checkout.clone().into(), Checkout {
        branch: "attachable-correlation".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![
            CorrelationKey::Branch("attachable-correlation".into()),
            CorrelationKey::CheckoutPath(remote_checkout.clone().into()),
        ],
        association_keys: vec![],
        host_name: None,
        environment_id: None,
    });

    let peers = vec![(node(remote_host.as_str()), peer_data)];
    let default_snap = RefreshSnapshot::default();
    let snapshot = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/Users/robert/dev/flotilla")),
            path: Path::new("/Users/robert/dev/flotilla"),
            local_providers: &local_providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &local_host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        Some(&peers),
    );

    let set = snapshot.providers.attachable_sets.get(&set_id).expect("attachable set should remain projected");
    assert_eq!(set.host_affinity.as_ref(), Some(&remote_host), "remote attachable set host affinity should stay on feta");
    assert_eq!(set.checkout.as_ref(), Some(&remote_checkout.clone().into()), "remote attachable set checkout should stay on feta");

    let set_item =
        snapshot.work_items.iter().find(|item| item.attachable_set_id.as_ref() == Some(&set_id)).expect("work item for attachable set");
    assert_eq!(set_item.node_id, node(remote_host.as_str()).node_id, "correlated work item should be anchored to feta");
    assert_eq!(
        set_item.checkout.as_ref().and_then(|checkout| checkout.host_path()),
        Some(&remote_checkout),
        "correlated work item should point at the remote checkout"
    );
    assert_eq!(set_item.workspace_refs, vec!["workspace:9".to_string()]);

    let ghost_checkout = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(
        &local_host,
        PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"),
    );
    assert!(
        !snapshot.providers.checkouts.contains_key(&ghost_checkout),
        "remote checkout path must not be duplicated under the local host"
    );
}

// --- collect_linked_issue_ids ---

#[test]
fn collect_linked_issue_ids_from_change_requests() {
    let mut providers = ProviderData::default();
    providers.change_requests.insert("PR-1".into(), ChangeRequest {
        title: "Fix bug".into(),
        branch: "fix/bug".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![
            AssociationKey::IssueRef("github".into(), "42".into()),
            AssociationKey::IssueRef("github".into(), "99".into()),
        ],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });

    let mut ids = collect_linked_issue_ids(&providers);
    ids.sort();
    assert_eq!(ids, vec!["42", "99"]);
}

#[test]
fn collect_linked_issue_ids_from_checkouts() {
    let mut providers = ProviderData::default();
    providers.checkouts.insert(HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")).into(), Checkout {
        branch: "feat".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "7".into())],
        host_name: None,
        environment_id: None,
    });

    let ids = collect_linked_issue_ids(&providers);
    assert_eq!(ids, vec!["7"]);
}

#[test]
fn collect_linked_issue_ids_deduplicates() {
    let mut providers = ProviderData::default();
    // Same issue referenced from both a change request and a checkout
    providers.change_requests.insert("PR-1".into(), ChangeRequest {
        title: "Fix".into(),
        branch: "fix".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });
    providers.checkouts.insert(HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")).into(), Checkout {
        branch: "fix".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        host_name: None,
        environment_id: None,
    });

    let ids = collect_linked_issue_ids(&providers);
    assert_eq!(ids.len(), 1, "duplicate issue refs should be deduplicated");
    assert_eq!(ids[0], "42");
}

#[test]
fn collect_linked_issue_ids_empty_when_no_associations() {
    let providers = ProviderData::default();
    let ids = collect_linked_issue_ids(&providers);
    assert!(ids.is_empty());
}

/// When `ProviderData.issues` is populated (as it would be after
/// `fetch_missing_linked_issues`), correlation picks up the issue
/// references and includes them in the snapshot's work items.
#[test]
fn snapshot_includes_linked_issues_when_populated() {
    let host = HostName::new("test-host");
    let checkout_path = HostPath::new(host.clone(), PathBuf::from("/tmp/repo"));

    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone().into(), Checkout {
        branch: "fix/42".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::Branch("fix/42".into()), CorrelationKey::CheckoutPath(checkout_path.into())],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        host_name: None,
        environment_id: None,
    });
    providers.change_requests.insert("PR-100".into(), ChangeRequest {
        title: "Fix issue #42".into(),
        branch: "fix/42".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![CorrelationKey::Branch("fix/42".into()), CorrelationKey::ChangeRequestRef("github".into(), "100".into())],
        association_keys: vec![AssociationKey::IssueRef("github".into(), "42".into())],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });
    // Simulate fetch_missing_linked_issues having populated the issue
    providers.issues.insert("42".into(), Issue {
        title: "Something is broken".into(),
        labels: vec!["bug".into()],
        association_keys: vec![],
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    });

    let default_snap = RefreshSnapshot::default();
    let snapshot = build_repo_snapshot_with_peers(
        SnapshotBuildContext {
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            path: Path::new("/tmp/repo"),
            local_providers: &providers,
            errors: &default_snap.errors,
            provider_health: &default_snap.provider_health,
            node_id: &local_node_id(),
            host_name: &host,
            environment_manager: test_environment_manager(),
            environment_id: None,
        },
        1,
        None,
    );

    // The snapshot should have the issue in its provider data
    assert!(snapshot.providers.issues.contains_key("42"), "issue 42 should be present in snapshot providers");

    // Find the work item that correlates checkout + change request
    let work_item =
        snapshot.work_items.iter().find(|wi| wi.branch.as_deref() == Some("fix/42")).expect("should have a work item for fix/42");

    // The work item should reference issue 42
    assert!(
        work_item.issue_keys.contains(&"42".to_string()),
        "work item should reference linked issue 42, got: {:?}",
        work_item.issue_keys
    );
}

#[tokio::test]
async fn get_repo_providers_uses_preferred_root_environment_host_discovery_for_non_local_direct_repo() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::env_var("LOCAL_MARKER", "local")))
        .expect("replace local environment bag");

    let remote_environment_id = EnvironmentId::new("remote-direct-env");
    daemon
        .register_direct_environment_for_test(
            remote_environment_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("REMOTE_MARKER", "remote")),
            None,
        )
        .expect("register remote direct environment");

    let mut model = RepoModel::new(
        repo.clone(),
        crate::providers::registry::ProviderRegistry::new(),
        None,
        Some(remote_environment_id.clone()),
        None,
        shared_in_memory_attachable_store(),
        shared_in_memory_agent_state_store(),
    );
    model.data.loading = false;

    let identity = fallback_repo_identity(&repo);
    let root = RepoRootState { path: repo.clone(), model, slug: None, repo_bag: EnvironmentBag::new(), unmet: Vec::new(), is_local: true };

    {
        let mut repos = daemon.repos.write().await;
        let mut order = daemon.repo_order.write().await;
        repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
        order.push(identity.clone());
    }
    daemon.path_identities.write().await.insert(repo.clone(), identity);

    let providers = daemon.get_repo_providers_internal(&RepoSelector::Path(repo)).await.expect("repo providers should resolve");

    assert!(
        providers
            .host_discovery
            .iter()
            .any(|entry| entry.kind == "env_var_set" && entry.detail.get("key").map(String::as_str) == Some("REMOTE_MARKER")),
        "host discovery should report the preferred non-local direct environment bag"
    );
    assert!(
        !providers
            .host_discovery
            .iter()
            .any(|entry| entry.kind == "env_var_set" && entry.detail.get("key").map(String::as_str) == Some("LOCAL_MARKER")),
        "host discovery should not fall back to the daemon-local environment bag"
    );
}

#[tokio::test]
async fn convoy_completion_command_updates_convoy_task_status() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;
    let convoys = daemon.resource_backend().using::<Convoy>("flotilla");
    let created = convoys
        .create(&empty_input_meta("convoy-a"), &ConvoySpec {
            workflow_ref: "review-and-fix".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: Some("laptop-docker".to_string()),
            repository: None,
            r#ref: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: None,
            tasks: [("implement".to_string(), TaskState {
                phase: TaskPhase::Running,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })]
            .into_iter()
            .collect(),
            message: None,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: Some("review-and-fix".to_string()),
            observed_workflows: None,
        })
        .await
        .expect("convoy status update should succeed");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyTaskComplete {
                convoy: "convoy-a".to_string(),
                task: "implement".to_string(),
                message: Some("done".to_string()),
            },
        })
        .await
        .expect("execute should return a command id");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result");

    assert_eq!(result, CommandValue::Ok);
    let convoy = convoys.get("convoy-a").await.expect("convoy get should succeed");
    let status = convoy.status.expect("convoy status should exist");
    assert_eq!(status.tasks["implement"].phase, TaskPhase::Completed);
    assert_eq!(status.tasks["implement"].message.as_deref(), Some("done"));
}

#[tokio::test]
async fn convoy_completion_command_targets_configured_provisioning_namespace() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;
    daemon.set_provisioning_namespace("custom-ns".to_string()).await;

    let convoys = daemon.resource_backend().using::<Convoy>("custom-ns");
    let created = convoys
        .create(&empty_input_meta("convoy-a"), &ConvoySpec {
            workflow_ref: "review-and-fix".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: Some("laptop-docker".to_string()),
            repository: None,
            r#ref: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: None,
            tasks: [("implement".to_string(), TaskState {
                phase: TaskPhase::Running,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })]
            .into_iter()
            .collect(),
            message: None,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: Some("review-and-fix".to_string()),
            observed_workflows: None,
        })
        .await
        .expect("convoy status update should succeed");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyTaskComplete {
                convoy: "convoy-a".to_string(),
                task: "implement".to_string(),
                message: Some("done".to_string()),
            },
        })
        .await
        .expect("execute should return a command id");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result");

    assert_eq!(result, CommandValue::Ok);
    let convoy = convoys.get("convoy-a").await.expect("convoy get should succeed");
    let status = convoy.status.expect("convoy status should exist");
    assert_eq!(status.tasks["implement"].phase, TaskPhase::Completed);

    // The default namespace must NOT contain the convoy — completion should target
    // only the configured provisioning namespace, not the legacy hardcoded one.
    let default_convoys = daemon.resource_backend().using::<Convoy>("flotilla");
    let missing = default_convoys.get("convoy-a").await;
    assert!(missing.is_err(), "convoy should not exist in the default namespace: got {missing:?}");
}

#[tokio::test]
async fn normalize_local_provider_hosts_uses_mount_metadata_for_provisioned_checkouts() {
    struct TestProvisionedEnvironment {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
        mounts: Vec<ProvisionedMount>,
    }

    #[async_trait]
    impl ProvisionedEnvironment for TestProvisionedEnvironment {
        fn id(&self) -> &EnvironmentId {
            &self.id
        }

        fn image(&self) -> &ImageId {
            &self.image
        }

        fn container_name(&self) -> Option<&str> {
            None
        }

        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
            self.mounts.clone()
        }

        async fn status(&self) -> Result<EnvironmentStatus, String> {
            Ok(EnvironmentStatus::Running)
        }

        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(HashMap::new())
        }

        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::clone(&self.runner)
        }

        async fn destroy(&self) -> Result<(), String> {
            Ok(())
        }
    }

    let local_environment_id = EnvironmentId::new("local-env");
    let local_host_id = HostId::new("local-host-id");
    let environment_manager = EnvironmentManager::from_local_state(
        local_environment_id,
        local_host_id.clone(),
        Arc::new(DiscoveryMockRunner::builder().build()),
        EnvironmentBag::new(),
    );

    let environment_id = EnvironmentId::new("provisioned-env");
    let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
        id: environment_id.clone(),
        image: ImageId::new("image:test"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        mounts: vec![ProvisionedMount::new("/host/reference-repo", "/workspace/repo")],
    });
    environment_manager
        .register_provisioned_environment(environment_id.clone(), handle, EnvironmentBag::new(), None)
        .expect("register provisioned environment");

    let checkout_path = QualifiedPath::from_host_name(&HostName::local(), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, &environment_manager, Some(&environment_id), &HostName::local());
    let expected = QualifiedPath::host(local_host_id, "/host/reference-repo/feature");
    let checkout = normalized.checkouts.get(&expected).expect("mount-covered checkout should be host-qualified");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(expected.clone())]);
    assert!(
        !normalized.checkouts.contains_key(&checkout_path),
        "environment-local publication should be replaced by the host-qualified path"
    );
}

#[tokio::test]
async fn normalize_local_provider_hosts_preserves_host_qualified_checkout_when_provisioned_mount_lookup_misses() {
    let environment_manager = test_environment_manager();
    let environment_id = EnvironmentId::new("provisioned-env-miss");

    let checkout_path = QualifiedPath::host(HostId::new("persistent-host-id"), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, environment_manager, Some(&environment_id), &HostName::local());
    let checkout = normalized.checkouts.get(&checkout_path).expect("host-qualified checkout should be preserved");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(checkout_path.clone())]);
}

#[tokio::test]
async fn normalize_local_provider_hosts_keeps_environment_qualified_checkout_when_no_host_mapping_exists() {
    let local_environment_id = EnvironmentId::new("local-env-no-mount");
    let local_host_id = HostId::new("local-host-id-no-mount");
    let environment_manager = EnvironmentManager::from_local_state(
        local_environment_id,
        local_host_id,
        Arc::new(DiscoveryMockRunner::builder().build()),
        EnvironmentBag::new(),
    );

    let environment_id = EnvironmentId::new("provisioned-env-no-mount");
    let checkout_path = QualifiedPath::environment(environment_id.clone(), "/workspace/repo/feature");
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_path.clone(), Checkout {
        branch: "feature".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path.clone())],
        association_keys: vec![],
        host_name: None,
        environment_id: Some(environment_id.clone()),
    });

    let normalized = normalize_local_provider_hosts(providers, &environment_manager, Some(&environment_id), &HostName::local());
    let checkout = normalized.checkouts.get(&checkout_path).expect("environment-qualified checkout should remain environment-qualified");

    assert_eq!(checkout.environment_id.as_ref(), Some(&environment_id));
    assert_eq!(checkout.correlation_keys, vec![CorrelationKey::CheckoutPath(checkout_path.clone())]);
}

// --- namespace_state correctness under lag / delta-before-snapshot ---

/// A NamespaceDelta that arrives before any snapshot for that namespace must be
/// silently skipped.  `replay_since` must not return a synthetic seq-0 snapshot.
#[tokio::test]
async fn namespace_delta_before_snapshot_is_skipped() {
    use flotilla_protocol::{
        namespace::{ConvoyId, ConvoyPhase, ConvoySummary, NamespaceDelta},
        DaemonEvent,
    };

    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    // Broadcast a NamespaceDelta with no prior snapshot — simulates the broken
    // path (lag/race).  The background subscriber should discard it.
    let delta = DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 5,
        namespace: "flotilla".into(),
        changed: vec![ConvoySummary {
            id: ConvoyId::new("flotilla", "orphan-convoy"),
            namespace: "flotilla".into(),
            name: "orphan-convoy".into(),
            workflow_ref: "wf".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            tasks: vec![],
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        }],
        removed: vec![],
    }));
    daemon.event_tx.send(delta).expect("broadcast delta should succeed");

    // Give the background subscriber time to process the event.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // replay_since should not surface a synthetic seq-0 snapshot for "flotilla".
    let events = daemon.replay_since(&HashMap::new()).await.expect("replay_since should succeed");
    let has_flotilla_snapshot = events.iter().any(|e| match e {
        DaemonEvent::NamespaceSnapshot(snap) => snap.namespace == "flotilla",
        _ => false,
    });
    assert!(!has_flotilla_snapshot, "delta-before-snapshot must not produce a synthetic seq-0 entry in replay_since");
}

/// On broadcast lag the entire namespace_state cache is cleared.  After clearing,
/// `replay_since` must not return any namespace snapshots until the next real snapshot
/// arrives and repopulates the cache.
#[tokio::test]
async fn namespace_state_cleared_on_broadcast_lag() {
    use flotilla_protocol::{
        namespace::{ConvoyId, ConvoyPhase, ConvoySummary, NamespaceSnapshot},
        DaemonEvent,
    };

    let temp = tempfile::tempdir().expect("create tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("create config dir");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");

    let daemon =
        InProcessDaemon::new(vec![], Arc::new(ConfigStore::with_base(&config_base)), fake_discovery(false), HostName::local()).await;

    // Inject a snapshot directly into namespace_state, bypassing the projection.
    // This simulates a namespace that was previously populated.
    {
        let mut state = daemon.namespace_state.write().await;
        state.insert("flotilla".into(), NamespaceSnapshot {
            seq: 10,
            namespace: "flotilla".into(),
            convoys: vec![ConvoySummary {
                id: ConvoyId::new("flotilla", "existing-convoy"),
                namespace: "flotilla".into(),
                name: "existing-convoy".into(),
                workflow_ref: "wf".into(),
                phase: ConvoyPhase::Active,
                message: None,
                repo_hint: None,
                tasks: vec![],
                started_at: None,
                finished_at: None,
                observed_workflow_ref: None,
                initializing: false,
            }],
        });
    }

    // Confirm it shows up in replay_since before lag.
    let events_before = daemon.replay_since(&HashMap::new()).await.expect("replay_since before lag");
    let snap_before = events_before.iter().any(|e| matches!(e, DaemonEvent::NamespaceSnapshot(s) if s.namespace == "flotilla"));
    assert!(snap_before, "namespace snapshot should appear in replay_since before lag");

    // Simulate lag by directly clearing namespace_state (as the subscriber now does on Lagged).
    daemon.namespace_state.write().await.clear();

    // After clearing, replay_since must return no namespace snapshots for "flotilla".
    let events_after = daemon.replay_since(&HashMap::new()).await.expect("replay_since after lag");
    let snap_after = events_after.iter().any(|e| matches!(e, DaemonEvent::NamespaceSnapshot(s) if s.namespace == "flotilla"));
    assert!(!snap_after, "namespace snapshot must not appear in replay_since after namespace_state was cleared");
}

use std::{collections::HashMap, path::PathBuf};

use flotilla_protocol::{
    snapshot::{WorkItem, WorkItemIdentity, WorkItemKind},
    HostName, HostPath, NodeId, NodeInfo,
};

fn node(name: &str) -> NodeId {
    NodeId::new(format!("node-{name}"))
}

fn health(entries: &[(&str, &str, bool)]) -> HashMap<String, HashMap<String, bool>> {
    let mut map: HashMap<String, HashMap<String, bool>> = HashMap::new();
    for (cat, name, ok) in entries {
        map.entry(cat.to_string()).or_default().insert(name.to_string(), *ok);
    }
    map
}

fn make_work_item(kind: WorkItemKind, branch: Option<&str>, description: &str) -> WorkItem {
    WorkItem {
        kind,
        identity: WorkItemIdentity::Checkout(HostPath::new(HostName::new("test"), PathBuf::from("/tmp/wt")).into()),
        node_id: node("test"),
        branch: branch.map(String::from),
        description: description.to_string(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: vec![],
        workspace_refs: vec![],
        is_main_checkout: false,
        debug_group: vec![],
        source: None,
        terminal_keys: vec![],
        attachable_set_id: None,
        agent_keys: vec![],
    }
}

mod status_human {
    use flotilla_protocol::{
        qualified_path::HostId, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostEnvironment, HostListEntry, HostListResponse,
        HostProviderStatus, HostProvidersResponse, HostStatusResponse, HostSummary, ImageId, PeerConnectionState, RepoSummary,
        StatusResponse, SystemInfo, ToolInventory, TopologyResponse, TopologyRoute,
    };

    use super::*;
    use crate::cli::{
        format_host_list_human, format_host_providers_human, format_host_status_human, format_status_response_human, format_topology_human,
    };

    #[test]
    fn empty_repos() {
        let status = StatusResponse { repos: vec![] };
        assert_eq!(format_status_response_human(&status), "No repos tracked.\n");
    }

    #[test]
    fn single_repo_with_health() {
        let status = StatusResponse {
            repos: vec![RepoSummary {
                path: PathBuf::from("/tmp/my-repo"),
                slug: Some("org/my-repo".into()),
                provider_health: health(&[("vcs", "Git", true)]),
                work_item_count: 3,
                error_count: 0,
            }],
        };
        let output = format_status_response_human(&status);
        assert!(output.contains("my-repo"), "should contain repo name");
        assert!(output.contains("3"), "should show work item count");
    }

    fn sample_host_summary(name: &str) -> HostSummary {
        HostSummary {
            environment_id: EnvironmentId::host(HostId::new(format!("{name}-env"))),
            host_name: Some(HostName::new(name)),
            node: NodeInfo::new(node(name), name),
            system: SystemInfo {
                home_dir: Some("/home/dev".into()),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(8),
                memory_total_mb: Some(16384),
                environment: HostEnvironment::Container,
            },
            inventory: ToolInventory::default(),
            providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), implementation: "git".into(), healthy: true }],
            environments: vec![],
        }
    }

    fn sample_visible_environments() -> Vec<EnvironmentInfo> {
        vec![
            EnvironmentInfo::Direct {
                id: EnvironmentId::new("direct-env"),
                display_name: Some("direct".into()),
                host_id: None,
                status: EnvironmentStatus::Running,
            },
            EnvironmentInfo::Provisioned {
                id: EnvironmentId::new("provisioned-env"),
                display_name: Some("provisioned".into()),
                image: ImageId::new("mock:image"),
                status: EnvironmentStatus::Running,
            },
        ]
    }

    #[test]
    fn host_list_shows_hosts_and_counts() {
        let response = HostListResponse {
            hosts: vec![
                HostListEntry {
                    environment_id: EnvironmentId::host(HostId::new("local-env")),
                    host_name: HostName::new("local"),
                    node: NodeInfo::new(NodeId::new("local"), "local"),
                    is_local: true,
                    configured: false,
                    connection_status: PeerConnectionState::Connected,
                    has_summary: true,
                    repo_count: 2,
                    work_item_count: 5,
                },
                HostListEntry {
                    environment_id: EnvironmentId::host(HostId::new("remote-env")),
                    host_name: HostName::new("remote"),
                    node: NodeInfo::new(NodeId::new("remote"), "remote"),
                    is_local: false,
                    configured: true,
                    connection_status: PeerConnectionState::Disconnected,
                    has_summary: false,
                    repo_count: 0,
                    work_item_count: 0,
                },
            ],
        };

        let output = format_host_list_human(&response);
        assert!(output.contains("remote"));
        assert!(output.contains("disconnected"));
        assert!(output.contains("5"));
    }

    #[test]
    fn host_status_shows_summary_and_counts() {
        let response = HostStatusResponse {
            environment_id: EnvironmentId::host(HostId::new("local-env")),
            host_name: HostName::new("local"),
            node: NodeInfo::new(NodeId::new("local"), "local"),
            is_local: true,
            configured: false,
            connection_status: PeerConnectionState::Connected,
            summary: Some(sample_host_summary("local")),
            visible_environments: sample_visible_environments(),
            repo_count: 2,
            work_item_count: 5,
        };

        let output = format_host_status_human(&response);
        assert!(output.contains("Node: local"));
        assert!(output.contains("Repositories: 2"));
        assert!(output.contains("linux"));
        assert!(output.contains("Visible Environments:"));
        assert!(output.contains("direct-env"));
        assert!(output.contains("provisioned-env"));
    }

    #[test]
    fn host_providers_shows_inventory_and_provider_rows() {
        let response = HostProvidersResponse {
            environment_id: EnvironmentId::host(HostId::new("local-env")),
            host_name: HostName::new("local"),
            node: NodeInfo::new(NodeId::new("local"), "local"),
            is_local: true,
            configured: false,
            connection_status: PeerConnectionState::Connected,
            summary: sample_host_summary("local"),
            visible_environments: sample_visible_environments(),
        };

        let output = format_host_providers_human(&response);
        assert!(output.contains("Providers:"));
        assert!(output.contains("Git"));
        assert!(output.contains("Visible Environments:"));
        assert!(output.contains("direct-env"));
        assert!(output.contains("provisioned-env"));
    }

    #[test]
    fn topology_shows_route_rows() {
        let response = TopologyResponse {
            local_node: NodeInfo::new(NodeId::new("local"), "local"),
            routes: vec![TopologyRoute {
                target: NodeInfo::new(NodeId::new("remote"), "remote"),
                next_hop: NodeInfo::new(NodeId::new("relay"), "relay"),
                direct: false,
                connected: true,
                fallbacks: vec![NodeInfo::new(NodeId::new("backup"), "backup")],
            }],
        };

        let output = format_topology_human(&response);
        assert!(output.contains("remote"));
        assert!(output.contains("relay"));
        assert!(output.contains("backup"));
    }
}

mod watch_human {
    use std::path::PathBuf;

    use flotilla_protocol::{commands::CommandValue, DaemonEvent, HostName, NodeId, PeerConnectionState, RepoDelta, RepoSnapshot};

    use crate::cli::format_event_human;

    fn dummy_snapshot(seq: u64, repo: &str, work_item_count: usize) -> RepoSnapshot {
        use std::collections::HashMap;

        use flotilla_protocol::snapshot::{WorkItem, WorkItemIdentity, WorkItemKind};

        RepoSnapshot {
            seq,
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: repo.into() },
            repo: Some(PathBuf::from(repo)),
            node_id: NodeId::new("test"),
            work_items: (0..work_item_count)
                .map(|i| WorkItem {
                    kind: WorkItemKind::Checkout,
                    identity: WorkItemIdentity::Checkout(
                        flotilla_protocol::HostPath::new(HostName::new("test"), PathBuf::from(format!("/tmp/wt{i}"))).into(),
                    ),
                    node_id: NodeId::new("test"),
                    branch: None,
                    description: String::new(),
                    checkout: None,
                    change_request_key: None,
                    session_key: None,
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                    source: None,
                    terminal_keys: vec![],
                    attachable_set_id: None,
                    agent_keys: vec![],
                })
                .collect(),
            providers: Default::default(),
            provider_health: HashMap::new(),
            errors: vec![],
        }
    }

    #[test]
    fn snapshot_full() {
        let event = DaemonEvent::RepoSnapshot(Box::new(dummy_snapshot(42, "/tmp/my-repo", 5)));
        let line = format_event_human(&event);
        assert!(line.contains("[snapshot]"), "should have snapshot tag");
        assert!(line.contains("my-repo"), "should extract repo name from path");
        assert!(line.contains("seq 42"), "should show seq");
        assert!(line.contains("5 work items"), "should show work item count");
    }

    #[test]
    fn snapshot_delta() {
        let event = DaemonEvent::RepoDelta(Box::new(RepoDelta {
            seq: 42,
            prev_seq: 41,
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/my-repo".into() },
            repo: Some(PathBuf::from("/tmp/my-repo")),
            changes: vec![],
            work_items: vec![],
        }));
        let line = format_event_human(&event);
        assert!(line.contains("[delta]"), "should have delta tag");
        assert!(line.contains("41→42") || line.contains("41->42"), "should show prev→seq");
    }

    #[test]
    fn repo_tracked() {
        let event = DaemonEvent::RepoTracked(Box::new(flotilla_protocol::snapshot::RepoInfo {
            identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/added-repo".into() },
            name: "added-repo".into(),
            path: Some(PathBuf::from("/tmp/added-repo")),
            labels: Default::default(),
            provider_names: Default::default(),
            provider_health: Default::default(),
            loading: false,
        }));
        let line = format_event_human(&event);
        assert!(line.contains("[repo]"), "should have repo tag");
        assert!(line.contains("added-repo"), "should show repo name");
        assert!(line.contains("tracked"), "should say tracked");
    }

    #[test]
    fn repo_untracked() {
        let event = DaemonEvent::RepoUntracked {
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/old-repo".into() },
            path: Some(PathBuf::from("/tmp/old-repo")),
        };
        let line = format_event_human(&event);
        assert!(line.contains("[repo]"), "should have repo tag");
        assert!(line.contains("old-repo"), "should extract name");
        assert!(line.contains("untracked"), "should say untracked");
    }

    #[test]
    fn command_started() {
        let event = DaemonEvent::CommandStarted {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/my-repo".into() },
            repo: Some(PathBuf::from("/tmp/my-repo")),
            description: "Refreshing...".into(),
        };
        let line = format_event_human(&event);
        assert!(line.contains("[command]"), "should have command tag");
        assert!(line.contains("started"), "should say started");
        assert!(line.contains("Refreshing..."), "should include description");
    }

    #[test]
    fn command_finished_ok() {
        let event = DaemonEvent::CommandFinished {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/my-repo".into() },
            repo: Some(PathBuf::from("/tmp/my-repo")),
            result: CommandValue::Ok,
        };
        let line = format_event_human(&event);
        assert!(line.contains("[command]"), "should have command tag");
        assert!(line.contains("finished"), "should say finished");
        assert!(line.contains("ok"), "should show ok result");
    }

    #[test]
    fn command_finished_error() {
        let event = DaemonEvent::CommandFinished {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/tmp/my-repo".into() },
            repo: Some(PathBuf::from("/tmp/my-repo")),
            result: CommandValue::Error { message: "boom".into() },
        };
        let line = format_event_human(&event);
        assert!(line.contains("error: boom"), "should show error message");
    }

    #[test]
    fn peer_all_states() {
        for (state, expected) in [
            (PeerConnectionState::Connected, "connected"),
            (PeerConnectionState::Disconnected, "disconnected"),
            (PeerConnectionState::Connecting, "connecting"),
            (PeerConnectionState::Reconnecting, "reconnecting"),
            (PeerConnectionState::Rejected { reason: "protocol mismatch".to_string() }, "rejected"),
        ] {
            let event = DaemonEvent::PeerStatusChanged { node_id: NodeId::new("host-2"), status: state };
            let line = format_event_human(&event);
            assert!(line.contains("[peer]"), "should have peer tag for {expected}");
            assert!(line.contains("host-2"), "should show host name for {expected}");
            assert!(line.contains(expected), "should contain '{expected}'");
        }
    }
}

mod command_result_human {
    use std::path::PathBuf;

    use flotilla_protocol::{
        commands::{CheckoutStatus, CommandValue},
        HostName, NodeId, PreparedWorkspace,
    };

    use crate::cli::format_command_result;

    #[test]
    fn ok() {
        assert_eq!(format_command_result(&CommandValue::Ok), "ok");
    }

    #[test]
    fn repo_tracked() {
        let result = CommandValue::RepoTracked { path: PathBuf::from("/tmp/my-repo"), resolved_from: None };
        let output = format_command_result(&result);
        assert!(output.contains("repo tracked"), "should say repo tracked");
        assert!(output.contains("/tmp/my-repo"), "should include path");
        assert!(!output.contains("resolved from"), "should not mention resolved_from when None");
    }

    #[test]
    fn repo_tracked_with_resolved_from() {
        let result =
            CommandValue::RepoTracked { path: PathBuf::from("/tmp/my-repo"), resolved_from: Some(PathBuf::from("/tmp/my-repo/wt-feat")) };
        let output = format_command_result(&result);
        assert!(output.contains("repo tracked"), "should say repo tracked");
        assert!(output.contains("/tmp/my-repo/wt-feat"), "should include original path");
        assert!(output.contains("resolved from"), "should mention resolution");
    }

    #[test]
    fn repo_untracked() {
        let result = CommandValue::RepoUntracked { path: PathBuf::from("/tmp/old-repo") };
        let output = format_command_result(&result);
        assert!(output.contains("repo untracked"), "should say repo untracked");
        assert!(output.contains("/tmp/old-repo"), "should include path");
    }

    #[test]
    fn refreshed() {
        let result = CommandValue::Refreshed { repos: vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")] };
        let output = format_command_result(&result);
        assert!(output.contains("refreshed 3 repo(s)"), "should show count of repos");
    }

    #[test]
    fn refreshed_empty() {
        let result = CommandValue::Refreshed { repos: vec![] };
        let output = format_command_result(&result);
        assert!(output.contains("refreshed 0 repo(s)"), "should handle zero repos");
    }

    #[test]
    fn checkout_created() {
        let result = CommandValue::CheckoutCreated { branch: "feat-new".into(), path: PathBuf::from("/tmp/wt") };
        let output = format_command_result(&result);
        assert!(output.contains("checkout created"), "should say checkout created");
        assert!(output.contains("feat-new"), "should include branch name");
    }

    #[test]
    fn checkout_removed() {
        let result = CommandValue::CheckoutRemoved { branch: "feat-old".into() };
        let output = format_command_result(&result);
        assert!(output.contains("checkout removed"), "should say checkout removed");
        assert!(output.contains("feat-old"), "should include branch name");
    }

    #[test]
    fn branch_name_generated() {
        let result = CommandValue::BranchNameGenerated { name: "feat/cool-thing".into(), issue_ids: vec![("github".into(), "42".into())] };
        let output = format_command_result(&result);
        assert!(output.contains("branch name"), "should say branch name");
        assert!(output.contains("feat/cool-thing"), "should include generated name");
    }

    #[test]
    fn checkout_status_clean() {
        let result = CommandValue::CheckoutStatus(CheckoutStatus { branch: "main".into(), ..Default::default() });
        let output = format_command_result(&result);
        assert_eq!(output, "checkout status: main");
    }

    #[test]
    fn checkout_status_with_details() {
        let result = CommandValue::CheckoutStatus(CheckoutStatus {
            branch: "feat/x".into(),
            change_request_status: Some("open".into()),
            unpushed_commits: vec!["abc1234".into(), "def5678".into()],
            has_uncommitted: true,
            ..Default::default()
        });
        let output = format_command_result(&result);
        assert_eq!(output, "checkout status: feat/x, PR: open, 2 unpushed, uncommitted changes");
    }

    #[test]
    fn checkout_status_merged() {
        let result = CommandValue::CheckoutStatus(CheckoutStatus {
            branch: "feat/y".into(),
            change_request_status: Some("merged".into()),
            merge_commit_sha: Some("abc1234def5678".into()),
            ..Default::default()
        });
        let output = format_command_result(&result);
        assert_eq!(output, "checkout status: feat/y, PR: merged, merged via abc1234");
    }

    #[test]
    fn error() {
        let result = CommandValue::Error { message: "something broke".into() };
        let output = format_command_result(&result);
        assert_eq!(output, "error: something broke");
    }

    #[test]
    fn cancelled() {
        assert_eq!(format_command_result(&CommandValue::Cancelled), "cancelled");
    }

    #[test]
    fn prepared_workspace_is_internal_step_result() {
        let result = CommandValue::PreparedWorkspace(PreparedWorkspace {
            label: "feat".into(),
            target_node_id: NodeId::new("feta"),
            display_host: Some(HostName::new("feta")),
            checkout_path: PathBuf::from("/tmp/wt"),
            checkout_key: None,
            attachable_set_id: None,
            environment_id: None,
            container_name: None,
            template_yaml: Some("panes: []".into()),
            prepared_commands: vec![],
        });
        assert_eq!(format_command_result(&result), "internal step result");
    }
}

mod work_items_table {
    use flotilla_protocol::snapshot::WorkItemKind;

    use super::make_work_item;
    use crate::cli::format_work_items_table;

    #[test]
    fn empty_items() {
        let table = format_work_items_table(&[]);
        let output = table.to_string();
        assert!(output.contains("Kind"), "should have header");
        assert!(output.contains("Branch"), "should have Branch header");
        assert!(output.contains("Description"), "should have Description header");
    }

    #[test]
    fn single_item_none_fields_show_dash() {
        // format_work_items_table renders None/empty fields as "-".
        // The data row contains: Kind | Branch | Description | PR | Session | Issues
        // With all optional fields None/empty, the row should have "-" for each.
        let bare = make_work_item(WorkItemKind::Checkout, None, "my checkout");
        let bare_output = format_work_items_table(&[bare]).to_string();
        let data_line = bare_output.lines().find(|l| l.contains("Checkout")).expect("should have a data row");

        // Count occurrences of the placeholder "-" in the data row.
        // Branch, PR, Session, and Issues are all None/empty → 4 dashes expected.
        // We search for the dash bordered by non-alphanumeric chars so we don't
        // match dashes inside table borders.
        let dash_cells: Vec<&str> = data_line.split(|c: char| !c.is_ascii_alphanumeric() && c != '-').filter(|s| *s == "-").collect();
        assert_eq!(dash_cells.len(), 4, "expected 4 dash placeholders (branch, PR, session, issues), got: {dash_cells:?}");
    }

    #[test]
    fn item_with_all_fields_populated() {
        let mut item = make_work_item(WorkItemKind::ChangeRequest, Some("feat-x"), "Feature X");
        item.change_request_key = Some("10".to_string());
        item.session_key = Some("sess-1".to_string());
        item.issue_keys = vec!["I-1".to_string(), "I-2".to_string()];
        let table = format_work_items_table(&[item]);
        let output = table.to_string();
        assert!(output.contains("ChangeRequest"), "should show kind");
        assert!(output.contains("feat-x"), "should show branch");
        assert!(output.contains("Feature X"), "should show description");
        assert!(output.contains("10"), "should show PR key");
        assert!(output.contains("sess-1"), "should show session key");
        assert!(output.contains("I-1, I-2"), "should join issue keys with comma");
    }

    #[test]
    fn multiple_items() {
        let items = vec![
            make_work_item(WorkItemKind::Checkout, Some("main"), "Main branch"),
            make_work_item(WorkItemKind::Session, None, "Agent session"),
        ];
        let table = format_work_items_table(&items);
        let output = table.to_string();
        assert!(output.contains("Checkout"), "should contain first item kind");
        assert!(output.contains("Session"), "should contain second item kind");
        assert!(output.contains("Main branch"), "should contain first description");
        assert!(output.contains("Agent session"), "should contain second description");
    }
}

mod repo_detail_human {
    use std::{collections::HashMap, path::PathBuf};

    use flotilla_protocol::{snapshot::ProviderError, RepoDetailResponse};

    use super::make_work_item;
    use crate::cli::format_repo_detail_human;

    #[test]
    fn minimal_no_slug_no_items_no_errors() {
        let detail = RepoDetailResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: None,
            provider_health: HashMap::new(),
            work_items: vec![],
            errors: vec![],
        };
        let output = format_repo_detail_human(&detail);
        assert!(output.contains("Repo: /tmp/my-repo"), "should show repo path");
        assert!(!output.contains("Slug:"), "should not show slug when None");
        assert!(!output.contains("Kind"), "should not show table when no items");
        assert!(!output.contains("Errors"), "should not show errors when empty");
    }

    #[test]
    fn with_slug() {
        let detail = RepoDetailResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: Some("org/my-repo".into()),
            provider_health: HashMap::new(),
            work_items: vec![],
            errors: vec![],
        };
        let output = format_repo_detail_human(&detail);
        assert!(output.contains("Slug: org/my-repo"), "should show slug");
    }

    #[test]
    fn with_work_items() {
        let detail = RepoDetailResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: None,
            provider_health: HashMap::new(),
            work_items: vec![make_work_item(flotilla_protocol::snapshot::WorkItemKind::Checkout, Some("feat"), "My feature")],
            errors: vec![],
        };
        let output = format_repo_detail_human(&detail);
        assert!(output.contains("My feature"), "should render work items table");
        assert!(output.contains("Kind"), "should have table header");
    }

    #[test]
    fn with_errors() {
        let detail = RepoDetailResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: None,
            provider_health: HashMap::new(),
            work_items: vec![],
            errors: vec![ProviderError { category: "change_request".into(), provider: "GitHub".into(), message: "rate limited".into() }],
        };
        let output = format_repo_detail_human(&detail);
        assert!(output.contains("Errors:"), "should have errors header");
        assert!(output.contains("[change_request/GitHub]"), "should show category/provider");
        assert!(output.contains("rate limited"), "should show error message");
    }
}

mod repo_providers_human {
    use std::{collections::HashMap, path::PathBuf};

    use flotilla_protocol::{DiscoveryEntry, ProviderInfo, RepoProvidersResponse, UnmetRequirementInfo};

    use crate::cli::format_repo_providers_human;

    fn empty_response() -> RepoProvidersResponse {
        RepoProvidersResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: None,
            host_discovery: vec![],
            repo_discovery: vec![],
            providers: vec![],
            unmet_requirements: vec![],
        }
    }

    #[test]
    fn empty_response_shows_repo_only() {
        let resp = empty_response();
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Repo: /tmp/my-repo"), "should show repo path");
        assert!(!output.contains("Host Discovery"), "should not show host discovery when empty");
        assert!(!output.contains("Repo Discovery"), "should not show repo discovery when empty");
        assert!(!output.contains("Providers:"), "should not show providers when empty");
        assert!(!output.contains("Unmet Requirements"), "should not show unmet reqs when empty");
    }

    #[test]
    fn with_host_discovery() {
        let mut resp = empty_response();
        resp.host_discovery =
            vec![DiscoveryEntry { kind: "ssh_config".into(), detail: HashMap::from([("host".into(), "github.com".into())]) }];
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Host Discovery:"), "should show host discovery header");
        assert!(output.contains("ssh_config"), "should show discovery kind");
        assert!(output.contains("host=github.com"), "should show detail key=value");
    }

    #[test]
    fn with_repo_discovery() {
        let mut resp = empty_response();
        resp.repo_discovery = vec![DiscoveryEntry {
            kind: "git_remote".into(),
            detail: HashMap::from([("url".into(), "git@github.com:org/repo.git".into())]),
        }];
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Repo Discovery:"), "should show repo discovery header");
        assert!(output.contains("git_remote"), "should show discovery kind");
        assert!(output.contains("git@github.com:org/repo.git"), "should show detail value");
    }

    #[test]
    fn with_providers_table() {
        let mut resp = empty_response();
        resp.providers = vec![ProviderInfo { category: "vcs".into(), name: "Git".into(), healthy: true }, ProviderInfo {
            category: "change_request".into(),
            name: "GitHub".into(),
            healthy: false,
        }];
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Providers:"), "should show providers header");
        assert!(output.contains("vcs"), "should show category");
        assert!(output.contains("Git"), "should show name");
        assert!(output.contains("ok"), "should show healthy as ok");
        assert!(output.contains("error"), "should show unhealthy as error");
    }

    #[test]
    fn with_unmet_requirements() {
        let mut resp = empty_response();
        resp.unmet_requirements = vec![
            UnmetRequirementInfo { factory: "GitHubChangeRequest".into(), kind: "missing_binary".into(), value: Some("gh".into()) },
            UnmetRequirementInfo { factory: "Git".into(), kind: "no_vcs_checkout".into(), value: None },
        ];
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Unmet Requirements:"), "should show unmet requirements header");
        assert!(output.contains("GitHubChangeRequest"), "should show factory name");
        assert!(output.contains("missing_binary (gh)"), "should show kind and value");
        assert!(output.contains("no_vcs_checkout"), "should show kind without empty value");
    }

    #[test]
    fn with_slug() {
        let mut resp = empty_response();
        resp.slug = Some("org/my-repo".into());
        let output = format_repo_providers_human(&resp);
        assert!(output.contains("Slug: org/my-repo"), "should show slug");
    }
}

mod repo_work_human {
    use std::path::PathBuf;

    use flotilla_protocol::{snapshot::WorkItemKind, RepoWorkResponse};

    use super::make_work_item;
    use crate::cli::format_repo_work_human;

    #[test]
    fn empty_work_items() {
        let resp = RepoWorkResponse { path: PathBuf::from("/tmp/my-repo"), slug: None, work_items: vec![] };
        let output = format_repo_work_human(&resp);
        assert!(output.contains("Repo: /tmp/my-repo"), "should show repo path");
        assert!(output.contains("No work items."), "should say no work items");
    }

    #[test]
    fn with_slug() {
        let resp = RepoWorkResponse { path: PathBuf::from("/tmp/my-repo"), slug: Some("org/my-repo".into()), work_items: vec![] };
        let output = format_repo_work_human(&resp);
        assert!(output.contains("Slug: org/my-repo"), "should show slug");
    }

    #[test]
    fn with_work_items() {
        let resp = RepoWorkResponse {
            path: PathBuf::from("/tmp/my-repo"),
            slug: None,
            work_items: vec![
                make_work_item(WorkItemKind::Checkout, Some("feat-x"), "Feature X"),
                make_work_item(WorkItemKind::Checkout, Some("feat-y"), "Feature Y"),
            ],
        };
        let output = format_repo_work_human(&resp);
        assert!(!output.contains("No work items."), "should not say no work items");
        assert!(output.contains("Feature X"), "should render first work item");
        assert!(output.contains("Feature Y"), "should render second work item");
        assert!(output.contains("Kind"), "should have table header");
    }
}

mod repo_name_fn {
    use std::path::Path;

    use crate::cli::repo_name;

    #[test]
    fn normal_path() {
        assert_eq!(repo_name(Path::new("/tmp/my-repo")), "my-repo");
    }

    #[test]
    fn root_path_fallback() {
        let name = repo_name(Path::new("/"));
        assert_eq!(name, "/", "root path should fall back to full path display");
    }

    #[test]
    fn nested_path() {
        assert_eq!(repo_name(Path::new("/home/user/projects/flotilla")), "flotilla");
    }
}

mod query_event_formatting {
    use std::path::PathBuf;

    use flotilla_protocol::{commands::CommandValue, DaemonEvent, HostListResponse, HostName, NodeId, RepoIdentity};

    use crate::cli::format_event_human;

    fn test_identity() -> RepoIdentity {
        RepoIdentity { authority: String::new(), path: String::new() }
    }

    fn query_started(description: &str) -> DaemonEvent {
        DaemonEvent::CommandStarted {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: test_identity(),
            repo: None,
            description: description.to_string(),
        }
    }

    fn query_finished(result: CommandValue) -> DaemonEvent {
        DaemonEvent::CommandFinished {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: test_identity(),
            repo: None,
            result,
        }
    }

    #[test]
    fn started_event_with_empty_repo_shows_query_prefix() {
        let output = format_event_human(&query_started("query repo detail"));
        assert!(output.starts_with("[query]"), "expected [query] prefix, got: {output}");
        assert!(output.contains("query repo detail"));
    }

    #[test]
    fn started_event_with_repo_shows_command_prefix() {
        let event = DaemonEvent::CommandStarted {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: test_identity(),
            repo: Some(PathBuf::from("/tmp/myrepo")),
            description: "checkout".to_string(),
        };
        let output = format_event_human(&event);
        assert!(output.starts_with("[command]"), "expected [command] prefix, got: {output}");
    }

    #[test]
    fn finished_query_success_shows_result_directly() {
        let result = CommandValue::HostList(Box::new(HostListResponse { hosts: vec![] }));
        let output = format_event_human(&query_finished(result));
        assert!(!output.contains("[command]"), "query result should not have [command] prefix, got: {output}");
    }

    #[test]
    fn finished_query_error_shows_error_directly() {
        let result = CommandValue::Error { message: "repo not found".into() };
        let output = format_event_human(&query_finished(result));
        assert!(!output.contains("[command]"), "query error should not have [command] prefix, got: {output}");
        assert!(output.contains("error: repo not found"));
    }

    #[test]
    fn finished_non_query_shows_command_prefix() {
        let event = DaemonEvent::CommandFinished {
            command_id: 1,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: test_identity(),
            repo: Some(PathBuf::from("/tmp/myrepo")),
            result: CommandValue::Ok,
        };
        let output = format_event_human(&event);
        assert!(output.starts_with("[command]"), "non-query result should have [command] prefix, got: {output}");
    }
}

mod namespace_event_formatting {
    use flotilla_protocol::{
        namespace::{NamespaceDelta, NamespaceSnapshot},
        DaemonEvent,
    };

    use crate::cli::format_event_human;

    #[test]
    fn namespace_snapshot_formatting() {
        let snap = NamespaceSnapshot { seq: 7, namespace: "flotilla".into(), convoys: vec![] };
        let event = DaemonEvent::NamespaceSnapshot(Box::new(snap));
        let line = format_event_human(&event);
        assert!(line.contains("[namespace]"), "should have namespace tag");
        assert!(line.contains("flotilla"), "should include namespace name");
        assert!(line.contains("seq 7"), "should include seq number");
        assert!(line.contains("0 convoys"), "should count convoys");
    }

    #[test]
    fn namespace_delta_formatting() {
        use flotilla_protocol::namespace::ConvoyId;
        let delta = NamespaceDelta {
            seq: 12,
            namespace: "flotilla".into(),
            changed: vec![],
            removed: vec![ConvoyId::new("flotilla", "old-convoy")],
        };
        let event = DaemonEvent::NamespaceDelta(Box::new(delta));
        let line = format_event_human(&event);
        assert!(line.contains("[namespace]"), "should have namespace tag");
        assert!(line.contains("flotilla"), "should include namespace name");
        assert!(line.contains("seq 12") || line.contains("12"), "should include seq number");
        assert!(line.contains("0 changed"), "should show changed count");
        assert!(line.contains("1 removed"), "should show removed count");
    }
}

mod watch_dedupe_namespace {
    use std::collections::HashMap;

    use flotilla_protocol::{
        namespace::{NamespaceDelta, NamespaceSnapshot},
        DaemonEvent, StreamKey,
    };

    use crate::cli::event_stream_seq;

    fn ns_snapshot(namespace: &str, seq: u64) -> DaemonEvent {
        DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot { seq, namespace: namespace.into(), convoys: vec![] }))
    }

    fn ns_delta(namespace: &str, seq: u64) -> DaemonEvent {
        DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta { seq, namespace: namespace.into(), changed: vec![], removed: vec![] }))
    }

    /// Simulate the run_watch dedup logic: build replay_seqs from a slice of
    /// replay events, then return which of the live events would be printed
    /// (i.e. not suppressed by the dedup guard).
    fn events_printed_after_dedup<'a>(replay: &[DaemonEvent], live: &'a [DaemonEvent]) -> Vec<&'a DaemonEvent> {
        let mut replay_seqs: HashMap<StreamKey, u64> = HashMap::new();
        for event in replay {
            if let Some((stream_key, seq)) = event_stream_seq(event) {
                replay_seqs.entry(stream_key).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
            }
        }
        live.iter()
            .filter(|event| {
                if let Some((stream_key, seq)) = event_stream_seq(event) {
                    if let Some(&replay_seq) = replay_seqs.get(&stream_key) {
                        return seq > replay_seq;
                    }
                }
                true
            })
            .collect()
    }

    #[test]
    fn event_stream_seq_returns_namespace_key_for_snapshot() {
        let event = ns_snapshot("flotilla", 5);
        let result = event_stream_seq(&event);
        assert_eq!(result, Some((StreamKey::Namespace { name: "flotilla".into() }, 5)));
    }

    #[test]
    fn event_stream_seq_returns_namespace_key_for_delta() {
        let event = ns_delta("flotilla", 9);
        let result = event_stream_seq(&event);
        assert_eq!(result, Some((StreamKey::Namespace { name: "flotilla".into() }, 9)));
    }

    #[test]
    fn duplicate_seq_namespace_delta_is_suppressed() {
        // A delta arrives in replay at seq=5; the broadcast channel then delivers
        // the same event again (seq=5). The live duplicate must be suppressed.
        let replay = [ns_delta("flotilla", 5)];
        let live = [ns_delta("flotilla", 5)];
        let printed = events_printed_after_dedup(&replay, &live);
        assert!(printed.is_empty(), "duplicate-seq delta should be suppressed by dedup, but {} event(s) were printed", printed.len());
    }

    #[test]
    fn newer_seq_namespace_delta_is_printed() {
        // Replay has seq=5; a genuinely new delta (seq=6) must pass through.
        let replay = [ns_delta("flotilla", 5)];
        let live = [ns_delta("flotilla", 6)];
        let printed = events_printed_after_dedup(&replay, &live);
        assert_eq!(printed.len(), 1, "new-seq delta should pass dedup filter");
    }

    #[test]
    fn snapshot_replay_suppresses_same_seq_live_snapshot() {
        // Replay delivers a full namespace snapshot at seq=3; the broadcast
        // buffer then delivers that same snapshot — must be suppressed.
        let replay = [ns_snapshot("flotilla", 3)];
        let live = [ns_snapshot("flotilla", 3)];
        let printed = events_printed_after_dedup(&replay, &live);
        assert!(printed.is_empty(), "duplicate-seq snapshot should be suppressed");
    }

    #[test]
    fn different_namespace_events_are_not_cross_suppressed() {
        // Replay has "flotilla" at seq=5; a live delta for "other" at seq=5
        // should still be printed (different StreamKey).
        let replay = [ns_delta("flotilla", 5)];
        let live = [ns_delta("other", 5)];
        let printed = events_printed_after_dedup(&replay, &live);
        assert_eq!(printed.len(), 1, "events for different namespaces should not suppress each other");
    }

    #[test]
    fn older_seq_live_event_is_suppressed() {
        // Replay has seq=10; if a stale live event with seq=8 arrives, suppress it.
        let replay = [ns_snapshot("flotilla", 10)];
        let live = [ns_delta("flotilla", 8)];
        let printed = events_printed_after_dedup(&replay, &live);
        assert!(printed.is_empty(), "stale live event (seq < replay_seq) should be suppressed");
    }
}

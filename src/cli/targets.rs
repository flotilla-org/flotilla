use std::path::{Path, PathBuf};

use color_eyre::Result;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{commands::CommandValue, Command, CommandAction, EnvironmentId, HostName, RepoSelector};
use tracing::info;

use flotilla_tui::cli::args::Cli;

use super::daemon::connect_daemon;

pub(super) async fn resolve_optional_host_node(cli: &Cli, host: Option<&str>) -> Result<Option<flotilla_protocol::NodeId>> {
    match host {
        Some(host) => {
            let (_environment_id, node_id) = resolve_host_target(cli, &HostName::new(host)).await?;
            Ok(Some(node_id))
        }
        None => Ok(None),
    }
}

pub(super) async fn resolve_host_target(cli: &Cli, subject: &HostName) -> Result<(EnvironmentId, flotilla_protocol::NodeId)> {
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let CommandValue::HostList(response) = result else {
        return Err(color_eyre::eyre::eyre!("unexpected response while resolving host"));
    };

    select_host_target(&response.hosts, subject)
}

fn select_host_target(
    hosts: &[flotilla_protocol::HostListEntry],
    subject: &HostName,
) -> Result<(EnvironmentId, flotilla_protocol::NodeId)> {
    let mut matches: Vec<_> = hosts.iter().filter(|entry| entry.host_name == *subject).collect();
    match matches.len() {
        0 => Err(color_eyre::eyre::eyre!("unknown host: {subject}")),
        1 => {
            let entry = matches.pop().expect("single host match");
            let environment_id = entry
                .environment_id
                .clone()
                .ok_or_else(|| color_eyre::eyre::eyre!("host {subject} is configured but has no known environment identity"))?;
            let node_id = entry
                .node
                .as_ref()
                .map(|node| node.node_id.clone())
                .ok_or_else(|| color_eyre::eyre::eyre!("host {subject} is configured but has no known node identity"))?;
            Ok((environment_id, node_id))
        }
        _ => {
            let mut ids: Vec<_> = matches
                .iter()
                .map(|entry| entry.environment_id.as_ref().map(EnvironmentId::canonical_string).unwrap_or_else(|| "configured".into()))
                .collect();
            ids.sort();
            Err(color_eyre::eyre::eyre!("ambiguous host: {subject} ({})", ids.join(", ")))
        }
    }
}

pub(super) async fn resolve_environment_target(
    cli: &Cli,
    target_environment_id: &EnvironmentId,
) -> Result<(flotilla_protocol::ProvisioningTarget, flotilla_protocol::NodeId)> {
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let CommandValue::HostList(response) = result else {
        return Err(color_eyre::eyre::eyre!("unexpected response while resolving environment"));
    };

    for entry in response.hosts {
        let (Some(environment_id), Some(node)) = (entry.environment_id, entry.node) else {
            continue;
        };
        if environment_id == *target_environment_id {
            return Ok((provisioning_target_for_environment(&entry.host_name, &environment_id), node.node_id));
        }

        let status = daemon
            .execute_query(
                Command {
                    node_id: Some(node.node_id.clone()),
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryHostStatus { target_environment_id: environment_id.clone() },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .map_err(|e| color_eyre::eyre::eyre!(e));

        let status = match status {
            Ok(status) => status,
            Err(err) => {
                info!(
                    host = %entry.host_name,
                    %environment_id,
                    node_id = %node.node_id,
                    error = %err,
                    "skipping host while resolving environment target"
                );
                continue;
            }
        };

        let CommandValue::HostStatus(response) = status else {
            return Err(color_eyre::eyre::eyre!("unexpected host status response while resolving environment"));
        };

        if response.visible_environments.iter().any(|environment| environment.environment_id() == target_environment_id) {
            return Ok((
                flotilla_protocol::ProvisioningTarget::ExistingEnvironment { host: entry.host_name, env_id: target_environment_id.clone() },
                node.node_id,
            ));
        }
    }

    Err(color_eyre::eyre::eyre!("unknown environment: {target_environment_id}"))
}

async fn lookup_cli_repository(daemon: &dyn DaemonHandle, selector: RepoSelector) -> Result<Option<RepoSelector>, String> {
    if matches!(selector, RepoSelector::Repository(_)) {
        return Ok(Some(selector));
    }
    match daemon
        .execute_query(Command::builder().action(CommandAction::QueryResolveRepository { repo: selector }).build(), uuid::Uuid::new_v4())
        .await?
    {
        CommandValue::RepositoryResolved { key } => Ok(key.map(RepoSelector::Repository)),
        CommandValue::Error { message } => Err(message),
        _ => Err("unexpected repository resolution response".into()),
    }
}

// Unprefixed relative strings remain identity queries (including forge slugs).
fn is_path_like_selector(query: &str) -> bool {
    Path::new(query).is_absolute() || query == "." || query == ".." || query.starts_with("./") || query.starts_with("../")
}

pub(super) async fn resolve_cli_repository(daemon: &dyn DaemonHandle, selector: RepoSelector) -> Result<RepoSelector, String> {
    let label = selector.to_string();
    lookup_cli_repository(daemon, selector.clone()).await?.ok_or_else(|| {
        if let RepoSelector::Query(query) = &selector {
            if is_path_like_selector(query) {
                return format!(
                    "repository path selectors are not supported: '{label}'; run from inside the observed checkout and omit --repo for cwd inference, or use a Repository key, project member alias, or forge slug"
                );
            }
        }
        format!("no Repository matches '{label}'; adopt a checkout with `flotilla repo add <path>` or declare a Project member")
    })
}

pub(super) async fn resolve_optional_cwd_repository(daemon: &dyn DaemonHandle, path: PathBuf) -> Result<Option<RepoSelector>, String> {
    lookup_cli_repository(daemon, RepoSelector::Path(path)).await
}

pub(super) async fn resolve_command_repositories(daemon: &dyn DaemonHandle, command: &mut Command) -> Result<(), String> {
    let original_context = command.context_repo.clone();
    if let Some(selector) = command.context_repo.take() {
        command.context_repo = match selector {
            // CLI Path context only comes from inferred cwd. Explicit --repo
            // and FLOTILLA_REPO use Query and refuse when they do not match.
            RepoSelector::Path(path) => resolve_optional_cwd_repository(daemon, path).await?,
            selector => Some(resolve_cli_repository(daemon, selector).await?),
        };
    }
    let selector = match &mut command.action {
        CommandAction::Checkout { repo, .. }
        | CommandAction::QueryIssues { repo, .. }
        | CommandAction::QueryIssueFetchByIds { repo, .. }
        | CommandAction::QueryIssueOpenInBrowser { repo, .. }
        | CommandAction::QueryRepoProviders { repo }
        | CommandAction::Refresh { repo: Some(repo) } => Some(repo),
        // Removing an observation root must also work after inspection failed.
        // Its explicit path selector therefore stays a host-local observation operation.
        CommandAction::UntrackRepo { repo } => {
            if let RepoSelector::Query(query) = repo {
                let path = Path::new(query);
                if is_path_like_selector(query) {
                    *repo = RepoSelector::Path(tokio::fs::canonicalize(path).await.unwrap_or_else(|_| path.to_path_buf()));
                    return Ok(());
                }
            }
            Some(repo)
        }
        _ => None,
    };
    if let Some(selector) = selector {
        if original_context.as_ref() == Some(selector) {
            if let Some(resolved) = &command.context_repo {
                *selector = resolved.clone();
                return Ok(());
            }
        }
        // An action's own repository selector is required, even when its
        // envelope contains no optional cwd context.
        *selector = resolve_cli_repository(daemon, selector.clone()).await?;
    }
    Ok(())
}

fn provisioning_target_for_environment(host: &HostName, environment_id: &EnvironmentId) -> flotilla_protocol::ProvisioningTarget {
    if environment_id.is_host() {
        flotilla_protocol::ProvisioningTarget::Host { host: host.clone() }
    } else {
        flotilla_protocol::ProvisioningTarget::ExistingEnvironment { host: host.clone(), env_id: environment_id.clone() }
    }
}

pub(super) fn resolve_repo_from_env(cli: &Cli) -> Option<RepoSelector> {
    match (&cli.repo, std::env::var("FLOTILLA_REPO").ok()) {
        (Some(repo), _) => Some(RepoSelector::Query(repo.clone())),
        (None, Some(repo)) if !repo.is_empty() => Some(RepoSelector::Query(repo)),
        _ => None,
    }
}

pub(super) fn set_context_repo(cmd: &mut Command, cli: &Cli) {
    if cmd.context_repo.is_some() {
        return;
    }
    cmd.context_repo = resolve_repo_from_env(cli).or_else(|| std::env::current_dir().ok().map(RepoSelector::Path));
}

pub(super) fn inject_repo_context(cmd: &mut Command, cli: &Cli) -> Result<()> {
    let repo_selector = resolve_repo_from_env(cli).or_else(|| std::env::current_dir().ok().map(RepoSelector::Path));

    match &mut cmd.action {
        CommandAction::Checkout { repo, .. } if *repo == RepoSelector::Query(String::new()) => {
            *repo = repo_selector
                .ok_or_else(|| color_eyre::eyre::eyre!("checkout create requires --repo, FLOTILLA_REPO, or an observed checkout"))?;
        }
        CommandAction::QueryIssues { repo, .. } if *repo == RepoSelector::Query(String::new()) => {
            if let Some(selector) = repo_selector {
                *repo = selector.clone();
                cmd.context_repo = Some(selector);
            }
        }
        _ => {
            if cmd.context_repo.is_none() {
                cmd.context_repo = repo_selector;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_tui::cli::args::{DomainCommand, SubCommand};

    use clap::Parser;
    use flotilla_protocol::{
        qualified_path::HostId, EnvironmentId, HostListEntry, HostName, NodeId, NodeInfo, PeerConnectionState, ProvisioningTarget,
    };
    #[test]
    fn issue_list_without_repo_infers_cwd_without_adopting_it() {
        let cli = Cli::try_parse_from(["flotilla", "issue"]).expect("issue list");
        let Some(SubCommand::Domain(DomainCommand::Issue(noun))) = &cli.command else { panic!("issue noun") };
        let flotilla_commands::Resolved::NeedsContext { mut command, .. } = noun.clone().resolve().expect("issue list command") else {
            panic!("issue list requires repo context");
        };
        super::inject_repo_context(&mut command, &cli).expect("infer cwd repository context");
        assert!(
            matches!(command.action, super::CommandAction::QueryIssues { repo: super::RepoSelector::Path(ref path), .. } if path == &std::env::current_dir().expect("cwd"))
        );
    }

    // #1769: CLI preflight resolves aliases and slugs to the same Repository key,
    // and cwd is optional outside observed checkouts; no path is adopted by lookup.
    #[tokio::test]
    async fn cli_repository_preflight_uses_durable_identity() {
        use std::sync::Arc;

        use flotilla_core::config::ConfigStore;
        use flotilla_core::in_process::InProcessDaemon;
        use flotilla_discovery_testkit::fake_discovery_with_provider_set;
        use flotilla_discovery_testkit::FakeDiscoveryProviders;
        use flotilla_discovery_testkit::FakeIssueProvider;
        use flotilla_paths::path_context::ExecutionEnvironmentPath;
        use flotilla_protocol::{Command, CommandAction, DaemonEvent, RepoSelector};
        use flotilla_resources::{
            Checkout, CheckoutSpec, InputMeta, ObservedCheckoutSpec, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec,
            Repository, RepositorySpec,
        };

        let temp = tempfile::tempdir().expect("config");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"cli-addressing-test\"\n").expect("daemon identity");
        // Fake provider stands in for the external issue tracker API.
        let issues = Arc::new(FakeIssueProvider::new());
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = InProcessDaemon::new(
            Vec::new(),
            config.clone(),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_issue_tracker(issues.clone())),
            HostName::new("test"),
        )
        .await;
        let backend = daemon.resource_backend();
        let spec = RepositorySpec::remote("https://github.com/acme/widgets").expect("repository");
        let key = spec.key();
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
            .await
            .expect("declare Repository");
        let project = ProjectSpec::builder()
            .display_name("Widgets".into())
            .default_workflow_ref("single-agent".into())
            .repositories(vec![ProjectRepositorySpec::builder()
                .repo(key.clone())
                .alias("primary".into())
                .roles([ProjectRepositoryRole::Code].into())
                .build()])
            .build();
        backend
            .using::<Project>("flotilla")
            .create(&InputMeta::builder().name("widgets".into()).build(), &project)
            .await
            .expect("declare Project");
        for query in ["primary", "acme/widgets"] {
            let mut command = Command::builder()
                .context_repo(RepoSelector::Query(query.into()))
                .action(CommandAction::OpenIssue { id: "1".into() })
                .build();
            super::resolve_command_repositories(&*daemon, &mut command).await.expect("preflight");
            assert_eq!(command.context_repo, Some(RepoSelector::Repository(key.clone())));
        }
        assert_eq!(super::resolve_optional_cwd_repository(&*daemon, "/unobserved".into()).await.expect("optional cwd"), None);
        for args in [
            vec!["flotilla", "cr"],
            vec!["flotilla", "agent"],
            vec!["flotilla", "cr", "123", "open"],
            vec!["flotilla", "agent", "session", "archive"],
        ] {
            let cli = Cli::try_parse_from(args).expect("CLI");
            let resolved = match cli.command.as_ref().expect("noun") {
                SubCommand::Domain(DomainCommand::Cr(noun)) => noun.clone().resolve(),
                SubCommand::Domain(DomainCommand::Agent(noun)) => noun.clone().resolve(),
                _ => unreachable!(),
            }
            .expect("command");
            let mut command = match resolved {
                flotilla_commands::Resolved::Ready(command) => command,
                flotilla_commands::Resolved::NeedsContext { mut command, .. } => {
                    super::inject_repo_context(&mut command, &cli).expect("cwd");
                    command
                }
                _ => panic!("expected command or inferred command"),
            };
            super::resolve_command_repositories(&*daemon, &mut command).await.expect("optional inferred cwd");
            assert_eq!(command.context_repo, None);
            command.context_repo = Some(RepoSelector::Query("missing-explicit-repo".into()));
            assert!(super::resolve_command_repositories(&*daemon, &mut command).await.is_err());
        }
        let mut required = Command::builder()
            .context_repo(RepoSelector::Path("/unobserved".into()))
            .action(CommandAction::QueryIssues {
                repo: RepoSelector::Path("/unobserved".into()),
                params: Default::default(),
                page: 0,
                count: 10,
            })
            .build();
        assert!(super::resolve_command_repositories(&*daemon, &mut required).await.is_err());
        let observed = daemon.observed_resource_backend();
        let checkout = CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("main".into())
                .path("/work/widgets".into())
                .repo_ref(key.clone())
                .host_ref(daemon.local_host_id().expect("host id").to_string())
                .is_main(true)
                .build(),
        );
        observed
            .using::<Checkout>("flotilla")
            .create(&InputMeta::builder().name("widgets-checkout".into()).build(), &checkout)
            .await
            .expect("observed checkout");
        assert_eq!(
            super::resolve_optional_cwd_repository(&*daemon, "/work/widgets/src".into()).await.expect("cwd identity"),
            Some(RepoSelector::Repository(key.clone()))
        );
        // #2551: explicit path queries stay unsupported even for observed checkouts.
        // Single CLI call-through: the diagnostic directs users to identity or cwd inference.
        for path in ["/work/widgets", "/work/widgets/src", "./widgets", "../widgets", ".", ".."] {
            let cli = Cli::try_parse_from(["flotilla", "repo", path, "checkout", "--fresh", "feature"]).expect("CLI");
            let SubCommand::Domain(DomainCommand::Repo(noun)) = cli.command.expect("repo noun") else { panic!("repo noun") };
            let flotilla_commands::Resolved::Ready(mut command) = noun.resolve().expect("checkout command") else {
                panic!("ready checkout")
            };
            let error = super::resolve_command_repositories(&*daemon, &mut command).await.expect_err("explicit path refused");
            assert!(error.contains("repository path selectors are not supported"), "{error}");
            assert!(error.contains("cwd inference"), "{error}");
            assert!(error.contains("Repository key"), "{error}");
        }
        // Bare relative strings are identity queries, not filesystem path selectors.
        let error = super::resolve_cli_repository(&*daemon, RepoSelector::Query("widgets".into())).await.expect_err("unknown identity");
        assert!(error.starts_with("no Repository matches 'widgets'"), "{error}");
        let mut query = Command::builder()
            .action(CommandAction::QueryIssueFetchByIds { repo: RepoSelector::Query("primary".into()), ids: vec!["1".into()] })
            .build();
        super::resolve_command_repositories(&*daemon, &mut query).await.expect("issue query identity");
        use flotilla_daemon_api::daemon::DaemonHandle;
        let result = daemon.execute_query(query, uuid::Uuid::new_v4()).await.expect("issue query without checkout");
        assert!(matches!(result, CommandValue::IssuesByIds { .. }));
        assert_eq!(*issues.fetched_by_id.lock().await, vec![vec!["1".to_string()]]);
        // Slice 2: stop observing an unavailable path without requiring its
        // inspection to have succeeded, and retain durable Repository/Project intent.
        config.add_observation_root(&ExecutionEnvironmentPath::new("/unavailable/checkout")).expect("observation root");
        let mut remove =
            Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Query("/unavailable/checkout".into()) }).build();
        super::resolve_command_repositories(&*daemon, &mut remove).await.expect("remove path preflight");
        assert_eq!(remove.action, CommandAction::UntrackRepo { repo: RepoSelector::Path("/unavailable/checkout".into()) });
        let mut events = daemon.subscribe();
        let command_id = daemon.execute(remove).await.expect("stop observing");
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(DaemonEvent::CommandFinished { command_id: finished, result, .. }) = events.recv().await {
                    if finished == command_id {
                        break result;
                    }
                }
            }
        })
        .await
        .expect("remove result");
        assert!(matches!(result, CommandValue::RepoUntracked { .. }));
        assert!(config.load_observation_roots().expect("roots").is_empty());
        assert!(backend.using::<Repository>("flotilla").get(&key.to_string()).await.is_ok());
        assert!(backend.using::<Project>("flotilla").get("widgets").await.is_ok());
        assert!(daemon.tracked_repo_paths().await.is_empty());
    }

    #[test]
    fn host_target_selection_uses_host_facing_name() {
        let hosts = vec![HostListEntry {
            environment_id: Some(EnvironmentId::host(HostId::new("desktop-a"))),
            host_name: HostName::new("desktop"),
            node: Some(NodeInfo::new(NodeId::new("node-a"), "Builder")),
            is_local: false,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            reconnect: None,
            has_summary: true,
            repo_count: 0,
        }];

        let (environment_id, node_id) = select_host_target(&hosts, &HostName::new("desktop")).expect("resolve host");
        assert_eq!(environment_id, EnvironmentId::host(HostId::new("desktop-a")));
        assert_eq!(node_id, NodeId::new("node-a"));
    }

    #[test]
    fn host_target_selection_reports_ambiguity_for_duplicate_host_names() {
        let hosts = vec![
            HostListEntry {
                environment_id: Some(EnvironmentId::host(HostId::new("desktop-a"))),
                host_name: HostName::new("desktop"),
                node: Some(NodeInfo::new(NodeId::new("node-a"), "Desktop")),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                reconnect: None,
                has_summary: true,
                repo_count: 0,
            },
            HostListEntry {
                environment_id: Some(EnvironmentId::host(HostId::new("desktop-b"))),
                host_name: HostName::new("desktop"),
                node: Some(NodeInfo::new(NodeId::new("node-b"), "Desktop")),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                reconnect: None,
                has_summary: true,
                repo_count: 0,
            },
        ];

        let err = select_host_target(&hosts, &HostName::new("desktop")).expect_err("duplicate host names should be ambiguous");
        let message = err.to_string();
        assert!(message.contains("ambiguous host: desktop"), "unexpected error: {message}");
        assert!(message.contains("desktop-a"), "unexpected error: {message}");
        assert!(message.contains("desktop-b"), "unexpected error: {message}");
    }

    #[test]
    fn provisioning_target_for_host_environment_uses_host_target() {
        let host = HostName::new("desktop");
        let environment_id = EnvironmentId::host(HostId::new("desktop-a"));

        let target = provisioning_target_for_environment(&host, &environment_id);
        assert_eq!(target, ProvisioningTarget::Host { host });
    }

    #[test]
    fn provisioning_target_for_non_host_environment_preserves_environment_identity() {
        let host = HostName::new("desktop");
        let environment_id = EnvironmentId::new("builder-1");

        let target = provisioning_target_for_environment(&host, &environment_id);
        assert_eq!(target, ProvisioningTarget::ExistingEnvironment { host, env_id: environment_id });
    }
}

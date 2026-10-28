//! A `[dummy_fleet]` daemon run over copied stores, which name real forges,
//! repositories and issue sources, never writes to any of them.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    runtime::{DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_api::detectors::{parse_first_dotted_version, CommandDetector};
use flotilla_discovery_testkit::fake_discovery_with_runner;
use flotilla_fake_forge::{FakeForge, ForgeSettings, InProcessClient, NewPull, RepoRef, SystemClock};
use flotilla_forgejo_providers::factories::{change_request::ForgejoChangeRequestFactory, issue_tracker::ForgejoIssueProviderFactory};
use flotilla_github_providers::factories::{change_request::GitHubChangeRequestFactory, issue_tracker::GitHubIssueProviderFactory};
use flotilla_protocol::{commands::CommandValue, Command, CommandAction, DaemonEvent, HostName, IssueSource, RepoSelector};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner, HttpClient};
use flotilla_resources::{
    CredentialConsumer, CredentialLifecycle, CredentialSource, CredentialSpec, CredentialSpecSpec, Forge, ForgeKind, ForgeSpec, InputMeta,
    Project, ProjectRepositorySpec, ProjectSpec, Repository, RepositoryKey, RepositorySpec,
};

const NAMESPACE: &str = "flotilla";
const LAB_TOKEN: &str = "lab-daemon-token";

/// Every request that reached a real forge, by transport.
#[derive(Default)]
struct Outbound {
    reads: Mutex<Vec<String>>,
    writes: Mutex<Vec<String>>,
}

impl Outbound {
    fn record(&self, write: bool, request: String) {
        let log = if write { &self.writes } else { &self.reads };
        log.lock().expect("outbound log").push(request);
    }

    fn writes(&self) -> Vec<String> {
        self.writes.lock().expect("outbound log").clone()
    }
}

/// The Forgejo forge, served by the strict fake behind an HTTP stand-in.
struct ForgeHttp {
    outbound: Arc<Outbound>,
    forge: InProcessClient,
}

#[async_trait]
impl HttpClient for ForgeHttp {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let write = !matches!(*request.method(), reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS);
        self.outbound.record(write, format!("{} {}", request.method(), request.url()));
        self.forge.execute(request, label).await
    }
}

/// The host's `gh` and `git`: reads answer, writes are recorded.
struct ForgeCommands {
    outbound: Arc<Outbound>,
}

impl ForgeCommands {
    fn is_write(cmd: &str, args: &[&str]) -> bool {
        match (cmd, args) {
            ("git", ["push", ..]) => true,
            ("gh", ["api", rest @ ..]) => {
                rest.iter().any(|arg| matches!(*arg, "-X" | "--method" | "-f" | "-F" | "--field" | "--raw-field" | "--input"))
            }
            ("gh", ["--version"] | ["auth", "status", ..]) => false,
            ("gh", [_, "view" | "list" | "status" | "checks", ..]) => false,
            ("gh", _) => true,
            _ => false,
        }
    }
}

#[async_trait]
impl CommandRunner for ForgeCommands {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        let write = Self::is_write(cmd, args);
        if cmd == "gh" || write {
            self.outbound.record(write, format!("{cmd} {}", args.join(" ")));
        }
        match (cmd, args) {
            ("git", ["--version"]) => Ok("git version 2.43.0".into()),
            ("gh", ["--version"]) => Ok("gh version 2.49.0".into()),
            ("gh", ["api", "--include", endpoint]) if endpoint.starts_with("repos/acme/app/pulls/") => Ok(format!(
                "HTTP/2.0 200 OK\n\n{}",
                serde_json::json!({"number": 7, "title": "Real work", "head": {"ref": "feature"}, "body": "Notes", "state": "open"})
            )),
            _ if write => Ok(String::new()),
            _ => Err(format!("{cmd} {args:?} is not available")),
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        matches!(cmd, "gh" | "git")
    }
}

struct CopiedStoreDaemon {
    daemon: Arc<InProcessDaemon>,
    outbound: Arc<Outbound>,
    repositories: Vec<RepositoryKey>,
    _runtime: DaemonRuntime,
    _dir: tempfile::TempDir,
}

impl CopiedStoreDaemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("daemon.toml"),
            "machine_id = \"rehearsal\"\n\n[credentials.forgejo]\nlab = \"lab-daemon\"\n\n[dummy_fleet]\n",
        )
        .expect("daemon.toml");
        let token = dir.path().join("lab-token");
        std::fs::write(&token, format!("{LAB_TOKEN}\n")).expect("token");

        let fake = Arc::new(FakeForge::new("https://lab.example", Arc::new(SystemClock)));
        fake.add_forge("lab", ForgeSettings { token: LAB_TOKEN.into(), login: "daemon".into() }).expect("lab forge");
        let lab_repo = RepoRef::new("lab", "acme", "app");
        fake.seed_repository(&lab_repo, "main").expect("lab repository");
        let pull = NewPull {
            head: "feature".into(),
            base: "main".into(),
            title: "Real work".into(),
            body: "Notes".into(),
            user: None,
            draft: false,
        };
        let number = fake.create_pull(&lab_repo, pull).expect("lab pull request").number;
        assert_eq!(number, 1);

        let outbound = Arc::new(Outbound::default());
        let http: Arc<dyn HttpClient> =
            Arc::new(ForgeHttp { outbound: Arc::clone(&outbound), forge: InProcessClient::new(Arc::clone(&fake)) });
        let mut discovery = fake_discovery_with_runner(false, Arc::new(ForgeCommands { outbound: Arc::clone(&outbound) }));
        Arc::get_mut(&mut discovery.host_detectors).expect("unshared detectors").push(Box::new(CommandDetector::new(
            "gh",
            &["--version"],
            parse_first_dotted_version,
        )));
        discovery.factories.change_requests =
            vec![Box::new(GitHubChangeRequestFactory), Box::new(ForgejoChangeRequestFactory::new(Arc::clone(&http)))];
        discovery.factories.issue_trackers =
            vec![Box::new(GitHubIssueProviderFactory), Box::new(ForgejoIssueProviderFactory::new(Arc::clone(&http)))];
        discovery.http = http;

        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        let daemon = InProcessDaemon::new(
            vec![],
            Arc::clone(&config),
            discovery,
            HostName::new("rehearsal"),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
        let options = RuntimeOptions {
            namespace: NAMESPACE.into(),
            heartbeat_interval: Duration::from_secs(5),
            controller_resync_interval: Duration::from_millis(200),
            ..RuntimeOptions::default()
        };
        let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");

        let backend = daemon.resource_backend();
        let github = ForgeSpec::builder()
            .forge_id("github".into())
            .kind(ForgeKind::Github)
            .hosts(BTreeSet::from(["github.com".into()]))
            .https_url("https://github.com".into())
            .git_ssh_host("github.com".into())
            .build();
        let lab = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["lab.example".into()]))
            .https_url(fake.service_url("lab"))
            .git_ssh_host("lab.example".into())
            .build();
        let mut repositories = Vec::new();
        let mut sources = Vec::new();
        for forge in [github, lab] {
            backend
                .definitions::<Forge>(NAMESPACE)
                .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
                .await
                .expect("Forge");
            let spec =
                RepositorySpec::remote(format!("{}/acme/app", forge.https_url)).expect("remote").on_forge(&forge).expect("Forge identity");
            backend
                .using::<Repository>(NAMESPACE)
                .create(&InputMeta::builder().name(spec.key().to_string()).build(), &spec)
                .await
                .expect("Repository");
            repositories.push(spec.key());
            sources.push(IssueSource { service: forge.https_url.clone(), scope: "acme/app".into() });
        }
        backend
            .definitions::<CredentialSpec>(NAMESPACE)
            .create(
                &InputMeta::builder().name("lab-daemon".into()).build(),
                &CredentialSpecSpec::builder()
                    .consumer(CredentialConsumer::Forgejo { forge_ref: "lab".into(), username: "daemon".into() })
                    .source(CredentialSource::File { path: token.to_string_lossy().into_owned() })
                    .lifecycle(CredentialLifecycle::Static)
                    .build(),
            )
            .await
            .expect("lab credential");
        backend
            .definitions::<Project>(NAMESPACE)
            .create(
                &InputMeta::builder().name("app".into()).build(),
                &ProjectSpec::builder()
                    .display_name("App".into())
                    .default_workflow_ref("single-agent".into())
                    .repositories(repositories.iter().map(|key| ProjectRepositorySpec::builder().repo(key.clone()).build()).collect())
                    .issue_source_bindings(sources.into_iter().map(Into::into).collect())
                    .build(),
            )
            .await
            .expect("Project");
        Self { daemon, outbound, repositories, _runtime: runtime, _dir: dir }
    }

    async fn run(&self, action: CommandAction, repository: &RepositoryKey) -> CommandValue {
        let mut events = self.daemon.subscribe();
        let command = Command::builder().action(action).context_repo(RepoSelector::Repository(repository.clone())).build();
        let id = self.daemon.execute(command).await.expect("admit command");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
                    if command_id == id {
                        return result;
                    }
                }
            }
        })
        .await
        .expect("command finishes")
    }
}

#[tokio::test]
async fn a_dummy_fleet_daemon_never_writes_to_the_forges_its_stores_name() {
    let daemon = CopiedStoreDaemon::start().await;
    let (github, lab) = (&daemon.repositories[0], &daemon.repositories[1]);
    for (repository, id) in [(github, "7"), (lab, "1")] {
        for action in [
            CommandAction::LinkIssuesToChangeRequest { change_request_id: id.into(), issue_ids: vec!["10".into()] },
            CommandAction::CloseChangeRequest { id: id.into() },
            CommandAction::MergeChangeRequest { id: id.into(), confirmed: true },
        ] {
            let result = daemon.run(action.clone(), repository).await;
            assert!(
                matches!(&result, CommandValue::Error { message } if message.contains("dummy fleet refuses writes")),
                "{action:?} on {repository}: {result:?}"
            );
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(daemon.outbound.writes(), Vec::<String>::new());
}

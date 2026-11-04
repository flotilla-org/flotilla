//! A `[dummy_fleet]` daemon run over copied stores, which name real forges,
//! repositories and issue sources, never writes to any of them.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_daemon_testkit::daemon::{ForgeTransport, TestDaemon};
use flotilla_fake_forge::{FakeForge, ForgeSettings, InProcessClient, NewPull, RepoRef, SystemClock};
use flotilla_protocol::{commands::CommandValue, CommandAction, IssueSource};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner, HttpClient};
use flotilla_resources::{
    CredentialConsumer, CredentialLifecycle, CredentialSource, CredentialSpec, CredentialSpecSpec, Forge, ForgeKind, ForgeSpec, Project,
    ProjectRepositorySpec, ProjectSpec, Repository, RepositoryKey, RepositorySpec,
};

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

/// A `[dummy_fleet]` daemon whose stores name a real GitHub repository and a
/// Forgejo repository on the strict fake forge, with the real providers for
/// both installed behind recording transports.
async fn copied_store_daemon(token: &Path) -> (TestDaemon, Arc<Outbound>, Vec<RepositoryKey>) {
    let fake = Arc::new(FakeForge::new("https://lab.example", Arc::new(SystemClock)));
    fake.add_forge("lab", ForgeSettings { token: LAB_TOKEN.into(), login: "daemon".into() }).expect("lab forge");
    let lab_repo = RepoRef::new("lab", "acme", "app");
    fake.seed_repository(&lab_repo, "main").expect("lab repository");
    let pull =
        NewPull { head: "feature".into(), base: "main".into(), title: "Real work".into(), body: "Notes".into(), user: None, draft: false };
    let number = fake.create_pull(&lab_repo, pull).expect("lab pull request").number;
    assert_eq!(number, 1);

    let outbound = Arc::new(Outbound::default());
    let forges = ForgeTransport {
        http: Arc::new(ForgeHttp { outbound: Arc::clone(&outbound), forge: InProcessClient::new(Arc::clone(&fake)) }),
        commands: Arc::new(ForgeCommands { outbound: Arc::clone(&outbound) }),
    };
    let daemon = TestDaemon::builder()
        .settings("host_name = \"rehearsal\"\n\n[credentials.forgejo]\nlab = \"lab-daemon\"\n")
        .forges(forges)
        .start()
        .await;

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
        daemon.apply::<Forge>(&forge.forge_id.clone(), forge.clone()).await;
        let spec =
            RepositorySpec::remote(format!("{}/acme/app", forge.https_url)).expect("remote").on_forge(&forge).expect("Forge identity");
        daemon.apply::<Repository>(&spec.key().to_string(), spec.clone()).await;
        repositories.push(spec.key());
        sources.push(IssueSource { service: forge.https_url.clone(), scope: "acme/app".into() });
    }
    let credential = CredentialSpecSpec::builder()
        .consumer(CredentialConsumer::Forgejo { forge_ref: "lab".into(), username: "daemon".into() })
        .source(CredentialSource::File { path: token.to_string_lossy().into_owned() })
        .lifecycle(CredentialLifecycle::Static)
        .build();
    daemon.apply::<CredentialSpec>("lab-daemon", credential).await;
    let project = ProjectSpec::builder()
        .display_name("App".into())
        .default_workflow_ref("single-agent".into())
        .repositories(repositories.iter().map(|key| ProjectRepositorySpec::builder().repo(key.clone()).build()).collect())
        .issue_source_bindings(sources.into_iter().map(Into::into).collect())
        .build();
    daemon.apply::<Project>("app", project).await;
    (daemon, outbound, repositories)
}

#[tokio::test]
async fn a_dummy_fleet_daemon_never_writes_to_the_forges_its_stores_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = dir.path().join("lab-token");
    std::fs::write(&token, format!("{LAB_TOKEN}\n")).expect("token");
    let (daemon, outbound, repositories) = copied_store_daemon(&token).await;
    let (github, lab) = (&repositories[0], &repositories[1]);
    for (repository, id) in [(github, "7"), (lab, "1")] {
        for action in [
            CommandAction::LinkIssuesToChangeRequest { change_request_id: id.into(), issue_ids: vec!["10".into()] },
            CommandAction::CloseChangeRequest { id: id.into() },
            CommandAction::MergeChangeRequest { id: id.into(), confirmed: true },
        ] {
            let result = daemon.execute_in(repository, action.clone()).await;
            assert!(
                matches!(&result, CommandValue::Error { message } if message.contains("dummy fleet refuses writes")),
                "{action:?} on {repository}: {result:?}"
            );
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(outbound.writes(), Vec::<String>::new());
}

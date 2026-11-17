// The daemon's own Git work on this host (the managed clone, and the
// settlement inspection's ls-remote) signs in to a Forgejo forge as the
// identity `[credentials.forgejo]` maps it to, against a forge that refuses
// anonymous Git as a private Forgejo repository does.
use super::*;
use flotilla_fake_forge::{serve, Config as FakeForgeConfig, ForgeSettings, SystemClock};
use flotilla_vcs::checkout_integration::{inspect_recorded_checkout_integration, ChangeRequestIntegrationInput, RecordedAssociation};

const LAB_TOKEN: &str = "lab-daemon-token";

fn fake_forge_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../flotilla-fake-forge/tests/fixtures").join(name)
}

/// The host's Git: it trusts the rehearsal CA, cannot prompt, and reads no
/// configuration of the machine running the test.
struct HostGit;

impl HostGit {
    async fn git(args: &[&str], cwd: &Path) -> Result<CommandOutput, String> {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_SSL_CAINFO", fake_forge_fixture("ca.pem"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|error| error.to_string())?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
        })
    }
}

#[async_trait]
impl CommandRunner for HostGit {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd != "git" {
            return ProcessCommandRunner.run(cmd, args, cwd, label).await;
        }
        let output = Self::git(args, cwd).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        if cmd != "git" {
            return ProcessCommandRunner.run_output(cmd, args, cwd, label).await;
        }
        Self::git(args, cwd).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
    async fn same_filesystem(&self, source: &Path, destination: &Path) -> Result<bool, String> {
        ProcessCommandRunner.same_filesystem(source, destination).await
    }
}

struct LabForge {
    temp: TempDir,
    running: serve::Running,
    daemon: Arc<InProcessDaemon>,
    clones: RoutingCloneRuntime,
    url: String,
    /// The lab forge's `https_url`, which a Repository on it names as its service.
    service_url: String,
    head: String,
}

impl LabForge {
    /// Map the lab forge to the daemon's identity in `daemon.toml`, or not.
    fn map_identity(&self, mapped: bool) {
        let mapping = if mapped { "[credentials.forgejo]\nlab = \"lab-daemon\"\n" } else { "" };
        fs::write(self.temp.path().join("daemon.toml"), format!("machine_id = \"forge-identity-test\"\n{mapping}")).expect("daemon config");
    }

    async fn clone(&self, target: &Path) -> Result<Option<String>, String> {
        self.clones.clone_and_inspect(&self.url, target.to_str().expect("target")).await
    }

    /// The remote refs the settlement inspection observes for the checkout at `path` on `main`.
    async fn observed_remote_refs(&self, path: &Path) -> BTreeMap<String, String> {
        let vcs = self.daemon.local_vcs_for_checkout(path).await.expect("checkout VCS");
        let spec = CheckoutSpec::Observed(ResourceObservedCheckoutSpec {
            r#ref: "main".into(),
            path: path.to_string_lossy().into(),
            repo_ref: RepositoryKey("widgets".into()),
            host_ref: "test-host".into(),
            is_main: true,
        });
        let association = RecordedAssociation { change_request_id: None, complete: true, observe_remote_ref: true };
        let recorded = ChangeRequestIntegrationInput { status: None, landed_evidence: None };
        let status = inspect_recorded_checkout_integration(&HostGit, vcs.as_ref(), path, &spec, association, recorded).await;
        status.remote_refs.into_iter().map(|(reference, observed)| (reference, observed.digest)).collect()
    }
}

/// A private `acme/widgets` on the fake's `lab` forge, and a daemon whose host
/// Git reaches it, with the forge and the daemon's file-sourced identity on it declared.
async fn lab_forge() -> LabForge {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("source");
    for args in
        [&["init", "-q", "-b", "main"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "seed"]]
    {
        assert!(ProcessCommand::new("git").args(args).current_dir(&source).status().expect("seed source").success());
    }
    let head = ProcessCommand::new("git").args(["rev-parse", "HEAD"]).current_dir(&source).output().expect("head");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let root = temp.path().join("forge-git");
    fs::create_dir_all(root.join("lab/acme")).expect("forge root");
    let bare = root.join("lab/acme/widgets.git");
    assert!(ProcessCommand::new("git")
        .args(["clone", "-q", "--bare", source.to_str().expect("source"), bare.to_str().expect("bare")])
        .status()
        .expect("bare repository")
        .success());
    let config = FakeForgeConfig {
        public_url: "https://localhost:3443".into(),
        listen: "127.0.0.1:0".parse().expect("addr"),
        admin_listen: "127.0.0.1:0".parse().expect("addr"),
        tls_cert: fake_forge_fixture("server.pem"),
        tls_key: fake_forge_fixture("server-key.pem"),
        admin_token: "admin-token".into(),
        auto_merge_after: None,
        seed: None,
        git_root: Some(root),
        forges: [("lab".to_string(), ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() })].into(),
    };
    let fake = serve::build_forge(&config, Arc::new(SystemClock)).expect("fake forge");
    let running = serve::start(&config, fake).await.expect("serve the fake forge");
    let origin = format!("https://localhost:{}", running.api_addr().port());

    let token_file = temp.path().join("lab-daemon-token");
    fs::write(&token_file, format!("{LAB_TOKEN}\n")).expect("token file");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon =
        daemon_with_backend_and_runner(Vec::new(), Arc::clone(&config), flotilla_store_memory::engine_home_backend(), Arc::new(HostGit))
            .await;
    engine_environments(&daemon);
    let forge = ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["localhost".to_string()]))
        .https_url(format!("{origin}/lab"))
        .git_ssh_host("localhost".to_string())
        .build();
    daemon.resource_backend().definitions::<Forge>(NAMESPACE).create(&empty_meta("lab"), &forge).await.expect("Forge");
    daemon
        .resource_backend()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("lab-daemon"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { forge_ref: "lab".into(), username: "lab-bot".into() },
                source: CredentialSource::File { path: token_file.to_str().expect("token path").into() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("daemon identity");
    let host_id = daemon.local_host_id().expect("local host ID").to_string();
    let host_direct = flotilla_resources::host_direct_environment_name(&host_id);
    let state = ControllerRuntimeState::new(Arc::clone(&daemon), config, passthrough_registry(), None, host_id, host_direct);
    let forge = LabForge {
        temp,
        running,
        daemon,
        clones: RoutingCloneRuntime { state: Arc::new(state) },
        url: format!("{origin}/lab/acme/widgets.git"),
        service_url: format!("{origin}/lab"),
        head,
    };
    forge.map_identity(true);
    forge
}

#[tokio::test]
async fn the_managed_clone_signs_in_as_the_daemon_and_fails_without_its_identity() {
    let forge = lab_forge().await;

    forge.map_identity(false);
    let refused = forge.clone(&forge.temp.path().join("refused")).await.expect_err("an anonymous clone is refused");
    assert!(refused.contains("could not read Username"), "{refused}");
    assert!(!forge.temp.path().join("refused").exists());

    forge.map_identity(true);
    let target = forge.temp.path().join("managed/widgets");
    let branch = forge.clone(&target).await.expect("the clone signs in as the daemon");
    assert_eq!(branch.as_deref(), Some("main"));
    assert!(target.join(".git").is_dir());
    let config = fs::read_to_string(target.join(".git/config")).expect("clone config");
    assert!(!config.contains(LAB_TOKEN) && !config.contains("credential"), "{config}");
    forge.running.shutdown().await;
}

// A fresh host has no checkout or clone of the repository to observe its default branch from.
// What the Repository controller reads it from asks the forge's Git remote, as the daemon.
#[tokio::test]
async fn a_fresh_host_reads_a_forge_repositorys_default_branch_from_its_remote_as_the_daemon() {
    let forge = lab_forge().await;
    let resolver = super::super::forge_default_branches(&forge.clones.state);
    let repository = flotilla_resources::ForgeIdentity { service_url: forge.service_url.clone(), repository: "acme/widgets".into() };

    forge.map_identity(false);
    let refused = resolver.default_branch(&repository).await.expect_err("an anonymous read is refused");
    assert!(refused.contains("could not read Username"), "{refused}");

    forge.map_identity(true);
    assert_eq!(resolver.default_branch(&repository).await.expect("read as the daemon").as_deref(), Some("main"));
    forge.running.shutdown().await;
}

#[tokio::test]
async fn settlement_observes_the_remote_ref_as_the_daemon_and_nothing_without_its_identity() {
    let forge = lab_forge().await;
    let target = forge.temp.path().join("managed/widgets");
    forge.clone(&target).await.expect("the clone signs in as the daemon");

    let observed = forge.observed_remote_refs(&target).await;
    assert_eq!(observed, BTreeMap::from([("refs/heads/main".to_string(), forge.head.clone())]));

    forge.map_identity(false);
    assert_eq!(forge.observed_remote_refs(&target).await, BTreeMap::new());
    forge.running.shutdown().await;
}

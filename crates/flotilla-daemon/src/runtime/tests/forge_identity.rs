// The daemon's own Git work on this host (the managed clone, and the
// settlement inspection's ls-remote) signs in to a Forgejo forge as the
// identity `[credentials.forgejo]` maps it to, against a forge that refuses
// anonymous Git as a private Forgejo repository does.
use super::*;
use flotilla_fake_forge::{serve, Config as FakeForgeConfig, ForgeSettings, SystemClock};
use flotilla_vcs::checkout_integration::{inspect_recorded_checkout_integration, ChangeRequestIntegrationInput, RecordedAssociation};

const LAB_TOKEN: &str = "lab-daemon-token";
/// A second forge on the fake's host, beside `lab` under its own path.
const LAB2_TOKEN: &str = "lab2-daemon-token";

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
        self.map_identities(if mapped { &["lab"] } else { &[] });
    }

    /// Map each of `forges` to its daemon identity, `<forge>-daemon`, in `daemon.toml`.
    fn map_identities(&self, forges: &[&str]) {
        let mut config = "machine_id = \"forge-identity-test\"\n".to_string();
        if !forges.is_empty() {
            config.push_str("[credentials.forgejo]\n");
            for forge in forges {
                config.push_str(&format!("{forge} = \"{forge}-daemon\"\n"));
            }
        }
        fs::write(self.temp.path().join("daemon.toml"), config).expect("daemon config");
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

/// Declares the daemon's identity on `forge_id`, `<forge_id>-daemon`, sourced from its token file under `dir`.
async fn declare_identity(daemon: &InProcessDaemon, dir: &Path, forge_id: &str) {
    let token_file = dir.join(format!("{forge_id}-daemon-token"));
    daemon
        .resource_backend()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta(&format!("{forge_id}-daemon")),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { forge_ref: forge_id.into(), username: format!("{forge_id}-bot") },
                source: CredentialSource::File { path: token_file.to_str().expect("token path").into() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("daemon identity");
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
    for repository in ["lab/acme/widgets.git", "lab2/acme/gadgets.git"] {
        let bare = root.join(repository);
        fs::create_dir_all(bare.parent().expect("owner")).expect("forge root");
        assert!(ProcessCommand::new("git")
            .args(["clone", "-q", "--bare", source.to_str().expect("source"), bare.to_str().expect("bare")])
            .status()
            .expect("bare repository")
            .success());
    }
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
        forges: [
            ("lab".to_string(), ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() }),
            ("lab2".to_string(), ForgeSettings { token: LAB2_TOKEN.into(), login: "lab2-bot".into() }),
        ]
        .into(),
    };
    let fake = serve::build_forge(&config, Arc::new(SystemClock)).expect("fake forge");
    let running = serve::start(&config, fake).await.expect("serve the fake forge");
    let origin = format!("https://localhost:{}", running.api_addr().port());

    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon =
        daemon_with_backend_and_runner(Vec::new(), Arc::clone(&config), flotilla_store_memory::engine_home_backend(), Arc::new(HostGit))
            .await;
    engine_environments(&daemon);
    // Each forge and the daemon's file-sourced identity on it: `lab`, and `lab2` beside it.
    for (forge_id, token) in [("lab", LAB_TOKEN), ("lab2", LAB2_TOKEN)] {
        let token_file = temp.path().join(format!("{forge_id}-daemon-token"));
        fs::write(&token_file, format!("{token}\n")).expect("token file");
        let forge = ForgeSpec::builder()
            .forge_id(forge_id.to_string())
            .kind(flotilla_resources::ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["localhost".to_string()]))
            .https_url(format!("{origin}/{forge_id}"))
            .git_ssh_host("localhost".to_string())
            .build();
        daemon.resource_backend().definitions::<Forge>(NAMESPACE).create(&empty_meta(forge_id), &forge).await.expect("Forge");
        declare_identity(&daemon, temp.path(), forge_id).await;
    }
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

// F1 (run 3): a forge mapped to a daemon identity that is unusable (here, its CredentialSpec is
// not declared) refuses its Git with the reason. It never goes on without the identity, as if the
// forge needed none. Another forge on the same host still signs in as its own identity.
#[tokio::test]
async fn an_unusable_daemon_identity_refuses_its_own_forges_git_with_the_reason() {
    let forge = lab_forge().await;
    let resolver = super::super::forge_default_branches(&forge.clones.state);
    let on = |forge_id: &str, repository: &str| flotilla_resources::ForgeIdentity {
        service_url: forge.service_url.replace("/lab", &format!("/{forge_id}")),
        repository: repository.into(),
    };
    forge.map_identities(&["lab", "lab2"]);
    forge.daemon.resource_backend().definitions::<CredentialSpec>(NAMESPACE).delete("lab-daemon").await.expect("withdraw lab's identity");

    let refused = resolver.default_branch(&on("lab", "acme/widgets")).await.expect_err("lab's Git is refused");
    assert!(refused.contains("forge lab: daemon Forgejo credential lab-daemon"), "{refused}");
    assert!(!refused.contains("could not read Username"), "lab's Git went on without its identity: {refused}");
    assert_eq!(
        resolver.default_branch(&on("lab2", "acme/gadgets")).await.expect("lab2 signs in as its own identity").as_deref(),
        Some("main")
    );
    forge.running.shutdown().await;
}

/// The daemon's forge default-branch lookups, counted by forge service URL as each finishes.
struct CountedLookups {
    inner: Arc<dyn flotilla_provisioning::reconcilers::ForgeDefaultBranchResolver>,
    finished: std::sync::Mutex<BTreeMap<String, usize>>,
}

impl CountedLookups {
    fn finished(&self, service_url: &str) -> usize {
        self.finished.lock().expect("lookups").get(service_url).copied().unwrap_or(0)
    }

    /// Waits for `service_url`'s lookups to reach `at_least`, then to stop for a while: its
    /// Repository's own status writes have settled.
    async fn settled(&self, service_url: &str, at_least: usize) -> usize {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut seen = self.finished(service_url);
        loop {
            assert!(tokio::time::Instant::now() < deadline, "the lookups on {service_url} never settled");
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = self.finished(service_url);
            if now >= at_least && now == seen {
                return now;
            }
            seen = now;
        }
    }
}

#[async_trait]
impl flotilla_provisioning::reconcilers::ForgeDefaultBranchResolver for CountedLookups {
    async fn default_branch(&self, forge: &flotilla_resources::ForgeIdentity) -> Result<Option<String>, String> {
        let answer = self.inner.default_branch(forge).await;
        *self.finished.lock().expect("lookups").entry(forge.service_url.clone()).or_default() += 1;
        answer
    }
}

// F1 (run 3): a fresh host's Repository is declared before the daemon's identity on its forge.
// Its lookup is refused until the identity is declared; the declaration wakes the Repository
// controller, and the default branch resolves with no restart. The Repository on `lab2` beside
// it is not woken.
#[tokio::test]
async fn declaring_the_daemon_identity_wakes_the_repository_on_its_forge() {
    use flotilla_provisioning::reconcilers::RepositoryReconciler;
    let forge = lab_forge().await;
    let backend = forge.daemon.resource_backend();
    backend.definitions::<CredentialSpec>(NAMESPACE).delete("lab-daemon").await.expect("withdraw lab's identity");

    let lookups =
        Arc::new(CountedLookups { inner: super::super::forge_default_branches(&forge.clones.state), finished: Default::default() });
    let discovered = forge.daemon.discovered_checkouts();
    let controller = flotilla_store::controller::ControllerLoop {
        primary: backend.clone().using::<Repository>(NAMESPACE).with_lifecycle_status_reads(forge.daemon.lifecycle_reads()),
        secondaries: RepositoryReconciler::secondary_watches(discovered.clone(), forge.daemon.decided_store().ok()),
        reconciler: RepositoryReconciler::new(discovered, forge.daemon.lifecycle_reads())
            .with_forge_default_branch_resolver(lookups.clone()),
        backend: backend.clone(),
    };
    let running = tokio::spawn(controller.run());

    let lab = backend.definitions::<Forge>(NAMESPACE).get("lab").await.expect("lab").spec;
    let lab2 = backend.definitions::<Forge>(NAMESPACE).get("lab2").await.expect("lab2").spec;
    let spec = RepositorySpec::remote(&forge.url).expect("remote").on_forge(&lab).expect("on lab");
    let name = spec.key().to_string();
    let gadgets_url = forge.url.replace("/lab/acme/widgets.git", "/lab2/acme/gadgets.git");
    let gadgets = RepositorySpec::remote(&gadgets_url).expect("remote").on_forge(&lab2).expect("on lab2");
    let lab_url = spec.forge().expect("lab forge").service_url.clone();
    let lab2_url = gadgets.forge().expect("lab2 forge").service_url.clone();
    assert_ne!(lab_url, lab2_url);
    let repositories = backend.clone().using::<Repository>(NAMESPACE);
    repositories.create(&empty_meta(&name), &spec).await.expect("repository");
    repositories.create(&empty_meta(&gadgets.key().to_string()), &gadgets).await.expect("lab2 repository");
    let default_branch =
        || async { repositories.get(&name).await.ok().and_then(|repository| repository.status).and_then(|status| status.default_branch) };

    // The lookup on lab is refused for want of the identity; lab2's has its identity.
    lookups.settled(&lab_url, 1).await;
    assert_eq!(default_branch().await, None);
    let lab2_before = lookups.settled(&lab2_url, 1).await;

    declare_identity(&forge.daemon, forge.temp.path(), "lab").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while default_branch().await.as_deref() != Some("main") {
        assert!(tokio::time::Instant::now() < deadline, "declaring the identity did not wake the Repository");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A wake of lab2's Repository would have looked it up again by now.
    assert_eq!(lookups.settled(&lab2_url, lab2_before).await, lab2_before, "declaring lab's identity woke lab2's Repository");
    running.abort();
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

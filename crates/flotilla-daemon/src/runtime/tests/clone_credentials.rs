// #4218: a cloud vessel clones before any crew session exists, so the clone
// takes its repository's grants through the session's delivery path.
use super::*;
use flotilla_orchestration_testkit::InProcessDiscoveryExt;
use flotilla_provisioning::reconcilers::PreparedCheckout;
use flotilla_resources::CloneCredentials;

const FORGE_URL: &str = "https://forge.test/acme/widgets";
const FORGE_TOKEN: &str = "forge-secret-material";

/// The vessel's process boundary. The forge refuses Git that has no sign-in, as
/// a private repository does. Git that has one is asked for `forge.test`'s
/// credential through the clone's own environment, then clones a local source.
struct ForgeRunner {
    source: String,
    signed_in: StdMutex<Vec<String>>,
}

impl ForgeRunner {
    fn refusal(args: &[&str]) -> Option<String> {
        args.contains(&FORGE_URL).then(|| "fatal: could not read Username for 'https://forge.test': terminal prompts disabled".to_string())
    }

    async fn clone_args<'a>(&'a self, cmd: &str, args: &[&'a str]) -> Vec<&'a str> {
        if cmd == "sh" && args.contains(&FORGE_URL) {
            let probe = [args[0], args[1], args[2], "credential", "fill"];
            let answer = ProcessCommandRunner
                .run_with_input("sh", &probe, Path::new("/"), &ChannelLabel::Default, b"protocol=https\nhost=forge.test\n\n")
                .await
                .expect("the clone's Git environment answers for its forge");
            self.signed_in.lock().expect("sign-ins").push(answer);
        }
        args.iter().map(|arg| if *arg == FORGE_URL { self.source.as_str() } else { *arg }).collect()
    }
}

#[async_trait]
impl CommandRunner for ForgeRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "git" {
            if let Some(refusal) = Self::refusal(args) {
                return Err(refusal);
            }
        }
        let args = self.clone_args(cmd, args).await;
        ProcessCommandRunner.run(cmd, &args, cwd, label).await
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        if cmd == "git" {
            if let Some(stderr) = Self::refusal(args) {
                return Ok(CommandOutput { stdout: String::new(), stderr, exit_code: Some(128) });
            }
        }
        let args = self.clone_args(cmd, args).await;
        ProcessCommandRunner.run_output(cmd, &args, cwd, label).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        ProcessCommandRunner.run_with_input(cmd, args, cwd, label, input).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
    async fn same_filesystem(&self, source: &Path, destination: &Path) -> Result<bool, String> {
        ProcessCommandRunner.same_filesystem(source, destination).await
    }
    async fn writable_scratch_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        ProcessCommandRunner.writable_scratch_base(preferred, fallback).await
    }
    async fn writable_config_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        ProcessCommandRunner.writable_config_base(preferred, fallback).await
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        ProcessCommandRunner.ensure_file(path, content).await
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        ProcessCommandRunner.write_file(path, content).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        ProcessCommandRunner.write_file_with_mode(path, content, mode).await
    }
}

struct CloudVessel {
    temp: TempDir,
    runner: Arc<ForgeRunner>,
    runtime: RoutingCheckoutRuntime,
}

async fn cloud_vessel() -> CloudVessel {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"clone-credentials-test\"\n").expect("daemon config");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("source");
    for args in
        [&["init", "-q", "-b", "main"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "seed"]]
    {
        assert!(ProcessCommand::new("git").args(args).current_dir(&source).status().expect("seed source").success());
    }
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    daemon
        .resource_backend()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("lab-forge"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GitHttpToken { host: "forge.test".to_string(), username: "crew".to_string() },
                source: CredentialSource::Env { name: "TEST_FORGE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("forge credential declaration");
    let runtime_dir = temp.path().join("runtime");
    fs::create_dir(&runtime_dir).expect("runtime directory");
    let credential_store = Arc::new(CredentialStore::new(
        daemon.resource_backend(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_FORGE_TOKEN", FORGE_TOKEN), ("XDG_RUNTIME_DIR", &runtime_dir.display().to_string())])),
        EnvironmentBag::new(),
        Arc::new(ProcessCommandRunner),
        temp.path().join("state"),
    ));
    let runner = Arc::new(ForgeRunner { source: source.display().to_string(), signed_in: StdMutex::new(Vec::new()) });
    daemon
        .register_direct_environment_for_test(
            EnvironmentId::new("cloud-vessel"),
            runner.clone(),
            EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
            None,
        )
        .expect("register vessel environment");
    let state = ControllerRuntimeState::new(daemon, config, passthrough_registry(), None, "local".into(), "local-env".into())
        .with_credential_store(credential_store);
    CloudVessel { temp, runner, runtime: RoutingCheckoutRuntime { state: Arc::new(state), change_requests: None } }
}

impl CloudVessel {
    async fn clone(&self, credentials: &CloneCredentials) -> Result<PreparedCheckout, String> {
        let target = self.temp.path().join("vessel/widgets");
        self.runtime
            .create_fresh_clone_in(
                "cloud-vessel",
                FORGE_URL,
                "feature/widgets",
                Some("main"),
                target.to_str().expect("target"),
                credentials,
            )
            .await
    }
}

/// The checkout carries the grants as the vessel reconciler writes them.
fn checkout_grants(refs: &[&str]) -> CloneCredentials {
    let widgets = RepositoryKey("widgets".to_string());
    let refs = refs.iter().map(|name| name.to_string()).collect::<BTreeSet<_>>();
    let scopes = refs.iter().map(|name| (name.clone(), BTreeSet::from([widgets.clone()]))).collect();
    let mut annotations = BTreeMap::new();
    CloneCredentials::for_repository(&widgets, &refs, &scopes, &BTreeMap::new()).annotate(&mut annotations);
    CloneCredentials::from_annotations(&annotations).expect("decode checkout grants")
}

#[tokio::test]
async fn a_cloud_clone_runs_git_with_its_repository_grant_delivered() {
    let vessel = cloud_vessel().await;

    vessel.clone(&checkout_grants(&["lab-forge"])).await.expect("the granted clone succeeds");

    let signed_in = vessel.runner.signed_in.lock().expect("sign-ins").clone();
    assert!(!signed_in.is_empty(), "the clone's Git ran with a delivered environment");
    assert!(signed_in.iter().all(|answer| answer.contains("username=crew") && answer.contains(&format!("password={FORGE_TOKEN}"))));
    assert!(vessel.temp.path().join("vessel/widgets/.git").is_dir());
    let left =
        walk_files(vessel.temp.path()).into_iter().filter(|path| path.extension().is_some_and(|ext| ext == "env")).collect::<Vec<_>>();
    assert!(left.is_empty(), "the clone's credential environment is removed after its Git: {left:?}");
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path))
        } else {
            files.push(path)
        }
    }
    files
}

#[tokio::test]
async fn a_cloud_clone_without_a_usable_grant_names_the_repository_forge_and_grant() {
    for (refs, reason) in [
        (&[][..], "requires a sign-in, and no credential grant covers this repository"),
        (&["absent-forge"][..], "delivering its granted credentials `absent-forge` for forge.test failed"),
    ] {
        let vessel = cloud_vessel().await;

        let error = vessel.clone(&checkout_grants(refs)).await.expect_err("the clone fails");

        assert!(error.starts_with(&format!("cannot clone {FORGE_URL}: ")), "{error}");
        assert!(error.contains(reason), "{error}");
    }
}

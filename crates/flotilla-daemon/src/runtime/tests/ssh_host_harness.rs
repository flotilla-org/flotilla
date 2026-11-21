//! #3655 slice 1: SSH host crews take the contained harness path. A crew on a
//! bare host launches with a flotilla-owned harness home under the host
//! runner's writable base, seeded with onboarding and workspace trust for its
//! checkout, and the host user's own harness configuration is never written.
//!
//! The SSH runner is the injected collaborator: `FakeHost` stands in for the
//! remote host's processes and filesystem behind `SshCommandRunner`, so every
//! file the launch writes on the host is observable.

use super::*;

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

use crate::testkits::engine::engine;
use crate::testkits::engine::request_session;
use flotilla_provider_runtime::host_environment::{HostEnvironment, HostSupply};
#[cfg(target_os = "linux")]
use flotilla_provider_runtime::ssh_runner::SshCommandRunner;

use super::super::account_access::reconcile_place_provisions;
use super::ssh_host_vessels::{VesselKey, VesselPaths};
#[cfg(target_os = "linux")]
use super::vessel_support::store_entry;
use super::vessel_support::{vessels_over, ArtifactArrival, RunnerVesselAgents};

const HOST_HOME: &str = "/home/crew";
/// What `SshCommandRunner::writable_config_base` resolves on a host whose login has
/// `XDG_RUNTIME_DIR`: a tmpfs a reboot clears.
const RUNTIME_BASE: &str = "/run/user/1000/flotilla";
/// The host user's durable state base, which harness homes live under (#4012).
const HOST_BASE: &str = "/home/crew/.local/state/flotilla";
const CHECKOUT: &str = "/home/crew/flotilla/repos/work";
const ENVIRONMENT: &str = "host-direct-b3";

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostEntry {
    File(String),
    Link(PathBuf),
}

/// The remote end of the SSH runner: an in-memory host filesystem plus the
/// handful of commands a crew launch runs there.
#[derive(Default)]
struct FakeHost {
    files: StdMutex<BTreeMap<PathBuf, HostEntry>>,
    writes: StdMutex<Vec<PathBuf>>,
    /// Paths whose protected writes the host refuses, as a full disk would.
    refused: StdMutex<BTreeSet<PathBuf>>,
}

impl FakeHost {
    fn with_files(files: &[(&str, &str)]) -> Self {
        let host = Self::default();
        host.files
            .lock()
            .expect("files")
            .extend(files.iter().map(|(path, content)| (PathBuf::from(path), HostEntry::File(content.to_string()))));
        host
    }

    fn entry(&self, path: impl AsRef<Path>) -> Option<HostEntry> {
        self.files.lock().expect("files").get(path.as_ref()).cloned()
    }

    fn json(&self, path: impl AsRef<Path>) -> serde_json::Value {
        match self.entry(&path) {
            Some(HostEntry::File(content)) => serde_json::from_str(&content).expect("JSON host file"),
            other => panic!("{} is {other:?}", path.as_ref().display()),
        }
    }

    fn put(&self, path: &Path, content: &str) {
        self.writes.lock().expect("writes").push(path.to_path_buf());
        self.files.lock().expect("files").insert(path.to_path_buf(), HostEntry::File(content.to_string()));
    }
}

fn ok() -> Result<String, String> {
    Ok(String::new())
}

#[async_trait]
impl CommandRunner for FakeHost {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        match (cmd, args) {
            ("rm", ["-f", "--", path]) => {
                self.files.lock().expect("files").remove(Path::new(path));
                ok()
            }
            ("mkdir" | "chmod" | "mv" | "rm", _) => ok(),
            ("sh", ["-c", _, "flotilla-requester-home", _]) => ok(),
            ("sh", ["-c", _, "flotilla-requester-remove", paths @ ..]) => {
                self.files.lock().expect("files").retain(|file, _| !paths.iter().any(|path| file.starts_with(path)));
                ok()
            }
            ("sh", ["-c", _, "flotilla-create-dir", _, _]) => ok(),
            ("sh", ["-c", _, "flotilla-stat", path]) => Ok(match self.entry(path) {
                Some(HostEntry::File(_)) => "file\n".into(),
                Some(HostEntry::Link(_)) => "symlink\n".into(),
                None if self.files.lock().expect("files").keys().any(|file| file.starts_with(path)) => "directory\n".into(),
                None => "none\n".into(),
            }),
            ("sh", ["-c", _, "flotilla-is-file", path]) => {
                Ok(if matches!(self.entry(path), Some(HostEntry::File(_))) { "file\n" } else { "none\n" }.into())
            }
            ("sh", ["-c", _, "flotilla-symlink", target, path]) => {
                self.files.lock().expect("files").insert(PathBuf::from(path), HostEntry::Link(PathBuf::from(target)));
                ok()
            }
            ("sh", ["-c", _, "flotilla-durable-base"]) => Ok(HOST_BASE.into()),
            ("cat", ["--", path]) => match self.entry(path) {
                Some(HostEntry::File(content)) => Ok(content),
                _ => Err(format!("cat: {path}: No such file")),
            },
            ("getconf", ["_NPROCESSORS_ONLN"]) => Ok("8\n".into()),
            // Executables and sockets lie outside the modelled filesystem: the
            // delivered CLI is present and the forwarded socket is listening.
            ("test", ["-x" | "-S", _]) => ok(),
            ("cat", [path]) => match self.entry(path) {
                Some(HostEntry::File(content)) => Ok(content),
                _ => Err(format!("cat: {path}: No such file")),
            },
            _ => Err(format!("unexpected host command {cmd} {args:?}")),
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        match (cmd, args) {
            ("sh", ["-c", "command -v claude"]) => {
                Ok(CommandOutput { stdout: "/tools/claude\n".into(), stderr: String::new(), exit_code: Some(0) })
            }
            ("pwd", ["-P"]) => Ok(CommandOutput { stdout: format!("{}\n", cwd.display()), stderr: String::new(), exit_code: Some(0) }),
            // The checkout is a plain directory in this model, as a
            // multi-repository workspace root is.
            ("git", _) => Ok(CommandOutput {
                stdout: String::new(),
                stderr: "fatal: not a git repository (or any of the parent directories): .git".into(),
                exit_code: Some(128),
            }),
            _ => Err(format!("unexpected host command {cmd} {args:?}")),
        }
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel, _input: &[u8]) -> Result<String, String> {
        panic!("credential preparation cannot send model turns: {cmd} {args:?}")
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        matches!(cmd, "claude" | "codex")
    }

    async fn writable_scratch_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from(RUNTIME_BASE))
    }

    async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from(RUNTIME_BASE))
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        match self.entry(path) {
            Some(HostEntry::File(existing)) => Ok(existing),
            Some(HostEntry::Link(target)) => Err(format!("ensure_file through link {}", target.display())),
            None => {
                self.put(path, content);
                Ok(content.to_string())
            }
        }
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.put(path, content);
        Ok(())
    }

    async fn write_file_with_mode(&self, path: &Path, content: &str, _mode: u32) -> Result<(), String> {
        if self.refused.lock().expect("refused").contains(path) {
            return Err(format!("write {}: No space left on device", path.display()));
        }
        self.put(path, content);
        Ok(())
    }
}

/// The host user's own harness configuration, which a crew must never write.
const HOST_HARNESS_FILES: &[(&str, &str)] = &[
    ("/home/crew/.claude.json", r#"{"theme":"host"}"#),
    ("/home/crew/.claude/settings.json", r#"{"host":true}"#),
    ("/home/crew/.codex/config.toml", "model = \"host\"\n"),
    ("/home/crew/.codex/auth.json", r#"{"tokens":"host-login"}"#),
];

struct SshHostLaunch {
    host: Arc<FakeHost>,
    /// The convoy's launch the crew serves; empty when the launch was refused.
    convoy: String,
    env: TerminalEnvVars,
    command: String,
    /// What `convoy explain` shows the access layer delivered.
    deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>,
    artifacts: Vec<(String, ArtifactArrival)>,
}

impl SshHostLaunch {
    fn value(&self, name: &str) -> Option<String> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }
}

/// Launch one crew of `adapter` on an SSH host whose `claude` and
/// `codex` have never been run, through the terminal runtime's real launch
/// path with the fake host behind the SSH runner.
async fn launch_on_ssh_host(adapter: &str) -> SshHostLaunch {
    try_launch_on_ssh_host(adapter, adapter == "claude-code").await.expect("launch SSH host crew")
}

/// As [`launch_on_ssh_host`], optionally without the Claude credential grant.
async fn try_launch_on_ssh_host(adapter: &str, grant_claude: bool) -> Result<SshHostLaunch, String> {
    // The forwarded daemon socket needs a short real directory (SUN_LEN).
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let launched = launch_ssh_host(ssh_host(&host, &runtime_dir), adapter, grant_claude).await;
    match launched {
        Ok(session) => Ok(SshHostLaunch {
            host,
            convoy: session.convoy,
            env: session.env_vars,
            command: session.command,
            deliveries: session.deliveries,
            artifacts: session.artifacts,
        }),
        Err(error) => {
            let refused = SshHostLaunch {
                host,
                convoy: String::new(),
                env: Vec::new(),
                command: String::new(),
                deliveries: Vec::new(),
                artifacts: Vec::new(),
            };
            assert_host_harness_untouched(&refused);
            Err(error)
        }
    }
}

/// `host` as an SSH host whose `claude` and `codex` have never been run, its crew
/// frozen with one skill.
fn ssh_host(host: &Arc<FakeHost>, runtime_dir: &flotilla_test_support::TestSocketDir) -> SshHost {
    // The SSH host's discovery bag is its SSH login environment: it
    // never names a flotilla environment.
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
        .with(EnvironmentAssertion::binary("codex", "/usr/local/bin/codex"));
    SshHost {
        runner: host.clone(),
        bag,
        host_environment: HostEnvironment::default(),
        checkout: CHECKOUT.to_string(),
        session_env: BTreeMap::new(),
        skills: true,
    }
}

/// The SSH host a launch runs against.
struct SshHost {
    /// The host's runner, as the SSH environment registers it.
    runner: Arc<dyn CommandRunner>,
    /// The host's discovery bag.
    bag: EnvironmentBag,
    host_environment: HostEnvironment,
    checkout: String,
    /// What the session spec declares, as credential delivery adds to it.
    session_env: BTreeMap<String, String>,
    /// Whether the crew freezes a selection of the local `research` skill.
    skills: bool,
}

/// What the terminal pool was asked to start.
#[derive(Debug)]
struct LaunchedSession {
    /// The convoy's launch the crew serves.
    convoy: String,
    env_vars: TerminalEnvVars,
    command: String,
    deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>,
    /// What reached the vessel agent's store.
    artifacts: Vec<(String, ArtifactArrival)>,
}

/// The terminal runtime, SSH host and crew session spec a launch runs with.
struct SshHostRuntime {
    _temp: TempDir,
    /// The owning daemon's home, holding the central Codex login only its refresher writes.
    daemon_home: PathBuf,
    daemon: Arc<InProcessDaemon>,
    /// The convoy's launch the crew serves.
    convoy: String,
    state: Arc<ControllerRuntimeState>,
    pool: Arc<FakeTerminalPool>,
    agents: Arc<RunnerVesselAgents>,
    vessel: VesselPaths,
    spec: flotilla_resources::TerminalSessionSpec,
    // Only the Linux-only acceptance tests use this.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    /// The host's discovery bag, which places each vessel's paths.
    bag: EnvironmentBag,
}

/// The runtime for one crew of `adapter` on `host`, with the Claude credential
/// declared but not yet bound.
async fn ssh_host_runtime(host: SshHost, adapter: &str) -> SshHostRuntime {
    let temp = TempDir::new().expect("tempdir");
    let daemon_home = temp.path().join("daemon-home");
    write_central_login(&daemon_home, "access-one");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).expect("config directory");
    fs::write(config_dir.join("daemon.toml"), "machine_id = \"ssh-host-harness-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_dir));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let local_host_id = daemon.local_host_id().expect("local host ID").to_string();
    let backend = daemon.resource_backend();
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "crew@example.com".to_string() },
                source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("Claude credential declaration");

    let SshHost { runner, bag, host_environment, checkout, session_env, skills } = host;
    let environment_id = EnvironmentId::new(ENVIRONMENT);
    daemon
        .register_direct_environment_for_test(
            environment_id.clone(),
            Arc::clone(&runner),
            bag.clone(),
            Some(flotilla_protocol::qualified_path::HostId::new("b3")),
        )
        .expect("register SSH host environment");
    let pool = Arc::new(FakeTerminalPool::new());
    let mut registry = ProviderRegistry::new();
    registry.agent_adapters = AgentAdapterRegistry::discover(&bag, Arc::clone(&runner));
    registry.terminal_pools.insert(
        "cleat",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "cleat"),
        Arc::clone(&pool) as Arc<dyn TerminalPool>,
    );
    let registry = Arc::new(registry);
    daemon.set_direct_environment_registry(&environment_id, Arc::clone(&registry)).expect("register SSH host providers");
    let profile = SshHostProfile {
        provisioning: LocalProvisioningProfile { host_direct_pool: "cleat".to_string(), ..manual_profile("b3", false) },
        environment_id: environment_id.clone(),
        destination: "crew@b3.example".to_string(),
        env_bag: bag.clone(),
        runner: Arc::clone(&runner),
        host_environment,
        accounts: flotilla_discovery::FactoryRegistry::default_all().account_variables(),
        single_tenant: false,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
        vessel_agent: Err("the test's vessel agents run on the host runner".into()),
    };
    register_ssh_host_resources(&backend, engine_environments(&daemon).reads.clone(), NAMESPACE, &local_host_id, &profile)
        .await
        .expect("register SSH host resources");
    let source = LocalSkillSource::new(temp.path(), "research");
    let skills = if skills { vec![source.selected.clone()] } else { Vec::new() };
    let (convoy, launch) = freeze_crew(&daemon, "demo", "coder", adapter, skills).await;
    let credential_store = Arc::new(CredentialStore::new(
        backend,
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        bag,
        Arc::clone(&runner),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agents = RunnerVesselAgents::new(Arc::clone(&runner), registry);
    let vessel = VesselPaths::new(&profile.env_bag, &VesselKey::new(&environment_id, &launch)).expect("vessel paths");
    let bag = profile.env_bag.clone();
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            local_host_id.clone(),
            format!("host-direct-{local_host_id}"),
        )
        .with_ssh_host(vec![profile])
        // #3792: the crew runs in its vessel's agent, which here runs its
        // commands through the host runner, as the real agent runs them there.
        .with_ssh_host_vessels(Some(vessels_over(agents.clone(), temp.path())))
        .with_environment_tools(EnvironmentToolProvisioner::new(Vec::new()))
        .with_credential_store(credential_store)
        .with_agent_material(Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
            ("HOME", daemon_home.display().to_string()),
            (FLOTILLA_SKILLS_DIR_ENV, source.supply.display().to_string()),
        ])))))
        .with_skill_trees(LocalSkillSource::trees(temp.path())),
    );
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: ENVIRONMENT.to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "code".to_string(), adapter: Some(adapter.to_string()), model: None },
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Implement the issue.".to_string(),
                copies: Vec::new(),
            },
            context: Box::new(flotilla_resources::TerminalCrewContext {
                namespace: NAMESPACE.to_string(),
                convoy: convoy.clone(),
                vessel_ref: launch.clone(),
            }),
        },
        cwd: checkout,
        env: session_env,
        pool: "cleat".to_string(),
    };
    SshHostRuntime { _temp: temp, daemon_home, daemon, convoy, state, pool, agents, vessel, spec, bag }
}

/// The owning daemon's central Codex login under `home`, as its refresher leaves it.
fn write_central_login(home: &Path, access_token: &str) {
    let path = flotilla_credentials::codex_central_auth_path(&TestEnvVars::new([("HOME", home.display().to_string())]));
    fs::create_dir_all(path.parent().expect("central login directory")).expect("central login directory");
    let login = serde_json::json!({"tokens": {"access_token": access_token, "refresh_token": "central-refresh"}});
    fs::write(&path, login.to_string()).expect("central login");
}

/// The crew copy a Codex crew holds at `home`: the owner's central login carrying
/// `access_token`, and never its refresh token.
fn assert_holds_central_copy(host: &FakeHost, home: &str, access_token: &str) {
    let copy = host.json(format!("{home}/auth.json"));
    assert_eq!(copy["tokens"]["access_token"], access_token, "{copy}");
    assert_eq!(copy["tokens"]["refresh_token"], "", "a crew copy never carries the central refresh token");
}

/// Launch one crew of `adapter` on `host` through the terminal runtime's real
/// launch path, with a fake terminal pool recording what it would start.
async fn launch_ssh_host(host: SshHost, adapter: &str, grant_claude: bool) -> Result<LaunchedSession, String> {
    let runtime = ssh_host_runtime(host, adapter).await;
    if grant_claude {
        bind_account(&runtime.daemon, &runtime.convoy, "coder", "claude-max", "crew@example.com").await;
    }
    runtime.launch().await
}

impl SshHostRuntime {
    async fn launch(&self) -> Result<LaunchedSession, String> {
        let SshHostRuntime { state, pool, agents, vessel, spec, .. } = self;
        let before = pool.ensured.lock().await.len();
        let launched = TerminalControllerRuntime { state: Arc::clone(state) }
            .ensure_session("terminal-demo-work-coder", &first_launch("terminal-demo-work-coder"), spec.into(), &[])
            .await;
        let ensured = pool.ensured.lock().await;
        if let Err(error) = launched {
            assert_eq!(ensured.len(), before, "a refused launch started a session");
            return Err(error);
        }
        let Some(launch) = ensured.last() else { panic!("expected a launch") };
        assert!(launch.declared, "an SSH host crew runs with its declared environment and no login shell");
        // The vessel was prepared before its crew started: its layer pushed, its
        // environment configured, and its daemon forward up at the socket the
        // session names.
        let prepared = agents.recorded(vessel);
        assert!(prepared.tool_pushes >= 1);
        let socket = launch.env_vars.iter().find(|(name, _)| name == "FLOTILLA_DAEMON_SOCKET").map(|(_, value)| PathBuf::from(value));
        assert_eq!(socket.as_ref(), Some(&vessel.daemon_socket()));
        assert_eq!(prepared.environment.get("FLOTILLA_DAEMON_SOCKET").map(PathBuf::from), socket);
        assert!(matches!(prepared.forwards.as_slice(), [(guest, _)] if *guest == vessel.daemon_socket()), "{:?}", prepared.forwards);
        Ok(LaunchedSession {
            convoy: self.convoy.clone(),
            env_vars: launch.env_vars.clone(),
            command: launch.command.clone(),
            deliveries: self.deliveries().await,
            artifacts: prepared.artifacts,
        })
    }

    /// What `convoy explain` shows the access layer delivered for the convoy.
    async fn deliveries(&self) -> Vec<flotilla_protocol::ExplainedAccessDelivery> {
        flotilla_projections::read_projections::access_delivery_evidence(
            &self.daemon.resource_backend(),
            self.daemon.lifecycle_reads().as_ref(),
            NAMESPACE,
            &self.convoy,
        )
        .await
        .expect("access deliveries")
    }
}

/// #3964, #3663: the access layer pushed the crew's frozen skills into the vessel agent's
/// cache and linked exactly the home its harness launches with to them, and recorded what
/// landed there. #3909: the vessel fetched the adapter's pinned harness by digest from its
/// upstream URL into that home, and the launch runs it.
fn assert_staged_into_launched_home(launch: &SshHostLaunch, variable: &str, adapter: &str) {
    let home = launch.value(variable).expect("launched home");
    let Some(HostEntry::Link(entry)) = launch.host.entry(format!("{home}/skills")) else { panic!("no skills link in the launched home") };
    let skills = ArtifactArrival::Pushed(vec![".flotilla-sources.json".into(), "research/SKILL.md".into()]);
    assert!(
        launch.artifacts.iter().any(|(sha256, arrival)| entry.ends_with(sha256) && *arrival == skills),
        "the skills link names the pushed tree: {entry:?} {:?}",
        launch.artifacts
    );
    let Some(HostEntry::File(record)) = launch.host.entry(format!("{home}/{}", flotilla_vessel_delivery::DELIVERY_RECORD)) else {
        panic!("no delivery record in the launched home")
    };
    assert!(record.lines().any(|line| line.starts_with("skills\t") && line.contains("\tagent-push\t")), "{record}");
    assert!(record.lines().any(|line| line.starts_with("home\t") && line.contains("\thost-path(rw, live)\t")), "{record}");
    // Contract 4: the access's status shows the same home and each need's mechanism.
    let [delivery] = launch.deliveries.as_slice() else { panic!("one access delivered: {:?}", launch.deliveries) };
    assert_eq!(delivery.home, home);
    assert_eq!(delivery.retry, None);
    let mechanisms = delivery.needs.iter().map(|need| (need.need.as_str(), need.mechanism.as_str())).collect::<BTreeMap<_, _>>();
    assert_eq!(mechanisms.get("home"), Some(&"host-path(rw, live)"));
    assert_eq!(mechanisms.get("skills"), Some(&"agent-push"));
    assert_eq!(mechanisms.get("harness"), Some(&"agent-fetch(digest)"));
    let pinned = pinned_harness_sha256(adapter);
    let fetched = launch.artifacts.iter().filter(|(_, arrival)| matches!(arrival, ArtifactArrival::Fetched(_))).collect::<Vec<_>>();
    assert!(
        matches!(fetched.as_slice(), [(sha256, ArtifactArrival::Fetched(urls))] if *sha256 == pinned && urls.len() == 1),
        "{fetched:?}"
    );
    let binary = flotilla_agent::adapter::harness_binary(adapter).expect("built-in harness");
    assert!(launch.command.contains(&format!("{home}/.flotilla/harness/bin/{binary} ")), "{}", launch.command);
}

/// No launch writes the host user's harness configuration; it reads as it did.
fn assert_host_harness_untouched(launch: &SshHostLaunch) {
    for (path, content) in HOST_HARNESS_FILES {
        assert_eq!(launch.host.entry(path), Some(HostEntry::File(content.to_string())), "{path} changed");
    }
    let writes = launch.host.writes.lock().expect("writes");
    let host_config = [Path::new("/home/crew/.claude"), Path::new("/home/crew/.claude.json"), Path::new("/home/crew/.codex")];
    for write in writes.iter() {
        assert!(!host_config.iter().any(|config| write.starts_with(config)), "launch wrote host harness config {}", write.display());
    }
}

/// Contract 3: the image's harness environment reaches the session.
fn assert_image_harness_environment(launch: &SshHostLaunch) {
    for (name, value) in
        [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_INSTALLATION_CHECKS", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("LANG", "en_US.UTF-8")]
    {
        assert_eq!(launch.value(name).as_deref(), Some(value), "{name}");
    }
}

// Acceptance (#3655, #3964, #4012): a Claude crew on a host whose `claude` has never run
// starts with `CLAUDE_CONFIG_DIR` at its own flotilla-owned home under the host's
// durable state base, not its `XDG_RUNTIME_DIR`, its frozen skills staged there, seeded with
// onboarding, workspace trust for the checkout, and attribution suppression, so no first-run
// dialog can appear. Its token lies on the runtime base, outside the home.
// The host user's `~/.claude.json` and `~/.claude` are never written.
#[tokio::test]
async fn ssh_host_claude_crew_starts_in_a_prepared_flotilla_home() {
    let launch = launch_on_ssh_host("claude-code").await;
    // #3929: the launched home is the recorded requester home, not a level below it.
    let home = requester_home_at(HOST_BASE, &launch.convoy, "claude");

    assert_eq!(launch.value("CLAUDE_CONFIG_DIR").as_deref(), Some(home.as_str()));
    assert_staged_into_launched_home(&launch, "CLAUDE_CONFIG_DIR", "claude-code");
    // #3907: the vessel agent wrote the bound token into the requester's material directory
    // on the runtime base; the session environment carries no token of its own.
    assert_eq!(launch.value("CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert_eq!(
        launch.host.entry(format!("{}/.flotilla-oauth-token", bound_material(&launch.convoy))),
        Some(HostEntry::File("oauth-secret-material".into()))
    );
    assert!(!launch
        .host
        .files
        .lock()
        .expect("files")
        .keys()
        .any(|path| path.starts_with(&home) && path.ends_with(".flotilla-oauth-token")));
    assert!(launch.command.contains(&claude_token_prefix(&bound_material(&launch.convoy))), "{}", launch.command);
    assert_eq!(launch.value("FLOTILLA_ENVIRONMENT_ID").as_deref(), Some(ENVIRONMENT));
    let state = launch.host.json(format!("{home}/.claude.json"));
    assert_eq!(state["hasCompletedOnboarding"], true);
    assert_eq!(state["projects"][CHECKOUT]["hasTrustDialogAccepted"], true, "trust for the checkout: {state}");
    let settings = launch.host.json(format!("{CHECKOUT}/{}", flotilla_agent::adapter::CLAUDE_MANAGED_SETTINGS_PATH));
    assert_eq!(settings["attribution"], json!({ "commit": "", "pr": "", "sessionUrl": false }));
    assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
    assert_image_harness_environment(&launch);
    assert_host_harness_untouched(&launch);
}

// Acceptance (#3655, #3964, #4037): a Codex crew on an SSH host starts with
// `CODEX_HOME` at its own flotilla-owned home under the host's durable state base, its
// frozen skills staged there, with workspace trust for the checkout in that home's own
// `config.toml`. The home holds a read-only copy of the owning daemon's central login; the
// host's own `~/.codex` login and config are never linked or written.
/// The runtime for a Claude crew on a fresh SSH host, bound and launched once.
async fn launched_claude_crew() -> (SshHostRuntime, Arc<FakeHost>, flotilla_test_support::TestSocketDir, LaunchedSession) {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "claude-code").await;
    bind_account(&runtime.daemon, &runtime.convoy, "coder", "claude-max", "crew@example.com").await;
    let launched = runtime.launch().await.expect("first launch");
    (runtime, host, runtime_dir, launched)
}

impl SshHostRuntime {
    // Only the Linux-only acceptance tests use this.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    /// Move the crew to its work's next launch, a new vessel of its convoy on the same host, as
    /// one replacing a retired vessel is: the same home, under another vessel agent.
    async fn move_to_vessel(&mut self) {
        let TerminalSessionSource::Agent { context, .. } = &mut self.spec.source else { panic!("an agent session") };
        let fixture = engine(&self.daemon);
        fixture.vessels.step(&context.vessel_ref, flotilla_resources::vessel_records::VesselStep::Released);
        let launch = fixture.vessels.request(&self.convoy, "work", self.daemon.local_host_id().expect("local host").as_str());
        self.vessel = VesselPaths::new(&self.bag, &VesselKey::new(&EnvironmentId::new(ENVIRONMENT), &launch)).expect("vessel paths");
        context.vessel_ref = launch;
    }
}

/// The launched home's harness and skills links each resolve into the entry its record
/// names, and each is a use the runtime's current vessel agent holds, so collecting the
/// store after any other vessel retires leaves them in place.
#[cfg(target_os = "linux")]
fn assert_linked_under_current_vessel(runtime: &SshHostRuntime, host: &FakeHost, launched: &LaunchedSession) {
    let home = launched.env_vars.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, home)| home.clone()).expect("home");
    let Some(HostEntry::File(record)) = host.entry(format!("{home}/{}", flotilla_vessel_delivery::DELIVERY_RECORD)) else {
        panic!("no delivery record in the launched home")
    };
    let store = runtime.agents.store();
    for (need, place) in [("harness", ".flotilla/harness"), ("skills", "skills")] {
        let line = record.lines().find(|line| line.starts_with(&format!("{need}\t"))).unwrap_or_else(|| panic!("{need}: {record}"));
        let [_, _, _, _, sha256, link] = line.split('\t').collect::<Vec<_>>()[..] else { panic!("{need} names no entry: {line}") };
        let path = Path::new(&home).join(place).join(link);
        let path = if link.is_empty() { Path::new(&home).join(place) } else { path };
        assert_eq!(store.holder(&path, sha256), Some(runtime.vessel.agent.as_path()), "{need} at {}: {store:?}", path.display());
        assert_eq!(host.entry(&path), Some(HostEntry::Link(store_entry(&runtime.vessel.store, sha256))), "{need}");
    }
    assert!(launched.command.contains(&format!("{home}/.flotilla/harness/bin/claude ")), "{}", launched.command);
}

// Acceptance (#4088): a crew home outlives its vessel. Once vessel A retires and the shared
// store is collected, the home's record still names the harness and skills, but the store
// no longer holds them. Relaunched on vessel B, both are delivered again by the mechanisms
// they first landed by, before the harness starts, and each link is a use B's agent holds.
// An unchanged relaunch on A sends nothing.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_home_relaunched_after_its_vessel_is_collected_receives_its_harness_and_skills_again() {
    let (mut runtime, host, _runtime_dir, first) = launched_claude_crew().await;
    let arrived = first.artifacts.clone();
    assert_eq!(arrived.len(), 2, "the skills pushed and the harness fetched: {arrived:?}");
    let kept = runtime.launch().await.expect("unchanged relaunch");
    assert_eq!(kept.artifacts, arrived, "nothing still in place is sent again");

    runtime.agents.collect(&runtime.vessel.agent.clone());
    assert!(runtime.agents.store().entries.is_empty(), "A held the only uses");
    runtime.move_to_vessel().await;
    let relaunched = runtime.launch().await.expect("relaunch on vessel B");

    assert_eq!(relaunched.artifacts, arrived, "B receives both again, the same way");
    assert_linked_under_current_vessel(&runtime, &host, &relaunched);
}

// Acceptance (#4088): an entry another vessel still holds outlives A's collection, so B
// receives no bytes for it, but B's agent places the home's links again and holds those
// uses. Once the other vessel retires too, its collection leaves the entries to B.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_entry_another_vessel_holds_is_linked_again_under_the_new_vessel() {
    let (mut runtime, host, _runtime_dir, first) = launched_claude_crew().await;
    let neighbour = runtime.vessel.agent.with_file_name("neighbour");
    {
        let mut store = runtime.agents.store.lock().expect("store");
        for (sha256, _) in &first.artifacts {
            store.uses.insert(Path::new("/home/crew/neighbour").join(sha256), (sha256.clone(), neighbour.clone()));
        }
    }

    runtime.agents.collect(&runtime.vessel.agent.clone());
    runtime.move_to_vessel().await;
    let relaunched = runtime.launch().await.expect("relaunch on vessel B");
    runtime.agents.collect(&neighbour);

    assert_eq!(relaunched.artifacts, [], "the store still held both: no bytes were sent");
    let held = first.artifacts.iter().map(|(sha256, _)| sha256.clone()).collect::<BTreeSet<_>>();
    assert_eq!(runtime.agents.store().entries, held, "B's uses keep both entries");
    assert_linked_under_current_vessel(&runtime, &host, &relaunched);
}

// Acceptance (#4088): a store cleared by hand under a durable home is noticed the same way:
// the relaunch delivers the harness and skills again, the same way, and links them.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_relaunch_after_the_store_is_cleared_by_hand_delivers_the_harness_and_skills_again() {
    let (runtime, host, _runtime_dir, first) = launched_claude_crew().await;
    runtime.agents.clear_store();
    let relaunched = runtime.launch().await.expect("relaunch after the store was cleared");
    assert_eq!(relaunched.artifacts[first.artifacts.len()..], first.artifacts[..], "both are delivered again, the same way");
    assert_linked_under_current_vessel(&runtime, &host, &relaunched);
}

// Acceptance (#4088 contract 3): a harness that cannot be delivered again once the store
// no longer holds it fails the launch with the need it failed at. No harness starts, and
// the host's own `claude` is never run in its place.
#[tokio::test]
async fn a_harness_that_cannot_be_delivered_again_fails_the_launch_by_name() {
    let (runtime, _host, _runtime_dir, _) = launched_claude_crew().await;
    runtime.agents.clear_store();
    runtime.agents.refuse_artifacts.store(true, std::sync::atomic::Ordering::SeqCst);

    let error = runtime.launch().await.expect_err("nothing to run");

    assert!(error.contains("deliver `harness`") && error.contains("unreachable"), "{error}");
    let deliveries = runtime.deliveries().await;
    let [delivery] = deliveries.as_slice() else { panic!("one access: {deliveries:?}") };
    assert_eq!(delivery.retry.as_ref().map(|retry| retry.need.as_str()), Some("harness"), "{delivery:?}");
}

#[tokio::test]
async fn ssh_host_codex_crew_starts_in_a_prepared_flotilla_home() {
    let launch = launch_on_ssh_host("codex").await;
    let home = requester_home_at(HOST_BASE, &launch.convoy, "codex");

    assert_eq!(launch.value("CODEX_HOME").as_deref(), Some(home.as_str()));
    assert_eq!(launch.value("FLOTILLA_ENVIRONMENT_ID").as_deref(), Some(ENVIRONMENT));
    let Some(HostEntry::File(config)) = launch.host.entry(format!("{home}/config.toml")) else { panic!("no flotilla Codex config") };
    assert!(config.contains(&format!("[projects.\"{CHECKOUT}\"]\ntrust_level = \"trusted\"")), "trust for the checkout: {config}");
    // #3823: contained crews own hooks only in invocation overrides. Check
    // every subscription at the daemon launch boundary, without a home copy.
    for event in ["session-start", "session-end", "user-prompt-submit", "pre-tool-use", "post-tool-use", "stop", "interrupt"] {
        let command = format!("flotilla hook codex {event}");
        assert!(!config.contains(&command), "home duplicates {command}: {config}");
        assert_eq!(launch.command.matches(&command).count(), 1, "invocation defines {command} once");
    }
    assert!(launch.command.contains("hooks.state="), "invocation carries hook trust records");
    assert_holds_central_copy(&launch.host, &home, "access-one");
    assert_staged_into_launched_home(&launch, "CODEX_HOME", "codex");
    assert_image_harness_environment(&launch);
    assert_host_harness_untouched(&launch);
}

// Review #3659, #3907 contract 8: an SSH host Claude crew has no ambient-login
// fallback. Without an account binding the launch is refused with the reason,
// and nothing starts or touches the host's configuration.
#[tokio::test(start_paused = true)]
async fn ssh_host_claude_crew_without_a_delivered_credential_is_refused_with_the_reason() {
    let Err(error) = try_launch_on_ssh_host("claude-code", false).await else {
        panic!("an SSH host Claude crew without a credential must not launch");
    };
    assert_eq!(error, "crew `coder` has no account binding for `claude:subscription`: its harness home is never left to an ambient login");
}

/// Stands in for `ssh` to an SSH host and its sshd: the remote command
/// string runs in `/bin/sh -c`, as sshd runs it for a user whose shell is
/// POSIX, in a clean login environment whose home is a temporary directory.
/// Only the Linux acceptance test below uses it.
#[cfg(target_os = "linux")]
struct LoopbackSshHost {
    home: PathBuf,
    runtime_dir: PathBuf,
}

#[cfg(target_os = "linux")]
impl LoopbackSshHost {
    fn login_environment(&self) -> Vec<String> {
        vec![
            format!("HOME={}", self.home.display()),
            "USER=crew".into(),
            "LOGNAME=crew".into(),
            "SHELL=/bin/sh".into(),
            // sshd's own default; the login profile is what would extend it.
            "PATH=/usr/local/bin:/usr/bin:/bin:/usr/games".into(),
            format!("XDG_RUNTIME_DIR={}", self.runtime_dir.display()),
        ]
    }

    /// `env -i <login environment> /bin/sh -c <remote command>`.
    fn sshd_args(&self, args: &[&str]) -> Vec<String> {
        // Skip SshCommandRunner's flags (`-o` takes a value) and the destination.
        let mut index = 0;
        while index < args.len() && args[index].starts_with('-') {
            index += if args[index] == "-o" { 2 } else { 1 };
        }
        let mut sshd = vec!["-i".to_string()];
        sshd.extend(self.login_environment());
        sshd.extend(["/bin/sh".to_string(), "-c".to_string(), args[index + 1..].join(" ")]);
        sshd
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl CommandRunner for LoopbackSshHost {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        assert_eq!(cmd, "ssh");
        let sshd = self.sshd_args(args);
        ProcessCommandRunner.run("env", &sshd.iter().map(String::as_str).collect::<Vec<_>>(), &self.home, label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "ssh");
        let sshd = self.sshd_args(args);
        ProcessCommandRunner.run_output("env", &sshd.iter().map(String::as_str).collect::<Vec<_>>(), &self.home, label).await
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], _cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        assert_eq!(cmd, "ssh");
        let sshd = self.sshd_args(args);
        ProcessCommandRunner.run_with_input("env", &sshd.iter().map(String::as_str).collect::<Vec<_>>(), &self.home, label, input).await
    }

    async fn run_from_file(&self, cmd: &str, args: &[&str], _cwd: &Path, source: &Path) -> Result<(), String> {
        assert_eq!(cmd, "ssh");
        let sshd = self.sshd_args(args);
        ProcessCommandRunner.run_from_file("env", &sshd.iter().map(String::as_str).collect::<Vec<_>>(), &self.home, source).await
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        unreachable!("the SSH runner probes through run_output")
    }
}

/// The amd64 digest the stdlib pins for `adapter`'s harness.
pub(super) fn pinned_harness_sha256(adapter: &str) -> String {
    let layers = flotilla_resources::stdlib::stdlib_image_layers().expect("stdlib layers");
    let layer = layers.iter().find(|layer| layer.provides.contains(&format!("harness:{adapter}"))).expect("harness layer");
    layer.pins.values().next().and_then(|pin| pin.artifact("amd64")).expect("amd64 artifact").sha256.clone()
}

#[cfg(target_os = "linux")]
fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().expect("parent")).expect("executable directory");
    fs::write(path, script).expect("executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable mode");
}

// Acceptance (#3722): an SSH host crew's harness sees flotilla's composed
// environment, never the host login profile's. The host's profile prepends
// `~/.local/bin`, which holds a plain `gh` and a stale `flotilla`; the host
// declares a `rust` supply. The launch runs through the real SSH runner, and
// cleat's `$SHELL -lc` is reproduced with the recorded command and
// environment, so the profile does run before the harness. The harness's
// PATH is the delivered CLI, the GitHub credential wrapper, the supplies and
// the system tail; `gh` and `flotilla` resolve to the wrapper and the CLI;
// the profile directory is absent; `RUSTUP_HOME` is set; and no runner
// command read the profile. The host has no harness installed: the one that
// runs is the pinned harness its access layer delivered (#3909). Mutation
// check: `sh -lc` in the SSH runner fails it.
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn ssh_host_harness_path_is_composed_and_never_the_login_profile() {
    // The home holds the vessel's cleat runtime, a socket directory (SUN_LEN).
    let remote = flotilla_test_support::TestSocketDir::new();
    let home = remote.path().join("home");
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let checkout = home.join("flotilla/repos/work");
    fs::create_dir_all(&checkout).expect("checkout");
    // The profile also records every read outside a crew session's own
    // shell, which only a runner command reading it can produce.
    fs::write(
        home.join(".profile"),
        "PATH=\"$HOME/.local/bin:$PATH\"; export PATH\n[ -n \"${FLOTILLA_CREW_ID-}\" ] || echo read >> \"$HOME/profile-read-by-runner\"\n",
    )
    .expect("login profile");
    write_executable(&home.join(".local/bin/gh"), "#!/bin/sh\necho host-gh\n");
    write_executable(&home.join(".local/bin/flotilla"), "#!/bin/sh\necho stale-flotilla\n");
    write_executable(&home.join(".cargo/bin/cargo"), "#!/bin/sh\necho cargo\n");
    // No harness is installed on the host (#3909): the pinned one is delivered.
    let host_environment = HostEnvironment::new(vec![HostSupply {
        name: "rust".into(),
        path: vec!["~/.cargo/bin".into()],
        env: vec![("CARGO_HOME".into(), "~/.cargo".into()), ("RUSTUP_HOME".into(), "~/.rustup".into())],
    }])
    .expect("declared supplies");
    let runner: Arc<dyn CommandRunner> = Arc::new(
        SshCommandRunner::new(
            "crew@b3.example",
            false,
            Arc::new(LoopbackSshHost { home: home.clone(), runtime_dir: runtime_dir.path().to_path_buf() }),
        )
        .with_environment(host_environment.clone()),
    );

    // Discovery reads the host's environment through the runner, as
    // registration does.
    let discovered = runner.run("env", &[], Path::new("/"), &ChannelLabel::Default).await.expect("remote env");
    let mut bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
    for (name, value) in discovered.lines().filter_map(|line| line.split_once('=')) {
        if matches!(name, "HOME" | "PATH" | "USER" | "XDG_RUNTIME_DIR") {
            bag = bag.with(EnvironmentAssertion::env_var(name, value));
        }
    }
    // The GitHub App delivery puts its `gh` wrapper's directory ahead of the
    // host runner's PATH (`credential.rs`); minting its token is not this
    // scenario's concern.
    let wrapper_dir = runtime_dir.path().join("flotilla/credentials/github-crew-pr");
    write_executable(&wrapper_dir.join("gh"), "#!/bin/sh\necho flotilla-gh-wrapper\n");
    let runner_path =
        runner.run("sh", &["-c", "printf '%s' \"$PATH\""], Path::new("/"), &ChannelLabel::Default).await.expect("runner PATH");
    let session_env = BTreeMap::from([("PATH".to_string(), format!("{}:{runner_path}", wrapper_dir.display()))]);

    // A real shell stands in for the host here, so the crew selects no skills to fetch.
    let runtime = ssh_host_runtime(
        SshHost { runner, bag, host_environment, checkout: checkout.display().to_string(), session_env, skills: false },
        "claude-code",
    )
    .await;
    // The vessel's generation layer holds the delivered CLI, in the launch the runtime requested.
    let vessel = &runtime.vessel;
    let cli = vessel.cli();
    let cli_dir = cli.parent().expect("CLI directory").to_path_buf();
    write_executable(&cli, "#!/bin/sh\necho generation-cli\n");
    // The pinned harness's verified bytes in the vessel agent's store: it reports what it sees.
    write_executable(
        &store_entry(&vessel.store, &pinned_harness_sha256("claude-code")),
        "#!/bin/sh\n{ printf '%s\\n' \"$PATH\"; command -v gh; command -v flotilla; printf '%s\\n' \"${RUSTUP_HOME-unset}\"; } > \"$HOME/harness-report\"\n",
    );

    bind_account(&runtime.daemon, &runtime.convoy, "coder", "claude-max", "crew@example.com").await;
    let session = runtime.launch().await.expect("launch SSH host crew");

    // #4173: cleat runs a declared session's script in `sh -c` with exactly the
    // launch's environment over the pool's host baseline (the login basics);
    // no login shell reads the profile.
    let mut cleat = vec!["-i".to_string(), format!("HOME={}", home.display()), "USER=crew".into(), "SHELL=/bin/sh".into()];
    cleat.push(format!("FLOTILLA_PRELUDE_DIR={}", remote.path().join("no-preludes").display()));
    cleat.extend(session.env_vars.iter().map(|(name, value)| format!("{name}={value}")));
    cleat.extend(["/bin/sh".to_string(), "-c".to_string(), session.command.clone()]);
    let status = std::process::Command::new("env").args(&cleat).current_dir(&checkout).status().expect("run the session command");
    assert!(status.success(), "session command failed: {status}");

    let report = fs::read_to_string(home.join("harness-report")).expect("harness report");
    let lines = report.lines().collect::<Vec<_>>();
    let home_text = home.display().to_string();
    assert!(cli_dir.starts_with(home.join(".local/state/flotilla/vessels")), "{}", cli_dir.display());
    let expected_path = [
        cli_dir.display().to_string(),
        wrapper_dir.display().to_string(),
        format!("{home_text}/.cargo/bin"),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
    ]
    .join(":");
    assert_eq!(lines[0], expected_path, "the harness PATH is in the contract order");
    assert!(!lines[0].contains(".local/bin"), "the profile directory is absent: {}", lines[0]);
    assert_eq!(lines[1], wrapper_dir.join("gh").display().to_string(), "gh is flotilla's wrapper");
    assert_eq!(lines[2], cli_dir.join("flotilla").display().to_string(), "flotilla is the delivered CLI");
    assert_eq!(lines[3], format!("{home_text}/.rustup"));
    assert!(!home.join("profile-read-by-runner").exists(), "a runner command read the login profile");
}

// Acceptance (#3722): a supply variable that flotilla also sets for the
// session is a configuration error for that host, and nothing starts.
#[tokio::test(start_paused = true)]
async fn a_supply_overriding_a_flotilla_variable_is_a_host_configuration_error() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"));
    // The harness environment flotilla gives every SSH host session sets LANG.
    let host_environment =
        HostEnvironment::new(vec![HostSupply { name: "locale".into(), path: Vec::new(), env: vec![("LANG".into(), "C.UTF-8".into())] }])
            .expect("declared supply");
    let Err(error) = launch_ssh_host(
        SshHost { runner: host.clone(), bag, host_environment, checkout: CHECKOUT.to_string(), session_env: BTreeMap::new(), skills: true },
        "claude-code",
        true,
    )
    .await
    else {
        panic!("a colliding supply must not launch");
    };
    assert_eq!(error, "host configuration error for host-direct-b3: supply `locale` sets LANG, which flotilla sets");
}

// Supplies describe installations only: one that sets a variable a
// registered family keeps its account state in is a host configuration
// error, so the host's ambient Claude home can never reach a crew through it.
#[tokio::test(start_paused = true)]
async fn a_supply_setting_a_family_account_variable_is_a_host_configuration_error() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"));
    let host_environment = HostEnvironment::new(vec![HostSupply {
        name: "claude".into(),
        path: vec!["~/.local/opt/claude/bin".into()],
        env: vec![("CLAUDE_CONFIG_DIR".into(), "~/.claude".into())],
    }])
    .expect("well-formed supply");
    let Err(error) = launch_ssh_host(
        SshHost { runner: host.clone(), bag, host_environment, checkout: CHECKOUT.to_string(), session_env: BTreeMap::new(), skills: true },
        "claude-code",
        true,
    )
    .await
    else {
        panic!("a supply carrying account state must not launch");
    };
    assert_eq!(
        error,
        "host configuration error for host-direct-b3: supply `claude` sets CLAUDE_CONFIG_DIR, which carries `claude-code` account state"
    );
}

/// A Claude crew on an SSH host bound to `claude-box-a`, whose token is the host-local
/// file `token`: launched once, with its TerminalSession recorded.
async fn bound_ssh_host_crew(token: &Path) -> (SshHostRuntime, Arc<FakeHost>, flotilla_test_support::TestSocketDir) {
    bound_ssh_host_crew_with(token, None).await
}

/// As [`bound_ssh_host_crew`], with the binding replicated from another home whose clock runs
/// `skew` ahead of this host's.
async fn bound_ssh_host_crew_with(
    token: &Path,
    skew: Option<(&str, chrono::Duration)>,
) -> (SshHostRuntime, Arc<FakeHost>, flotilla_test_support::TestSocketDir) {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"));
    let runtime = ssh_host_runtime(
        SshHost {
            runner: host.clone(),
            bag,
            host_environment: HostEnvironment::default(),
            checkout: CHECKOUT.to_string(),
            session_env: BTreeMap::new(),
            skills: true,
        },
        "claude-code",
    )
    .await;
    let backend = runtime.daemon.resource_backend();
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("claude-box-a"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "box-a@example.com".to_string() },
                source: CredentialSource::File { path: token.display().to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("box account declaration");
    match skew {
        None => bind_account(&runtime.daemon, &runtime.convoy, "coder", "claude-box-a", "box-a@example.com").await,
        Some((home, ahead)) => bind_account_from_home(&runtime.daemon, home, &runtime.convoy, "coder", "claude-box-a", ahead).await,
    }
    let launch = request_session(&runtime.daemon, "terminal-demo-work-coder", &runtime.spec);
    runtime.launch().await.expect("bound launch");
    let session_id = runtime.pool.ensured.lock().await.last().expect("launched").session_name.clone();
    engine(&runtime.daemon).terminals.ready(&launch, &session_id, &Default::default());
    (runtime, host, runtime_dir)
}

/// Where the bound token lies: the requester's material directory on the runtime base.
/// Where the bound Claude token lies for convoy launch `convoy`: its requester material on the
/// runtime base.
fn bound_material(convoy: &str) -> String {
    flotilla_resources::account_access::requester_material(Path::new(RUNTIME_BASE), convoy, "coder", "claude").display().to_string()
}

/// Convoy launch `convoy`'s coder's requester home for `harness` under `base`.
fn requester_home_at(base: &str, convoy: &str, harness: &str) -> String {
    flotilla_resources::account_access::requester_home(Path::new(base), convoy, "coder", harness).display().to_string()
}

/// What the AccessDelivery controller does for delivery `name` held as `delivery` when its place
/// or its convoy changes: judges it against them and, once one has ended, releases it, which
/// removes it.
async fn controller_pass(
    state: &Arc<ControllerRuntimeState>,
    name: &str,
    delivery: &flotilla_resources::AccessDeliverySpec,
) -> Result<(), String> {
    use flotilla_provisioning::engine::{
        controller::{Actuator, ControlledRecords},
        Step,
    };
    let actuator = super::super::access_layer::status::AccessDeliveryActuator::new(Arc::clone(state));
    let service = state.daemon.decided_store();
    let holding = service.with_store(|store| actuator.holder(&mut store.read()?, name, delivery)).map_err(|error| error.to_string())?;
    if !holding.ended {
        return Ok(());
    }
    actuator.release(name, delivery, None).await.map_err(|failure| failure.cause.detail)?;
    let now = flotilla_schema::Timestamp::from_datetime(&state.daemon.clock().now());
    state.access_deliveries().controlled().step(name, &Step::Released, now).map_err(|error| error.to_string())
}

/// Ends convoy launch `convoy` at `daemon`'s home, as an operator's abandon ends it.
async fn end_convoy(daemon: &InProcessDaemon, convoy: &str) {
    let view = daemon.resource_backend().using::<Convoy>(NAMESPACE).get(convoy).await.expect("convoy");
    let engine = daemon.convoy_engine_path();
    engine.abandon(&view, "operator", "ended").await.expect("convoy ended");
}

/// The requester's current binding, as the account port reads it.
async fn binding(daemon: &InProcessDaemon, convoy: &str) -> flotilla_resources::account_access::BindingRecord {
    let access = flotilla_resources::account_access::AccessName(flotilla_agent::account_access::CLAUDE_SUBSCRIPTION.to_string());
    daemon.account_records().requester_binding(NAMESPACE, convoy, "coder", &access).await.expect("read binding").expect("binding")
}

/// This place's provision record of the requester's binding.
async fn provision_record(daemon: &InProcessDaemon) -> Option<flotilla_resources::account_access::ProvisionRecord> {
    let provisions = daemon.account_records().local_provisions(NAMESPACE).await.expect("provisions");
    provisions.into_iter().find(|record| record.spec.place == ENVIRONMENT)
}

async fn provision(daemon: &InProcessDaemon) -> Option<flotilla_resources::AccountProvisionSpec> {
    provision_record(daemon).await.map(|record| record.spec)
}

/// Reopens the requester's binding a second later, as an explicit rebind does.
async fn rebind(daemon: &InProcessDaemon, convoy: &str) {
    let current = binding(daemon, convoy).await;
    let now = flotilla_schema::Timestamp::from_datetime(&(flotilla_resources::clock::now() + chrono::Duration::seconds(1)));
    let reopened = super::launch_support::pending_binding(convoy, "coder", "claude-box-a", "box-a@example.com", now);
    assert_eq!(reopened.spec.requester, current.spec.requester);
    daemon.account_records().rebind(NAMESPACE, reopened, None, now).await.expect("rebind");
}

// Acceptance (#3907 sticky): a refresh is a new material generation of the same managed
// credential. The place pass pushes it into the requester's home (update mode push) and
// records the new generation; the binding does not move.
#[tokio::test]
async fn a_refreshed_token_is_pushed_into_the_bound_home_without_moving_the_binding() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "generation-one\n").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let bound = binding(&runtime.daemon, &runtime.convoy).await.spec;
    let delivered = provision(&runtime.daemon).await.expect("provision recorded at launch");
    assert_eq!((delivered.place.as_str(), delivered.environment_kind.as_str()), (ENVIRONMENT, "ssh-host"));
    assert_eq!(delivered.home, bound_material(&runtime.convoy), "the provision records where the copy lies");
    assert_eq!(
        host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))),
        Some(HostEntry::File("generation-one".into()))
    );

    fs::write(&token, "generation-two\n").expect("refreshed token");
    reconcile_place_provisions(&runtime.state, NAMESPACE).await.expect("place pass");

    assert_eq!(
        host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))),
        Some(HostEntry::File("generation-two".into()))
    );
    let pushed = provision(&runtime.daemon).await.expect("provision kept");
    let generation =
        |spec: &flotilla_resources::AccountProvisionSpec| spec.delivered.as_ref().map(|delivered| delivered.generation.clone());
    assert_ne!(generation(&pushed), generation(&delivered));
    assert_eq!(generation(&pushed), Some(flotilla_resources::account_access::material_generation("generation-two")));
    assert_eq!(binding(&runtime.daemon, &runtime.convoy).await.spec, bound);
}

// Acceptance (#3907 grant withdrawal, at the place): once the binding is revoked, the place
// pass deletes the delivered copy, stops the harness session and removes the record; a
// relaunch is refused with the binding's reason.
#[tokio::test]
async fn a_revoked_binding_stops_the_session_and_deletes_its_copy() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let bound = binding(&runtime.daemon, &runtime.convoy).await;
    let now = flotilla_schema::Timestamp::from_datetime(&flotilla_resources::clock::now());
    let revoked = flotilla_resources::account_access::BindingState::Revoked {
        reason: "binding revoked: no authorizing grant".into(),
        delivered_until: Some(now),
    };
    runtime.daemon.account_records().end(NAMESPACE, &bound, revoked, now).await.expect("revoke");

    reconcile_place_provisions(&runtime.state, NAMESPACE).await.expect("place pass");

    assert_eq!(host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))), None, "the delivered copy is deleted");
    assert!(provision(&runtime.daemon).await.is_none(), "the provision record is removed");
    assert_eq!(*runtime.pool.killed.lock().await, ["terminal-demo-work-coder"], "the harness session is stopped");
    let refused = runtime.launch().await.expect_err("a revoked requester waits");
    assert_eq!(refused, "crew `coder` waits: binding revoked: no authorizing grant; `flotilla convoy rebind` opens a new binding");
}

// Acceptance (#3907 rebind, at the place): a binding reopened after the delivery (a rebind)
// has its old copy deleted and its session stopped, so the requester starts afresh.
#[tokio::test]
async fn a_rebound_requester_loses_its_old_copy_and_session() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    rebind(&runtime.daemon, &runtime.convoy).await;

    reconcile_place_provisions(&runtime.state, NAMESPACE).await.expect("place pass");

    assert_eq!(host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))), None);
    // A rebind closes the old binding and opens a new one under its own name, so the old
    // binding's provision goes with its copy; a relaunch records the new binding's afresh.
    assert_eq!(provision(&runtime.daemon).await, None, "the closed binding's provision is removed");
    assert_eq!(*runtime.pool.killed.lock().await, ["terminal-demo-work-coder"], "the old session is stopped");
}

// Acceptance (#3907 sticky): a harness restart re-reads the same binding and redelivers the
// current generation; a record left at a replaced vessel's place, whose environment is gone,
// is swept without touching the binding.
#[tokio::test]
async fn a_restart_keeps_the_account_and_a_gone_place_is_swept() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, _host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let mut old_place = provision(&runtime.daemon).await.expect("provision");
    old_place.place = "replaced-vessel-env".into();
    let accounts = runtime.daemon.account_records();
    let old_name = format!("{}.replaced-vessel-env", old_place.binding.name);
    accounts.record_provision(NAMESPACE, &old_name, &runtime.convoy, &old_place, false).await.expect("old place record");

    let restarted = runtime.launch().await.expect("restart");
    let token_file = restarted.env_vars.iter().find(|(name, _)| name == "FLOTILLA_CLAUDE_OAUTH_TOKEN_FILE").map(|(_, value)| value.clone());
    assert_eq!(token_file.as_deref(), Some(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy)).as_str()));
    reconcile_place_provisions(&runtime.state, NAMESPACE).await.expect("place pass");

    let provisions = accounts.local_provisions(NAMESPACE).await.expect("provisions");
    assert_eq!(provisions.iter().map(|record| record.spec.place.as_str()).collect::<Vec<_>>(), [ENVIRONMENT]);
    let binding = binding(&runtime.daemon, &runtime.convoy).await.spec;
    assert_eq!(binding.credential.0, "claude-box-a");
}

// #4012: a provision recorded before the token moved out of the home names the old home.
// The relaunch deletes the copy there and records the material directory, so no copy is
// left that no record names.
#[tokio::test]
async fn a_relaunch_moves_a_copy_recorded_in_the_old_home() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let recorded = provision_record(&runtime.daemon).await.expect("provision");
    let old_home = requester_home_at(RUNTIME_BASE, &runtime.convoy, "claude");
    host.put(&Path::new(&old_home).join(".flotilla-oauth-token"), "box-a-token");
    let old = flotilla_resources::AccountProvisionSpec { home: old_home.clone(), ..recorded.spec.clone() };
    let written = runtime.daemon.account_records().update_provision(NAMESPACE, &recorded, &old).await.expect("pre-#4012 record");
    assert_eq!(written, flotilla_resources::account_access::ProvisionWrite::Written);

    runtime.launch().await.expect("relaunch");

    assert_eq!(host.entry(format!("{old_home}/.flotilla-oauth-token")), None, "the old copy is deleted");
    assert_eq!(
        host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))),
        Some(HostEntry::File("box-a-token".into()))
    );
    assert_eq!(provision(&runtime.daemon).await.expect("provision").home, bound_material(&runtime.convoy));
}

// Review #3910: a binding homed on another daemon whose clock runs ahead of this place is still
// delivered from its replica, and the place pass never mistakes the fresh copy for one made
// before the binding opened.
#[tokio::test]
async fn a_replica_binding_from_a_clock_ahead_keeps_its_fresh_copy() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew_with(&token, Some(("udder-root", chrono::Duration::hours(1)))).await;
    let delivered = provision(&runtime.daemon).await.expect("provision recorded at launch");
    assert_eq!(delivered.binding.home, "udder-root", "the provision names the binding's home");

    reconcile_place_provisions(&runtime.state, NAMESPACE).await.expect("place pass");

    assert_eq!(
        host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))),
        Some(HostEntry::File("box-a-token".into()))
    );
    assert_eq!(provision(&runtime.daemon).await, Some(delivered));
    assert!(runtime.pool.killed.lock().await.is_empty(), "the session keeps running");
}

// Review #3933: a withdrawal waits for a delivery of the same binding at the place to finish,
// so it can never remove a copy a launch is still delivering.
#[tokio::test]
async fn a_withdrawal_waits_for_a_delivery_in_progress() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let bound = binding(&runtime.daemon, &runtime.convoy).await;
    rebind(&runtime.daemon, &runtime.convoy).await;

    let lock = runtime.state.account_delivery_lock(&bound.name);
    let delivering = lock.lock().await;
    let state = Arc::clone(&runtime.state);
    let withdrawal = tokio::spawn(async move { reconcile_place_provisions(&state, NAMESPACE).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!withdrawal.is_finished(), "the withdrawal waits for the delivery");
    assert_eq!(
        host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))),
        Some(HostEntry::File("box-a-token".into()))
    );

    drop(delivering);
    withdrawal.await.expect("join").expect("place pass");
    assert_eq!(host.entry(format!("{}/.flotilla-oauth-token", bound_material(&runtime.convoy))), None);
}

// #3964 contract 4: a failed delivery records, for its access, the need it failed at and
// when the next attempt is due. A relaunch before then is refused without another attempt;
// once due, the next delivery lands and clears the retry.
#[tokio::test]
async fn failed_delivery_waits_for_its_retry_and_is_cleared_once_delivered() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "claude-code").await;
    runtime.launch().await.expect_err("no account is bound");
    let retry = |deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>| {
        let [delivery] = deliveries.as_slice() else { panic!("one access: {deliveries:?}") };
        delivery.retry.clone()
    };
    let failed = retry(runtime.deliveries().await).expect("the failure is recorded");
    assert_eq!((failed.need.as_str(), failed.attempts), ("account", 1));

    bind_account(&runtime.daemon, &runtime.convoy, "coder", "claude-max", "crew@example.com").await;
    runtime.launch().await.expect_err("the retry is not yet due");
    assert_eq!(retry(runtime.deliveries().await), Some(failed), "no attempt before the retry is due");

    // Time passes: the recorded retry falls due.
    let records = runtime.daemon.resource_backend().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    let mut due = record.spec.clone();
    due.retry.as_mut().expect("retry").next_at = flotilla_schema::Timestamp::from_datetime(&chrono::DateTime::UNIX_EPOCH);
    let meta = InputMeta::builder().name(record.metadata.name.clone()).labels(record.metadata.labels.clone()).build();
    records.update(&meta, &record.metadata.resource_version, &due).await.expect("retry due");
    runtime.launch().await.expect("delivered once due");
    assert_eq!(retry(runtime.deliveries().await), None);
}

// #3663: a delivery refused on positive evidence is recorded as permanent and never
// retried. Here the current supply no longer declares the crew's frozen source, so the
// skills need is refused; a later launch is refused without another attempt.
#[tokio::test(start_paused = true)]
async fn a_permanent_refusal_is_recorded_and_never_retried() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    let supply = runtime._temp.path().join("generation/skills/.flotilla-sources.json");
    let other =
        serde_json::json!({"schema_version": 5, "sources": [{"name": "other", "repository": "/elsewhere", "revision": "3".repeat(40)}]});
    fs::write(&supply, other.to_string()).expect("the supply moves on");
    runtime.launch().await.expect_err("the frozen source is no longer authorized");
    let failed = |deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>| {
        let [delivery] = deliveries.as_slice() else { panic!("one access: {deliveries:?}") };
        delivery.retry.clone().expect("the failure is recorded")
    };
    let refused = failed(runtime.deliveries().await);
    assert_eq!((refused.need.as_str(), refused.attempts), ("skills", 1));
    assert!(refused.error.starts_with("permanent") && refused.error.contains("not declared"), "{}", refused.error);
    assert_eq!(refused.next_at, "", "no retry is ever due");

    let error = runtime.launch().await.expect_err("still refused");
    assert!(error.contains("permanent"), "{error}");
    assert_eq!(failed(runtime.deliveries().await), refused, "no further attempt");
}

// #3964: an access's status lasts as long as its place and its convoy. While both
// remain it is kept; once the convoy is gone, the access has ended and the record goes.
#[tokio::test]
async fn access_status_goes_with_its_convoy() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    runtime.launch().await.expect("delivered");
    let backend = runtime.daemon.resource_backend();
    let records = backend.clone().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retire");
    assert_eq!(records.list().await.expect("records").items.len(), 1, "kept while its place and convoy remain");
    end_convoy(&runtime.daemon, &runtime.convoy).await;
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retire");
    assert!(records.list().await.expect("records").items.is_empty(), "removed with its convoy");
}

// #4089: retiring a convoy's access on an SSH host removes the requester's home and material
// from the host, and again is a no-op.
#[tokio::test]
async fn retiring_a_convoy_removes_its_requester_state_from_the_ssh_host() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let requester_files = |host: &FakeHost| {
        let files = host.files.lock().expect("files");
        files.keys().filter(|path| path.components().any(|part| part.as_os_str() == "requesters")).cloned().collect::<Vec<_>>()
    };
    let before = requester_files(&host);
    assert!(before.iter().any(|path| path.starts_with(bound_material(&runtime.convoy))), "the material landed: {before:?}");
    let backend = runtime.daemon.resource_backend();
    let records = backend.clone().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retire");
    assert_eq!(requester_files(&host), before, "kept while its convoy remains");
    end_convoy(&runtime.daemon, &runtime.convoy).await;
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retire");
    assert_eq!(requester_files(&host), Vec::<PathBuf>::new(), "removed with its convoy");
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retiring again is a no-op");
}

// #3108: not finding an access's convoy or place is no record that it ended. A delivery whose
// convoy or place cannot be found is held: its record and the requester state on the SSH host
// stay. Its launch's recorded end still retires it.
#[tokio::test]
async fn an_unresolvable_convoy_or_place_holds_the_access_and_its_requester_state() {
    let tokens = TempDir::new().expect("token dir");
    let token = tokens.path().join("box-a");
    fs::write(&token, "box-a-token").expect("token");
    let (runtime, host, _runtime_dir) = bound_ssh_host_crew(&token).await;
    let requester_files = |host: &FakeHost| {
        let files = host.files.lock().expect("files");
        files.keys().filter(|path| path.components().any(|part| part.as_os_str() == "requesters")).cloned().collect::<Vec<_>>()
    };
    let before = requester_files(&host);
    assert!(before.iter().any(|path| path.starts_with(bound_material(&runtime.convoy))), "the material landed: {before:?}");
    let backend = runtime.daemon.resource_backend();
    let records = backend.clone().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    let mut convoyless = record.spec.clone();
    convoyless.access = flotilla_resources::access_delivery::with_convoy(&convoyless.access, "not-imported").expect("an access key");
    controller_pass(&runtime.state, &record.metadata.name, &convoyless).await.expect("held");
    let mut placeless = record.spec.clone();
    placeless.place = "env-not-here".to_string();
    controller_pass(&runtime.state, &record.metadata.name, &placeless).await.expect("held");
    assert_eq!(records.list().await.expect("records").items.len(), 1, "held while its convoy or place cannot be found");
    assert_eq!(requester_files(&host), before, "the requester state stays");
    end_convoy(&runtime.daemon, &runtime.convoy).await;
    controller_pass(&runtime.state, &record.metadata.name, &record.spec).await.expect("retire");
    assert!(records.list().await.expect("records").items.is_empty(), "removed once its launch ended");
    assert_eq!(requester_files(&host), Vec::<PathBuf>::new(), "its requester state goes with it");
}

// #4037 contract 2: when the refresher rotates the central login, the next redelivery pass
// pushes the new copy into the SSH-host crew's home through its vessel agent, and its
// access records the copy that landed. The host's own login stays untouched.
#[tokio::test]
async fn a_rotated_central_login_is_pushed_into_the_ssh_host_crew_home() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    runtime.launch().await.expect("delivered");
    let home = requester_home_at(HOST_BASE, &runtime.convoy, "codex");
    let landed = |deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>| {
        let [delivery] = deliveries.as_slice() else { panic!("one access: {deliveries:?}") };
        delivery.needs.iter().find(|need| need.need == "login").expect("login need").digest.clone()
    };
    let launched = landed(runtime.deliveries().await);

    write_central_login(&runtime.daemon_home, "access-two");
    let retry = super::super::access_layer::redeliver_central_logins(&runtime.state, NAMESPACE, true).await.expect("redelivery pass");

    assert_eq!(retry, None, "nothing left to retry");
    assert_holds_central_copy(&host, &home, "access-two");
    let rotated = landed(runtime.deliveries().await);
    assert_ne!(rotated, launched, "the access records the rotated copy");
    let writes = host.writes.lock().expect("writes").len();
    super::super::access_layer::redeliver_central_logins(&runtime.state, NAMESPACE, false).await.expect("idle pass");
    assert_eq!(host.writes.lock().expect("writes").len(), writes, "an unchanged copy is not pushed again");
    assert_eq!(host.entry(format!("{HOST_HOME}/.codex/auth.json")), Some(HostEntry::File(r#"{"tokens":"host-login"}"#.into())));
}

// #4037 contract 2: a push that fails leaves the crew on its earlier copy, is shown on its
// access in `convoy explain` with when it is next attempted, and lands once due. The pass
// answers that time, which credential delivery takes as its deadline (#3108).
#[tokio::test]
async fn a_failed_login_push_is_explained_and_retried() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    runtime.launch().await.expect("delivered");
    let home = requester_home_at(HOST_BASE, &runtime.convoy, "codex");
    let retry = |deliveries: Vec<flotilla_protocol::ExplainedAccessDelivery>| {
        let [delivery] = deliveries.as_slice() else { panic!("one access: {deliveries:?}") };
        delivery.retry.clone()
    };
    let redeliver = |rotated| super::super::access_layer::redeliver_central_logins(&runtime.state, NAMESPACE, rotated);
    host.refused.lock().expect("refused").insert(PathBuf::from(format!("{home}/auth.json")));

    write_central_login(&runtime.daemon_home, "access-two");
    let next = redeliver(true).await.expect("redelivery pass");

    assert_holds_central_copy(&host, &home, "access-one");
    let failed = retry(runtime.deliveries().await).expect("the failed push is explained");
    assert_eq!((failed.need.as_str(), failed.attempts), ("login", 1));
    assert!(failed.error.contains("No space left on device"), "{}", failed.error);
    assert!(next.is_some(), "the pass answers when the retry falls due");
    assert_eq!(redeliver(false).await.expect("redelivery pass"), next, "the retry is still pending");
    assert_eq!(retry(runtime.deliveries().await), Some(failed), "no attempt before the retry is due");

    // The disk recovers and the recorded retry falls due.
    host.refused.lock().expect("refused").clear();
    let records = runtime.daemon.resource_backend().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    let mut due = record.spec.clone();
    due.retry.as_mut().expect("retry").next_at = flotilla_schema::Timestamp::from_datetime(&chrono::DateTime::UNIX_EPOCH);
    let meta = InputMeta::builder().name(record.metadata.name.clone()).labels(record.metadata.labels.clone()).build();
    records.update(&meta, &record.metadata.resource_version, &due).await.expect("retry due");
    assert_eq!(redeliver(false).await.expect("redelivery pass"), None);
    assert_holds_central_copy(&host, &home, "access-two");
    assert_eq!(retry(runtime.deliveries().await), None);
}

// #3108: a push failing ten times in a row is no longer retried on a deadline; the next
// rotation tries it once more.
#[tokio::test]
async fn an_exhausted_login_push_waits_for_the_next_rotation() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    runtime.launch().await.expect("delivered");
    let home = requester_home_at(HOST_BASE, &runtime.convoy, "codex");
    let redeliver = |rotated| super::super::access_layer::redeliver_central_logins(&runtime.state, NAMESPACE, rotated);
    let attempts = || async {
        let [delivery] = runtime.deliveries().await.try_into().expect("one access");
        delivery.retry.map(|retry| retry.attempts)
    };
    host.refused.lock().expect("refused").insert(PathBuf::from(format!("{home}/auth.json")));
    write_central_login(&runtime.daemon_home, "access-two");
    redeliver(true).await.expect("redelivery pass");

    // The tenth failure, long due.
    let records = runtime.daemon.resource_backend().using::<flotilla_resources::AccessDelivery>(NAMESPACE);
    let [record] = records.list().await.expect("records").items.try_into().expect("one record");
    let mut exhausted = record.spec.clone();
    let retry = exhausted.retry.as_mut().expect("retry");
    retry.attempts = 10;
    retry.next_at = flotilla_schema::Timestamp::from_datetime(&chrono::DateTime::UNIX_EPOCH);
    let meta = InputMeta::builder().name(record.metadata.name.clone()).labels(record.metadata.labels.clone()).build();
    records.update(&meta, &record.metadata.resource_version, &exhausted).await.expect("exhausted");

    assert_eq!(redeliver(false).await.expect("redelivery pass"), None, "nothing to wait for");
    assert_eq!(attempts().await, Some(10), "no attempt without a rotation");
    assert_eq!(redeliver(true).await.expect("redelivery pass"), None, "past the limit after another failure");
    assert_eq!(attempts().await, Some(11), "a rotation tries once more");
    assert_holds_central_copy(&host, &home, "access-one");
}

// #3108: the refresher's rotation wakes credential delivery, which pushes the new copy; no
// timer stands behind it.
#[tokio::test]
async fn a_rotation_wakes_credential_delivery_to_push_the_new_login() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runtime = ssh_host_runtime(ssh_host(&host, &runtime_dir), "codex").await;
    runtime.launch().await.expect("delivered");
    let home = requester_home_at(HOST_BASE, &runtime.convoy, "codex");
    let delivery = super::super::credential_delivery::CredentialDelivery::new(Arc::clone(&runtime.state)).expect("a credential store");
    let _controller = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        delivery.run(runtime.state.controller_metrics.for_kind("CredentialDelivery")),
    ));

    write_central_login(&runtime.daemon_home, "access-two");
    runtime.state.credential_events.codex_rotated();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.json(format!("{home}/auth.json"))["tokens"]["access_token"] != "access-two" {
        assert!(tokio::time::Instant::now() < deadline, "the rotation was never pushed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_holds_central_copy(&host, &home, "access-two");
}

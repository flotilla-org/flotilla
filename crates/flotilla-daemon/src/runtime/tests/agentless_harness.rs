//! #3655 slice 1: agentless crews take the contained harness path. A crew on a
//! bare host launches with a flotilla-owned harness home under the host
//! runner's writable base, seeded with onboarding and workspace trust for its
//! checkout, and the host user's own harness configuration is never written.
//!
//! The SSH runner is the injected collaborator: `FakeHost` stands in for the
//! remote host's processes and filesystem behind `SshCommandRunner`, so every
//! file the launch writes on the host is observable.

use super::*;

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

use flotilla_provider_runtime::host_environment::{HostEnvironment, HostSupply};
use flotilla_provider_runtime::ssh_runner::SshCommandRunner;

use super::agentless_vessels::{VesselKey, VesselPaths};
use super::vessel_support::{vessels_over, RunnerVesselAgents};

const HOST_HOME: &str = "/home/crew";
/// What `SshCommandRunner::writable_config_base` resolves on the host.
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

    /// `agent_material::crew_environment`'s seeding script, on this filesystem.
    fn seed_crew_home(&self, base: &Path, home: &Path) {
        for file in ["config.toml", "settings.json", ".claude.json"] {
            if let (Some(HostEntry::File(content)), None) = (self.entry(base.join(file)), self.entry(home.join(file))) {
                self.put(&home.join(file), &content);
            }
        }
        for file in ["auth.json", ".credentials.json"] {
            if self.entry(base.join(file)).is_some() {
                self.files.lock().expect("files").insert(home.join(file), HostEntry::Link(base.join(file)));
            }
        }
    }
}

fn ok() -> Result<String, String> {
    Ok(String::new())
}

#[async_trait]
impl CommandRunner for FakeHost {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        match (cmd, args) {
            ("mkdir" | "chmod" | "mv" | "rm", _) => ok(),
            ("getconf", ["_NPROCESSORS_ONLN"]) => Ok("8\n".into()),
            // Executables and sockets lie outside the modelled filesystem: the
            // delivered CLI is present and the forwarded socket is listening.
            ("test", ["-x" | "-S", _]) => ok(),
            ("cat", [path]) => match self.entry(path) {
                Some(HostEntry::File(content)) => Ok(content),
                _ => Err(format!("cat: {path}: No such file")),
            },
            ("sh", ["-c", _, "flotilla-crew-home", base, home]) => {
                self.seed_crew_home(Path::new(base), Path::new(home));
                ok()
            }
            ("sh", ["-c", _, "flotilla-codex-ambient-login", home]) => {
                let ambient = Path::new(HOST_HOME).join(".codex/auth.json");
                let link = Path::new(home).join("auth.json");
                if self.entry(&ambient).is_some() && self.entry(&link).is_none() {
                    self.files.lock().expect("files").insert(link, HostEntry::Link(ambient));
                }
                ok()
            }
            _ => Err(format!("unexpected host command {cmd} {args:?}")),
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        match (cmd, args) {
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
        match (cmd, args) {
            // The Claude OAuth delivery preflight (`claude -p ok`).
            ("sh", ["-c", _, "flotilla-claude-oauth-preflight", _]) => Ok("ok".into()),
            _ => Err(format!("unexpected host command with input {cmd} {args:?}")),
        }
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        matches!(cmd, "claude" | "codex")
    }

    async fn writable_scratch_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from(HOST_BASE))
    }

    async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        Ok(PathBuf::from(HOST_BASE))
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

struct AgentlessLaunch {
    host: Arc<FakeHost>,
    env: TerminalEnvVars,
    command: String,
}

impl AgentlessLaunch {
    fn value(&self, name: &str) -> Option<String> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }
}

/// Launch one crew of `adapter` on an agentless host whose `claude` and
/// `codex` have never been run, through the terminal runtime's real launch
/// path with the fake host behind the SSH runner.
async fn launch_on_agentless_host(adapter: &str) -> AgentlessLaunch {
    try_launch_on_agentless_host(adapter, adapter == "claude-code").await.expect("launch agentless crew")
}

/// As [`launch_on_agentless_host`], optionally without the Claude credential grant.
async fn try_launch_on_agentless_host(adapter: &str, grant_claude: bool) -> Result<AgentlessLaunch, String> {
    // The forwarded daemon socket needs a short real directory (SUN_LEN).
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    // The agentless host's discovery bag is its SSH login environment: it
    // never names a flotilla environment.
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
        .with(EnvironmentAssertion::binary("codex", "/usr/local/bin/codex"));
    let launched = launch_agentless(
        AgentlessHost {
            runner: host.clone(),
            bag,
            host_environment: HostEnvironment::default(),
            checkout: CHECKOUT.to_string(),
            session_env: BTreeMap::new(),
        },
        adapter,
        grant_claude,
    )
    .await;
    match launched {
        Ok(session) => Ok(AgentlessLaunch { host, env: session.env_vars, command: session.command }),
        Err(error) => {
            assert_host_harness_untouched(&AgentlessLaunch { host, env: Vec::new(), command: String::new() });
            Err(error)
        }
    }
}

/// The agentless host a launch runs against.
struct AgentlessHost {
    /// The host's runner, as the SSH environment registers it.
    runner: Arc<dyn CommandRunner>,
    /// The host's discovery bag.
    bag: EnvironmentBag,
    host_environment: HostEnvironment,
    checkout: String,
    /// What the session spec declares, as credential delivery adds to it.
    session_env: BTreeMap<String, String>,
}

/// What the terminal pool was asked to start.
struct LaunchedSession {
    env_vars: TerminalEnvVars,
    command: String,
}

/// Launch one crew of `adapter` on `host` through the terminal runtime's real
/// launch path, with a fake terminal pool recording what it would start.
async fn launch_agentless(host: AgentlessHost, adapter: &str, grant_claude: bool) -> Result<LaunchedSession, String> {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).expect("config directory");
    fs::write(config_dir.join("daemon.toml"), "machine_id = \"agentless-harness-test\"\n").expect("daemon config");
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
            },
        )
        .await
        .expect("Claude credential declaration");

    let AgentlessHost { runner, bag, host_environment, checkout, session_env } = host;
    let environment_id = EnvironmentId::new(ENVIRONMENT);
    daemon
        .register_direct_environment_for_test(
            environment_id.clone(),
            Arc::clone(&runner),
            bag.clone(),
            Some(flotilla_protocol::qualified_path::HostId::new("b3")),
        )
        .expect("register agentless environment");
    let pool = Arc::new(FakeTerminalPool::new());
    let mut registry = ProviderRegistry::new();
    registry.agent_adapters = AgentAdapterRegistry::discover(&bag, Arc::clone(&runner));
    registry.terminal_pools.insert(
        "cleat",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "cleat"),
        Arc::clone(&pool) as Arc<dyn TerminalPool>,
    );
    let registry = Arc::new(registry);
    daemon.set_direct_environment_registry(&environment_id, Arc::clone(&registry)).expect("register agentless providers");
    let profile = AgentlessSshProfile {
        provisioning: LocalProvisioningProfile { host_direct_pool: "cleat".to_string(), ..manual_profile("b3", false) },
        environment_id: environment_id.clone(),
        destination: "crew@b3.example".to_string(),
        env_bag: bag.clone(),
        runner: Arc::clone(&runner),
        host_environment,
        max_vessels: None,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
        vessel_agent: Err("the test's vessel agents run on the host runner".into()),
    };
    register_agentless_ssh_resources(&backend, NAMESPACE, &local_host_id, &profile).await.expect("register agentless resources");
    let credential_store = Arc::new(CredentialStore::new(
        backend,
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        bag,
        Arc::clone(&runner),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agents = RunnerVesselAgents::new(Arc::clone(&runner), registry);
    let vessel = VesselPaths::new(&profile.env_bag, &VesselKey::new(&environment_id, "demo-work")).expect("vessel paths");
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            local_host_id.clone(),
            format!("host-direct-{local_host_id}"),
        )
        .with_agentless_ssh(vec![profile])
        // #3792: the crew runs in its vessel's agent, which here runs its
        // commands through the host runner, as the real agent runs them there.
        .with_agentless_vessels(Some(vessels_over(agents.clone(), temp.path())))
        .with_environment_tools(EnvironmentToolProvisioner::new(Vec::new()))
        .with_credential_store(credential_store)
        .with_agent_material(Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([] as [(&str, &str); 0]))))),
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
                convoy: "demo".to_string(),
                vessel_ref: "demo-work".to_string(),
            }),
            message: None,
        },
        cwd: checkout,
        env: session_env,
        pool: "cleat".to_string(),
    };
    let tags = if grant_claude {
        vec![TerminalSessionTag { key: CREDENTIAL_REF_SESSION_TAG.to_string(), value: "claude-max".to_string() }]
    } else {
        Vec::new()
    };
    let launched = TerminalControllerRuntime { state }.ensure_session("terminal-demo-work-coder", &spec, &tags).await;
    let ensured = pool.ensured.lock().await;
    if let Err(error) = launched {
        assert!(ensured.is_empty(), "a refused launch started a session");
        return Err(error);
    }
    let [launch] = ensured.as_slice() else { panic!("expected one launch, got {}", ensured.len()) };
    // The vessel was prepared before its crew started: its layer pushed, its
    // environment configured, and its daemon forward up at the socket the
    // session names.
    let prepared = agents.recorded(&vessel);
    assert_eq!(prepared.tool_pushes, 1);
    let socket = launch.env_vars.iter().find(|(name, _)| name == "FLOTILLA_DAEMON_SOCKET").map(|(_, value)| PathBuf::from(value));
    assert_eq!(socket.as_ref(), Some(&vessel.daemon_socket()));
    assert_eq!(prepared.environment.get("FLOTILLA_DAEMON_SOCKET").map(PathBuf::from), socket);
    assert!(matches!(prepared.forwards.as_slice(), [(guest, _)] if *guest == vessel.daemon_socket()), "{:?}", prepared.forwards);
    Ok(LaunchedSession { env_vars: launch.env_vars.clone(), command: launch.command.clone() })
}

/// No launch writes the host user's harness configuration; it reads as it did.
fn assert_host_harness_untouched(launch: &AgentlessLaunch) {
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
fn assert_image_harness_environment(launch: &AgentlessLaunch) {
    for (name, value) in
        [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_INSTALLATION_CHECKS", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("LANG", "en_US.UTF-8")]
    {
        assert_eq!(launch.value(name).as_deref(), Some(value), "{name}");
    }
}

// Acceptance (#3655): a Claude crew on a host whose `claude` has never run
// starts with `CLAUDE_CONFIG_DIR` at its flotilla-owned home (the delivered
// credential home under the host's writable base, in the crew's own
// `crews/<role>` home), seeded with onboarding, workspace trust for the
// checkout, and attribution suppression, so no first-run dialog can appear.
// The host user's `~/.claude.json` and `~/.claude` are never written.
#[tokio::test]
async fn agentless_claude_crew_starts_in_a_prepared_flotilla_home() {
    let launch = launch_on_agentless_host("claude-code").await;
    let home = format!("{HOST_BASE}/credentials/claude-max/claude/crews/coder");

    assert_eq!(launch.value("CLAUDE_CONFIG_DIR").as_deref(), Some(home.as_str()));
    assert_eq!(launch.value("CLAUDE_CODE_OAUTH_TOKEN").as_deref(), Some("oauth-secret-material"));
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

// Acceptance (#3655): a Codex crew on an agentless host starts with
// `CODEX_HOME` at a flotilla-owned home under the host's writable base, with
// workspace trust for the checkout in that home's own `config.toml`. The host
// login is linked, never copied, and the host's `~/.codex/config.toml` is
// never written.
#[tokio::test]
async fn agentless_codex_crew_starts_in_a_prepared_flotilla_home() {
    let launch = launch_on_agentless_host("codex").await;
    let home = format!("{HOST_BASE}/agent-homes/{ENVIRONMENT}/codex");

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
    assert_eq!(launch.host.entry(format!("{home}/auth.json")), Some(HostEntry::Link(PathBuf::from("/home/crew/.codex/auth.json"))));
    assert_image_harness_environment(&launch);
    assert_host_harness_untouched(&launch);
}

// Review #3659: an agentless Claude crew has no ambient-login fallback, since
// the host login's configuration is never selected. Without a delivered OAuth
// credential the launch is refused with a reason naming the missing grant,
// and nothing starts or touches the host's configuration.
#[tokio::test]
async fn agentless_claude_crew_without_a_delivered_credential_is_refused_with_the_reason() {
    let Err(error) = try_launch_on_agentless_host("claude-code", false).await else {
        panic!("an agentless Claude crew without a credential must not launch");
    };
    assert!(error.contains("require a delivered Claude OAuth credential") && error.contains("claude-oauth"), "{error}");
}

/// Stands in for `ssh` to an agentless host and its sshd: the remote command
/// string runs in `/bin/sh -c`, as sshd runs it for a user whose shell is
/// POSIX, in a clean login environment whose home is a temporary directory.
struct LoopbackSshHost {
    home: PathBuf,
    runtime_dir: PathBuf,
}

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

fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().expect("parent")).expect("executable directory");
    fs::write(path, script).expect("executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable mode");
}

// Acceptance (#3722): an agentless crew's harness sees flotilla's composed
// environment, never the host login profile's. The host's profile prepends
// `~/.local/bin`, which holds a plain `gh` and a stale `flotilla`; the host
// declares a `rust` supply. The launch runs through the real SSH runner, and
// cleat's `$SHELL -lc` is reproduced with the recorded command and
// environment, so the profile does run before the harness. The harness's
// PATH is the delivered CLI, the GitHub credential wrapper, the supplies and
// the system tail; `gh` and `flotilla` resolve to the wrapper and the CLI;
// the profile directory is absent; `RUSTUP_HOME` is set; and no runner
// command read the profile. Mutation check: `sh -lc` in the SSH runner fails it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn agentless_harness_path_is_composed_and_never_the_login_profile() {
    let remote = TempDir::new().expect("remote host");
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
    // The harness answers the delivery preflight, then reports what it sees.
    write_executable(
        &home.join(".local/opt/claude/bin/claude"),
        "#!/bin/sh\n[ \"$1\" = -p ] && { printf ok; exit 0; }\n\
         { printf '%s\\n' \"$PATH\"; command -v gh; command -v flotilla; printf '%s\\n' \"${RUSTUP_HOME-unset}\"; } > \"$HOME/harness-report\"\n",
    );
    let host_environment = HostEnvironment::new(vec![
        HostSupply {
            name: "rust".into(),
            path: vec!["~/.cargo/bin".into()],
            env: vec![("CARGO_HOME".into(), "~/.cargo".into()), ("RUSTUP_HOME".into(), "~/.rustup".into())],
        },
        HostSupply { name: "claude".into(), path: vec!["~/.local/opt/claude/bin".into()], env: Vec::new() },
    ])
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
    let mut bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", home.join(".local/opt/claude/bin/claude").display().to_string()));
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

    // The vessel's generation layer holds the delivered CLI.
    let vessel = VesselPaths::new(&bag, &VesselKey::new(&EnvironmentId::new(ENVIRONMENT), "demo-work")).expect("vessel paths");
    let cli_dir = vessel.cli_directory();
    write_executable(&cli_dir.join("flotilla"), "#!/bin/sh\necho generation-cli\n");

    let session = launch_agentless(
        AgentlessHost { runner, bag, host_environment, checkout: checkout.display().to_string(), session_env },
        "claude-code",
        true,
    )
    .await
    .expect("launch agentless crew");

    // cleat starts the command through `$SHELL -lc` with the launch's
    // environment over the login basics.
    let mut cleat = vec!["-i".to_string(), format!("HOME={}", home.display()), "USER=crew".into(), "SHELL=/bin/sh".into()];
    cleat.push(format!("FLOTILLA_PRELUDE_DIR={}", remote.path().join("no-preludes").display()));
    cleat.extend(session.env_vars.iter().map(|(name, value)| format!("{name}={value}")));
    cleat.extend(["/bin/sh".to_string(), "-lc".to_string(), session.command.clone()]);
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
        format!("{home_text}/.local/opt/claude/bin"),
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
#[tokio::test]
async fn a_supply_overriding_a_flotilla_variable_is_a_host_configuration_error() {
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"));
    // The harness environment flotilla gives every agentless session sets LANG.
    let host_environment =
        HostEnvironment::new(vec![HostSupply { name: "locale".into(), path: Vec::new(), env: vec![("LANG".into(), "C.UTF-8".into())] }])
            .expect("declared supply");
    let Err(error) = launch_agentless(
        AgentlessHost { runner: host.clone(), bag, host_environment, checkout: CHECKOUT.to_string(), session_env: BTreeMap::new() },
        "claude-code",
        true,
    )
    .await
    else {
        panic!("a colliding supply must not launch");
    };
    assert_eq!(error, "host configuration error for host-direct-b3: supply `locale` sets LANG, which flotilla sets");
}

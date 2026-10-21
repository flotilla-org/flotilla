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

use super::agentless_reach::{AgentlessReach, CliSource, ForwardTiming};
use crate::server::environment_sockets::{EnvironmentSocketRegistry, EnvironmentSocketService};

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
            // Remote forward socket preparation. `ListeningSsh` binds the
            // forwarded socket for real, so its directory exists for real.
            ("sh", ["-c", script, "sh", directory, _socket]) if script.contains("umask 077") => {
                fs::create_dir_all(directory).map_err(|error| error.to_string())?;
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

    // The forwarded daemon socket needs a short real directory (SUN_LEN).
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let host = Arc::new(FakeHost::with_files(HOST_HARNESS_FILES));
    let runner: Arc<dyn CommandRunner> = host.clone();
    // The agentless host's discovery bag is its SSH login environment: it
    // never names a flotilla environment.
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", HOST_HOME))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
        .with(EnvironmentAssertion::binary("codex", "/usr/local/bin/codex"));
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
    daemon.set_direct_environment_registry(&environment_id, Arc::new(registry)).expect("register agentless providers");
    let profile = AgentlessSshProfile {
        provisioning: LocalProvisioningProfile { host_direct_pool: "cleat".to_string(), ..manual_profile("b3", false) },
        environment_id: environment_id.clone(),
        destination: "crew@b3.example".to_string(),
        env_bag: bag.clone(),
        runner: Arc::clone(&runner),
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
    };
    register_agentless_ssh_resources(&backend, NAMESPACE, &local_host_id, &profile).await.expect("register agentless resources");
    let own_cli = temp.path().join("controller-bin/flotilla");
    let credential_store = Arc::new(CredentialStore::new(
        backend,
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        bag,
        Arc::clone(&runner),
        config.state_dir().as_path().to_path_buf(),
    ));
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
        .with_agentless_reach(Some(Arc::new(AgentlessReach::new(
            Arc::new(ListeningSsh),
            agentless_socket_service(temp.path()),
            CliSource::new(Ok(own_cli), "gen-1"),
            ForwardTiming::default(),
        ))))
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
        cwd: CHECKOUT.to_string(),
        env: BTreeMap::new(),
        pool: "cleat".to_string(),
    };
    let tags = if adapter == "claude-code" {
        vec![TerminalSessionTag { key: CREDENTIAL_REF_SESSION_TAG.to_string(), value: "claude-max".to_string() }]
    } else {
        Vec::new()
    };
    TerminalControllerRuntime { state }.ensure_session("terminal-demo-work-coder", &spec, &tags).await.expect("launch agentless crew");
    let ensured = pool.ensured.lock().await;
    let [launch] = ensured.as_slice() else { panic!("expected one launch, got {}", ensured.len()) };
    AgentlessLaunch { host, env: launch.env_vars.clone() }
}

fn agentless_socket_service(state: &Path) -> EnvironmentSocketService {
    EnvironmentSocketService::new(
        Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        DaemonHostPath::new(state.join("env")),
        |listener, _, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        },
    )
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
    assert!(config.contains("command = \"flotilla hook codex stop\""), "contained Codex installs native hooks: {config}");
    assert_eq!(launch.host.entry(format!("{home}/auth.json")), Some(HostEntry::Link(PathBuf::from("/home/crew/.codex/auth.json"))));
    assert_image_harness_environment(&launch);
    assert_host_harness_untouched(&launch);
}

use std::{
    os::unix::process::ExitStatusExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex as StdMutex,
    },
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_provider_runtime::CommandOutput;

use super::*;
use crate::server::environment_sockets::{EnvironmentSocketRegistry, SocketAliasGuard};

type Current = Arc<StdMutex<Option<Arc<AtomicBool>>>>;

/// Stands in for the controller's `ssh`: each spawn is one forward process,
/// alive until it is killed by the supervisor or dropped by the test.
#[derive(Default)]
struct FakeSsh {
    spawns: StdMutex<Vec<Vec<String>>>,
    current: Current,
}

impl FakeSsh {
    fn spawns(&self) -> Vec<Vec<String>> {
        self.spawns.lock().expect("spawns").clone()
    }

    /// Simulate the network or sshd dropping the forward.
    fn drop_forward(&self) {
        if let Some(alive) = self.current.lock().expect("current").take() {
            alive.store(false, Ordering::SeqCst);
        }
    }

    fn forward_alive(&self) -> bool {
        self.current.lock().expect("current").as_ref().is_some_and(|alive| alive.load(Ordering::SeqCst))
    }
}

struct FakeProcess(Arc<AtomicBool>);

#[async_trait]
impl CommandProcess for FakeProcess {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        Ok((!self.0.load(Ordering::SeqCst)).then(|| std::process::ExitStatus::from_raw(255 << 8)))
    }

    async fn kill(&mut self) -> Result<(), String> {
        self.0.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
        while self.0.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(std::process::ExitStatus::from_raw(255 << 8))
    }
}

#[async_trait]
impl CommandRunner for FakeSsh {
    async fn run(&self, cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        Err(format!("controller runner only spawns forwards, not {cmd}"))
    }

    async fn run_output(&self, cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Err(format!("controller runner only spawns forwards, not {cmd}"))
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }

    async fn spawn_long_lived(
        &self,
        cmd: &str,
        args: &[&str],
        _cwd: &Path,
        _label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        assert_eq!(cmd, "ssh");
        self.spawns.lock().expect("spawns").push(args.iter().map(|arg| arg.to_string()).collect());
        let alive = Arc::new(AtomicBool::new(true));
        *self.current.lock().expect("current") = Some(Arc::clone(&alive));
        Ok(Box::new(FakeProcess(alive)))
    }
}

/// Stands in for the agentless host's SSH runner. Its reverse socket exists
/// exactly while the fake forward process lives.
struct FakeHost {
    forward: Current,
    uname: String,
    fail_prepare: AtomicBool,
    installed: AtomicBool,
    calls: StdMutex<Vec<Vec<String>>>,
    writes: StdMutex<Vec<(PathBuf, PathBuf)>>,
}

impl FakeHost {
    fn new(ssh: &FakeSsh, uname: &str) -> Self {
        Self {
            forward: Arc::clone(&ssh.current),
            uname: uname.into(),
            fail_prepare: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            calls: StdMutex::new(Vec::new()),
            writes: StdMutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls").clone()
    }

    fn writes(&self) -> Vec<(PathBuf, PathBuf)> {
        self.writes.lock().expect("writes").clone()
    }
}

#[async_trait]
impl CommandRunner for FakeHost {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.calls.lock().expect("calls").push(std::iter::once(cmd).chain(args.iter().copied()).map(str::to_string).collect());
        match (cmd, args.first().copied()) {
            ("sh", _) if self.fail_prepare.load(Ordering::SeqCst) => Err("mkdir: permission denied".into()),
            ("sh", _) => Ok(String::new()),
            ("test", Some("-S")) if self.forward.lock().expect("forward").as_ref().is_some_and(|alive| alive.load(Ordering::SeqCst)) => {
                Ok(String::new())
            }
            ("test", Some("-x")) if self.installed.load(Ordering::SeqCst) => Ok(String::new()),
            ("test", _) => Err(String::new()),
            ("uname", _) => Ok(format!("{}\n", self.uname)),
            ("chmod", _) => {
                self.installed.store(true, Ordering::SeqCst);
                Ok(String::new())
            }
            ("mkdir" | "rm", _) => Ok(String::new()),
            _ => Err(format!("unexpected host command {cmd}")),
        }
    }

    async fn run_output(&self, cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Err(format!("unexpected host command {cmd}"))
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }

    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.writes.lock().expect("writes").push((source.to_path_buf(), destination.to_path_buf()));
        Ok(())
    }
}

fn fast_timing() -> ForwardTiming {
    ForwardTiming {
        open_timeout: Duration::from_secs(5),
        probe_interval: Duration::from_millis(2),
        liveness_interval: Duration::from_millis(2),
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
    }
}

fn host_bag() -> EnvironmentBag {
    EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", "/home/crew"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", "/run/user/1000"))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
}

/// The controller's own CLI and an optional sibling platform build, laid out
/// as a fleet release (`releases/<generation>/bin`, `.../platforms`).
struct Release {
    _root: tempfile::TempDir,
    own: PathBuf,
}

impl Release {
    fn new(platforms: &[&str]) -> Self {
        let root = tempfile::tempdir().expect("release");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let own = bin.join("flotilla");
        std::fs::write(&own, "controller cli").expect("own cli");
        for platform in platforms {
            let directory = root.path().join("platforms").join(platform).join("bin");
            std::fs::create_dir_all(&directory).expect("platform bin");
            std::fs::write(directory.join("flotilla"), *platform).expect("platform cli");
        }
        Self { _root: root, own }
    }

    fn source(&self) -> CliSource {
        CliSource::new(Ok(self.own.clone()), "gen 7+abc")
    }
}

struct Harness {
    ssh: Arc<FakeSsh>,
    host: Arc<FakeHost>,
    reach: AgentlessReach,
    target: ReachTarget,
    _release: Release,
    _alias: SocketAliasGuard,
    _sockets: flotilla_test_support::TestSocketDir,
}

fn controller_uname() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "Linux x86_64",
        ("linux", "aarch64") => "Linux aarch64",
        ("macos", "aarch64") => "Darwin arm64",
        ("macos", "x86_64") => "Darwin x86_64",
        other => panic!("unsupported test platform {other:?}"),
    }
}

fn harness_with(uname: &str, release: Release) -> Harness {
    let ssh = Arc::new(FakeSsh::default());
    let host = Arc::new(FakeHost::new(&ssh, uname));
    let sockets = flotilla_test_support::TestSocketDir::new();
    let state = DaemonHostPath::new(sockets.socket_path("env"));
    let alias = SocketAliasGuard::new(&state);
    let service = EnvironmentSocketService::new(Arc::new(Mutex::new(EnvironmentSocketRegistry::new())), state, |listener, _, _| {
        tokio::spawn(async move {
            let _listener = listener;
            std::future::pending::<()>().await
        })
    });
    let reach = AgentlessReach::new(Arc::clone(&ssh) as Arc<dyn CommandRunner>, service, release.source(), fast_timing());
    let target = ReachTarget {
        environment: EnvironmentId::new("host-direct-b1"),
        destination: "crew@b1".into(),
        remote: Arc::clone(&host) as Arc<dyn CommandRunner>,
        env_bag: host_bag(),
    };
    Harness { ssh, host, reach, target, _release: release, _alias: alias, _sockets: sockets }
}

fn harness() -> Harness {
    harness_with(controller_uname(), Release::new(&[]))
}

async fn wait_for_health(reach: &AgentlessReach, environment: &EnvironmentId, wanted: impl Fn(&ForwardHealth) -> bool) -> ForwardHealth {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(health) = reach.health(environment).await.filter(|health| wanted(health)) {
                return health;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("forward health transition")
}

fn reverse_spec(args: &[String]) -> &str {
    &args[args.iter().position(|arg| arg == "-R").expect("reverse forward") + 1]
}

// #3618 acceptance: the forward carries the environment socket (never the main
// socket) to an owner-only remote path; killing it shows the reason while it is
// down, and it is re-established on the same path after stale-socket cleanup.
#[tokio::test]
async fn dropped_forward_is_reported_then_re_established() {
    let harness = harness();
    let paths = harness.reach.prepare_session(&harness.target).await.expect("session reach");
    assert_eq!(
        paths.remote_socket,
        Path::new("/run/user/1000/flotilla/env").join(directory_name(&harness.target.environment)).join("flotilla.sock")
    );
    assert_eq!(harness.reach.health(&harness.target.environment).await, Some(ForwardHealth::Up));
    let spawns = harness.ssh.spawns();
    assert_eq!(spawns.len(), 1);
    let (remote, local) = reverse_spec(&spawns[0]).split_once(':').expect("forward spec");
    assert_eq!(Path::new(remote), paths.remote_socket);
    assert!(local.ends_with(&format!("{}/flotilla.sock", directory_name(&harness.target.environment))), "environment socket, got {local}");
    assert!(!local.ends_with("/run/flotilla.sock"));
    assert_eq!(spawns[0].last().map(String::as_str), Some("crew@b1"));
    let prepare = |calls: &[Vec<String>]| calls.iter().filter(|call| call[0] == "sh" && call[2] == PREPARE_REMOTE_SOCKET).count();
    assert_eq!(prepare(&harness.host.calls()), 1, "stale remote socket removed before the first dial");

    harness.ssh.drop_forward();
    let down = wait_for_health(&harness.reach, &harness.target.environment, |health| matches!(health, ForwardHealth::Down(_))).await;
    let reason = down.unreachable_reason().expect("down forward has a reason");
    assert!(reason.starts_with("daemon forward down: ssh forward exited"), "{reason}");

    wait_for_health(&harness.reach, &harness.target.environment, |health| *health == ForwardHealth::Up).await;
    let spawns = harness.ssh.spawns();
    assert_eq!(spawns.len(), 2, "one replacement forward");
    assert_eq!(reverse_spec(&spawns[1]), reverse_spec(&spawns[0]), "sessions keep their baked-in socket path");
    assert_eq!(prepare(&harness.host.calls()), 2, "stale remote socket removed before the re-dial");
    assert!(harness.ssh.forward_alive());
}

// Ensuring an established forward never dials a second one; a stopped
// supervisor (e.g. after a controller restart) is replaced.
#[tokio::test]
async fn ensure_is_idempotent_and_retire_stops_the_forward() {
    let harness = harness();
    harness.reach.prepare_session(&harness.target).await.expect("first");
    harness.reach.ensure_forward(&harness.target).await.expect("again");
    harness.reach.prepare_session(&harness.target).await.expect("second session");
    assert_eq!(harness.ssh.spawns().len(), 1);
    assert_eq!(harness.host.writes().len(), 1, "the CLI is installed once per generation");

    let local = reverse_spec(&harness.ssh.spawns()[0]).split_once(':').expect("spec").1.to_string();
    assert!(Path::new(&local).exists());
    harness.reach.retire(&harness.target.environment).await.expect("retire");
    assert!(!harness.ssh.forward_alive(), "teardown stops the forward process");
    assert!(harness
        .host
        .calls()
        .iter()
        .any(|call| call[0] == "rm"
            && call.last() == Some(&reverse_spec(&harness.ssh.spawns()[0]).split_once(':').expect("spec").0.to_string())));
    assert!(!Path::new(&local).exists(), "teardown retires the environment socket");
    assert_eq!(harness.reach.health(&harness.target.environment).await, None);
    harness.reach.retire(&harness.target.environment).await.expect("duplicate teardown");
}

// A host that cannot host the forward fails the launch with the reason rather
// than starting a crew whose hooks cannot reach the daemon.
#[tokio::test]
async fn unreachable_host_fails_the_launch_with_its_reason() {
    let harness = harness();
    harness.host.fail_prepare.store(true, Ordering::SeqCst);
    let error = harness.reach.prepare_session(&harness.target).await.expect_err("forward cannot open");
    assert!(error.contains("permission denied"), "{error}");
    assert!(harness.ssh.spawns().is_empty());
    let health = harness.reach.health(&harness.target.environment).await.expect("supervised");
    assert!(health.unreachable_reason().is_some_and(|reason| reason.contains("prepare remote socket")));
    harness.host.fail_prepare.store(false, Ordering::SeqCst);
    wait_for_health(&harness.reach, &harness.target.environment, |health| *health == ForwardHealth::Up).await;
}

// #3618 binary delivery: the controller generation's build for the host's
// platform wins; the controller's own CLI serves a same-platform host; a host
// with no build is refused with the reason.
#[tokio::test]
async fn cli_delivery_follows_the_host_platform() {
    let foreign = if std::env::consts::ARCH == "x86_64" { ("Linux aarch64", "linux-aarch64") } else { ("Linux x86_64", "linux-x86_64") };

    let platform_build = harness_with(foreign.0, Release::new(&[foreign.1]));
    let paths = platform_build.reach.prepare_session(&platform_build.target).await.expect("platform build");
    let (source, destination) = platform_build.host.writes().pop().expect("installed");
    assert!(source.ends_with(format!("platforms/{}/bin/flotilla", foreign.1)));
    assert_eq!(destination, paths.cli_directory.join("flotilla"));
    assert_eq!(paths.cli_directory, Path::new("/home/crew/.local/state/flotilla/agentless-cli/gen_7_abc"));

    let same_platform = harness();
    same_platform.reach.prepare_session(&same_platform.target).await.expect("controller platform");
    assert_eq!(same_platform.host.writes().pop().expect("installed").0, same_platform._release.own);

    let missing = harness_with(foreign.0, Release::new(&[]));
    let error = missing.reach.prepare_session(&missing.target).await.expect_err("no build");
    assert!(error.contains(&format!("has no flotilla CLI build for {}", foreign.1)), "{error}");
    assert!(missing.host.writes().is_empty());
    assert!(missing.ssh.spawns().is_empty(), "no forward without a CLI");
}

#[test]
fn platform_names_normalise_uname() {
    assert_eq!(HostPlatform::from_uname("Linux", "x86_64").to_string(), "linux-x86_64");
    assert_eq!(HostPlatform::from_uname("Linux", "amd64").to_string(), "linux-x86_64");
    assert_eq!(HostPlatform::from_uname("Darwin", "arm64").to_string(), "macos-aarch64");
    assert_eq!(HostPlatform::from_uname("FreeBSD\n", "riscv64\n").to_string(), "freebsd-riscv64");
    assert_eq!(HostPlatform::from_uname(std::env::consts::OS, std::env::consts::ARCH).arch, HostPlatform::controller().arch);
}

// Remote paths are deterministic (sessions bake them in), prefer the owner-only
// runtime directory, and refuse a socket path the kernel cannot bind.
#[test]
fn remote_paths_are_stable_and_bounded() {
    let cli = CliSource::new(Err("unused".into()), "g");
    let target = |bag: EnvironmentBag| ReachTarget {
        environment: EnvironmentId::new("host-direct-b1"),
        destination: "b1".into(),
        remote: Arc::new(FakeSsh::default()),
        env_bag: bag,
    };
    let runtime = target(host_bag()).paths(&cli).expect("paths");
    assert_eq!(runtime, target(host_bag()).paths(&cli).expect("again"));
    assert!(runtime.remote_socket.starts_with("/run/user/1000/flotilla/env"));
    let state =
        target(EnvironmentBag::new().with(EnvironmentAssertion::env_var("HOME", "/home/crew"))).paths(&cli).expect("no runtime dir");
    assert!(state.remote_socket.starts_with("/home/crew/.local/state/flotilla/run/env"));
    let long = target(EnvironmentBag::new().with(EnvironmentAssertion::env_var("HOME", format!("/home/{}", "x".repeat(80)))));
    assert!(long.paths(&cli).expect_err("SUN_LEN").contains("SUN_LEN"));
    assert!(target(EnvironmentBag::new()).paths(&cli).expect_err("no HOME").contains("no HOME"));
}

#[test]
fn session_environment_points_at_the_forward_and_cli() {
    let paths =
        ReachPaths { remote_socket: PathBuf::from("/run/user/1000/flotilla/env/x/flotilla.sock"), cli_directory: PathBuf::from("/cli") };
    let mut env = vec![("FLOTILLA_DAEMON_SOCKET".to_string(), "/main.sock".to_string()), ("KEEP".to_string(), "1".to_string())];
    apply_reach_environment(&mut env, "host-direct-b1", &paths, Some("/usr/bin"));
    fn get(env: &[(String, String)], name: &str) -> Vec<String> {
        env.iter().filter(|(key, _)| key == name).map(|(_, value)| value.clone()).collect()
    }
    assert_eq!(get(&env, DAEMON_SOCKET_ENV), ["/run/user/1000/flotilla/env/x/flotilla.sock"]);
    assert_eq!(get(&env, "FLOTILLA_ENVIRONMENT_ID"), ["host-direct-b1"]);
    assert_eq!(get(&env, CONTAINED_DAEMON_REQUIRED_ENV), ["1"], "the CLI must never spawn a daemon on the host");
    assert_eq!(get(&env, "PATH"), ["/cli:/usr/bin"]);
    assert_eq!(get(&env, "KEEP"), ["1"]);
    apply_reach_environment(&mut env, "host-direct-b1", &paths, Some("/ignored"));
    assert_eq!(get(&env, "PATH"), ["/cli:/usr/bin"], "an explicit session PATH is kept, and the CLI is prepended once");
}

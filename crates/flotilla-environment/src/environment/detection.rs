//! Provider-owned observation execution with no inherited authentication or
//! configuration. The bounded owner finishes cleanup even if its caller leaves.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_provider_runtime::{execution_context::ObservationContext, ChannelLabel, CommandOutput, CommandRunner};

pub(super) const OBSERVATION_SCRIPT: &str = r#"
set -eu
# Monitor mode gives each owned background job a distinct process group.
# Its supervisor stays alive after the harness exits, keeping group ownership
# until every descendant is stopped and reaped by cleanup.
set -m
root=$1
count=$2
shift 2
# The prepared base layer, as NAME=value words with every path absolute: no
# word may depend on the HOME this script replaces.
base=("${@:1:$count}")
shift "$count"
count=$1
shift
homes=("${@:1:$count}")
shift "$count"
umask 077
mkdir -m 700 "$root"
child=
watchdog=
cleanup() {
    # Signals during cleanup must not cut it short. The watchdog stays alive until
    # the child is stopped and the root is gone, so a cleanup that is still cut short
    # (SIGKILL) leaves the watchdog to finish it.
    trap '' HUP INT TERM
    if [ -n "$child" ]; then kill -KILL -- "-$child" 2>/dev/null || :; wait "$child" 2>/dev/null || :; fi
    rm -rf -- "$root"
    if [ -n "$watchdog" ]; then kill -KILL -- "-$watchdog" 2>/dev/null || :; wait "$watchdog" 2>/dev/null || :; fi
}
trap cleanup EXIT
trap 'trap "" HUP INT TERM; exit 143' HUP INT TERM
mkdir -p "$root/home" "$root/config" "$root/cache" "$root/state" "$root/runtime" "$root/accounts" "$root/tmp" "$root/cwd"
overlay=(HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_CACHE_HOME="$root/cache" XDG_STATE_HOME="$root/state" XDG_RUNTIME_DIR="$root/runtime" TMPDIR="$root/tmp")
for name in ${homes[@]+"${homes[@]}"}; do
    mkdir "$root/accounts/$name"
    overlay+=("$name=$root/accounts/$name")
done
cd "$root/cwd"
parent=$$
(
    set +m
    status=0
    env -i ${base[@]+"${base[@]}"} "${overlay[@]}" "$@" || status=$?
    printf '%s' "$status" > "$root/status.tmp"
    mv "$root/status.tmp" "$root/status"
    # Holding the group until cleanup is bounded and ends with the wrapper, so a
    # child whose wrapper is gone never loops on.
    limit=$((SECONDS + 12))
    while [ "$SECONDS" -lt "$limit" ] && kill -0 "$parent" 2>/dev/null; do sleep 1; done
) &
child=$!
# A target-local fallback survives a disconnected SSH client or a hard local
# owner exit. The live supervisor reserves its group until this cleanup runs.
(sleep 11; kill -KILL -- "-$child" 2>/dev/null || :; rm -rf -- "$root") </dev/null >/dev/null 2>&1 &
watchdog=$!
deadline=$((SECONDS + 10))
while [ ! -f "$root/status" ]; do
    [ "$SECONDS" -lt "$deadline" ] || exit 143
    sleep 0.01
done
status=$(cat "$root/status")
exit "$status"
"#;

/// The isolation shell's argv: the base layer and the families' account
/// homes go in as words, never through the target's own environment.
pub(super) fn observation_args(root: &str, context: &ObservationContext, binary: &str, args: &[&str]) -> Vec<String> {
    let base = context.assignments();
    let homes = context.account_homes();
    ["-u", "BASH_ENV", "-u", "ENV", "bash", "--noprofile", "--norc", "-p", "-c", OBSERVATION_SCRIPT, "flotilla-observation", root]
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(base.len().to_string()))
        .chain(base)
        .chain(std::iter::once(homes.len().to_string()))
        .chain(homes)
        .chain(std::iter::once(binary.to_string()))
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect()
}

pub(super) async fn host_detection(
    runner: Arc<dyn CommandRunner>,
    context: &ObservationContext,
    binary: &str,
    args: &[&str],
    _scratch: &Path,
) -> Result<CommandOutput, String> {
    let root = format!("/tmp/flotilla-observation-{}", uuid::Uuid::new_v4());
    let command = observation_args(&root, context, binary, args);
    run_with_cleanup(runner, "env", command, PathBuf::from("/"), "rm", vec!["-rf".into(), "--".into(), root], true).await
}

// The spawned owner is deliberately not aborted with a dropped JoinHandle:
// cancellation must not strand writable state on an SSH host or in an image.
// Both execution and cleanup are bounded and use the same injected endpoint.
pub(super) async fn run_with_cleanup(
    runner: Arc<dyn CommandRunner>,
    command: &str,
    args: Vec<String>,
    cwd: PathBuf,
    cleanup_command: &str,
    cleanup_args: Vec<String>,
    cleanup_always: bool,
) -> Result<CommandOutput, String> {
    let command = command.to_string();
    let cleanup_command = cleanup_command.to_string();
    tokio::spawn(async move {
        let argv = args.iter().map(String::as_str).collect::<Vec<_>>();
        let result = tokio::time::timeout(Duration::from_secs(12), runner.run_output(&command, &argv, &cwd, &ChannelLabel::Default)).await;
        if cleanup_always || result.is_err() || result.as_ref().is_ok_and(Result::is_err) {
            let argv = cleanup_args.iter().map(String::as_str).collect::<Vec<_>>();
            let cleanup =
                tokio::time::timeout(Duration::from_secs(2), runner.run_output(&cleanup_command, &argv, &cwd, &ChannelLabel::Default))
                    .await;
            if !cleanup.is_ok_and(|result| result.is_ok_and(|output| output.success())) {
                // Never include command output: a misbehaving harness can put
                // inherited secrets or profile contents in diagnostics.
                tracing::warn!("observation state cleanup failed or exceeded its deadline");
                return Err("observation state cleanup failed or exceeded its deadline".into());
            }
        }
        result.map_err(|_| "observation exceeded its deadline".to_string())?
    })
    .await
    .map_err(|_| "observation owner failed".to_string())?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::environment::{
        agent::{
            client::{AgentClient, Configuration},
            memory::MemoryAgent,
            AgentRunner,
        },
        command_provider, EnvironmentInput,
    };
    use flotilla_provider_runtime::{
        execution_context::{AccountState, AccountVariables, BaseLayer, BoundLayer, ExecutionPreparer},
        host_environment::{HostEnvironment, HostSupply},
        ssh_runner::SshCommandRunner,
        ProcessCommandRunner,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    // Process/SSH/container transport stand-in. It executes the real isolation
    // shell while replacing only the SSH connection or container namespace.
    struct Endpoint {
        operator: PathBuf,
        image_root: PathBuf,
        roots: Mutex<Vec<PathBuf>>,
    }
    #[async_trait::async_trait]
    impl CommandRunner for Endpoint {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let output = self.run_output(cmd, args, cwd, label).await?;
            if output.success() {
                Ok(output.stdout)
            } else {
                Err(output.stderr)
            }
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            let args = args.iter().map(|arg| scaled_deadlines(arg)).collect::<Vec<_>>();
            let args = &args.iter().map(String::as_str).collect::<Vec<_>>()[..];
            let mut command = cmd.to_string();
            let mut argv = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
            if cmd == "ssh" {
                command = "sh".into();
                argv = vec!["-c".into(), args.last().expect("remote command").to_string()];
            } else if cmd == "docker" {
                if args[..2] == ["image", "inspect"] {
                    // The image declares the search path its tools are on.
                    let path = std::env::var("PATH").expect("test PATH");
                    return Ok(CommandOutput { stdout: format!("[\"PATH={path}\"]"), stderr: String::new(), exit_code: Some(0) });
                }
                if args[0] == "rm" {
                    let _ = std::fs::remove_dir_all(&self.image_root);
                    return Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) });
                }
                assert!(args.contains(&"--network=none"));
                assert!(args.contains(&"--pull=never"));
                assert!(args.windows(2).any(|pair| pair == ["--entrypoint", "env"]));
                command = "env".into();
                argv = args[args.iter().position(|arg| *arg == "-u").expect("isolation shell")..].iter().map(|arg| (*arg).into()).collect();
                let root_index = argv.iter().position(|arg| arg == "flotilla-observation").expect("observation label") + 1;
                argv[root_index] = self.image_root.to_string_lossy().into_owned();
            }
            if command == "env" {
                if let Some(index) = argv.iter().position(|arg| arg == "flotilla-observation") {
                    self.roots.lock().expect("roots").push(PathBuf::from(&argv[index + 1]));
                }
            }
            // Supply hostile ambient configuration without process-global env.
            let mut outer = vec![
                "HOME=".to_string() + &self.operator.to_string_lossy(),
                "ANTHROPIC_API_KEY=fake-ambient".into(),
                "CLAUDE_CODE_OAUTH_TOKEN=fake-unrelated".into(),
                "BASH_ENV=".to_string() + &self.operator.join(".profile").to_string_lossy(),
                "BASH_FUNC_env%%=() { exit 98; }".into(),
                "XDG_CONFIG_HOME=".to_string() + &self.operator.to_string_lossy(),
                command,
            ];
            outer.extend(argv);
            ProcessCommandRunner.run_output("env", &outer.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    // The endpoint runs the real isolation script with its 10s harness deadline and 11s
    // watchdog scaled to 2s and 3s, keeping their order, so a cancelled live harness is
    // still cleaned up by the script's own deadline without ten seconds of waiting.
    fn scaled_deadlines(arg: &str) -> String {
        const SCALES: [(&str, &str); 3] = [
            ("deadline=$((SECONDS + 10))", "deadline=$((SECONDS + 2))"),
            ("(sleep 11; kill", "(sleep 3; kill"),
            ("limit=$((SECONDS + 12))", "limit=$((SECONDS + 4))"),
        ];
        if !arg.contains("deadline=$((SECONDS") {
            return arg.to_string();
        }
        SCALES.iter().fold(arg.to_string(), |arg, (from, to)| {
            assert!(arg.contains(from), "isolation script no longer contains {from}");
            arg.replace(from, to)
        })
    }

    const CLAUDE: AccountState =
        AccountState::new("claude").homes(&["CLAUDE_CONFIG_DIR"]).secrets(&["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]);
    const CODEX: AccountState = AccountState::new("codex").homes(&["CODEX_HOME"]);

    // The base layer of a target whose tools are on the test's own PATH.
    fn test_observation() -> ObservationContext {
        let accounts = AccountVariables::from_families(&[CLAUDE, CODEX]);
        let base = BaseLayer::declared([("PATH".to_string(), std::env::var("PATH").expect("test PATH"))], &accounts).expect("base");
        ObservationContext::new(base, &accounts)
    }

    async fn sentinel_case(target: usize, mode: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let operator = dir.path().join("operator");
        std::fs::create_dir(&operator).expect("operator");
        std::fs::write(operator.join(".claude.json"), "unchanged").expect("sentinel");
        std::fs::write(operator.join(".profile"), "exit 99\n").expect("profile");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).expect("project");
        std::fs::write(project.join("CLAUDE.md"), "operator project config").expect("project config");
        let harness = dir.path().join("sentinel");
        std::fs::write(
            &harness,
            r#"#!/bin/sh
set -eu
[ "${ANTHROPIC_API_KEY-unset}" = unset ]
[ "${CLAUDE_CODE_OAUTH_TOKEN-unset}" = unset ]
for variable in HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_RUNTIME_DIR CLAUDE_CONFIG_DIR CODEX_HOME TMPDIR; do
    eval 'directory=$'"$variable"
    [ -d "$directory" ]
    [ "$directory" != "$3" ]
    printf written > "$directory/sentinel"
done
[ "$PWD" != "$4" ]
[ ! -f ../CLAUDE.md ]
printf written > "$HOME/.claude.json"
printf '%s' "$HOME" > "$2"
case "$1" in success|failure)
    (sleep 1; mkdir -p "$HOME"; printf leaked > "$HOME/late-writer"; printf leaked > "${2}.leak") &
    [ "$1" = success ] && echo '2.1.300' && exit 0
    exit 7
    ;; cancel)
    (sleep 3; mkdir -p "$HOME"; printf leaked > "$HOME/late-writer"; printf leaked > "${2}.leak") &
    wait
    ;; esac
"#,
        )
        .expect("script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let endpoint =
            Arc::new(Endpoint { operator: operator.clone(), image_root: dir.path().join("image"), roots: Mutex::new(Vec::new()) });
        let runner: Arc<dyn CommandRunner> =
            if target == 1 { Arc::new(SshCommandRunner::new("fake-remote", false, endpoint.clone())) } else { endpoint.clone() };
        let provider = command_provider(if target == 2 { EnvironmentInput::Docker } else { EnvironmentInput::HostDirect }, runner);
        let marker = dir.path().join("started");
        let args = [
            mode.to_string(),
            marker.to_string_lossy().into_owned(),
            operator.to_string_lossy().into_owned(),
            project.to_string_lossy().into_owned(),
        ];
        let binary = harness.to_string_lossy().into_owned();
        let workdir = project.clone();
        let context = test_observation();
        let task = tokio::spawn(async move {
            provider
                .detection()
                .expect("detection")
                .detect(
                    (target == 2).then_some("crew:test"),
                    &context,
                    &binary,
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    &workdir,
                )
                .await
        });
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("harness started");
            task.abort();
            let _ = task.await;
        } else {
            let output = task.await.expect("join").expect("detection");
            assert_eq!(output.exit_code, Some(if mode == "success" { 0 } else { 7 }));
        }
        tokio::time::timeout(Duration::from_secs(16), async {
            loop {
                let gone = if target == 1 {
                    marker.exists() && !PathBuf::from(std::fs::read_to_string(&marker).expect("home")).exists()
                } else {
                    {
                        let roots = endpoint.roots.lock().expect("roots");
                        !roots.is_empty() && roots.iter().all(|root| !root.exists())
                    }
                };
                if gone {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("disposable state cleaned even after cancellation");
        assert_eq!(std::fs::read(operator.join(".claude.json")).expect("operator sentinel"), b"unchanged");
        assert_eq!(std::fs::read_dir(&operator).expect("operator directory").count(), 2);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!marker.with_extension("leak").exists(), "no surviving child may recreate state");
    }

    /// Processes whose command line, environment or working directory names `root`.
    #[cfg(target_os = "linux")]
    fn processes_naming(root: &Path) -> Vec<u32> {
        let needle = root.to_string_lossy().into_owned();
        let names = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).contains(&needle);
        std::fs::read_dir("/proc")
            .expect("proc")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                let proc = Path::new("/proc").join(pid.to_string());
                std::fs::read(proc.join("cmdline")).is_ok_and(names)
                    || std::fs::read(proc.join("environ")).is_ok_and(names)
                    || std::fs::read_link(proc.join("cwd")).is_ok_and(|cwd| cwd.to_string_lossy().contains(&needle))
            })
            .collect()
    }

    // #4090: however the wrapper ends, nothing it started outlives the bound and its root is gone.
    // Deadlines are scaled as for the endpoint (watchdog 3s, child limit 4s).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_probe_outlives_its_bound_however_the_wrapper_ends() {
        use std::os::unix::process::CommandExt;
        for ending in ["sigkill-wrapper", "sigkill-group", "term-during-cleanup"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("flotilla-observation-probe");
            let args = observation_args(&root.to_string_lossy(), &test_observation(), "sleep", &["30"]);
            let args = args.iter().map(|arg| scaled_deadlines(arg)).collect::<Vec<_>>();
            let mut wrapper = std::process::Command::new("env").args(&args).process_group(0).spawn().expect("wrapper");
            let pid = wrapper.id() as i32;
            while !root.join("cwd").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            match ending {
                "sigkill-wrapper" => unsafe {
                    libc::kill(pid, libc::SIGKILL);
                },
                "sigkill-group" => unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                },
                _ => {
                    // A burst of TERMs lands during cleanup, which must run to the end.
                    for _ in 0..50 {
                        unsafe {
                            libc::kill(pid, libc::SIGTERM);
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }
            }
            let _ = wrapper.wait();
            tokio::time::sleep(Duration::from_secs(6)).await;
            assert_eq!(processes_naming(&root), Vec::<u32>::new(), "{ending}: processes survive the bound");
            assert!(!root.exists(), "{ending}: the root remains");
        }
    }

    // The same sentinel contract exercises local, SSH host, and image
    // targets, including nonzero exits and cancellation during a live process.
    #[tokio::test]
    async fn detection_isolates_and_cleans_every_provider_target() {
        let cases = ["success", "failure", "cancel"].into_iter().flat_map(|mode| (0..3).map(move |target| sentinel_case(target, mode)));
        futures::future::join_all(cases).await;
    }

    /// A host with a declared rust supply whose toolchain, like rustup's
    /// proxies, is found only through `RUSTUP_HOME`.
    struct RustHost {
        dir: tempfile::TempDir,
        home: PathBuf,
    }

    impl RustHost {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().expect("tempdir");
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join(".rustup/toolchains/stable")).expect("toolchain");
            std::fs::write(home.join(".rustup/toolchains/stable/marker"), "").expect("marker");
            std::fs::create_dir_all(home.join(".cargo/bin")).expect("cargo bin");
            let rustc = home.join(".cargo/bin/rustc");
            std::fs::write(
                &rustc,
                "#!/bin/sh\n[ -f \"${RUSTUP_HOME:-$HOME/.rustup}/toolchains/stable/marker\" ] || { echo 'error: no default toolchain' >&2; exit 1; }\nenv > \"$2\"\necho 'rustc 1.99.0'\n",
            )
            .expect("rustc");
            std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).expect("executable");
            Self { dir, home }
        }

        fn preparer(&self) -> ExecutionPreparer {
            let host = HostEnvironment::new(vec![HostSupply {
                name: "rust".into(),
                path: vec!["~/.cargo/bin".into()],
                env: vec![("CARGO_HOME".into(), "~/.cargo".into()), ("RUSTUP_HOME".into(), "~/.rustup".into())],
            }])
            .expect("supplies");
            ExecutionPreparer::new(host, AccountVariables::from_families(&[CLAUDE, GH]), Some(&self.home.to_string_lossy()))
                .with_generation(vec![self.dir.path().join("generation/bin").to_string_lossy().into_owned()])
        }

        fn bound(&self) -> BoundLayer {
            let state = self.dir.path().join("state");
            BoundLayer::new(
                vec![state.join("credentials").to_string_lossy().into_owned()],
                [
                    ("GITHUB_TOKEN_FILE".to_string(), state.join("credentials/token").to_string_lossy().into_owned()),
                    ("CLAUDE_CONFIG_DIR".to_string(), state.join("agent-homes/claude").to_string_lossy().into_owned()),
                    ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "fake-delivered".to_string()),
                ],
            )
            .expect("bound layer")
        }

        fn dump(&self, name: &str) -> (PathBuf, impl Fn() -> BTreeMap<String, String>) {
            let path = self.dir.path().join(name);
            let read = path.clone();
            (path, move || {
                std::fs::read_to_string(&read)
                    .expect("environment dump")
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect()
            })
        }
    }

    const GH: AccountState = AccountState::new("gh").homes(&["GH_CONFIG_DIR"]).secrets(&["GH_TOKEN"]);

    /// How a runner launches a crew with its rendered environment.
    enum CrewLaunch {
        /// The command starts from nothing but the rendered environment.
        Clean,
        /// The vessel agent is configured with it, as a vessel's push is.
        Agent(Arc<dyn AgentClient>),
    }

    // The shared scenario every runner passes: a probe and a crew launch on
    // one prepared environment. Both get the same base layer; the probe finds
    // the supplied toolchain, sees no bound-layer variable and only throwaway
    // family homes, and leaves no scratch root; the crew sees both layers.
    async fn prepared_context_case(runner: Arc<dyn CommandRunner>, launch: CrewLaunch) {
        let host = RustHost::new();
        let preparer = host.preparer();
        let observation = preparer.observation().expect("observation");
        let crew = preparer.prepare(Some(host.bound())).expect("crew");
        assert_eq!(observation.base().environment(), crew.base().environment(), "one base layer for probe and crew");

        let provider = command_provider(EnvironmentInput::HostDirect, runner.clone());
        let (probe_dump, probe_env) = host.dump("probe-environment");
        let output = provider
            .detection()
            .expect("detection")
            .detect(None, &observation, "rustc", &["--version", &probe_dump.to_string_lossy()], host.dir.path())
            .await
            .expect("probe");
        assert!(output.success(), "probe misses the toolchain: {}", output.stderr);
        assert_eq!(output.stdout.trim(), "rustc 1.99.0");
        let probe = probe_env();
        for (name, value) in observation.base().environment() {
            assert_eq!(probe.get(&name), Some(&value), "probe {name}");
        }
        let root = Path::new(&probe["HOME"]).parent().expect("scratch root").to_path_buf();
        for (name, _) in host.bound().variables() {
            if name != "CLAUDE_CONFIG_DIR" {
                assert!(!probe.contains_key(name), "probe inherits bound {name}");
            }
        }
        for name in ["CLAUDE_CONFIG_DIR", "GH_CONFIG_DIR"] {
            assert!(Path::new(&probe[name]).starts_with(&root), "probe {name} is not throwaway: {}", probe[name]);
        }
        for name in ["CLAUDE_CODE_OAUTH_TOKEN", "GH_TOKEN", "ANTHROPIC_API_KEY"] {
            assert!(!probe.contains_key(name), "probe has account variable {name}");
        }
        assert!(!root.exists(), "scratch root {} survives the probe", root.display());

        let (crew_dump, crew_env) = host.dump("crew-environment");
        let mut env = vec![("FLOTILLA_CREW_ROLE".to_string(), "coder".to_string())];
        crew.render_crew(&mut env, observation.accounts()).expect("render");
        let script = "rustc --version \"$0\"";
        let dump = crew_dump.to_string_lossy().into_owned();
        let result = match launch {
            CrewLaunch::Clean => {
                let mut argv = vec!["-i".to_string()];
                argv.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
                argv.extend(["sh".into(), "-c".into(), script.into(), dump]);
                runner.run("env", &argv.iter().map(String::as_str).collect::<Vec<_>>(), Path::new("/"), &ChannelLabel::Default).await
            }
            CrewLaunch::Agent(client) => {
                let environment = env.iter().cloned().collect();
                client.configure(Configuration { environment }).await.expect("configure");
                runner.run("sh", &["-c", script, &dump], Path::new("/"), &ChannelLabel::Default).await
            }
        };
        assert_eq!(result.expect("crew launch").trim(), "rustc 1.99.0");
        let launched = crew_env();
        assert_eq!(launched.get("PATH"), Some(&crew.crew_path()));
        for (name, value) in crew.base().variables().chain(host.bound().variables()) {
            assert_eq!(launched.get(name).map(String::as_str), Some(value), "crew {name}");
        }
    }

    #[tokio::test]
    async fn local_runner_renders_the_prepared_context() {
        prepared_context_case(Arc::new(ProcessCommandRunner), CrewLaunch::Clean).await;
    }

    #[tokio::test]
    async fn loopback_ssh_runner_renders_the_prepared_context() {
        // The SSH connection is the stand-in: its remote end is a local shell
        // with a hostile login environment.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint =
            Arc::new(Endpoint { operator: dir.path().into(), image_root: dir.path().join("unused"), roots: Mutex::new(Vec::new()) });
        prepared_context_case(Arc::new(SshCommandRunner::new("fake-remote", false, endpoint)), CrewLaunch::Clean).await;
    }

    #[tokio::test]
    async fn vessel_agent_renders_the_prepared_context() {
        let agent = MemoryAgent::new();
        let client = agent.client();
        let directory = tempfile::tempdir().expect("tempdir");
        let runner = Arc::new(AgentRunner::new(agent.clone(), directory.path().into()));
        prepared_context_case(runner, CrewLaunch::Agent(client)).await;
        agent.stop();
    }
}

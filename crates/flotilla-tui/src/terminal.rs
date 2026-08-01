use std::{
    collections::HashSet,
    io::stdout,
    path::PathBuf,
    process::{Child, Stdio},
    sync::{LazyLock, Mutex, MutexGuard, Once},
};

use crossterm::{event::DisableMouseCapture, execute};
use flotilla_protocol::{arg, ResolvedAttachAction, ResolvedAttachPlan, SendKeyStep};

/// Restore the terminal to its original state.
///
/// Safe to call multiple times or when mouse capture was never enabled —
/// `DisableMouseCapture` and `ratatui::restore()` are both no-ops in those cases.
pub fn restore_terminal() {
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
}

fn reinitialize_terminal() -> ratatui::DefaultTerminal {
    use crossterm::event::EnableMouseCapture;

    let terminal = ratatui::init();
    if let Err(error) = execute!(stdout(), EnableMouseCapture) {
        tracing::warn!(%error, "failed to re-enable mouse capture");
    }
    terminal
}

trait AttachCommandRunner {
    fn status(&mut self, program: &str, args: &[String]) -> Result<std::process::ExitStatus, String>;

    fn start_cleanup_watchdog(&mut self, _commands: &[String]) -> Result<Option<CleanupWatchdog>, String> {
        Ok(None)
    }
}

struct SystemAttachCommandRunner;

static ACTIVE_ATTACH_BRIDGES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static ATTACH_PANIC_HOOK: Once = Once::new();
#[cfg(unix)]
static ATTACH_SIGNAL_HANDLER: Once = Once::new();

impl AttachCommandRunner for SystemAttachCommandRunner {
    fn status(&mut self, program: &str, args: &[String]) -> Result<std::process::ExitStatus, String> {
        std::process::Command::new(program).args(args).status().map_err(|error| format!("could not start {program}: {error}"))
    }

    fn start_cleanup_watchdog(&mut self, commands: &[String]) -> Result<Option<CleanupWatchdog>, String> {
        CleanupWatchdog::spawn(commands).map(Some)
    }
}

struct CleanupWatchdog {
    lease: PathBuf,
    child: Child,
}

impl CleanupWatchdog {
    fn spawn(commands: &[String]) -> Result<Self, String> {
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;

        let lease = std::env::temp_dir().join(format!("flotilla-attach-watchdog-{}", uuid::Uuid::new_v4()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lease)
            .map_err(|error| format!("could not create attach cleanup lease {}: {error}", lease.display()))?;
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "parent=$1; lease=$2; shift 2; while kill -0 \"$parent\" 2>/dev/null && [ -e \"$lease\" ]; do sleep 0.05; done; \
                 for command in \"$@\"; do sh -lc \"$command\"; done; rm -f \"$lease\"",
            )
            .arg("flotilla-attach-watchdog")
            .arg(std::process::id().to_string())
            .arg(&lease)
            .args(commands)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        unsafe {
            // The watchdog must survive the attach owner's entire terminal
            // session disappearing, not merely an individual-process signal.
            command.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
        }
        match command.spawn() {
            Ok(child) => Ok(Self { lease, child }),
            Err(error) => {
                let _ = std::fs::remove_file(&lease);
                Err(format!("could not start attach cleanup watchdog: {error}"))
            }
        }
    }
}

impl Drop for CleanupWatchdog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lease);
        let _ = self.child.wait();
    }
}

fn active_attach_bridges() -> MutexGuard<'static, HashSet<String>> {
    match ACTIVE_ATTACH_BRIDGES.lock() {
        Ok(bridges) => bridges,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn register_attach_bridge(bridge: &str) {
    active_attach_bridges().insert(bridge.to_string());
}

fn kill_attach_bridge(bridge: &str, runner: &mut dyn AttachCommandRunner) {
    if active_attach_bridges().remove(bridge) {
        let _ = runner.status("cleat", &["kill".to_string(), bridge.to_string()]);
    }
}

fn kill_all_attach_bridges() {
    let bridges: Vec<_> = active_attach_bridges().drain().collect();
    let mut runner = SystemAttachCommandRunner;
    for bridge in bridges {
        let _ = runner.status("cleat", &["kill".to_string(), bridge]);
    }
}

fn run_direct_attach(
    args: &[flotilla_protocol::arg::Arg],
    runner: &mut dyn AttachCommandRunner,
) -> Result<std::process::ExitStatus, String> {
    let command = arg::flatten(args, 0);
    runner.status("sh", &["-lc".to_string(), command])
}

fn run_send_keys_attach(
    plan: &ResolvedAttachPlan,
    bridge: &str,
    runner: &mut dyn AttachCommandRunner,
) -> Result<std::process::ExitStatus, String> {
    let plan = instantiate_attach_plan(plan);
    let cleanup_commands = std::iter::once(vec![
        flotilla_protocol::arg::Arg::Literal("cleat".into()),
        flotilla_protocol::arg::Arg::Literal("kill".into()),
        flotilla_protocol::arg::Arg::Quoted(bridge.to_string()),
    ])
    .chain(plan.0.iter().filter_map(|action| match action {
        ResolvedAttachAction::Cleanup(args) => Some(args.clone()),
        _ => None,
    }))
    .map(|args| arg::flatten(&args, 0))
    .collect::<Vec<_>>();
    let mut actions = plan.0.iter().filter(|action| !matches!(action, ResolvedAttachAction::Cleanup(_))).cloned().collect::<Vec<_>>();
    let Some(ResolvedAttachAction::Command(args)) = actions.pop() else {
        return Err("attach plan must end with an outer command".to_string());
    };
    let command = arg::flatten(&args, 0);
    register_attach_bridge(bridge);
    let _watchdog = match runner.start_cleanup_watchdog(&cleanup_commands) {
        Ok(watchdog) => watchdog,
        Err(error) => {
            kill_attach_bridge(bridge, runner);
            return Err(error);
        }
    };
    let launch =
        match runner.status("cleat", &["launch".to_string(), bridge.to_string(), "--record".to_string(), "--cmd".to_string(), command]) {
            Ok(status) => status,
            Err(error) => {
                kill_attach_bridge(bridge, runner);
                return Err(error);
            }
        };
    if !launch.success() {
        kill_attach_bridge(bridge, runner);
        return Err(format!("could not launch attach bridge for outer command (status {launch})"));
    }

    let result = (|| {
        while let Some(action) = actions.pop() {
            let ResolvedAttachAction::SendKeys { hop, mut steps } = action else {
                return Err("attach plan contains more than one executable command".to_string());
            };
            while let Some(step) = steps.pop() {
                match step {
                    SendKeyStep::WaitForReady => {
                        let status = runner.status("cleat", &[
                            "wait".to_string(),
                            bridge.to_string(),
                            "--screen-stable".to_string(),
                            "100ms".to_string(),
                            "--timeout".to_string(),
                            "30".to_string(),
                        ])?;
                        if !status.success() {
                            return Err(format!("attach hop {hop} did not become ready (cleat wait status {status})"));
                        }
                    }
                    SendKeyStep::Type { text } => {
                        let status = runner.status("cleat", &["send".to_string(), bridge.to_string(), text])?;
                        if !status.success() {
                            return Err(format!("could not send keys for attach hop {hop} (cleat send status {status})"));
                        }
                    }
                }
            }
        }
        runner.status("cleat", &["attach".to_string(), bridge.to_string()])
    })();

    kill_attach_bridge(bridge, runner);
    result
}

fn instantiate_attach_plan(plan: &ResolvedAttachPlan) -> ResolvedAttachPlan {
    let lease = uuid::Uuid::new_v4().simple().to_string();
    ResolvedAttachPlan(
        plan.0
            .iter()
            .cloned()
            .map(|action| match action {
                ResolvedAttachAction::Command(args) => ResolvedAttachAction::Command(instantiate_args(args, &lease)),
                ResolvedAttachAction::Cleanup(args) => ResolvedAttachAction::Cleanup(instantiate_args(args, &lease)),
                ResolvedAttachAction::SendKeys { hop, steps } => ResolvedAttachAction::SendKeys {
                    hop,
                    steps: steps
                        .into_iter()
                        .map(|step| match step {
                            SendKeyStep::WaitForReady => SendKeyStep::WaitForReady,
                            SendKeyStep::Type { text } => {
                                SendKeyStep::Type { text: text.replace(flotilla_protocol::ATTACH_LEASE_PLACEHOLDER, &lease) }
                            }
                        })
                        .collect(),
                },
            })
            .collect(),
    )
}

fn instantiate_args(args: Vec<flotilla_protocol::arg::Arg>, lease: &str) -> Vec<flotilla_protocol::arg::Arg> {
    args.into_iter()
        .map(|arg| match arg {
            flotilla_protocol::arg::Arg::Literal(value) => {
                flotilla_protocol::arg::Arg::Literal(value.replace(flotilla_protocol::ATTACH_LEASE_PLACEHOLDER, lease))
            }
            flotilla_protocol::arg::Arg::Quoted(value) => {
                flotilla_protocol::arg::Arg::Quoted(value.replace(flotilla_protocol::ATTACH_LEASE_PLACEHOLDER, lease))
            }
            flotilla_protocol::arg::Arg::NestedCommand(args) => flotilla_protocol::arg::Arg::NestedCommand(instantiate_args(args, lease)),
        })
        .collect()
}

/// Execute an interactive attach plan and return the attached process status.
pub fn run_attach_plan(plan: &ResolvedAttachPlan) -> Result<std::process::ExitStatus, String> {
    // Standalone CLI and convoy auto-attach paths do not pass through TUI
    // startup, so install the idempotent bridge cleanup hooks here too.
    install_panic_hook();
    #[cfg(unix)]
    install_sigterm_handler();

    let mut runner = SystemAttachCommandRunner;
    match plan.0.as_slice() {
        [ResolvedAttachAction::Command(args)] => run_direct_attach(args, &mut runner),
        _ => {
            let bridge = format!("flotilla-attach-{}", uuid::Uuid::new_v4());
            run_send_keys_attach(plan, &bridge, &mut runner)
        }
    }
}

/// Temporarily leave the TUI to inspect a terminal session, then restore it.
///
/// This deliberately does not stamp Presentation Manager metadata: the pane
/// remains owned by its existing project/archipelago context while the attach
/// is only a transient foreground excursion.
pub fn run_temporary_attach(plan: &ResolvedAttachPlan) -> (ratatui::DefaultTerminal, Result<(), String>) {
    restore_terminal();
    let result = run_attach_plan(plan).and_then(|status| {
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "attach command exited with status {}",
                status.code().map(|code| code.to_string()).unwrap_or_else(|| "signal".to_string())
            ))
        }
    });
    (reinitialize_terminal(), result)
}

/// Install a panic hook that restores the terminal before printing the panic.
///
/// Safe to call before terminal initialization and more than once. Wraps
/// whatever hook is currently installed (including color_eyre's) so error
/// reporting still works.
pub fn install_panic_hook() {
    ATTACH_PANIC_HOOK.call_once(|| {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            kill_all_attach_bridges();
            hook(info);
        }));
    });
}

/// Spawn a background task that listens for SIGINT or SIGTERM and cleanly exits.
///
/// Must be called within a tokio runtime. Safe before terminal initialization
/// and safe to call more than once. Covers the entire process lifetime,
/// including the startup window before the event loop begins.
#[cfg(unix)]
pub fn install_sigterm_handler() {
    ATTACH_SIGNAL_HANDLER.call_once(|| {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        tokio::spawn(async move {
            let exit_code = tokio::select! {
                _ = sigint.recv() => 130,
                _ = sigterm.recv() => 0,
            };
            restore_terminal();
            kill_all_attach_bridges();
            std::process::exit(exit_code);
        });
    });
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::VecDeque,
        os::unix::process::ExitStatusExt,
        path::Path,
        process::ExitStatus,
        time::{Duration, Instant},
    };

    use flotilla_protocol::{
        arg::{self, Arg},
        ResolvedAttachAction, ResolvedAttachPlan, SendKeyStep,
    };

    use super::{instantiate_attach_plan, run_send_keys_attach, AttachCommandRunner};

    #[derive(Default)]
    struct FakeRunner {
        statuses: VecDeque<i32>,
        calls: Vec<(String, Vec<String>)>,
    }

    impl FakeRunner {
        fn with_statuses(statuses: impl IntoIterator<Item = i32>) -> Self {
            Self { statuses: statuses.into_iter().collect(), calls: Vec::new() }
        }
    }

    impl AttachCommandRunner for FakeRunner {
        fn status(&mut self, program: &str, args: &[String]) -> Result<ExitStatus, String> {
            self.calls.push((program.to_string(), args.to_vec()));
            let code = self.statuses.pop_front().unwrap_or(0);
            Ok(ExitStatus::from_raw(code << 8))
        }
    }

    struct DockerProbeRunner {
        marker: String,
        inner: super::SystemAttachCommandRunner,
    }

    struct SigkillProbeRunner {
        ready: String,
        inner: super::SystemAttachCommandRunner,
    }

    struct DockerLivenessCleanup {
        bridge: String,
        container: String,
    }

    impl Drop for DockerLivenessCleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("cleat").args(["kill", &self.bridge]).status();
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.container]).status();
        }
    }

    impl AttachCommandRunner for SigkillProbeRunner {
        fn status(&mut self, program: &str, args: &[String]) -> Result<ExitStatus, String> {
            if program == "cleat" && args.first().is_some_and(|command| command == "attach") {
                std::fs::write(&self.ready, b"ready").map_err(|error| format!("write SIGKILL probe marker: {error}"))?;
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            self.inner.status(program, args)
        }

        fn start_cleanup_watchdog(&mut self, commands: &[String]) -> Result<Option<super::CleanupWatchdog>, String> {
            self.inner.start_cleanup_watchdog(commands)
        }
    }

    impl AttachCommandRunner for DockerProbeRunner {
        fn status(&mut self, program: &str, args: &[String]) -> Result<ExitStatus, String> {
            if program == "cleat" && args.first().is_some_and(|command| command == "attach") {
                return self.inner.status("cleat", &[
                    "expect".to_string(),
                    args[1].clone(),
                    "--since-marker".to_string(),
                    "before-inner-attach".to_string(),
                    "--text".to_string(),
                    self.marker.clone(),
                    "--timeout".to_string(),
                    "10".to_string(),
                ]);
            }
            let status = self.inner.status(program, args)?;
            if status.success() && program == "cleat" && args.first().is_some_and(|command| command == "wait") {
                return self.inner.status("cleat", &["mark".to_string(), args[1].clone(), "before-inner-attach".to_string()]);
            }
            Ok(status)
        }
    }

    fn two_hop_plan() -> ResolvedAttachPlan {
        ResolvedAttachPlan(vec![
            ResolvedAttachAction::SendKeys {
                hop: "docker environment 'crew-box'".into(),
                steps: vec![SendKeyStep::Type { text: "exec flotilla attach 'crew session'".into() }, SendKeyStep::WaitForReady],
            },
            ResolvedAttachAction::Command(vec![
                Arg::Literal("docker".into()),
                Arg::Literal("exec".into()),
                Arg::Literal("-it".into()),
                Arg::Quoted("crew-box".into()),
                Arg::Literal("/bin/sh".into()),
            ]),
        ])
    }

    #[test]
    fn shell_death_while_waiting_names_the_hop() {
        let mut runner = FakeRunner::with_statuses([0, 2, 0]);

        let error = run_send_keys_attach(&two_hop_plan(), "dead-bridge", &mut runner).expect_err("dead shell should fail");

        assert!(error.contains("docker environment 'crew-box'"), "{error}");
        assert!(error.contains("did not become ready"), "{error}");
        assert_eq!(runner.calls.last().expect("cleanup call").1[0], "kill");
    }

    #[test]
    fn readiness_uses_screen_stability_before_sending_echoed_text() {
        let mut runner = FakeRunner::default();

        run_send_keys_attach(&two_hop_plan(), "ready-bridge", &mut runner).expect("attach plan should run");

        let commands: Vec<_> = runner.calls.iter().map(|(_, args)| args[0].as_str()).collect();
        assert_eq!(commands, ["launch", "wait", "send", "attach", "kill"]);
        let wait = &runner.calls[1].1;
        assert!(wait.windows(2).any(|pair| pair == ["--screen-stable", "100ms"]));
        assert!(!runner.calls.iter().any(|(_, args)| args.iter().any(|arg| arg == "expect")));
    }

    #[test]
    fn attach_plan_instantiation_shares_one_fresh_lease() {
        let placeholder = flotilla_protocol::ATTACH_LEASE_PLACEHOLDER;
        let plan = ResolvedAttachPlan(vec![
            ResolvedAttachAction::SendKeys { hop: "Docker".into(), steps: vec![SendKeyStep::Type { text: format!("echo {placeholder}") }] },
            ResolvedAttachAction::Cleanup(vec![Arg::Quoted(format!("kill {placeholder}"))]),
            ResolvedAttachAction::Command(vec![Arg::NestedCommand(vec![Arg::Quoted(format!("run {placeholder}"))])]),
        ]);

        let instantiated = instantiate_attach_plan(&plan);
        let rendered = format!("{instantiated:?}");
        assert!(!rendered.contains(placeholder));
        let lease = rendered.split("echo ").nth(1).and_then(|rest| rest.split('"').next()).expect("instantiated lease");
        assert_eq!(rendered.matches(lease).count(), 3, "all lifecycle actions should share the lease: {rendered}");
        assert!(format!("{plan:?}").contains(placeholder), "instantiation must not mutate the deterministic resolved plan");
    }

    #[test]
    fn attach_sigkill_helper() {
        let Ok(bridge) = std::env::var("FLOTILLA_ATTACH_SIGKILL_BRIDGE") else { return };
        let ready = std::env::var("FLOTILLA_ATTACH_SIGKILL_READY").expect("SIGKILL helper ready path");
        let cleaned = std::env::var("FLOTILLA_ATTACH_SIGKILL_CLEANED").expect("SIGKILL helper cleanup path");
        let plan = if let Ok(container) = std::env::var("FLOTILLA_ATTACH_SIGKILL_CONTAINER") {
            let pid_file = std::env::var("FLOTILLA_ATTACH_SIGKILL_PID_FILE").expect("SIGKILL helper PID file");
            let seat = std::env::var("FLOTILLA_ATTACH_SIGKILL_SEAT").expect("SIGKILL helper seat path");
            ResolvedAttachPlan(vec![
                ResolvedAttachAction::SendKeys {
                    hop: "Docker SIGKILL probe".into(),
                    steps: vec![
                        SendKeyStep::Type {
                            text: arg::flatten(
                                &[
                                    Arg::Literal("exec".into()),
                                    Arg::Literal("sh".into()),
                                    Arg::Literal("-c".into()),
                                    Arg::Quoted(format!("echo $$ > {pid_file}; exec flock -n {seat} sleep 300")),
                                ],
                                0,
                            ),
                        },
                        SendKeyStep::WaitForReady,
                    ],
                },
                ResolvedAttachAction::Cleanup(vec![
                    Arg::Literal("docker".into()),
                    Arg::Literal("exec".into()),
                    Arg::Quoted(container.clone()),
                    Arg::Literal("sh".into()),
                    Arg::Literal("-c".into()),
                    Arg::Quoted(format!(
                        "pid=$(cat {pid_file} 2>/dev/null) || exit 0; kill -KILL \"$pid\" 2>/dev/null || true; rm -f {pid_file}"
                    )),
                ]),
                ResolvedAttachAction::Cleanup(vec![Arg::Literal("touch".into()), Arg::Quoted(cleaned)]),
                ResolvedAttachAction::Command(vec![
                    Arg::Literal("docker".into()),
                    Arg::Literal("exec".into()),
                    Arg::Literal("-it".into()),
                    Arg::Quoted(container),
                    Arg::Literal("/bin/sh".into()),
                ]),
            ])
        } else {
            ResolvedAttachPlan(vec![
                ResolvedAttachAction::SendKeys {
                    hop: "SIGKILL probe".into(),
                    steps: vec![SendKeyStep::Type { text: "exec sleep 300".into() }, SendKeyStep::WaitForReady],
                },
                ResolvedAttachAction::Cleanup(vec![Arg::Literal("touch".into()), Arg::Quoted(cleaned)]),
                ResolvedAttachAction::Command(vec![Arg::Literal("sh".into())]),
            ])
        };
        let mut runner = SigkillProbeRunner { ready, inner: super::SystemAttachCommandRunner };

        run_send_keys_attach(&plan, &bridge, &mut runner).expect("SIGKILL helper attach");
    }

    #[test]
    #[ignore = "requires the cleat binary"]
    fn killing_attach_owner_reaps_the_bridge() {
        let temp = tempfile::tempdir().expect("SIGKILL probe tempdir");
        let ready = temp.path().join("ready");
        let cleaned = temp.path().join("cleaned");
        let bridge = format!("flotilla-attach-sigkill-{}", uuid::Uuid::new_v4());
        let mut helper = std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args(["attach_sigkill_helper", "--nocapture"])
            .env("FLOTILLA_ATTACH_SIGKILL_BRIDGE", &bridge)
            .env("FLOTILLA_ATTACH_SIGKILL_READY", &ready)
            .env("FLOTILLA_ATTACH_SIGKILL_CLEANED", &cleaned)
            .spawn()
            .expect("spawn SIGKILL attach helper");

        wait_for_path(&ready, Duration::from_secs(10));
        unsafe {
            libc::kill(helper.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = helper.wait();

        wait_for_path(&cleaned, Duration::from_secs(3));
        let deadline = Instant::now() + Duration::from_secs(3);
        let reaped = loop {
            let output = std::process::Command::new("cleat").args(["list", "--json"]).output().expect("list cleat sessions");
            if !String::from_utf8_lossy(&output.stdout).contains(&bridge) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = std::process::Command::new("cleat").args(["kill", &bridge]).status();
        assert!(reaped, "attach bridge {bridge} survived its SIGKILLed owner");
    }

    #[test]
    #[ignore = "requires Docker, cleat, and the debian:trixie-slim image"]
    fn killing_attach_owner_reaps_docker_exec_and_frees_the_interior_seat() {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let container = format!("flotilla-attach-liveness-{suffix}");
        let bridge = format!("flotilla-attach-liveness-{suffix}");
        let pid_file = format!("/tmp/flotilla-attach-{suffix}.pid");
        let seat = format!("/tmp/flotilla-attach-{suffix}.seat");
        let temp = tempfile::tempdir().expect("Docker liveness tempdir");
        let ready = temp.path().join("ready");
        let cleaned = temp.path().join("cleaned");

        let started = std::process::Command::new("docker")
            .args(["run", "-d", "--name", &container, "debian:trixie-slim", "sleep", "infinity"])
            .status()
            .expect("start Docker liveness container");
        assert!(started.success(), "Docker liveness container should start");
        let _cleanup = DockerLivenessCleanup { bridge: bridge.clone(), container: container.clone() };

        let mut helper = std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args(["attach_sigkill_helper", "--nocapture"])
            .env("FLOTILLA_ATTACH_SIGKILL_BRIDGE", &bridge)
            .env("FLOTILLA_ATTACH_SIGKILL_READY", &ready)
            .env("FLOTILLA_ATTACH_SIGKILL_CLEANED", &cleaned)
            .env("FLOTILLA_ATTACH_SIGKILL_CONTAINER", &container)
            .env("FLOTILLA_ATTACH_SIGKILL_PID_FILE", &pid_file)
            .env("FLOTILLA_ATTACH_SIGKILL_SEAT", &seat)
            .spawn()
            .expect("spawn Docker attach helper");
        wait_for_path(&ready, Duration::from_secs(10));

        let pid = wait_for_docker_pid(&container, &pid_file, Duration::from_secs(5));
        unsafe {
            libc::kill(helper.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = helper.wait();
        wait_for_path(&cleaned, Duration::from_secs(3));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let bridge_gone = !cleat_sessions().contains(&bridge);
            let interior_gone = !docker_process_exists(&container, &pid);
            let exec_gone = !host_processes().contains(&format!("docker exec -it {container} /bin/sh"));
            if bridge_gone && interior_gone && exec_gone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "attach chain survived owner death: bridge={bridge_gone}, interior={interior_gone}, exec={exec_gone}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let second = std::process::Command::new("docker")
            .args(["exec", &container, "flock", "-n", &seat, "true"])
            .status()
            .expect("attempt second interior attach");
        assert!(second.success(), "the second attach should acquire the freed controller seat");
    }

    fn wait_for_docker_pid(container: &str, pid_file: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let output =
                std::process::Command::new("docker").args(["exec", container, "cat", pid_file]).output().expect("read interior PID");
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
            assert!(Instant::now() < deadline, "timed out waiting for interior attach PID");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn docker_process_exists(container: &str, pid: &str) -> bool {
        std::process::Command::new("docker")
            .args(["exec", container, "test", "-e", &format!("/proc/{pid}")])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn cleat_sessions() -> String {
        let output = std::process::Command::new("cleat").args(["list", "--json"]).output().expect("list cleat sessions");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn host_processes() -> String {
        let output = std::process::Command::new("ps").args(["-eo", "args="]).output().expect("list host processes");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn wait_for_path(path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !path.exists() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    #[ignore = "requires Docker, cleat, and the debian:trixie-slim image"]
    fn real_docker_hop_types_the_inner_attach_into_the_container_shell() {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let container = format!("flotilla-attach-test-{suffix}");
        let bridge = format!("flotilla-attach-test-{suffix}");
        let marker = format!("CREW_READY_{suffix}");
        let split = marker.len() / 2;
        let typed = format!("exec sh -c \"printf '{}''{}'; exit\"", &marker[..split], &marker[split..]);
        let plan = ResolvedAttachPlan(vec![
            ResolvedAttachAction::SendKeys {
                hop: format!("docker container '{container}'"),
                steps: vec![SendKeyStep::Type { text: typed }, SendKeyStep::WaitForReady],
            },
            ResolvedAttachAction::Command(vec![
                Arg::Literal("docker".into()),
                Arg::Literal("exec".into()),
                Arg::Literal("-it".into()),
                Arg::Quoted(container.clone()),
                Arg::Literal("/bin/sh".into()),
            ]),
        ]);

        let started = std::process::Command::new("docker")
            .args(["run", "-d", "--name", &container, "debian:trixie-slim", "sleep", "infinity"])
            .status()
            .expect("start docker container");
        assert!(started.success(), "docker container should start");

        let result = (|| {
            let mut runner = DockerProbeRunner { marker, inner: super::SystemAttachCommandRunner };
            let status = run_send_keys_attach(&plan, &bridge, &mut runner)?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("container crew marker was not observed (status {status})"))
            }
        })();

        let _ = std::process::Command::new("cleat").args(["kill", &bridge]).status();
        let _ = std::process::Command::new("docker").args(["rm", "-f", &container]).status();
        result.expect("send-keys should land in the container shell");
    }
}

/// Suspend the process (Ctrl-Z / SIGTSTP).
///
/// Restores the terminal to its original state, delivers SIGTSTP to the
/// process group (which suspends execution here), then re-initialises the
/// terminal when the process is resumed (SIGCONT).
///
/// Returns the new [`ratatui::DefaultTerminal`] — callers must replace
/// their existing terminal binding with this value.
#[cfg(unix)]
pub fn suspend_and_resume() -> ratatui::DefaultTerminal {
    restore_terminal();
    // SAFETY: kill(0, SIGTSTP) sends the signal to the entire process group.
    // The process suspends at this point and resumes on SIGCONT.
    let rc = unsafe { libc::kill(0, libc::SIGTSTP) };
    if rc == -1 {
        tracing::warn!(err = %std::io::Error::last_os_error(), "SIGTSTP delivery failed");
    }
    // Resumed — re-initialise terminal
    reinitialize_terminal()
}

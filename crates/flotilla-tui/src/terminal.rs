use std::{
    collections::{HashSet, VecDeque},
    io::{stderr, stdout, Read, Write},
    process::{ExitStatus, Stdio},
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
    fn run(&mut self, program: &str, args: &[String]) -> Result<AttachCommandOutput, String>;
}

#[derive(Debug)]
struct AttachCommandOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

struct SystemAttachCommandRunner;

const ATTACH_STDERR_TAIL_LIMIT: usize = 64 * 1024;

static ACTIVE_ATTACH_BRIDGES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static ATTACH_PANIC_HOOK: Once = Once::new();
#[cfg(unix)]
static ATTACH_SIGNAL_HANDLER: Once = Once::new();

impl AttachCommandRunner for SystemAttachCommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<AttachCommandOutput, String> {
        let mut child = std::process::Command::new(program)
            .args(args)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start {program}: {error}"))?;
        let mut child_stderr = child.stderr.take().expect("piped child stderr should be available");
        let stderr_reader = std::thread::spawn(move || tee_stderr_tail(&mut child_stderr, stderr(), ATTACH_STDERR_TAIL_LIMIT));
        let status = child.wait().map_err(|error| format!("could not wait for {program}: {error}"))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| format!("stderr reader for {program} panicked"))?
            .map_err(|error| format!("could not read stderr from {program}: {error}"))?;
        Ok(AttachCommandOutput { status, stderr })
    }
}

fn tee_stderr_tail(mut reader: impl Read, mut live: impl Write, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut tail = VecDeque::with_capacity(limit);
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        live.write_all(&chunk[..read])?;
        live.flush()?;
        for byte in &chunk[..read] {
            if tail.len() == limit {
                tail.pop_front();
            }
            if limit > 0 {
                tail.push_back(*byte);
            }
        }
    }
    Ok(tail.into_iter().collect())
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
        let _ = runner.run("cleat", &["kill".to_string(), bridge.to_string()]);
    }
}

fn kill_all_attach_bridges() {
    let bridges: Vec<_> = active_attach_bridges().drain().collect();
    let mut runner = SystemAttachCommandRunner;
    for bridge in bridges {
        let _ = runner.run("cleat", &["kill".to_string(), bridge]);
    }
}

fn run_direct_attach(args: &[flotilla_protocol::arg::Arg], runner: &mut dyn AttachCommandRunner) -> Result<AttachCommandOutput, String> {
    let command = arg::flatten(args, 0);
    runner.run("sh", &["-lc".to_string(), command])
}

fn run_send_keys_attach(
    plan: &ResolvedAttachPlan,
    bridge: &str,
    runner: &mut dyn AttachCommandRunner,
) -> Result<AttachCommandOutput, String> {
    let mut actions = plan.0.clone();
    let Some(ResolvedAttachAction::Command(args)) = actions.pop() else {
        return Err("attach plan must end with an outer command".to_string());
    };
    let command = arg::flatten(&args, 0);
    register_attach_bridge(bridge);
    let launch =
        match runner.run("cleat", &["launch".to_string(), bridge.to_string(), "--record".to_string(), "--cmd".to_string(), command]) {
            Ok(status) => status,
            Err(error) => {
                kill_attach_bridge(bridge, runner);
                return Err(error);
            }
        };
    if !launch.status.success() {
        kill_attach_bridge(bridge, runner);
        return Err(format!("could not launch attach bridge for outer command (status {})", launch.status));
    }

    let result = (|| {
        while let Some(action) = actions.pop() {
            let ResolvedAttachAction::SendKeys { hop, mut steps } = action else {
                return Err("attach plan contains more than one executable command".to_string());
            };
            while let Some(step) = steps.pop() {
                match step {
                    SendKeyStep::WaitForReady => {
                        let status = runner.run("cleat", &[
                            "wait".to_string(),
                            bridge.to_string(),
                            "--screen-stable".to_string(),
                            "100ms".to_string(),
                            "--timeout".to_string(),
                            "30".to_string(),
                        ])?;
                        if !status.status.success() {
                            return Err(format!("attach hop {hop} did not become ready (cleat wait status {})", status.status));
                        }
                    }
                    SendKeyStep::Type { text } => {
                        let status = runner.run("cleat", &["send".to_string(), bridge.to_string(), text])?;
                        if !status.status.success() {
                            return Err(format!("could not send keys for attach hop {hop} (cleat send status {})", status.status));
                        }
                    }
                }
            }
        }
        runner.run("cleat", &["attach".to_string(), bridge.to_string()])
    })();

    kill_attach_bridge(bridge, runner);
    result
}

fn execute_attach_plan(plan: &ResolvedAttachPlan, runner: &mut dyn AttachCommandRunner) -> Result<AttachCommandOutput, String> {
    match plan.0.as_slice() {
        [ResolvedAttachAction::Command(args)] => run_direct_attach(args, runner),
        _ => {
            let bridge = format!("flotilla-attach-{}", uuid::Uuid::new_v4());
            run_send_keys_attach(plan, &bridge, runner)
        }
    }
}

fn replay_attach_stderr(captured: &[u8], mut writer: impl Write) -> Result<(), String> {
    writer.write_all(captured).map_err(|error| format!("could not replay attach error: {error}"))?;
    writer.flush().map_err(|error| format!("could not replay attach error: {error}"))
}

/// Execute an interactive attach plan and return the attached process status.
/// Child stderr remains live during the attach. On failure, its bounded tail is
/// replayed after the primary screen has been restored.
pub fn run_attach_plan(plan: &ResolvedAttachPlan) -> Result<ExitStatus, String> {
    // Standalone CLI and convoy auto-attach paths do not pass through TUI
    // startup, so install the idempotent bridge cleanup hooks here too.
    install_panic_hook();
    #[cfg(unix)]
    install_sigterm_handler();

    let output = execute_attach_plan(plan, &mut SystemAttachCommandRunner)?;
    restore_terminal();
    if !output.status.success() {
        replay_attach_stderr(&output.stderr, stderr())?;
    }
    Ok(output.status)
}

/// Temporarily leave the TUI to inspect a terminal session, then restore it.
///
/// This deliberately does not stamp Presentation Manager metadata: the pane
/// remains owned by its existing project/archipelago context while the attach
/// is only a transient foreground excursion.
pub fn run_temporary_attach(plan: &ResolvedAttachPlan) -> (ratatui::DefaultTerminal, Result<(), String>) {
    restore_terminal();
    let result = execute_attach_plan(plan, &mut SystemAttachCommandRunner).and_then(|output| {
        if output.status.success() {
            Ok(())
        } else {
            let mut message = format!(
                "attach command exited with status {}",
                output.status.code().map(|code| code.to_string()).unwrap_or_else(|| "signal".to_string())
            );
            let captured = String::from_utf8_lossy(&output.stderr);
            if !captured.trim().is_empty() {
                message.push_str(": ");
                message.push_str(captured.trim());
            }
            Err(message)
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
    use std::{collections::VecDeque, os::unix::process::ExitStatusExt, process::ExitStatus};

    use flotilla_protocol::{arg::Arg, ResolvedAttachAction, ResolvedAttachPlan, SendKeyStep};

    use super::{replay_attach_stderr, run_send_keys_attach, tee_stderr_tail, AttachCommandOutput, AttachCommandRunner};

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
        fn run(&mut self, program: &str, args: &[String]) -> Result<AttachCommandOutput, String> {
            self.calls.push((program.to_string(), args.to_vec()));
            let code = self.statuses.pop_front().unwrap_or(0);
            Ok(AttachCommandOutput { status: ExitStatus::from_raw(code << 8), stderr: Vec::new() })
        }
    }

    struct DockerProbeRunner {
        marker: String,
        inner: super::SystemAttachCommandRunner,
    }

    impl AttachCommandRunner for DockerProbeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> Result<AttachCommandOutput, String> {
            if program == "cleat" && args.first().is_some_and(|command| command == "attach") {
                return self.inner.run("cleat", &[
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
            let output = self.inner.run(program, args)?;
            if output.status.success() && program == "cleat" && args.first().is_some_and(|command| command == "wait") {
                return self.inner.run("cleat", &["mark".to_string(), args[1].clone(), "before-inner-attach".to_string()]);
            }
            Ok(output)
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
    fn attach_failure_replay_preserves_the_original_error() {
        let mut replayed = Vec::new();

        replay_attach_stderr(b"session held by host feta pid 4242: already has a foreground client\n", &mut replayed)
            .expect("refusal should replay");

        let replayed = String::from_utf8(replayed).expect("replayed stderr should be UTF-8");
        assert_eq!(replayed, "session held by host feta pid 4242: already has a foreground client\n");
    }

    #[test]
    fn attach_stderr_is_live_and_retains_only_a_bounded_tail() {
        let mut live = Vec::new();

        let tail = tee_stderr_tail(&b"warning then refusal"[..], &mut live, 7).expect("stderr should be teed");

        assert_eq!(live, b"warning then refusal");
        assert_eq!(tail, b"refusal");
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
            let output = run_send_keys_attach(&plan, &bridge, &mut runner)?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!("container crew marker was not observed (status {})", output.status))
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

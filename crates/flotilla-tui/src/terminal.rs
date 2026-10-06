use std::{convert::Infallible, io::stdout, process::Command, sync::Once};

use crossterm::{event::DisableMouseCapture, execute};
use flotilla_protocol::{arg, arg::Arg, ResolvedAttachAction, ResolvedAttachPlan};

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

static ATTACH_PANIC_HOOK: Once = Once::new();
#[cfg(unix)]
static ATTACH_SIGNAL_HANDLER: Once = Once::new();

fn attach_argv(plan: &ResolvedAttachPlan) -> Result<(String, Vec<String>), String> {
    let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else {
        return Err("attach resolution must produce exactly one command".to_string());
    };
    let mut argv = args
        .iter()
        .map(|value| {
            Ok(match value {
                Arg::Literal(value) | Arg::Quoted(value) => value.clone(),
                Arg::NestedCommand(inner) => arg::flatten(inner, 1),
                Arg::EnvAssignment { key, value } => {
                    arg::validate_env_key(key)?;
                    format!("{key}={value}")
                }
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if argv.is_empty() {
        return Err("attach resolution produced an empty command".to_string());
    }
    let program = argv.remove(0);
    Ok((program, argv))
}

/// Resolve the viewer's first hop, rather than executing a daemon-local plan.
/// The destination comes from the viewer's hosts.toml; native OpenSSH inherits
/// the console handles and carries terminal resize through its allocated PTY.
pub fn remote_attach_plan(
    hosts: &flotilla_core::config::HostsConfig,
    host: &flotilla_protocol::HostName,
    reference: &str,
    mode: flotilla_protocol::commands::AttachMode,
) -> Result<ResolvedAttachPlan, String> {
    use flotilla_core::config::ssh_destination;
    use flotilla_protocol::commands::AttachMode;

    let mut routes = hosts.hosts.values().filter(|route| route.expected_host_name == host.as_str());
    let route = routes.next().ok_or_else(|| format!("host {host} has no configured SSH route from this viewer"))?;
    if routes.next().is_some() {
        return Err(format!("host {host} has ambiguous SSH routes from this viewer"));
    }
    let mut command = vec![
        Arg::Literal("flotilla".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("--transient".into()),
        Arg::Literal("--host".into()),
        Arg::Quoted(host.to_string()),
    ];
    match mode {
        AttachMode::Default => command.push(Arg::Literal("--watch".into())),
        AttachMode::PreferTake => {}
        AttachMode::Strict => command.push(Arg::Literal("--strict".into())),
        AttachMode::Take => command.push(Arg::Literal("--take".into())),
    }
    command.push(Arg::Literal("--".into()));
    command.push(Arg::Quoted(reference.into()));
    Ok(ResolvedAttachPlan::command(vec![
        Arg::Literal("ssh".into()),
        Arg::Literal("-t".into()),
        Arg::Literal("-o".into()),
        Arg::Literal("BatchMode=yes".into()),
        Arg::Literal("--".into()),
        Arg::Quoted(ssh_destination(&route.hostname, route.user.as_deref())),
        Arg::NestedCommand(vec![
            Arg::Literal("${SHELL:-/bin/sh}".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(command),
        ]),
    ]))
}

/// Scope terminal state to the child lifetime, including spawn and wait errors.
#[cfg(any(windows, test))]
fn with_raw_terminal<T, S>(
    enter: impl FnOnce() -> Result<S, String>,
    restore: impl FnOnce(S),
    run: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    struct Restore<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Restore<F> {
        fn drop(&mut self) {
            if let Some(restore) = self.0.take() {
                restore();
            }
        }
    }
    let state = enter()?;
    let _restore = Restore(Some(|| restore(state)));
    run()
}

// Windows ENABLE_PROCESSED_INPUT, ENABLE_LINE_INPUT and ENABLE_ECHO_INPUT
// occupy bits 0..2. Preserve window/VT input and every other console flag.
#[cfg(any(windows, test))]
fn raw_input_mode(mode: u32) -> u32 {
    #[cfg(windows)]
    let cooked_input = windows_console::COOKED_INPUT;
    // Portable tests exercise the documented Windows flag layout without
    // linking console APIs on Unix.
    #[cfg(all(test, not(windows)))]
    let cooked_input = 0x0007;
    mode & !cooked_input
}

#[cfg(windows)]
mod windows_console {
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
    };

    pub(super) const COOKED_INPUT: u32 = ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;

    pub(super) struct Modes {
        input: HANDLE,
        output: HANDLE,
        input_mode: u32,
        output_mode: u32,
    }

    pub(super) fn enter() -> Result<Modes, String> {
        // Capture the cooked console after leaving any TUI mouse/raw modes.
        super::restore_terminal();
        // SAFETY: standard handles are borrowed for the child lifetime; mode
        // pointers refer to live u32s. No handle is closed by this module.
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            if input.is_null() || output.is_null() || input == INVALID_HANDLE_VALUE || output == INVALID_HANDLE_VALUE {
                return Err("attach requires Windows console input and output; missing or invalid standard handle".into());
            }
            let mut input_mode = 0;
            let mut output_mode = 0;
            if GetConsoleMode(input, &mut input_mode) == 0 || GetConsoleMode(output, &mut output_mode) == 0 {
                return Err(format!(
                    "attach requires Windows console input and output (redirected handles are unsupported): {}",
                    std::io::Error::last_os_error()
                ));
            }
            let modes = Modes { input, output, input_mode, output_mode };
            let raw = super::raw_input_mode(input_mode);
            if SetConsoleMode(input, raw) == 0 {
                let error = std::io::Error::last_os_error();
                restore(modes);
                return Err(format!("could not enter raw terminal mode: {error}"));
            }
            Ok(modes)
        }
    }

    pub(super) fn restore(modes: Modes) {
        // SAFETY: these are the borrowed standard handles captured by enter.
        unsafe {
            if SetConsoleMode(modes.input, modes.input_mode) == 0 {
                tracing::warn!(error = %std::io::Error::last_os_error(), "failed to restore console input mode");
            }
            if SetConsoleMode(modes.output, modes.output_mode) == 0 {
                tracing::warn!(error = %std::io::Error::last_os_error(), "failed to restore console output mode");
            }
        }
    }
}

/// Replace this process with the single resolved attach command. The real TTY
/// chain then carries bytes, resize signals, stderr, and exit status natively.
#[cfg(unix)]
pub fn exec_attach_plan(plan: &ResolvedAttachPlan) -> Result<Infallible, String> {
    use std::os::unix::process::CommandExt;

    install_panic_hook();
    install_sigterm_handler();
    restore_terminal();
    let (program, args) = attach_argv(plan)?;
    let error = Command::new(&program).args(args).exec();
    Err(format!("could not exec {program} attach hop: {error}"))
}

/// Run the single resolved attach command on platforms without process
/// replacement, then terminate with the command's exit status.
#[cfg(windows)]
pub fn exec_attach_plan(plan: &ResolvedAttachPlan) -> Result<Infallible, String> {
    install_panic_hook();
    let (program, args) = attach_argv(plan)?;
    let status = with_raw_terminal(windows_console::enter, windows_console::restore, || {
        Command::new(&program).args(args).status().map_err(|error| format!("could not start {program} attach hop: {error}"))
    })?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Preserve process-spawn attachment on platforms without Unix exec or a
/// Windows console adapter.
#[cfg(not(any(unix, windows)))]
pub fn exec_attach_plan(plan: &ResolvedAttachPlan) -> Result<Infallible, String> {
    install_panic_hook();
    restore_terminal();
    let (program, args) = attach_argv(plan)?;
    let status = Command::new(&program).args(args).status().map_err(|error| format!("could not start {program} attach hop: {error}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Temporarily leave the TUI to inspect a terminal session, then restore it.
///
/// This deliberately does not stamp Presentation Manager metadata: the pane
/// remains owned by its existing project/archipelago context while the attach
/// is only a transient foreground excursion.
pub fn run_temporary_attach(plan: &ResolvedAttachPlan) -> (ratatui::DefaultTerminal, Result<(), String>) {
    restore_terminal();
    let result = attach_argv(plan).and_then(|(program, args)| {
        let status =
            Command::new(&program).args(args).status().map_err(|error| format!("could not start {program} attach hop: {error}"))?;
        status.success().then_some(()).ok_or_else(|| format!("{program} attach hop exited with status {status}"))
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
            std::process::exit(exit_code);
        });
    });
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

#[cfg(test)]
mod tests {
    use flotilla_protocol::{arg::Arg, ResolvedAttachPlan};

    use super::attach_argv;

    // Raw input disables processing, line buffering and echo, while preserving
    // every unrelated flag (including window and VT input for native SSH).
    // Exhaust all ten documented input-mode bits and the u32 maximum boundary.
    #[test]
    fn raw_input_preserves_unrelated_console_flags() {
        for mode in (0..=0x03ff).chain([u32::MAX]) {
            let raw = super::raw_input_mode(mode);
            assert_eq!(raw & 0x0007, 0);
            assert_eq!(raw & !0x0007, mode & !0x0007);
        }
    }

    // Raw mode spans the child lifetime and is restored on success, spawn/wait
    // failure, and unwinding. These fakes replace only the console/process seam.
    #[test]
    fn raw_terminal_restores_after_child_exit_and_failure() {
        use std::cell::RefCell;
        for outcome in [Ok(()), Err("spawn failed".to_string()), Err("wait failed".to_string())] {
            let events = RefCell::new(Vec::new());
            let result = super::with_raw_terminal(
                || {
                    events.borrow_mut().push("enter");
                    Ok(())
                },
                |_| events.borrow_mut().push("restore"),
                || {
                    events.borrow_mut().push("child");
                    outcome.clone()
                },
            );
            assert_eq!(result, outcome);
            assert_eq!(*events.borrow(), ["enter", "child", "restore"]);
        }
        let events = RefCell::new(Vec::new());
        let result = super::with_raw_terminal(
            || Err::<(), _>("no console".to_string()),
            |_| events.borrow_mut().push("restore"),
            || {
                events.borrow_mut().push("child");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(events.borrow().is_empty());
        let restored = std::cell::Cell::new(false);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = super::with_raw_terminal(|| Ok(()), |_| restored.set(true), || -> Result<(), String> { panic!("child panic") });
        }));
        assert!(panic.is_err());
        assert!(restored.get());
    }

    // The viewer must route to the binding host with its own SSH destination,
    // preserving all seat modes and refusing missing or duplicate routes.
    // Exhaustive finite cases cover every mode and route cardinality.
    #[test]
    fn remote_attach_uses_viewer_routes_and_preserves_seat_mode() {
        use flotilla_core::config::HostsConfig;
        use flotilla_protocol::{commands::AttachMode, HostName};
        let hosts: HostsConfig = serde_json::from_value(serde_json::json!({"hosts": {"kiwi": {
            "hostname": "kiwi-alias", "user": "crew", "expected_host_name": "kiwi"
        }}}))
        .expect("hosts");
        for (mode, flag) in [
            (AttachMode::Default, Some("--watch")),
            (AttachMode::PreferTake, None),
            (AttachMode::Strict, Some("--strict")),
            (AttachMode::Take, Some("--take")),
        ] {
            let plan = super::remote_attach_plan(&hosts, &HostName::new("kiwi"), "crew session", mode).expect("route");
            let (program, args) = attach_argv(&plan).expect("argv");
            assert_eq!(program, "ssh");
            assert_eq!(&args[..5], ["-t", "-o", "BatchMode=yes", "--", "crew@kiwi-alias"]);
            let [flotilla_protocol::ResolvedAttachAction::Command(argv)] = plan.0.as_slice() else { panic!("single command") };
            let Arg::NestedCommand(shell) = argv.last().expect("remote command") else { panic!("login shell") };
            assert_eq!(&shell[..3], [Arg::Literal("${SHELL:-/bin/sh}".into()), Arg::Literal("-l".into()), Arg::Literal("-c".into())]);
            let Arg::NestedCommand(command) = shell.last().expect("attach command") else { panic!("attach argv") };
            assert_eq!(&command[..5], [
                Arg::Literal("flotilla".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--transient".into()),
                Arg::Literal("--host".into()),
                Arg::Quoted("kiwi".into())
            ]);
            assert_eq!(&command[command.len() - 2..], [Arg::Literal("--".into()), Arg::Quoted("crew session".into())]);
            let seat_args = &command[5..command.len() - 2];
            assert_eq!(seat_args, &flag.map(|flag| vec![Arg::Literal(flag.into())]).unwrap_or_default());
        }
        assert!(super::remote_attach_plan(&hosts, &HostName::new("missing"), "s", AttachMode::Default).is_err());
        let duplicate: HostsConfig = serde_json::from_value(serde_json::json!({"hosts": {
            "first": {"hostname": "one", "expected_host_name": "kiwi"},
            "second": {"hostname": "two", "expected_host_name": "kiwi"}
        }}))
        .expect("duplicate routes");
        assert!(super::remote_attach_plan(&duplicate, &HostName::new("kiwi"), "s", AttachMode::Default).is_err());
    }

    // Direct argv has no shell boundary: assignment values retain their bytes.
    // Glue: one mapping from the structured assignment to env's argv element.
    // Direct argv rejects the same invalid identifiers as shell flattening,
    // while reporting a recoverable error to the caller.
    #[test]
    fn env_assignment_rejects_invalid_argv_keys() {
        for key in ["", "A=B", "FOO-BAR", "9KEY", "A;cmd"] {
            let plan = ResolvedAttachPlan::command(vec![Arg::Literal("env".into()), Arg::EnvAssignment {
                key: key.into(),
                value: "value".into(),
            }]);
            assert!(attach_argv(&plan).is_err());
        }
    }

    #[test]
    fn env_assignment_becomes_unquoted_argv() {
        let plan = ResolvedAttachPlan::command(vec![
            Arg::Literal("env".into()),
            Arg::EnvAssignment { key: "KEY".into(), value: "it's a $VALUE".into() },
            Arg::Literal("bash".into()),
        ]);
        let (program, args) = attach_argv(&plan).expect("attach argv");
        assert_eq!(program, "env");
        assert_eq!(args, ["KEY=it's a $VALUE", "bash"]);
    }

    #[test]
    fn attach_plan_becomes_direct_argv_without_a_shell_boundary() {
        let plan = ResolvedAttachPlan::command(vec![
            Arg::Literal("docker".into()),
            Arg::Literal("exec".into()),
            Arg::Literal("-it".into()),
            Arg::Quoted("crew box".into()),
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted("crew session".into()),
        ]);

        let (program, args) = attach_argv(&plan).expect("single attach command");

        assert_eq!(program, "docker");
        assert_eq!(args, ["exec", "-it", "crew box", "cleat", "attach", "crew session"]);
    }

    #[test]
    fn nested_remote_command_is_one_ssh_argv_element() {
        let plan = ResolvedAttachPlan::command(vec![
            Arg::Literal("ssh".into()),
            Arg::Literal("-t".into()),
            Arg::Quoted("udder".into()),
            Arg::NestedCommand(vec![Arg::Literal("flotilla".into()), Arg::Literal("attach".into()), Arg::Quoted("crew session".into())]),
        ]);

        let (program, args) = attach_argv(&plan).expect("single SSH command");

        assert_eq!(program, "ssh");
        assert_eq!(args, ["-t", "udder", "flotilla attach 'crew session'"]);
    }
}

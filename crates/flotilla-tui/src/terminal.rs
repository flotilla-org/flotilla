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
        .map(|value| match value {
            Arg::Literal(value) | Arg::Quoted(value) => value.clone(),
            Arg::NestedCommand(inner) => arg::flatten(inner, 1),
        })
        .collect::<Vec<_>>();
    if argv.is_empty() {
        return Err("attach resolution produced an empty command".to_string());
    }
    let program = argv.remove(0);
    Ok((program, argv))
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

#[cfg(all(test, unix))]
mod tests {
    use flotilla_protocol::{arg::Arg, ResolvedAttachPlan};

    use super::attach_argv;

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

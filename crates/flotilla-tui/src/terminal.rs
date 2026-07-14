use std::io::stdout;

use crossterm::{event::DisableMouseCapture, execute};

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

/// Temporarily leave the TUI to inspect a terminal session, then restore it.
///
/// This deliberately does not stamp Presentation Manager metadata: the pane
/// remains owned by its existing project/archipelago context while the attach
/// is only a transient foreground excursion.
pub fn run_temporary_attach(command: &str) -> (ratatui::DefaultTerminal, Result<(), String>) {
    restore_terminal();
    let result = std::process::Command::new("sh")
        .arg("-lc")
        .arg(command)
        .status()
        .map_err(|error| format!("could not start attach command: {error}"))
        .and_then(|status| {
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
/// Must be called after `ratatui::init()`. Wraps whatever hook is currently
/// installed (including color_eyre's) so error reporting still works.
pub fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
}

/// Spawn a background task that listens for SIGTERM and cleanly exits.
///
/// Must be called after `ratatui::init()` within a tokio runtime.
/// Covers the entire process lifetime — including the startup window
/// before the event loop begins.
#[cfg(unix)]
pub fn install_sigterm_handler() {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to register SIGTERM handler");
    tokio::spawn(async move {
        sigterm.recv().await;
        restore_terminal();
        std::process::exit(0);
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

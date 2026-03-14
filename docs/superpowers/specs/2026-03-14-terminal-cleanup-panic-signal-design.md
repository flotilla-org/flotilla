# Terminal Cleanup on Panic/Signal

**Issue:** [#330](https://github.com/rjwittams/flotilla/issues/330)
**Date:** 2026-03-14

## Problem

If flotilla panics after `ratatui::init()`, the terminal is left in raw mode with the alternate screen active. The user must run `reset` to recover. This happens because `color_eyre::install()` runs before `ratatui::init()`, so its panic hook has no knowledge of terminal state.

Additionally, there is no SIGTERM handler for the TUI process, no Ctrl-Z (suspend/resume) support, and `DisableMouseCapture` is only called on the normal exit path.

## Solution

A new `terminal` module in `flotilla-tui` that centralises terminal lifecycle management.

### New module: `crates/flotilla-tui/src/terminal.rs`

Three public functions:

- **`restore_terminal()`** — Calls `DisableMouseCapture` then `ratatui::restore()`. Both are safe to call redundantly. Used by all cleanup paths (panic hook, signal handler, normal exit).

- **`install_panic_hook()`** — Wraps the existing panic hook (including color_eyre's) to call `restore_terminal()` before printing the panic. Must be called after `ratatui::init()`.

- **`suspend_and_resume() -> io::Result<DefaultTerminal>`** — Restores terminal, sends `SIGTSTP` to self (process suspends at this point), then on resume re-initialises terminal and re-enables mouse capture. Returns the new `DefaultTerminal`. `#[cfg(unix)]` only.

### Integration points

**`src/main.rs`** — Call `install_panic_hook()` immediately after `ratatui::init()`. Replace three existing `ratatui::restore()` calls in early-exit error paths with `restore_terminal()`.

**`crates/flotilla-tui/src/event.rs`** — Add `Event::Signal` variant. Register a `SIGTERM` listener in `EventHandler::new()` alongside the crossterm reader and tick interval, emitting `Event::Signal` when received.

**`crates/flotilla-tui/src/run.rs`** — Handle `Event::Signal` by setting `app.should_quit = true`. Intercept `Ctrl+Z` key events before `app.handle_key()` and call `suspend_and_resume()`, replacing the `terminal` binding. Replace inline `DisableMouseCapture` + `ratatui::restore()` on normal exit with `restore_terminal()`.

**`crates/flotilla-tui/src/lib.rs`** — Add `pub mod terminal;`.

## Out of scope

- **Input buffer flush (`tcflush`)** — Only relevant when external program launching is added. Deferred.

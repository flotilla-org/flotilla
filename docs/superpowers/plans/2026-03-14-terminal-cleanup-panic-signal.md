# Terminal Cleanup on Panic/Signal Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure the terminal is always restored to a usable state when flotilla exits — whether by panic, signal, suspend/resume, or normal quit.

**Architecture:** A new `terminal` module in `flotilla-tui` centralises three cleanup functions (`restore_terminal`, `install_panic_hook`, `suspend_and_resume`). These are called from `main.rs` (panic hook + early exits), `event.rs` (SIGTERM → `Event::Signal`), and `run.rs` (signal handling, Ctrl-Z, normal exit). Mouse capture cleanup is folded into `restore_terminal` so every path gets it.

**Tech Stack:** ratatui 0.30, crossterm 0.29, tokio signals, libc (SIGTSTP)

**Testing note:** Terminal lifecycle code is inherently side-effectful — it requires an actual terminal and process signals. These changes are verified by: compilation, existing test suite passing, and manual verification (panic with `todo!()`, `kill -TERM`, Ctrl-Z). No new unit tests are added.

---

## Chunk 1: Core terminal module + panic hook + SIGTERM + Ctrl-Z

### Task 1: Create `terminal.rs` with `restore_terminal` and `install_panic_hook`

**Files:**
- Create: `crates/flotilla-tui/src/terminal.rs`
- Modify: `crates/flotilla-tui/src/lib.rs`

- [ ] **Step 1: Create `terminal.rs`**

```rust
use std::io::stdout;

use crossterm::{execute, event::DisableMouseCapture};

/// Restore the terminal to its original state.
///
/// Safe to call multiple times or when mouse capture was never enabled —
/// `DisableMouseCapture` and `ratatui::restore()` are both no-ops in those cases.
pub fn restore_terminal() {
    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = ratatui::restore();
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
```

- [ ] **Step 2: Register the module in `lib.rs`**

Add `pub mod terminal;` to `crates/flotilla-tui/src/lib.rs`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-tui`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/terminal.rs crates/flotilla-tui/src/lib.rs
git commit -m "feat: add terminal lifecycle module with restore and panic hook"
```

---

### Task 2: Wire up panic hook and `restore_terminal` in `main.rs`

**Files:**
- Modify: `src/main.rs:152-246` (the `run_tui` function)

- [ ] **Step 1: Install panic hook after `ratatui::init()`**

At `src/main.rs`, in `run_tui()`, immediately after `let mut terminal = ratatui::init();` (line 159), add:

```rust
flotilla_tui::terminal::install_panic_hook();
```

- [ ] **Step 2: Replace early-exit `ratatui::restore()` calls with `restore_terminal()`**

Three call sites in `run_tui()`:

1. Line 166 (no repos found):
   ```rust
   // Before:
   ratatui::restore();
   // After:
   flotilla_tui::terminal::restore_terminal();
   ```

2. Line 219 (daemon error):
   ```rust
   // Before:
   ratatui::restore();
   // After:
   flotilla_tui::terminal::restore_terminal();
   ```

3. Line 225 (daemon panic):
   ```rust
   // Before:
   ratatui::restore();
   // After:
   flotilla_tui::terminal::restore_terminal();
   ```

- [ ] **Step 3: Verify it compiles and tests pass**

Run: `cargo build && cargo test --locked`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "fix: install panic hook and use restore_terminal for early exits"
```

---

### Task 3: Add `Event::Signal` and SIGTERM listener

**Files:**
- Modify: `crates/flotilla-tui/src/event.rs`

- [ ] **Step 1: Add `Signal` variant to `Event` enum**

In `event.rs`, add to the `Event` enum:

```rust
#[derive(Clone, Debug)]
pub enum Event {
    Tick,
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Daemon(Box<DaemonEvent>),
    Signal,
}
```

- [ ] **Step 2: Register SIGTERM in `EventHandler::new()`**

In the spawned task inside `EventHandler::new()`, before the main loop, create the signal listener. Add it as a third arm in the main `tokio::select!`:

```rust
tokio::spawn(async move {
    let mut reader = EventStream::new();

    // ... existing drain loop ...

    let mut interval = tokio::time::interval(tick_rate);

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    loop {
        let delay = interval.tick();
        let event = reader.next().fuse();
        tokio::select! {
            _ = delay => { let _ = tx_clone.send(Event::Tick); }
            maybe = event => match maybe {
                Some(Ok(crossterm::event::Event::Key(k)))
                    if k.kind == KeyEventKind::Press =>
                {
                    let _ = tx_clone.send(Event::Key(k));
                }
                Some(Ok(crossterm::event::Event::Mouse(m))) => {
                    let _ = tx_clone.send(Event::Mouse(m));
                }
                _ => {}
            }
            #[cfg(unix)]
            _ = sigterm.recv() => {
                let _ = tx_clone.send(Event::Signal);
            }
        }
    }
});
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-tui`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/event.rs
git commit -m "feat: add SIGTERM listener to TUI event stream"
```

---

### Task 4: Handle `Event::Signal` and use `restore_terminal` on normal exit in `run.rs`

**Files:**
- Modify: `crates/flotilla-tui/src/run.rs`

- [ ] **Step 1: Handle `Event::Signal` in the event processing loop**

In `run.rs`, in the `for evt in other_events` loop (line 83), add a new arm:

```rust
Event::Signal => {
    app.should_quit = true;
}
```

- [ ] **Step 2: Replace normal exit cleanup with `restore_terminal()`**

Replace lines 233-234:
```rust
// Before:
execute!(stdout(), DisableMouseCapture)?;
ratatui::restore();

// After:
crate::terminal::restore_terminal();
```

Remove `DisableMouseCapture` from the crossterm imports (line 5) since it's no longer used directly in this file. Keep `EnableMouseCapture`.

- [ ] **Step 3: Verify it compiles and tests pass**

Run: `cargo build && cargo test --locked`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/run.rs
git commit -m "feat: handle SIGTERM signal and centralise terminal cleanup on exit"
```

---

### Task 5: Add `suspend_and_resume` and handle Ctrl-Z

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml` (add `libc` dependency)
- Modify: `crates/flotilla-tui/src/terminal.rs` (add `suspend_and_resume`)
- Modify: `crates/flotilla-tui/src/run.rs` (intercept Ctrl-Z)

- [ ] **Step 1: Add `libc` dependency**

Add to `crates/flotilla-tui/Cargo.toml` under `[dependencies]`:

```toml
libc = "0.2"
```

- [ ] **Step 2: Add `suspend_and_resume` to `terminal.rs`**

Append to `crates/flotilla-tui/src/terminal.rs`:

```rust
/// Suspend the process (Ctrl-Z / SIGTSTP).
///
/// Restores the terminal to its original state, delivers SIGTSTP to the
/// process group (which suspends execution here), then re-initialises the
/// terminal when the process is resumed (SIGCONT).
///
/// Returns the new [`ratatui::DefaultTerminal`] — callers must replace
/// their existing terminal binding with this value.
#[cfg(unix)]
pub fn suspend_and_resume() -> std::io::Result<ratatui::DefaultTerminal> {
    use crossterm::{execute, event::EnableMouseCapture};

    restore_terminal();
    // SAFETY: kill(0, SIGTSTP) sends the signal to the entire process group.
    // The process suspends at this point and resumes on SIGCONT.
    unsafe { libc::kill(0, libc::SIGTSTP); }
    // Resumed — re-initialise terminal
    let terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    Ok(terminal)
}
```

- [ ] **Step 3: Intercept Ctrl-Z in `run.rs`**

In `run.rs`, in the `Event::Key(k)` arm (around line 88), add a Ctrl-Z check before the existing `handle_key` dispatch:

```rust
Event::Key(k) => {
    // Ctrl-Z: suspend/resume (unix only)
    #[cfg(unix)]
    if k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) {
        match crate::terminal::suspend_and_resume() {
            Ok(new_terminal) => terminal = new_terminal,
            Err(e) => tracing::warn!(err = %e, "suspend/resume failed"),
        }
        continue;
    }

    let is_normal = matches!(app.ui.mode, UiMode::Normal);
    if k.code == KeyCode::Char('r') && is_normal {
        // ... existing refresh logic ...
```

Add `KeyModifiers` to the crossterm imports in `run.rs` if not already present.

- [ ] **Step 4: Verify it compiles and all tests pass**

Run: `cargo build && cargo test --locked`
Expected: success

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml crates/flotilla-tui/src/terminal.rs crates/flotilla-tui/src/run.rs
git commit -m "feat: add Ctrl-Z suspend/resume support"
```

---

### Task 6: Final check — clippy + format

- [ ] **Step 1: Run formatter and linter**

Run: `cargo +nightly fmt && cargo clippy --all-targets --locked -- -D warnings`
Expected: success (fix any warnings)

- [ ] **Step 2: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass

- [ ] **Step 3: Commit any formatting fixes**

```bash
git add -A
git commit -m "chore: fmt + clippy fixes"
```

(Skip if no changes.)

# Cleat Send-Keys First Cut Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a first-cut `cleat send-keys` command that injects tmux-style key tokens into an existing session.

**Architecture:** Extend the existing `cleat` daemon control protocol with an input-injection request, then add a tmux-shaped CLI/parser layer that converts user tokens into terminal bytes before sending them to the daemon. Keep the first cut tightly scoped: recognized tmux-style key names, fallback-to-literal behavior, and support for `-l`, `-H`, and `-N`.

**Tech Stack:** Rust, clap, existing `cleat` frame protocol, PTY input path in `crates/cleat/src/session.rs`

---

## File Map

- Modify: `crates/cleat/src/cli.rs`
  - Add the public `send-keys` subcommand and CLI execution path.
- Modify: `crates/cleat/src/server.rs`
  - Add `SessionService::send_keys` request plumbing.
- Modify: `crates/cleat/src/protocol.rs`
  - Add a daemon control frame for injected input and any small response handling needed.
- Modify: `crates/cleat/src/session.rs`
  - Handle the new input-injection frame in the daemon loop and write bytes into the PTY input path.
- Create: `crates/cleat/src/keys.rs`
  - Parse tmux-style key tokens into terminal byte sequences.
- Modify: `crates/cleat/src/lib.rs`
  - Export the new key parser module if needed by tests.
- Modify: `crates/cleat/tests/cli.rs`
  - Lock the command shape and flags.
- Modify: `crates/cleat/tests/lifecycle.rs`
  - Add end-to-end behavior tests proving injected input reaches a live session.
- Create or modify: `crates/cleat/tests/keys.rs`
  - Focused parser tests for literals, named keys, modifiers, hex mode, and repeat behavior.

## Scope Notes

- First cut supports:
  - `cleat send-keys <id> [key ...]`
  - tmux-style recognized key names
  - fallback to literal text for unknown tokens
  - `-l` literal mode
  - `-H` hex byte mode
  - `-N <repeat-count>`
- Explicitly out of scope in this plan:
  - `-R`, `-M`, `-F`, `-K`, `-X`
  - mouse event injection
  - client key-table semantics
  - copy-mode semantics
  - a `view` command

### Task 1: Lock The CLI Surface

**Files:**
- Modify: `crates/cleat/tests/cli.rs`
- Modify: `crates/cleat/src/cli.rs`

- [ ] **Step 1: Write the failing CLI parser tests**

Add tests covering:
- `cleat send-keys demo Enter`
- `cleat send-keys -l demo hello world`
- `cleat send-keys -H demo 41 0a`
- `cleat send-keys -N 3 demo C-l`

- [ ] **Step 2: Run the CLI test target to verify failure**

Run: `cargo test -p cleat --locked send_keys`
Expected: FAIL because `Command::SendKeys` does not exist yet.

- [ ] **Step 3: Add the minimal CLI shape**

In `crates/cleat/src/cli.rs`:
- add `Command::SendKeys { id, literal, hex, repeat, keys }`
- wire `execute()` to call `service.send_keys(...)`
- keep help text tmux-shaped but minimal

- [ ] **Step 4: Re-run the CLI tests**

Run: `cargo test -p cleat --locked send_keys`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "feat: add cleat send-keys cli"
```

### Task 2: Add A Key Parser Module

**Files:**
- Create: `crates/cleat/src/keys.rs`
- Modify: `crates/cleat/src/lib.rs`
- Create or modify: `crates/cleat/tests/keys.rs`

- [ ] **Step 1: Write the failing parser tests**

Add focused tests for:
- literal token fallback: `hello` -> `b"hello"`
- named keys: `Enter`, `Tab`, `BSpace`, `Up`
- control modifiers: `C-c`, `^D`
- meta modifiers: `M-x`
- shifted named keys where supported
- literal mode: tokens are concatenated verbatim as UTF-8
- hex mode: `41 0a` -> `b"A\n"`
- repeat count: parsed output repeats the full sequence

- [ ] **Step 2: Run the parser tests to verify failure**

Run: `cargo test -p cleat --locked keys`
Expected: FAIL because `keys.rs` and its parser do not exist.

- [ ] **Step 3: Implement the minimal parser**

In `crates/cleat/src/keys.rs`:
- define a parser API that accepts:
  - `tokens: &[String]`
  - `literal: bool`
  - `hex: bool`
  - `repeat: usize`
- emit `Vec<u8>`
- implement tmux-style recognized names
- use fallback-to-literal for unknown tokens when not in hex mode
- reject invalid hex input clearly

- [ ] **Step 4: Re-run the parser tests**

Run: `cargo test -p cleat --locked keys`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/keys.rs crates/cleat/src/lib.rs crates/cleat/tests/keys.rs
git commit -m "feat: add cleat send-keys parser"
```

### Task 3: Add Protocol And Service Support

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/server.rs`

- [ ] **Step 1: Write the failing service-level tests**

Add tests proving:
- sending keys to a missing session errors cleanly
- the service sends an input-injection request frame to the daemon socket

- [ ] **Step 2: Run the focused tests to verify failure**

Run: `cargo test -p cleat --locked send_keys_missing send_keys_request`
Expected: FAIL because the service method and protocol frame do not exist.

- [ ] **Step 3: Add the minimal protocol/service layer**

In `crates/cleat/src/protocol.rs`:
- add an input-injection control frame carrying raw bytes

In `crates/cleat/src/server.rs`:
- add `SessionService::send_keys(&self, id: &str, bytes: &[u8]) -> Result<(), String>`
- mirror the `detach`/`capture` pattern:
  - check session existence
  - connect to the session socket
  - write the input-injection frame

- [ ] **Step 4: Re-run the focused tests**

Run: `cargo test -p cleat --locked send_keys_missing send_keys_request`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/server.rs
git commit -m "feat: add cleat send-keys service path"
```

### Task 4: Inject Bytes In The Daemon

**Files:**
- Modify: `crates/cleat/src/session.rs`

- [ ] **Step 1: Write the failing lifecycle test**

Add an end-to-end test in `crates/cleat/tests/lifecycle.rs`:
- create a session running `cat`
- call `cleat send-keys alpha hello Enter`
- capture or attach-read output and assert `hello` appears

Also add a test for a named key such as `Enter` or `C-c` reaching the PTY path.

- [ ] **Step 2: Run the lifecycle test to verify failure**

Run: `cargo test -p cleat --locked --test lifecycle send_keys`
Expected: FAIL because the daemon ignores the new frame.

- [ ] **Step 3: Implement minimal daemon handling**

In `crates/cleat/src/session.rs`:
- handle the new input-injection frame in the same non-foreground request path as `Detach` and `Capture`
- write the bytes through the existing PTY input helper
- avoid involving the foreground client socket path

- [ ] **Step 4: Re-run the lifecycle test**

Run: `cargo test -p cleat --locked --test lifecycle send_keys`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/session.rs crates/cleat/tests/lifecycle.rs
git commit -m "feat: inject cleat send-keys input into sessions"
```

### Task 5: Wire CLI To Parser

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write one failing end-to-end CLI test**

Add a test that executes the parsed `send-keys` command path through `cli::execute(...)` and proves:
- text mode works
- repeat mode works
- unsupported hex input errors clearly

- [ ] **Step 2: Run the focused test to verify failure**

Run: `cargo test -p cleat --locked send_keys_cli`
Expected: FAIL because `cli::execute` does not yet parse/send the bytes through the new parser.

- [ ] **Step 3: Implement the minimal wiring**

In `crates/cleat/src/cli.rs`:
- parse flags
- call the key parser
- pass the resulting bytes to `service.send_keys(...)`

- [ ] **Step 4: Re-run the focused test**

Run: `cargo test -p cleat --locked send_keys_cli`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/lifecycle.rs
git commit -m "feat: wire cleat send-keys command"
```

### Task 6: Final Verification

**Files:**
- No new files expected

- [ ] **Step 1: Run package verification**

Run: `cargo test -p cleat --locked`
Expected: PASS

- [ ] **Step 2: Run feature-on verification**

Run: `cargo test -p cleat --locked --features ghostty-vt`
Expected: PASS

- [ ] **Step 3: Run lint and format**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: PASS

Run: `cargo clippy -p cleat --all-targets --locked --features ghostty-vt -- -D warnings`
Expected: PASS

- [ ] **Step 4: Manual sanity check**

Run:

```bash
target/debug/cleat send-keys <id> h e l l o Enter
target/debug/cleat send-keys -N 3 <id> Up
target/debug/cleat send-keys -l <id> 'literal text'
target/debug/cleat send-keys -H <id> 41 0a
```

Expected:
- injected text appears in the session
- repeated keys are repeated
- literal mode sends text unchanged
- hex mode sends exact bytes

- [ ] **Step 5: Commit**

```bash
git add crates/cleat
git commit -m "test: verify cleat send-keys first cut"
```

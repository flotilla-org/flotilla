# Agent Notes

## Test Command Defaults

- Normal environment: `cargo test --workspace --locked`
- Restricted Codex sandbox (socket bind/listen blocked): `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Use the sandbox-safe command when `CODEX_SANDBOX` is set or socket-bind tests are expected to fail with `Operation not permitted`. The repo-local `TMPDIR` avoids native build failures from crates like `aws-lc-sys` under the sandbox.

## Cursor Cloud specific instructions

### Overview

Flotilla is a single Rust TUI binary (no databases, Docker, or background services). All development commands are in `CLAUDE.md`.

### Toolchain

The VM ships with Rust 1.83 by default, but dependencies require edition 2024 (Rust ≥ 1.85). The update script handles upgrading to `stable` automatically via `rustup default stable && rustup update stable`.

### Running the app

```bash
cargo run -- --repo-root /workspace   # launches TUI against this repo
```

The app auto-detects git, GitHub (`gh` CLI), Claude, and terminal multiplexers from the environment. Only `git` is required; everything else degrades gracefully when absent.

### Key commands

| Task | Command |
|------|---------|
| Build | `cargo build --locked` |
| Lint (format) | `cargo fmt --check` |
| Lint (clippy) | `cargo clippy --all-targets --locked -- -D warnings` |
| Test | `cargo test --workspace --locked` |
| Test (sandbox) | `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests` |
| Run | `cargo run -- --repo-root /workspace` |

### Gotchas

- `cargo build` without `--locked` may update `Cargo.lock`; use `--locked` for reproducible builds.
- The TUI needs a real terminal (TTY). Use `cargo run` inside a terminal emulator, not piped.

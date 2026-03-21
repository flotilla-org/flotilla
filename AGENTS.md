# Agent Notes

**All project guidance lives in `CLAUDE.md`** — architecture, conventions, testing philosophy, CI commands, record/replay testing, issue types and labels. Read it first.

This file covers agent-specific environment quirks.

## Codex Sandbox

Socket bind/listen is blocked. Use the sandbox-safe test command:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

The repo-local `TMPDIR` avoids native build failures from crates like `aws-lc-sys` under the sandbox.

Use this when `CODEX_SANDBOX` is set or socket-bind tests fail with `Operation not permitted`.

## Cursor Cloud

The VM ships with Rust 1.83 by default, but dependencies require edition 2024 (Rust >= 1.85). Update with `rustup default stable && rustup update stable`.

The TUI needs a real terminal (TTY). Use `cargo run` inside a terminal emulator, not piped.

## General

- `cargo build` without `--locked` may update `Cargo.lock`; use `--locked` for reproducible builds.
- Formatting follows `cargo +nightly-2026-03-12 fmt`; `cargo fmt` on stable may not match.
- If you say a change matches CI locally, it should have been checked against the exact commands in `CLAUDE.md` rather than close approximations.

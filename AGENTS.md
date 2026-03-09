# Agent Notes

## Test Command Defaults

- Normal environment: `cargo test --workspace --locked`
- Restricted Codex sandbox (socket bind/listen blocked): `cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Use the sandbox-safe command when `CODEX_SANDBOX` is set or socket-bind tests are expected to fail with `Operation not permitted`.

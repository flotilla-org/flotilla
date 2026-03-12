# Shpool: Only restart daemon when config changes (#251)

## Context

Flotilla manages a shpool daemon with a dedicated socket and config at `~/.config/flotilla/shpool/`. The embedded config (`shpool_config.toml`) is written to disk by `ensure_config()` on startup.

Currently, `start_daemon()` reuses an existing live daemon without checking whether the config has changed since it was started. This means config updates (e.g., new `forward_env` entries added in a flotilla release) are not picked up until the daemon is manually restarted or dies.

## Design

### `ensure_config()` returns whether config changed

Change the return type from `()` to `bool`. Returns `true` when the config file was successfully written (missing or stale), `false` when it already matched or when the write failed. This prevents killing a good daemon when the new config couldn't be persisted. The existing `needs_write` variable captures the comparison; the return value is gated on both `needs_write` and `fs::write` succeeding.

### New `stop_daemon()` helper

A `#[cfg(unix)]` async method that gracefully stops a running daemon:

1. Read pid from `daemonized-shpool.pid` (sibling of socket file). Handle missing pid file gracefully (log and return — if there's no pid file, we can't signal the daemon).
2. Send `SIGTERM` via `libc::kill(pid, libc::SIGTERM)`.
3. Poll `is_process_alive(pid)` up to 20 times with 100ms sleep (2s timeout, same pattern as `start_daemon`'s socket polling).
4. Remove socket and pid files after the process exits (or after timeout). This removal is load-bearing: `start_daemon()` checks `socket_path.exists()` as its first guard and returns early if true, so the socket must be gone for the replacement daemon to spawn.
5. If the process won't die within the timeout, log a warning but continue — the new daemon may fail to bind the socket, but shpool's auto-daemonize fallback from `attach` still works.

Non-unix stub is a no-op.

### Updated `create()` flow

```rust
let config_changed = Self::ensure_config(&config_path);
Self::clean_stale_socket(&socket_path);
if config_changed && socket_path.exists() {
    Self::stop_daemon(&socket_path).await;
}
Self::start_daemon(&socket_path, &config_path).await;
```

After `clean_stale_socket()`, if the socket still exists the daemon is alive. If `config_changed` is also true, log `info!("shpool config changed, restarting daemon")` and stop the daemon so `start_daemon()` will spawn a replacement with the new config.

### Testing

- `ensure_config` returns `true` on first write, `false` on idempotent second call, `true` again after the file is externally modified.
- `stop_daemon` with a fake pid file pointing to a dead pid cleans up files without error.
- Existing `create_writes_config_and_returns_pool` test continues to pass unchanged.

## Files changed

| File | Change |
|------|--------|
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | `ensure_config() -> bool`, add `stop_daemon()`, update `create()` flow, new tests |

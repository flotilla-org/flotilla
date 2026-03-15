# Remote Terminal Execution Design

**Issue:** [#273 — Remote terminal execution](https://github.com/rjwittams/flotilla/issues/273)
**Parent:** [#253 — Multi-host Phase 2](https://github.com/rjwittams/flotilla/issues/253)
**Depends on:** Remote command routing (#272, done), Remote checkout creation (#274, done)

## Problem

PR #334 landed the passthrough approach for remote terminals: the local workspace manager SSHes to the remote host and runs commands directly. This works partially but has three problems:

1. **Broken environment** — `ssh -t host 'cd /path && command'` runs in a non-login shell. PATH, tool versions, and shell configuration are missing, so commands that work interactively fail when launched this way.
2. **No SSH multiplexing** — each pane opens a separate SSH connection. With multiple panes per workspace, this is slow and wasteful.
3. **No host visibility** — the UI does not indicate which host a workspace's terminals run on.

The persistent terminal pool path (shpool on the remote host) already has plumbing in place — `prepare_terminal_commands` calls `resolve_terminal_pool` — but has not been validated end-to-end.

## Design

### 1. Login shell wrapping

Change `wrap_remote_attach_commands` to wrap the remote command in a login shell:

```
ssh -t host '$SHELL -l -c "cd /path && command"'
```

`$SHELL` is set from `/etc/passwd` by sshd, so it is available even in non-interactive SSH. The `-l` flag sources the user's profile, giving proper PATH and environment. The `-c` flag is POSIX and works on sh, bash, zsh, dash, ksh, and fish (fish has slightly different double-quote escaping rules, but the commands we send contain no backticks, so POSIX escaping is sufficient).

Note: `$SHELL` is a literal string in the Rust source, not expanded locally. The remote shell expands it after SSH delivers the command.

**Quoting strategy:** The command needs two layers of escaping:

1. **Inner layer** — escape `"`, `$`, `` ` ``, and `\` for the double-quoted `-c` argument. Add an `escape_for_double_quotes` helper alongside `shell_quote`.
2. **Outer layer** — single-quote the entire SSH command string (existing `shell_quote` function).

For the shpool path, the attach command contains no special characters, so the inner escaping is a no-op in practice. Note that shpool's `attach_command` already wraps the inner command in a login shell (`$SHELL -lic 'command'`). The outer `$SHELL -l -c` from the SSH wrapping ensures `shpool` itself is on PATH. This double login-shell wrapping is intentional and harmless.

**Files:** `crates/flotilla-core/src/executor.rs` — modify `wrap_remote_attach_commands`, add `escape_for_double_quotes`.

### 2. SSH multiplexing

Add connection multiplexing so multiple panes reuse a single SSH connection per host.

**Config in `hosts.toml`:**

```toml
# Global default (optional, defaults to true)
[ssh]
multiplex = true

# Per-host override
[hosts.feta]
hostname = "feta.local"
expected_host_name = "feta"
daemon_socket = "/tmp/flotilla.sock"
ssh_multiplex = false
```

Resolution: per-host `ssh_multiplex` overrides global `ssh.multiplex`, which defaults to true.

**SSH args when enabled:**

```
-o ControlMaster=auto -o ControlPath=<dir>/ctrl-%r@%h-%p -o ControlPersist=60
```

The control socket directory (`~/.config/flotilla/ssh/`) is created by `wrap_remote_attach_commands` via `std::fs::create_dir_all` before building the SSH args. If the directory cannot be created, fall back to non-multiplexed SSH and log a warning. The path is constructed programmatically — no user-facing path configuration.

**Files:**
- `crates/flotilla-core/src/config.rs` — add `SshConfig` struct with `multiplex: bool`. Add `ssh_multiplex: Option<bool>` to both `RemoteHostConfig` and `RawRemoteHostConfig`. Add `ssh: Option<SshConfig>` to `RawHostsConfig`. Thread both through the existing custom `Deserialize` impl for `HostsConfig`. Add `resolved_ssh_multiplex` method that resolves per-host override against global default.
- `crates/flotilla-core/src/executor.rs` — refactor `remote_ssh_target` to return a struct containing the SSH target string and resolved multiplex setting (currently returns just `Result<String, String>`). `wrap_remote_attach_commands` uses this to conditionally add multiplex args.

### 3. Shpool path validation

The remote terminal pool flow already exists in code:

1. TUI sends `PrepareTerminalForCheckout` to the remote daemon.
2. Remote daemon calls `prepare_terminal_commands`, which calls `resolve_terminal_pool`.
3. If shpool is present, `ensure_running` starts a persistent session; `attach_command` returns `shpool --socket /path attach flotilla/feat/main/0`.
4. The command is returned in `TerminalPrepared`.
5. Local TUI queues `CreateWorkspaceFromPreparedTerminal`.
6. Local executor wraps in SSH: `ssh -t host '$SHELL -l -c "shpool --socket ... attach ..."'`.

**What to verify:** This flow works end-to-end on real hosts. The login shell fix in Section 1 ensures shpool is on PATH. If shpool fails on the remote host (daemon not running, binary missing), the error must surface clearly rather than silently producing a broken workspace.

No structural changes — just E2E validation and fixing whatever breaks.

### 4. Workspace host naming

When creating a workspace via `CreateWorkspaceFromPreparedTerminal`, prefix the workspace name with the target host:

```
feat@feta
```

instead of:

```
feat
```

This gives immediate visibility in the workspace manager's own UI (cmux tab bar, zellij tab name) with no TUI rendering changes. The host prefix is applied in the executor when building `WorkspaceConfig` for remote terminals.

Future directions (deferred): policy-driven naming, workspace manager styling (icons, colours), cross-host workspaces showing all agents on a work item across hosts.

**Files:** `crates/flotilla-core/src/executor.rs` — modify `CreateWorkspaceFromPreparedTerminal` arm to format `config.name` as `branch@host`.

### 5. Test coverage

**Unit tests (new):**
- `escape_for_double_quotes` — special characters, empty string, no-op cases.
- `wrap_remote_attach_commands` with login shell wrapping — verify `$SHELL -l -c` structure.
- SSH multiplex args — present when enabled, absent when disabled, per-host override.
- Workspace naming — `host:branch` format for remote, plain `branch` for local.

**Existing tests to extend:**
- `create_workspace_from_prepared_terminal_wraps_remote_commands_in_ssh` — add assertions for `$SHELL -l -c` wrapping structure (existing `.contains()` assertions remain valid).
- `create_workspace_from_prepared_terminal_uses_local_fallback_for_remote_only_repo` — same.

**Manual E2E:** Test on 3-host setup:
1. Passthrough path — verify panes get login shell environment.
2. Shpool path — verify persistent sessions, SSH attach, session survival after local teardown.
3. SSH multiplexing — verify connection reuse, test with multiplexing disabled.

## Changes to existing code

| File | Change |
|------|--------|
| `crates/flotilla-core/src/executor.rs` | Login shell wrapping, SSH multiplex args, host-prefixed workspace name, `escape_for_double_quotes` helper, refactor `remote_ssh_target` to return struct |
| `crates/flotilla-core/src/config.rs` | `SshConfig` struct, `ssh_multiplex` on `RemoteHostConfig` + `RawRemoteHostConfig`, `ssh` on `RawHostsConfig`, update custom `Deserialize` impl, `resolved_ssh_multiplex` method |
| `crates/flotilla-core/src/executor.rs` (tests) | Extend existing SSH wrapping tests, add new tests for quoting/multiplex/naming |

## Out of scope

- Remote terminal pool visibility in the TUI (list/kill remote terminals) — follow-up issue.
- Session handoff (#275) — depends on this work.
- SSH bootstrap via `flotilla daemon-socket` subcommand — follow-up.
- Workspace manager styling (icons, colours) — follow-up.
- Cross-host workspaces (all agents on a work item across hosts) — future vision.

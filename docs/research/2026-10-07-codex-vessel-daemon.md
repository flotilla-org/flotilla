# Vessel-scoped Codex managed daemon

**Date:** 2026-10-07
**Status:** Source research plus passing direct-protocol and candidate Rust adapter live acceptance.
**Version examined:** OpenAI Codex `rust-v0.160.0`.

## Finding

A vessel can share one Codex-managed app-server among crew threads. This aligns with the operator's stated vessel isolation boundary. Codex already owns detached startup, lifecycle locking, readiness, socket reporting, shutdown, and package selection; Flotilla need not implement those with another cleat session. Functional crew identity still needs a per-thread command-environment overlay. That is a routing concern, not a new security boundary.

## Supported by pinned sources

**Managed daemon lifecycle.** `codex app-server daemon start` is idempotent and returns after the Unix control socket can answer initialize. Successful lifecycle commands emit one JSON object reporting backend, socket path, local CLI version, and running app-server version when applicable. Lifecycle mutations are serialized per `CODEX_HOME`. State, settings, PID records and lifecycle lock live under `CODEX_HOME/app-server-daemon/`. Linux, macOS and Windows are supported, subject to Windows detached-process/socket-path constraints. [Daemon README](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/README.md).

**Installation prerequisite.** `bootstrap` can create the managed daemon package by copying a complete invoking CLI package, but a bare executable cannot supply a fresh installation. Existing selected packages are reused, including legacy packages; a broken selection is not silently replaced. New packages use `CODEX_HOME/packages/app-server-daemon/current/bin/codex`. Therefore “the vessel contains a codex executable” does not establish that managed start/bootstrap will work; image/package provisioning must supply and validate a complete package. [Daemon README, Bootstrap and Managed packages](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/README.md).

**TUI connection.** The TUI can connect to an existing implicit local daemon; absent an eligible socket it selects an embedded server. Implicit initialization failure permits embedded fallback. Explicit `--remote` endpoints are authoritative and fail rather than silently choose another server. `CODEX_EXEC_SERVER_URL`, workload identity and incompatible launch configuration can prevent implicit sharing. Use the reported daemon endpoint explicitly instead of depending on discovery. The relevant selection function is `app_server_target_for_launch`, and connection handling is `start_app_server`. These findings do not establish that every ordinary TUI invocation starts a daemon automatically. [TUI source](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/tui/src/lib.rs), [daemon README](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/README.md).

**Thread-specific configuration.** `ThreadStartParams` supplies independent cwd, model/provider, approval policy/reviewer, sandbox/permissions, instructions and a `config` map. Resume also has a `config` map. Neither is an arbitrary client process environment field. The experimental `environments` field selects execution environments; it is not a map of environment variables. [Thread protocol types](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-protocol/src/protocol/v2/thread.rs).

**Environment distinction.** The daemon inherits its startup environment, and connecting clients do not replace it. Shell-like tool processes have a configurable environment policy: start from inherited daemon variables, apply exclusions, insert `shell_environment_policy.set`, then apply any `include_only` filter. The correct configuration key is **`set`**, not `env_vars`. Codex additionally injects its thread ID. This supports command-level identity overrides but does not give per-client isolation of every service, model credential, MCP process, or daemon startup setting. [Daemon README](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/README.md), [configuration policy](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/config/src/shell_environment_policy.rs), [environment construction](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/protocol/src/shell_environment.rs), [core environment wrapper](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/core/src/exec_env.rs).

**Multiple connections.** App-server tracks subscribed connection IDs per thread and threads per connection. Thread-scoped outgoing requests are sent to that thread's connections. Ordinary server-request callbacks are consumed once; the first processed response removes the callback. Special verification requests enforce an owner. Flotilla should observe approvals without competing with the TUI to answer them. Pending ordinary thread requests can be replayed to a reconnecting connection. These are mechanisms, not live proof of our complete two-client behavior. [Thread state](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server/src/thread_state.rs), [outgoing messages and callback handling](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server/src/outgoing_message.rs).

**Shutdown and upgrades.** Daemon stop/restart is daemon-wide. It requests graceful termination then forces exit after configurable `shutdownGraceSeconds` (default 60, range 0–300). Eligible latest-channel managed packages can launch an updater; updates can restart a running daemon and interrupt active/queued work. Explicitly pinned package installations do not automatically advance. Vessel provisioning should pin the managed package or explicitly disable automatic updates, rather than assume the invoking CLI version controls the running daemon. [Daemon README, lifecycle and update cases](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/README.md).

## Proposed Flotilla arrangement — inference/design

The Codex harness implementation ensures the vessel daemon using Codex lifecycle JSON, connects through the first-party proxy, creates a thread for each crew, and returns a persisted thread binding plus the TUI attach command. The generic terminal runtime launches one ordinary cleat TUI session and handles harness-neutral results; it does not construct Codex thread RPCs, sockets or supervisor commands.

The vessel owns daemon lifetime; removing one crew must not stop a daemon used by another. Crew teardown archives its own thread; the candidate live probe verifies that the other crew remains available. Vessel retirement owns daemon shutdown. Sharing `CODEX_HOME` inside a contained vessel is acceptable; host-direct vessels must avoid accidentally adopting a personal or another vessel's daemon. A private vessel home is appropriate where namespace collisions require it, not a per-crew trust boundary.

Flotilla currently injects `FLOTILLA_CREW_ID`, `FLOTILLA_CREW_ROLE` and `FLOTILLA_TERMINAL_SESSION` into each crew process. CLI crew operations use that identity. Starting the shared daemon with the first crew's identity would make other threads' tools act as that crew. Supply the equivalent per-thread `shell_environment_policy.set` overrides, preserving existing policy and filters, and reapply them when necessary during resume. The live probes below validate this mechanism across initial execution, TUI attachment, resume, reconnect and daemon restart. [Runtime](../../crates/flotilla-daemon/src/runtime.rs), [CLI](../../src/main.rs), [crew operations](../../crates/flotilla-core/src/in_process/crew_ops.rs).

## Operator acceptance scope

1. Start/bootstrap from the actual crew-image package and vessel home; parse lifecycle JSON and check installed/running versions. Concurrent ensures must yield one daemon.
2. Create two threads with different cwd, model/policy where appropriate, and different Flotilla identity overlays. Run shell commands to print non-secret identity fields and call a harmless crew status operation; each must resolve its own crew. Repeat after TUI attach, thread resume, proxy reconnect and daemon restart.
3. Confirm explicit TUI attachment never falls back to an embedded server and connects to the thread Flotilla created.
4. Subscribe both the TUI and Flotilla. Observe approvals without auto-answering; show that a TUI decision resolves once. Exercise ordinary delivery receipts, urgent steering, reconnect reconciliation and independent thread event routing.
5. Stop one crew while the other remains active. Demonstrate no daemon-wide interruption. Retire the vessel and demonstrate daemon cleanup, including updater state if enabled.
6. Verify actual MCP/client-auth configuration required by our image separately. Shell identity overlays do not prove that thread-specific settings reach every long-lived service.

Keep these tests targeted and operator-run initially. Source-backed injected protocol scenarios should cover Flotilla queueing and receipt logic; large model-output recordings are not needed to prove the daemon ownership design.

## Live identity probe (2026-10-08)

`scripts/prove-codex-vessel-identity.py` bootstraps a private managed daemon using the installed complete npm Codex 0.160.0 package and a temporary copy of existing authentication. It disables automatic updating, gives the daemon deliberately wrong Flotilla identity, and starts two threads with distinct `shell_environment_policy.set` values. It demands actual command-execution events and Python-written files whose environment values match each crew; model prose is not accepted as evidence. The script removes authentication and stops its own daemon.

The full operator script passed with exit status 0 on Codex 0.160.0: initial two-thread execution, explicit attachment of two cleat TUIs, thread resume, websocket reconnect, and managed daemon restart. Each of five phases ran both crew threads concurrently, accepted only actual shell execution events plus independently inspected output files, and observed the correct three identity fields. Idempotent daemon start preserved its PID record. TUI attachment explicitly supplies the same identity policy via `-c`; this does not prove an unconfigured TUI would preserve the policy.

The restart phase initially produced completed turns without the requested files when resume supplied only identity config. Explicitly reapplying `approvalPolicy: never` and `sandbox: danger-full-access` on resume made the complete matrix pass. Therefore the harness must reapply its declared permissions as well as identity on resume; do not infer that an unloaded thread automatically retains every startup setting. This experiment demonstrates the required behavior, but does not isolate which omitted policy field caused the earlier failure. The script also correlates completion to the newly returned turn ID, rather than accepting any completion on the thread.

To repeat in a provisioned vessel:

```sh
uv run --with websockets python scripts/prove-codex-vessel-identity.py
```

Set `CODEX_HOME` to a logged-in home; optionally set `CODEX_BIN` and `CODEX_TRANSPORT_MODEL`. The script uses a scratch home, does not touch the operator's daemon/TUI, and disables automatic updating. Its small PASS report is acceptance evidence, not a replay fixture.

This proves functional shell-command identity routing for those phases, not isolation between crew and not per-thread authentication/MCP configuration. The daemon itself uses one shared `CODEX_HOME`; changing a shell child's `CODEX_HOME` is not a way to reconfigure Codex's own long-lived services.

## Candidate Rust adapter probe

The first live Rust-adapter launch on 2026-10-08 failed before publication:
`thread/read` with `includeTurns: true` returned `-32601: list_turns is not
supported yet`. The identity-only Python probe had not requested history, so
it could not establish this receipt-recovery path. The candidate now explicitly
selects `historyMode: legacy` when creating its threads. This retains durable
rollout-based receipt recovery instead of treating a successful input RPC as
acceptance or relying only on ephemeral notifications.

Codex 0.160 defaults persistent threads to paginated history when the store
advertises that capability. Its loaded-thread read path delegates paginated
history to `list_turns`, while legacy history loads rollout items. These paths
are visible in [the pinned thread processor](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server/src/request_processors/thread_processor.rs).
The explicit mode is part of the harness's protocol contract, not a generic
Message transport concern. The corrected candidate passed durable native receipt recovery in the live probe.

The second and instrumented third Rust probes reached both crew launches, but
the first crew's connection was disconnected when the second launched. This
was a candidate implementation bug: bootstrap was called for every crew.
`bootstrap_locked` in [the pinned daemon implementation](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-daemon/src/lib.rs)
explicitly stops a running backend before starting it. Only `daemon start` is
idempotent. The candidate now serializes initial package selection using
`flock -o` in its private home, bootstraps only when its managed package is
absent, and otherwise uses start. Closing the inherited lock descriptor avoids
leaving the detached daemon holding the installation lock. The fourth probe passed with exit status 0: two adapter launches, independently
verified tool identities, one shared home and separate threads, native proxy
reconnect, independent crew retirement, and final vessel daemon shutdown.
This is candidate Rust implementation evidence, beyond the direct-protocol
identity proof.

The expanded candidate probe also passed actual role-material selection: each
thread executed a command defined only in its staged role skill, and independently
written files contained the role-specific source configuration and all three
production identity variables. Ordinary Message delivery and urgent active-turn
steering each produced a correlated native receipt and their required tool-written
files. Structured turn events were observed. A separate read-only thread emitted
a structured approval event and NeedsInput attention; the requested filesystem
operation remained unexecuted, and Flotilla did not answer the approval.

These probes do not exercise a full fleet image or actual Flotilla crew-status
resolution, nor an interactive TUI approval decision. Those remain operator
acceptance checks; the identity proof uses the exact production variables and
independently checks their command-level values.

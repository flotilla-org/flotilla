# Contained-vessel plumbing: how far is it from working?

**Date:** 2026-07-27

**Issue:** [#1117](https://github.com/flotilla-org/flotilla/issues/1117)

**Status:** Code-reading research, not a fix proposal

## Executive summary

A contained vessel is **not currently end-to-end functional**. The code is
closer than a failed probe convoy would suggest, but it has two independent
hard blockers before an agent can be attached:

1. interior provider discovery has the right `docker exec` runner but does not
   run the host detectors that produce binary assertions, so no Codex or Claude
   agent adapter is registered inside the container; and
2. `worktree_on_host_and_mount` deliberately requests a read-write mount, but
   the runtime discards the requested mode and the Docker provider hard-codes
   every provisioned mount as read-only.

The attach link after those blockers is substantially present. The current
control-plane `flotilla attach` path resolves a Docker-backed, passthrough
`TerminalSession` to a structured hop chain whose flattened shape is:

```text
docker exec -it -w '/workspace' flotilla-env-env-<vessel> <agent launch command>
```

Passthrough is intentional here: it has no durable session to reattach to.
Provisioning records the launch command without starting it, and attachment
starts the command in the container. The hop-chain representation replaces the
old hand-built multiply quoted SSH strings. It has focused quoting tests, but
there is no real-Docker, real-TTY test of this exact current control-plane
attach path, so the final classification is **probably works**, not **works**.

| Link | Verdict | Short reason |
|---|---|---|
| Docker hop in the hop-chain abstraction | **works** | `EnterEnvironment` supports both architecture-level forms: interactive attach enters `docker exec -it ... /bin/sh` and sends the inner attach one boundary at a time; non-interactive execution wraps the structured command with `docker exec -it`, including `-w` for the interior cwd. |
| Current crew attach with `pool: passthrough` | **probably works** | The control-plane attach path supplies the container id and resolves the passthrough launch command through the Docker hop; only synthetic/unit coverage proves it. |
| Hop-chain quoting | **probably works** | Quoting is centralized in the structured `Arg` tree and handles nested commands and apostrophes; the already-built agent command remains trusted raw shell. |
| `worktree_on_host_and_mount` writable workspace | **broken** | The reconciler asks for `Rw`, the runtime erases the mode, and Docker emits `:ro`. |
| Container uid/gid remapping | **absent** | There is no user field and no Docker `--user`/uid/gid plumbing. The image's configured user is used. |
| Interior command execution | **works** | `DockerEnvironmentRunner` consistently delegates through `docker exec`, including cwd and stdin-aware file writes. |
| Interior host discovery | **absent** | Provisioning reads interior env vars but never runs the existing host detectors against the interior runner. |
| Declared-adapter verification at admission | **absent** | Admission compares requirements directly with `docker_per_vessel.agent_adapters`. |
| Interior adapter availability at session start | **broken** | Runtime requires an actually discovered adapter, but the provisioned environment bag contains no binary assertions from which to register one. |

Credentials are deliberately excluded, per #1118.

## 1. Attach into a Docker placement

### Verdict: probably works on the current control-plane path

The hop-chain model explicitly supports an environment hop:
`Hop::EnterEnvironment { env_id, provider }`
(`crates/flotilla-core/src/hop_chain/mod.rs:15-22`). Resolution walks
inside-out, so the terminal or launch command is resolved first and the
environment resolver then wraps it
(`crates/flotilla-core/src/hop_chain/resolver.rs:32-87`).

`DockerEnvironmentHopResolver::resolve_wrap` constructs structured arguments:

```text
docker exec -it [-w <interior cwd>] <container> <inner args...>
```

The cwd is consumed as a container-local path and passed with `docker exec -w`,
not interpreted on the host
(`crates/flotilla-core/src/hop_chain/environment.rs:33-55`). Unit coverage also
exercises the full remote → environment → terminal ordering and asserts the
nested `ssh ... docker exec ... attach` tree
(`crates/flotilla-core/src/hop_chain/tests.rs:1242-1283`).

There are two attach paths in the repository, and only one is relevant to a
current resource-backed crew:

- The older Plane-A `TerminalManager::attach_command` builds an environment hop
  from an attachable set, but installs `NoopEnvironmentHopResolver`; an
  attachable with `environment_id` therefore errors during resolution. It also
  explicitly rejects remote attachables
  (`crates/flotilla-core/src/terminal_manager.rs:165-212`). This path does **not**
  prove contained crew attachment.
- The current `TerminalSession` path looks up the session's `Environment`,
  selects a provider registry for that environment, obtains the pool's
  `attach_args`, and, for Docker environments, calls
  `resolve_prepared_commands_via_hop_chain` with both the environment id and
  `Environment.status.docker_container_id`
  (`crates/flotilla-core/src/in_process.rs:5790-5818`,
  `crates/flotilla-core/src/in_process.rs:5856-5889`). That helper installs a
  real `DockerEnvironmentHopResolver`, inserts `EnterEnvironment` immediately
  outside `RunCommand`, and flattens the single resulting action
  (`crates/flotilla-core/src/executor/workspace.rs:327-391`).

### What passthrough means for a crew

The daemon seeds Docker placement policies with `pool: passthrough` when that
pool is available (`crates/flotilla-daemon/src/runtime.rs:248-273`). The
passthrough pool:

- reports no sessions;
- treats `ensure_session` as a no-op; and
- returns the supplied launch command from `attach_args`
  (`crates/flotilla-core/src/providers/terminal/passthrough.rs:7-47`).

The terminal controller nevertheless records the terminal as `Running`, with
the generated agent command in `status.launch_command`, after
`pool.ensure_session` returns
(`crates/flotilla-daemon/src/runtime.rs:1277-1353`). Attachment later reads that
recorded command through `terminal_session_attach_target`
(`crates/flotilla-resources/src/terminal_session.rs:62-82`) and wraps it in
`docker exec`.

Therefore a Docker/passthrough crew attach is not “attach to an already running
session”. It is “start the recorded crew command in a foreground
`docker exec -it`”. This also means a passthrough `TerminalSession` can say
`Running` before any agent process exists; its liveness is intentionally not
tracked (`crates/flotilla-daemon/src/runtime.rs:1356-1362`).

### Quoting: structured now, but not pure argv

Before the hop-chain change, remote attachment assembled nested strings with
several `format!` calls, a local `shell_quote`, and a separate
`escape_for_double_quotes`, ultimately producing:

```text
ssh -t ... '<login shell containing "cd ... && <command>">'
```

That implementation is visible immediately before commit `49d279fb`
(`crates/flotilla-core/src/executor/terminals.rs` at that revision, lines
173-245). Commit `49d279fb` replaced it with the structured hop chain.

Current quoting has one centralized representation:

- `Literal` is trusted raw shell at the current depth;
- `Quoted` is single-quoted with embedded apostrophes escaped; and
- `NestedCommand` is recursively flattened and quoted as one argument
  (`crates/flotilla-protocol/src/arg.rs:5-20`,
  `crates/flotilla-protocol/src/arg.rs:46-72`).

Tests cover spaces, shell metacharacters, embedded apostrophes, multiple nested
commands, SSH login-shell wrapping, and the former remote-attach shape
(`crates/flotilla-protocol/src/arg.rs:74-279`). The hop chain is consequently
not the old ad hoc multiply escaped string builder.

It is still deliberately a shell-fragment model rather than a universal argv
model. In particular, passthrough inserts the already-rendered agent launch
command as one `Arg::Literal`
(`crates/flotilla-core/src/providers/terminal/passthrough.rs:26-41`). That
command is produced by trusted adapter code using the same flattener
(`crates/flotilla-core/src/agent_adapter.rs:416-428`), so user-controlled brief
text remains quoted, but a future producer that places untrusted content in a
`Literal` would violate the model's stated safety invariant.

### Smallest experiment

Create a disposable Docker `Environment` plus a synthetic `Running`
passthrough `TerminalSession` whose launch command and cwd contain spaces and an
apostrophe. Resolve it through `flotilla attach`, then execute the returned
command from a real TTY and have the inner command print argv and cwd. This
settles the only remaining uncertainty: shell parsing plus real `docker exec
-it` behavior at the CLI/TUI execution boundary.

## 2. Worktree mount identity and writeability

### Verdict: broken before uid/gid becomes relevant

The vessel reconciler deliberately creates the host-worktree mount as
`EnvironmentMountMode::Rw`
(`crates/flotilla-controllers/src/reconcilers/vessel.rs:527-563`). This is not
an accidental reliance on a root container at the resource-model layer.

The mode is then lost:

1. `DockerControllerRuntime::provision` maps every `EnvironmentMount` to a
   `ProvisionedMount` containing only source and target paths; it never reads
   `mount.mode` (`crates/flotilla-daemon/src/runtime.rs:899-929`).
2. `DockerEnvironmentProvider::create` formats every such mount as
   `<host>:<container>:ro`
   (`crates/flotilla-core/src/providers/environment/docker.rs:65-115`).

So the current `/workspace` bind mount is unconditionally read-only despite the
resource saying `Rw`. This is a hard failure, not a silent degradation. Agent
session creation calls `adapter.prepare` before passthrough `ensure_session`;
every adapter writes the crew brief under the workspace, and Claude also writes
managed settings (`crates/flotilla-daemon/src/runtime.rs:1293-1307`,
`crates/flotilla-core/src/agent_adapter.rs:431-453`). The Docker runner
propagates a failed atomic file write as an error
(`crates/flotilla-core/src/providers/environment/runner.rs:70-74`), the terminal
reconciler marks the session `Failed`, and the vessel propagates that failure
(`crates/flotilla-controllers/src/reconcilers/terminal_session.rs:133-158`,
`crates/flotilla-controllers/src/reconcilers/vessel.rs:608-626`).

Git history explains why older behavior is not a reliable guide. General
provisioned mounts were introduced in `b66b14d1` with `:ro` hard-coded. The
later task/vessel reconciler requested `Rw`, but no mode reached the older
provider abstraction. Reading the present tree and that transition proves the
current break; it cannot establish which unmerged or earlier Plane-A dogfood
configuration produced the reported successful mount.

### Verdict: uid/gid remapping is absent

The resource types expose image, mounts, and env, but no container user
(`crates/flotilla-resources/src/environment.rs:23-31`). Docker creation emits
no `--user`, uid, gid, `--userns`, or ownership adjustment; the full argument
construction is in
`crates/flotilla-core/src/providers/environment/docker.rs:65-113`. The process
therefore runs as the image's configured user (root if the image has no
non-root `USER`). Nothing in Flotilla aligns that identity with the owner of the
host checkout.

If the mount mode were correctly carried as read-write, a uid mismatch would
not cause Docker to silently convert the mount to read-only. Ordinary host
inode permissions would decide each operation: a sufficiently privileged
container user may write, while an unmatched unprivileged user generally gets
a permission error. The exact outcome depends on host permissions, Docker
user-namespace/rootless configuration, and the image's configured user, none
of which this model records. In the current code those distinctions are masked
by the unconditional `:ro`.

### Smallest experiment

No experiment is needed to establish the current read-only bug:
`docker inspect flotilla-env-...` and checking `.Mounts[].RW` would merely
confirm the emitted `:ro`. After mode propagation exists, the smallest identity
experiment is one bind-mounted temporary worktree and two `docker exec` calls:
print `id`, then create and replace a file as the image default user and as a
deliberately mismatched uid. That separates image-user behavior from
user-namespace behavior without launching a convoy.

## 3. Interior discovery and declared adapters

### Verdict: the execution substrate works

The injected-runner premise from Plane A remains intact.
`DockerEnvironmentRunner` decorates every `CommandRunner` operation with
`docker exec`, passes the interior cwd with `-w`, adds `-i` for stdin, and
implements file writes inside the container
(`crates/flotilla-core/src/providers/environment/runner.rs:11-75`). A
provisioned Docker handle exposes this runner
(`crates/flotilla-core/src/providers/environment/docker.rs:192-200`,
`crates/flotilla-core/src/providers/environment/docker.rs:252-284`).

The generic command detector is also compatible with it: detectors receive an
injected `CommandRunner`, and `CommandDetector` invokes the requested binary
through that runner
(`crates/flotilla-core/src/providers/discovery/mod.rs:368-405`,
`crates/flotilla-core/src/providers/discovery/detectors/generic.rs:29-51`).
In principle the existing detector list can probe `codex --version`,
`claude --version`, `cleat --version`, and the other interior tools
(`crates/flotilla-core/src/providers/discovery/detectors/mod.rs:11-33`).

### Verdict: the current interior-discovery orchestration is broken

Both current provisioned-environment probe functions do only this:

1. execute `env` inside the container;
2. turn those values into `EnvVarSet` assertions; and
3. call `FactoryRegistry::probe_all` with the interior runner.

They do **not** call `run_host_detectors` with that runner
(`crates/flotilla-core/src/environment_manager.rs:402-420`,
`crates/flotilla-daemon/src/runtime.rs:952-965`).

`probe_all` does not discover binaries itself. It asks
`AgentAdapterRegistry::discover` to consume the supplied bag
(`crates/flotilla-core/src/providers/discovery/mod.rs:434-493`), and that
registry only installs Claude or Codex when `find_binary("claude")` or
`find_binary("codex")` succeeds
(`crates/flotilla-core/src/agent_adapter.rs:570-600`). Because the provisioned
bag contains only env-var assertions, an image can contain a working Codex
binary and still get no Codex adapter.

Session startup later requires the actual interior registry to contain the
adapter and fails loudly when it does not
(`crates/flotilla-daemon/src/runtime.rs:1293-1301`). Thus runtime does not
blindly execute a declared adapter, but its attempted concrete check is wired
to an incomplete discovery pass.

### Verdict: admission verification is absent

Admission treats `docker_per_vessel.agent_adapters` as authoritative. For a
Docker policy, `placement_agent_adapters` returns the declared set directly;
only host-direct placement reads observed host capabilities
(`crates/flotilla-core/src/in_process.rs:3385-3449`). Default Docker policies
are currently seeded with an empty declared set
(`crates/flotilla-daemon/src/runtime.rs:456-471`), so the seeded
`single-agent-contained` workflow cannot use that untouched default policy.
Manually declaring `codex` makes admission pass whether or not the image
contains Codex.

The pieces needed to verify declarations already exist, but verification cannot
happen at initial admission without provisioning or consulting a separately
cached image capability observation. ADR 0007's requirements-first resolver
will eventually replace this transitional named-policy plumbing; it does not
change the current evidence
(`docs/adr/0007-requirements-first-placement.md:47-58`).

### Smallest experiment

Use an image known to contain Codex, provision one environment through the
existing Docker runtime, and inspect
`environment_registry_for_environment(...).agent_adapters.ids()`. The expected
current result is empty. Running the existing `default_host_detectors` against
the same handle's runner should then produce a `BinaryAvailable("codex")`
assertion. This isolates the missing orchestration call without involving
placement, credentials, a checkout, or crew launch.

## End-to-end conclusion

The plumbing is not one monolithic missing feature:

1. **Admission** can select a contained policy only by trusting its declared
   adapters; the seeded default declares none.
2. **Environment creation and interior command transport** are present.
3. **Interior discovery** omits host detectors, so required adapters are absent
   from the environment registry.
4. **Workspace mounting** asks for read-write but receives read-only, so agent
   preparation would fail even if discovery succeeded.
5. **Crew attach** is already shaped correctly for passthrough and Docker and is
   no longer based on the old ad hoc quoting code, but it is downstream of both
   blockers.

Accordingly, a real probe convoy today would most likely fail at admission
(empty policy declaration), then at interior adapter lookup if the declaration
were populated, then at the first workspace write if discovery were bypassed.
It would not reach the attach link that is most likely already usable.

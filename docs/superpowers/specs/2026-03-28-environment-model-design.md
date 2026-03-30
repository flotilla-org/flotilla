# Environment Model

**Status:** Design — ready for implementation planning after review.
**Related:** #500 (HostName FQDN collisions), #552 (environments in UI/correlation), service host resolution spec (2026-03-26), node identity spec (2026-03-28)

## Problem

`HostName` conflates daemon mesh identity with execution environment identity. Discovery probes "the host" for available binaries, env vars, and sockets. Steps execute "on a host." But the local machine, an SSH target, and a Docker container are all execution environments — they just have different reachability.

The current model forces a separate daemon on every machine you want to use as an execution target. The `EnvironmentId` type already exists for Docker containers but isn't used for the local machine or static SSH targets. These are all the same thing — execution contexts that a node can reach and probe.

This spec unifies them under a single environment model. It is the first of two specs:

1. **This spec** — Environment model: host/environment relationship, path namespacing, discovery, execution context.
2. **Node identity spec** (separate document) — Cryptographic daemon identity, replacing `HostName` in mesh internals. Also defines the `<machine-id>` resolution order (Linux `/etc/machine-id`, macOS `IOPlatformUUID`, config fallback) used by both specs for NFS-safe path scoping.

An issue (#552) for UI/correlation updates (making environments observable) sits between the two.

## Core Concepts

### Host

A **host** is a machine — the daemon's local machine, a remote machine reachable via SSH, etc. Hosts persist. They have a filesystem. They are identified by a `HostId` — a UUID that **lives on the machine itself** at `~/.local/share/flotilla/<machine-id>/host-id` (scoped by machine-id, same as EnvironmentId and node keypair, to prevent NFS-shared-home collisions). Generated on first use (either by the local daemon or by a remote controller's first probe via SSH), then read on subsequent access. Any controller reaching the same machine gets the same `HostId`. This avoids the config-label instability problem — two controllers managing the same SSH target under different labels still see the same `HostId`.

**Atomic creation:** To prevent races when two controllers probe a pristine machine concurrently, generation uses an atomic create-or-read pattern. Since this runs via `CommandRunner` (potentially over SSH), it must use portable shell primitives:

```sh
mkdir -p "$(dirname "$file")" && (set -C; echo "$uuid" > "$file") 2>/dev/null || cat "$file"
```

`set -C` (noclobber) prevents overwriting an existing file. If the write fails because another writer created the file first, we read it back. This is portable across sh/bash/zsh on Linux and macOS and works over SSH.

A host **has-a**:
- A **direct environment** — the host's own execution context. Can be suppressed (`suppress_local_environment`). Paths in the direct environment are the host's own filesystem paths (no translation needed).
- Zero or more **provisioned environments** — Docker containers, future cloud VMs, etc. These have their own path namespaces, their own runners (wrapping the host's runner), and their own lifecycle.

The host concept is slimmed down from today — it loses its role as mesh identity (that moves to NodeId) and its role as the sole execution target (environments take over). But it persists as the stable, physical layer that environments are rooted in.

### Environment

An **environment** is an execution context with a `CommandRunner`, an `EnvVars` set, and a set of discoverable tools.

- Identified by a globally unique **`EnvironmentId`** — generated at provisioning or first-start, never derived from ambient state.
- A direct environment shares its host's filesystem and runner.
- A provisioned environment (Docker, etc.) has its own path namespace and a wrapping runner (e.g. `EnvironmentRunner` that translates commands to `docker exec`).
- All environments are probed through the same discovery pipeline: `CommandRunner` + `EnvVars` injection → detectors → `EnvironmentBag` → factory probing.

### Relationship Between Host and Environment

```
Host ("laptop")
  ├─ Direct environment (paths = host paths, runner = host runner)
  ├─ Docker container "dev-1" (own path namespace, EnvironmentRunner wraps host runner)
  └─ Docker container "dev-2" (own path namespace, EnvironmentRunner wraps host runner)

Host ("desktop", managed via SSH from homelab daemon)
  ├─ Direct environment (paths = host paths, runner = SSH CommandRunner)
  └─ Docker container "build-box" (own path namespace, EnvironmentRunner wraps SSH runner)
```

A daemon does not have to be local to a host — a remote host managed via SSH has the same structure. The daemon reaches the host via an SSH `CommandRunner`, and the host's direct environment uses that runner. Provisioned environments on that host wrap the same SSH runner with their container-specific translation.

## Path Namespacing: QualifiedPath

### The Problem

Paths exist in different namespaces:
- `/home/dev/repo` on laptop is a different checkout from `/home/dev/repo` on desktop
- `/workspace/repo` inside a Docker container is yet another namespace
- A bind-mounted checkout is the *same underlying code* as the host path it's mounted from

The current `HostPath(HostName, PathBuf)` handles the first case. `EnvironmentId` handles the second. But there's no unified key and no way to express the bind-mount relationship.

### QualifiedPath

```rust
struct QualifiedPath {
    qualifier: PathQualifier,
    path: PathBuf,
}

enum PathQualifier {
    Host(HostId),
    Environment(EnvironmentId),
}
```

`QualifiedPath` replaces `HostPath` as the checkout identity key. The qualifier determines which namespace the path lives in.

### Normalize to Most Persistent Qualifier

**Rule:** At discovery/publication time, normalize paths to the most persistent qualifier available.

- A checkout discovered inside a Docker container via a bind mount should be published as `QualifiedPath(Host("laptop"), "/home/dev/repo")` — not as an environment-qualified path. The host path is the persistent identity; the container is transient.
- A checkout that exists only on container-local storage (no bind mount) is published as `QualifiedPath(Environment("container-id"), "/workspace/repo")`. This accurately reflects that the checkout lives and dies with the container.
- A checkout on a direct environment uses `QualifiedPath(Host("desktop"), "/home/dev/repo")` — the direct environment's paths *are* host paths.

This normalization happens once, at discovery time. The rest of the system (correlation, merge, display) sees `QualifiedPath` as an opaque key and doesn't need mount translation logic.

### Direct Environments and Host Paths

A key property of an environment: **can it use its containing host's paths?**

- **Direct environment:** Yes, always. Paths are host paths. No translation needed in either direction.
- **Provisioned environment:** No, unless explicitly mapped. Bind mounts create a translation table between container paths and host paths. The discovery pipeline uses this table to normalize checkout paths to host-qualified form when possible.

### Execution Inside Environments

Commands targeting a provisioned environment execute *inside* the environment via hop chains and `CommandRunner` wrapping (e.g. `EnvironmentRunner` translates commands to `docker exec`). When executing inside the environment, paths in command arguments are naturally environment-local — the shell fragments, working directories, and file references are written from the environment's perspective.

The normalization rule (normalize to most persistent qualifier at discovery time) applies to **identity and correlation only** — it determines which `QualifiedPath` key represents a checkout in `ProviderData`, correlation, and merge. It does not mean that execution-time paths need rewriting.

**Structured path fields:** The narrow case where translation matters is when a step's structured fields (e.g. `cwd`, `repo_path`) reference the normalized host-qualified path but execution targets a provisioned environment. For these fields:

1. The `Checkout` struct carries both its `QualifiedPath` (normalized identity) and the `environment_id` it was discovered in.
2. The mount table (from container bind mount configuration) maps `(host_path_prefix, environment_internal_prefix)` pairs.
3. The executor rewrites structured path fields using longest-prefix matching against the mount table before dispatch.
4. If no mount entry matches a required path, the step fails: the checkout is not accessible from that environment.
5. For direct environments, no translation is needed (paths are already host paths).

The mount table is populated at environment provisioning time and is immutable for the lifetime of the environment.

**Opaque shell fragments:** `Arg::Literal` and similar opaque command payloads are not rewritten. In practice this is fine — current step variants use structured path fields for checkout paths and working directories, while `Arg::Literal` is used for attachment commands (tmux/shpool attach strings) that don't embed checkout paths. If future step types embed host-qualified paths into opaque shell fragments targeting provisioned environments, those would need to be refactored to use structured path fields. This is a future invariant to enforce when relevant, not a current design gap.

**Future direction:** A recursive `flotilla attach` model (resolve one hop, then call flotilla again at that hop) would eliminate most translation concerns — each hop runs in its own context with the right paths naturally.

### Correlation Impact

`CorrelationKey::CheckoutPath(HostPath)` becomes `CorrelationKey::CheckoutPath(QualifiedPath)`.

Because paths are normalized to the most persistent qualifier at discovery time, correlation "just works" — two references to the same bind-mounted checkout both resolve to the same host-qualified path. No mount-table lookups needed during correlation.

The qualifier also communicates persistence semantics:
- `Host`-qualified = durable, survives container lifecycle
- `Environment`-qualified = ephemeral, lost when the environment is destroyed

### Merge Validation

Currently: "a peer can only claim checkouts for its own `HostName`."

With the new model: a node manages multiple hosts and environments. Merge validation uses a **deterministic ownership** rule per checkout:

1. The receiver maintains `HashMap<QualifiedPath, NodeIdentity>` — the ownership map, keyed by the full checkout identity (qualifier + path), not just the qualifier.
2. When a node claims a specific checkout, it becomes the owner if no other node has claimed that exact `QualifiedPath`.
3. If two nodes claim the same `QualifiedPath` (e.g. after a network partition heals, or two controllers managing the same host), the node with the **lexicographically lower NodeId** wins. This is deterministic and requires no coordination — every node in the mesh reaches the same conclusion independently.
4. For cloud-provisioned environments, the leader for that cloud endpoint is the owner.

This is per-checkout, not per-host: two nodes can legitimately manage different repos on the same host, each owning their own checkouts. The deterministic tiebreaker on NodeId avoids timing dependencies — arrival order doesn't matter.

## EnvironmentId Generation

`EnvironmentId` values are globally unique and never derived from ambient state (`EnvironmentId::local()` or similar static constructors are forbidden).

**The ID lives on the machine it identifies**, not on the controller that manages it. This ensures that any controller reaching the same machine learns the same `EnvironmentId`.

- **Daemon's own machine (direct environment):** A UUID generated on first daemon start, stored at `~/.local/share/flotilla/<machine-id>/environment-id` on the local machine (scoped by machine-id, same as the node keypair, to prevent NFS-shared-home collisions). Read on subsequent starts.
- **Static SSH targets (direct environment):** On first probe via SSH, the controller reads `~/.local/share/flotilla/<machine-id>/environment-id` on the remote machine (where `<machine-id>` is the remote machine's own machine-id, resolved per the node identity spec's machine-id resolution order). If it doesn't exist, the controller generates a UUID and writes it using the same atomic create-or-read pattern as HostId (write temp, rename-if-absent, re-read on conflict). Any other controller that later probes the same machine reads the same ID.
- **Docker containers:** Already generated at provisioning time and passed into the container. No change.

## Configuration

Static hosts and their environments are configured in `daemon.toml`:

```toml
# ~/.config/flotilla/daemon.toml
suppress_local_environment = true

[environments.nas]
hostname = "robert@nas.local"             # SSH destination
flotilla_command = "flotilla"             # used for probing, not daemon bootstrap
display_name = "NAS"

[environments.desktop]
hostname = "robert@desktop.local"
display_name = "Desktop"
```

These are probed on startup (and re-probed on refresh) using an ephemeral `CommandRunner` over SSH — the same mechanism used for Docker containers but with SSH as the transport.

**`suppress_local_environment`**: When true, the daemon skips environment detection for its own machine. No binary probing, no provider construction for local tools. The daemon exists as a mesh participant managing remote environments and routing commands. Intended for headless controller nodes (e.g. a Proxmox LXC that should not be a development target).

**`flotilla_command`**: Path or command used to run probe commands on the remote side. Not required for probing (probing uses the injected `CommandRunner` directly), but available if the remote needs a specific binary path for flotilla-specific operations. Defaults to `"flotilla"`.

## Discovery Pipeline Changes

The current discovery pipeline runs once for the daemon's local machine:

```
Host detection (one-shot at startup)
  → EnvironmentBag (host-level assertions)
    → Per-repo: repo detection → merged bag → factory probing → providers
```

This becomes per-environment:

```
For each host (local machine, SSH targets):
  For each environment (direct env, provisioned containers):
    Environment detection (CommandRunner + EnvVars injection)
      → EnvironmentBag (environment-level assertions)
        → Per-repo: repo detection → merged bag → factory probing → providers
```

The `HostDetector` trait doesn't need renaming (it detects what's available on a host/environment — the name is fine as a verb). The key change is that it runs per-environment with injected collaborators, not once for the daemon's own machine.

### What Changes in Discovery

- `DiscoveryRuntime` gains awareness of multiple environments. Currently it holds one set of host assertions — it needs to hold a set per `EnvironmentId`.
- `run_host_detectors()` takes a `CommandRunner` + `EnvVars` (already does) and an `EnvironmentId` to associate results with.
- `discover_providers()` receives the environment-scoped bag for the relevant environment.
- Factory probing is unchanged — factories already receive an `EnvironmentBag` and don't care where it came from.

### Provider Attribution

Providers are currently attributed to "this daemon." With multiple environments, each provider instance is scoped to an `EnvironmentId`:

- A `git` provider discovered on the NAS environment is distinct from `git` discovered locally.
- `ProviderData` gains an `environment_id: EnvironmentId` field (or the existing structure is keyed by environment).
- This feeds into the service directory: `ServiceInstanceKey → Vec<(HostName, EnvironmentId)>` (where `HostName` is the daemon managing it — replaced by `NodeId` in the node identity spec).

## Execution Context Changes

`StepExecutionContext` evolves to use `EnvironmentId`:

```rust
enum StepExecutionContext {
    /// Run in a specific environment on a specific daemon.
    /// Post-resolution only — planners don't construct this directly
    /// unless the target is unambiguous.
    Resolved { host: HostName, environment: EnvironmentId },
    /// Run on whichever daemon+environment serves this service.
    Service(ServiceInstanceSelector),
    /// Run in a specific environment. Daemon resolved at dispatch.
    Environment(EnvironmentId),
}
```

Note: `Resolved` still uses `HostName` for the daemon identity — this becomes `NodeId` in the node identity spec. The important change here is that the execution *target* is an `EnvironmentId`, not a hostname.

### What Was HostName (Execution Context Uses)

These uses of `HostName` become `EnvironmentId`:

| Current use | Becomes |
|---|---|
| `StepExecutionContext::Host(HostName)` | `StepExecutionContext::Environment(EnvironmentId)` |
| `ServiceScope::Host(HostName)` | `ServiceScope::Environment(EnvironmentId)` |
| Service directory values | `(HostName, EnvironmentId)` pairs (HostName → NodeId in later spec) |
| Discovery/probing target | `EnvironmentId` |
| Provider attribution | Scoped to `EnvironmentId` |
| `HostPath` (host + path namespace) | `QualifiedPath` (host or environment + path) |

Uses of `HostName` for **mesh identity** (peer maps, vector clocks, routing, message origin) are unchanged by this spec — they are addressed by the node identity spec.

## EnvironmentSummary

The current `HostSummary` captures system info and tool inventory for one daemon. With multiple environments per daemon, this needs to reflect per-environment state:

```rust
/// Summary of a single execution environment's capabilities.
struct EnvironmentSummary {
    environment_id: EnvironmentId,
    display_name: String,
    kind: EnvironmentKind,   // Direct, Docker, etc.
    system: SystemInfo,
    inventory: ToolInventory,
    providers: Vec<ProviderStatus>,
}
```

The existing `HostSummary` evolves to carry the daemon's identity plus its hosts and environments:

```rust
struct HostSummary {
    host_name: HostName,              // daemon identity (becomes NodeId in later spec)
    display_name: String,
    hosts: Vec<HostInfo>,             // managed hosts, each with their environments
}

struct HostInfo {
    host_id: HostId,
    display_name: String,
    direct_environment: Option<EnvironmentSummary>,   // None if suppressed
    provisioned_environments: Vec<EnvironmentSummary>,
}
```

This replaces the flat `SystemInfo` + `ToolInventory` on `HostSummary` with a host→environment hierarchy.

## The Solo Deployment

The motivating use case: a developer with a laptop and an always-on home server.

```
┌─────────────────────────────┐     ┌──────────────────────────────┐
│ Laptop Daemon               │     │ Homelab Daemon               │
│                             │     │ suppress_local_environment   │
│ Hosts:                      │◄───►│                              │
│   - laptop (local)          │mesh │ Hosts:                       │
│     └─ direct env           │     │   - homelab (local, no env)  │
│                             │     │   - desktop (SSH)            │
│                             │     │     └─ direct env            │
│                             │     │   - nas (SSH)                │
│                             │     │     ├─ direct env            │
│                             │     │     └─ dev-container (Docker)│
└─────────────────────────────┘     └──────────────────────────────┘
```

- Laptop runs its own daemon, manages its local host with direct environment.
- Homelab runs a daemon with `suppress_local_environment` (it's a controller, not a dev target). Manages the desktop and NAS as remote hosts, each with their own environments.
- Laptop connects to homelab when at home or via VPN.
- Homelab keeps running when laptop sleeps — agents, refreshes, state replication all continue.

### Client Socket on Managed Hosts

For CLI use on a managed host (e.g. SSHed into the desktop, want `flotilla` commands to work):

- The homelab daemon's SSH session to the desktop can reverse-forward the daemon socket.
- A persistent session (cleat/shpool) on the managed host keeps the forward alive.
- The local `flotilla` CLI on the desktop discovers the forwarded socket and connects.
- This is opportunistic — if the forward isn't up, the CLI reports that no daemon is reachable.

## What This Does Not Cover

- **Cryptographic node identity.** Covered by the node identity spec. This spec retains `HostName` for daemon identity in the mesh as a temporary measure.
- **UI/correlation updates.** Making environments observable in the TUI. Tracked as #552 to be brainstormed.
- **Automatic environment discovery.** mDNS, cloud provider APIs, etc. Future work. The model supports it but initial implementation is config-driven.
- **Environment lifecycle management.** Starting/stopping managed environments, health checks, automatic reprobing. Future work — the initial model is "probe on startup/refresh."

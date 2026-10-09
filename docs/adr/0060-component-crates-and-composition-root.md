# 60. Component crates and the composition root

**Status:** Accepted (owner grill, 2026-10-09; #2749)
**Date:** 2026-10-09
**Amends:** the VCS operation boundary rule in CLAUDE.md (CLI is no longer the universal implementation); the #2222 ruling that command handlers live in `flotilla-commands`
**Supersedes:** the two-layer providers/orchestration cut proposed on #2749
**Relates to:** ADR 0047 (stored data stays decodable for one generation), ADR 0058 (environment providers), ADR 0059 (agent sessions, processes and channels), ADR 0029 (the leaf engine), #2747 (build graph check), #2948 (test support), #2953 (store query pushdown)

## Context

On 2026-10-09, after the day's splits (#2947, #2949, #2954, #2956, #2960, #2961), main at faf399ac still had one strongly connected group of 25 modules in `flotilla-core`: 47,053 non-test lines covering providers, VCS, charter, agent adapters, the observers and orchestration. Four edges point back into `in_process`, and three point up out of providers. The two crates split out that day, `flotilla-credentials` and `flotilla-aggregator`, still depend on core; the aggregator holds an `Arc<InProcessDaemon>`. The client compiles core for about 240 lines it needs. `ResourceBackend` is a closed enum, so every user of resources, the client included, links SQLite and an HTTP stack. And about 7,000 lines of test support are compiled into release builds through default-on features.

The owner's direction is to break flotilla into components so it isn't committed to one sandbox kind, orchestration style, persistence posture, or tool-invocation style. Agents work well inside a component whose dependencies and dependees are clearly delineated, and go wrong when the structure looks fluid. Splits are not judged by how few imports they change.

## Decision

### Principles

1. **Components, not layers.** A component depends only on API and type crates and on traits it owns. It never depends on the composition root or on an aggregate crate. Anything it needs from above becomes a port that the lower crate owns and the composition root implements.
2. **Dyn for services, generics for values.** Service traits are object-safe. Typed convenience APIs are extension layers over an object-safe core.
3. **Postures are composition choices.** These are all chosen by the composition root, not encoded in the crate structure:
   - the store backend;
   - replicated, central or federated persistence;
   - a message broker versus resource-backed messaging;
   - CLI versus library implementations.
4. **No test code in production.** Shared test helpers live in testkit crates used only as dev-dependencies. Production crates have no `test-support` or `replay` features, and tokio `test-util` appears only in dev-dependencies.
5. **Enforced by the build-graph check (#2747):**
   - no component depends on the composition root or an aggregate crate;
   - no production crate depends on a testkit;
   - one shared build graph across the documented commands.

### Target crates

- **Base:**
  - `flotilla-protocol`;
  - `flotilla-resources`, holding kind types, resolvers and lifecycle authority, with no storage dependencies;
  - `flotilla-paths` (path policy and path context);
  - `flotilla-daemon-api` (`DaemonHandle`, build info, the lifecycle lock file);
  - `flotilla-config`.
- **Store:**
  - `flotilla-store`: an object-safe store trait at the untyped level (kind key, serialized body, query value), with a typed extension on top. It carries the declared query vocabulary and indexes (#2953) and per-request read counters.
  - Backends: `flotilla-store-sqlite`, `flotilla-store-memory` and `flotilla-store-http`.
  - `flotilla-replication`: store-to-store sync over `dyn Store`. An event-sourced, log-backed backend may later replace or sit beneath it.
- **Providers:**
  - `flotilla-provider-runtime`: the runner, the HTTP client and the invocation macros.
  - `flotilla-discovery-api`: the `Factory` trait, `EnvironmentBag`, `UnmetRequirement`, `ProviderDescriptor` and a narrow config view.
  - **One crate per provider type**, each with a file module per provider: `flotilla-vcs`, `-change-request`, `-issue-tracker`, `-terminal`, `-environment`, `-ai-utility`, `-cloud-agent` and `-agent` (the harness adapters). Each type crate exports its own factories and type-specific detectors. CLI and library implementations are sibling modules, with heavy library dependencies behind per-provider features. Provider traits never mention the runner.
  - Forge clients `flotilla-forge-github` and `flotilla-forge-forgejo`, shared by the change-request and issue-tracker crates.
  - `flotilla-discovery`: the generic detectors, plus assembly of the factory list, static for now and open to registration later (for example plugin factories: scripted, wasm, dylib).
- **Charter:** `flotilla-charter` holds the project's declarations read through a `RevisionedTree` interface: a local directory, git at a ref, or a database later. VCS offers only a generic "read files under a path at a revision" operation. `CharterSnapshot` leaves the `Vcs` trait. Agent guidance (briefs and prose delivery) consumes the charter from the agent and role layer.
- **Agents:**
  - `flotilla-agent-channel` (#2951): sessions over transports (codex-app-server, Claude SDK, Claude channels, cleat keystrokes, ACP). It contains no resource types.
  - cleat owns terminal content and agent screen state (flotilla-org/cleat#326).
  - The TerminalSession resource and its reconciler remain orchestration.
- **Messaging:** `flotilla-messaging` behind a trait. It is resource-backed by default; a broker may replace it for large installs.
- **Orchestration** splits into capability crates: `flotilla-admission`, `flotilla-crew`, `flotilla-projections` (which absorbs the aggregator) and `flotilla-executor`.
  - Each owns its command handlers.
  - Admission is shaped around admitting a unit of work, so that, for example, standing agents can admit ad-hoc vessels.
- **Hosts:**
  - `flotilla-daemon` is the host process: the socket server, routing, health, and the composition root that picks backends and postures and wires every port.
  - `flotilla-client` never compiles core.
  - `flotilla-tui`, and `flotilla-commands` (parsing only; it may become modular; moving off clap is allowed).
  - `flotilla-manifest` remains the presentation manifest wire layer.

### VCS operation boundary (amended)

VCS operations go through the `Vcs` trait. Each provider chooses a CLI implementation through the environment runner, or a library implementation. The previous rule ("CLI via the runner is the universal implementation; a library backend must not replace it for remote or provisioned environments") no longer holds: an environment may run a minimal flotilla agent beside cleat, so library implementations can run in-environment. The Git boundary check continues to forbid invoking `git` outside the VCS implementation.

### Sequencing

1. **Ports and services inside the current crates** (#2212; the rewritten #2222), plus small down-moves:
   - `EnvironmentBag` and `ConfigStore` move down;
   - the three provider up-edges are removed;
   - `charter_snapshot` comes off `Vcs`.
2. **Extract the leaf crates:** paths and daemon-api (which frees the client), the testkits (#2948), then the store API, discovery API, provider runtime and type crates.
3. **The store trait and backend crates**, together with #2953's pushdown.
4. **The orchestration capability crates.** The aggregator and credentials move onto ports.
5. **The terminal and agent-channel moves**, with cleat#326 and #2951.
6. **#2909**, CI by crate scope.

Stored shapes are unaffected by moving code. Any crate move that changes a serialized type follows ADR 0047.

## Consequences

- Adding a provider touches one crate. Swapping a store backend, persistence posture or message transport is a composition change.
- The client and Windows CI scope stop compiling core.
- Release binaries carry no test code.
- More crates, and more ports to maintain. The build-graph check makes the direction of dependencies a CI failure rather than a review comment.

## Recorded for later

- An implementation label on providers, with picking the best when none is specified (for example libgit2 versus gitoxide).
- Plugin factories.
- Modular command definitions.
- A message broker for large installs.
- An event-sourced store backend.
- An in-environment flotilla agent for library operations.

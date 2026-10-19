# 60. Component crates and the composition root

**Status:** Accepted (owner grill, 2026-10-09; #2749)
**Date:** 2026-10-09
**Amended:** 2026-10-13 (homes for modules the target crates did not place; #3152)
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
  - `flotilla-provider-runtime`: the runner, the HTTP client, generic invocation macros and the neutral `ObservationError { Failed(String), RateLimited { budget, retry_at, detail } }`. The error exposes a retry deadline and adapter-owned diagnostic text without a forge dependency.
  - `flotilla-discovery-api`: the `Factory` trait, `EnvironmentBag`, `UnmetRequirement`, `ProviderDescriptor` and a narrow config view.
  - **One crate per provider type**, each with a file module per provider: `flotilla-vcs`, `-change-request`, `-issue-tracker`, `-terminal`, `-environment`, `-ai-utility`, `-cloud-agent` and `-agent` (the harness adapters). Each type crate exports its own factories and type-specific detectors. CLI and library implementations are sibling modules, with heavy library dependencies behind per-provider features. Provider traits never mention the runner.
  - Forge clients `flotilla-forge-github` and `flotilla-forge-forgejo`, shared by the change-request and issue-tracker crates. `flotilla-forge-github` owns `GhApi`, the `gh_api_*` macros, polling, observation telemetry, host budget accounting and `GithubRateLimit`; the classifier fills the neutral error detail and preserves existing diagnostic text.
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

### Amendment, 2026-10-13: homes the target crates did not place (#3152)

The step-4 survey found core and store code with no home in the target crates. Owner rulings:

- **Change feed.** Domain code drifted into `flotilla-store` because the store is the only component that publishes domain changes. The change-feed interface (typed watches, indexed views and the controller runtime) is the seam domain crates depend on; persistence is one backend behind it. It stays in `flotilla-store`'s public API until the resident-state redesign lifts it into its own crate. `flotilla-store` holds only generic machinery: the store trait, queries, views and watches, the controller runtime, owner garbage collection and dynamic kinds. A build-graph rule refusing domain modules in it lands with the store-goes-domain-free stream of #3152; until then the rule is a review check.
- **`flotilla-observation`** (new): the change-request, issue and forge observers and the observed-resource projection, turning provider facts into observed resources. It sits between the provider type crates, which stay storage-free, and the store. It owns the public `ChangeRequestQueryPort` and `IssueQueryPort`; core supplies their provider-resolution implementations. It is expected to become event-log driven.
- **`flotilla-messaging`**: delivery (with the `MessageTransport` seam that agent-channel transports implement), inbox, retention, conditions and role routing (including charter notifications), moved out of `flotilla-store`. Receiver resolution moves with messaging and reads resource kinds directly; there is no receiver-resolution port. The primitives-model mission #3108 will replace it with role bindings. `MessageQuery` stays a pure pushdown-query data type in the store interface; SQL translation and memory indexing belong to the respective backends (resolution #3297, extraction #3298).
- **Reconcilers live with the capability that owns their kind,** and `flotilla-controllers` retires. Each capability crate exports its reconcilers; the daemon registers them.
  - `flotilla-admission`: standing-convoy ensure and vessel placement.
  - `flotilla-crew`: the convoy phase and landing reconciler, vessel, terminal session, settlement-claim publishing.
  - **`flotilla-provisioning`** (new): environment, image build, checkout, clone and repository reconcilers, which realise environments, images and checkouts on hosts. It is distinct from the executor, which runs command steps.
  - `flotilla-projections`: principal attention, and the host and fleet read models (host registry, host summary, fleet views and fleet health). The peer-connection parts of the host registry are Plane-A peer-layer state and stay with the daemon's peer layer until it is deleted.
  - `flotilla-charter`: the project hierarchy extension.
- **Environments.** The live environment table (per-environment runner, environment bag, provider registry, path mapping and mounts) is implemented in `flotilla-discovery` as its per-environment runtime. Capability crates consume it through a narrow `Environments` port; the daemon wires the implementation.
- **`flotilla-capability-contract`** (new, #3200): the session capability contract (`CredentialCapability`, `SessionCapabilitySource`, `endpoint_for_env`, `credential_card`). It depends on no flotilla crate, so `flotilla-credentials` implements the source and `flotilla-core` and the daemon consume it without a cycle. Card observation and rendering stay in core until the crew stream.
- **Host identity** (HostId and NodeId resolution) lives in `flotilla-daemon`: the composition root resolves it once at startup and injects it.

### Admission capability (#3299)

`flotilla-admission` owns the admission transaction, admission command handlers,
free-space floors, placement policy, mission normalization and accounting,
repository addressing, regard lifecycle, composed-image planning, project
registration/refresh and project image layers. Standing-convoy admission ports
and the default fail-closed backing inspector live with this capability; the
reverse reconciler port and install step remain in place until #3492.

Core composes admission with three owned ports:

- `ConvoyTeardown` supplies `abandon` and `reap` over the crew service.
- `AdmissionEnvironments` supplies the local host identity, environment bag,
  runner and environment-to-host lookup. It is distinct from projections'
  `Environments`; the live `EnvironmentManager` stays in core's composition.
- `ProjectOperations` is the repository port: repository inspection, project
  integration, adopted-checkout inspection, image checks, convoy-start worker
  submission and namespace-explicit forge-identity resolution. It owns the
  `RepositoryInspector` interface and inspection values; core retains the
  `GitRepositoryInspector` implementation.

Admission reads observations and discovery directly. Core retains observation
query-port implementations, forge-identity sweep composition, image-check
execution and the attach-waiting convoy-start worker. Repository tracker leases
are accessed through admission's public accessor, not its state field.

Shared naming, architecture aliases, checkout path selection, crew accounting
and the brief-artifact marker live in resources. `BriefArtifactWriter` lives in
agent, command-event helpers in daemon-api, and the change-request observation
result `ConvoyBranchRefresh` in observation. Consumers import these APIs from
their owners directly. The build-graph guard registers admission as a capability
and refuses other capabilities, composition, provider reachbacks and the
state-machine engine. These moves change no resource schema or wire shape.

## Consequences

- Adding a provider touches one crate. Swapping a store backend, persistence posture or message transport is a composition change.
- The client and Windows CI scope stop compiling core.
- Release binaries carry no test code.
- More crates, and more ports to maintain. The build-graph check makes the direction of dependencies a CI failure rather than a review comment.

Provider implementations live in one crate per forge (`flotilla-github-providers` and `flotilla-forgejo-providers`), apart from their forge-neutral traits (#3480).

## Recorded for later

- An implementation label on providers, with picking the best when none is specified (for example libgit2 versus gitoxide).
- Plugin factories.
- Modular command definitions.
- A message broker for large installs.
- An event-sourced store backend.
- An in-environment flotilla agent for library operations.

### Inbox engine boundary (#3316)

`flotilla-inbox` depends on `flotilla-state-machines` and the schema stack. Its
`InboxBackend` trait has memory and SQLite implementations, exercised by the
dev-only `flotilla-inbox-testkit`. No production crate depends on either new
crate until the Message switch in #3317; `ci/build-graph/check.py` enforces that
isolation. Existing `flotilla-messaging` and daemon delivery remain unchanged.

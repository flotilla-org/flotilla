# Flotilla

A system for improving multi-agent developer workflows. Consists of a daemon mesh, CLI, and TUI dashboard — managing development workspaces across git worktrees, code review (GitHub PRs), issue trackers, cloud agents (Claude Code, Codex, Cursor), terminal persistence (cleat), and multiplexers (cmux, zellij).

## Development Phase

**Wire formats change freely; stored data stays decodable for one generation** (ADR 0047).

- **Wire and protocol**: protocol types, socket messages, peer envelopes, and replication wire formats change freely. There are no multi-version peers or deprecation windows, because the fleet rolls as a unit and the fingerprint handshake refuses mismatched peers.
- **Stored data (N→N+1)**: each generation must decode everything the previous generation stored:
  - resource specs **and statuses**, including embedded structures such as a convoy's frozen workflow snapshot;
  - manifests in project-map and ops repositories;
  - daemon config.

  Write only the new shape. For a rename add a serde `alias`, for a new field a `default`, and for a retired field a deserialize-only record type that accepts and drops it (see `VesselRequirement`'s `stance`). Comment each shim with when it can be removed (one roll later). There is still no migration framework.
- **Out-of-repo manifests**: stay compatible, or ship the project-map/ops manifest update in the same roll.
  Operational entries in ops repositories follow the same N→N+1 rule: retired fields are accepted and dropped for one generation, while genuinely unknown fields remain errors. The candidate pre-roll validation must parse registered projects' operational entries as well as decode stored records; a refusal fails the gate.
- **Enforcement**: `cargo test` decodes a golden corpus of real stored records (#2169). If it fails, fix the decoder with an alias, default, or record type. **Never regenerate the corpus to make a PR pass**; it is refreshed only after a fleet roll. Before each roll, the candidate decodes every host's live store (#2167).

### Plane-A freeze (transition in progress)

Flotilla is consolidating its provider integrations and k8s-isomorphic control plane around resources. Provider refreshes still gather adapter data, while discovered checkout facts are projected into a second, ephemeral `ResourceBackend` by `crates/flotilla-observation/src/observed_resources.rs`. The Aggregator combines observed and durable resources into query result sets for the TUI. See `docs/roadmap.md`, `CONTEXT.md`, and the ADRs in `docs/adr/`; the umbrella is flotilla-org/flotilla#604.

**The remaining Plane-A peer layer is bugfix-only.** Do not add net-new features to the peer-merge subsystem (`crates/flotilla-daemon/src/peer/`); it is slated for deletion as multi-host behavior finishes moving to resource-store federation. Bugfixes and tests are fine. New multi-host capability belongs in resource replication and the eventual Tender. If a change feels like it grows peer-merge, stop and check the roadmap.

The TUI is *factored, not frozen* — keep it maintained, but extract the surface-agnostic view-model rather than duplicating presentation logic per surface.

## Quick Reference

```bash
cargo build                                    # build
cargo fmt --check          # CI format gate
cargo clippy --workspace --all-targets --locked -- -D warnings  # CI clippy gate (local gate: -p <affected>, see below)
cargo test --workspace --locked                # CI test gate (local gate: -p <affected>, see below)
python3 ci/build-graph/check.py                 # CI dependency/feature gate
python3 -m unittest discover -s ci/build-graph -p test_check.py
cargo fmt                  # apply pinned formatting
uv run --with-requirements ci/git-boundary/requirements.txt python ci/git-boundary/check.py  # ast-grep Git boundary check
cargo run                                      # run, auto-detect repo from cwd
scripts/prune-target.sh --dry-run              # preview the per-checkout target size-cap backstop
scripts/prune-target.sh                        # apply the per-checkout target size-cap backstop
scripts/install-cargo-sweep-schedule.sh        # install/verify the daily per-host mtime sweep
```

Use the [Git boundary check recipe](docs/development.md#git-boundary-check) for the check and its tests. It uses `uv` without requiring Python's `venv`/`ensurepip` packages.

Platform-specific code gets cfg-gated tests in its own crate; platform jobs pick them up through their fixed crate scope (the explicit list of crates each platform job tests; see [#2909](https://github.com/flotilla-org/flotilla/issues/2909)). Do not add lines to `ci/platform-tests/selectors.txt` (being retired, #2909). Add a CI job only for a new runner type or OS; otherwise use an existing job on the same runner.

Before pushing, run the local gate:
- `cargo fmt --check` (whole workspace).
- Clippy and tests for the **affected packages only**: each workspace package whose files the change touches (`crates/<name>/`, plus the root `flotilla` package for `src/`), as `cargo clippy -p <pkg>... --all-targets --locked -- -D warnings` and `cargo test -p <pkg>... --locked`.
- The build-graph check and the Git check, with their tests, when the change touches Cargo manifests, dependencies or Git-invoking code.
- The stored-record corpus decode (`cargo test -p flotilla-store-testkit --locked --test integration stored_corpus`) when the change touches a resource kind's serialized shape.
- A change that maps to no package (`ci/`, workspace `Cargo.toml`, `rust-toolchain.toml`) runs the check that owns that file: the build-graph or Git check and their tests, or `cargo check --workspace --locked` for a toolchain or workspace-manifest change.
- A change to a widely used package (protocol, resources, store, core) still runs only its own tests locally; CI covers its dependents.

CI runs the full workspace commands (`cargo clippy --workspace ...`, `cargo test --workspace --locked`); don't repeat that run locally, and fix a red CI result with a follow-up push. A local `-p` run can resolve features differently from the workspace run, so a local pass is not a CI guarantee.

Desk and crew vessel builds keep Cargo incrementals enabled. CI builds set `CARGO_INCREMENTAL=0`. Each fleet host runs an mtime-based `cargo-sweep --time 3` daily, while `scripts/prune-target.sh` remains a size-cap backstop. See [docs/development.md](docs/development.md) for installation, scope, logs, and thresholds.

**Stable toolchain:** `rust-toolchain.toml` is the single compiler pin (currently Rust 1.99.0). Rustup selects it automatically and installs rustfmt, Clippy, llvm-tools, and the Relay Workers WebAssembly target. CI installs that pin and runs `ci/toolchain/assert.sh` to reject a mismatched compiler. Coverage uses stable with line coverage only. Release candidates and the crew image read the same file.

**Formatting:** `cargo fmt` uses the same stable pin; no nightly toolchain is required.


Only when `CODEX_SANDBOX` is set, use `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests` so native dependencies can create temp files and socket-bind tests stay skipped. Everywhere else, use the default `TMPDIR`. Either way, pass the affected packages (`-p <pkg>`) instead of `--workspace` for the local gate. Unix-socket tests use the shared harness's SUN_LEN-safe directory directly beneath `/tmp`, independent of `TMPDIR`; do not change this command to work around socket-path length errors.

Controller-backed in-process daemon coverage lives in `flotilla-controllers`' registered integration binary:

```bash
cargo test -p flotilla-controllers --locked --test integration in_process_daemon::
```

Shared helpers live in dev-only `flotilla-*-testkit` crates; production crates have no `test-support` or `replay` features.
Tokio `test-util` is selected only by dev-dependencies. Package-local test commands need no helper flags.
Core and daemon unit tests compile shared adapter sources only under `cfg(test)` to retain the unit-test library's trait identity;
integration tests import the testkit crates directly. Native libraries share `flotilla-build-features`' feature-only dependency
selections on stable Cargo; `ci/build-graph/check.py` checks build and test selections separately for every workspace package,
rejects production testkit dependencies and production `test-util`, and runs in workspace CI. Keep its anchors aligned when
adding dependencies. The build-graph check resolves the full Cargo graph to enforce the boundary on external Rigging crates; its first run may fetch the pinned Git sources. Optional TLS providers and sandbox skips remain opt-in. The Relay Workers WebAssembly target does not
depend on the native feature selections.
Workspace tests require Python 3.9+ and Cargo on PATH (`python` on Windows, `python3` elsewhere) for this guard.

`flotilla-store-testkit`, `flotilla-crew` and `flotilla-controllers` collect their integration tests in `tests/integration/main.rs`.
The store-testkit `watch_allocations` and `decode_cost` targets and the daemon `daemon_allocations` target stay separate because they install process-wide counting allocators.
`flotilla-daemon` uses that target for general integration tests and `tests/request_session_pair/main.rs` for routing scenarios.
Run `cargo test -p <package> --locked --test integration` for the integration target, and
`cargo test -p flotilla-daemon --locked --test request_session_pair` for routing scenarios. Append a former target's module name or a test name
(for example, `cargo test -p flotilla-daemon --locked --test request_session_pair generated_partition_heal_restart_preserves_tombstones`) to select that area.
The workspace CI commands still run all of these tests. Fixtures and shared harnesses remain in `tests/fixtures/` and `tests/common/`.
Register each new integration module in its `main.rs`; each suite checks this with the shared `test_module_registration` test.
Tests in a shared binary must isolate state: avoid changing process-wide environment variables, working directories, or global tracing
subscribers. Keep tests that require process-wide state (such as a global allocator) in a separate target.

## Testing Philosophy

- Outbound HTTP code must be tested against an in-process stand-in that enforces the remote service's documented request requirements, or against a recorded replay fixture. Mock-only coverage that accepts arbitrary requests is insufficient. Document justified exceptions beside the affected tests; the [HTTP audit](docs/http-request-contract-audit.md) indexes coverage and exceptions. Use test identities and signing keys for credential contracts; never record live token mints.

- Prefer behavior tests that run through injected collaborators over tests that depend on real filesystem state, subprocess orchestration, sockets, or live multi-host setup.
- When a subsystem has multiple storage or transport implementations, specify the behavior once and run the same contract tests against each implementation.
- Use in-memory implementations for most logical scenario tests when they make setup clearer and failures easier to reason about.
- Keep real-backed implementations covered too, but verify them against the same behavioral contract instead of forcing all behavior tests through the real backing store.
- Favor reusable test harnesses over ad hoc setup. The goal is to make new multi-step scenarios cheap to express and debug.
- For multi-host orchestration logic, prefer `InProcessDaemon`-level tests unless the bug specifically depends on real process or transport boundaries.
- Test multi-host command routing at the router and request-dispatch level over in-memory transport sessions; `InProcessDaemon` tests do not exercise routing. Add a scenario row when routing, admission, or command targeting changes.
- Test multi-host routing, homing, and teardown through the router-level scenario engine; targeting changes add a pinned row and pass the generated CI profile.
- **The `insta` snapshots in `crates/flotilla-tui/src/widgets/table.rs` are a signal, not a formality.** Never run `cargo insta accept` or update those snapshots just because a test failed. A failing snapshot means the rendered output changed — investigate *why* it changed. If the change is an intended consequence of the current work, accept it with a clear justification. If the change is unintended, it's a bug — fix the code, don't update the snapshot.

## Claude Code Web (changedirection/flotilla fork)

This fork exists for Claude Code Web sessions. Two things to be aware of:

- **Upstream repo for gh commands**: Issues and PRs live on the upstream `flotilla-org/flotilla`. Always pass `-R flotilla-org/flotilla` to `gh issue` and `gh pr` commands (e.g. `gh issue list -R flotilla-org/flotilla`, `gh pr create -R flotilla-org/flotilla`). PRs should target `flotilla-org/flotilla:main` as the base.
- **No local servers**: The sandbox blocks binding ports, so MCP servers and visualization tools that start local HTTP servers won't work.

## Architecture

### Environment provider lifecycle

See [ADR 0058](docs/adr/0058-environment-providers-own-lifecycle-and-runtime-capabilities.md)
for provider selection, preparation ownership, host adoption and image capability boundaries.


`flotilla-observation` is a shared layer below the capability crates. It turns provider facts into observed resources through forge-neutral query ports; core supplies provider discovery and keeps the observed-checkout reconciliation lock.

Provider observations and control-plane resources meet at the Aggregator:

```
Providers (git, wt, github, claude, codex, cursor, cmux, zellij, cleat)
  → provider refresh / adapters
    → observed Checkout resources → ephemeral ResourceBackend ─┐
                                                            ├→ Aggregator → result sets/events → TUI
Desired state → durable ResourceBackend → reconcilers/leaf engine ─┘
```

User actions flow: **TableIntent/UI action → Command → daemon executor → provider or resource operation → refresh/watch update**

### Crate Structure

| Crate | Role |
|-------|------|
| `flotilla-build-features` | No runtime interface; shared native dependency features and host proc-macro features for stable build/test reuse |
| `flotilla-terminal` | Terminal pool traits, Cleat and passthrough adapters, factories and shared session scans |
| `flotilla-discovery-api` | Environment facts, narrow provider config view, detector traits and factory contracts; wraps Rigging version/binary probes into Flotilla environment assertions |
| `flotilla-provider-runtime` | Flotilla Git guard, scratch namespace, helper scripts, SSH runner and neutral observation errors; re-exports Rigging process, probe and HTTP interfaces |
| `flotilla-forge-github` | GitHub client, invocation macros, rate-limit classification, polling, telemetry, host budget accounting and discovery-time repository resolution |
| `flotilla-environment` | Environment and image capability traits, Docker and host-direct adapters, factories and transport hop resolution |
| `flotilla-vcs` | Checkout-scoped VCS traits, Git strategies, factory and Git discovery probes |
| `flotilla-ai-utility` | AI utility trait, Claude API/CLI implementations and factories |
| `flotilla-cloud-agent` | Cloud session provider trait, Claude/Codex/Cursor implementations, factories and detectors |
| `flotilla-forge-forgejo` | Shared Forgejo HTTP client, authentication values and discovery-time credential resolution |
| `flotilla-github-providers` | GitHub change-request tracker, issue provider, dispatch-board reader and factories |
| `flotilla-forgejo-providers` | Forgejo change-request tracker, issue provider and factories |
| `flotilla-change-request` | Forge-neutral change-request tracker and observation-source traits, observation decoding and branch-absence observer |
| `flotilla-issue-tracker` | Forge-neutral source-addressed issue-provider trait, mission-field decoding and provider contract |
| `flotilla-agent` | Agent harness adapters and registry, crew brief rendering, hook parsing and installation, agent state store, process-exit receipts |
| `flotilla-agent-connector` | The traits a harness connector implements (session shapes and launch, hook parsing, state readings marked native or inferred, per-operation and per-version transport declarations), value types, state arbitration and the `TransportRouter` that picks one agent transport per delivery; no resource types or store (#2951, #3545) |
| `flotilla-dummy-fleet` | Load-testing dummies installed behind `daemon.toml` `[dummy_fleet]`: scripted crews in an in-process terminal pool and agent adapter, a dummy VCS, a dummy forge's change-request tracker, and the workflow/policy catalog (#3206); the dummy environment provider lives in `flotilla-environment` |
| `flotilla-fake-forge` | Rehearsal-only fake Forgejo service (#3374): one shared in-memory forge behind the strict Forgejo API the providers use, an HTTPS admin API for scenario drivers, seed generation and the in-process `HttpClient` the provider contract tests run against |
| `flotilla-discovery` | Static factory-list assembly, default detector lists, detector runs, provider discovery, host-scoped provider cache, `ProviderRegistry`, provider status, and environment management (`EnvironmentManager` behind the `Environments` and `EnvironmentLifecycle` ports) |
| `flotilla-charter` | Charter inputs read through a `RevisionedTree` (Git via `Vcs`, or a local directory): `project.yaml` declarations, operational entries and their provenance annotations, charter locks and source status |
| `flotilla-executor` | Provider-backed command handlers, step plans and resolution, remote step batches and host orchestration ports |
| `flotilla-observation` | Provider-neutral change-request, issue and forge observation, demand servicing and observed-resource projection |
| `flotilla-provisioning` | Environment, ImageBuild, Checkout, Clone and Repository reconcilers; orphaned-environment and image-inventory loops; the four provisioning kinds' engine lifecycle specs (`stored`, #3629) and their write path behind `[primitives]` (`engine`: each old reconciler's patches decided by the engine, with a one-way copy back) |
| `flotilla-admission` | Convoy admission, standing-convoy reconciliation, vessel placement projection, placement planning, project operations and composed-image planning; host orchestration through owned ports |
| `flotilla-crew` | Crew operations and actions, capability cards, the leaf engine, the decision log, charter notifications, review bundles, and the Vessel, TerminalSession and Convoy reconcilers; host collaborators through owned ports |
| `flotilla-core` | Refresh and `InProcessDaemon` composition root; implements admission, crew and observation ports |
| `flotilla-protocol` | Commands, self-describing query rows and shared table vocabulary (flotilla-schema), provider snapshots, events and envelopes; no storage backend |
| `flotilla-paths` | Shared path policy, filesystem canonicalization and protocol path context |
| `flotilla-config` | Host-local config files (`config.toml`, `daemon.toml`, `hosts.toml`, observation roots, open views), provider preference values, default checkout path and bounded log files |
| `flotilla-daemon-api` | `DaemonHandle` and event publication interfaces, query subscription token, build identity and lifecycle lock file |
| `flotilla-client` | Socket client: `SocketDaemon`, `connect_or_spawn`, gap recovery |
| `flotilla-cli` | Shared CLI parsing/output, image-check client, PM connector and foreground terminal attachment; no dashboard renderer |
| `flotilla-vessel-agent` | Narrow resident vessel startup, environment configuration, logging and shutdown; no store, reconcilers or providers |
| `flotilla-tui` | UI rendering (widget tree), input handling, binding table, keymap and event loop; selected by the root default-on `tui` feature |
| `flotilla-daemon` | Runtime supervision, socket server and the one HTTP router (ADR 0065) on every listener, replication composition and routed transport, peer networking, multi-host command routing |
| `flotilla-address` | ADR 0066 addresses: typed `kind:value` segments, canonical encoding, forge-kind normalisation, lenient input through a Forge lookup, `record_name`, references and the condition/template grammar; no flotilla dependencies |
| `flotilla-cbor` | The ADR 0063 CBOR profile over `cbor2`: deterministic writers, safe readers, limits, framing and the conformance corpus; no flotilla dependencies; moving to the shared [rigging](https://github.com/flotilla-org/rigging) repo, where sibling projects take it from (ADR 0068) |
| `flotilla-capability-contract` | Session capability contract: `CredentialCapability`, `SessionCapabilitySource`, endpoint and credential-card helpers; no flotilla dependencies |
| `flotilla-credentials` | Host-local credential minting/delivery, agent homes and skills, private vessel configuration composition |
| `flotilla-projections` | Surface read models, host/fleet views, dispatch board, query actions, durable/observed aggregation and demand-backed issue materialization; reads host inputs through ports |
| `flotilla-resources` | Resource kinds, lifecycle authority, typed list/watch shapes and shared pure helpers, including convoy-address selection, terminal delivery/attention evidence, unmet-expectation explanations and landing-evidence freshness; no storage dependencies |
| `flotilla-state-machines` | ADR 0064 engine: machine declarations whose states are values, with typed edge dispatch and stable codes, gated edges at a single writer, judgements and typed reads through a recording handle, and each machine's stored kind; uses `flotilla-schema`'s records, time, codes and edge events and `flotilla-address`'s conditions, and reaches no other workspace crate; its production dependents are the engine-store and inbox crates and, behind the `[primitives]` switch, `flotilla-core` and `flotilla-daemon` (#3317) |
| `flotilla-inbox` | Async InboxBackend contract over the Message engine, memory and SQLite implementations; no production callers until #3317 |
| `flotilla-engine-store` | The engine's storage port: `Log` over `flotilla-schema-sqlite` (creation through `insert`, edges through `apply_edge`, one transaction per write), Message admission in the create transaction, `ReceiptLog` from `Read::about`, the `[primitives]` switch's kind registry (Message and AgentSession) with its DDL snapshot (#3317), and `open_home`, a machine's home restored from the store |
| `flotilla-inbox-testkit` | Dev-only shared inbox scenarios, generated claim interleavings and Message race adapter, generic over backend factories |
| `flotilla-quint-testkit` | Dev-only shared ITF parser, graph walker, trace/stream helpers and Message race mapping for engine and inbox adapters |
| `flotilla-messaging` | Resource-backed inbox admission, delivery through the agent-connector `TransportRouter`, retention, conditions and receiver/role routing |
| `flotilla-store` | Object-safe `Store`, typed resolvers, replica/watch machinery, store queries, generic request snapshots and merged definition-catalog reads, and the generic controller runtime || `flotilla-store-memory` | In-memory storage and observed replicas; no native persistence dependencies |
| `flotilla-store-sqlite` | SQLite persistence, schema, transactions and decode quarantine |
| `flotilla-store-http` | Kubernetes-isomorphic HTTP storage client |
| `flotilla-store-testkit` | Shared backend contracts, fixtures and per-request read counters |
| `flotilla-schema-derive` | `Stored` generates codecs, descriptors and edge handles; `Describe` generates descriptions only. Shape attributes (`child`, `flatten`, `intern`, `code`) are separate from storage policies (#3320, ADR 0067) |
| `flotilla-schema` | Backend-neutral `TableSet` descriptions and `Describe` traits for stored kinds and query rows, declared indexes, logical rows codec, the append-only `Records` type, the `EdgeEvent` edge event and inline CBOR documents, typed edge handles and the `Edge` builder, typed index queries (`IndexQuery`), log entries (`Event`, with named field-level `Effect`s) and write origins, timestamps, the kind-code registry and the schema review view (`render_view`); re-exports the derive |
| `flotilla-schema-sqlite` | SQLite DDL rendering (`DdlMode`), SQL extension traits and the Rusqlite runtime for derived kinds: shared tables (`strings`, `resource_identity`, `objects`, `home_sequence`, `log`), per-kind statements, interning, owner-checked writes (creation, spec edits, `apply_edge`, delete and expunge), the per-home event log with one lowering for local writes and `apply` of other homes' events, snapshot restore, reads of entries about a master, keyset-paged index queries and the startup scan |
| `flotilla-replication` | Store-to-store replication over `dyn Store`: list/watch/resume, origin filtering, digest scheduling; host ports for stores, identity, health and peer sources |
| `flotilla-review-bundle-object-store` | Object-store/S3 byte adapter for the review-bundle port |
| `rigging-tls` (external) | TLS provider selection and reqwest builders from the pinned Rigging revision |
| `rigging-process` (external) | Command supervision, invocation macros, environment variables, filesystem/space probes, scan cache and binary/version detection |
| `rigging-http-serve` (external) | Axum serving over Unix sockets, accepted connections and loopback TCP: typed caller identity, the token file, and the Host/Origin/cookie-Origin TCP policy around consumer authentication |
| `rigging-http-client` (external) | Injectable HTTP execution and the reqwest implementation |
| `rigging-replay-testkit` (external, dev-only) | Replay sessions, recording, masks, mock runners and HTTP stand-ins |
| `flotilla-replay-testkit` | GitHub replay channel over `rigging-replay-testkit` scheduling, masks and test helpers |
| `flotilla-controllers` | Cross-capability reconciler tests and the `provisioned_mount` helper, pending retirement |
| `flotilla-commands` | Resource-oriented command preparation and execution |
| `flotilla-transport` | Framed message sessions over Unix sockets or in-memory channels for daemon client and peer connections |
| `tender` | Publication, discovery, pinned-exposure, and raw-stream contracts with an in-memory adapter and shared behavior tests |
| `flotilla` (root) | Thin `src/main.rs` entry point |

### Key Modules

| Path | Role |
|------|------|
| `src/main.rs` | Entry point, CLI dispatch |
| `crates/flotilla-daemon-api/src/daemon.rs` | `DaemonHandle` trait |
| `crates/flotilla-core/src/in_process.rs` | `InProcessDaemon` composition root and narrow controller ports |
| `crates/flotilla-admission/src/convoy_ensure.rs` | Narrow admission and controller ports for standing convoys |
| `crates/flotilla-admission/src/convoy_admission.rs` | Transaction-owning convoy admission primitive |
| `crates/flotilla-admission/src/reconcilers/convoy_ensure.rs` | Standing-convoy reconciliation, retry state and the complete ensure transaction guard |
| `crates/flotilla-crew/src/crew_ops.rs` | `CrewService`: crew outcomes, lifecycle teardown and serialized crew message delivery |
| `crates/flotilla-crew/src/leaf_engine.rs` | Leaf subscriptions, wait wakes, turn delivery, stalls and supervision |
| `crates/flotilla-crew/src/convoy.rs` | Convoy phase and landing reconciler; reaches crew operations only through `ConvoyTeardownRuntime` |
| `crates/flotilla-crew/src/reconcilers/vessel.rs` | Vessel reconciler and its `WorktreeMetadataResolver` port |
| `crates/flotilla-crew/src/reconcilers/terminal_session.rs` | TerminalSession reconciler, its `TerminalRuntime` port and the tool-session `TerminalActuator` |
| `crates/flotilla-executor/src/executor.rs` | Executes provider-backed command steps and returns `CommandValue` |
| `crates/flotilla-executor/src/executor/` | Executor submodules: checkout, session actions and tests |
| `crates/flotilla-core/src/model.rs` | `RepoModel` and provider label/name helpers |
| `crates/flotilla-executor/src/data.rs` | Checkout status inspection |
| `crates/flotilla-observation/src/observed_resources.rs` | Projects discovered and adopted Checkouts into the ephemeral observed-resource store |
| `crates/flotilla-executor/src/step.rs` | Step planning and execution system |
| `crates/flotilla-agent/src/adapter.rs` | Agent harness adapters, `AgentAdapterRegistry`, capability table and crew brief rendering |
| `crates/flotilla-agent/src/hooks.rs`, `crates/flotilla-agent/src/store.rs` | Agent hook handling and state management |
| `crates/flotilla-agent/src/process.rs` | Process-exit receipts and image preludes for agents in persistent terminal shells |
| `crates/flotilla-agent/src/attachable.rs` | Agent-hook identity shim pending AgentSession |
| `crates/flotilla-core/src/convert.rs` | Core-to-protocol type conversion |
| `crates/flotilla-discovery/src/lib.rs` | `DiscoveryRuntime`, detector runs, checkout and host-scoped provider discovery |
| `crates/flotilla-discovery/src/registry.rs` | `ProviderRegistry`, typed provider sets and `command_provider_registry` |
| `crates/flotilla-discovery/src/environment_manager.rs`, `crates/flotilla-discovery/src/environments.rs` | `EnvironmentManager` and the `Environments` and `EnvironmentLifecycle` ports |
| `crates/flotilla-discovery/src/factories.rs`, `crates/flotilla-discovery/src/detectors/` | Default factory and detector lists |
| `crates/flotilla-config/src/lib.rs` | Persistence to `~/.config/flotilla/` |
| `crates/flotilla-core/src/template.rs` | `.flotilla/workspace.yaml` pane templates |
| `crates/flotilla-protocol/src/lib.rs` | `Message` envelope, `DaemonEvent` |
| `crates/flotilla-protocol/src/commands.rs` | `Command`, `CommandAction`, and `CommandValue` |
| `crates/flotilla-protocol/src/result_set.rs` | Aggregator query rows, result sets, and deltas |
| `crates/flotilla-protocol/src/snapshot.rs` | `RepoInfo`, provider errors and labels |
| `crates/flotilla-daemon/src/server.rs` | Daemon server with peer networking |
| `crates/flotilla-daemon/src/server/` | Server submodules: client/peer connections, request dispatch, remote commands, and `http/` (the router, caller authentication, login sessions, the Unix front door and the TCP listener) |
| `crates/flotilla-projections/src/lib.rs` | Aggregator runtime context and public resolver ports; wires store watch sources |
| `crates/flotilla-projections/src/aggregator.rs` | Watches durable and observed resources and maintains query result sets |
| `crates/flotilla-projections/src/read_projections.rs` | Store-backed explanation, host/fleet, project and crew-stall views; environment and leaf-evidence ports |
| `crates/flotilla-projections/src/projections_actions.rs` | Query conversion, enrichment and resource watch handlers over host ports |
| `crates/flotilla-projections/src/host_registry.rs` | Host read models joined with the injected peer overlay |
| `crates/flotilla-projections/src/dispatch_board.rs` | Shared tracker observation cache and dispatch-board assembly over stores and issue resolvers |
| `crates/flotilla-core/src/in_process/projection_ports.rs` | Composition adapters for aggregator resolvers, environment reads, immutable message receiver evidence and leaf subscription evidence |
| `crates/flotilla-daemon/src/peer/host_overlay.rs` | Plane-A peer identities, configured peers, per-node connectivity and overlay policy |
| `crates/flotilla-credentials/src/lib.rs` | Runtime-facing credential/material operations and agent-environment composition |
| `crates/flotilla-messaging/src/` | Message inbox and receiver snapshots, transport delivery, audit retention, conditions and charter notification routing |
| `crates/flotilla-store/src/load_metrics.rs` | Load metrics: store queue, controller loop and replication counters and latency histograms that load drivers sample (#3206) |
| `crates/flotilla-replication/src/replicator.rs` | Store-to-store replication engine: per-kind list/watch/resume, relay origin filtering, digest repair, peer generations |
| `crates/flotilla-replication/src/ports.rs` | Replication host ports: local stores and node identity, health, direct and routed peer sources |
| `crates/flotilla-daemon/src/server/replication.rs` | Daemon port implementations: `InProcessDaemon` stores/health, `HttpBackend` direct peers, routed watches over the remote command router |
| `crates/flotilla-tui/src/app/mod.rs` | `App` struct, key/mouse dispatch, mode transitions |
| `crates/flotilla-tui/src/app/executor.rs` | Thin executor: routes to core, interprets results into UI state |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Key handling logic per binding mode |
| `crates/flotilla-tui/src/app/navigation.rs` | Navigation logic |
| `crates/flotilla-tui/src/app/ui_state.rs` | App-global layout, tab, status, drag, and pending-action state |
| `crates/flotilla-tui/src/table_view.rs` | Surface-agnostic table projection, state, and `TableIntent` action model |
| `crates/flotilla-tui/src/binding_table.rs` | `BindingModeId`, flat keybinding table |
| `crates/flotilla-tui/src/keymap.rs` | Keymap management, configurable key bindings |
| `crates/flotilla-tui/src/cli.rs` | CLI subcommand parsing |
| `crates/flotilla-tui/src/widgets/` | Widget tree: `screen`, `tabs`, `repo_page`, `overview_page`, `command_palette`, `work_item_table`, etc. |
| `crates/flotilla-tui/src/ui_helpers.rs` | Shared rendering utilities |
| `crates/flotilla-tui/src/event.rs` | Terminal event stream (key, mouse, tick) |
| `crates/flotilla-tui/src/event_log.rs` | In-app tracing log with level filtering |

### Provider Traits

Provider traits live in their type crates:

- **Vcs** (`crates/flotilla-vcs/src/vcs.rs`, `crates/flotilla-vcs/src/providers/git.rs`, `crates/flotilla-vcs/src/providers/git_worktree.rs`)
- **ChangeRequestTracker** (`crates/flotilla-change-request/src/lib.rs`, `crates/flotilla-github-providers/src/change_request.rs`, `crates/flotilla-forgejo-providers/src/change_request.rs`)
- **IssueProvider** (`crates/flotilla-issue-tracker/src/lib.rs`, `crates/flotilla-github-providers/src/issue_tracker.rs`, `crates/flotilla-forgejo-providers/src/issue_tracker.rs`)
- **CloudAgentService** (`crates/flotilla-cloud-agent/src/claude.rs`, `crates/flotilla-cloud-agent/src/codex.rs`, `crates/flotilla-cloud-agent/src/cursor.rs`)
- **AiUtility** (`crates/flotilla-ai-utility/src/claude_api.rs`, `crates/flotilla-ai-utility/src/claude_cli.rs`)
- **TerminalPool** (`crates/flotilla-terminal/src/cleat.rs`, `crates/flotilla-terminal/src/passthrough.rs`)

Provider labels and implementation names live in the `ProviderDescriptor` registered alongside each implementation: `section_label`, `item_noun`, `abbreviation`, and `display_name`.

### Discovery and Provider Construction

Providers are constructed via **factories** (each type crate's `factories` module, assembled by `FactoryRegistry::default_all()` in `crates/flotilla-discovery/src/factories.rs`) that receive an `EnvironmentBag` — a typed collection of `EnvironmentAssertion` values populated by **detectors** (type-specific detectors in their type crates, shared generic probes in `crates/flotilla-discovery-api/src/detectors.rs`, and the default lists in `crates/flotilla-discovery/src/detectors/`).

- **Host detectors** (`HostDetector`) probe the daemon's environment: available binaries, env vars, sockets. They receive an injected `CommandRunner` and `EnvVars` trait — **never use `std::env` directly** in providers or factories.
- **Repo detectors** (`RepoDetector`) probe checkout facts (VCS roots). Forge assertions come from `RepositorySpec::forge()`, not checkout remotes.
- The `EnvironmentBag` provides typed queries: `find_binary()`, `find_env_var()`, `find_socket()`, `find_vcs_checkout()`, `find_remote_host()`.
- Factories call `env.find_binary("tool")` to check availability, `env.find_env_var("KEY")` for env values, and receive a `ProviderConfigView` for user preferences.

**Why injected collaborators?** The daemon may run discovery for remote hosts or container environments where the host process's own env vars and binaries are wrong. The `EnvVars` trait and `CommandRunner` trait abstract this so tests can inject values and environments can provide their own. Never use `std::env` or `std::process::Command` directly in providers — always go through the injected `EnvVars` and `CommandRunner`.

Checkout VCS providers are demand-discovered through `vcs_for_checkout` and cached for the observed Checkout instance. Removing a Checkout retires its cached provider; active operations may retain a lease.

### VCS operation boundary

Use the checkout-scoped `flotilla_vcs::vcs::Vcs` trait for Git and other VCS operations outside its implementation in `crates/flotilla-vcs/src/`. Add a typed operation to the trait when a caller needs a new VCS action. Each VCS provider chooses its implementation: a CLI through the environment runner, or a library. ADR 0060 removed the earlier rule that the CLI is the universal implementation, because an environment may run a minimal flotilla agent beside cleat.

Do not invoke `git` directly through `CommandRunner` methods or `run!`, or through `std::process::Command::new("git")`, outside the VCS implementation. The ast-grep Git boundary check runs in the format job on every PR. Build scripts and build tooling are exempt because they run before a `CommandRunner` exists; tests may use Git to construct fixtures.

### Environment runtime boundary

The environment reconciler and its unchanged `DockerEnvironmentRuntime` port live in `flotilla-provisioning`.

Use `EnvironmentProvider`, its instance-owned `LocalImageCache`, and its `ImageBuildService` capability for environment and image operations. Pre-provisioning tool detection uses `EnvironmentProvider::detect`; read-only image-reference validation uses the owning cache's `inspect_reference` and `registry_available` methods. These observations never pull images or admit mutable tags for provisioning. Select providers by `EnvironmentKind` or a configured instance name, never by a literal runtime name. Direct OCI distribution reads use `RegistryClient`. Candidate validation's read-only registry fallback stays behind the provider cache and uses the host tool's configured credentials. Resource specifications and runtime-neutral traits remain usable by consumers.

The ast-grep operation boundary check in the existing format job rejects literal invocations of `docker`, `podman`, `buildah`, `skopeo`, and `ctr` outside these exact implementation files:

- `crates/flotilla-environment/src/environment/docker.rs`
- `crates/flotilla-environment/src/environment/docker_image.rs`
- `crates/flotilla-environment/src/environment/runner.rs`
- `crates/flotilla-environment/src/factories/docker.rs` (discovery version probe)

Local daemon probing uses the discovered provider instance. Callers without a discovery registry may compose a known command endpoint with `flotilla_environment::environment::command_provider` or `flotilla_discovery::registry::command_provider_registry`: candidate validation deliberately reads the operator host through `ProcessCommandRunner`, and agentless SSH detection uses its remote runner. These helpers do not select a configured cache instance; callers with a registry must use that existing instance. Missing or ambiguous providers publish fresh unknown fulfilment facts, clearing stale affirmative evidence. Malformed successful image inspections are also unknown, rather than reporting an image present without an identity.

Concrete environment adapters and Buildx types belong under `crates/flotilla-environment/src/`; only `crates/flotilla-daemon/src/runtime.rs`, the composition root, may name them elsewhere. This allowance does not permit raw runtime commands or literal provider selection in the composition root. New runtime adapters require an explicit file-level allowlist change. Genuine test fixtures are exempt; production inclusions of fixture files remain checked. Build tooling has no runtime exemption.

### Observed resources and aggregation

Provider refreshes publish discovered checkout facts into the daemon's ephemeral observed-resource backend. Durable resources remain authoritative for desired and adopted state. The Aggregator watches both stores, applies durable-over-observed precedence where a resource is present in both, and emits named-query result sets and deltas for surfaces.

## Conventions

### Resource schema changes

Every PR that changes a resource kind's serialized shape must name the out-of-repo sources that author that kind. Update those sources in a companion change, or explicitly state that none exist. Reviewers should verify the statement and the companion change before approval.

**Schema review before build.** Any change that creates or reshapes generated tables gets an owner review of the resulting schema. That covers a new stored kind, a new or changed storage attribute, and a field added to a stored type. Once the stored types and their generated DDL exist, show the owner the schema and wait for the look-over before writing code that reads, writes or queries those tables. When the change also alters a resource kind's serialized shape, the out-of-repo rule above applies as well.

- Present the schema as a readable view of each table: its column count, which columns come from expanding an enum, and its child tables.
- Call out any table wider than about 30 columns.
- Include the DDL snapshot diff, with those numbers, in the PR description.

See [CODING_STANDARDS.md](CODING_STANDARDS.md) for coding conventions and judgement-call standards.

- **Formatting**: `cargo fmt` — uses `max_width=140` and `use_small_heuristics="Max"`. See `rustfmt.toml`.
- **Inline paths**: Prefer `use` imports over long inline `crate::` paths (>3 segments). Enforced by Clippy `absolute_paths`, with external crates allowed in `clippy.toml` to preserve the former crate-relative scope. `self::` and `super::` paths are unchecked. Add new external dependencies to the allowed-crates list as needed.

## Design and Substrate Work

A lot of work in this repo is substrate design — defining resources, controllers, abstractions, protocols that many things will sit on top of. The default "ship a feature" calibration that prefers smaller scope and deferred abstraction is wrong for substrate work, where the cheap moment to make a sound architectural cut is right now.

Guidance specifically for brainstorming, design discussions, and substrate implementation:

- **Don't reflexively scope down when scope is coherent.** A coherent scope that does the right thing is preferable to a smaller scope that bakes in cuts you'll have to undo.
- **Treat "small framework / abstraction extraction that pays off" as a strong signal, not a YAGNI red flag.** If three controllers will share the same watch loop, extract it now — not when the third one is half-written.
- **When uncertain between "simpler now, refactor later" and "more considered now," lean more considered.** Especially when: multiple consumers will sit on the abstraction, refactoring requires data-shape changes on disk / wire / DB, or you've already pushed back toward the more-considered option once.
- **Disaggregation transitions are expensive; getting the cuts wrong now and changing them is comparatively cheap.** Optimise against the former, not the latter.
- **Don't underspec to "force a refactor later."** That's a polite name for accumulating debt. If the right shape is visible, write the right shape.

This complements (does not replace) the "do not add features no one asked for" instinct elsewhere — that's still right for application-layer work, where YAGNI is a healthy default. The two modes coexist; pick the one matching the work.

## UI Modes

Modal and page behavior is driven by `BindingModeId` in `crates/flotilla-tui/src/binding_table.rs`:

`Shared`, `Normal`, `Overview`, `TabPage`, `TabShell`, `Convoys`, `Project`, `DemandTable`, `ConvoyVessels`, `Help`, `ActionMenu`, `DeleteConfirm`, `DispatchConfirm`, `FindInput`, `CommandPalette`, `FilePicker`

Key bindings are configurable via TOML. Defaults include: `j/k` navigate, `Enter` confirm or drill in, `Space` multi-select in demand tables, `.` action menu, `r` refresh, `[/]` switch tabs, `{/}` reorder tabs, `:` command palette, `/` find, `q` quit, and `h` help.

## Config

Stored in `~/.config/flotilla/`:
- `observation-roots.toml` — host-local paths-only list; all repo-scoped intent lives in replicated Repository/Project specs
- `open-views.toml` — ordered TUI-owned View addresses and label overrides
- `keybindings.toml` — user key binding overrides

Workspace templates: `.flotilla/workspace.yaml` in repo root.

## Issue Types and Labels

Issues have a **type** (lifecycle stage) and **labels** (topic tags). `gh issue create` does not support `--type`, so set it after creation via the API: `gh api -X PATCH repos/flotilla-org/flotilla/issues/N -f type="TypeName"`.

| Type | Use for |
|------|---------|
| `Task` | A specific piece of work |
| `Bug` | An unexpected problem or behavior |
| `Feature` | A request, idea, or new functionality |
| `Brainstorm` | Needs design thinking before it can become a task or feature |

Use labels to tag topics. Combine as appropriate (e.g. `bug` + `ui`, or `from-review` + `refactor` + `quick-win`). Triage-state labels (`needs-triage`, `needs-info`, `ready`, `ready-for-human`, `wontfix`) are a separate, orthogonal vocabulary — see `docs/agents/triage-labels.md`.

| Label | Use for |
|-------|---------|
| `bug` | Something broken or incorrect |
| `enhancement` | New feature or improvement to existing feature |
| `refactor` | Code restructuring for maintainability |
| `ui` | UI/UX and rendering changes |
| `testing` | Test coverage and testing infrastructure |
| `multi-host` | Multi-host peering and networking |
| `protocol` | Wire protocol, message format, handshake |
| `cli` | CLI commands and output formatting |
| `infrastructure` | Build, CI, and tooling |
| `integration` | New provider or tool integration |
| `provider` | Provider infrastructure or implementation |
| `vision` | Long-term direction, ambitious features |
| `documentation` | Docs improvements |
| `from-review` | Issue filed from PR review feedback |
| `quick-win` | Small, well-scoped tasks suitable for agent batches |
| `good first issue` | Good for newcomers |
| `duplicate` | Already exists |
| `wontfix` | Will not be worked on |

## Agent skills

### Issue tracker

Issues are tracked on GitHub at `flotilla-org/flotilla` (always pass `-R flotilla-org/flotilla`); external PRs are not a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Canonical triage roles map to repo labels; `ready-for-agent` maps to the existing `ready` label. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Testing Providers with Record/Replay

Providers (git, GitHub, Claude, etc.) integrate with external systems. The `replay` module captures and replays interactions to enable deterministic, offline testing.

### Pattern

Tests follow a 5-step workflow:

```rust
fn fixture(name: &str) -> String {
    format!("{}/src/providers/<category>/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"), name)
}

#[tokio::test]
async fn test_my_provider() {
    let session = replay::test_session(&fixture("my_fixture.yaml"), masks);
    let runner = replay::test_runner(&session);
    let gh_api = replay::test_gh_api(&session, &runner);
    let http = replay::test_http_client(&session);

    // Inject replay implementations into your provider
    let provider = MyProvider::new(runner, gh_api, http);

    // Assert expected behavior
    let result = provider.some_method().await;
    assert!(result.is_ok());

    // Verify all fixtures were consumed
    session.finish();
}
```

In replay mode (the default), fixtures load from disk and responses are deterministic. In record mode (`REPLAY=record`), real interactions are captured and masking applies to the fixture before saving. In passthrough mode (`REPLAY=passthrough`), real commands execute without reading or writing fixtures — useful for validating that tests work against live execution.

### Fixture Format

YAML fixtures contain `interactions` — a sequence of captured requests and responses. Each interaction has a `channel` tag: `command`, `gh_api`, or `http`.

**Command channel** (shell execution):
```yaml
interactions:
  - channel: command
    cmd: git
    args: [branch, --list, --format=%(refname:short)]
    cwd: '{repo}'
    stdout: |
      main
      feature/foo
    stderr: null
    exit_code: 0
```

**GitHub API channel** (gh CLI or GhApiClient):
```yaml
interactions:
  - channel: gh_api
    method: GET
    endpoint: repos/owner/repo/pulls?state=open&per_page=100
    status: 200
    body: '[{"number": 42, "title": "Fix bug"}]'
    headers:
      etag: 'W/"abc123"'
      total_count: "1"
```

**HTTP channel** (reqwest client):
```yaml
interactions:
  - channel: http
    method: GET
    url: https://api.anthropic.com/v1/sessions
    request_headers:
      authorization: Bearer token-123
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data": [{"id": "s1", "title": "Work"}]}'
    response_headers:
      content-type: application/json
```

Note: `{repo}` in `cwd` fields is a **mask placeholder** (see Masks section). It is replaced with the real path during recording and restored during replay.

### Recording

Capture real interactions:

```bash
REPLAY=record cargo test -p flotilla-core test_my_provider
```

This runs your test against real systems (git, GitHub, Claude API, HTTP endpoints). The test must succeed. On exit, the fixture file is written with all interactions masked.

**Never edit replay fixture YAML files directly.** Always re-record them against the real system with `REPLAY=record`. Fixtures capture real interaction sequences including exact command arguments, response formats, and ordering — hand-editing risks subtle mismatches that pass in replay but fail against the real system. If recording requires setup that can't be automated (e.g. a running cmux instance, an active zellij session), flag it for human intervention.

### Passthrough

Validate tests against live execution without writing fixtures:

```bash
REPLAY=passthrough cargo test -p flotilla-core test_my_provider
```

This is useful on properly-equipped CI runners (e.g. macOS for cmux/claude) to verify that replay fixtures haven't drifted from real behavior.

### Masks

Masks replace sensitive or environment-dependent values (paths, tokens, IDs) with placeholders before saving fixtures. During replay, placeholders are restored.

```rust
let mut masks = Masks::new();
masks.add("/Users/alice/dev/my-repo", "{repo}");
masks.add("/Users/alice", "{home}");
masks.add("ghp_secrettoken123", "{github_token}");

let session = replay::test_session(&fixture("my.yaml"), masks);
```

**Important:** Register longer (more specific) values first. Shorter values can partially match longer ones.

Feature-anchor maintenance and release-build trade-offs are documented in `docs/measurements/2747-build-graph.md`. The contributor changing a dependency owns its anchor update; the guard prints the feature-context differences to reconcile.

# #2948: dev-only testkits and build reuse

The shared helper implementations now live in seven testkit crates. Production
libraries have no `test-support` or `replay` features, and their dependency
graphs exclude testkits and Tokio `test-util`.

| Testkit | Owned helpers |
| --- | --- |
| `flotilla-protocol-testkit` | Provider-data fixture builders |
| `flotilla-store-testkit` | Store contracts and fixtures, liveness harness, virtual clock, read/write counting, legacy-message fixtures |
| `flotilla-discovery-testkit` | Discovery runners, fake providers, environment fixture methods, issue-provider contract |
| `flotilla-replay-testkit` | Recording/replay adapters, command runner doubles, HTTP stand-in |
| `flotilla-orchestration-testkit` | Standing-convoy scenarios and controller-injected setup |
| `flotilla-daemon-testkit` | Peer doubles, in-memory request topologies, explicit digest driver |
| `flotilla-credentials-testkit` | Throwaway GitHub App private key |

The existing generic `flotilla-test-support` remains dev-only. The memory store
itself remains a production adapter: the daemon uses it for observed resources.
Moving that adapter requires the store interface/backend work from ADR 0060,
which is outside this contract. Its test-only counting implementation moved;
production accepts a read observer without changing query behavior.

Production modules stay in place. Testkits use a discovery builder, existing
functional operations and narrow collaborator access, an injected read observer,
and replication scheduling channels. Production retains its default periodic
HTTP replication posture. The testkit explicitly constructs routed sessions.
Provider channel labels remain enabled, preserving the former default replay
feature's labeling behavior without recording/replay implementations.

Rust compiles a library twice for unit tests: the normal library and its
`cfg(test)` unit-test library have distinct trait identities. Core and daemon
include adapter sources from their testkits only under `cfg(test)`, against
aliases of their unit-test crate identity. Their integration tests import the
testkit crates directly. This preserves private unit coverage without duplicating
adapter source or restructuring production modules. There are no production
re-export shims, testkit dependency edges or helper inclusions.

## Enforcement

The build-graph guard rejects production normal/build edges to testkits,
production helper features, and direct or transitive production Tokio
`test-util`. It preserves per-package feature comparison separately for normal
builds and tests; all package-local tests must select the workspace test graph.
Tokio `test-util` appears only in dev-dependency declarations.

The Git boundary guard recognizes testkit fixture source as test code, and still
checks it if a production module includes it. Replay fixture YAML and the stored
record corpus are unchanged.

Two targeted guard mutants were caught and reverted: allowing a production
edge to a testkit, and allowing a production Tokio `test-util` declaration.

## Measurements

The original extraction measurements below precede the conflict-driven rebase
onto `bedce9caa` (the paths/daemon API extraction).

The sequence follows the [#2947 measurements](2747-build-graph.md): warm the
workspace, append a comment to core's `daemon.rs`, build workspace tests, run
workspace tests, then immediately run core tests and daemon integration tests.
The temporary comment was restored. Each command used `--locked` plus
`--message-format=json`; distinct non-fresh `compiler-artifact` package IDs are
counted. No competing Cargo build ran during the sequence.

| Command | Historical #2947 after | This extraction | Recompiled packages |
| --- | ---: | ---: | ---: |
| `cargo test --workspace --locked --no-run` after core edit | 142.60 s / 6 packages | 206.78 s | 12 |
| `cargo test --workspace --locked` | 85.14 s / 0 packages | 92.09 s | 0 |
| `cargo test -p flotilla-core --locked` | 12.74 s / 0 packages | 12.38 s | 0 |
| `cargo test -p flotilla-daemon --locked --test integration` | 4.45 s / 0 packages | 4.52 s | 0 |

All four commands passed. The contract's command-switch reuse holds: neither
package-local command rebuilds the chain. The edited-core rebuild includes the
root binary, aggregator, client, controllers, core, credentials, daemon, TUI and
four new dependent testkits (daemon, discovery, orchestration and replay).
Historical timings are context, not a controlled same-machine speed comparison.

Environment: contained Linux x86_64 vessel, Rust 1.99.0, 32 available CPUs,
`CARGO_INCREMENTAL=0`, workspace dev debuginfo `line-tables-only`, default
`TMPDIR` (unset), and `CODEX_SANDBOX` unset. Third-party debuginfo follows the
contained build defaults.

## Validation

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `python3 ci/build-graph/check.py` and its 15 Python contract tests
- `uv run --with-requirements ci/git-boundary/requirements.txt python ci/git-boundary/check.py` and its 11 Python contract tests
- Both targeted guard mutants fail the contract tests and were reverted.
- The root binary's locked normal/build dependency tree contains no testkits,
  generic test-support crate, or Tokio `test-util` (1,151 dependency rows).

- `cargo build --release --locked` passed for `flotilla` and `flotillad`.
  Demangled symbol scans of both executables found no testkit/generic helper
  namespaces, `ReplayRunner`, `VirtualClock`, `ensure_scenarios`, discovery
  mock runner, or Tokio clock-pause symbols. Dependency feature scans, rather
  than symbol absence alone, establish that Tokio `test-util` is disabled.

## Conflict-driven rebase verification

Rebased onto `bedce9caa`, retaining the landed paths and daemon API owners and
using those lower crates directly from testkits. Cargo regenerated the lockfile
from the base lockfile without changing third-party versions. The new API/type
crates receive dev-only Tokio feature anchors, preserving the production/test
feature boundary.

The exact formatting, Clippy and workspace test gates passed again, as did both
repository guards (15 build-graph tests and 11 Git-boundary tests). The rebased
root normal/build tree has 1,171 dependency rows and still contains no helpers
or Tokio `test-util`. Release executable/symbol evidence above belongs to the
original extraction run.

After the rebased workspace run and Clippy, with no competing Cargo command:

| Command | Seconds | Recompiled packages |
| --- | ---: | ---: |
| `cargo test -p flotilla-core --locked` | 12.67 | 0 |
| `cargo test -p flotilla-daemon --locked --test integration` | 4.75 | 0 |

Both commands passed; the command-switch reuse requirement still holds.

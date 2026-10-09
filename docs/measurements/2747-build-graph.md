# #2747: stable build graph and command switching

Measured on 2026-10-09 in the contained Linux x86_64 crew vessel, with Rust
1.99.0, 32 visible CPUs, `CARGO_INCREMENTAL=0` and
`CARGO_PROFILE_DEV_DEBUG=line-tables-only`. Both runs use the default TMPDIR;
there are no sandbox feature or explicit debug overrides. These are single
observations on this vessel, not reproductions of the issue's kiwi estimates.

## Method

Baseline: `5bda4d1ff9d5863351e73d83779f6edf77f1e1de`, with only an obsolete
extra `None` removed from a daemon unit-test constructor call. That prerequisite
is present in both runs; otherwise workspace tests would not compile. Toolchain
installation and initial compilation are excluded from the measurements.

For each revision, warm `cargo test --workspace --locked --no-run`, append the
same comment to `crates/flotilla-core/src/daemon.rs`, then run the commands below
in order without another edit. Measure wall time with Python's monotonic clock.
Restore the comment after the sequence. The complete workspace test command
precedes the package-local core command, as requested by the contract.

## Results

| Command | Before | After | Recompiled packages before → after |
| --- | ---: | ---: | ---: |
| Core edit + `cargo test --workspace --locked --no-run` | 170.18 s | 142.60 s | 7 → 6 |
| `cargo test --workspace --locked` | 85.71 s | 85.14 s | 0 → 0 |
| `cargo test -p flotilla-core --locked` immediately afterward | 151.22 s | 12.74 s | 57 → 0 |
| `cargo test -p flotilla-daemon --locked --test integration` | Compile error (13 helper visibility/import errors) | Pass, 4.45 s | 52 → 0 |

The incremental core-edit build is 16.2% faster. Before, its recompilation
list includes resources; afterward it contains core, controllers, client, TUI,
daemon and the root package, while resources' tests and examples stay cached.
The package-core switch previously recompiled protocol, relay-protocol,
resources, manifest, core and controllers plus their shared dependencies. It now
prints no `Compiling` lines: test execution, rather than rebuilding the chain,
accounts for its elapsed time.

## Ownership and feature selections

- Controller-backed `in_process_daemon` and `convoy_reconcile` scenarios now
  register in controllers' `tests/integration/main.rs`. The in-process test
  source is byte-identical to its original location.
- Storage-only SQLite and owner-GC tests remain in resources. Controller-backed
  finalizer/restart scenarios move to controllers' `vessel_finalization` module;
  the GC startup barrier remains shared in resources' `tests/common`.
- The `convoy_controller` example moves to controllers, removing the last need
  for either upward resources dev-dependency.
- Existing helper feature names remain supported and are enabled by default,
  including core replay. `flotilla-build-features` has no runtime interface and
  anchors the shared native dependency union and host proc-macro features on
  stable Cargo. Optional TLS providers and sandbox flags remain opt-in; Workers
  WebAssembly builds omit the native anchors.
- This deliberately widens a clean isolated package's dependency build in
  exchange for reuse across commands. Changing dependencies requires keeping
  the anchors aligned. Production missing-socket replication waits for a
  forwarded socket; in-memory harnesses explicitly select routed replication.

## Regression checks

The registered `flotilla-build-features` integration target runs
`ci/build-graph/check.py` and its seven unit tests in the existing workspace
test CI job. It requires Python 3 on PATH (`python` on Windows; `python3` elsewhere). The check rejects upward dev-dependencies, non-default helper gates and
feature drift between every package's default build/test selections and the
workspace union. The initial manifests failed the graph check. Afterward the
check and all seven tests pass.

Three targeted Python mutants were caught and reverted: dropping the upward
edge rule, dropping the helper-default rule, and ignoring feature differences.
The existing federation and overlay scenarios also caught legacy topology
helpers relying on implicit compile-time routing; those helpers now opt in.
No replay fixtures or stored-record corpus were regenerated.

## Verification

Passed the exact documented formatting, Clippy and workspace test gates,
the Git boundary check and its ten tests, the graph checker and seven tests,
and the registered build-features integration target (three tests). The
documented resources/controllers/daemon integration commands, daemon routing
target (64 tests), and Relay Workers WebAssembly check also pass.

Before adding the registered guard, switching from tests to `cargo build
--locked` and back to workspace `--no-run` took 0.21 s each with no recompilation.
The final registered guard preserves the uniform feature selections, checked
through Cargo's actual build/test graphs.

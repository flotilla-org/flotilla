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

That edit path records the historical revisions measured here. #2968 later
moved the module to `crates/flotilla-daemon-api/src/daemon.rs`; editing it now
measures an API edit rather than a core edit. For the later core-edit/client
isolation measurement, see [#2968](2968-client-core-extraction.md).

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
`ci/build-graph/check.py` and its ten unit tests in the existing workspace
test CI job. It requires Python 3.9+ and Cargo on PATH (`python` on Windows; `python3` elsewhere). The check rejects upward dev-dependencies, non-default helper gates and
feature drift between every package's default build/test selections and the
workspace union. The initial manifests failed the graph check. Afterward the
check and all ten tests pass.

Three targeted Python mutants were caught and reverted: dropping the upward
edge rule, dropping the helper-default rule, and ignoring feature differences.
The existing federation and overlay scenarios also caught legacy topology
helpers relying on implicit compile-time routing; those helpers now opt in.
No replay fixtures or stored-record corpus were regenerated.

## Verification

Passed the exact documented formatting, Clippy and workspace test gates,
the Git boundary check and its ten tests, the graph checker and ten tests,
and the registered build-features integration target (three tests). The
documented resources/controllers/daemon integration commands, daemon routing
target (64 tests), and Relay Workers WebAssembly check also pass.

Before adding the registered guard, switching from tests to `cargo build
--locked` and back to workspace `--no-run` took 0.21 s each with no recompilation.
The final registered guard preserves the uniform feature selections, checked
through Cargo's actual build/test graphs.

## Release-build trade-off

This strategy intentionally compiles helper APIs and Tokio `test-util` into
release libraries too. A dev-only anchor would restore the build/test feature
split that this issue removes. Compilation is not a guarantee that these APIs
cannot alter behavior: callers can explicitly pause a Tokio clock, disable
self-origin suppression with the resources fault-injection API, or choose
routed replication through the harness options. Production callers must not
invoke those seams. Tokio time is not paused merely by enabling `test-util`;
its ordinary clock remains running unless a caller explicitly pauses it or
builds a paused runtime. The helpers add callable surface and dependencies,
not automatically running tests. Release linkers can discard unused code, but
this change does not promise an unchanged binary size or execution overhead.

Core's default replay feature also enables channel-label construction in normal
provider calls. That is an intentional diagnostics/overhead change, not a
claim of identical runtime behavior. No recording or playback is activated
solely by the feature. Daemon production construction still uses
`MissingSocketTransport::WaitForSocket`; routed transport is selected explicitly
by in-memory harnesses. The configuration regression test and existing
federation/overlay coverage exercise that distinction. This is a reversible
build-policy choice within the clean-up window, not a new peer protocol.

## Maintaining the feature anchors

The contributor changing a dependency owns the corresponding anchor update in
`crates/flotilla-build-features/Cargo.toml`. After a dependency edit or locked
version update, run:

```sh
python3 ci/build-graph/check.py 2> /tmp/flotilla-feature-diff.txt
cat /tmp/flotilla-feature-diff.txt
cargo tree --workspace --locked --edges normal,build,dev --prefix none --format '{p}|{f}'
```

The checker prints the package/command, selected feature contexts and workspace
feature contexts for every mismatch. Use these as the regeneration input:
add or update anchors for the affected locked versions to select the workspace
union; prefer public umbrella features over private implementation features.
Preserve separate host build-dependency anchors (such as `syn`) and native/OS
target conditions. Do not add optional TLS providers or sandbox switches to the
union. Cargo's feature graph is not invertible to a unique minimal manifest,
so regeneration deliberately requires contributor judgment rather than blindly
copying every transitive feature into a generated manifest. Repeat the checker,
its ten unit tests and the workspace tests until they pass. Removing an anchor
requires the same checks to prove command switching remains uniform.

The checker retains distinct feature sets printed for duplicate host/target
contexts rather than merging their union. A package-local command may omit a
workspace context but may not introduce a different feature set. Cargo tree
does not label context identities in this format, so identical sets remain
indistinguishable; this is a feature-selection guard, not a profile or target
identity verifier. The dependency rule follows both normal and build edges.

The registered guard deliberately fails if Python or Cargo is unavailable,
rather than silently giving a false CI pass. Python 3.9+ and Cargo on PATH are
workspace-test prerequisites, including sandbox-safe invocations. It uses only
Cargo metadata/tree, not builds, socket binding or temporary-path overrides;
Cargo may serialize metadata access behind another process's package-cache
lock. The measured full workspace run includes this guard's subprocess cost.

The checker explicitly requests `cargo --color never` for machine-readable
output. Its registered integration test forces `CARGO_TERM_COLOR=always`,
matching CI, to prevent ANSI duplicate markers being parsed as feature names.

Contributors adding behavior-changing hooks under default helper/replay features
must update the release-build trade-off section above in the same change.
The checker launches one metadata process and one workspace tree plus two trees
per workspace member. A single metadata resolve graph does not describe each
package selection under resolver 2; replacing these calls requires preserving
that per-selection evidence. Revisit the cost if workspace membership grows.

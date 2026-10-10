# #2747: stable build graph and command switching

The original measurements and release trade-off below describe the historical
#2747 revision. #2948 subsequently removed production test helpers and default
replay features. The current #2978 layer policy and measurements are recorded
at the end of this document.

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

## Maintaining the feature anchors (current policy, #2978)

`flotilla-build-features` now anchors only host proc-macro features (`syn`, plus
`proc-macro2` and `quote` defaults to match their existing workspace contexts).
Twelve static, empty sibling anchor crates cover:

| Anchor | Dependency family |
| --- | --- |
| `base` | Serde/JSON and pure hashing |
| `types` | Chrono, URL encoding and collections |
| `os` | Rustix, native OS types and shared initialization |
| `async` | Tokio, Mio and their shared logging/value features |
| `tracing` | Tracing and tracing-core |
| `signing` | Ed25519, DER/PKCS and signing randomness |
| `http` | HTTP clients, TLS and transport utilities |
| `hmac` | HMAC and pure digest dependencies |
| `sqlite` | Bundled SQLite and its shared types |
| `logging` | Subscriber filtering and structured logging |
| `time` | Time formatting/local offset and shared initialization |
| `http-server` | Axum and HTTP serving |

Consumers select only anchors whose dependencies are reachable through their
real normal/build graph at the same locked versions. Selections needed only by
tests go in dev-dependencies. Each anchor also normalizes the existing features
of its own reachable transitive dependencies, so its standalone package commands
pass the same guard. Some declarations intentionally overlap (for example
bitflags in OS and SQLite); they select identical features without adding a
new dependency to either consumer graph. Native and Linux target conditions
remain in the anchor manifests; WebAssembly omits the native dependencies.
No optional TLS provider or operational/sandbox flag is anchored. The twelve
new crates become workspace members through path dependencies; the root
workspace manifest does not change.

The anchors have **no optional feature switches**: their own Cargo artifact
identities stay stable across package selections. A single optional-dependency
anchor would change its own identity even when external feature contexts match,
invalidating downstream consumers. Every anchor's own contexts are checked;
there is no special-case exemption in the feature comparison. The guard compares
each package's production and test graphs with the union of consumers **in its
layer**, rather than imposing the native workspace union on base-only commands.
The base layer consists of protocol, transport, paths, daemon-api and
relay-protocol; other workspace packages use the native layer. A native layer
comparison still checks base dependencies reached by that native consumer.
Production dependencies on testkits, upward dev edges and production Tokio
test-util remain forbidden. Anchor packages needing Tokio select test-util only
in their dev-dependencies.

The guard also checks each named base crate's isolated **Windows GNU
normal/build tree** for SQLite, ring, other known C libraries and compiler-driver
dependencies (`cc`, `cmake`, `autotools`). This catches future C-building anchors
without relying on their names. Rust-only build scripts and proc macros remain
legal. Client, manifest, TUI and the root crate are explicitly exempt because
their real resources dependency still includes SQLite and ring. Shrink that list
with ADR 0060 step 3's store split. The issue amendment withdraws the Windows CI
change: the current cross-check scope and MinGW installation remain unchanged.

After changing dependencies or the lockfile, run:

```sh
python3 ci/build-graph/check.py 2> /tmp/flotilla-feature-diff.txt
cat /tmp/flotilla-feature-diff.txt
cargo tree --workspace --locked --edges normal,build,dev --prefix none --format '{p}|{f}'
python3 -m unittest discover -s ci/build-graph -p test_check.py
```

To add a dependency, first declare the real dependency in its consumer. Select
only anchors already reachable through that dependency graph; place test-only
anchors in dev-dependencies. Run the guard above, then update the corresponding
anchor's feature declarations for reported context drift, including transitive
dependencies. Re-run the guard for both production and dev edges.

The contributor owns the static anchor declarations and affected consumers'
production/dev selections. Reconcile reported dependency contexts within the
corresponding layer; do not add an unrelated native anchor merely to match a
workspace-wide union. Cargo's feature graph is not invertible to one minimal
manifest, so this remains a deliberate contributor decision. Run the documented
format, Clippy, test and Git boundary gates before pushing.

The checker retains versions and distinct host/target feature sets instead of
merging their union. A selected command may omit a layer context but may not
invent a different set. Cargo tree does not label context identities in this
format, so identical sets remain indistinguishable; this is a feature-selection
guard, not a profile or target identity verifier. Metadata/tree commands use
`--locked` and `--color never`. The guard needs Python 3.9+ and Cargo, does not
build dependencies or bind sockets, and requires no TMPDIR override. It remains
registered in the existing workspace-test job; no CI topology changes.

## #2978: static layer anchors (2026-10-09–10 UTC)

Baseline: `f2daa69a1bbf83572a9532bc82092a0b135df937`. Measured on the same
contained Linux x86_64 vessel with Rust 1.99.0, 32 visible CPUs, eight Cargo
jobs, `CARGO_INCREMENTAL=0` and the inherited line-tables-only dev debug
setting. Both revisions use the default TMPDIR without sandbox feature or
explicit debug overrides. These are single observations, not statistical
performance estimates.

Count unique `{p}` identities in each isolated normal/build Windows GNU tree:

```sh
cargo tree --locked -p <package> --target x86_64-pc-windows-gnu \
  --edges normal,build --prefix none --format '{p}'
```

Include the selected package and anchor crates, retain distinct versions and
proc-macro labels, and collapse Cargo's `(*)` duplicates.

| Package | Before | After |
| --- | ---: | ---: |
| `flotilla-protocol` | 199 | 102 |
| `flotilla-transport` | 200 | 103 |
| `flotilla-paths` | 203 | 106 |
| `flotilla-daemon-api` | 202 | 105 |
| `flotilla-relay-protocol` | 188 | 30 |
| `flotilla-client` | 231 | 205 |

All five named base graphs now exclude SQLite, ring and C compiler drivers.
The client still reaches those dependencies through its real resources edge;
it has fewer unrelated anchors, but remains exempt pending the store split.

For the cold client check, use separate initially empty Cargo target directories
with the same already-downloaded registry sources and toolchain. Run
`cargo check -p flotilla-client --locked` once in each revision. The baseline
uses an archive outside the vessel checkout; the after run uses the working
branch. Cold elapsed time is **21.71 s → 22.52 s**. This small
difference is within ordinary single-run variation; base graph isolation is
the substantive result.

For switching, warm both exact documented commands in the same default target
directory, then run Clippy immediately after tests and tests immediately after
Clippy, without an intervening edit. Count `Compiling` and `Checking` lines as
recompiled packages. Test elapsed time includes execution and the registered
Cargo/Python graph guard. The first test run after warming Clippy took
327.32 s before and
320.81 s after, including initial code generation; that setup
cost is distinct from the warm switches below.

| Warm command switch | Before | After | Recompiled packages before → after |
| --- | ---: | ---: | ---: |
| Tests → `cargo clippy --workspace --all-targets --locked -- -D warnings` | 0.38 s | 1.75 s | 0 → 1 |
| Clippy → `cargo test --workspace --locked` | 91.23 s | 97.92 s | 0 → 1 |

The #2747 trade-off is narrower: dependency features are reusable within each
layer, while isolated base commands avoid compiling unrelated server/store
libraries. Native consumers still select their real HTTP/store dependencies.
Twelve small empty anchor crates add package identities and Cargo-tree queries;
static identities avoid invalidating consumers when command selection changes.
Only the root package rebuilt in both warm after switches; library packages
stayed cached.
Cross-layer command reuse is not promised. Release profiles and runtime source
code are unchanged, and no test helpers are added to production.

The updated guard's 21 tests cover all five names independently of the policy
implementation, compiler-driver dependencies, duplicates, empty/Rust-only
graphs, the four resources exemptions, anchor contexts and CLI Windows wiring.
The new C rule rejects all five baseline trees (cc, SQLite and ring). Five
mutants were caught and reverted in isolated copies: remove the C ban (45
failures), ignore feature drift (3), omit transport (10), bypass Windows
validation (10), and skip anchor contexts (1). No fixtures or stored-record
corpus were regenerated. The registered integration target runs these checks
in the existing workspace CI job; no workflow change is made.

Review regression: both async and HTTP-server anchor scenarios allow Tokio
`test-util` on dev edges and reject transitive production activation. Disabling
that production check makes both negative scenarios fail.

## #2994: target-specific feature contexts (2026-10-10 UTC)

The macOS graph at `cbb6221f6` reproduced all ten reported differences when
queried from Linux with `--target aarch64-apple-darwin`. Unlike Linux, sha2's
cpufeatures path reaches libc, rustix reaches libc and errno, and hyper-util's
system-configuration path reaches bitflags. Their isolated anchor selections
lacked the default/std features selected by the rest of the same layer.

Three macOS-only dependency sections now normalize those existing transitive
packages: libc in base on Apple Silicon, libc/errno in OS, and bitflags/std in
HTTP-server. The base libc declaration matches cpufeatures' architecture gate
so Intel macOS does not gain an unrelated dependency.
They introduce no new locked package versions or runtime source changes.
Linux anchor lines, the C-free policy/exemptions and CI topology are unchanged.

The guard accepts `--target <triple>` and applies it to both the layer reference
and every package-local build/test tree. Without that flag it retains Cargo's
host selection, including Cargo configuration. The separate Windows C-free
queries remain fixed to Windows GNU. This makes target validation available
from Linux without installing another target's standard library or compiler.

Validation commands and output:

```text
$ python3 ci/build-graph/check.py --target aarch64-apple-darwin
Workspace build graph (aarch64-apple-darwin): C-free Windows base; production excludes testkits and test-util; layer-local build/test features reusable
$ python3 ci/build-graph/check.py --target x86_64-apple-darwin
Workspace build graph (x86_64-apple-darwin): C-free Windows base; production excludes testkits and test-util; layer-local build/test features reusable
$ python3 ci/build-graph/check.py --target x86_64-unknown-linux-gnu
Workspace build graph (x86_64-unknown-linux-gnu): C-free Windows base; production excludes testkits and test-util; layer-local build/test features reusable
```

The CLI regression generates Linux, macOS and Windows targets, with matching
contexts and independent production/test drift controls. Its subprocess fake
represents Cargo's process boundary and a macOS-only libc dependency. A target
must be used by both reference and selected queries; checking the Linux graph
instead cannot silently pass. Empty libc feature sets on either macOS edge
selection are rejected. The existing registered Rust integration suite runs
all 22 Python tests without a new CI job.

Mutation checks in isolated copies: omit target propagation (nine failed
subcases), and disable feature-difference rejection (five failed assertions).
Both were caught and reverted. The real macOS graph also failed before the
anchor fix and passed afterward.

A full Windows GNU feature run reports the same 44 winapi/windows-sys
context diagnostics as the unmodified baseline; this fix does not normalize
the native Windows graph.
The mandatory Windows C-free base queries still pass, and target feature drift
is rejected rather than hidden. No macOS build or test execution is claimed:
these are Cargo graph queries from Linux.

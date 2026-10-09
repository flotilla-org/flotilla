# Daemon credential and aggregator extraction (#2748)

Measured on 2026-10-09 in the same contained crew vessel, from `60a4fae589`
(the merged #2747 build-graph change), before any delivery rebase.
Rust was `1.99.0 (b940084d7 2026-09-28)`, `CARGO_INCREMENTAL=0`,
`CARGO_BUILD_JOBS=8`, with the injected Rust workspace wrapper and
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` unchanged. `CARGO_PROFILE_TEST_DEBUG`
was unset. The default TMPDIR was used (TMPDIR unset), with no additional profile
overrides; `CARGO_TARGET_DIR` was unset (the checkout's `target/` was reused).

## Interface and ownership

```text
flotilla-daemon runtime
  ├─ flotilla-credentials
  │    ├─ CredentialStore / AgentMaterialRegistry: injected storage, environment, runners
  │    ├─ CodexCentralRefresher: central credential refresh
  │    └─ compose_agent_environment: opaque claims → shell file + launch values
  └─ flotilla-aggregator::run
       ├─ private watch-source and provider-resolver wiring
       ├─ private query projection/readiness
       └─ private demand-backed issue materialization
```

Both crates depend downward on core, protocol, and resources; neither depends
on daemon. The daemon retains controller supervision, restart policy, and runtime
credential reconciliation. The aggregator crate exposes a run future and polling
health handle, rather than its sixteen-resolver builder or implementation traits.
The credential crate hides the configuration policy engine; its `Fragment` is an
opaque claim produced by credential and agent-material operations. The runtime
can compose agent environments but cannot manufacture arbitrary target/key/merge
policies through a public builder. Private collaborators stay private. Runtime-facing async operations return boxed
`Send` futures, so their implementation state machines and `Send` proofs stay
inside the owning crate rather than expanding into daemon task types. This adds
one allocation and dynamic future dispatch per exported operation; task ordering,
I/O, cancellation, errors, and returned values remain unchanged.

Tests and throwaway RSA fixtures follow their implementation. A small default
`test-support` feature exposes the same throwaway private key to the daemon's
runtime HTTP test. The frozen-skill validator remains re-exported by daemon for
the existing CLI consumer; runtime and image distribution use the new crate
interface directly. The frozen `peer/` implementation is untouched.

## Measurement method

The target directory and downloaded dependencies remain warm. Each phase first
runs both commands below to warm the build/check and test profiles; these warm-up
runs are excluded from the comparison. A measured daemon edit appends one comment
line to `crates/flotilla-daemon/src/lib.rs`. Each measured group edit appends one
comment line to `credential.rs` or `aggregator.rs`, at its before/after location.
After each edit, run these commands in order, then restore the exact file bytes:

```bash
cargo check -p flotilla-daemon --locked
cargo test -p flotilla-daemon --lib --no-run --locked
```

Python `time.monotonic()` measures subprocess wall time, including Cargo and
linking. No other build or test command runs in this vessel during the measured
commands. These are single samples, sensitive to host load and filesystem caches;
they measure source invalidation without Cargo incrementals, not cold downloads
or cold workspace builds. Restoring a file also invalidates its mtime. After the
credential edit, the restored credential crate is warmed again before the
aggregator measurement,
so each measured leaf edit includes only that leaf and its daemon consumer.
Before the split every invalidation is in the same daemon crate and every edit
rebuilds one daemon unit-test binary;
after the split the command builds the daemon binary and affected leaf libraries,
but not the leaf crates' separate unit-test binaries.

| Metric | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Daemon type-check (daemon edit) | 38.385 s | 33.280 s | 13.3% |
| Daemon unit-test binary (daemon edit) | 101.020 s | 80.988 s | 19.8% |
| Credential edit: owning package check | 38.479 s | 0.896 s | 97.7% |
| Credential edit: owning package unit-test binary | 95.114 s | 3.929 s | 95.9% |
| Aggregator edit: owning package check | 38.569 s | 1.648 s | 95.7% |
| Aggregator edit: owning package unit-test binary | 99.252 s | 4.712 s | 95.3% |

For a leaf-only edit, before the owning package is daemon; after it is the new
leaf package. Both profiles are warmed first, and the same one-line comment edit
is used. These local commands are:

```bash
cargo check -p flotilla-credentials --locked
cargo test -p flotilla-credentials --lib --no-run --locked
cargo check -p flotilla-aggregator --locked
cargo test -p flotilla-aggregator --lib --no-run --locked
```

If the workflow also rebuilds the daemon consumer after a leaf edit, use the
same daemon commands in both phases. Those measurements are:

| Edit / daemon command | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Credential edit: daemon check | 38.479 s | 33.621 s | 12.6% |
| Credential edit: daemon unit-test binary | 95.114 s | 83.152 s | 12.6% |
| Aggregator edit: daemon check | 38.569 s | 35.209 s | 8.7% |
| Aggregator edit: daemon unit-test binary | 99.252 s | 87.219 s | 12.1% |

The separate leaf unit-test binaries should be selected explicitly when working
inside those groups:

```bash
cargo test -p flotilla-credentials --locked
cargo test -p flotilla-aggregator --locked
cargo test -p flotilla-daemon --locked --test integration aggregator::
```

No extra helper features are needed. The shared native feature anchor and
build-graph guard cover both new packages' build and test selections.

## Behavior preservation

The before/after combined unit-test inventories have exactly the same 719 test
names: 476 remain in daemon, 140 move to credentials, and 103 move to aggregator.
The aggregator and readiness implementation and the credential HTTP-contract
suite move byte-for-byte. All six moved test modules retain byte-identical source; changes outside them
are visibility, boxed runtime futures, private configuration-builder access,
and fixture/import wiring.
Runtime integration tests remain in daemon because they exercise supervision and
composition across the new crate interfaces.

Validation passes with the default TMPDIR and no extra helper flags:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `cargo test -p flotilla-credentials --locked` (140 passed, none ignored)
- `cargo test -p flotilla-aggregator --locked` (103 passed, none ignored)
- `python3 ci/build-graph/check.py` and its 10 Python unit tests
- Git-boundary checker and its 10 Python unit tests, in the documented isolated environment

Two targeted mutations demonstrate that the moved tests can still fail:
dropping NUL-value rejection fails
`agent_environment_rejects_invalid_names_and_values`; reversing local source
precedence fails
`shadowed_convoy_source_cannot_replace_the_effective_change_request`.
Both mutants were reverted before the passing package and workspace runs.

# Integration-test consolidation: phase 1b of #2538

Measured on 2026-10-03 in the same crew vessel, starting from commit `84912025`,
before rebasing onto newer main.
Each build used an empty `/tmp/phase1b-target` with the same environment and command:

```bash
CARGO_TARGET_DIR=/tmp/phase1b-target cargo test --workspace --no-run --locked
```

The target path was empty at the start of each measured build. Cargo's downloaded dependencies
remained available; this measures cold target builds, not cold downloads or OS caches.
Wall time includes compilation and linking, but excludes target deletion and inventory.
The default TMPDIR was used (TMPDIR unset). Rust was `1.94.1`, with
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=8`, and the injected
`RUSTC_WORKSPACE_WRAPPER=/usr/local/bin/flotilla-rustc-wrapper` unchanged.
No dev/test debug profile environment overrides were set. Phase 1a profile changes
are outside this comparison.

| Metric | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Integration binaries in the three packages | 52 | 4 | 92.3% |
| Workspace test executables | 80 | 32 | 60.0% |
| Cold workspace no-run wall time | 300.3 s | 216.1 s | 28.0% |
| Target file bytes (decimal GB) | 51.15 | 29.19 | 42.9% |
| Test executable bytes (decimal GB) | 35.83 | 13.88 | 61.3% |

Size is the sum of regular-file lengths immediately after the measured build,
not allocated disk blocks. Executable counts come from Cargo's `Executable` output.
These are single runs; timing is sensitive to host load and filesystem cache warmth.

Each package now has `tests/integration/main.rs`, with one module per consolidated root.
Daemon request-session scenarios retain their standalone target, with the original
source moved verbatim to `tests/request_session_pair/main.rs`.
An initial single-daemon-binary layout intermittently exceeded Hegel's 30-second
slow-generation health check, including without competing compilation. The original
standalone request-session suite passed. Keeping that target intact preserves its scheduling isolation and Hegel test identities without
relaxing health checks or assertions.
The shared harness stays at `tests/common/`, compiled once per integration crate,
and fixtures stay at their existing paths. No feature/platform gates or test bodies
were removed. The workspace commands continue to discover all integration tests.

Comparing executable `--list` output before and after, after adding the former
root's module prefix where consolidated, preserves exactly 475 resources tests, 135 controllers tests,
and 119 daemon tests. The workspace total stays at 3,949 listed tests.
Use `--test integration <module>::` (or daemon `--test request_session_pair <test-name>`)
to select an area previously selected with `--test <module>`. The core `in_process_daemon` target remains unchanged.

The delivery rebase picked up #2533's tombstone generation-budget fix and #2535's
new `watch_allocations` target. That target stays separate because its process-wide
counting allocator must not instrument unrelated tests. The rebased tree therefore
has five integration binaries across these packages (resources two, controllers
one, daemon two); the table compares the original workload before those upstream
changes. Workspace gates are rerun after the rebase.

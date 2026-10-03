# Crew debuginfo profile — October 2026

Phase 1a of [#2538](https://github.com/flotilla-org/flotilla/issues/2538), measured 2026-10-03 in its dispatched vessel. This change supplies line-tables-only workspace debuginfo and disables third-party dependency debuginfo in contained builds and CI. It does not change integration-test roots or add a compiler cache.

## Method

The vessel ran Linux x86_64 (kernel `7.2.8-1-cachyos`), rustc 1.94.1 (`e408947bf`), Cargo 1.94.1, an eight-CPU container quota, `CARGO_BUILD_JOBS=8`, `CARGO_INCREMENTAL=0`, and the existing workspace rustc wrapper with `FLOTILLA_LINKER_THREADS=8`. There was no compiler cache. `TMPDIR` was left at its default. Each profile started in a separate empty target outside the repository. The commands ran sequentially, avoiding competition between these two builds, though other host activity was not controlled.

The baseline used the vessel's original full-debug profile. The after run set `CARGO_PROFILE_DEV_DEBUG=line-tables-only` and prepended the exact staged Cargo shim from `runtime.rs` to `PATH`, supplying `profile.dev.package."*".debug=0`. Both measured the checkout based on `84912025e320e8d5da97dc10a7e6907f72ab4dbc` with phase-1a glue under development; the measured profile selection was finalized before the after run. Subsequent changes preserve subprocess PATH, guard against path aliases back to the shim, add a missing-Cargo diagnostic, and identify stale parent test shims with a stable marker. They do not change the measured compiler profile. The baseline includes the new glue but does not activate the profile defaults. These are single samples, measuring profile selection rather than a statistical performance benchmark.

For each target:

```bash
cargo test --workspace --locked --no-run --timings
cargo clippy --workspace --all-targets --locked --timings -- -D warnings
```

Clippy reused the test target, so it measures the first clippy pass in a normal test-then-clippy validation round, rather than another independent cold build. Wall times use Python's monotonic clock around the complete Cargo process. Target bytes use `du -sb` (apparent bytes); executable bytes below count executable regular files under `debug/deps`, excluding shared libraries. Cargo timings identify whole compile-and-link units, not isolated linker time.

## Measurements

| Measurement | Full debug baseline | Reduced debug | Change |
|---|---:|---:|---:|
| Cold test compilation wall | 296.79s | 203.95s | -31.3% |
| First all-target clippy wall | 156.46s | 110.66s | -29.3% |
| Target after test compilation | 45.41 GB | 14.66 GB | -67.7% |
| Target after clippy | 45.83 GB | 15.80 GB | -65.5% |
| Executables in `debug/deps` (82 in each target) | 37.51 GB | 11.54 GB | -69.2% |

| Compile-and-link unit | Full debug | Reduced debug |
|---|---:|---:|
| `flotilla-core lib (test)` | 105.36s | 59.67s |
| `flotilla-daemon lib (test)` | 104.77s | 63.11s |
| `flotilla-daemon` | 100.99s | 54.02s |
| `flotilla-resources lib (test)` | 56.92s | 26.44s |
| `flotilla-core` | 50.23s | 39.25s |
| `flotilla-resources test "controller_loop" (test)` | 23.13s | 9.70s |
| `flotilla-core test "in_process_daemon" (test)` | 19.59s | 7.31s |

## Interpretation

The complete measured test-then-clippy round fell from **453.3s to 314.6s (30.6%)**. The cold test compilation fell by **31.3%**, the test target by **67.7%**, and executable bytes under `debug/deps` by **69.2%**. The daemon test binary fell from **1,080 MB to 430 MB**; its `.debug*` sections fell from **800 MB (74.1%) to 164 MB (38.2%)**. Line tables still occupy meaningful space, but retaining them preserves workspace file and line backtraces. The largest whole compile-and-link units became substantially shorter. These samples support shipping the profile change; they do not establish a fleet-wide or CI wall-time guarantee.

The [July build profile](build-profile-2026-07.md) found test-binary multiplication and large core/daemon units, as well as 19 GiB of incrementals in a long-lived 32 GiB desk target. These vessel measurements have incrementals disabled, and still reproduce large targets and large test executables under full debuginfo. July's Apple M4 timings and marginal test build following a dev build are not directly comparable to this Linux cold test build. The shared observation is that test executable size and repeated workspace compilation remain substantial even when dependency compilation and incremental storage are controlled.

## Validation and limits

The real offline Cargo regression workspace checks a workspace library used as a dependency, an excluded path dependency, line-tables-only/full/off workspace profiles, desk defaults, repository config, explicit `RUSTFLAGS`, paths containing spaces, and `+toolchain` forwarding, and isolation from a stale staged parent shim. A separate test checks missing-Cargo exit status and diagnostics, directory and file symlinks, trailing slashes, empty PATH entries, and `sh cargo` invocation. The toolchain-selector case is skipped when rustup is absent; the remaining real Cargo matrix still runs. The existing provisioning capture checks delivery of the shim and workspace default. Six targeted mutations were caught: restoring dependency full debuginfo, clobbering an explicit workspace full-debug override, dropping the shim from Cargo subprocess PATH, removing the missing-Cargo diagnostic, treating a self-alias as a successful invocation, and requiring a rustup toolchain again.

The crew-facing brief now documents `CARGO_PROFILE_DEV_DEBUG=full cargo test --workspace --locked`; that command retains full workspace debuginfo while dependencies remain at zero. Desk builds without the contained environment retain full workspace debuginfo. No resource serialized shapes change, and no out-of-repo resource authors need updates.

Final validation passed with the staged contained tool environment: `cargo +nightly-2026-03-12 fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked`. The first full-suite run hit an unrelated `ETXTBSY` while spawning the temporary zellij sink executable; that test and the full suite passed on retry. The final fixture also isolates its tool environment from any shim already running the parent suite, avoiding layered test shims. Workflow YAML parsed successfully and all nine compiling Cargo commands carry the dependency override.

# Resource/store split measurements (#2980)

Baseline: `cbb6221f6`, before the split. After: this change. Both use the pinned
Rust 1.99.0 compiler, the contained crew's default Cargo profile, no incremental
builds, default TMPDIR, downloaded dependency sources, and a fresh target directory
for each package and each measurement. These are single cold-check samples, not a
statistical benchmark. Other workspace checks ran on the same vessel.

| Package | Dependencies before | Dependencies after | Cold check before | Cold check after |
|---|---:|---:|---:|---:|
| `flotilla-client` | 194 | 124 | 19.00 s | 15.92 s |
| `flotilla-tui` | 298 | 300 | 50.17 s | 49.87 s |

Count unique `(package name, version)` pairs from
`cargo tree --locked -p <package> --edges normal --prefix none`, excluding the
selected package itself. This measures the production graph for the native Linux
host, including the extracted crates, and excludes build/dev dependencies.
Time `cargo check --locked -p <package> --target-dir <fresh directory>` with
Python's `time.monotonic()` around the subprocess. Every measured check exited 0.

The client no longer compiles SQLite, object storage, or TLS. The TUI continues to
compile core, which uses store, and manifest, which uses TLS. Its graph adds the
two extracted crates and its cold-check time is effectively unchanged. Removing
those fixed-neighbour dependencies requires subsequent ADR 0060 extractions.

The Windows production guard now enforces the C-free graph for resources and
client. Manifest remains native because its HTTP sink uses ring-backed TLS;
TUI and the executable remain native because core uses the store. Manifest and
TUI import store only in dev-dependencies for their existing integration coverage.

## Feature-anchor ownership

On native targets, `flotilla-store` retains the backend/runtime anchors: umbrella,
async, base, hmac, http, os, sqlite, tracing and types. `flotilla-tls` retains the
TLS/client anchors: umbrella, async, base, http, os, tracing and types. Both use
logging only as a dev anchor; TLS also enables Tokio test-util in dev builds to
match the workspace test feature selection. Dependency changes must keep build
and test selections aligned with `ci/build-graph/check.py`.

The Relay Workers job builds and lints `flotilla-relay` for
`wasm32-unknown-unknown`. Its production/build dependency tree includes none of
`flotilla-store`, `flotilla-tls`, or `flotilla-resources`; the native anchor blocks
in the extracted crates are outside that job's graph. Verified with
`cargo tree --locked -p flotilla-relay --target wasm32-unknown-unknown --edges normal,build`.
The exact Relay Workers build and Clippy commands from `ci.yml` also pass.

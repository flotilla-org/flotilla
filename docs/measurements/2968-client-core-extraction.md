# Client rebuild isolation after extracting paths and daemon-api (#2968)

The client now depends directly on `flotilla-paths` and `flotilla-daemon-api`
instead of `flotilla-core`. It retains protocol, transport and resources. The
build-graph guard rejects normal/build paths from client to core, including
transitive paths.

## Measurement

Measured in the same contained Linux checkout, Rust 1.99.0, default TMPDIR,
workspace dev profile (line-tables-only debug), `CARGO_INCREMENTAL=0`, using the
same target directory. Warm `cargo build -p flotilla-client --locked` before
each measurement; append one comment line to `crates/flotilla-core/src/lib.rs`,
time `cargo build -p flotilla-client --locked` with Python's monotonic clock,
then restore the source. No other Cargo build ran during either timed command.
These are single observations, not a statistical benchmark or a cold-build
comparison.

| Client build after one-line core edit | Wall time | Cargo work |
| --- | ---: | --- |
| Before extraction | 44.150 s | Compiled core and client |
| After extraction | 0.137 s | No crates compiled; Cargo reported 0.10 s |

`cargo tree -p flotilla-client --locked` contains no `flotilla-core` after the
extraction. Its direct workspace production dependencies are:

```text
flotilla-client
├── flotilla-build-features (existing native feature selections)
├── flotilla-daemon-api
├── flotilla-paths
├── flotilla-protocol
├── flotilla-resources
└── flotilla-transport
```

Resources still brings storage dependencies into the client as allowed by the
contract; this measurement makes no claim about removing those dependencies.

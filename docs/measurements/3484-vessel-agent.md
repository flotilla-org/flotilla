# Vessel-agent skeleton and TUI split (#3484)

The CLI links `flotilla-vessel-agent` directly. The agent crate owns only startup,
environment configuration, stderr logging and shutdown signals. Its production
workspace dependencies are feature-only build anchors, checked by the build-graph
guard. It has no store, reconciler or provider dependencies. Listeners, hello,
readiness and supervision arrive in the next ADR 0069 slices.

`flotilla-cli` owns the argument tree, command output, image-check client, PM
connector and CLI terminal attachment. `flotilla-tui` reexports the previous CLI
and connector paths for callers, and owns rendering and terminal restoration.
The root package's default-on `tui` feature preserves `cargo run`, `flotilla`,
and `flotilla view …` for operators.

## Build selections

```sh
# Operator CLI with dashboard (existing release selection is unchanged).
cargo build -p flotilla --bin flotilla --release --locked

# Vessel CLI with the resident agent and no dashboard dependency.
cargo build -p flotilla --bin flotilla --release --locked --no-default-features

# Check the actual selected vessel graph, including transitive dependencies.
cargo tree -p flotilla --no-default-features --locked --edges normal,build
python3 ci/build-graph/check.py
```

The feature must be omitted when building the binary intended for vessel delivery.
This ticket does not switch the fleet candidate/release build. A no-TUI binary
still supports CLI operations and `vessel-agent`; invoking its dashboard reports
that an operator build with `tui` is needed.

## Startup contract

```sh
flotilla vessel-agent --help
FLOTILLA_ENVIRONMENT_ID=env-example \
FLOTILLA_DAEMON_SOCKET=/run/flotilla-daemon/flotilla.sock \
RUST_LOG=info flotilla vessel-agent
```

The environment provider already injects these identity and parent-link variables.
The identity must be nonempty UTF-8; the socket path must be absolute and may
contain native non-UTF-8 bytes on Unix. The skeleton does not open that socket.
Logging goes to stderr, defaults to `info`, and accepts `RUST_LOG` filtering.
Invalid configuration or filtering fails startup. SIGINT and SIGTERM stop the
resident skeleton cleanly. It explicitly logs that it has no listeners.

## Size measurement

Measured on x86_64 Linux with the pinned Rust 1.99.0, ordinary Cargo release
settings, no stripping and no release-profile changes. The baseline was built
from the unmodified issue checkout before extracting any source. Both selections
use the same toolchain, lockfile dependency versions and build environment.

| Tool/asset | Before (bytes) | Vessel build (bytes) |
|---|---:|---:|
| flotilla | 70,086,680 | 66,346,432 |
| cleat | 6,651,264 | 6,651,264 |
| libghostty-vt.so.0 | 12,597,640 | 12,597,640 |
| xterm-ghostty | 3,884 | 3,884 |
| Total | 89,339,468 | 85,599,220 |

The CLI shrank by 3,740,248 bytes (5.3%); the complete tool set shrank by 4.2%.


The frozen tool set also includes cleat's shared library and terminfo, so both
are counted alongside the two executables. Cleat and its assets are the supplied
fleet artifacts, unchanged across the comparison; this measures the CLI split,
not a cleat rebuild. Byte counts dereference symlinks and exclude directory and
symlink metadata, compression and filesystem allocation overhead.

## Static build probe

A complete static tool set was **not demonstrated** in this environment:

- `x86_64-unknown-linux-musl` is not installed in the pinned toolchain.
- `rustup target add x86_64-unknown-linux-musl` downloaded the component but failed
  to create its target directory in the read-only toolchain (`Permission denied`).
- `musl-gcc` is absent.
- The supplied cleat artifact uses `libghostty-vt.so.0`; that tool set is currently
  dynamic regardless of the CLI's build selection.

This is an environment-limited probe, not evidence that musl is incompatible with
the source. A successful static build needs a writable musl-capable toolchain and
a static cleat/ghostty selection. Release builds remain unchanged as requested.

# Cleat launch-environment contracts

`revisions.sh` is the single source of old/additive and new/declared cleat pins.
The operator-applied workflow in PR #2837 runs the Linux PR CI
shared scenario matrix against the fake and both actual Ghostty-enabled binaries
when terminal, discovery, build inputs or this rig change.
Missing binaries fail the explicitly enabled real suite; ordinary workspace tests
run the same matrix against the fake without downloading/building cleat.

Reproduce using a scratch checkout outside the vessel checkout:

```sh
scratch=$(mktemp -d)
git clone https://github.com/flotilla-org/cleat.git "$scratch/source"
ci/cleat-environment/build.sh "$scratch/source" "$scratch/bin"
unset TMPDIR
FLOTILLA_TEST_CLEAT_OLD="$scratch/bin/old/cleat" \
FLOTILLA_TEST_CLEAT_NEW="$scratch/bin/new/cleat" \
  cargo test -p flotilla-core --locked --lib --features cleat-environment-contract launch_environment_contract -- --nocapture
```

The build checks out both pins in the caller's scratch clone and leaves it detached
at the new revision. Use a disposable clone as shown above.

The build uses each source revision's Rust, Zig and Ghostty pins, verifies Zig's
checksum, and records binary revision/version/hash. Each scenario uses a private
short runtime directory beneath `/tmp`, subprocess-local environment pollution,
command/read deadlines and daemon cleanup on assertion failure. It never connects
to the user's cleat daemon. Child values are captured by an executable first-exec `SHELL` probe before
login-shell startup can rewrite them; embedded newlines, empty values and shell syntax survive.

The old binary deliberately preserves an independently contaminated daemon's
ambient values; the new binary must exclude them. Both must preserve on-demand
controlled startup, vessel values, last explicit overrides, and refuse managed
coordinates. Removing the managed-key filter in `session_environment` is the
#2826 negative control: the real launch must fail with cleat's refusal.

The real probe requires `/usr/bin/python3`; the suite checks it before launching.
Each old/new scenario is a separate test result. Successful real scenarios assert
the private PID-file layout and verify that fixture cleanup stops the daemon.
Builds use separate Cargo target directories and retain the effective prepared
Ghostty toolchain file before restoring the source manifest.

The operator-owned workflow should restore the entire binary output directory
using the key printed by `ci/cleat-environment/cache-key.sh`, and skip clone/build
only on an exact hit. The key includes both source revisions (which pin each
revision's Rust, Zig and Ghostty), preparation scripts, and the Flotilla toolchain.
Keep the provenance beside the binaries in the cache and upload it on hits too.

Keep the provisioned contract in its separate Linux job: enabling its feature in
the ordinary workspace test run would require both real binaries on every Test
runner and would make missing provisioned tools fail unrelated tests. Restore a
workspace-inclusive `cleat-environment` Rust cache to reuse its feature-specific
core test binary and dependencies; save that Rust cache only from `main`. Sharing
the ordinary Test job's dependency cache alone does not reuse the feature-specific
core test executable. Measure the warm job separately from the cold source builds.

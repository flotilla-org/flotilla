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

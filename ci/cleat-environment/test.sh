#!/usr/bin/env bash
# Run the same provisioned checks locally and from the operator-owned workflow.
set -euo pipefail
: "${FLOTILLA_TEST_CLEAT_OLD:?select the pinned old cleat}"
: "${FLOTILLA_TEST_CLEAT_NEW:?select the pinned new cleat}"
unset TMPDIR
ci/toolchain/assert.sh
cargo clippy -p flotilla-core --all-targets --locked --features cleat-environment-contract -- -D warnings
cargo test -p flotilla-core --locked --lib --features cleat-environment-contract launch_environment_contract -- --nocapture

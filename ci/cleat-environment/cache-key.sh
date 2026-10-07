#!/usr/bin/env bash
# Both source revisions pin their own Rust/Zig/Ghostty inputs; include the
# preparation scripts too, since they can override those source pins.
# This cache is for the Linux cleat job; sha256sum is supplied by its runner.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
printf 'cleat-environment-linux-%s\n' "$(
    {
        uname -m
        printf '%s\n' "${ImageOS:-unknown-image}" "${ImageVersion:-unknown-version}"
        sha256sum ci/cleat-environment/revisions.sh ci/cleat-environment/build.sh ci/fleet-candidates/cleat-toolchain.sh ci/fork-actions/runtime/cleat/prepare-ghostty-vt.sh ci/fork-actions/runtime/cleat/verify-zig-version.sh rust-toolchain.toml
    } | sha256sum | cut -d' ' -f1
)"

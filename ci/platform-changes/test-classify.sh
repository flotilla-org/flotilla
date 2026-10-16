#!/usr/bin/env bash
# Behaviour tests for classify.sh: which changes run the Windows, macOS and Quint jobs.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

check() {
  local name=$1 event=$2 label=$3 files=$4 diff=$5 expected=$6
  printf '%s' "$files" >"$work/files"
  printf '%s' "$diff" >"$work/diff"
  local actual
  actual=$("$here/classify.sh" "$event" "$label" "$work/files" "$work/diff" | tr '\n' ' ')
  if [[ "$actual" != "$expected" ]]; then
    echo "FAIL $name: expected '$expected', got '$actual'" >&2
    failures=$((failures + 1))
  fi
}

core_diff=$'--- a/crates/flotilla-core/src/x.rs\n+++ b/crates/flotilla-core/src/x.rs\n+fn helper() {}\n'

check "push to main runs every job" push false "docs/x.md" "" "windows=true macos=true quint=true "
check "ci:full label runs every job" pull_request true "docs/x.md" "" "windows=true macos=true quint=true "
check "docs-only PR runs neither" pull_request false "docs/x.md" "" "windows=false macos=false quint=false "
check "plain core change runs neither" pull_request false "crates/flotilla-core/src/x.rs" "$core_diff" "windows=false macos=false quint=false "
check "client change runs Windows" pull_request false "crates/flotilla-client/src/lib.rs" "" "windows=true macos=false quint=false "
check "CLI definition change runs Windows" pull_request false "crates/flotilla-commands/src/commands/convoy.rs" "" "windows=true macos=false quint=false "
# Connector viewer-locality changes must run the Windows-client tests.
check "connector locality runs Windows" pull_request false "crates/flotilla-tui/src/pm_connect.rs" "" "windows=true macos=false quint=false "
check "connector tests run Windows" pull_request false "crates/flotilla-tui/src/pm_connect/tests.rs" "" "windows=true macos=false quint=false "
check "transport change runs both" merge_group false "crates/flotilla-transport/src/message.rs" "" "windows=true macos=true quint=false "
check "caller change runs macOS" pull_request false "crates/flotilla-daemon/src/server/caller.rs" "" "windows=false macos=true quint=false "
# The compiler pin and its assertion are shared platform-job inputs.
check "compiler pin runs every job" pull_request false "rust-toolchain.toml" "" "windows=true macos=true quint=true "
check "selector changes run both" pull_request false "ci/platform-tests/selectors.txt" "" "windows=true macos=true quint=false "
check "local Rust action changes run both" pull_request false "ci/toolchain/action/action.yml" "" "windows=true macos=true quint=false "
check "compiler assertion runs both" pull_request false "ci/toolchain/assert.sh" "" "windows=true macos=true quint=false "
check "lockfile runs both" pull_request false "Cargo.lock" "" "windows=true macos=true quint=false "
check "workflow change runs every job" pull_request false ".github/workflows/ci.yml" "" "windows=true macos=true quint=true "
check "core cfg(unix) runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'--- a/crates/flotilla-core/src/x.rs\n+++ b/crates/flotilla-core/src/x.rs\n+#[cfg(unix)]\n+fn helper() {}\n' "windows=true macos=false quint=false "
check "core std::os::unix runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+use std::os::unix::fs::PermissionsExt;\n' "windows=true macos=false quint=false "
check "removed cfg(windows) runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'-#[cfg(windows)]\n' "windows=true macos=false quint=false "
check "macOS target_os runs macOS" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+#[cfg(target_os = "macos")]\n' "windows=true macos=true quint=false "
check "diff header alone is not content" pull_request false "crates/flotilla-core/src/cfg_unix.rs" \
  $'--- a/crates/flotilla-core/src/cfg(unix).rs\n+++ b/crates/flotilla-core/src/cfg(unix).rs\n+fn x() {}\n' "windows=false macos=false quint=false "

check "unknown event runs every job" workflow_dispatch false "" "" "windows=true macos=true quint=true "
check "manifest sink runs Windows" pull_request false "crates/flotilla-manifest/src/sink.rs" "" "windows=true macos=false quint=false "
check "crate manifest runs both" pull_request false "crates/flotilla-core/Cargo.toml" "" "windows=true macos=true quint=false "
check "core libc call runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+    let pid = unsafe { libc::getpid() };\n' "windows=true macos=false quint=false "
check "core UnixStream runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+use tokio::net::UnixStream;\n' "windows=true macos=false quint=false "
check "caller path plus macOS content runs macOS" pull_request false "crates/flotilla-daemon/src/server/caller.rs" \
  $'+#[cfg(target_vendor = "apple")]\n' "windows=true macos=true quint=false "

check "core cfg! macro runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+    if cfg!(windows) { return; }\n' "windows=true macos=false quint=false "
check "core cfg_attr runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+#[cfg_attr(unix, path = "unix.rs")]\n' "windows=true macos=false quint=false "

# The Quint quick tier runs on its specs, the engine crate and the shared
# workflow inputs, never on a diff's content.
check "Quint spec runs Quint" pull_request false "specs/primitives/scenarios/convoy_deliver.qnt" "" \
  "windows=false macos=false quint=true "
check "engine crate runs Quint" merge_group false "crates/flotilla-state-machines/src/lib.rs" "" \
  "windows=false macos=false quint=true "
check "conformance harness runs Quint" pull_request false "specs/primitives/conformance/message.qnt" "" \
  "windows=false macos=false quint=true "
check "classifier change runs every job" pull_request false "ci/platform-changes/classify.sh" "" \
  "windows=true macos=true quint=true "
check "prototypes skip Quint" pull_request false "prototypes/1262-scenarios/builtin-implement-review.yaml" "" \
  "windows=false macos=false quint=false "
check "other crates skip Quint" pull_request false "crates/flotilla-resources/src/lib.rs" "" \
  "windows=false macos=false quint=false "
check "empty diff lines are not Quint content" pull_request false "crates/flotilla-core/src/x.rs" \
  $'+fn x() {}\n\n' "windows=false macos=false quint=false "

large_diff=$'+#[cfg(unix)]\n'$(printf '+line %s padding padding padding\n' $(seq 1 200000))
check "early content hit in a large diff runs Windows" pull_request false "crates/flotilla-core/src/x.rs" \
  "$large_diff" "windows=true macos=false quint=false "

if ((failures > 0)); then
  echo "$failures platform-changes classification test(s) failed" >&2
  exit 1
fi
echo "platform-changes classification tests passed"
